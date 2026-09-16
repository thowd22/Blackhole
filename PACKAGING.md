# Blackhole — Windows Target, Packaging & Acceleration

*2026-09-15. Companion to [CONCEPT.md](CONCEPT.md) and [FEATURES.md](FEATURES.md).*

## Decisions

- **First platform: Windows** (10/11). Linux/macOS later.
- **Self-contained distribution.** No runtime model downloads. The user gets one thing and it works offline from first launch.
- **Two binaries, split by CPU architecture, not GPU vendor:**
  - `blackhole-x64` — Intel/AMD PCs, all GPUs via DirectML.
  - `blackhole-arm64` — Snapdragon X Copilot+ PCs, NPU via QNN.
- **Runtime accelerator detection** inside each binary: NPU → GPU → CPU, with graceful fallback.

## Packaging

### What ships where

| Component | Size (approx) | Placement |
|---|---|---|
| App + engine (Rust) | 10–30 MB | inside exe |
| ONNX Runtime + DirectML / QNN | 20–60 MB | inside exe (static link) or DLLs beside it |
| Embedding model (bge-small / MiniLM / nomic-embed, int8) | 30–130 MB | **embedded** via `include_bytes!` |
| OCR model (small, e.g. PaddleOCR / RapidOCR ONNX) | 10–20 MB | embedded |
| Image caption / answer LLM (1–3B, Q4) | 0.7–2 GB | **beside the exe**, inside the installer |
| `onnxruntime.dll` + `DirectML.dll` | ~35 MB | beside the exe (see GPU plan) |

### Two editions
- **Lite** — single exe, ~150–250 MB. Embedding + OCR only. Search works fully; no "ask mode", no image captions beyond OCR.
- **Full** — single-file installer (~1.5–2.5 GB) that unpacks the exe plus the LLM. Same exe as Lite; it detects the model file next to it and enables ask mode / captioning.

Why the LLM is not compiled into the exe: a multi-GB executable is slow for antivirus to scan on every launch, awkward to code-sign, and forces a full re-download on every app update. Keeping it as a sibling file preserves "everything in one download" without those costs.

### Installer
- Single-file installer (e.g. Inno Setup / NSIS / MSIX) or a portable zip.
- Options: start at login, vault location.
- Code-sign the exe and installer (SmartScreen warnings otherwise).
- Updates: delta-update the exe only; model files are versioned and only replaced when changed.

## Acceleration

### Inference stack
- **ONNX Runtime** for embeddings, OCR, the vision encoder and the LLM, loaded dynamically at runtime (`ort` `load-dynamic`).
- ~~llama.cpp + Vulkan for the LLM~~ — dropped 2026-09-15; the LLM runs on ONNX Runtime too (int4 ONNX). See "GPU plan".

### Per-binary execution providers

| Binary | Hardware | EP / backend | Notes |
|---|---|---|---|
| x64 | NVIDIA / AMD / Intel GPU | ONNX Runtime **DirectML** (embeddings, LLM, OCR) | One build, all vendors. Covers Intel Arc and AMD iGPUs too. |
| x64 | Intel Lunar Lake / AMD Ryzen AI NPU | (optional) OpenVINO EP / Vitis AI EP | Not for v1; DirectML on their iGPU is good enough and simpler. |
| x64 | No usable GPU | ONNX Runtime CPU EP | Always available fallback. |
| arm64 | Snapdragon X NPU (Hexagon) | ONNX Runtime **QNN** EP | Copilot+ path. Needs QNN-compatible quantized models. |
| arm64 | Adreno GPU | DirectML (ORT) | Fallback when the NPU can't run a model. |
| arm64 | CPU | CPU EPs | Final fallback. |

### Why not one binary per GPU vendor
- DirectML already abstracts NVIDIA/AMD/Intel. A CUDA-specific build gains maybe 5–20% on NVIDIA but adds ~1 GB of CUDA/cuDNN libraries and a second x64 artefact to test. Not worth it for a small embedding model that already runs in milliseconds.
- The only hardware that truly needs a different build is ARM64 (Snapdragon), and that split is unavoidable regardless of accelerators.

### Runtime selection
1. On startup, probe: QNN NPU (arm64) → DirectML device → CPU.
2. Benchmark once (a few embedding calls) and cache the choice in settings; expose an override in Settings → Acceleration.
3. Per-model fallback: if a model fails to load on the NPU (unsupported op), retry on GPU, then CPU, without user intervention.
4. Show the active accelerator in the right-click menu / settings ("Running on: NVIDIA RTX 4070 (DirectML)").

