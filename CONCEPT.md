# Blackhole — Concept

*Captured 2026-09-14*

## One-line pitch

A small black dot that lives on your desktop. Throw anything into it — pictures, copied text, whole PDFs, any file — and it swallows it. Left-click the dot and you can search everything it has ever eaten.

## Core idea

Blackhole is a personal, local-first knowledge sink with a near-zero UI. There is no window, no library view, no folders to manage. There is only the dot and a search box.

- **Ingest by gravity.** Drag-and-drop or paste onto the dot. Files, images, text snippets, PDFs, and anything else are pulled in without dialogs or confirmation.
- **Understood, not just stored.** Each item is read by a very small on-device AI model and indexed by a highly efficient RAG (retrieval-augmented generation) engine.
- **Retrieved by intent.** Left-click the dot to open a search field. Queries are semantic — you describe what you remember, not the filename.

## The dot

- A small, always-on-top black circle. Draggable anywhere on the screen.
- Idle: a plain black dot.
- Receiving a drop: brief visual feedback (e.g. the dot pulses, ripples, or the item appears to fall into it).
- Processing: subtle indication that ingestion/indexing is in progress.
- Left-click: opens the search UI.
- Right-click (possible): settings, quit, storage location, model selection.

## Ingestion

Anything dropped or pasted onto the dot becomes a searchable item.

| Input | How it's read |
|---|---|
| Plain text / clipboard paste | Indexed directly |
| PDFs | Text extraction; OCR fallback for scanned pages |
| Images | Captioning / OCR via the small model so pictures become searchable by content |
| Office docs, markdown, code, etc. | Text extraction per format |
| Arbitrary files | Metadata + any extractable text; the file itself is kept for retrieval |

Every item stores: original content (or a reference to it), extracted text, chunks, embeddings, and metadata (source, timestamp, type).

## The engine

- **Very small AI model.** Runs entirely on-device. Used for embeddings, image captioning/OCR assistance, and lightweight answer synthesis. Small enough to run continuously in the background without noticeable resource use.
- **Super efficient RAG engine.** Fast chunking and embedding at ingest time; low-latency vector (plus keyword/hybrid) search at query time. Designed to handle thousands of items on a laptop with instant results.
- **Local-first and private.** Nothing leaves the machine by default.

## Search

- Left-click the dot → a minimal search field appears next to it.
- Type a natural-language query. Results appear as you type.
- Results show a snippet, the source item, and a way to open the original file or copy the text.
- Optionally, the small model synthesizes a short answer from the top results (true RAG), with citations back to the items.

## Design principles

1. **Zero friction in.** If it takes more than a drop or a paste, it's too much.
2. **Zero clutter.** The dot is the entire interface until you ask it something.
3. **Small and local.** Tiny model, tiny footprint, no cloud dependency.
4. **Everything is findable.** If it went in, it can be found — by meaning, not just by name.

## Open questions

- Which small model(s) to use for embeddings and captioning, and their size/quality trade-offs.
- Vector store choice (embedded DB vs. in-memory index with persistence).
- Whether to keep copies of dropped files or only references.
- Cross-platform target (Linux/Windows/macOS) and framework for the always-on-top dot.
- How to handle very large files or huge batches of drops.
- Whether items can ever be removed ("nothing escapes a black hole" vs. practical deletion).

## Possible name/theme hooks

- Dropping = "falling into the event horizon."
- Search = "Hawking radiation" — information getting back out.
