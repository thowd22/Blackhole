# Blackhole — Feature Plan

*Started 2026-09-15. Companion to [CONCEPT.md](CONCEPT.md).*

This document breaks the concept into concrete features, grouped by area, with a rough priority for each:

- **P0** — must exist for the app to be Blackhole at all
- **P1** — expected in a first real release
- **P2** — nice to have / later

---

## 1. The dot (visual identity)

### 1.1 Pixel-art black hole — P0
- The dot is not a flat circle; it's a small pixel-art black hole: a dark core with a thin accretion ring of a few pixels in dim colour (deep purple / orange / white specks).
- Rendered at a native pixel grid (e.g. 32×32 or 48×48 sprite) and scaled with nearest-neighbour so pixels stay crisp at 2×/3× on HiDPI screens.
- Transparent background — only the sprite pixels are drawn, no window chrome, no square frame.

### 1.2 Size — P0
- Small. Target ≈ 40–64 px on screen (user-adjustable in settings: small / medium / large).
- Must never feel like a "window"; it should read like a desktop sticker.

### 1.3 Animation states — P1
Sprite-sheet animations, a few frames each, low frame rate (8–12 fps) to keep the pixel feel and CPU near zero.

| State | Animation |
|---|---|
| Idle | Slow rotation of the accretion ring; occasional single-pixel twinkle |
| Drag hover (something is being dragged over it) | Ring brightens / expands slightly, pulling inward |
| Swallow (item dropped) | Item shrinks into the core; ring flashes; a couple of pixels spiral in |
| Processing (indexing) | Ring rotates faster; a small dim orbiting speck |
| Ready / done | Brief bright pulse, then back to idle |
| Error | Ring flickers red for a moment |
| Search open | Ring holds steady and slightly brighter |