### Alternative considered: Windows ML
Windows ML (Windows App SDK) ships ONNX Runtime and automatically downloads the right EP (NVIDIA, AMD, Intel, Qualcomm) for the machine — exactly the "one binary, any hardware" goal. Rejected for v1 because the EPs are fetched at runtime, which breaks the fully-offline, everything-included requirement. Revisit if that constraint relaxes.

## GPU plan (decided 2026-09-15): ONNX Runtime + DirectML, no llama.cpp

### Why
- **One runtime for everything.** Embeddings (bge-small, already ONNX), the LLM, and later OCR/captioning all run on ONNX Runtime. `tract` and `candle` go away — a smaller, single-path stack.
- **No C++ toolchain.** The `ort` crate in `load-dynamic` mode loads `onnxruntime.dll` at runtime; nothing is linked at build time, so the WSL → mingw cross-compile stays exactly as it is.
- **All vendors in one x64 binary.** DirectML runs on NVIDIA, AMD and Intel GPUs. NPUs later are just other execution providers (QNN on ARM64, OpenVINO / Vitis AI on x64) behind the same API.
- **Ready-made models.** Qwen2.5-Instruct (0.5B / 1.5B / 3B / 7B) and Phi-3.5-mini exist as int4 ONNX exports (ONNX Runtime GenAI model-builder format: `onnx-community/…-ONNX`, `microsoft/…-onnx` DirectML variants).

### What we ship
| File | Size | Where |
|---|---|---|
| `onnxruntime.dll` (with DirectML EP) | ~20 MB | beside the exe (or embedded and extracted to `%LOCALAPPDATA%\Blackhole\bin` on first run, keeping the single-exe download) |
| `DirectML.dll` | ~15 MB | same |
| `bge-small` ONNX (embeddings) | 133 MB fp32 → target int8 ~35 MB | embedded |
| `mxbai-rerank-xsmall-v1` int8 + tokenizer (ask-mode reranker) | 92 MB | embedded |
| LLM ONNX int4 (Qwen2.5-1.5B → 3B once GPU is fast) | 1–2 GB | beside the exe, as today |

If the runtime DLL is missing or fails to initialise, the app falls back to the CPU execution provider of the same runtime — no separate code path.

### Phases
1. ~~**Embeddings on ORT**~~ — **done 2026-09-15**: `tract` replaced by `ort` (`load-dynamic`, `api-20`, ORT 1.20.1 + DirectML 1.15.4). DLLs embedded in the exe and unpacked beside it (fallback: vault `bin/`), `SetDllDirectory` so ORT finds DirectML. Inputs padded to 32/64/128/256-token buckets to limit DML shape recompiles. Loads in ~340 ms on DirectML. Still to do from this phase: int8 bge (RAM diet).
2. ~~**LLM on ORT + DirectML**~~ — **done 2026-09-15**, with findings that changed the design:
   - Model: `onnx-community/Qwen2.5-1.5B-Instruct` `model_q4.onnx` (int4 weights, fp32 activations, 1.8 GB), with a last-token Slice inserted before the LM head (`tools/last_logits.py`). The `q4f16` variant produces NaNs on DirectML (Qwen2.5 overflows fp16) — avoid.
   - DirectML picks the *integrated* GPU by default on a dual-GPU machine; `gpu.rs` enumerates DXGI adapters and pins the one with the most dedicated VRAM (RX 9070 XT: 0.9 s to first token vs 12 s on the iGPU).
   - Prompt pass on DirectML is 5–6× faster than CPU (1,300 tokens: 1.1 s vs 6.4 s). Decode on DirectML is a flat ~120 ms/token regardless of context — per-op dispatch on this unfused dynamic-shape graph; neither IO binding (cache resident on GPU) nor `ep.dml.disable_graph_fusion` changed it. CPU decode runs 10–22 tok/s.
   - Therefore **hybrid**: prompt pass on the GPU with outputs bound to CPU memory, decode on the CPU session. Costs two sessions (~2 GB RAM + ~2 GB VRAM while loaded; unloaded 60 s after the panel closes).
   - Fast GPU decode needs a static-shape GenAI-style export (GroupQueryAttention, fixed-length cache shared between past and present) — see phase 2b.
