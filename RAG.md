# Blackhole — RAG Research & Scaled-Down Design

*2026-09-15. Companion to [FEATURES.md](FEATURES.md) §4–5 and [PACKAGING.md](PACKAGING.md).*

## 1. What the state of the art actually is

Production RAG in 2025–26 has converged on a pipeline, not a model. Every serious writeup and paper
lands on the same stages, in this order of payoff:

| Stage | What it does | Reported effect |
|---|---|---|
| **Hybrid retrieval** (dense + BM25, fused) | catches paraphrase *and* exact names/numbers | "mandatory for production"; consistent nDCG gains ([Starmorph](https://blog.starmorph.com/blog/rag-techniques-compared-best-practices-guide), [Anthropic](https://platform.claude.com/cookbook/capabilities-contextual-embeddings-guide)) |
| **Cross-encoder reranking** of the top-k | re-scores (query, passage) pairs jointly | +10–25 % precision on top of hybrid; ~33 % avg accuracy gain over 8 benchmarks ([Substack survey](https://nandigamharikrishna.substack.com/p/7-advanced-rag-techniques-that-deliver)) |
| **Contextual chunk enrichment** | prepend a document-situating sentence to each chunk before embedding | −35 % retrieval failures alone, −67 % with reranking ([Anthropic](https://platform.claude.com/cookbook/capabilities-contextual-embeddings-guide)) |
| **Sane chunking** | recursive/fixed 256–512 tokens, 10–20 % overlap; *not* tiny semantic chunks | recursive 512 = 69 % end-to-end vs semantic 54 % ([PremAI 2026 benchmark](https://www.premai.io/blog/rag-chunking-strategies-the-2026-benchmark-guide/), [Vectara NAACL 2025](https://arxiv.org/html/2603.06976)) |
| **Parent / hierarchical context** | retrieve small, hand the LLM the parent section or whole doc; RAPTOR summarises upward | fixes "answer spans chunks" and whole-document questions ([RAPTOR](https://github.com/parthsarthi03/raptor)) |
| **Query understanding** | rewriting / multi-query; HyDE | helps weak readers; HyDE adds 25–60 % latency and hallucinates on personal, fact-bound data — skip ([Query Rewriting](https://arxiv.org/abs/2305.14283), [HyDE](https://www.emergentmind.com/topics/hypothetical-document-embeddings-hyde)) |
| **Evaluation set** | synthetic Q/A per document, Hit@k / MRR | the thing that makes every other knob tunable ([Evidently](https://www.evidentlyai.com/llm-guide/rag-evaluation), [Can we evaluate RAGs with synthetic data?](https://arxiv.org/html/2508.11758v2)) |

Two findings matter most for a small local app:

- **Small embeddings + a reranker beat a bigger embedding model alone.** ([Rethinking Hybrid Retrieval, 2025](https://arxiv.org/pdf/2506.00049)) — architecture offsets model size. This is exactly what our bge-base test showed: 300 MB more model, no gain.
- **Lightweight rerankers match LLM rerankers on in-distribution queries at a fraction of the cost.** ([How Good are LLM-based Rerankers?, 2025](https://arxiv.org/pdf/2508.16757)) — no need to burn the 1.5B on reranking.

Late-interaction (ColBERT) and multi-vector retrieval are the research frontier
([LIR @ ECIR 2026](https://arxiv.org/abs/2511.00444)) but need per-token vectors (≈100× the index) and degrade
on long queries; not for a 400-item personal vault.

## 2. What we measured on the real vault (9 items, 57 chunks)

Reference harness: Python + official tokenizers + CPU ONNX Runtime. The app's DirectML vectors match it at
cosine 1.0000, so these numbers are about the method, not the code.

| Pipeline | Top-document correct |
|---|---|
| bge-small, title+chunk (shipped today) | 7/8 → 10/12 → 12/14 depending on set |
| bge-base instead | worse (3/5; 4/5 with query instruction) |
| + ms-marco-MiniLM-L-6 reranker over top-20 chunks | +1 (fixes "what did I do at Jacobs") |
| + LLM-generated chunk contexts (Qwen 1.5B) | 0 here; pushed one match *down* |
| + title/opening prepended to chunks (heuristic) | −2 |
| + **document-level title vectors** in the pool | +1 (fixes "list all my employers") |
| **doc-level fusion: embeddings + reranker, RRF at document level** | **13/14** |

The single stubborn failure is document-level ("list all my employers"): no chunk of a résumé says
"employer", but the *document* is obviously the answer. Chunk-only indexes cannot see that; a
document-level unit can.

## 2b. Ultracode experiment round (2026-09-15) — what survived a blind held-out set

Nine parallel agents, one technique each, against a 10-question dev set; a blind agent wrote 10 held-out
questions; a judge re-ran the decisive comparisons on an idle machine with real query embeddings
(`eval/harness.py` is the merged harness; `eval/results/judge-*.json`).

**Every dev-set "9/9" was two questions wide and most did not survive the held-out set** — the
dev↔held-out Hit@1 correlation across the ten proposals was *negative*. What did hold:

| change | dev H@1 | held H@1 | held H@3 | verdict |
|---|---|---|---|---|
| shipped baseline | 7/9 | 7/9 | 8/9 | reference, 4.6 ms |
| **document-level RRF** (`rrf_docs`), alone | 7/9 | **5/9** | 7/9 | **regression** — RRF throws away score magnitude; junk keyword ranks tie with 0.73-cosine matches. Do not replace the score blend. |
| **title-only document units** under the existing score fusion | 7/9 | 7/9 | **9/9** | keep — free, +1 held Hit@3; opening words / LLM descriptions measured worse |
| stopwords removed from the IDF term boost | fixes q07 | — | — | keep — function words were "distinctive" in a 9-item vault |
| absent gate: ≥2 content terms, none anywhere in FTS vocabulary, cosine < 0.7 | 1/1 | 1/1 | 0 false absents | keep — the only working "not in your vault" signal; cosine thresholds and margins cannot do it |
| corpus-anchored synonym table (dense side only) | 9/9 | 7/9 | 9/9 | fragile: gain came from job-title words of the one résumé; default OFF, ON in ask mode as a latency buy |
| cross-encoder `mxbai-rerank-xsmall-v1` int8, k=10, 128 tokens, 6 threads, batch 1, **final order = reranker among scored docs** | **9/9** | **9/9** | 9/9 | **18/18 at 117 ms** — the only component that fixes the two questions every cheap config missed |
| MiniLM reranker (fp32/O3/int8) | 9/9 | 8/9 | — | worse than mxbai and int8 flips answers |
| bge-base / gte-small / arctic-s / e5-small / nomic / EmbeddingGemma | ≤9/9 | ≤7/9 | — | no reason to leave bge-small (nomic, gemma blow the latency budget) |
| chunk size 200 words, structure-aware | 7/9 | 6/9 | 8/9 | neutral for retrieval; +3/33 evidence recall in a 600-word ask context — hold until an end-to-end answer eval exists |
| LLM contextual enrichment (per doc / per chunk) | −1 | 5/9 | — | **measured worse twice**; dropped |
| pseudo-relevance feedback / lite HyDE | 6/9 | — | 7/9 | strictly worse, doubles query-embed cost, inflates absent scores |

Production configs (both validated on dev + held-out, 20 questions):
- **Live search** (≤15 ms): baseline score fusion + title doc units + IDF stopwords + absent gate → 14/18 Hit@1, **18/18 Hit@3, 2/2 absent**, **4.5–5.0 ms** (85 % of it the bge-small query embed). Zero bytes added.
- **Ask mode** (≤250 ms): the same + dense synonym expansion + mxbai int8 reranker (k=10) deciding the final document order → **18/18 Hit@1, 2/2 absent, 117 ms mean / 136 ms worst**. +92 MB download.

Cancelled: RRF fusion in `Store::search`, chunk.rs change, embedding-model change, LLM enrichment.
Next measurements (in order): a third ~30-question set written blind; per-pair reranker cost on a 4-core
laptop (set `rerank_k` from a measured budget); a ~400-item vault; the absent rule against short queries;
end-to-end answer quality with the 1.5B (the only thing that can settle chunking).

## 2c. Third, blind question set (2026-09-16) — 30 questions, written by an agent that saw only the vault

26 answerable (12 factual, 5 document, 5 temporal, 4 multi) + 4 absent; two-thirds pure paraphrase, half
typed casually with typos, three aimed at one-line pasted snippets, decoy-heavy numbers on purpose.

| | dev (18) | held-out (18) | **blind (26)** |
|---|---|---|---|
| Retrieval Hit@1, live search | 7/9 | 7/9 | 16/26 (62 %) |
| Retrieval Hit@3, live search | 9/9 | 9/9 | 24/26 (92 %) |
| Retrieval Hit@1, ask (rerank) | 9/9 | 9/9 | 19/26 (73 %) |
| Absent caught / false absents | 1/1, 0 | 1/1, 0 | 3/4 by the gate (+1 by the model), 0 |
| **Answers correct, Llama 3.2 3B (shipped)** | 7/9 | 6/9 | **17/26 (65 %)** |
| Answers correct, same model on the pure-GPU fp16 path (2026-09-16, now default) | 7/9 | 7/9 | 15/26 (58 %) — four flips vs fp32, all numerical noise (one gained, one was a lucky hit on the wrong document, two same-content rewordings) |
| TTFT / tok/s / peak WS | | | 1.18 s / 7.9 / 7.1 GB |

What the misses have in common (retrieval trace in `eval/results/blind-*.json`):
1. **One-line pasted snippets lose to the 10 KB customs PDF** (git remote, model id, night-shift title, test
   note): 4 of the 7 retrieval misses. The document unit is the snippet's own text, so it competes with 22
   customs chunks on cosine alone; the reranker helped only one of them.
2. **The customs PDF's decoy numbers**: vessel name (answered with the inland carrier code), duty vs portal
   charge, declared value — the model grabs a neighbouring field. Whole-document context makes this worse.
3. **Absent gate** misses questions whose nouns exist in the vault for other reasons ("expire", "date" — passport);
   the model then refused correctly, so the user-visible result was still right.
4. Two document-level questions ("proof I sent money", "throwaway sample note") retrieve the wrong document
   outright — title-only units carry no notion of *what kind* of document an item is.

Takeaway: the earlier 18/18 was partly the sets being kind; 62–73 % top-1 retrieval and 65 % correct answers
on realistic phrasing is the honest baseline to improve from. Candidates, in order: (a) score short items by
a length-aware prior or index snippets as their own "document kind" unit; (b) a per-item one-line description
for the document unit — measured worse when LLM-written on the dev set, but the blind set's document
questions are exactly what it targets, so re-test; (c) field-aware chunking for forms (the customs PDF) so a
chunk carries one labelled field, not a row of codes.

## 2d. Accuracy round on the GPU path (2026-09-16) — what moved 29/44 to 31/44 + 6/6, Hit@1 33 → 37/44

All 44 questions (dev + held-out + blind), Llama 3.2 3B fp16 on the static-shape DirectML path, one run per
configuration, each run ~4 min. Run-to-run noise is ±1–2 answers (the same config flips b17/b19/b21 between
runs), so only steps that moved several questions are called wins.

| Step | Answers | Hit@1 | Absent | Verdict |
|---|---|---|---|---|
| Baseline (fp16 GPU decode) | 29/44 | 33 | 6/6 | |
| Layout-aware PDF extraction (`src/pdf_layout.rs`) + every chunk that literally contains a rare query word joins the candidate set | 29/44 | 33 | 6/6 | different misses: q04/b19/b21 gained, q07/q08 lost — the résumé lost its paragraph breaks |
| + paragraph breaks kept in layout text, `logit`-free prompt hint for document questions, LLM query rewrites ON | 26/44 | 28 | 5/6 | **rewrites hurt**: the 3B prefixes "Here are three alternative search queries:" and writes generic terms ("Ship's log entry"); off by default (`BLACKHOLE_REWRITE=1` to test) |
| same, rewrites OFF | 27/44 | 34 | 6/6 | retrieval up; model now *refuses* on the right document (h01, b10, b26) |
| + no refusal clause, 2000-word budget, vault catalog for document questions | 28/44 | 20* | 6/6 | **2000 words backfired**: a second document enters the context and the model answers from it; peak WS 9.9 GB. (*Hit@1 mis-measured: whole-doc blocks were emitted after other items.) |
| budget back to 1400, overprint dedupe fixed ("Norton Lil y") | 30/44 | 33 | 6/6 | |
| **+ line breaks preserved inside chunks and merged blocks** (`chunk.rs`) | **32/44** | 33 | 5/6 | the big one: the model had been reading a résumé and a form as one run-on line. Refusal clause gone → one absent question answered |
| + literal-hit chunks always reach the reranker, decoy one-liners skipped in whole-doc mode, synonyms (employed/ssh/repo…), precise refusal clause back | 31/44 | 33 | 6/6 | ties E on correct+absent (37) |
| + reranker bonus for literal hits (substring), top document emitted first, "newest first" timeline | 28/44 | 35 | 6/6 | substring "repo" matched "repositories" and stole two questions; the timeline wording made ordering worse |
| **+ bonus only for whole-word hits on rare terms (≤2 items); timeline wording reverted — shipped** | **31/44** | **37/44 (84 %)** | **6/6** | |

What the shipped configuration still misses (13): three timeline questions where the 3B lists the résumé's
jobs oldest-first and reads the top line (q08/h07/b20 — the same three with the old extractor were right by
luck of ordering); three multi-hop customs questions (b23–b25: two documents or two fields of the form);
"which file has my career history" (h06 — the reranker prefers CONCEPT.md); the colloquial one-liners
(b09 git remote, b11 "model identifier with a reasoning setting" — no lexical overlap and a 3B rewrite makes
it worse); b12/b14 decoys; h09 wants the vehicle named; b17 the model refuses on the right note.

Retrieval is no longer the bottleneck (84 % top-1, 6/6 absent); the model's reading of forms and ordering
is. Three things measured *not* to help at this scale, so nobody re-tries them blind: LLM multi-query
rewriting with a 3B, a wider context budget, and prompt wording about timeline order. What should move the
remaining third: a stronger reader now that decode is 100 tok/s (Qwen3-4B fp16 on the same path, ~1 GB more),
and cell-aware form extraction (the PDF's stroked boxes are available from `pdf-extract`'s `stroke` callback,
so a label and its value can be paired by cell instead of by row).

## 2e. Reasoning (2026-09-16): Qwen3-4B with a 128-token thinking budget — 31 → 36/44

Same 44 questions and retrieval as §2d. A model that reasons before answering was the obvious lever for the
ordering and multi-field misses, and the GPU path made it affordable (50 tok/s → a 128-token think costs
~3 s). Qwen3-4B (hybrid think mode, budgetable, DirectML/NPU family) against Llama 3.2 3B:

| | Answers | Absent | To first text |
|---|---|---|---|
| Llama 3.2 3B, direct | 31/44 | 6/6 | 0.9 s |
| Qwen3-4B, direct | 29/44 (refuses more) | 6/6 | 2.1 s |
| **Qwen3-4B, think ≤128 — shipped** | **36/44 (82 %)** | 6/6 | 5.0 s |
| Qwen3-4B, think ≤256 | 35/44 (+1 right answer the regex rejected) | 6/6 | 5.8 s |
| Qwen3-4B, think ≤512 | 35/44 | 6/6 | 7.4 s |

What flipped: the résumé ordering questions (q08, h07, b20-class), the customs multi-field questions (q04,
b25), several decoys. Still missing: b23/b24 (two documents *and* arithmetic), h09 (must name the vehicle),
b09/b11/b12 (retrieval of one-liners with no lexical overlap), b22 (refusal). The budget curve is flat past
128: the model needs a short pass to order dates or pick a field, not a long one. A DirectML quirk found on
the way (prompt passes 8–14 s when the prompt is 45–64 % of the cache capacity) and its fix are in
PACKAGING.md.

## 2f. Cell-aware forms and the VRAM question (2026-09-16)

**Forms as cells.** `pdf_layout.rs` now also collects the page's ruled lines (from `pdf-extract`'s
`stroke`/`fill` callbacks: axis-aligned segments and thin filled rectangles). When a page has enough of
them, every glyph is assigned to the cell that boxes it in (nearest rules left/above/right/below), and a
cell becomes one line: `LABEL: value` when a small-font or all-caps first row is followed by the value,
the rows kept as lines when the cell is a block of text (status logs, declarations), plain rows for text
outside any cell. The customs form now yields `15. VESSEL CODE/NAME: TRANQUIL ACE`, `10. Country of
Origin: JP`, `44. Total: 3211.71` instead of labels and values on separate rows.

| | Answers | Hit@1 | Absent |
|---|---|---|---|
| Qwen3-4B think ≤128, row layout (§2e) | 36/44 | 37 | 6/6 |
| same, cell-aware forms (first cut: big cells flattened to one line) | 34/44 | 37 | 6/6 — lost the cargo-release dates |
| **same, cell-aware with multi-row cells kept as lines — shipped** | 35/44 | 37 | 6/6 — the one flip (b22) is a refusal on a CONCEPT.md question whose answer is in the context |
| same + rare digit-token pin in the ask context (§2g, 2026-09-17) | 35/44 | 37 | 6/6 — identical failing set; the pin never fires on this vault's questions, it only bites among near-identical siblings |

So: no measurable accuracy change within the ±1–2 noise band, but the text is unambiguously better for
the model and for a person reading a preview, and the two multi-hop customs questions (b23, b24) now
fail on arithmetic/reasoning with the right numbers in front of the model rather than on extraction.

**Sharing weights between the two DirectML sessions.** Investigated and not possible with ONNX Runtime
1.20: initializer sharing (`AddInitializer`, prepacked-weight containers) works for CPU memory, but the
DirectML provider uploads each session's initializers to its own GPU resources, and the single-session
alternative (padded decode steps) corrupts GroupQueryAttention (PACKAGING.md). What shipped instead: a
**low-VRAM mode** — adapters with under 7 GB of dedicated VRAM keep the DirectML prompt pass but decode
on the CPU (one weight copy in VRAM, one in RAM; `BLACKHOLE_DECODE=gpu` forces the two-session path).



## 2g. Scale, reranker cost and short-query refusals (2026-09-16)

Three questions §4.2c left open, answered on a **synthetic** vault so the numbers can be published:
`eval/gen_vault.py` writes 400 items (170 k words, 3 264 chunks; one-liners, page-length notes and
900–1 800-word documents across note/text/md/code/pdf) plus 30 questions. Every fact hangs off an
invented name that occurs in exactly one item ("Project Vexlorn-217 is led by Ilsa Brantwood out of the
Kobe office. Its access code is 8831"), so a question has exactly one right document and a wrong
retrieval cannot pass by luck. `askeval --scale <dir> <items.json> <questions.json>` builds the vault
with the real chunker and embedder and then runs the panel's own live-search path.

**Live search at 400 items** (RX-class DirectML embeddings, 24-core CPU, warm):

| | p50 | p95 |
|---|---|---|
| embed the query (bge-small, DirectML) | 29.0 ms | 32.5 ms |
| hybrid search over 3 264 chunks (FTS5 + brute-force cosine + fusion) | **6.3 ms** | 7.7 ms |
| keystroke to ranked list | **35.0 ms** | 40.1 ms |

**Hit@1 30/30, Hit@3 30/30** (the top hit's title is the one document that holds the answer). Ingest of
the whole vault — extract, chunk, embed 3 264 chunks — took 38 s.

The same vault answered live through MCP on an isolated instance (`retrieve`, limit 8, 30 queries):
**Hit@1 30/30**, round trip p50 76 ms / p95 85 ms — of which ~68 ms is `curl.exe` start-up (the same
measurement against `tools/list` costs 68 ms), so the server side is ≈8 ms, consistent with the
in-process numbers above. Brute-force cosine over every chunk is not the bottleneck at this size: at
6.3 ms for 3 264 chunks, an ANN index would buy nothing before ~50 k chunks (≈6 000 items).

**Reranker cost.** `askeval --rerank-bench` scores 10 real-shaped pairs five times: **8.7 ms per pair**
at 128 tokens, batch 1, 6 intra-op threads on a 24-core Zen 4 (the reranker is CPU-only by
construction); the session loads in 0.67 s. So k is now derived from a budget instead of being a magic
number (`rerank::candidates()`):

    per_pair = 8.7 ms × 6 / intra-op threads        threads = clamp(cores − 2, 2, 6)
    k        = clamp(400 ms / per_pair, 6, 10)

The ceiling is the accuracy result, not the budget: k=10 is where accuracy saturated and k=5 lost two
held-out questions, so the budget may only trade k **down** on a machine slower than two intra-op
threads — it never buys more pairs than the measured-good number. On this machine k=10 costs 87 ms; a
two-thread laptop pays 261 ms, still inside the 400 ms budget. `BLACKHOLE_RERANK_K` overrides it.

**The absent gate on one- and two-word queries.** The rule ("≥2 content terms, none of them anywhere in
the vault, and no strong semantic match") had a plain bug: FTS5's `unicode61` tokenizer does not stem, so
*invoices totals* matched nothing although the vault says *invoice … total*, and the question was refused
with "that isn't in your vault". `Store::term_present` now also tries the query word's singulars
("queries" → "query", "boxes" → "box") and a prefix match (a stored "resumes" answers "resume"). One-word
queries were already never refused (they cannot reach two content terms).

Measured on the 400-item vault (`askeval --absent`, real embeddings) — 10 cases, refusals marked:

| query | words | best cosine | before | after |
|---|---|---|---|---|
| Vexlorn-117 / quarterly | 1 | 0.88 / 0.67 | no | no |
| vexlorn budget | 2 | 0.80 | no | no |
| budgets codes | 2 | 0.74 | no | no |
| **reviews meetings** | 2 | 0.62 | **refused** | no |
| **projects offices** | 2 | 0.63 | **refused** | no |
| brantwood trondheim | 2 | 0.66 | no | no |
| quantum teleportation | 2 | 0.54 | refused | refused |
| gravitational lensing | 2 | 0.59 | refused | refused |
| wombat husbandry | 2 | 0.58 | refused | refused |

Two false refusals fixed, all three true refusals kept. A second idea — a softer cosine threshold for
short queries, on the theory that two words carry little lexical evidence — was **measured and dropped**:
two-word questions whose words are absent but whose meaning is present score 0.63–0.66
("funding allowance" 0.651, "passcode credentials" 0.629, "supervisor headcount" 0.656) while nonsense
scores 0.54–0.64 ("photosynthesis chlorophyll" 0.644 beats "funding allowance"). The bands overlap
outright, so any threshold in that region would only stop the gate from ever firing. The single 0.70
cut stays for every query length; §2b's finding that cosine cannot separate unanswerable questions
holds at 400 items too.

**One thing this exposed.** Live search ranks the right document first for all 30 questions, but the
*ask* path's chunk-level context (`context_reranked`) picked a sibling document for one question out of
eight in the in-app self-check ("access code for Corvane-225" → excerpts from *Corvane-375*). With 400
near-identical documents the distinguishing token is a number inside one line, which neither the cosine
nor the cross-encoder weighs heavily. **Fixed 2026-09-17:** a digit-bearing token from the raw query
("225", an invoice number, a year) that occurs in at most two items now pins the candidates to those
items (all such tokens must match; failing that any; failing that the list is left alone) — tokenised
from the query itself, since "225" is shorter than the content-term floor. Self-check on the 400-item
vault: 29/30 → **30/30** (retrieval only); the pin fired on every question. The real-vault answer eval
(44 questions) was re-run as the regression check — see the table in §2f.

**In-app self-check.** Right-click → **Self-check** runs the whole thing on demand: `<data>\questions.json`
(`[{"q": …, "expect": ["substring", …]}]`) against the user's own vault, or — with no such file — a
built-in five-question set against a scratch vault built on the spot (five invented documents, so the
answers are known). It reports a score in a bubble and writes `selfcheck.txt` beside the vault. Without
an LLM installed it still runs, scoring the retrieved excerpts instead of an answer.


Everything below runs on what we already ship (ONNX Runtime + DirectML, bge-small, Qwen2.5-1.5B) plus one
22 M-parameter cross-encoder. Total added download ≈ 23 MB (int8) or 90 MB (fp32).

### 3.1 Index: two granularities
- **Chunks** as today (100 words, 20 overlap; the research says 256–512 *tokens* — our 100 words ≈ 140
  tokens is on the small side; try 200 words with the evaluation set, §3.5).
- **Document units**: one vector per item = `title` (and, when the LLM is loaded, a one-sentence
  LLM description of the document). Stored as chunk `ord = -1`. Cheap: one embedding per item.

### 3.2 Retrieval: hybrid → candidates → rerank → document fusion
1. Dense top-40 over chunks + document units; BM25 hits via FTS5 (existing).
2. **Rerank** the top-20 passages with `cross-encoder/ms-marco-MiniLM-L-6-v2` (22 M params; 7 ms/pair
   CPU, less on DirectML; document units are reranked as `title: first 60 words`).
3. **Document score** = RRF over (rank of the doc's best unit by embedding, rank of its best unit by
   reranker) + IDF-weighted term-hit boost (existing). Pick the top document(s).
4. **Context assembly** (existing): whole document when it fits the word budget, else reranked chunks
   with neighbours in document order.
5. Search panel: same fusion for the result list when the query has ≥ 3 words; single-word queries stay
   keyword-first (rerank is wasted on "invoice").

### 3.3 Contextual enrichment — only where the LLM already runs
Anthropic's technique needs a capable LLM at ingest; our 1.5B produced generic, sometimes leaky contexts
("order #4471" copied from the few-shot example) and gained nothing on this vault. Scaled-down version:
- At ingest, when the LLM is loaded (panel recently open) or on an idle timer, generate **one description per
  document**, not per chunk — cheap, and it feeds the document unit in §3.1.
- Per-chunk contexts stay off until the evaluation set (§3.5) shows a gain.

### 3.4 Query understanding — minimal
- Keep the typo-tolerant OR keyword query and IDF boosts.
- Add a tiny **synonym expansion table** for query words that documents rarely use literally
  (employer → company/worked/position; salary → pay/compensation; invoice → receipt/total …). Zero
  latency, no LLM, and it targets exactly the failure class we saw.
- No HyDE (latency + hallucination on personal facts); no multi-query until eval shows the need.

### 3.5 Evaluation set — the missing piece
- On demand ("Self-check" in the menu, and in CI), generate 2 questions per document with the LLM
  (one factual, one about the document as a whole), run retrieval, report Hit@1 / Hit@5 for the
  document and the chunk, and log per-question traces (as `log.txt` already does).
- Every knob above (chunk size, RRF k, boost weights, reranker on/off) gets tuned against this, not by feel.

### 3.6 Answering (unchanged, but noted)
- Whole-document context + timeline scratchpad already fixed most "before/after" cases. The remaining
  errors are the 1.5B's reasoning; the fix is a bigger model on the GPU (PACKAGING.md phase 2b), not
  retrieval.

## 4. Order of work (revised after §2b)
1. `store.rs`: stopword filter in the IDF boost + absent gate (≥2 content terms, none in the FTS vocabulary, cosine < 0.7). No model, no index change.
2. `store.rs` + ingest: title-only document units (`chunks.ord = -1`), never handed to the LLM as evidence.
3. `rerank.rs` (new): mxbai-rerank-xsmall-v1 int8 on ORT, ask path only; k=10, 128 tokens, batch 1, threads pinned; final document order = reranker among scored docs, fused score as tiebreak.
4. `expand.rs` (new): dense-side synonym table, vocabulary-anchored, ask mode only.
5. Evaluation: third blind question set; per-pair reranker cost on low-end hardware; end-to-end answer eval before touching chunking.

## 5. Rejected for this scale
- Bigger embedding model (bge-base): measured worse here, 3× the bytes.
- HyDE: latency and hallucination on fact-bound personal data.
- ColBERT / multi-vector: index size and long-query degradation.
- GraphRAG / RAPTOR trees: needs many LLM calls per document; revisit if vaults reach thousands of long
  documents.
- LLM-as-reranker with the 1.5B: cross-encoder is equal or better in-distribution at ~1/100 the cost.

## Sources
- [Anthropic — Contextual Retrieval](https://platform.claude.com/cookbook/capabilities-contextual-embeddings-guide)
- [Rethinking Hybrid Retrieval: small embeddings + re-ranking beat bigger models](https://arxiv.org/pdf/2506.00049)
- [How Good are LLM-based Rerankers? (22 methods)](https://arxiv.org/pdf/2508.16757)
- [7 Advanced RAG Techniques That Deliver Real Accuracy Gains](https://nandigamharikrishna.substack.com/p/7-advanced-rag-techniques-that-deliver)
- [RAG Techniques Compared 2026](https://blog.starmorph.com/blog/rag-techniques-compared-best-practices-guide)
- [RAG Chunking Strategies: 2026 Benchmark Guide](https://www.premai.io/blog/rag-chunking-strategies-the-2026-benchmark-guide/)
- [A Systematic Investigation of Document Chunking Strategies](https://arxiv.org/html/2603.06976)
- [RAPTOR](https://github.com/parthsarthi03/raptor) · [LIR workshop @ ECIR 2026](https://arxiv.org/abs/2511.00444)
- [Query Rewriting for RAG](https://arxiv.org/abs/2305.14283) · [HyDE](https://www.emergentmind.com/topics/hypothetical-document-embeddings-hyde)
- [Best Rerankers for RAG 2026](https://futureagi.com/blog/best-rerankers-for-rag-2026/) · [Sentence-Transformers cross-encoder efficiency](https://sbert.net/docs/cross_encoder/usage/efficiency.html)
- [RAG evaluation guide](https://www.evidentlyai.com/llm-guide/rag-evaluation) · [Can we Evaluate RAGs with Synthetic Data?](https://arxiv.org/html/2508.11758v2)
