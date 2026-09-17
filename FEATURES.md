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
- **Smoother particle motion** (digesting state): the specks currently jump pixel to pixel at the sprite's 32×32 grid and ~11 fps. — 2026-09-16: overlay is now four sub-pixels per sprite pixel (quarter-pixel steps) at 30 fps while specks move; speeds unchanged. Smooth them by moving along sub-pixel paths and rendering with 2×2 "half-pixel" steps at the output scale (still snapped to the pixel-art grid, just a finer one), easing velocity along the spiral, and raising the animation tick to ~20 fps only while particles are on screen. — done 2026-09-16

### 1.4 Opacity & unobtrusiveness — P1
- Optional idle opacity (e.g. 70%) that goes to 100% on hover. — done 2026-09-17: `config.idle_opacity` (100/85/70/50 %, cycled from Settings) is the layered window's constant alpha; it eases back to full over the same 150 ms whenever the dot is "engaged" (pointer over it, dragging, panel open, a bubble up, a notice queued, or any non-idle mood).
- Optional "shy" mode: shrinks to a few pixels when not hovered for a while. — done 2026-09-17: `config.shy` shrinks the sprite to ~8 screen px 3.5 s after the last engagement and eases it back the moment the pointer arrives; the sprite is re-sampled, not scaled, so it stays the same dot.

---

## 2. Window behaviour

### 2.1 Always on top — P0
- Floats above all other windows, including fullscreen apps where the platform allows.
- Does not take focus when clicked unless the search box is opened.
- Does not appear in the taskbar / dock / alt-tab list.
- Survives desktop switches / virtual desktops (visible on all of them).

### 2.2 Draggable — P0
- Click-and-drag moves the dot. Position is remembered across restarts.
- Optional edge snapping / corner docking. — done 2026-09-17: `config.snap` snaps each axis independently within 16 px of the current monitor's work area (per-monitor, so it docks to the screen the dot is actually on), minus the sprite's transparent margin so it looks flush.

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
- **Screenshot shortcut** (2026-09-16): see §3.6; a global shortcut (default Ctrl+Shift+S, rebindable in Settings since the same day) and a panel button start a drag-region capture. — done 2026-09-16

