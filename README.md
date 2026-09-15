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
- Hybrid search: keyword + semantic (cosine over chunks), score-fused (keyword weight scales with query-term coverage);
  results are tagged `≈` (semantic), `≈=` (both) or untagged (keyword)
- Search panel: live results, ↵ open, Ctrl+↵ reveal in Explorer, Ctrl+C copy text, Del forget
- Ask mode: start a query with `?` and press ↵ — Qwen2.5-1.5B-Instruct (GGUF, via `candle` on CPU) answers
  from a ~1,400-word context: the best-matching chunks with their neighbours in document order, or the whole
  document when the top hit is small (a résumé, a receipt). Streams into an answer box; Esc stops.
  Questions about order/time get a forced "Timeline:" scratchpad first, then "Answer:".
  The model is optional: the app looks for a `*.gguf` next to the exe (or in the vault folder). It is
  pre-loaded when the search panel opens (~3 s, so a question typed a moment later is instant) and unloaded
  60 s after the panel closes (~1.1 GB RAM while loaded). The exact prompt of the last question is written to
  `%LOCALAPPDATA%\Blackhole\last_ask.txt` for debugging odd answers.
  Known limit: the 1.5B model lists dates correctly but often misreads before/after off its own timeline;
  a larger model needs GPU inference (see PACKAGING.md). Qwen3 GGUFs load too (architecture detected) but
  garble digits under candle 0.11 — treat as experimental.

- Tray icon (the sprite) with the same menu; **Show dot** toggles dot visibility, **Start at login**
  writes the per-user Run key, **Center on new message** warps the dot to screen centre for notifications
- Pixel-art speech bubbles: a 6-step first-run tutorial (each step waits for the action it describes;
  the orange × skips the tour; "Show tutorial" in the menu replays it) and notifications (ingest failures now; MCP `notify` later).
  Notifications pre-empt a tutorial step and it resumes afterwards; click a bubble to dismiss.
- Diagnostics: `%LOCALAPPDATA%\Blackhole\log.txt` (ingest failures), `last_ask.txt` (last ask prompt)

Not yet: OCR & image captions, GPU/NPU acceleration, panel resizing / rich snippets, MCP server (see FEATURES.md).

## Building (from WSL)

```sh
sudo apt install mingw-w64
rustup target add x86_64-pc-windows-gnu
# model files (not committed):
#   models/bge-small-en-v1.5/{model.onnx,vocab.txt}  from https://huggingface.co/BAAI/bge-small-en-v1.5
#   models/qwen2.5/tokenizer.json                    from https://huggingface.co/Qwen/Qwen2.5-1.5B-Instruct
#   models/qwen2.5/qwen2.5-1.5b-instruct-q4_k_m.gguf from https://huggingface.co/Qwen/Qwen2.5-1.5B-Instruct-GGUF
./build.sh            # builds and copies the exe to /mnt/c/Users/<you>/blackhole/
# copy the .gguf next to the exe to enable ask mode
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
| `src/llm.rs` | Qwen2.5 GGUF generation via candle, prompt template |
| `src/ask.rs` | Ask worker: retrieval → generation, streams tokens to the panel |
| `src/store.rs` | SQLite + FTS5 vault |
| `src/search.rs` | Search panel window |
| `src/config.rs` | Position / scale / settings / tutorial progress |
| `src/bubble.rs` | Pixel-art speech bubble window (tutorial + notifications) |
| `src/tray.rs` | System tray icon built from the sprite |
| `src/startup.rs` | Start-at-login (HKCU Run key) |
