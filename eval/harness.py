#!/usr/bin/env python3
"""Blackhole retrieval measurement harness.

A single-file, dependency-light (stdlib + numpy + onnxruntime + tokenizers) rig for
measuring the retrieval pipeline described in RAG.md against the real vault snapshot.

Everything is a config knob, and the *defaults reproduce the shipped app*
(bge-small, "<title>\\n<chunk>" units, chunks of 100 words / 20 overlap, score
fusion with KW_WEIGHT=0.6, no document units, no reranker), so `--config '{}'`
is the baseline to beat.

    python eval/harness.py --config '{"rerank": true, "fusion": "rrf_docs"}' \
        --questions eval/questions.json --out eval/results/rerank-rrf.json --name rerank-rrf

Deterministic: no sampling anywhere, ties broken by (score, unit key).
"""

from __future__ import annotations

import argparse
import hashlib
import json
import math
import os
import re
import resource
import sqlite3
import sys
import time
from collections import defaultdict
from typing import Any, Dict, Iterable, List, Optional, Sequence, Tuple

import numpy as np

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))

# --------------------------------------------------------------------------------------
# Config
# --------------------------------------------------------------------------------------

# Every knob, with the value that reproduces the shipped app's behaviour.
DEFAULTS: Dict[str, Any] = {
    # --- corpus ----------------------------------------------------------------------
    "db": "eval/vault-snapshot.db",
    "rechunk": True,            # re-chunk items.content (mirrors src/chunk.rs); False = use stored chunks
    "chunk_words": 100,         # chunk.rs TARGET_WORDS
    "chunk_overlap": 20,        # chunk.rs OVERLAP_WORDS
    "max_chunks": 600,          # chunk.rs MAX_CHUNKS
    # --- units -----------------------------------------------------------------------
    "unit_text": "title_chunk",  # "title_chunk" (app) | "chunk"
    "doc_units": False,          # add one document-level unit per item (RAG.md §3.1, ord = -1)
    "doc_unit_text": "title",    # "title" | "title_opening" | "description"
    "doc_opening_words": 60,     # words of content for doc_unit_text="title_opening"
    "doc_descriptions": None,    # JSON file {"<item_id>": "one-sentence description"} for "description"
    "chunk_contexts": None,      # JSON file {"<item_id>:<ord>": "context"} prepended to chunk text
    # --- embedding -------------------------------------------------------------------
    "embed_model": "models/bge-small-en-v1.5",
    "pooling": "cls",            # "cls" (bge) | "mean" (e5/gte/nomic-style)
    "max_len": 512,
    "batch_size": 16,
    "query_instruction": "",     # e.g. BGE's "Represent this sentence for searching relevant passages: "
    "query_prefix": "",          # nomic-style "search_query: "
    "passage_prefix": "",        # nomic-style "search_document: "
    "cache_dir": "eval/.cache",
    # --- dense search ----------------------------------------------------------------
    "dense_top": 40,             # units kept from the dense pass
    "min_cosine": 0.45,          # store.rs MIN_COSINE (score fusion only)
    "cosine_window": 0.12,       # store.rs COSINE_WINDOW (score fusion only)
    "sem_floor": 0.40,           # store.rs SEM_FLOOR
    # --- keyword / BM25 --------------------------------------------------------------
    "bm25": True,
    "bm25_k1": 1.5,
    "bm25_b": 0.75,
    "bm25_title_weight": 3.0,    # store.rs bm25(items_fts, 3.0, 1.0)
    "typo_or": True,             # OR semantics + prefix match on the last token (store.rs to_fts_query)
    "idf_boost": True,           # store.rs context(): distinctive-term boost on dense scores
    "idf_boost_weight": 0.08,
    "idf_min_len": 4,
    "idf_max_frac": 0.34,
    "kw_weight": 0.6,            # store.rs KW_WEIGHT
    # --- reranker --------------------------------------------------------------------
    "rerank": False,
    "rerank_model": "models/rerank",
    "rerank_k": 20,              # candidates fed to the cross-encoder
    "rerank_max_len": 320,
    "rerank_doc_words": 60,      # document units are reranked as "title: first N words" (RAG.md §3.2)
    # --- fusion / reporting ----------------------------------------------------------
    "fusion": "score",           # "score" (app) | "rrf_units" | "rrf_docs"
    "rrf_k": 60,
    "topk": 5,                   # item ids recorded per question
    "absent_metric": "cosine",   # "cosine" (scale-stable) | "fused"
    "absent_threshold": 0.45,    # "absent" questions are correct when the top score is below this
}


