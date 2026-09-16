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

## 3. Scaled-down design for Blackhole

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

## 4. Order of work
1. Document units (title vector) + document-level RRF — **measured 13/14**, half a day.
2. Cross-encoder reranker on ORT (embed model bytes; DirectML → CPU) — measured +1, the standard
   production step.
3. Synonym expansion table + evaluation harness ("Self-check").
4. Chunk-size sweep (100 → 200 words) against the harness.
5. LLM document descriptions at idle; per-chunk contexts only if eval says so.

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
