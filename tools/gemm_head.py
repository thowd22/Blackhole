"""Replace a runtime `Transpose(W) -> MatMul(hidden, W^T)` LM head with
`Reshape -> Gemm(hidden, W, transB=1) -> Reshape`. Some exports leave the
(tied) embedding matrix untransposed and transpose it on every run — 1.5 GB
of work per pass, and DirectML mishandles it. Graph-only edit; weights untouched.
Run AFTER tools/last_logits.py (expects logits to be [b, 1, V]).

usage: python tools/gemm_head.py model.onnx   (in place)
"""
import sys
import numpy as np
import onnx
from onnx import helper, numpy_helper

path = sys.argv[1]
m = onnx.load(path, load_external_data=False)
g = m.graph
prod = {o: n for n in g.node for o in n.output}
inits = {i.name for i in g.initializer}
mm = prod["logits"]
while mm.op_type == "Cast":
    mm = prod[mm.input[0]]
if mm.op_type != "MatMul":
    raise SystemExit(f"LM head is {mm.op_type}, nothing to do")
tr = next((prod[i] for i in mm.input if i in prod and prod[i].op_type == "Transpose"), None)
if tr is None:
    raise SystemExit("LM head weight is not produced by a Transpose, nothing to do")
hidden = next(i for i in mm.input if i != tr.output[0])
W = tr.input[0]
dims = next(i.dims for i in g.initializer if i.name == W)
H = list(dims)[1]  # W is [V, H]
g.initializer.append(numpy_helper.from_array(np.array([-1, H], dtype=np.int64), "bh_shape_2d"))
g.initializer.append(numpy_helper.from_array(np.array([1, 1, -1], dtype=np.int64), "bh_shape_3d"))
nodes = [
    helper.make_node("Reshape", [hidden, "bh_shape_2d"], ["bh_hidden_2d"], name="bh_reshape_2d"),
    helper.make_node("Gemm", ["bh_hidden_2d", W], ["bh_logits_2d"], name="bh_lm_head_gemm", transB=1),
    helper.make_node("Reshape", ["bh_logits_2d", "bh_shape_3d"], list(mm.output), name="bh_reshape_3d"),
]
idx = list(g.node).index(mm)
g.node.remove(mm)
g.node.remove(tr)
for k, n in enumerate(nodes):
    g.node.insert(idx - 1 + k, n)
onnx.save(m, path)
print("rewired LM head to Gemm(transB) on", hidden, "W dims", list(dims))
