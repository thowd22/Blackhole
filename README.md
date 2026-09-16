# Blackhole

A tiny pixel-art black hole that floats above every window. Drop files or text
on it (or paste with **Ctrl+Shift+V**) and it swallows them into a local vault.
Click it — or press **Ctrl+Shift+Space** to summon it to the mouse — and search.

Design notes: [CONCEPT.md](CONCEPT.md) · [FEATURES.md](FEATURES.md) · [PACKAGING.md](PACKAGING.md)

Issues and PRs are welcome: https://github.com/thowd22/Blackhole

## Status: MVP (Windows x64)

Working now:
- Layered, always-on-top, no-taskbar dot with procedurally rendered pixel-art sprite and mood animations
- Drag to move (position remembered), left-click to search, right-click menu (size, vault folder, quit)
- Drag-and-drop of files and text; clipboard swallow via hotkey/menu
- Ingest: plain text, text/code/markdown files, PDFs (text extraction); other files by name
- SQLite vault (`%LOCALAPPDATA%\Blackhole\vault.db`) with FTS5 keyword search (BM25)
- On-device embeddings: bge-small-en-v1.5 (384-dim, CLS-pooled) on **ONNX Runtime + DirectML** (any GPU;
  CPU provider fallback). The runtime DLLs and the model are compiled into the exe; the DLLs are unpacked
  beside it on first run. Items are chunked (~100 words, overlapping), each chunk embedded with its title
- Hybrid search: keyword + semantic (cosine over chunks and one title-only document unit per item),
  score-fused (keyword weight scales with query-term coverage); English stopwords never count as
  distinctive terms; results are tagged `≈` (semantic), `≈=` (both) or untagged (keyword)
- "Not in your vault" gate: ≥2 content words, none anywhere in the vault, no strong semantic match →
  the panel shows nothing and ask mode answers without calling the LLM (2/2 absent, 0 false absents on
  dev + held-out question sets)
- Ask-mode retrieval adds a vault-anchored synonym expansion of the dense query and a cross-encoder
  reranker (mxbai-rerank-xsmall-v1 int8 on ORT, top-10 candidates, ~110 ms) that decides the top document;
  18/18 Hit@1 on dev + held-out. Live search stays at ~5 ms/query. Method and numbers: RAG.md §2b
- Search panel: live results, ↵ open, Ctrl+↵ reveal in Explorer, Ctrl+C copy text, Del forget
- Ask mode: start a query with `?` and press ↵ — **Llama 3.2 3B Instruct** (int4 ONNX) on ONNX Runtime,
  hybrid execution: prompt pass on DirectML (the GPU with the most VRAM, picked via DXGI), decode on the CPU
  provider. Measured end to end on 18 questions: 14/18 correct (Qwen2.5-1.5B: 9/18), ~1 s to first text,
  ~7.6 tok/s. Any instruct model dropped beside the exe works if its `tokenizer.json` sits next to it: the
  chat template (ChatML / Llama 3 / Phi / Gemma) and stop tokens are detected from the tokenizer; fp16 or fp32
  KV caches are both handled. Exports go through `tools/last_logits.py` → `gemm_head.py` → `explicit_rotary.py` →
  `shrink_embeddings.py` → `repack.py` (PACKAGING.md § LLM selection round and § Memory); the shipped Llama is
  2.7 GB on disk and peaks at ~6 GB of RAM while answering. The largest `*.onnx` (graph + external data)
  in the exe folder or its subfolders wins.
  Context = best chunks with neighbours in document order, or the whole document when the top hit is small.
  Order/time questions get a forced "Timeline:" scratchpad. Pre-loaded when the panel opens, unloaded 60 s
  after it closes. The exact prompt of the last question goes to `%LOCALAPPDATA%\Blackhole\last_ask.txt`;
  `cargo build --release --bin askeval` builds the end-to-end evaluation binary.
- Tray icon (the sprite) with the same menu; **Show dot** toggles dot visibility, **Start at login**
  writes the per-user Run key, **Center on new message** warps the dot to screen centre for notifications
- Pixel-art speech bubbles: a 6-step first-run tutorial (each step waits for the action it describes;
  the orange × skips the tour; "Show tutorial" in the menu replays it) and notifications (ingest failures now; MCP `notify` later).
  Notifications pre-empt a tutorial step and it resumes afterwards; click a bubble to dismiss.
- Diagnostics: `%LOCALAPPDATA%\Blackhole\log.txt` (ingest failures), `last_ask.txt` (last ask prompt)

