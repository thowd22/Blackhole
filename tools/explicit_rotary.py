"""Move rotary embedding out of GroupQueryAttention (do_rotary=1, cos/sin fed to
the op) into explicit com.microsoft RotaryEmbedding nodes on Q and K, driven by
a new `position_ids` graph input — the layout Microsoft's own DirectML exports
use. DirectML's GQA kernel produces prompt-independent garbage with in-op
rotary; with explicit rotary it runs correctly.

usage: python tools/explicit_rotary.py model.onnx   (in place; graph only)
"""
import sys
import onnx
from onnx import helper, TensorProto

path = sys.argv[1]
m = onnx.load(path, load_external_data=False)
g = m.graph
gqas = [n for n in g.node if n.op_type == "GroupQueryAttention"]
if not gqas or not any(a.name == "do_rotary" and a.i == 1 for a in gqas[0].attribute):
    raise SystemExit("no GroupQueryAttention with do_rotary=1; nothing to do")
if not any(i.name == "position_ids" for i in g.input):
    pos = helper.make_tensor_value_info("position_ids", TensorProto.INT64, ["batch_size", "sequence_length"])
    g.input.append(pos)
for n in gqas:
    attrs = {a.name: a for a in n.attribute}
    heads = attrs["num_heads"].i
    kv_heads = attrs["kv_num_heads"].i
    interleaved = attrs["rotary_interleaved"].i if "rotary_interleaved" in attrs else 0
    q, k, cos, sin = n.input[0], n.input[1], n.input[7], n.input[8]
    base = n.name.rsplit("/", 1)[0]
    qr, kr = f"{base}/q_rotary/output_0", f"{base}/k_rotary/output_0"
    idx = list(g.node).index(n)
    g.node.insert(idx, helper.make_node("RotaryEmbedding", [q, "position_ids", cos, sin], [qr], name=f"{base}/q_rotary", domain="com.microsoft", interleaved=interleaved, num_heads=heads))
    g.node.insert(idx + 1, helper.make_node("RotaryEmbedding", [k, "position_ids", cos, sin], [kr], name=f"{base}/k_rotary", domain="com.microsoft", interleaved=interleaved, num_heads=kv_heads))
    n.input[0], n.input[1] = qr, kr
    n.input[7], n.input[8] = "", ""
    attrs["do_rotary"].i = 0
onnx.save(m, path)
print(f"rewired {len(gqas)} GroupQueryAttention nodes to explicit RotaryEmbedding; added position_ids input")
