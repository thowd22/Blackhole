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

### Installer (shipped 2026-09-16; built by CI on tags since the same day)

Release assets: GitHub caps each asset at 2 GiB and the installer is ~3 GB with the model inside, so the
release job splits it into 1900 MB parts (`copy /b a.part0 + a.part1 a.exe` to re-join) next to a portable
exe zip and `SHA256SUMS.txt`. A web installer that downloads the model on install (Inno's
`DownloadTemporaryFile`) would remove the split; it needs a host for the prepared model (a Hugging Face repo
or the release parts themselves) and is the natural next step for packaging.

- **Inno Setup 6** script `installer/blackhole.iss`, built from WSL by `build-installer.sh` (stages exe + DLLs +
  model under `dist/stage`, runs `ISCC.exe`). Output `dist/Blackhole-<version>-x64-setup.exe`, ≈2.9 GB; the int4
  model data is stored uncompressed (it does not compress), everything else lzma2.
- Per-user install by default (`PrivilegesRequired=lowest`, override to all-users allowed); Start-menu shortcut,
  optional desktop shortcut and "start at sign-in" task (same HKCU Run value the app's menu manages); closes a
  running instance on install, kills it on uninstall; asks before deleting the vault on uninstall.
- The exe carries the pixel-art icon and version info (`build.rs` → mingw `windres`).
- Still to do: code-signing (SmartScreen), delta updates (exe only; model versioned separately), an MSIX/Store variant.

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
- fp16 exports decode slowly on the CPU provider (4–5 tok/s vs 7–8 fp32) but are strictly better on the GPU;
  the shipped model is the fp16 graph.
- Peak working set in the hybrid was dominated by holding two sessions (CPU + DirectML) plus fp32 embedding
  tables; GPU decode (now the default, see below) drops the CPU session.

Model preparation recipe (all graph-only, no weight rewrite): `last_logits.py` → `gemm_head.py` (if the LM head
transposes at runtime) → `explicit_rotary.py` (if GQA has `do_rotary=1`) → `trim_gqa.py` (GenAI graphs on ORT 1.20).

## Memory (2026-09-16): what the ask-mode footprint is made of, and what moved it

Measured with `askeval.exe` (peak working set, Llama 3.2 3B int4, hybrid DirectML prompt / CPU decode):

| Change | Peak WS | Decode | Verdict |
|---|---|---|---|
| as shipped (two sessions, fp32 tied embedding matrix) | 8.7 GB | 7.0 tok/s | baseline |
| CPU memory arena off (`arena0`) | 8.2 GB | 5.2 tok/s | not worth −25 % speed |
| weight prepacking off (`prepack0`) | 6.6 GB | **0.3 tok/s** | never — MatMulNBits needs packed weights |
| memory pattern off / device-allocated initializers | 8.7 GB | — | no effect |
| **embedding → fp16 Gather + Cast, LM head → int4 MatMulNBits** (`tools/shrink_embeddings.py`) | **6.1 GB** | **8.4 tok/s** | shipped; disk 3.25 → 2.73 GB after `tools/repack.py` |
| GPU-resident decode, no CPU session (`BLACKHOLE_DECODE=gpu`) | 2.2–3.7 GB | 5–6 tok/s dynamic, **~105 tok/s static** | **shipped** — see "GPU decode" below |

Where the hybrid's ~6 GB went: the DirectML session keeps ~2.5 GB of host memory alongside VRAM, the CPU
session holds the int4 weights plus their prepacked copies (~2.5 GB), embeddings + reranker ~0.6 GB, the rest
is KV cache and arenas. The GPU-decode path below removes the CPU session; the remaining lever is an ORT
upgrade whose GQA/`GatherBlockQuantized` support lets the embedding itself be int4 (another ~0.75 GB).

Model preparation recipe, in order: `last_logits.py` → `gemm_head.py` → `explicit_rotary.py` (GQA exports with
`do_rotary=1`) → `shrink_embeddings.py` (fp32 embeddings only) → `logit_index.py` → `repack.py` (one data
file, dead bytes dropped). All graph edits are streaming/low-memory and never rewrite the int4 weights.

## GPU decode (2026-09-16): from 5 to 105 tok/s on the same engine

Everything runs on DirectML, so this works unchanged on AMD, NVIDIA and Intel GPUs, and on the Copilot+
iGPUs; NPU providers plug in as further execution providers later.

What the profiler said (`BLACKHOLE_LLM_OPTS=profile`, ORT's chrome trace): a decode step on the dynamic-shape
graph was 186 ms, of which 166 ms was **CPU-side dispatch of ~400 DirectML operators, ~0.4 ms each**
(MatMulNBits 350 µs, RotaryEmbedding 890 µs, SkipSimplifiedLayerNorm 435 µs) and 0 ms on the CPU provider.
Not arithmetic: fp16 vs fp32 activations, a fixed-capacity mask, `ep.dml.enable_graph_capture`,
`ep.dml.enable_cpu_sync_spinning` and `ep.dml.disable_graph_fusion` all left it at 5.3–5.5 tok/s.

What fixed it: **pin every symbolic dimension** (`AddFreeDimensionOverrideByName`: `batch_size=1`,
`sequence_length=1`, `past_sequence_length=total_sequence_length=4096`). With static shapes DirectML
compiles the decoder into one fused operator at session load (~1.5 s extra) and a step is one dispatch:
**9 ms, 105–112 tok/s** on the RX 9070 XT, answers byte-identical to the CPU path.

Things learned on the way, so nobody repeats them:

- **Padding a step corrupts DirectML's GroupQueryAttention.** A 64-token static width with one live token
  plus 63 pads (mask counting the pads, so GQA appends after the live rows) produced "IQuestionQuestion…"
  from the second token on, fused or not. Width 1 is exact. So the prompt cannot go through the static
  session in chunks; it runs on a second, dynamic-shape DirectML session (~1 s for 1,300 tokens at fp16).
  Both sessions bind the same fixed-capacity cache buffers as past *and* present (GenAI's shared-buffer
  convention), so the prompt's cache is simply there when decode starts.
- **The last-token pick must be an ordinary tensor op.** `logit_index.py` turns the `Slice(-1)` from
  `last_logits.py` into `Gather(axis=1, indices=logit_index)` fed by an int64 graph input; Slice starts/ends
  are CPU-side inputs and cannot vary inside a fused graph. The loader recognises the `logit_index` input
  and only enables static shapes for graphs that have it.
- Two DirectML sessions do **not** double host memory: peak working set is 3.7 GB either way (2.2 GB for one
  dynamic session; the fused static graph accounts for the rest). VRAM holds the weights twice (~4.6 GB fp16).
- fp16 activations (`model_q4f16`) beat fp32 on the GPU everywhere: prompt pass 1.0 s vs 6.8 s, 2.3 vs 2.7 GB
  on disk, same answers. fp32 was only ever preferred for CPU decode, which this path no longer does.

| Path | TTFT | Decode | Peak WS | Disk |
|---|---|---|---|---|
| hybrid: DirectML prompt, CPU decode (fp32 graph) | 1.2 s | 7–8 tok/s | 6.1 GB | 2.7 GB |
| DirectML decode, dynamic shapes (fp16) | 1.1 s | 5.4 tok/s | 2.2 GB | 2.3 GB |
| **DirectML decode, static shapes + dynamic prompt session (fp16)** | **1.1 s** | **~105 tok/s** | 3.7 GB | 2.3 GB |

Default: GPU decode whenever a DirectML adapter with ≥ 7 GB of dedicated VRAM exists (two copies of the
weights live in VRAM: DirectML cannot share initializers between sessions — investigated 2026-09-16,
RAG.md §2f); smaller cards fall back to the hybrid automatically. `BLACKHOLE_DECODE=cpu` restores the hybrid;
`BLACKHOLE_KV_CAP` sets the cache capacity (4096 tokens ≈ 0.5 GB VRAM at fp16 for the 3B); `BLACKHOLE_SEQ`
sets the static width (leave at 1). No GPU → CPU session for everything, as before.

## Reasoning models (2026-09-16): Qwen3-4B with a thinking budget

The GPU path made reasoning affordable: a 256-token think costs ~5 s at 50 tok/s instead of 40 s on the
CPU. Candidates that fit the constraints (ONNX Runtime + DirectML, families with Copilot+ NPU support):
Qwen3 (hybrid think mode, budgetable), DeepSeek-R1-Distill-Qwen (Microsoft ships DirectML/NPU builds, but
thousands of thinking tokens with no off switch), Phi-4-mini-reasoning (math-tuned, long chains; the
instruct sibling already over-refused here). Qwen3-4B it is.

Implementation (`llm_ort.rs`): the prompt ends in an open `<think>\n` block; tokens up to `</think>` are kept
private; when the budget (`BLACKHOLE_THINK`, tokens) runs out, `</think>\n\n` is force-fed and the model
answers from what it has. Without a budget the empty think block selects Qwen3's direct mode, as before.
The temporal `Timeline:` prefill is skipped when thinking — the model does that itself.

| Model / mode (all 44 questions, same retrieval) | Answers | Absent | TTFT | Decode | Peak WS | Disk |
|---|---|---|---|---|---|---|
| Llama 3.2 3B, direct (shipped before) | 31/44 | 6/6 | 0.9 s | 100 tok/s | 3.8 GB | 2.3 GB |
| Qwen3-4B, direct | 29/44 | 6/6 | 2.1 s* | 47 tok/s | 5.5 GB | 2.7 GB |
| **Qwen3-4B, think ≤128** | **36/44 (82 %)** | 6/6 | 5.0 s | 41–50 tok/s | 5.5–7.7 GB | 2.7 GB |
| Qwen3-4B, think ≤256 | 35/44 (36: one right answer failed the regex) | 6/6 | 5.8 s | 50 tok/s | 5.4 GB | 2.7 GB |
| Qwen3-4B, think ≤512 | 35/44 | 6/6 | 7.4 s | 49 tok/s | 5.6 GB | 2.7 GB |

*after the band fix below; 7 s before it. Qwen3 direct refuses more often than Llama on the same
contexts (9 "I couldn't find that" with the right document), which thinking mostly cures. What the
thinking flipped: the timeline/ordering questions (q08, h07 and friends), the multi-field customs
questions (b25, q04's sibling), the decoy misreads (b12 stays). 23 of 54 thinks hit the 256 budget, yet
128 scores the same or better: the model needs a short pass to order dates or pick the right field, not a
long one. Default budget 128; the right-click menu's "Think before answering" turns it off (direct mode,
~2 s to first text, fewer correct answers).

### DirectML GroupQueryAttention has a slow band — pad the prompt past it

Prompt passes of 1,850–2,620 tokens took 8–14 s at cache capacity 4,096 while both shorter (~1,000) and
longer (≥2,622) prompts took 1–2 s; at capacity 6,144 the slow band moved to ~2,700–3,900, at 3,072 the
same prompts were all fast. The profile shows layer 0's GQA spending 0.7 s on the CPU side (compiling an
alternative kernel) and the GPU work then taking ~9 s: the metacommand path is abandoned when the prompt
is roughly 45–64 % of the buffer. Peak working set also balloons (9 GB vs 5 GB) on that path.

Fix: a prompt whose length falls in [0.42, 0.66) × capacity is padded with EOS tokens up to 0.66 ×
capacity before the prompt pass. Real tokens never attend the pads (causal mask); the logits are taken at
the last real token (`logit_index`); the first decode step writes at the real length and overwrites the
pads' cache rows. 2,114-token prompt: 10 s → 1.7 s, answers identical. `BLACKHOLE_PAD_BAND=0` disables.
Chunking the prompt pass instead would need the multi-token dynamic session for every chunk and was not
pursued.

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