### 2.5 System tray icon — P1
- A tray icon (pixel black hole) so the app has a home when the dot is hidden, for start-at-login setup, and so Windows has somewhere to put it.
- Tray menu: show/hide dot, summon, open search, pause ingestion, start at login (checkbox), settings, quit. — pause done 2026-09-17 (the tray shares the dot's menu, so "Pause swallowing" is in both).
- Closing/hiding the dot never quits; quitting is explicit from the tray or dot menu.

### 2.6 Right-click menu — P1
- **Default view** submenu (2026-09-16): *Notes* or *Files* — which tab the panel opens on (left-click and Ctrl+Shift+Space). Ctrl+Shift+N always opens Notes. — done 2026-09-16
- Paste from clipboard
- Open search
- Recent items
- Pause ingestion — done 2026-09-17: "Pause swallowing" (menu checkbox + Settings row, `config.paused`) makes drops, paste, screenshots and MCP `put` refuse; the ring drains to grey, the halo dims and the specks stop, and one bubble says "Paused — I'm not swallowing anything. Click here to resume." whose body resumes.
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
- OCR — done 2026-09-16: PNG/JPEG images and screenshots are read with PP-OCRv4 (detector) + PP-OCRv3 English (recognizer) on ONNX Runtime/DirectML (`src/ocr.rs`, models compiled into the exe, ~14 MB); scanned PDFs have their page images (JPEG, raw RGB/grey, 1-bit) OCR'd when no text layer exists; existing images/PDFs re-extract on upgrade. Synthetic benchmark (eval/ocr, gitignored): 98 % word accuracy, ~0.5 s/image on the GPU. Not yet: other languages' dictionaries, CCITT/JBIG2 scans, rotated text.
| Type | Handling | Priority |
|---|---|---|
| Plain text, markdown, code | Direct | P0 |
| PDF | Text extraction | P0 |
| PDF (scanned) | OCR fallback | P1 |
| Images (png/jpg/webp/gif) | OCR + short caption via small vision model | P1 |
| Office docs (docx/xlsx/pptx) | Text extraction | P1 — done 2026-09-17 |
| HTML / URLs dropped from a browser | Fetch & extract readable text | P1 — done 2026-09-17 |
| Audio | Transcription via small on-device model | P2 |
| Anything else | Metadata + filename + any extractable strings | P0 |

- **Office documents** — done 2026-09-17: `src/office.rs` is a self-contained read-only zip reader (stored + deflate, zip64 refused, a 16 MiB inflate budget and a 512-part cap against zip bombs) plus an XML scanner. `.docx` keeps paragraphs and renders tables as ` | ` rows and skips tracked deletions; `.xlsx` resolves sharedStrings and walks the sheets in workbook order under `## <sheet name>` headings; `.pptx` walks slides numerically with speaker notes appended. Macro variants (`.docm/.xlsm/.pptm`) too; the file bytes are the identity and a copy is kept like a PDF. No new crates (`flate2` was already in the build for PNG). Not done: OpenDocument (`.odt/.ods/.odp`), and an xlsx formula only contributes its cached value.
- **Web pages and URLs** — done 2026-09-17: `src/web.rs` fetches with **WinHTTP** (http/https only, 10 s timeouts, 5 MB cap, gzip/deflate, redirects followed but never https→http, credentials stripped) and turns HTML into readable text — script/style/nav/header/footer/aside dropped, the main block chosen by text-to-tag ratio, headings kept as `#`-prefixed lines, tables as ` | ` rows. Hooks: a bare http(s) URL dropped or pasted as text, a link dragged or copied from a browser (`CFSTR_INETURLW`), `.url/.website/.webloc` shortcuts, local `.html/.htm/.xhtml` files (parsed offline, never fetched), and MCP `put { url }`. Items land as kind `web` with the final URL as source and identity, so re-swallowing a page updates it in place. Every fetch is logged; nothing is fetched that the user did not drop, paste or `put`.

### 3.6 Screenshot tool — P1 (2026-09-16)
- **Drag-region only**: a global shortcut (Ctrl+Shift+S) or the panel's camera button (done 2026-09-16) dims the screen; drag a rectangle; release swallows it. No full-screen or window modes — one gesture, on purpose. — done 2026-09-16
- The capture is ingested like a dropped image: PNG stored in the vault, findable by time/title, and OCR'd once §3.3's OCR lands so its text is searchable. It is also put on the clipboard (CF_DIB) so it pastes anywhere. A quiet speech bubble confirms for ~2.5 s without moving the dot ("Swallowed a screenshot, 412×188 — it's on the clipboard too"). — done 2026-09-16
- Esc cancels; multi-monitor and per-monitor DPI respected (capture in physical pixels). — done 2026-09-16

### 3.4 Storage policy — P1 — done 2026-09-17
- Choose per install: **copy** dropped files into the Blackhole vault, or **reference** the original path. — done 2026-09-17: `config.store_policy` = `copy` (default), `copy-small` (≤ 25 MB) or `reference`, cycled from the Settings row "Keep copies of files". Screenshots and pasted bitmaps are written into the vault in the first place, so they are never affected; under `reference` an item stops opening once its original is moved or deleted, and the row's hint says so.
- Default: copy for clipboard/text/images (there is no original), reference for files, with an option to always copy.
- Deduplicate by content hash. — 2026-09-16: files (images, PDFs) dedupe by their bytes; a re-dropped file is re-extracted anyway and the item's text is updated when it changed. Images and PDFs are always copied into `<data>\files\` (`items.stored`), the original path stays as `source`; opening prefers the original while it exists. Clipboard bitmaps are saved as PNG and swallowed like dropped images.

### 3.5 Feedback — P0
- Swallow animation on drop; processing animation while indexing; done pulse.
- Hover tooltip: "3 items indexing…" / "1,204 items inside".
- Failures show the error state and are listed in a small "undigested" list in settings. — done 2026-09-17: a new `undigested` table records path, title, error and time whenever ingest or MCP `put` cannot read a file (and a later success on the same path clears it). The Settings row "Undigested items  <n>" opens a Files-tab view with a warning icon, the age, the error in the theme's red and the source path; Enter/R retries through the ingest worker, Del dismisses, Esc returns. `extract_file` now fails loudly on a missing file instead of storing a 0-byte item.

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
- **Metadata filters**: type, date range, source. — done 2026-09-17: `kind:` / `type:` / `is:`, `since:` / `after:` and `source:` / `from:` / `in:` are parsed out of the query the way `#tag` already was (`store::parse_filters`). Kinds include pdf, note, image, text, file, code, docx/xlsx/pptx (and `kind:office`), web, with plural and plain-English aliases; dates take `7d`/`2w`/`3m`, `today`, `yesterday`, `week`, `month` or an ISO date; `source:` is a substring of the path or title. Filters alone browse in recent order, and the status line names the active scope ("3 of 9 items · browsing · kind:code"). An unknown value stays in the query text rather than being silently dropped.
- Target: search results in well under 100 ms for tens of thousands of chunks on a laptop.

### 4.2b Embedding model upgrade — evaluated 2026-09-15, bge-small kept
- bge-small (33M, 384-dim) misses paraphrase-level matches: "list my employers" does not reach a résumé that never uses the word.
- **bge-base-en-v1.5 tested** (110M, 768-dim, 435 MB fp32, ~600 ms load on DirectML, re-embeds the vault in ~3 s): top-document accuracy on a 5-question set was 3/5 plain, 4/5 with BGE's query instruction — versus bge-small's 4/5 without it — and it demoted the résumé below customs paperwork for "most recent job title". Neither model gets "list my employers". Pipeline verified exact against the official tokenizer + CPU ORT (cosine 1.0000), so this is model behaviour, not a bug.
- Next candidates when this is revisited: nomic-embed-text-v1.5 / gte-base / e5-base (different training mixes), query rewriting ("employers" → "work history, positions, companies"), or a small reranker over the top 20 chunks.

### 4.2c Retrieval accuracy programme — shipped 2026-09-15 (see [RAG.md](RAG.md) §2b)
Delivered after a nine-agent experiment round with a blind held-out set: title-only document units, stopwords out of the term boost, a lexical absent gate, and in ask mode a vault-anchored synonym expansion plus the mxbai int8 cross-encoder (k=10) deciding the top document. Live search 14/18 Hit@1, 18/18 Hit@3, ~5 ms; ask mode 18/18 Hit@1, ~110 ms retrieval. Cancelled by measurement: document-level RRF, chunk-size change, embedding-model change, LLM contextual enrichment.
Closed 2026-09-17 (RAG.md §2g): a 400-item / 3,264-chunk synthetic vault measures keystroke→ranked list at 35 ms p50 and 30/30 Hit@1; reranker cost is now measured (8.7 ms per pair) and `rerank::candidates()` derives k from a 400 ms budget, clamped 6–10 so a slow laptop can only trade k *down*; the absent rule was fixed for short queries (FTS5 does not stem, so "invoice" missed a stored "invoices" — `term_present` now tries singulars and a prefix match, turning two false refusals into hits while keeping every true refusal); and **Self-check** is a right-click menu item that runs `<data>\questions.json` (or a built-in five-question set over a scratch vault) through the real pipeline, reports PASS/FAIL in a bubble and writes `selfcheck.txt`.
Still open from that list: a third blind question set (~30) and an end-to-end answer eval with the 1.5B. The sibling-document confusion the scale run exposed ("Corvane-375" for a question about "Corvane-225") is closed 2026-09-17: a digit-bearing query token found in at most two items pins the ask context to those items (RAG.md §2g), self-check 29/30 → 30/30.

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
- Each result: type icon (pixel-art), snippet with the matching text highlighted, source name, date. — icons done 2026-09-17: a 7×7 two-layer pixel icon per kind (lined page, PDF page with a P, image frame, note with a folded corner, angle brackets for code, a W / grid / slide-stand for docx/xlsx/pptx, a globe for web, a warning triangle for undigested rows), drawn in the theme's accent and dim colours in place of the old `[kind]` text tag.
- Enter opens the original item; Ctrl+C copies the snippet; Ctrl+Enter reveals the file in the file manager.
- Keyboard-first navigation (up/down/enter).

### 5.3 Ask mode — P1
- Prefix a query with `?` (or toggle) to have the small model write a short answer from the top chunks, with citations back to the items.
- Streams the answer into the panel.

#### Ask-mode model (as shipped): Qwen3-4B, q4f16 ONNX, thinking, GPU-resident on ONNX Runtime + DirectML
- Chosen from Qwen2.5-1.5B/3B, Qwen3-1.7B/4B, Phi-4-mini and Llama 3.2 3B under the rule "same engine, runs on AMD/NVIDIA/Intel GPUs via DirectML, has a Copilot+ NPU story". Table in PACKAGING.md.
- **Thinking on by default** (2026-09-16): the prompt opens a `<think>` block and the model reasons privately for up to 128 tokens before the answer streams — 70 % → 82 % correct on the blind sets; 256 and 512 tokens scored no better. "Think before answering" in Settings turns it off for ~2 s direct answers.
- **GPU-resident decode — shipped 2026-09-16** (not parked): the prompt pass runs on a dynamic-shape DirectML session and decode on a second, *static-shape* session that DirectML fuses into one operator, sharing a fixed-capacity KV cache that never leaves the GPU — ~9 ms/token, ~50 tok/s end to end, 5 to 105 tok/s over the round. Memory work (2026-09-16) took the peak working set 8.7 → 6.1 GB and disk 3.25 → 2.73 GB by shrinking the tied embedding matrix (fp16 lookup, int4 LM head). On a machine with under ~7 GB of VRAM the app falls back to CPU decode on the same model. Details in PACKAGING.md § Memory and § GPU decode.

#### Ask-mode quality notes (from testing, 2026-09-15)
- Retrieval is solved for small documents by whole-document context; the remaining errors are the 1.5B model's reasoning (e.g. reading "the job before Maxar" off a correct timeline).
- Fixes in order of payoff: GPU inference (ONNX Runtime + DirectML, see PACKAGING.md) → run a 3B model at the same latency; then a "thinking" model (Qwen3) — its digit garbling was a candle issue, moot once candle is retired.

### 5.4 Recent & browse — P1 — done 2026-09-17
- Empty query shows the most recent items.
- Simple filters: images / text / files / this week. — done 2026-09-17: typed into the query box as `kind:images`, `kind:code`, `since:week`, `source:reports`, alone or with words and `#tag`s (see §4.2); the status line names the scope. Filter and tag words are stripped before the query embedding and before the absent gate, so `kind:pdf` alone browses instead of reading as a question about the word "pdf".

### 5.5 Panel sizing — P1
- **Resize to content**: the panel grows/shrinks to fit what it shows — result count, answer length, note height — up to a cap, instead of a fixed number of empty rows. Animated (§1.3 pace rule). — done 2026-09-16
- **Corner drag**: a pixel-art grip in the bottom-right corner; dragging it resizes the panel. The dragged size is saved to the config and restored on the next start; it becomes the cap for content-based resizing until dragged again. — done 2026-09-16
- **Restore the previous view** (2026-09-16): clicking away closes the panel but keeps its state — query text, results, streamed answer, open note, scroll position, size. The next open shows exactly what was there; Esc twice (or a new query) clears it. — done 2026-09-16

### 5.6 Rich results — P1
- **Code blocks**: snippets and previews from code files / fenced ```` ``` ```` blocks render monospace with the language tag, preserving indentation; a hit inside a code block shows the enclosing block, not a one-line fragment. — done 2026-09-17: the top 12 rows plan a multi-line snippet — a hit inside a fence shows the whole block up to 6 lines, a code file shows a window around the hit, drawn on the editor ground in the theme's code colours. The result list became owner-draw-variable so a row grows with its snippet.
- **Rich text formatting**: markdown headings, bold/italics, lists and links render styled in snippets and in an item preview pane, rather than as raw `#`/`*` markup; HTML clippings keep basic structure. — done 2026-09-17 in snippets: headings draw in the accent with the `#`, `**`, `_` and backtick marks stripped (`_` only where it is emphasis, so `snake_case` survives), list items get a pixel accent dot, and matched terms stay in the accent. PDFs, images and items over 400 KB keep the cheap one-line snippet.
- **Match highlighting**: the matched terms in a snippet are drawn in the accent colour (FTS5 already marks them; the panel currently strips the markers). — done 2026-09-16: list snippets draw FTS-marked runs in the accent colour; previews highlight query terms via Neovim `matchadd`
- **Preview pane**: expand a result (→ or Tab) to read the full item inline with the above formatting, without opening the source app. — done 2026-09-16: Tab shows the hit in a read-only Neovim buffer below the list, filetype from the extension (code and markdown coloured), Esc/Tab returns; needs the bundled nvim

### 5.6b On-theme scrollbars — P1
- The stock Windows scrollbars (grey, rounded, anti-aliased) break the pixel look on the result list and the answer box. Replace them app-wide with custom-drawn pixel-art scrollbars: dark track, 1-unit orange border, blocky thumb, no arrows (or 1-unit stepped arrows), sized in the same integer units as the dot. — done 2026-09-16
- Applies to every scrollable surface: results list, answer box, future preview pane and settings; long speech-bubble messages that overflow should scroll the same way. — bubbles done 2026-09-17: a bubble caps its body at 12 lines and draws the same pixel scrollbar in its own right-hand column (the × moves left of it); a click on the bar pages instead of dismissing, and because a bubble never takes focus a low-level mouse hook — installed only while a scrollable bubble is up — forwards wheel notches over it. Scrolling restarts the dismiss timer.
- Implementation: hide the native bars (`ShowScrollBar(..., FALSE)` / owner-draw the controls) and paint the bar in the parent, handling drag, wheel and keyboard so behaviour matches the native control.

### 5.7 Item actions — P1
- Open, copy, reveal, re-index, delete ("let it escape" — with confirmation). — done 2026-09-16 except re-index: Del asks and a second Del within 5 s forgets. — re-index done 2026-09-17: Ctrl+R (and a right-click item menu: Open / Reveal / Copy text / Re-index / Let it escape) re-extracts a pdf, image, Office document or plain file from its original — or Blackhole's copy — then re-chunks and re-embeds it; notes and pasted text keep their stored text and just re-embed. The status line reports `re-indexed "title" (n words)`, and a failure is recorded in the undigested list with its reason. A `web` item is deliberately not re-fetched.
- Tag / rename display title. — done 2026-09-16: `:Name <title>` (sticky) and `:Tag a, b` on notes and previewed items; tags shown on rows; `#tag` words filter searches in Files and Notes

### 5.8 Notes tab — built-in editor — P1 (2026-09-16)
- The panel gets two tabs: **Files** (today's search/ask view) and **Notes**: direct note-taking inside Blackhole. A note is a vault item like any other (searchable, askable, MCP-retrievable) that stays editable; saving is automatic on every pause. — done 2026-09-16 (pixel tab strip, "+" button, notes list above the editor, autosave 600 ms after a pause with re-embedding on a worker, Ctrl+Tab switches tabs, Ctrl+N new, Ctrl+Del forgets; a note found in Files opens in Notes)
- Ctrl+Shift+N summons the dot and opens a fresh note; the right-click **Default view** picks which tab opens otherwise (§2.6). — done 2026-09-16
- **Editor with LSP support**: the goal is a real editor, not a text box — syntax highlighting, completion and diagnostics for code snippets, markdown for prose. Options, to decide when this is built:
  1. **Package Neovim**: ship `nvim` (≈10 MB, MIT) and embed it — either a terminal control hosting `nvim --embed`/`--headless` over its msgpack-RPC UI protocol, drawn with the panel's pixel font (the "external UI" route Neovim supports natively), or launch it in a Windows Terminal/ConHost window positioned over the panel. Gives LSP, treesitter, the user's own config for free; the cost is a modal editor for non-vim users (a `-u` starter config with insert-mode defaults mitigates that).
  2. A native pixel-styled edit control with an LSP client speaking to external servers (`rust-analyzer`, `marksman`, …): full control of the look, much more work.
  Leaning to (1): it is the only way "LSP support" is honest at this project's size. — done 2026-09-16, option (1): `:w` saves, `:wq`/`:x`/ZZ save and start a new note, `:q` saves and closes, `:new`; titles from the first line (heading marks stripped) or "Note <date time>" when empty, `:Name <title>` for an explicit, sticky name; `src/nvim.rs` runs the stock `nvim.exe` shipped beside the app as `nvim --embed` and renders its grid (ext_linegrid) in the panel's font and palette; line numbers on, markdown by default with treesitter, LSP started for any server on PATH, insert mode for new notes, Esc in normal mode closes the panel, Ctrl+Tab/N/Del remain the panel's. The plain edit control stays as the fallback when nvim.exe is absent.
- Notes render with §5.6's rich formatting in the Files view and previews; the raw text is what's stored. — previews done 2026-09-16 (Neovim markdown colours)
- Notes follow-ups — done 2026-09-16: `:Copy`/Ctrl+Shift+C copies the note; `? question` in the Notes search box or `:Ask` answers from the open note only; `:Tidy` rewrites it as clean markdown (undoable); MCP `put` with `kind: "note"`; cursor position remembered per note and persisted in the vault; the user's own init.lua/init.vim loaded via a file browser (right-click → Editor) and sourced after Blackhole's; Neovim messages and the command line rendered in the panel's status line (ext_messages/ext_cmdline, cmdheight 0); IME composition window placed at the cursor and surrogate pairs handled.

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
- Queue if several arrive; unread count shown as a tiny badge on the dot. — done 2026-09-17: a 3×5 pixel digit ("9+" above nine) in the palette's accent on a dark outline at the sprite's top-right, drawn inside the normal render pass; it appears while notices wait behind a visible bubble and clears with the queue.
- Honours "Center on new message" (§2.6).

## 7. Settings — P1 — Settings tab shipped 2026-09-16
- Themes — done 2026-09-16: Blackhole, Dracula, Gruvbox, Nord, Catppuccin (Mocha), One Dark, Tokyo Night, Solarized Dark (`src/theme.rs`); the panel's brushes and the Neovim init palette follow; the dot and speech bubbles keep the Blackhole look. Chosen from the Settings tab (cycles), stored as `config.theme`.
- As built: a Settings tab in the panel (gear button / right-click → Settings…) with the four global hotkeys (click a row, press Ctrl/Alt/Win + key; registered immediately, "taken by another program" shown when Win32 refuses), think-before-answering, center on message, default view, start at sign-in, Neovim config. Hotkeys are stored as text in `config.json` (`hotkeys`), parsed by `src/hotkeys.rs`.
- Dot size, opacity, shy mode. — opacity and shy done 2026-09-17 ("Idle opacity" cycles 100/85/70/50 %, "Shy mode" on/off); dot size stays in the right-click menu.
- Sprite/theme selection (a few colour variants of the black hole). — done 2026-09-17: "Dot colours" cycles four palettes — **Ember** (the original), **Ice**, **Emerald** and **Violet** — each with its own ring, halo, listening colours and accent. The accent drives the speech bubbles, the unread badge and the tray icon (rebuilt on change); upset stays red in every palette, and the panel's theme is separate.
- Global shortcuts (summon, paste, show/hide). — done 2026-09-16 (four rebindable hotkey rows).
- Vault location, copy vs. reference policy. — done 2026-09-17: "Vault folder" opens a folder picker (a `Blackhole` subfolder is appended unless the pick already is one), confirms, then restarts via `blackhole.exe --move-vault <from> <to>`, which insists on a **rename** for ~4 s before allowing a copy+delete across volumes and reverts config and files if the move fails. `config.json` stays in the base directory; "Open vault folder" follows the move. "Keep copies of files" cycles the §3.4 policy.
- Model selection and download management. — done 2026-09-17: `src/models.rs` scans the exe folder, the vault and `<vault>\models\` (skipping half-downloaded folders that still hold a `.part`). "Ask model" shows the active model and its size on disk and cycles between the ones found, saving `config.model_name` and unloading the old session. With no model at all the row becomes "Download the default model": a WinHTTP download of the four pinned Qwen3-4B files (2.84 GB) on a background thread, with `.part` files, `Range:` resume across restarts, sha256 verified before each rename, a click to cancel, progress in the row, and ask mode live again without a restart.
- Start at login. — done 2026-09-16.
- Undigested/failed items list. — done 2026-09-17, see §3.5.
- **Pause swallowing** (2026-09-17) — see §2.6.
- **Snap to edges** (2026-09-17) — see §2.2.

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

As built: the running dot hosts a Streamable-HTTP endpoint on `127.0.0.1:47811` (bearer token in `%LOCALAPPDATA%\Blackhole\mcp.json`); `blackhole.exe --mcp` is a stdio proxy to it that starts the dot if needed. WSL clients run the same Windows exe through interop, so one command works everywhere. Tools shipped: `put` (text or path; `/mnt/c/...` paths are translated; tags; `kind: note`), `retrieve` (hybrid / keyword / semantic, tag and kind filters, best passage per hit), `get` (full item + stored path), `list_recent`, `forget`, `ask` (ask mode as a call, with sources), `notify` (bubble with `open_search` / `open_url` click actions and `timeout_ms`; "Agent bubbles" toggle in Settings mutes it). Resources: `blackhole://item/{id}` (list + read). Not done from the plan: a per-client allowlist (one global mute instead). `put { url }` — done 2026-09-17: the app fetches with WinHTTP and stores the page as a `web` item (§3.3). The rest of this section is the original plan.

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

- **CI on every push/PR** (`windows-latest` runner, native `x86_64-pc-windows-gnu` or MSVC target): `cargo fmt --check`, `cargo clippy -D warnings`, `cargo build --release`, unit tests for the pure parts (chunker, tokenizer, FTS query builder, store fusion). — done 2026-09-17: a `fmt` job against a new `rustfmt.toml` (`max_width = 200`, `use_small_heuristics = "Max"`, chosen by measuring the diff), and **86 unit tests** over the chunker, query expansion, the store (split_tags, fusion order, cosine window, FTS quoting, the ten absent-gate cases, over an in-memory vault), the PDF layout pass, hotkey parsing, themes and the reranker budget. `cargo check` for a Linux host is impossible (the `windows` crates do not compile off-Windows), so the cross-build job compiles the test binary with `cargo test --no-run` and the existing `windows-latest` job runs it before the smoke test — no extra toolchain. Two real bugs fell out of writing them: a tag plus words returned nothing, and the absent gate refused plurals.
- **Model & runtime fetch step**: the build embeds bge-small (`model.onnx`, `vocab.txt`), the Qwen tokenizers and the ORT/DirectML DLLs via `include_bytes!`, so CI downloads them (Hugging Face + NuGet, pinned versions/hashes) into `models/` and `runtime/` and caches them between runs (`actions/cache` keyed on the pin file).
- **Smoke test**: launch the exe headless-ish on the runner (`--version` / `--selftest` flag: load runtime on CPU EP, embed one sentence, open the vault, exit 0) so a broken DLL/model bundle fails CI, not the user.
- **Release on tag** (`v*`): build, zip `blackhole.exe` (DLLs are embedded), attach to a GitHub Release with checksums; optional second artefact with the default GGUF for the "Full" edition; later the installer from PACKAGING.md. Code-signing as a follow-up once a certificate exists.
- **Dependabot / cargo-audit** weekly. — done 2026-09-17: `.github/dependabot.yml` (cargo + github-actions, grouped, Mondays) and a separate `audit.yml` running `rustsec/audit-check` weekly, on `Cargo.lock`/`Cargo.toml` changes and on demand — its own file so the weekly run does not drag in MinGW and 3 GB of models.

## 11. Suggested build order

1. Dot window: transparent, always-on-top, draggable, pixel sprite, summon-to-cursor hotkey.
2. Drop + paste of plain text and files into a local vault.
3. Chunk + embed + hybrid search; basic search panel.
4. PDF extraction, images (OCR/caption), animations & states.
5. Ask mode, settings, storage policies, more formats.
6. Tray icon, speech bubbles (tutorial + notifications), center-on-message.
7. MCP server (put / retrieve / notify).
7b. CI/CD in GitHub Actions (§10) — build, smoke test, tagged releases.
8. **GPU inference** — move embeddings and the LLM onto **ONNX Runtime + DirectML** (dynamically loaded, no C++ build; one x64 binary for NVIDIA/AMD/Intel; NPU providers later). Retire tract and candle. Target: < 1 s to first token — met, and the model that shipped is Qwen3-4B q4f16 with thinking, decoding GPU-resident at ~50 tok/s. Phased plan in PACKAGING.md § GPU plan.

---

## Open decisions — settled 2026-09-17

- **Primary target platform**: Windows first (10 1903+ / 11, x64). Linux and macOS are roadmap, not v1 — §8's Wayland and accessibility caveats stand.
- **Framework**: raw Win32 — a layered per-pixel-alpha window for the dot, GDI for the panel and bubbles. No Tauri, no web view.
- **Sprite size**: a 32×32 sprite rendered procedurally every frame (no sprite sheets), scaled ×1–×4 with nearest-neighbour from the right-click menu; animations are computed, so "how many frames" does not arise.
- **Summon shortcut**: `Ctrl+Shift+Space` summons the dot to the cursor **and opens search**; press again to send it home. Rebindable in Settings, alongside paste (Ctrl+Shift+V), new note (Ctrl+Shift+N) and screenshot (Ctrl+Shift+S).
- **Models**: bge-small, the mxbai reranker and the two OCR models are compiled into the exe; the Qwen3-4B answer model ships inside the installer and, when it is absent (portable zip, or a trimmed install), can be downloaded from the Settings tab — see §7.

Still open: Copilot+ NPU execution providers (QNN / Ryzen AI / OpenVINO), code signing, a web installer that downloads the model at install time, and Linux/macOS ports.