2b. **Static-cache DML model** — export with ONNX Runtime GenAI's model builder (`-e dml -p int4`), bind a fixed max-length cache in and out, so DirectML compiles once and decodes at 30+ tok/s. Also enables 3B/7B at interactive speed.
3. **Bigger model — evaluated 2026-09-15, not adopted yet.** Qwen2.5-3B-Instruct int4 (ONNX Runtime GenAI export, `keisuke-miyako/Qwen2.5-3B-Instruct-onnx-int4`, 3.0 GB; needs `tools/trim_gqa.py` for ORT 1.20 and `tools/last_logits.py`). Findings:
   - The GenAI layout (GroupQueryAttention, no `position_ids`) is now supported by the loop and runs on the CPU provider. Its fp32 GQA kernel is rejected by DirectML at run time (`80070057`), so the prompt pass falls back to CPU: 10–15 s to first text vs ~1 s for the 1.5B on the GPU.
   - Answer quality on the test vault: cleaner, well-structured timelines and correct "most recent title", but still wrong on "the job before Maxar" (picked Raytheon, skipped AWS) and refused the shipping-cost question the 1.5B answers. Not a clear win for 2× the latency and ~3 GB more RAM.
   - Decision: 1.5B stays the default. The 3B (or 7B) becomes worthwhile only with phase 2b — an fp16 static-cache DirectML export where GQA runs on the GPU.
   - Retrieval limit surfaced during testing: bge-small ranks customs paperwork above the résumé for "list all my employers with dates" (the résumé never says "employer"). Term boosts are now IDF-weighted so common words can't drag junk up, and the top document is scored over its best three chunks; the remaining gap needs a stronger embedding model (try bge-base, 768-dim) — tracked in FEATURES.md §4.
4. **Accelerator selection UI** — probe DirectML device → CPU; show "Running on: <adapter>" in the menu; manual override in settings.
5. **NPU** — ARM64 build with the QNN EP for Snapdragon X; evaluate OpenVINO / Vitis AI EPs on x64 Copilot+ machines.

### Risks and mitigations
- *DirectML op coverage for int4 (`MatMulNBits`)* — supported since ORT 1.17; use a current ORT release and GenAI-builder models targeted at DML.
- *KV-cache plumbing* — the per-layer past/present tensors are verbose but mechanical; test against the same questions as today (timeline, shipping cost) before switching the default.
- *DLL distribution* — pin ORT and DirectML versions together; verify on Windows 10 1903+ (DirectML floor).
- *No GPU / old driver* — CPU EP fallback; ORT's int4 CPU kernels are expected to beat candle anyway.

### Rejected alternatives
- **llama.cpp + Vulkan** — fastest on paper, but a C++/Vulkan-SDK build inside the mingw cross-compile, and a second inference stack next to the ONNX one.
- **burn + wgpu** — pure Rust and vendor-neutral, but f16 only in practice (1.5B ≈ 3 GB VRAM), Qwen would need porting, and embeddings/OCR would stay on a different runtime.
- **candle CUDA** — NVIDIA-only and needs the CUDA toolkit at build time.
- **Windows ML (Windows App SDK)** — downloads execution providers at runtime; conflicts with the offline single-download requirement. Revisit if that relaxes.

## LLM selection round (2026-09-15) — same engine, vendor-neutral models only

Constraint: ONNX on ONNX Runtime + DirectML (AMD / NVIDIA / Intel GPUs) with model families that
Qualcomm, AMD and Intel also ship for Copilot+ NPUs; no CUDA-only exports. Measured end to end with
`askeval.exe` (real pipeline → LLM) on 18 answerable + 2 absent questions (`eval/questions*.json`).

| Model (int4 ONNX) | Correct | TTFT | tok/s | Peak WS | Disk | Notes |
|---|---|---|---|---|---|---|
| Qwen2.5-1.5B-Instruct (shipped before) | 9/18 | 0.72 s | 10.9 | 9.9 GB | 1.7 GB | over-refuses; weak on multi-part questions |
| Qwen3-1.7B | 9/18 | 0.75 s | 10.1 | 12.6 GB | 2.0 GB | no gain |
| Phi-4-mini-instruct (Microsoft GPU export, fp16 GQA) | 10/18 | 0.69 s | 8.6 | **5.5 GB** | 3.3 GB | refuses often; lowest memory |
| **Llama-3.2-3B-Instruct** (fp32, explicit rotary) | **14/18** | 1.05 s | 7.6 | 9.1 GB | 3.2 GB | **chosen** |
| Llama-3.2-3B-Instruct (fp16, explicit rotary) | 14/18 | 0.63 s | 4.6 (fp16 on CPU) | 8.5 GB | 2.3 GB | GPU-decode candidate |
| Qwen3-4B (fp16 only export) | 15/18 | 1.10 s | 4.1 (fp16 on CPU) | 8.8 GB | 2.7 GB | best accuracy; NPU support spotty; no fp32 export |
| Qwen2.5-3B (GenAI CPU export) | mixed | 10–15 s | — | — | 3.0 GB | fp32 GQA fails on DirectML |
| Phi-4-mini (community q4) | — | — | — | — | 2.6 GB | needs GatherBlockQuantized (ORT ≥ 1.21) |

