"""Insert a last-token Slice before the LM head of an HF-style decoder ONNX export,
so `logits` is [batch, 1, vocab] instead of [batch, seq, vocab] (the prompt pass
would otherwise return ~1 GB of logits for a long prompt).

usage: python tools/last_logits.py in.onnx out.onnx
"""
import sys
import numpy as np
import onnx
from onnx import helper, numpy_helper

src, dst = sys.argv[1], sys.argv[2]
m = onnx.load(src)
g = m.graph
prod = {o: n for n in g.node for o in n.output}
n = prod["logits"]
while n.op_type == "Cast":
    n = prod[n.input[0]]
hidden = n.input[0]
g.initializer.extend([
    numpy_helper.from_array(np.array([-1], dtype=np.int64), "bh_slice_start"),
    numpy_helper.from_array(np.array([np.iinfo(np.int64).max], dtype=np.int64), "bh_slice_end"),
    numpy_helper.from_array(np.array([1], dtype=np.int64), "bh_slice_axis"),
])
g.node.insert(list(g.node).index(n), helper.make_node("Slice", [hidden, "bh_slice_start", "bh_slice_end", "bh_slice_axis"], ["bh_hidden_last"], name="bh_last_token"))
n.input[0] = "bh_hidden_last"
for o in g.output:
    if o.name == "logits":
        o.type.tensor_type.shape.dim[1].ClearField("dim_param")
        o.type.tensor_type.shape.dim[1].dim_value = 1
onnx.save(m, dst)
print("wrote", dst)
