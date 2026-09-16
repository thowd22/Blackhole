#!/usr/bin/env bash
# Fetch the assets that `include_bytes!` compiles into blackhole.exe: the embedder,
# the reranker, the fallback tokenizer and the ONNX Runtime / DirectML DLLs.
# Every file is pinned by sha256; a changed upstream file fails the build instead of
# silently shipping something else. Idempotent: files already present and matching
# are kept (CI caches models/ and runtime/ on the hash of this script).
#
# Needs: curl, unzip, sha256sum (coreutils; `shasum -a 256` fallback for macOS).
set -euo pipefail
cd "$(dirname "$0")/.."

sha() { if command -v sha256sum >/dev/null; then sha256sum "$1" | cut -c1-64; else shasum -a 256 "$1" | cut -c1-64; fi; }

# fetch <url> <dest> <sha256>
fetch() {
  local url=$1 dest=$2 want=$3
  if [ "$want" != "__SKIP__" ] && [ -f "$dest" ] && [ "$(sha "$dest")" = "$want" ]; then echo "ok      $dest"; return; fi
  mkdir -p "$(dirname "$dest")"
  echo "fetch   $dest"
  curl -sSL --retry 5 --retry-delay 3 -o "$dest.part" "$url"
  local got; got=$(sha "$dest.part")
  if [ "$want" != "__SKIP__" ] && [ "$got" != "$want" ]; then echo "sha256 mismatch for $dest: got $got want $want" >&2; rm -f "$dest.part"; exit 1; fi
  mv "$dest.part" "$dest"
}

HF=https://huggingface.co
fetch $HF/BAAI/bge-small-en-v1.5/resolve/main/onnx/model.onnx \
      models/bge-small-en-v1.5/model.onnx 828e1496d7fabb79cfa4dcd84fa38625c0d3d21da474a00f08db0f559940cf35
fetch $HF/BAAI/bge-small-en-v1.5/resolve/main/vocab.txt \
      models/bge-small-en-v1.5/vocab.txt 07eced375cec144d27c900241f3e339478dec958f92fddbc551f295c992038a3
fetch $HF/mixedbread-ai/mxbai-rerank-xsmall-v1/resolve/main/onnx/model_quantized.onnx \
      models/rerank-mxbai-int8/model.onnx 15ef19a6de90be7d52b627f2c784107bd806e64826450f41fb75fa4f0179ab30
fetch $HF/mixedbread-ai/mxbai-rerank-xsmall-v1/resolve/main/tokenizer.json \
      models/rerank-mxbai-int8/tokenizer.json 305674b4d785287feecfb5f73f24aa75e9b57c87c579cfe24fbd207987d4b4c4
fetch $HF/Qwen/Qwen2.5-1.5B-Instruct/resolve/main/tokenizer.json \
      models/qwen2.5/tokenizer.json c0382117ea329cdf097041132f6d735924b697924d6f6fc3945713e96ce87539

# OCR: PP-OCRv4 detector + PP-OCRv3 English recognizer (RapidOCR's ONNX exports) and
# PaddleOCR's English dictionary; compiled into the exe like the embedder.
fetch $HF/SWHL/RapidOCR/resolve/main/PP-OCRv4/ch_PP-OCRv4_det_infer.onnx \
      models/ocr/det.onnx d2a7720d45a54257208b1e13e36a8479894cb74155a5efe29462512d42f49da9
fetch $HF/SWHL/RapidOCR/resolve/main/PP-OCRv3/en_PP-OCRv3_rec_infer.onnx \
      models/ocr/rec_en.onnx ef7abd8bd3629ae57ea2c28b425c1bd258a871b93fd2fe7c433946ade9b5d9ea
fetch https://raw.githubusercontent.com/PaddlePaddle/PaddleOCR/main/ppocr/utils/en_dict.txt \
      models/ocr/en_dict.txt 5662df9d2d03f0e8ca0d3b0649d6acbab904b6a14b3d3521463c71c37c668ce3

# ONNX Runtime (DirectML build) and DirectML itself come as NuGet packages (zip files).
# Windows-only: these DLLs are what the x64 Windows exe embeds. A Linux build would
# instead fetch libonnxruntime.so from the GitHub release tarball, macOS the
# .dylib (CoreML EP), and runtime.rs would unpack those — see the notes in ci.yml.
NUGET=https://api.nuget.org/v3-flatcontainer
if [ ! -f runtime/onnxruntime.dll ] || [ "$(sha runtime/onnxruntime.dll)" != 903c92c54acc57caa77d44d4c856c829eb82f5cf6755b47367a8153bd3fabeb6 ]; then
  fetch $NUGET/microsoft.ml.onnxruntime.directml/1.20.1/microsoft.ml.onnxruntime.directml.1.20.1.nupkg \
        /tmp/ort-dml.nupkg __SKIP__ || true
  mkdir -p runtime && unzip -o -j -q /tmp/ort-dml.nupkg runtimes/win-x64/native/onnxruntime.dll -d runtime
fi
if [ ! -f runtime/DirectML.dll ] || [ "$(sha runtime/DirectML.dll)" != 9c9e6d822561c6c41b90e6994b3e8857cf1d66dbfb1e0c4c799c7c89b4e92da1 ]; then
  fetch $NUGET/microsoft.ai.directml/1.15.4/microsoft.ai.directml.1.15.4.nupkg /tmp/directml.nupkg __SKIP__ || true
  mkdir -p runtime && unzip -o -j -q /tmp/directml.nupkg bin/x64-win/DirectML.dll -d runtime
fi
[ "$(sha runtime/onnxruntime.dll)" = 903c92c54acc57caa77d44d4c856c829eb82f5cf6755b47367a8153bd3fabeb6 ] || { echo "onnxruntime.dll hash mismatch" >&2; exit 1; }
[ "$(sha runtime/DirectML.dll)" = 9c9e6d822561c6c41b90e6994b3e8857cf1d66dbfb1e0c4c799c7c89b4e92da1 ] || { echo "DirectML.dll hash mismatch" >&2; exit 1; }
# Neovim (the notes editor) ships beside the exe, not inside it: installer + portable zip.
NVIM_VER=0.12.5
fetch https://github.com/neovim/neovim/releases/download/v$NVIM_VER/nvim-win64.zip \
      runtime/nvim-win64.zip de8625ba8cf65ebf40eb80a388ba1ec8e9c15b30218821e2c639119b05920de1
if [ ! -x runtime/nvim/nvim-win64/bin/nvim.exe ] && [ ! -f runtime/nvim/nvim-win64/bin/nvim.exe ]; then
  rm -rf runtime/nvim && mkdir -p runtime/nvim && unzip -q runtime/nvim-win64.zip -d runtime/nvim
fi
echo "assets ready"