def load_config(raw: Optional[str]) -> Dict[str, Any]:
    """Merge a JSON blob (or @file) of overrides onto DEFAULTS, rejecting unknown keys."""
    cfg = dict(DEFAULTS)
    if raw:
        if raw.startswith("@"):
            with open(abspath(raw[1:]), encoding="utf-8") as fh:
                over = json.load(fh)
        else:
            over = json.loads(raw)
        unknown = set(over) - set(DEFAULTS)
        if unknown:
            raise SystemExit(f"unknown config keys: {sorted(unknown)}")
        cfg.update(over)
    return cfg


def abspath(p: str) -> str:
    return p if os.path.isabs(p) else os.path.join(ROOT, p)


# --------------------------------------------------------------------------------------
# Chunking — mirrors src/chunk.rs exactly
# --------------------------------------------------------------------------------------

def chunk(text: str, target: int, overlap: int, max_chunks: int) -> List[str]:
    """Paragraph-aware packing to `target` words with `overlap` words carried forward."""
    out: List[str] = []
    cur: List[str] = []

    def flush() -> None:
        if cur:
            out.append(" ".join(cur))
            keep = max(len(cur) - overlap, 0)
            del cur[:keep]          # drop everything but the trailing `overlap` words

    for para in text.split("\n\n"):
        words = para.split()
        if not words:
            continue
        # A paragraph that doesn't fit into what's left starts a new chunk.
        if cur and len(cur) + len(words) > target:
            flush()
        for w in words:
            cur.append(w)
            if len(cur) >= target:
                flush()
                if len(out) >= max_chunks:
                    return out
    # Tail: only keep it if it adds words beyond the overlap already emitted.
    if (len(cur) > overlap or not out) and cur:
        out.append(" ".join(cur))
    return out


# --------------------------------------------------------------------------------------
# Corpus / units
# --------------------------------------------------------------------------------------

class Unit:
    """One retrieval unit: a chunk (ord >= 0) or a document unit (ord == -1)."""

    __slots__ = ("item_id", "ord", "title", "text", "embed_text", "rerank_text")

    def __init__(self, item_id: int, ord_: int, title: str, text: str,
                 embed_text: str, rerank_text: str) -> None:
        self.item_id, self.ord, self.title = item_id, ord_, title
        self.text, self.embed_text, self.rerank_text = text, embed_text, rerank_text

    @property
    def key(self) -> str:
        return f"{self.item_id}:{self.ord}"


def build_units(cfg: Dict[str, Any]) -> Tuple[List[Unit], Dict[int, str]]:
    """Read the snapshot and materialise the retrieval units under this config."""
    conn = sqlite3.connect(abspath(cfg["db"]))
    items = conn.execute("SELECT id, title, content FROM items ORDER BY id").fetchall()
    titles = {i: t for i, t, _ in items}

    contexts: Dict[str, str] = {}
    if cfg["chunk_contexts"]:
        with open(abspath(cfg["chunk_contexts"]), encoding="utf-8") as fh:
            contexts = json.load(fh)
    descriptions: Dict[str, str] = {}
    if cfg["doc_descriptions"]:
        with open(abspath(cfg["doc_descriptions"]), encoding="utf-8") as fh:
            descriptions = json.load(fh)

    stored: Dict[int, List[str]] = defaultdict(list)
    if not cfg["rechunk"]:
        for iid, _o, txt in conn.execute("SELECT item_id, ord, text FROM chunks ORDER BY item_id, ord"):
            stored[iid].append(txt)
    conn.close()

    units: List[Unit] = []
    for iid, title, content in items:
        texts = (chunk(content, cfg["chunk_words"], cfg["chunk_overlap"], cfg["max_chunks"])
                 if cfg["rechunk"] else stored.get(iid, []))
        for ord_, txt in enumerate(texts):
            ctx = contexts.get(f"{iid}:{ord_}", "")
            body = f"{ctx}\n{txt}" if ctx else txt
            embed_text = f"{title}\n{body}" if cfg["unit_text"] == "title_chunk" else body
            units.append(Unit(iid, ord_, title, txt, embed_text, txt))
        if cfg["doc_units"]:
            mode = cfg["doc_unit_text"]
            if mode == "title":
                doc_text = title
            elif mode == "title_opening":
                doc_text = title + "\n" + " ".join(content.split()[: cfg["doc_opening_words"]])
            elif mode == "description":
                doc_text = title + "\n" + descriptions.get(str(iid), "")
            else:
                raise SystemExit(f"bad doc_unit_text: {mode}")
            # Reranked as "title: first N words" (RAG.md §3.2).
            rr = f"{title}: " + " ".join(content.split()[: cfg["rerank_doc_words"]])
            units.append(Unit(iid, -1, title, doc_text, doc_text, rr))
    return units, titles


