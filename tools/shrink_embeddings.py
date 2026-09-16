"""Shrink the tied embedding / LM-head matrix of a decoder ONNX export:
  - embedding Gather reads a float16 copy (+ Cast to float32),
  - the LM head Gemm(transB=1) becomes MatMulNBits (int4, block 32, symmetric),
so the 1.5 GB fp32 matrix disappears from both the CPU and the DirectML session.
Streams the weight from the external data file in row blocks (low memory) and
writes the new tensors to `<graph>_data_shrunk`; the old bytes stay in the
original data file (use a repack step to reclaim disk).

usage: python tools/shrink_embeddings.py model.onnx   (in place, graph only + new data file)
"""
import os
import sys
import numpy as np
import onnx
from onnx import helper, numpy_helper, TensorProto
from onnx.external_data_helper import uses_external_data

path = sys.argv[1]
d = os.path.dirname(path) or "."
m = onnx.load(path, load_external_data=False)
g = m.graph
prod = {o: n for n in g.node for o in n.output}
inits = {i.name: i for i in g.initializer}

gemm = prod["bh_logits_2d"] if "bh_logits_2d" in prod else None
if gemm is None or gemm.op_type != "Gemm":
    raise SystemExit("expected the Gemm(transB) LM head from tools/gemm_head.py")
W = inits[gemm.input[1]]
if not uses_external_data(W) or W.data_type != TensorProto.FLOAT:
    raise SystemExit("weight must be an external float32 tensor")
V, H = (int(x) for x in W.dims)
ext = {kv.key: kv.value for kv in W.external_data}
src = np.memmap(os.path.join(d, ext["location"]), dtype=np.float32, mode="r", offset=int(ext.get("offset", 0)), shape=(V, H))

BLOCK = 32
nblk = H // BLOCK
q = np.empty((V, nblk, BLOCK // 2), dtype=np.uint8)
scales = np.empty((V, nblk), dtype=np.float32)
f16 = np.empty((V, H), dtype=np.float16)
rows = 2048
for r0 in range(0, V, rows):
    w = np.asarray(src[r0:r0 + rows], dtype=np.float32)          # [r, H]
    f16[r0:r0 + rows] = w.astype(np.float16)
    b = w.reshape(w.shape[0], nblk, BLOCK)                        # [r, nblk, 32]
    amax = np.abs(b).max(axis=2)                                  # symmetric int4: scale = max/7
    sc = np.where(amax > 0, amax / 7.0, 1.0).astype(np.float32)
    qi = np.clip(np.rint(b / sc[..., None]), -8, 7).astype(np.int8) + 8   # 0..15, zero point 8
    lo, hi = qi[..., 0::2], qi[..., 1::2]                          # pack two nibbles per byte (low first)
    q[r0:r0 + rows] = (lo | (hi << 4)).astype(np.uint8)
    scales[r0:r0 + rows] = sc
    print(f"  rows {r0 + w.shape[0]}/{V}", end="\r", flush=True)
print()

data_name = os.path.basename(path) + "_data_shrunk"
out = os.path.join(d, data_name)
offsets = {}
with open(out, "wb") as fh:
    for name, arr in (("bh_embed_f16", f16), ("bh_lm_head_q4", q), ("bh_lm_head_scales", scales.reshape(-1))):
        offsets[name] = (fh.tell(), arr.nbytes, arr)
        fh.write(arr.tobytes())

def ext_tensor(name, arr):
    t = onnx.TensorProto()
    t.name = name
    t.data_type = {np.float16: TensorProto.FLOAT16, np.uint8: TensorProto.UINT8, np.float32: TensorProto.FLOAT}[arr.dtype.type]
    t.dims.extend(arr.shape)
    t.data_location = TensorProto.EXTERNAL
    off, length, _ = offsets[name]
    for k, v in (("location", data_name), ("offset", str(off)), ("length", str(length))):
        e = t.external_data.add(); e.key, e.value = k, v
    return t

g.initializer.extend([ext_tensor("bh_embed_f16", f16), ext_tensor("bh_lm_head_q4", q), ext_tensor("bh_lm_head_scales", scales.reshape(-1))])

# Embedding: Gather(W) -> Gather(W_f16) -> Cast(float)
for n in g.node:
    if n.op_type == "Gather" and n.input[0] == W.name:
        out_name = n.output[0]
        n.input[0] = "bh_embed_f16"
        n.output[0] = out_name + "_f16"
        idx = list(g.node).index(n)
        g.node.insert(idx + 1, helper.make_node("Cast", [out_name + "_f16"], [out_name], name=n.name + "/cast_f32", to=TensorProto.FLOAT))
# LM head: Gemm(hidden, W, transB) -> MatMulNBits(hidden, W_q4, scales)
mmn = helper.make_node("MatMulNBits", [gemm.input[0], "bh_lm_head_q4", "bh_lm_head_scales"], list(gemm.output), name="bh_lm_head_q4_matmul", domain="com.microsoft", K=H, N=V, bits=4, block_size=BLOCK, accuracy_level=4)
g.node.insert(list(g.node).index(gemm), mmn)
g.node.remove(gemm)
g.initializer.remove(W)
if not any(o.domain == "com.microsoft" for o in m.opset_import):
    m.opset_import.append(helper.make_opsetid("com.microsoft", 1))
onnx.save(m, path)
print(f"embedding -> float16 ({f16.nbytes >> 20} MB), LM head -> int4 MatMulNBits ({(q.nbytes + scales.nbytes) >> 20} MB); wrote {data_name}")