Not yet: OCR & image captions, GPU/NPU acceleration, panel resizing / rich snippets, MCP server (see FEATURES.md).

## Installing

Download `Blackhole-<version>-x64-setup.exe` (≈2.9 GB: the app, the ONNX Runtime + DirectML runtime and the
Llama 3.2 3B model) and run it. It installs per user by default (no admin needed), can start at sign-in, and
puts the dot on your desktop. Your vault lives in `%LOCALAPPDATA%\Blackhole` and survives updates; uninstall
asks before deleting it. Requires Windows 10 1903+ / Windows 11, x64, any GPU with DirectML (the CPU is used
otherwise). Build it yourself with `./build-installer.sh` (needs Inno Setup: `winget install JRSoftware.InnoSetup`).

## Building (from WSL)

```sh
sudo apt install mingw-w64
rustup target add x86_64-pc-windows-gnu
# model files (not committed):
#   models/bge-small-en-v1.5/{model.onnx,vocab.txt}  from https://huggingface.co/BAAI/bge-small-en-v1.5
#   models/qwen2.5/tokenizer.json                    from https://huggingface.co/Qwen/Qwen2.5-1.5B-Instruct
#   models/rerank-mxbai-int8/{model.onnx,tokenizer.json} = onnx/model_quantized.onnx + tokenizer.json from https://huggingface.co/mixedbread-ai/mxbai-rerank-xsmall-v1
#   runtime/{onnxruntime.dll,DirectML.dll}           from NuGet Microsoft.ML.OnnxRuntime.DirectML 1.20.1 / Microsoft.AI.DirectML 1.15.4
./build.sh            # builds and copies the exe to /mnt/c/Users/<you>/blackhole/
# Ask mode model (beside the exe): onnx/model_q4.onnx from https://huggingface.co/onnx-community/Qwen2.5-1.5B-Instruct,
# run through tools/last_logits.py so the prompt pass only returns the last token's logits
# (GenAI-builder exports additionally need tools/trim_gqa.py for ORT 1.20).
# cargo build --release --bin ortllm gives a console bench for the LLM path.
# Build memory: .cargo/config.toml caps cargo at 4 jobs — 24 parallel rustc on candle/tract at
# opt-level 3 can take down a 15 GB WSL VM. Avoid running a second heavy cargo build concurrently.
```

SQLite is bundled. `onnxruntime.dll` + `DirectML.dll` (from the `Microsoft.ML.OnnxRuntime.DirectML` 1.20.1 and
`Microsoft.AI.DirectML` 1.15.4 NuGet packages) live in `runtime/` (not committed) and are embedded at build time.

## Layout

| File | What |
|---|---|
| `src/main.rs` | Startup, message loop, ingest worker thread |
| `src/dot.rs` | The dot window: rendering, drag, hotkeys, menu, moods |
| `src/sprite.rs` | Procedural 32×32 pixel-art renderer |
| `src/drop.rs` | OLE `IDropTarget` and clipboard reading |
| `src/ingest.rs` | Text extraction per file type, chunk + embed on the worker thread |
| `src/chunk.rs` | Paragraph-aware overlapping chunker |
| `src/runtime.rs` | Unpacks and loads ONNX Runtime (+DirectML) dynamically |
| `src/embed.rs` | bge-small embedder (ONNX Runtime, DirectML→CPU) + WordPiece tokenizer |
| `src/rerank.rs` | mxbai cross-encoder reranker (ask mode only) |
| `src/expand.rs` | Vault-anchored synonym expansion of the dense query (ask mode only) |
| `eval/harness.py` | Retrieval measurement harness (Hit@k, latency) over a vault snapshot |
| `src/llm_ort.rs` | Qwen2.5 ONNX generation: DirectML prompt pass + CPU decode, prompt template |
| `src/gpu.rs` | Picks the DirectML adapter (most dedicated VRAM) via DXGI |
| `src/bin/ortllm.rs` | Console bench for the LLM path |
| `src/ask.rs` | Ask worker: retrieval → generation, streams tokens to the panel |
| `src/store.rs` | SQLite + FTS5 vault |
| `src/search.rs` | Search panel window |
| `src/config.rs` | Position / scale / settings / tutorial progress |
| `src/bubble.rs` | Pixel-art speech bubble window (tutorial + notifications) |
| `src/tray.rs` | System tray icon built from the sprite |
| `src/startup.rs` | Start-at-login (HKCU Run key) |