# --------------------------------------------------------------------------------------
# Embedding (ONNX Runtime, CPU) with an on-disk cache
# --------------------------------------------------------------------------------------

TOKEN_RE = re.compile(r"[a-z0-9]+")


class Embedder:
    """bge-style ONNX encoder: CLS or mean pooling, L2 normalised, batched, disk-cached."""

    def __init__(self, cfg: Dict[str, Any]) -> None:
        import onnxruntime as ort
        from tokenizers import Tokenizer

        self.cfg = cfg
        model_dir = abspath(cfg["embed_model"])
        self.model_key = f"{os.path.basename(model_dir.rstrip('/'))}|{cfg['pooling']}|{cfg['max_len']}"
        t0 = time.perf_counter()
        self.tok = Tokenizer.from_file(os.path.join(model_dir, "tokenizer.json"))
        self.tok.enable_truncation(max_length=cfg["max_len"])
        self.tok.enable_padding(pad_id=0, pad_token="[PAD]")
        so = ort.SessionOptions()
        so.log_severity_level = 3
        self.sess = ort.InferenceSession(os.path.join(model_dir, "model.onnx"),
                                         sess_options=so, providers=["CPUExecutionProvider"])
        self.load_ms = (time.perf_counter() - t0) * 1000
        self.inputs = {i.name for i in self.sess.get_inputs()}
        # Disk cache: {sha1(model_key + text): vector}. Loaded once, written once at the end.
        cache_root = abspath(cfg["cache_dir"])
        os.makedirs(cache_root, exist_ok=True)
        safe = re.sub(r"[^A-Za-z0-9_.-]", "_", self.model_key)
        self.cache_path = os.path.join(cache_root, f"emb-{safe}.npz")
        self.cache: Dict[str, np.ndarray] = {}
        if os.path.exists(self.cache_path):
            with np.load(self.cache_path) as z:
                self.cache = {k: z[k] for k in z.files}
        self.dirty = False

    def _hash(self, text: str) -> str:
        return hashlib.sha1((self.model_key + "\x00" + text).encode("utf-8")).hexdigest()

    def _forward(self, texts: Sequence[str]) -> np.ndarray:
        enc = self.tok.encode_batch(list(texts))
        ids = np.array([e.ids for e in enc], dtype=np.int64)
        mask = np.array([e.attention_mask for e in enc], dtype=np.int64)
        feed = {"input_ids": ids, "attention_mask": mask}
        if "token_type_ids" in self.inputs:
            feed["token_type_ids"] = np.zeros_like(ids)
        hidden = self.sess.run(None, feed)[0]
        if self.cfg["pooling"] == "cls":
            vec = hidden[:, 0, :]
        elif self.cfg["pooling"] == "mean":
            m = mask[:, :, None].astype(np.float32)
            vec = (hidden * m).sum(1) / np.clip(m.sum(1), 1e-9, None)
        else:
            raise SystemExit(f"bad pooling: {self.cfg['pooling']}")
        vec = vec.astype(np.float32)
        return vec / np.clip(np.linalg.norm(vec, axis=1, keepdims=True), 1e-9, None)

    def embed(self, texts: Sequence[str]) -> np.ndarray:
        """Embed with cache lookup; only misses hit the model."""
        keys = [self._hash(t) for t in texts]
        todo = [i for i, k in enumerate(keys) if k not in self.cache]
        for s in range(0, len(todo), self.cfg["batch_size"]):
            idx = todo[s: s + self.cfg["batch_size"]]
            vecs = self._forward([texts[i] for i in idx])
            for i, v in zip(idx, vecs):
                self.cache[keys[i]] = v
            self.dirty = True
        return np.stack([self.cache[k] for k in keys]) if texts else np.zeros((0, 1), np.float32)

    def embed_query(self, q: str) -> np.ndarray:
        return self.embed([self.cfg["query_prefix"] + self.cfg["query_instruction"] + q])[0]

    def embed_passages(self, texts: Sequence[str]) -> np.ndarray:
        return self.embed([self.cfg["passage_prefix"] + t for t in texts])

    def flush(self) -> None:
        if self.dirty:
            np.savez(self.cache_path, **self.cache)
            self.dirty = False