Findings that generalise:
- **DirectML's GroupQueryAttention kernel silently returns prompt-independent output when rotary is done inside
  the op (`do_rotary=1`).** Every GenAI-style export that fails on DirectML shares this; moving rotary to explicit
  `RotaryEmbedding` nodes (`tools/explicit_rotary.py`) fixes it for fp32 and fp16 alike. Microsoft's own GPU
  exports already use the explicit form.
- Runtime `Transpose` of the tied 1.5 GB embedding matrix in the LM head (`tools/gemm_head.py` → `Gemm(transB)`)
  and full-sequence logits (`tools/last_logits.py`) are both worth removing from any export before use.
- fp16 exports decode slowly on the CPU provider (4–5 tok/s vs 7–8 fp32): pick fp32 for the hybrid
  (GPU prompt / CPU decode) or decode on the GPU.
- Peak working set is dominated by holding two sessions (CPU + DirectML) plus fp32 embedding tables; the
  GPU-only decode mode (`BLACKHOLE_DECODE=gpu`) exists to trade tok/s for RAM.

Model preparation recipe (all graph-only, no weight rewrite): `last_logits.py` → `gemm_head.py` (if the LM head
transposes at runtime) → `explicit_rotary.py` (if GQA has `do_rotary=1`) → `trim_gqa.py` (GenAI graphs on ORT 1.20).

## Windows-specific implementation notes

- **Dot window**: `WS_EX_LAYERED | WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE` — transparent, always on top, no taskbar entry, doesn't steal focus. Per-pixel alpha via `UpdateLayeredWindow` or a DirectComposition surface.
- **Pixel rendering**: draw the sprite at native size to an offscreen buffer, scale with nearest-neighbour by the DPI scale factor rounded to an integer (1×, 2×, 3×).
- **Global hotkey**: `RegisterHotKey`; summon uses `GetCursorPos` + `MonitorFromPoint` for multi-monitor placement.
- **Drag-and-drop**: OLE `IDropTarget` for files (`CF_HDROP`), text (`CF_UNICODETEXT`), HTML, and bitmaps; browsers drop images as URLs or bitmaps.
- **Clipboard**: same formats via the Win32 clipboard API.
- **Fullscreen apps**: exclusive-fullscreen games will cover the dot; borderless-fullscreen is fine. Accept this.
- **Startup**: registry `Run` key or Task Scheduler entry.
- **Storage**: vault under `%LOCALAPPDATA%\Blackhole\` by default; SQLite for metadata + FTS5 for keyword search; vector index as an on-disk HNSW (or `sqlite-vec`) file.

## Framework choice (Windows v1)

**Recommendation: Rust end-to-end.**
- Window/dot: `winit` + raw Win32 for the layered/topmost flags, or Tauri with a transparent always-on-top window if a web view is preferred for the search panel.
- Engine: `ort` (ONNX Runtime bindings, `load-dynamic`), `rusqlite` + FTS5, in-memory vector index (brute force; HNSW when the vault outgrows it).
- Single static exe, models via `include_bytes!`, no runtime dependencies beyond GPU drivers.

Tauri is the fallback if the search panel UI grows beyond what an immediate-mode Rust GUI is comfortable with — it still produces a small binary and supports the same window flags.

## Open questions

- Which exact embedding model is best under QNN (NPU) *and* DirectML — pick one that quantizes cleanly for both so the two binaries share model files.
- Whether to ship the LLM in Q4 only or offer Q8 for high-end GPUs.
- Portable-zip vs. installer as the primary download.
- Minimum Windows version (DirectML needs Windows 10 1903+; QNN needs Windows 11 on ARM).
