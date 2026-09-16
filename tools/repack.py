"""Repack an ONNX model's external data into ONE file containing only the tensors
the graph still references, streamed in chunks (constant memory). Removes dead
weights left behind by graph edits (e.g. the fp32 embedding matrix after
tools/shrink_embeddings.py) and merges split `_data`, `_data_1`, … files.

usage: python tools/repack.py model.onnx out_dir
Writes out_dir/model.onnx + out_dir/model.onnx.data (and copies tokenizer.json if present).
"""
import os
import shutil
import sys
import onnx
from onnx.external_data_helper import uses_external_data

src, out_dir = sys.argv[1], sys.argv[2]
os.makedirs(out_dir, exist_ok=True)
d = os.path.dirname(src) or "."
m = onnx.load(src, load_external_data=False)
g = m.graph
used = set()
for n in g.node:
    used.update(n.input)
for o in g.output:
    used.add(o.name)
data_name = os.path.basename(src) + ".data"
out_data = os.path.join(out_dir, data_name)
kept, dropped_bytes = 0, 0
with open(out_data, "wb") as fh:
    for t in list(g.initializer):
        if t.name not in used:
            dropped_bytes += (int({kv.key: kv.value for kv in t.external_data}.get("length", 0)) if uses_external_data(t) else len(t.raw_data))
            g.initializer.remove(t)
            continue
        if not uses_external_data(t):
            continue
        ext = {kv.key: kv.value for kv in t.external_data}
        off, length = int(ext.get("offset", 0)), int(ext["length"])
        # 64-byte alignment keeps mmap-friendly offsets.
        pad = (-fh.tell()) % 64
        fh.write(b"\0" * pad)
        new_off = fh.tell()
        with open(os.path.join(d, ext["location"]), "rb") as fin:
            fin.seek(off)
            remaining = length
            while remaining:
                chunk = fin.read(min(remaining, 64 << 20))
                fh.write(chunk)
                remaining -= len(chunk)
        del t.external_data[:]
        for k, v in (("location", data_name), ("offset", str(new_off)), ("length", str(length))):
            e = t.external_data.add(); e.key, e.value = k, v
        kept += 1
onnx.save(m, os.path.join(out_dir, os.path.basename(src)))
tok = os.path.join(d, "tokenizer.json")
if os.path.exists(tok):
    shutil.copy(tok, out_dir)
print(f"kept {kept} external tensors -> {out_data} ({os.path.getsize(out_data) >> 20} MB); dropped {dropped_bytes >> 20} MB of unreferenced weights")