# --------------------------------------------------------------------------------------
# BM25 over units + store.rs keyword semantics
# --------------------------------------------------------------------------------------

def tokenize(text: str) -> List[str]:
    return TOKEN_RE.findall(text.lower())


class BM25:
    """Okapi BM25 over the retrieval units, with store.rs's typo-tolerant OR semantics.

    store.rs builds an FTS5 expression of quoted OR-ed tokens with the last one as a
    prefix (so one typo doesn't empty the list and results update while typing), and
    ranks with bm25(items_fts, 3.0, 1.0) — the title field weighted 3x. Here each unit
    carries its title, so title terms are counted `bm25_title_weight` times.
    """

    def __init__(self, units: Sequence[Unit], cfg: Dict[str, Any]) -> None:
        self.cfg = cfg
        self.units = units
        self.tf: List[Dict[str, float]] = []
        self.len: List[float] = []
        df: Dict[str, int] = defaultdict(int)
        for u in units:
            counts: Dict[str, float] = defaultdict(float)
            for t in tokenize(u.title):
                counts[t] += cfg["bm25_title_weight"]
            for t in tokenize(u.text):
                counts[t] += 1.0
            self.tf.append(counts)
            self.len.append(sum(counts.values()))
            for t in counts:
                df[t] += 1
        self.df = df
        self.n = max(len(units), 1)
        self.avglen = (sum(self.len) / self.n) if self.len else 1.0
        # Per-item document frequency, for the IDF boost and term coverage below.
        self.item_terms: Dict[int, set] = defaultdict(set)
        for u in units:
            self.item_terms[u.item_id] |= set(tokenize(u.title)) | set(tokenize(u.text))
        self.item_df: Dict[str, int] = defaultdict(int)
        for terms in self.item_terms.values():
            for t in terms:
                self.item_df[t] += 1
        self.n_items = max(len(self.item_terms), 1)

    def _matched(self, counts: Dict[str, float], term: str, prefix: bool) -> float:
        if term in counts:
            return counts[term]
        if prefix:  # last query token matches as a prefix (FTS5 `"tok"*`)
            return sum(v for k, v in counts.items() if k.startswith(term))
        return 0.0

    def search(self, query: str) -> List[Tuple[int, float]]:
        """(unit index, score) sorted best-first. OR semantics when typo_or, else AND."""
        terms = tokenize(query)
        if not terms:
            return []
        k1, b = self.cfg["bm25_k1"], self.cfg["bm25_b"]
        out: List[Tuple[int, float]] = []
        for i, counts in enumerate(self.tf):
            score, hits = 0.0, 0
            for j, t in enumerate(terms):
                prefix = self.cfg["typo_or"] and j + 1 == len(terms)
                f = self._matched(counts, t, prefix)
                if f <= 0:
                    continue
                hits += 1
                idf = math.log(1 + (self.n - self.df.get(t, 0) + 0.5) / (self.df.get(t, 0) + 0.5))
                dl = self.len[i] / self.avglen if self.avglen else 1.0
                score += idf * (f * (k1 + 1)) / (f + k1 * (1 - b + b * dl))
            if hits == 0:
                continue
            if not self.cfg["typo_or"] and hits < len(terms):
                continue  # AND semantics: every term must appear
            out.append((i, score))
        out.sort(key=lambda p: (-p[1], p[0]))
        return out

    def coverage(self, query: str) -> Dict[int, float]:
        """store.rs term_coverage: fraction of query terms each *item* contains."""
        terms = tokenize(query)
        cov: Dict[int, float] = defaultdict(float)
        if not terms:
            return cov
        for item, vocab in self.item_terms.items():
            hit = 0
            for j, t in enumerate(terms):
                prefix = self.cfg["typo_or"] and j + 1 == len(terms)
                if t in vocab or (prefix and any(v.startswith(t) for v in vocab)):
                    hit += 1
            cov[item] = hit / len(terms)
        return cov

    def boost_terms(self, query: str) -> List[Tuple[str, float]]:
        """store.rs context(): distinctive query terms weighted by rarity across items."""
        if not self.cfg["idf_boost"]:
            return []
        terms: List[Tuple[str, float]] = []
        for t in {t for t in tokenize(query) if len(t) >= self.cfg["idf_min_len"]}:
            frac = self.item_df.get(t, 0) / self.n_items
            if frac <= self.cfg["idf_max_frac"]:
                terms.append((t, self.cfg["idf_boost_weight"] * (1.0 - frac)))
        return sorted(terms)


