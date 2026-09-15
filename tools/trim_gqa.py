"""Drop trailing empty optional inputs from GroupQueryAttention nodes so a graph
from a newer ONNX Runtime GenAI builder loads on ONNX Runtime 1.20 (max 9 inputs).

usage: python tools/trim_gqa.py in.onnx out.onnx   (external data untouched; keep the same .data name)
"""
import sys
import onnx

src, dst = sys.argv[1], sys.argv[2]
m = onnx.load(src, load_external_data=False)
n_fixed = 0
for node in m.graph.node:
    if node.op_type == "GroupQueryAttention":
        while len(node.input) > 9 and node.input[-1] == "":
            node.input.pop()
        n_fixed += 1
onnx.save(m, dst)
print("trimmed", n_fixed, "GroupQueryAttention nodes ->", dst)
