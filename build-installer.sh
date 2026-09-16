#!/usr/bin/env bash
# Build the Windows installer from WSL: stage exe + runtime + model under dist/stage, then run Inno Setup.
set -euo pipefail
cd "$(dirname "$0")"
VERSION=${BLACKHOLE_VERSION:-$(grep -m1 '^version' Cargo.toml | sed -E 's/.*"(.*)"/\1/')}
MODEL_DIR=${MODEL_DIR:-models/qwen3-4b}
ISCC=${ISCC:-}
for c in "/mnt/c/Program Files (x86)/Inno Setup 6/ISCC.exe" /mnt/c/Users/*/AppData/Local/Programs/"Inno Setup 6"/ISCC.exe; do
  [ -n "$ISCC" ] || { [ -x "$c" ] && ISCC="$c"; }
done
[ -n "$ISCC" ] || { echo "Inno Setup not found (winget install JRSoftware.InnoSetup)"; exit 1; }

cargo build --release --bin blackhole
rm -rf dist/stage && mkdir -p dist/stage/qwen3-4b
cp target/x86_64-pc-windows-gnu/release/blackhole.exe runtime/onnxruntime.dll runtime/DirectML.dll README.md dist/stage/
cp installer/LICENSE-MODELS.txt dist/stage/
cp "$MODEL_DIR"/model_q4f16.onnx "$MODEL_DIR"/model_q4f16.onnx.data "$MODEL_DIR"/tokenizer.json dist/stage/qwen3-4b/
du -sh dist/stage
# Inno Setup is a Windows program: hand it a Windows path.
WIN_ISS=$(wslpath -w "$(pwd)/installer/blackhole.iss")
BLACKHOLE_VERSION="$VERSION" "$ISCC" /Qp "$WIN_ISS"
ls -la dist/*.exe