# --------------------------------------------------------------------------------------
# Cross-encoder reranker (optional)
# --------------------------------------------------------------------------------------

class Reranker:
    """ms-marco-MiniLM-L-6-v2 style cross-encoder: (query, passage) -> relevance logit."""

    def __init__(self, cfg: Dict[str, Any]) -> None:
        import onnxruntime as ort
        from tokenizers import Tokenizer

        self.cfg = cfg
        d = abspath(cfg["rerank_model"])
        t0 = time.perf_counter()
        self.tok = Tokenizer.from_file(os.path.join(d, "tokenizer.json"))
        self.tok.enable_truncation(max_length=cfg["rerank_max_len"])
        self.tok.enable_padding(pad_id=0, pad_token="[PAD]")
        so = ort.SessionOptions()
        so.log_severity_level = 3
        self.sess = ort.InferenceSession(os.path.join(d, "model.onnx"),
                                         sess_options=so, providers=["CPUExecutionProvider"])
        self.load_ms = (time.perf_counter() - t0) * 1000
        self.inputs = {i.name for i in self.sess.get_inputs()}

    def score(self, query: str, passages: Sequence[str]) -> List[float]:
        if not passages:
            return []
        out: List[float] = []
        bs = self.cfg["batch_size"]
        for s in range(0, len(passages), bs):
            batch = [(query, p) for p in passages[s: s + bs]]
            enc = self.tok.encode_batch(batch)
            ids = np.array([e.ids for e in enc], dtype=np.int64)
            mask = np.array([e.attention_mask for e in enc], dtype=np.int64)
            feed = {"input_ids": ids, "attention_mask": mask}
            if "token_type_ids" in self.inputs:
                feed["token_type_ids"] = np.array([e.type_ids for e in enc], dtype=np.int64)
            out.extend(float(x) for x in self.sess.run(None, feed)[0].reshape(-1))
        return out


# --------------------------------------------------------------------------------------
# Fusion
# --------------------------------------------------------------------------------------

def rrf(ranked_lists: Iterable[Sequence[Any]], k: int) -> Dict[Any, float]:
    """Reciprocal-rank fusion: sum 1/(k + rank) over every list an id appears in."""
    scores: Dict[Any, float] = defaultdict(float)
    for lst in ranked_lists:
        for rank, key in enumerate(lst):
            scores[key] += 1.0 / (k + rank + 1)
    return scores


def best_per_doc(order: Sequence[int], units: Sequence[Unit]) -> List[int]:
    """Collapse a unit ranking to a document ranking, each doc at its best unit."""
    seen: set = set()
    docs: List[int] = []
    for i in order:
        d = units[i].item_id
        if d not in seen:
            seen.add(d)
            docs.append(d)
    return docs


# --------------------------------------------------------------------------------------
# The pipeline
# --------------------------------------------------------------------------------------

