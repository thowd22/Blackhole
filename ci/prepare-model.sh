#!/usr/bin/env bash
# Download the LLM export and run Blackhole's graph tools on it, producing the
# single-data-file model the installer ships (models/qwen3-4b/). Pinned by sha256.
# Needs: curl, python3 with `onnx` and `numpy` (pip install onnx numpy).
# ~2.8 GB download; the result is ~2.7 GB. Same recipe as PACKAGING.md.
set -euo pipefail
cd "$(dirname "$0")/.."
sha() { if command -v sha256sum >/dev/null; then sha256sum "$1" | cut -c1-64; else shasum -a 256 "$1" | cut -c1-64; fi; }
fetch() {
  local url=$1 dest=$2 want=$3
  if [ -f "$dest" ] && [ "$(sha "$dest")" = "$want" ]; then echo "ok      $dest"; return; fi
  echo "fetch   $dest"; curl -sSL --retry 5 --retry-delay 5 -o "$dest.part" "$url"
  local got; got=$(sha "$dest.part"); [ "$got" = "$want" ] || { echo "sha256 mismatch for $dest" >&2; exit 1; }
  mv "$dest.part" "$dest"
}
REPO=https://huggingface.co/onnx-community/Qwen3-4B-ONNX/resolve/main
WORK=models/qwen3-4b-src; OUT=models/qwen3-4b
mkdir -p "$WORK"
fetch $REPO/onnx/model_q4f16.onnx        $WORK/model_q4f16.onnx        d3e946f9e38577411b0251051c91a1f20c3c0c831e1cf76a13c58ce279d950de
fetch $REPO/onnx/model_q4f16.onnx_data   $WORK/model_q4f16.onnx_data   050398248de4fce7b31b2d2caa909596e4c7aa0f35696270df46b8aaf2209fc8
fetch $REPO/onnx/model_q4f16.onnx_data_1 $WORK/model_q4f16.onnx_data_1 363ff5e70ebeb5866afea8eb80b7bdfe22d94d735a8c4d2ebf90c75424c2a410
fetch $REPO/tokenizer.json               $WORK/tokenizer.json          e7a95fce95bf5b0946d0ddb3f9d7caa030b7e850bbe92b0edb26bcf563e9f3d5
# Graph-only edits (weights are never rewritten): last-token logits, Gemm LM head,
# explicit rotary if the export needs it, Gather-based logit index, then one data file.
python3 tools/last_logits.py    "$WORK/model_q4f16.onnx" "$WORK/model_q4f16.onnx"
python3 tools/gemm_head.py      "$WORK/model_q4f16.onnx"
python3 tools/explicit_rotary.py "$WORK/model_q4f16.onnx" || true   # "nothing to do" for this export
python3 tools/logit_index.py    "$WORK/model_q4f16.onnx"
rm -rf "$OUT" && python3 tools/repack.py "$WORK/model_q4f16.onnx" "$OUT"
cp "$WORK/tokenizer.json" "$OUT/"
rm -rf "$WORK"
ls -la "$OUT"
echo "model ready: $OUT"
