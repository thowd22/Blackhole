"""Make the last-token selection inserted by last_logits.py take its position from
a new int64[1] graph input `logit_index` instead of the constant -1. With a
static-shape session the prompt is fed in fixed-size chunks padded at the end,
so the token whose logits we want is no longer the last one.

The Slice becomes a Gather: Slice starts/ends are CPU-side inputs that a fused
DirectML graph bakes in from the first run, whereas Gather indices are an
ordinary device tensor read on every run.
usage: python tools/logit_index.py model.onnx   (in place; graph only)
"""
import sys
import onnx
from onnx import helper, TensorProto

path = sys.argv[1]
m = onnx.load(path, load_external_data=False)
g = m.graph
nodes = list(g.node)
sl = next(n for n in nodes if n.name == "bh_last_token")
if sl.op_type == "Gather":
    raise SystemExit("already a Gather on logit_index")
if not any(i.name == "logit_index" for i in g.input):
    g.input.append(helper.make_tensor_value_info("logit_index", TensorProto.INT64, [1]))
# an earlier revision of this tool fed the Slice through an Add; drop it
for n in nodes:
    if n.name == "bh_logit_end":
        g.node.remove(n)
gather = helper.make_node("Gather", [sl.input[0], "logit_index"], list(sl.output), name="bh_last_token", axis=1)
idx = list(g.node).index(sl)
g.node.remove(sl)
g.node.insert(idx, gather)
onnx.save(m, path)
print("last-token pick is Gather(axis=1, indices=logit_index)")