class Harness:
    def __init__(self, cfg: Dict[str, Any]) -> None:
        self.cfg = cfg
        self.load_ms: Dict[str, float] = {}
        t0 = time.perf_counter()
        self.units, self.titles = build_units(cfg)
        self.load_ms["corpus"] = (time.perf_counter() - t0) * 1000
        self.embedder = Embedder(cfg)
        self.load_ms["embed_model"] = self.embedder.load_ms
        t0 = time.perf_counter()
        self.mat = self.embedder.embed_passages([u.embed_text for u in self.units])
        self.load_ms["index_embed"] = (time.perf_counter() - t0) * 1000
        t0 = time.perf_counter()
        self.bm25 = BM25(self.units, cfg)
        self.load_ms["bm25_index"] = (time.perf_counter() - t0) * 1000
        self.reranker = None
        if cfg["rerank"]:
            self.reranker = Reranker(cfg)
            self.load_ms["rerank_model"] = self.reranker.load_ms
        self.embedder.flush()

    # -- one question ------------------------------------------------------------------
    def run(self, question: str) -> Dict[str, Any]:
        cfg, units = self.cfg, self.units
        lat: Dict[str, float] = {}

        t0 = time.perf_counter()
        qvec = self.embedder.embed_query(question)
        lat["query_embed"] = (time.perf_counter() - t0) * 1000

        # --- dense ---------------------------------------------------------------------
        t0 = time.perf_counter()
        cos = self.mat @ qvec if len(units) else np.zeros(0, np.float32)
        boosts = self.bm25.boost_terms(question)
        dense = cos.copy()
        if boosts:
            # store.rs context(): literal hits on rare query terms lift a chunk's cosine.
            for i, u in enumerate(units):
                low = (u.title + " " + u.text).lower()
                dense[i] += sum(w for t, w in boosts if t in low)
        dense_order = sorted(range(len(units)), key=lambda i: (-dense[i], units[i].key))[: cfg["dense_top"]]
        lat["dense"] = (time.perf_counter() - t0) * 1000

        # --- bm25 ----------------------------------------------------------------------
        t0 = time.perf_counter()
        kw = self.bm25.search(question) if cfg["bm25"] else []
        coverage = self.bm25.coverage(question) if cfg["bm25"] else {}
        kw_order = [i for i, _ in kw]
        lat["bm25"] = (time.perf_counter() - t0) * 1000

        # --- rerank --------------------------------------------------------------------
        t0 = time.perf_counter()
        rr_order: List[int] = []
        rr_scores: Dict[int, float] = {}
        if self.reranker is not None:
            # Candidates: the dense head plus the keyword head, deduplicated, order-stable.
            cands: List[int] = []
            for i in dense_order + kw_order:
                if i not in cands:
                    cands.append(i)
            cands = cands[: cfg["rerank_k"]]
            scores = self.reranker.score(question, [units[i].rerank_text for i in cands])
            rr_scores = dict(zip(cands, scores))
            rr_order = sorted(cands, key=lambda i: (-rr_scores[i], units[i].key))
        lat["rerank"] = (time.perf_counter() - t0) * 1000

        # --- fusion --------------------------------------------------------------------
        t0 = time.perf_counter()
        doc_scores = self._fuse(dense, dense_order, cos, kw, kw_order, coverage, rr_order, rr_scores)
        ranked = sorted(doc_scores.items(), key=lambda p: (-p[1], p[0]))
        lat["fusion"] = (time.perf_counter() - t0) * 1000

        top_cos = float(cos.max()) if len(cos) else 0.0
        return {
            "top_items": [d for d, _ in ranked[: cfg["topk"]]],
            "top_scores": [round(s, 5) for _, s in ranked[: cfg["topk"]]],
            "top_title": self.titles.get(ranked[0][0], "") if ranked else "",
            "top_cosine": round(top_cos, 5),
            "top_fused": round(ranked[0][1], 5) if ranked else 0.0,
            "top_unit": units[dense_order[0]].key if dense_order else None,
            "latency_ms": {k: round(v, 3) for k, v in lat.items()},
        }

    def _fuse(self, dense, dense_order, cos, kw, kw_order, coverage, rr_order, rr_scores) -> Dict[int, float]:
        cfg, units = self.cfg, self.units
        mode = cfg["fusion"]

        if mode == "score":
            # src/store.rs search(): keyword contributes KW_WEIGHT*coverage^2/(1+rank) at the
            # *item* level; semantic contributes the normalised cosine of the item's best unit,
            # filtered by MIN_COSINE and a window below the best hit.
            fused: Dict[int, float] = defaultdict(float)
            kw_items: List[int] = []
            for i, _s in kw:                      # item-level keyword ranking
                d = units[i].item_id
                if d not in kw_items:
                    kw_items.append(d)
            for rank, d in enumerate(kw_items):
                c = coverage.get(d, 0.0)
                fused[d] += cfg["kw_weight"] * c * c / (1.0 + rank)
            best: Dict[int, float] = {}
            for i in range(len(units)):
                c = float(dense[i])
                if c < cfg["min_cosine"]:
                    continue
                d = units[i].item_id
                if c > best.get(d, -1e9):
                    best[d] = c
            if best:
                top = max(best.values())
                best = {d: c for d, c in best.items() if c >= top - cfg["cosine_window"]}
            for d, c in best.items():
                fused[d] += min(max((c - cfg["sem_floor"]) / (1.0 - cfg["sem_floor"]), 0.0), 1.0)
            return dict(fused)

        lists: List[Sequence[Any]] = []
        if mode == "rrf_units":
            lists.append(dense_order)
            if cfg["bm25"] and kw_order:
                lists.append(kw_order[: cfg["dense_top"]])
            if rr_order:
                lists.append(rr_order)
            unit_scores = rrf(lists, cfg["rrf_k"])
            # Each document takes its best unit's fused score.
            docs: Dict[int, float] = {}
            for i, s in unit_scores.items():
                d = units[i].item_id
                docs[d] = max(docs.get(d, 0.0), s)
            return docs

        if mode == "rrf_docs":
            # RAG.md §3.2 step 3: rank documents in each evidence list by their best unit,
            # then RRF the document orders.
            lists.append(best_per_doc(dense_order, units))
            if cfg["bm25"] and kw_order:
                lists.append(best_per_doc(kw_order[: cfg["dense_top"]], units))
            if rr_order:
                lists.append(best_per_doc(rr_order, units))
            return dict(rrf(lists, cfg["rrf_k"]))

        raise SystemExit(f"bad fusion mode: {mode}")