- **Animation pace rule (2026-09-16)**: every transition (mood change, panel open/close, bubble in/out, resize) is animated — fast but visible, ~120–180 ms, eased, still stepped on the pixel grid; nothing snaps and nothing lingers. — done 2026-09-16 (dot mood changes and bubble in/out; panel open/close/resize belong to the panel block)
- **Thinking animation** (2026-09-16): while the LLM is reasoning or generating, the dot shows it — ring pulses in a slow "breathing" rhythm with a single bright speck orbiting once per second, and the answer box shows a pixel ellipsis that ticks; both stop the instant the first answer token streams. Replaces the static "thinking over N excerpts…" line as the primary signal (the line stays as detail). — done 2026-09-16 (dot half: Mood::Thinking breathing ring + orbiting speck, ended by WM_ASK_FIRST_TOKEN; the panel's pixel ellipsis belongs to the panel block)
- **Smoother particle motion** (digesting state): the specks currently jump pixel to pixel at the sprite's 32×32 grid and ~11 fps. Smooth them by moving along sub-pixel paths and rendering with 2×2 "half-pixel" steps at the output scale (still snapped to the pixel-art grid, just a finer one), easing velocity along the spiral, and raising the animation tick to ~20 fps only while particles are on screen. — done 2026-09-16

### 1.4 Opacity & unobtrusiveness — P1
- Optional idle opacity (e.g. 70%) that goes to 100% on hover.
- Optional "shy" mode: shrinks to a few pixels when not hovered for a while.

---

## 2. Window behaviour

### 2.1 Always on top — P0
- Floats above all other windows, including fullscreen apps where the platform allows.
- Does not take focus when clicked unless the search box is opened.
- Does not appear in the taskbar / dock / alt-tab list.
- Survives desktop switches / virtual desktops (visible on all of them).

### 2.2 Draggable — P0
- Click-and-drag moves the dot. Position is remembered across restarts.
- Optional edge snapping / corner docking.

### 2.3 Summon to mouse (keyboard shortcut) — P0
- A global hotkey (default suggestion: `Ctrl+Shift+Space` or `Super+B`) instantly moves the dot to the current mouse cursor position.
- Behaviour options:
  - **Move**: the dot jumps to the cursor and stays there.
  - **Move + open search**: jumps to the cursor and opens the search field immediately (probably the most useful default).
  - **Toggle**: press once to summon, press again to send it back to its previous position.
- Small "warp" animation when it arrives (dot appears from a single pixel and expands).
- Multi-monitor aware — follows the cursor to whichever screen it's on.

### 2.4 Other global shortcuts — P1
- Show/hide the dot.
- Paste clipboard directly into Blackhole without dragging (ingest the current clipboard contents).
- Open search from anywhere without moving the dot.
- **Ctrl+Shift+N — summon to mouse and open a new note** (2026-09-16): the dot warps to the cursor, the panel opens on the Notes tab with an empty note ready to type into (see §5.8). — done 2026-09-16
- **Screenshot shortcut** (2026-09-16): see §3.6; a global shortcut (default Ctrl+Shift+S, falling back to Ctrl+Alt+S when another program owns it — logged, and the menu label follows) and a panel button start a drag-region capture. — done 2026-09-16

### 2.5 System tray icon — P1
- A tray icon (pixel black hole) so the app has a home when the dot is hidden, for start-at-login setup, and so Windows has somewhere to put it.
- Tray menu: show/hide dot, summon, open search, pause ingestion, start at login (checkbox), settings, quit.
- Closing/hiding the dot never quits; quitting is explicit from the tray or dot menu.

### 2.6 Right-click menu — P1
- **Default view** submenu (2026-09-16): *Notes* or *Files* — which tab the panel opens on (left-click and Ctrl+Shift+Space). Ctrl+Shift+N always opens Notes. — done 2026-09-16
- Paste from clipboard
- Open search
- Recent items
- Pause ingestion
- **Center on new message** (checkbox) — when a message/notification arrives (see §9), the dot warps to the center of the active monitor and shows the bubble there, instead of showing it wherever the dot currently sits.
- Settings
- Quit

---

## 3. Ingestion

### 3.1 Drag-and-drop — P0
- Accept file drops (single or multiple) and text/image drops from browsers and other apps.
- Multiple files dropped at once are queued and processed in the background.
- Dropping a folder ingests its contents (with a confirmation for large folders).

### 3.2 Paste — P0
- Ctrl+V while the dot is hovered/focused, or via the right-click/global-shortcut paste.
- Handles text, rich text (HTML → text), images, and file lists on the clipboard.

### 3.3 Supported content — P0 / P1
| Type | Handling | Priority |
|---|---|---|
| Plain text, markdown, code | Direct | P0 |
| PDF | Text extraction | P0 |
| PDF (scanned) | OCR fallback | P1 |
| Images (png/jpg/webp/gif) | OCR + short caption via small vision model | P1 |
| Office docs (docx/xlsx/pptx) | Text extraction | P1 |
| HTML / URLs dropped from a browser | Fetch & extract readable text | P1 |
| Audio | Transcription via small on-device model | P2 |
| Anything else | Metadata + filename + any extractable strings | P0 |

### 3.6 Screenshot tool — P1 (2026-09-16)
- **Drag-region only**: a global shortcut (Ctrl+Shift+S) or the panel's camera button dims the screen; drag a rectangle; release swallows it. No full-screen or window modes — one gesture, on purpose. — done 2026-09-16
- The capture is ingested like a dropped image: PNG stored in the vault, findable by time/title, and OCR'd once §3.3's OCR lands so its text is searchable. It is also put on the clipboard (CF_DIB) so it pastes anywhere. A quiet speech bubble confirms for ~2.5 s without moving the dot ("Swallowed a screenshot, 412×188 — it's on the clipboard too"). — done 2026-09-16
- Esc cancels; multi-monitor and per-monitor DPI respected (capture in physical pixels). — done 2026-09-16

### 3.4 Storage policy — P1
- Choose per install: **copy** dropped files into the Blackhole vault, or **reference** the original path.
- Default: copy for clipboard/text/images (there is no original), reference for files, with an option to always copy.
- Deduplicate by content hash.

### 3.5 Feedback — P0
- Swallow animation on drop; processing animation while indexing; done pulse.
- Hover tooltip: "3 items indexing…" / "1,204 items inside".
- Failures show the error state and are listed in a small "undigested" list in settings.

---

## 4. The engine (small AI + RAG)

### 4.1 On-device small model — P0
- A very small embedding model (tens of MB, e.g. a MiniLM / bge-small / nomic-embed-class model) for chunk embeddings.
- Runs on CPU by default; uses GPU/NPU if available.
- Optional slightly larger small model (≈1–3B, quantized) for answer synthesis and image captioning — P1, and clearly optional so the base install stays tiny.

### 4.2 Efficient RAG pipeline — P0
- **Chunking**: structure-aware (paragraphs/headings/pages) with overlap; small chunks for precision.
- **Embeddings**: computed at ingest, stored in a compact local vector index (e.g. SQLite + a vector extension, or a small HNSW index persisted to disk).
- **Hybrid search**: vector similarity + keyword/BM25 so exact names, numbers and code identifiers still hit.
- **Metadata filters**: type, date range, source.
- Target: search results in well under 100 ms for tens of thousands of chunks on a laptop.

### 4.2b Embedding model upgrade — evaluated 2026-09-15, bge-small kept
- bge-small (33M, 384-dim) misses paraphrase-level matches: "list my employers" does not reach a résumé that never uses the word.
- **bge-base-en-v1.5 tested** (110M, 768-dim, 435 MB fp32, ~600 ms load on DirectML, re-embeds the vault in ~3 s): top-document accuracy on a 5-question set was 3/5 plain, 4/5 with BGE's query instruction — versus bge-small's 4/5 without it — and it demoted the résumé below customs paperwork for "most recent job title". Neither model gets "list my employers". Pipeline verified exact against the official tokenizer + CPU ORT (cosine 1.0000), so this is model behaviour, not a bug.
- Next candidates when this is revisited: nomic-embed-text-v1.5 / gte-base / e5-base (different training mixes), query rewriting ("employers" → "work history, positions, companies"), or a small reranker over the top 20 chunks.

### 4.2c Retrieval accuracy programme — shipped 2026-09-15 (see [RAG.md](RAG.md) §2b)
Delivered after a nine-agent experiment round with a blind held-out set: title-only document units, stopwords out of the term boost, a lexical absent gate, and in ask mode a vault-anchored synonym expansion plus the mxbai int8 cross-encoder (k=10) deciding the top document. Live search 14/18 Hit@1, 18/18 Hit@3, ~5 ms; ask mode 18/18 Hit@1, ~110 ms retrieval. Cancelled by measurement: document-level RRF, chunk-size change, embedding-model change, LLM contextual enrichment.
Still open: a third blind question set (~30), reranker cost on 4-core laptops (derive k from a measured per-pair budget), a ~400-item vault, the absent rule against one/two-word queries, an end-to-end answer eval with the 1.5B, and a "Self-check" menu item that runs the harness questions in-app.

### 4.3 Background & resource behaviour — P0
- Indexing runs at low priority; the UI never blocks.
- Idle CPU ≈ 0, RAM small enough to leave running permanently.
- Batching for large drops; progress visible via tooltip.

### 4.4 Privacy — P0
- Fully local. No network access unless the user enables a specific feature (e.g. URL fetching).
- Vault directory is user-chosen and can be encrypted at rest (P2).

---

## 5. Search UI

### 5.1 Open / close — P0
- Left-click the dot (or the summon shortcut) opens a compact search field anchored beside the dot.
- Pixel-art styled to match: monospace / bitmap font, dark panel, thin bright border.
- Esc or clicking elsewhere closes it.

### 5.2 Results — P0
- Live results as you type.
- Each result: type icon (pixel-art), snippet with the matching text highlighted, source name, date.
- Enter opens the original item; Ctrl+C copies the snippet; Ctrl+Enter reveals the file in the file manager.
- Keyboard-first navigation (up/down/enter).

### 5.3 Ask mode — P1
- Prefix a query with `?` (or toggle) to have the small model write a short answer from the top chunks, with citations back to the items.
- Streams the answer into the panel.

#### Ask-mode model (2026-09-15): Llama 3.2 3B Instruct, int4 ONNX, hybrid DirectML/CPU
- Chosen from Qwen2.5-1.5B/3B, Qwen3-1.7B/4B, Phi-4-mini and Llama 3.2 3B under the rule "same engine, runs on AMD/NVIDIA/Intel GPUs via DirectML, has a Copilot+ NPU story". Table in PACKAGING.md.
- Memory (2026-09-16): peak working set 8.7 → 6.1 GB and decode 7.0 → 8.4 tok/s by shrinking the tied embedding matrix (fp16 embedding lookup, int4 LM head); disk 3.25 → 2.73 GB after repack. GPU-resident decode (would reach ~3.7 GB) is parked: its crash is fixed but DirectML's GQA cache convention is not yet reproduced. Details in PACKAGING.md § Memory.

#### Ask-mode quality notes (from testing, 2026-09-15)
- Retrieval is solved for small documents by whole-document context; the remaining errors are the 1.5B model's reasoning (e.g. reading "the job before Maxar" off a correct timeline).
- Fixes in order of payoff: GPU inference (ONNX Runtime + DirectML, see PACKAGING.md) → run a 3B model at the same latency; then a "thinking" model (Qwen3) — its digit garbling was a candle issue, moot once candle is retired.

### 5.4 Recent & browse — P1
- Empty query shows the most recent items.
- Simple filters: images / text / files / this week.

### 5.5 Panel sizing — P1
- **Resize to content**: the panel grows/shrinks to fit what it shows — result count, answer length, note height — up to a cap, instead of a fixed number of empty rows. Animated (§1.3 pace rule). — done 2026-09-16
- **Corner drag**: a pixel-art grip in the bottom-right corner; dragging it resizes the panel. The dragged size is saved to the config and restored on the next start; it becomes the cap for content-based resizing until dragged again. — done 2026-09-16
- **Restore the previous view** (2026-09-16): clicking away closes the panel but keeps its state — query text, results, streamed answer, open note, scroll position, size. The next open shows exactly what was there; Esc twice (or a new query) clears it. — done 2026-09-16

### 5.6 Rich results — P1
- **Code blocks**: snippets and previews from code files / fenced ```` ``` ```` blocks render monospace with the language tag, preserving indentation; a hit inside a code block shows the enclosing block, not a one-line fragment.
- **Rich text formatting**: markdown headings, bold/italics, lists and links render styled in snippets and in an item preview pane, rather than as raw `#`/`*` markup; HTML clippings keep basic structure.
- **Match highlighting**: the matched terms in a snippet are drawn in the accent colour (FTS5 already marks them; the panel currently strips the markers).
- **Preview pane**: expand a result (→ or Tab) to read the full item inline with the above formatting, without opening the source app.

### 5.6b On-theme scrollbars — P1
- The stock Windows scrollbars (grey, rounded, anti-aliased) break the pixel look on the result list and the answer box. Replace them app-wide with custom-drawn pixel-art scrollbars: dark track, 1-unit orange border, blocky thumb, no arrows (or 1-unit stepped arrows), sized in the same integer units as the dot. — done 2026-09-16
- Applies to every scrollable surface: results list, answer box, future preview pane and settings; long speech-bubble messages that overflow should scroll the same way.
- Implementation: hide the native bars (`ShowScrollBar(..., FALSE)` / owner-draw the controls) and paint the bar in the parent, handling drag, wheel and keyboard so behaviour matches the native control.

### 5.7 Item actions — P1
- Open, copy, reveal, re-index, delete ("let it escape" — with confirmation).
- Tag / rename display title.

### 5.8 Notes tab — built-in editor — P1 (2026-09-16)
- The panel gets two tabs: **Files** (today's search/ask view) and **Notes**: direct note-taking inside Blackhole. A note is a vault item like any other (searchable, askable, MCP-retrievable) that stays editable; saving is automatic on every pause. — done 2026-09-16 (pixel tab strip, "+" button, notes list above the editor, autosave 600 ms after a pause with re-embedding on a worker, Ctrl+Tab switches tabs, Ctrl+N new, Ctrl+Del forgets; a note found in Files opens in Notes)
- Ctrl+Shift+N summons the dot and opens a fresh note; the right-click **Default view** picks which tab opens otherwise (§2.6). — done 2026-09-16
- **Editor with LSP support**: the goal is a real editor, not a text box — syntax highlighting, completion and diagnostics for code snippets, markdown for prose. Options, to decide when this is built:
  1. **Package Neovim**: ship `nvim` (≈10 MB, MIT) and embed it — either a terminal control hosting `nvim --embed`/`--headless` over its msgpack-RPC UI protocol, drawn with the panel's pixel font (the "external UI" route Neovim supports natively), or launch it in a Windows Terminal/ConHost window positioned over the panel. Gives LSP, treesitter, the user's own config for free; the cost is a modal editor for non-vim users (a `-u` starter config with insert-mode defaults mitigates that).
  2. A native pixel-styled edit control with an LSP client speaking to external servers (`rust-analyzer`, `marksman`, …): full control of the look, much more work.
  Leaning to (1): it is the only way "LSP support" is honest at this project's size. — done 2026-09-16, option (1): `src/nvim.rs` runs the stock `nvim.exe` shipped beside the app as `nvim --embed` and renders its grid (ext_linegrid) in the panel's font and palette; line numbers on, markdown by default with treesitter, LSP started for any server on PATH, insert mode for new notes, Esc in normal mode closes the panel, Ctrl+Tab/N/Del remain the panel's. The plain edit control stays as the fallback when nvim.exe is absent.
- Notes render with §5.6's rich formatting in the Files view and previews; the raw text is what's stored.

---

## 6. Speech bubbles — P1

Small pixel-art speech bubbles anchored to the dot. One rendering component, two uses:

### 6.1 First-run tutorial
- On first install, a short sequence of bubbles walks through the basics, one step at a time, each waiting for the action it describes:
  1. "Hi. Drop a file or some text on me." → waits for the first swallow
  2. "Click me to search what I've eaten." → waits for the first search
  3. "Ctrl+Shift+Space summons me to your mouse." → waits for the first summon
  4. "Start a search with ? to ask me a question." → waits for the first ask
  5. "Right-click for size, tray, and settings. That's it."
- Skippable at any point (Esc or a small ×); never shown again once completed or skipped (flag in config). Re-runnable from the menu ("Show tutorial").

### 6.2 Messages & notifications
- The same bubbles display messages: ingest results worth knowing ("Couldn't read scan.pdf — no text layer"), and messages pushed by external tools over MCP (§9 `notify`).
- Bubbles auto-dismiss after a timeout, or on click; a click on an MCP message can carry an action (open a search, open a URL, run the associated tool's follow-up).
- Queue if several arrive; unread count shown as a tiny badge on the dot.
- Honours "Center on new message" (§2.6).

## 7. Settings — P1
- Dot size, opacity, shy mode.
- Sprite/theme selection (a few colour variants of the black hole).
- Global shortcuts (summon, paste, show/hide).
- Vault location, copy vs. reference policy.
- Model selection and download management.
- Start at login.
- Undigested/failed items list.

---

## 8. Platform notes (for the always-on-top pixel dot)

The dot needs: a frameless, transparent, always-on-top, non-focus-stealing window, global hotkeys, and cursor position on demand. Candidates:

- **Tauri (Rust + web view)** — small binaries, cross-platform, supports transparent always-on-top windows and global shortcuts; engine in Rust fits the "efficient" goal.
- **Electron** — easiest, but heavy; conflicts with the "small" spirit.
- **Native per platform / Rust GUI (egui, iced, winit)** — smallest footprint, most control over pixel rendering, more work.

Platform quirks to plan for:
- Linux: X11 vs. Wayland differ on always-on-top, global hotkeys and cursor position (Wayland restricts both).
- Windows: fine for all requirements; fullscreen games may still cover it.
- macOS: needs accessibility permission for global hotkeys; floating panel window level works well.

---

## 9. MCP support — P1 — shipped 2026-09-16 (`src/mcp.rs`)

Blackhole exposes itself as a local **MCP server** so agents and tools (Claude Code, IDE assistants, scripts) can use the vault as memory.

As built: the running dot hosts a Streamable-HTTP endpoint on `127.0.0.1:47811` (bearer token in `%LOCALAPPDATA%\Blackhole\mcp.json`); `blackhole.exe --mcp` is a stdio proxy to it that starts the dot if needed. WSL clients run the same Windows exe through interop, so one command works everywhere. Tools shipped: `put` (text or path; `/mnt/c/...` paths are translated), `retrieve` (hybrid or keyword, best passage per hit), `notify`. The rest of this section is the original plan.

- Transport: stdio (spawned by the client) and/or a local HTTP/SSE endpoint on localhost with a token; nothing listens off-machine.
- Tools:
  - **`put`** — swallow content: `{ text | path | url, title?, tags? }` → item id. Goes through the same ingest pipeline (extract, chunk, embed) and shows the swallow animation.
  - **`retrieve`** — search: `{ query, limit?, mode?: "hybrid" | "keyword" | "semantic", filters? }` → ranked hits with title, snippet, source, and the best-matching chunk text, so an agent can do RAG over the user's vault.
  - **`notify`** — show a speech bubble: `{ text, title?, action?: { open_search: query | open_url: url }, timeout_ms? }`. Triggers "center on new message" if enabled.
  - Later: `get` (full item), `forget`, `list_recent`, `ask` (ask mode as a tool).
- Resources: `blackhole://item/{id}` for full content.
- Security: put/retrieve are local-only; an allowlist of client names and a per-client "may notify" toggle in settings so a noisy tool can be muted.
- Ships as a `--mcp` flag on the same exe (no second binary): the running dot instance handles requests, so the animation and bubbles reflect agent activity.

## 10. CI/CD — GitHub Actions — P1 — shipped 2026-09-16 (`.github/workflows/ci.yml`)

As built: one workflow. Every push/PR cross-compiles the Windows exe on `ubuntu-latest` with MinGW (the dev
flow), runs clippy, then a `windows-latest` job runs `blackhole.exe --selftest` (runtime unpack, embed, scratch
vault add + search, CPU provider). Embedded assets come from `ci/fetch-assets.sh` (sha256-pinned, cached).
Tags `v*` add a release job: `ci/prepare-model.sh` downloads Qwen3-4B and runs the graph tools, Inno Setup
builds the installer, and a GitHub Release gets the portable zip, the installer split into <2 GiB parts
(GitHub's per-asset cap) and `SHA256SUMS.txt`. The workflow file carries notes on the Linux/macOS plumbing.
The plan as written follows.

- **CI on every push/PR** (`windows-latest` runner, native `x86_64-pc-windows-gnu` or MSVC target): `cargo fmt --check`, `cargo clippy -D warnings`, `cargo build --release`, unit tests for the pure parts (chunker, tokenizer, FTS query builder, store fusion).
- **Model & runtime fetch step**: the build embeds bge-small (`model.onnx`, `vocab.txt`), the Qwen tokenizers and the ORT/DirectML DLLs via `include_bytes!`, so CI downloads them (Hugging Face + NuGet, pinned versions/hashes) into `models/` and `runtime/` and caches them between runs (`actions/cache` keyed on the pin file).
- **Smoke test**: launch the exe headless-ish on the runner (`--version` / `--selftest` flag: load runtime on CPU EP, embed one sentence, open the vault, exit 0) so a broken DLL/model bundle fails CI, not the user.
- **Release on tag** (`v*`): build, zip `blackhole.exe` (DLLs are embedded), attach to a GitHub Release with checksums; optional second artefact with the default GGUF for the "Full" edition; later the installer from PACKAGING.md. Code-signing as a follow-up once a certificate exists.
- **Dependabot / cargo-audit** weekly.

## 11. Suggested build order

1. Dot window: transparent, always-on-top, draggable, pixel sprite, summon-to-cursor hotkey.
2. Drop + paste of plain text and files into a local vault.
3. Chunk + embed + hybrid search; basic search panel.
4. PDF extraction, images (OCR/caption), animations & states.
5. Ask mode, settings, storage policies, more formats.
6. Tray icon, speech bubbles (tutorial + notifications), center-on-message.
7. MCP server (put / retrieve / notify).
7b. CI/CD in GitHub Actions (§10) — build, smoke test, tagged releases.
8. **GPU inference** — move embeddings and the LLM onto **ONNX Runtime + DirectML** (dynamically loaded, no C++ build; one x64 binary for NVIDIA/AMD/Intel; NPU providers later). Retire tract and candle. Target: < 1 s to first token so Qwen2.5-3B int4 fits today's latency budget. Phased plan in PACKAGING.md § GPU plan.

---

## Open decisions

- Primary target platform for the first version (Windows? Linux X11/Wayland? macOS?).
- Framework choice from §7.
- Exact sprite size and how many animation frames.
- Default summon shortcut and whether summoning also opens search.
- Which small models ship by default vs. are downloaded on demand.