# --------------------------------------------------------------------------------------
# Evaluation + reporting
# --------------------------------------------------------------------------------------

def evaluate(cfg: Dict[str, Any], questions: List[Dict[str, Any]], name: str) -> Dict[str, Any]:
    h = Harness(cfg)
    rows: List[Dict[str, Any]] = []
    for q in questions:
        r = h.run(q["question"])
        expected = list(q.get("expected_items") or [])
        qtype = q.get("type", "factual")
        gold = set(expected)          # any expected item counts (q's like "multi" list several)
        absent = qtype == "absent" or not expected
        metric = r["top_cosine"] if cfg["absent_metric"] == "cosine" else r["top_fused"]
        rows.append({
            "id": q.get("id"),
            "question": q["question"],
            "type": qtype,
            "expected_items": expected,
            "top_items": r["top_items"],
            "top_scores": r["top_scores"],
            "top_title": r["top_title"],
            "top_cosine": r["top_cosine"],
            "top_fused": r["top_fused"],
            "absent_score": round(metric, 5),
            "hit1": (not absent) and bool(r["top_items"]) and r["top_items"][0] in gold,
            "hit3": (not absent) and any(d in gold for d in r["top_items"][:3]),
            "absent_correct": metric < cfg["absent_threshold"] if absent else None,
            "latency_ms": r["latency_ms"],
        })

    answerable = [r for r in rows if r["absent_correct"] is None]
    absents = [r for r in rows if r["absent_correct"] is not None]
    stages = ["query_embed", "dense", "bm25", "rerank", "fusion"]
    lat = {s: {"mean_ms": round(float(np.mean([r["latency_ms"][s] for r in rows])), 3),
               "max_ms": round(float(np.max([r["latency_ms"][s] for r in rows])), 3)}
           for s in stages} if rows else {}
    totals = [sum(r["latency_ms"].values()) for r in rows]
    summary = {
        "n_questions": len(rows),
        "n_answerable": len(answerable),
        "hit1": sum(r["hit1"] for r in answerable),
        "hit3": sum(r["hit3"] for r in answerable),
        "hit1_rate": round(sum(r["hit1"] for r in answerable) / max(len(answerable), 1), 4),
        "hit3_rate": round(sum(r["hit3"] for r in answerable) / max(len(answerable), 1), 4),
        "n_absent": len(absents),
        "absent_correct": sum(bool(r["absent_correct"]) for r in absents),
        "latency": lat,
        "total_latency_ms": {"mean": round(float(np.mean(totals)), 3) if totals else 0.0,
                             "max": round(float(np.max(totals)), 3) if totals else 0.0},
    }
    return {
        "name": name,
        "config": cfg,
        "n_units": len(h.units),
        "n_items": len(h.titles),
        "load_ms": {k: round(v, 1) for k, v in h.load_ms.items()},
        "peak_rss_mb": round(resource.getrusage(resource.RUSAGE_SELF).ru_maxrss / 1024, 1),
        "summary": summary,
        "questions": rows,
    }


def print_report(res: Dict[str, Any]) -> None:
    s = res["summary"]
    print(f"\n=== {res['name']} — {res['n_items']} items, {res['n_units']} units ===")
    for r in res["questions"]:
        if r["absent_correct"] is None:
            mark = "✓" if r["hit1"] else ("~" if r["hit3"] else "✗")
        else:
            mark = "✓" if r["absent_correct"] else "✗"
        q = r["question"][:46].ljust(46)
        title = (r["top_title"] or "-")[:26]
        print(f" {mark} {str(r['id'] or ''):>4}  {q}  → {title:<26} cos={r['top_cosine']:.3f} "
              f"fused={r['top_fused']:.3f} want={r['expected_items']} got={r['top_items'][:3]}")
    print(f"\n Hit@1 {s['hit1']}/{s['n_answerable']} ({s['hit1_rate']:.0%})   "
          f"Hit@3 {s['hit3']}/{s['n_answerable']} ({s['hit3_rate']:.0%})   "
          f"absent {s['absent_correct']}/{s['n_absent']}")
    print(" stage        mean ms   max ms")
    for stage, v in s["latency"].items():
        print(f"  {stage:<12}{v['mean_ms']:>8.2f}{v['max_ms']:>9.2f}")
    print(f"  {'TOTAL':<12}{s['total_latency_ms']['mean']:>8.2f}{s['total_latency_ms']['max']:>9.2f}")
    print(" load: " + ", ".join(f"{k} {v:.0f}ms" for k, v in res["load_ms"].items())
          + f"   peak RSS {res['peak_rss_mb']:.0f} MB\n")


# --------------------------------------------------------------------------------------
# Self-test
# --------------------------------------------------------------------------------------

def selftest(cfg: Dict[str, Any]) -> int:
    """Sanity: the embedder puts related sentences closer than unrelated ones."""
    emb = Embedder(cfg)
    a, b, c = "How much did I pay PayCargo?", "the customs broker invoice total", "a pixel-art sprite animation loop"
    va, vb, vc = emb.embed([a, b, c])
    emb.flush()
    sim, dis = float(va @ vb), float(va @ vc)
    print(f"selftest: cos(sim)={sim:.4f}  cos(dissim)={dis:.4f}")
    # Chunker must also mirror chunk.rs: 250 words at 100/20 → 3 chunks, overlap preserved.
    ch = chunk(" ".join(str(i) for i in range(250)), 100, 20, 600)
    assert len(ch) == 3 and ch[1].split()[:20] == ch[0].split()[-20:], "chunker mismatch"
    ok = sim > dis
    print("selftest:", "PASS" if ok else "FAIL")
    return 0 if ok else 1


# --------------------------------------------------------------------------------------
# CLI
# --------------------------------------------------------------------------------------

def main(argv: Optional[List[str]] = None) -> int:
    ap = argparse.ArgumentParser(description="Blackhole retrieval evaluation harness")
    ap.add_argument("--config", default="{}", help="JSON object of overrides, or @file.json")
    ap.add_argument("--questions", default="eval/questions.json")
    ap.add_argument("--out", default=None, help="write full JSON results here")
    ap.add_argument("--name", default=None, help="run name (defaults to the --out basename)")
    ap.add_argument("--print-config", action="store_true", help="dump the effective config and exit")
    ap.add_argument("--selftest", action="store_true")
    args = ap.parse_args(argv)

    cfg = load_config(args.config)
    if args.print_config:
        print(json.dumps(cfg, indent=2, sort_keys=True))
        return 0
    if args.selftest:
        return selftest(cfg)

    qpath = abspath(args.questions)
    if not os.path.exists(qpath):
        raise SystemExit(f"no questions file at {qpath}")
    with open(qpath, encoding="utf-8") as fh:
        questions = json.load(fh)["questions"]

    name = args.name or (os.path.splitext(os.path.basename(args.out))[0] if args.out else "baseline")
    res = evaluate(cfg, questions, name)
    print_report(res)
    if args.out:
        out = abspath(args.out)
        os.makedirs(os.path.dirname(out), exist_ok=True)
        with open(out, "w", encoding="utf-8") as fh:
            json.dump(res, fh, indent=2)
        print(f" wrote {out}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
