//! The vault: SQLite with an FTS5 index over everything swallowed, plus
//! chunk embeddings for semantic search. Queries fuse both rankings.

use crate::embed::{cosine, DIM, MODEL_ID};
use rusqlite::{params, Connection, OptionalExtension};
use std::collections::HashMap;
use std::path::Path;

pub struct Store {
    conn: Connection,
    /// All chunk vectors, kept in memory for brute-force cosine search.
    index: Vec<VecEntry>,
}

struct VecEntry {
    chunk_id: i64,
    item_id: i64,
    vec: Vec<f32>,
}

#[derive(Clone, Debug)]
pub struct Hit {
    pub id: i64,
    pub title: String,
    pub kind: String,
    pub source: Option<String>,
    pub snippet: String,
    /// How it matched: "kw" (keyword), "sem" (semantic) or "both".
    pub via: &'static str,
}

/// Semantic matches must clear this cosine (bge: relevant ≈ 0.6+, noise ≈ 0.5)…
const MIN_COSINE: f32 = 0.45;
/// Absent gate: a query with at least this many content terms, none of which occur
/// anywhere in the vault, and no strong semantic match, has no answer here.
const ABSENT_MIN_TERMS: usize = 2;
const ABSENT_MAX_COSINE: f32 = 0.70;
/// Minimum length for a query token to count as a content term.
const CONTENT_TERM_MIN_LEN: usize = 4;

/// English function words. In a small personal vault a word like "these" occurs in
/// few items and would otherwise pass for a distinctive term (measured: it flipped
/// two of nine test questions). Sorted for binary search.
const STOPWORDS: &[&str] = &[
    "about", "actually", "after", "again", "also", "another", "anything", "around", "back", "because",
    "been", "before", "being", "below", "best", "between", "both", "came", "cant", "come", "could",
    "days", "didnt", "does", "doesnt", "doing", "done", "dont", "down", "during", "each", "else",
    "even", "ever", "every", "from", "gets", "getting", "give", "given", "going", "good", "have",
    "having", "here", "hers", "however", "into", "isnt", "just", "keep", "kept", "know", "known",
    "last", "like", "little", "long", "made", "make", "many", "might", "mine", "more", "most", "much",
    "must", "myself", "need", "never", "next", "often", "once", "only", "other", "others", "over",
    "please", "really", "right", "said", "same", "seen", "shall", "should", "show", "since", "some",
    "something", "soon", "still", "such", "take", "taken", "tell", "than", "that", "their", "them",
    "then", "there", "these", "they", "thing", "things", "this", "those", "though", "through", "thus",
    "time", "took", "under", "until", "upon", "used", "using", "very", "want", "well", "went", "were",
    "what", "whats", "when", "where", "whether", "which", "while", "whom", "whose", "will", "with",
    "within", "without", "would", "youre", "your",
];

fn is_stopword(t: &str) -> bool {
    STOPWORDS.binary_search(&t).is_ok()
}

/// Lower-cased alphanumeric query tokens long enough to carry meaning, minus function words.
fn content_terms(query: &str) -> Vec<String> {
    let mut out: Vec<String> = query
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| t.len() >= CONTENT_TERM_MIN_LEN)
        .map(|t| t.to_lowercase())
        .filter(|t| !is_stopword(t))
        .collect();
    out.sort();
    out.dedup();
    out
}
/// Cosine is mapped to a 0..1 score as (cos - SEM_FLOOR) / (1 - SEM_FLOOR).
const SEM_FLOOR: f32 = 0.4;
/// Weight of a perfect keyword hit relative to a perfect semantic one.
const KW_WEIGHT: f32 = 0.6;
/// …and also be within this much of the best match, so a strong top hit
/// isn't followed by a tail of weak ones.
const COSINE_WINDOW: f32 = 0.12;

impl Store {
    pub fn open(path: &Path) -> rusqlite::Result<Store> {
        let conn = Connection::open(path)?;
        conn.execute_batch(
            r#"
            PRAGMA journal_mode = WAL;
            CREATE TABLE IF NOT EXISTS items (
                id       INTEGER PRIMARY KEY,
                title    TEXT NOT NULL,
                kind     TEXT NOT NULL,
                source   TEXT,
                content  TEXT NOT NULL,
                hash     TEXT NOT NULL UNIQUE,
                added_at INTEGER NOT NULL
            );
            CREATE VIRTUAL TABLE IF NOT EXISTS items_fts USING fts5(
                title, content, content='items', content_rowid='id', tokenize='unicode61'
            );
            CREATE TRIGGER IF NOT EXISTS items_ai AFTER INSERT ON items BEGIN
                INSERT INTO items_fts(rowid, title, content) VALUES (new.id, new.title, new.content);
            END;
            CREATE TRIGGER IF NOT EXISTS items_ad AFTER DELETE ON items BEGIN
                INSERT INTO items_fts(items_fts, rowid, title, content) VALUES ('delete', old.id, old.title, old.content);
            END;
            CREATE TABLE IF NOT EXISTS chunks (
                id      INTEGER PRIMARY KEY,
                item_id INTEGER NOT NULL REFERENCES items(id) ON DELETE CASCADE,
                ord     INTEGER NOT NULL,
                text    TEXT NOT NULL,
                vec     BLOB NOT NULL
            );
            CREATE INDEX IF NOT EXISTS chunks_item ON chunks(item_id);
            CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
            "#,
        )?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        // Vectors from a different embedding model are useless: drop them and
        // let the ingest worker re-embed everything.
        let stamped: Option<String> = conn
            .query_row("SELECT value FROM meta WHERE key = 'embed_model'", [], |r| r.get(0))
            .optional()?;
        if stamped.as_deref() != Some(MODEL_ID) {
            conn.execute("DELETE FROM chunks", [])?;
            conn.execute("INSERT OR REPLACE INTO meta (key, value) VALUES ('embed_model', ?1)", params![MODEL_ID])?;
        }
        let mut store = Store { conn, index: Vec::new() };
        store.load_index()?;
        Ok(store)
    }

    fn load_index(&mut self) -> rusqlite::Result<()> {
        let mut stmt = self.conn.prepare("SELECT id, item_id, vec FROM chunks")?;
        self.index = stmt
            .query_map([], |r| {
                let blob: Vec<u8> = r.get(2)?;
                Ok(VecEntry { chunk_id: r.get(0)?, item_id: r.get(1)?, vec: blob_to_vec(&blob) })
            })?
            .filter_map(|r| r.ok())
            .collect();
        Ok(())
    }

    /// Store embedded chunks for an item (replacing any it already had), plus one
    /// document unit — the title alone, `ord = -1`. A title is a whole-document
    /// handle: "which file has my career history" matches "Resume.pdf" when no
    /// single chunk does. Measured: +1 Hit@3 out of sample; adding opening words
    /// or an LLM description to it measured worse, so it stays title-only.
    pub fn add_chunks(&mut self, item_id: i64, chunks: &[(String, Vec<f32>)], title_unit: Option<(String, Vec<f32>)>) -> rusqlite::Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        tx.execute("DELETE FROM chunks WHERE item_id = ?1", params![item_id])?;
        self.index.retain(|e| e.item_id != item_id);
        if let Some((title, vec)) = title_unit {
            tx.execute(
                "INSERT INTO chunks (item_id, ord, text, vec) VALUES (?1, -1, ?2, ?3)",
                params![item_id, title, vec_to_blob(&vec)],
            )?;
            let chunk_id = tx.last_insert_rowid();
            self.index.push(VecEntry { chunk_id, item_id, vec });
        }
        for (ord, (text, vec)) in chunks.iter().enumerate() {
            tx.execute(
                "INSERT INTO chunks (item_id, ord, text, vec) VALUES (?1, ?2, ?3, ?4)",
                params![item_id, ord as i64, text, vec_to_blob(vec)],
            )?;
            let chunk_id = tx.last_insert_rowid();
            self.index.push(VecEntry { chunk_id, item_id, vec: vec.clone() });
        }
        tx.commit()
    }

    /// Items that were swallowed before embeddings existed, whose embedding was
    /// interrupted, or that predate document units — used to backfill on startup.
    pub fn unembedded(&self) -> Vec<(i64, String, String)> {
        self.conn
            .prepare("SELECT id, title, content FROM items WHERE id NOT IN (SELECT DISTINCT item_id FROM chunks WHERE ord = -1) ORDER BY id")
            .and_then(|mut s| s.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).map(|rows| rows.filter_map(|r| r.ok()).collect()))
            .unwrap_or_default()
    }

    /// Returns the new item id, or `None` when an identical item (same hash) is already inside.
    pub fn add(
        &self,
        title: &str,
        kind: &str,
        source: Option<&str>,
        content: &str,
        hash: &str,
        added_at: i64,
    ) -> rusqlite::Result<Option<i64>> {
        let n = self.conn.execute(
            "INSERT OR IGNORE INTO items (title, kind, source, content, hash, added_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![title, kind, source, content, hash, added_at],
        )?;
        Ok((n > 0).then(|| self.conn.last_insert_rowid()))
    }

    pub fn count(&self) -> i64 {
        self.conn
            .query_row("SELECT COUNT(*) FROM items", [], |r| r.get(0))
            .unwrap_or(0)
    }

    /// Hybrid search. Keyword (BM25) and semantic (cosine over chunks) hits are
    /// fused by score, not rank: a semantic hit contributes its normalised
    /// cosine, a keyword hit contributes KW_WEIGHT × coverage² × 1/(1+rank),
    /// where coverage is the fraction of query terms the item contains. So an
    /// exact phrase wins outright, a one-word coincidence barely registers.
    pub fn search(&self, query: &str, qvec: Option<&[f32]>, limit: usize) -> Vec<Hit> {
        if query.trim().is_empty() {
            return self.recent(limit);
        }
        let kw = self.search_keyword(query, limit * 2);
        let kw_weight = self.term_coverage(query, kw.iter().map(|h| h.id));
        let sem = match qvec {
            Some(v) => self.search_semantic(v, limit * 2),
            None => Vec::new(),
        };

        // item id -> (fused score, hit to display, matched by keyword, matched semantically)
        let mut fused: HashMap<i64, (f32, Hit, bool, bool)> = HashMap::new();
        for (rank, hit) in kw.into_iter().enumerate() {
            let cov = kw_weight.get(&hit.id).copied().unwrap_or(0.0);
            let s = KW_WEIGHT * cov * cov / (1.0 + rank as f32);
            fused.insert(hit.id, (s, hit, cov >= 0.5, false));
        }
        for (hit, cos) in sem {
            let s = ((cos - SEM_FLOOR) / (1.0 - SEM_FLOOR)).clamp(0.0, 1.0);
            match fused.get_mut(&hit.id) {
                Some(e) => {
                    e.0 += s;
                    e.3 = true;
                    // Prefer showing the semantic chunk when the keyword match was weak.
                    if !e.2 {
                        e.1.snippet = hit.snippet;
                    }
                }
                None => {
                    fused.insert(hit.id, (s, hit, false, true));
                }
            }
        }
        let mut out: Vec<(f32, Hit)> = fused
            .into_values()
            .map(|(score, mut hit, k, s)| {
                hit.via = match (k, s) {
                    (true, true) => "both",
                    (true, false) => "kw",
                    (false, true) => "sem",
                    (false, false) => "kw",
                };
                (score, hit)
            })
            .collect();
        out.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
        out.into_iter().map(|(_, h)| h).take(limit).collect()
    }

    fn search_semantic(&self, qvec: &[f32], limit: usize) -> Vec<(Hit, f32)> {
        if qvec.len() != DIM {
            return Vec::new();
        }
        // Best-scoring chunk per item.
        let mut best: HashMap<i64, (f32, i64)> = HashMap::new();
        for e in &self.index {
            let c = cosine(qvec, &e.vec);
            if c < MIN_COSINE {
                continue;
            }
            let entry = best.entry(e.item_id).or_insert((c, e.chunk_id));
            if c > entry.0 {
                *entry = (c, e.chunk_id);
            }
        }
        let mut ranked: Vec<(i64, f32, i64)> = best.into_iter().map(|(item, (c, ch))| (item, c, ch)).collect();
        ranked.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        if let Some(top) = ranked.first().map(|r| r.1) {
            ranked.retain(|r| r.1 >= top - COSINE_WINDOW);
        }
        ranked.truncate(limit);

        let mut stmt = match self.conn.prepare(
            "SELECT i.title, i.kind, i.source,
                    CASE WHEN c.ord < 0 THEN (SELECT substr(text, 1, 160) FROM chunks WHERE item_id = i.id AND ord = 0)
                         ELSE substr(c.text, 1, 160) END
             FROM items i JOIN chunks c ON c.id = ?2 WHERE i.id = ?1",
        ) {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        ranked
            .into_iter()
            .filter_map(|(item_id, c, chunk_id)| {
                stmt.query_row(params![item_id, chunk_id], |r| {
                    Ok((
                        Hit {
                            id: item_id,
                            title: r.get(0)?,
                            kind: r.get(1)?,
                            source: r.get(2)?,
                            snippet: r.get::<_, String>(3)?.replace(['\r', '\n'], " "),
                            via: "sem",
                        },
                        c,
                    ))
                })
                .ok()
            })
            .collect()
    }

    #[allow(dead_code)]
    pub fn title_of(&self, id: i64) -> Option<String> {
        self.conn.query_row("SELECT title FROM items WHERE id = ?1", params![id], |r| r.get(0)).ok()
    }

    /// Does any item contain this word?
    pub fn contains_term(&self, term: &str) -> bool {
        self.items_containing(term) > 0
    }

    /// Number of items whose text contains `term` (FTS5 token match).
    fn items_containing(&self, term: &str) -> i64 {
        self.conn
            .query_row("SELECT COUNT(*) FROM items_fts WHERE items_fts MATCH ?1", params![format!("\"{term}\"")], |r| r.get(0))
            .unwrap_or(0)
    }

    /// Highest cosine between the query vector and anything in the vault.
    pub fn best_cosine(&self, qvec: &[f32]) -> f32 {
        if qvec.len() != DIM {
            return 0.0;
        }
        self.index.iter().map(|e| cosine(qvec, &e.vec)).fold(0.0, f32::max)
    }

    /// "Not in your vault": the query has ≥2 content terms, none of them occurs in
    /// any item, and nothing matches strongly by meaning. Lexical on purpose — cosine
    /// thresholds and rank margins could not separate unanswerable questions
    /// (they score as high as real ones); this rule caught 2/2 with 0 false absents.
    pub fn absent(&self, query: &str, qvec: Option<&[f32]>) -> bool {
        let terms = content_terms(query);
        if terms.len() < ABSENT_MIN_TERMS {
            return false;
        }
        if terms.iter().any(|t| self.items_containing(t) > 0) {
            return false;
        }
        qvec.map(|v| self.best_cosine(v) < ABSENT_MAX_COSINE).unwrap_or(true)
    }

    /// Fraction of the query's terms each of the given items matches.
    fn term_coverage(&self, query: &str, ids: impl Iterator<Item = i64>) -> HashMap<i64, f32> {
        let tokens: Vec<String> = fts_tokens(query);
        let mut counts: HashMap<i64, f32> = ids.map(|id| (id, 0.0)).collect();
        if tokens.is_empty() || counts.is_empty() {
            return counts;
        }
        let n = tokens.len();
        let Ok(mut stmt) = self.conn.prepare("SELECT rowid FROM items_fts WHERE items_fts MATCH ?1") else {
            return counts;
        };
        for (i, t) in tokens.iter().enumerate() {
            let expr = if i + 1 == n { format!("\"{t}\"*") } else { format!("\"{t}\"") };
            let Ok(rows) = stmt.query_map(params![expr], |r| r.get::<_, i64>(0)) else { continue };
            for id in rows.flatten() {
                if let Some(c) = counts.get_mut(&id) {
                    *c += 1.0 / n as f32;
                }
            }
        }
        counts
    }

    /// Context for grounding an answer, as (title, text) blocks.
    ///
    /// Chunks are scored by cosine plus a boost for literally containing
    /// distinctive query terms. The best ones are taken along with their
    /// neighbours (so a fact split across a boundary, or an answer that lives
    /// in the line *before* the matching one, is still in view), the top
    /// document's opening chunk is always included, and everything is emitted
    /// in document order within a word budget.
    /// Context for grounding an answer, with an optional cross-encoder deciding the top document:
    /// the cheap pipeline proposes the candidates, the reranker scores the best
    /// `rerank::CANDIDATES` of them, and the document with the highest logit
    /// becomes the top document. Documents it never saw keep their cheap order.
    /// (Rank-fusing the reranker with RRF instead measured a regression.)
    pub fn context_reranked(
        &self,
        query: &str,
        qvec: &[f32],
        budget_words: usize,
        rerank: Option<&dyn Fn(&str, &[String]) -> Option<Vec<f32>>>,
    ) -> Vec<(String, String)> {
        #[derive(Clone)]
        struct C {
            id: i64,
            item: i64,
            ord: i64,
            words: usize,
            score: f32,
        }
        let mut scored: Vec<(f32, i64)> = self.index.iter().map(|e| (cosine(qvec, &e.vec), e.chunk_id)).collect();
        scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
        scored.truncate(40);

        // Distinctive query terms (names, numbers, jargon) that embeddings blur:
        // weight each by how rare it is across the vault, so "list", "with" or
        // "dates" don't drag random chunks up.
        let n_items = self.count().max(1) as f32;
        let terms: Vec<(String, f32)> = content_terms(query)
            .into_iter()
            .filter_map(|t| {
                let docs = self.items_containing(&t);
                let frac = docs as f32 / n_items;
                // Present in more than a third of items: not distinctive.
                (frac <= 0.34).then(|| (t, 0.08 * (1.0 - frac)))
            })
            .collect();
        let Ok(mut meta) = self.conn.prepare("SELECT item_id, ord, text FROM chunks WHERE id = ?1") else {
            return Vec::new();
        };
        let mut cands: Vec<C> = scored
            .iter()
            .filter_map(|&(c, id)| {
                meta.query_row(params![id], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?, r.get::<_, String>(2)?)))
                    .ok()
                    .map(|(item, ord, text)| {
                        let lower = text.to_lowercase();
                        let boost: f32 = terms.iter().filter(|(t, _)| lower.contains(t.as_str())).map(|(_, w)| *w).sum();
                        C { id, item, ord, words: text.split_whitespace().count(), score: c + boost }
                    })
            })
            .collect();
        cands.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
        if cands.is_empty() {
            return Vec::new();
        }
        // The "top document" is judged on its best three chunks (decayed), not
        // its single best one: a long document that matches moderately all over
        // (a résumé for "list my employers") beats a short one with one lucky chunk.
        let mut doc_scores: HashMap<i64, Vec<f32>> = HashMap::new();
        for c in &cands {
            doc_scores.entry(c.item).or_default().push(c.score);
        }
        let mut top_item = doc_scores
            .iter()
            .map(|(item, scores)| {
                let s: f32 = scores.iter().take(3).enumerate().map(|(i, v)| v * [1.0, 0.5, 0.25][i]).sum();
                (*item, s)
            })
            .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal))
            .map(|(item, _)| item)
            .unwrap_or(cands[0].item);
        if let Some(rerank) = rerank {
            // Passage text per candidate; a document unit is scored as "title: opening words".
            let head: Vec<&C> = cands.iter().take(crate::rerank::CANDIDATES).collect();
            let passages: Vec<String> = head
                .iter()
                .map(|c| {
                    if c.ord < 0 {
                        let (title, content): (String, String) = self
                            .conn
                            .query_row("SELECT title, content FROM items WHERE id = ?1", params![c.item], |r| Ok((r.get(0)?, r.get(1)?)))
                            .unwrap_or_default();
                        format!("{title}: {}", content.split_whitespace().take(crate::rerank::DOC_UNIT_WORDS).collect::<Vec<_>>().join(" "))
                    } else {
                        meta.query_row(params![c.id], |r| r.get::<_, String>(2)).unwrap_or_default()
                    }
                })
                .collect();
            if let Some(logits) = rerank(query, &passages) {
                let mut best: HashMap<i64, f32> = HashMap::new();
                for (c, &l) in head.iter().zip(&logits) {
                    let e = best.entry(c.item).or_insert(f32::NEG_INFINITY);
                    if l > *e {
                        *e = l;
                    }
                }
                if let Some((item, _)) = best.iter().max_by(|a, b| a.1.partial_cmp(b.1).unwrap_or(std::cmp::Ordering::Equal)) {
                    top_item = *item;
                }
                let line: Vec<String> = head.iter().zip(&logits).map(|(c, l)| format!("{l:.2} {}#{}", c.item, c.ord)).collect();
                crate::util::log(&format!("rerank: {}", line.join(" | ")));
                // Seeds from the chosen document go first so its evidence fills the budget.
                cands.sort_by(|a, b| (b.item == top_item).cmp(&(a.item == top_item)).then(b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal)));
            }
        }
        let top = cands.iter().find(|c| c.item == top_item).cloned().unwrap_or_else(|| cands[0].clone());
        {
            // Retrieval trace for debugging odd answers (pairs with last_ask.txt).
            let mut title_of = self.conn.prepare("SELECT title FROM items WHERE id = ?1").ok();
            let line: Vec<String> = cands
                .iter()
                .take(8)
                .map(|c| {
                    let t: String = title_of.as_mut().and_then(|st| st.query_row(params![c.item], |r| r.get(0)).ok()).unwrap_or_default();
                    format!("{:.3} {}#{}", c.score, t.chars().take(14).collect::<String>(), c.ord)
                })
                .collect();
            crate::util::log(&format!("retrieval for {query:?}: {}", line.join(" | ")));
        }

        // Pick chunks under the budget: seeds in score order, each with its neighbours.
        let Ok(mut by_pos) = self.conn.prepare("SELECT id, text FROM chunks WHERE item_id = ?1 AND ord = ?2") else {
            return Vec::new();
        };
        let mut chosen: HashMap<i64, (i64, i64, String)> = HashMap::new(); // id -> (item, ord, text)
        let mut used = 0usize;
        let mut try_add = |item: i64, ord: i64, chosen: &mut HashMap<i64, (i64, i64, String)>, used: &mut usize| -> bool {
            if ord < 0 {
                return false;
            }
            let Ok((id, text)) = by_pos.query_row(params![item, ord], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))) else {
                return false;
            };
            if chosen.contains_key(&id) {
                return true;
            }
            let w = text.split_whitespace().count();
            if *used + w > budget_words {
                return false;
            }
            *used += w;
            chosen.insert(id, (item, ord, text));
            true
        };
        // A small top document (a résumé, a letter, a receipt) goes in whole:
        // questions about order or totals need all of it, not fragments.
        let (n_chunks, raw_words): (i64, i64) = self
            .conn
            .query_row(
                "SELECT COUNT(*), COALESCE(SUM(length(text) - length(replace(text, ' ', '')) + 1), 0) FROM chunks WHERE item_id = ?1 AND ord >= 0",
                params![top.item],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap_or((0, 0));
        // Consecutive chunks share OVERLAP_WORDS, which the merge below removes again.
        let doc_words = (raw_words as usize).saturating_sub(crate::chunk::OVERLAP_WORDS * (n_chunks.max(1) as usize - 1));
        if n_chunks > 0 && doc_words <= budget_words {
            let Ok(mut all) = self.conn.prepare("SELECT id, ord, text FROM chunks WHERE item_id = ?1 AND ord >= 0 ORDER BY ord") else {
                return Vec::new();
            };
            if let Ok(rows) = all.query_map(params![top.item], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?, r.get::<_, String>(2)?))) {
                for (id, ord, text) in rows.flatten() {
                    chosen.insert(id, (top.item, ord, text));
                }
            }
            used += doc_words;
        }
        // The top document's opening chunk carries headline facts (names, current role, totals).
        try_add(top.item, 0, &mut chosen, &mut used);
        for c in &cands {
            let _ = c.id;
            let _ = c.words;
            if !try_add(c.item, c.ord, &mut chosen, &mut used) {
                continue;
            }
            try_add(c.item, c.ord - 1, &mut chosen, &mut used);
            try_add(c.item, c.ord + 1, &mut chosen, &mut used);
            if used >= budget_words * 9 / 10 {
                break;
            }
        }

        // Emit in document order, merging runs of consecutive chunks into one block.
        let mut list: Vec<(i64, i64, String)> = chosen.into_values().collect();
        list.sort_by_key(|(item, ord, _)| (*item, *ord));
        let Ok(mut title_of) = self.conn.prepare("SELECT title FROM items WHERE id = ?1") else {
            return Vec::new();
        };
        let mut out: Vec<(String, String)> = Vec::new();
        let mut last: Option<(i64, i64)> = None;
        for (item, ord, text) in list {
            let contiguous = last.map(|(i, o)| i == item && o + 1 == ord).unwrap_or(false);
            if contiguous {
                if let Some(block) = out.last_mut() {
                    // Chunks overlap by their first OVERLAP words; drop that prefix.
                    let skip = crate::chunk::OVERLAP_WORDS;
                    let tail: Vec<&str> = text.split_whitespace().skip(skip).collect();
                    block.1.push(' ');
                    block.1.push_str(&tail.join(" "));
                }
            } else {
                let title: String = title_of.query_row(params![item], |r| r.get(0)).unwrap_or_default();
                out.push((title, text));
            }
            last = Some((item, ord));
        }
        out
    }

    fn search_keyword(&self, query: &str, limit: usize) -> Vec<Hit> {
        let Some(fts) = to_fts_query(query) else {
            return Vec::new();
        };
        let mut stmt = match self.conn.prepare(
            "SELECT i.id, i.title, i.kind, i.source,
                    snippet(items_fts, 1, '\u{1}', '\u{2}', ' … ', 14)
             FROM items_fts JOIN items i ON i.id = items_fts.rowid
             WHERE items_fts MATCH ?1
             ORDER BY bm25(items_fts, 3.0, 1.0)
             LIMIT ?2",
        ) {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        stmt.query_map(params![fts, limit as i64], |r| {
            Ok(Hit {
                id: r.get(0)?,
                title: r.get(1)?,
                kind: r.get(2)?,
                source: r.get(3)?,
                snippet: r.get(4)?,
                via: "kw",
            })
        })
        .map(|rows| rows.filter_map(|r| r.ok()).collect())
        .unwrap_or_default()
    }

    pub fn recent(&self, limit: usize) -> Vec<Hit> {
        let mut stmt = match self.conn.prepare(
            "SELECT id, title, kind, source, substr(content, 1, 120)
             FROM items ORDER BY added_at DESC, id DESC LIMIT ?1",
        ) {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        stmt.query_map(params![limit as i64], |r| {
            Ok(Hit {
                id: r.get(0)?,
                title: r.get(1)?,
                kind: r.get(2)?,
                source: r.get(3)?,
                snippet: r.get::<_, String>(4)?.replace(['\r', '\n'], " "),
                via: "",
            })
        })
        .map(|rows| rows.filter_map(|r| r.ok()).collect())
        .unwrap_or_default()
    }

    pub fn content(&self, id: i64) -> Option<String> {
        self.conn
            .query_row("SELECT content FROM items WHERE id = ?1", params![id], |r| r.get(0))
            .optional()
            .ok()
            .flatten()
    }

    pub fn delete(&mut self, id: i64) -> rusqlite::Result<()> {
        self.conn.execute("DELETE FROM items WHERE id = ?1", params![id])?;
        self.index.retain(|e| e.item_id != id);
        Ok(())
    }
}

fn vec_to_blob(v: &[f32]) -> Vec<u8> {
    v.iter().flat_map(|x| x.to_le_bytes()).collect()
}

fn blob_to_vec(b: &[u8]) -> Vec<f32> {
    b.chunks_exact(4).map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect()
}

/// Turn free text into a safe FTS5 MATCH expression: every token quoted and
/// OR-ed (so one typo doesn't empty the list; BM25 still ranks docs matching
/// more terms higher), the last one as a prefix so results update while typing.
fn fts_tokens(q: &str) -> Vec<String> {
    q.split_whitespace()
        .map(|t| t.replace('"', ""))
        .filter(|t| !t.is_empty())
        .collect()
}

fn to_fts_query(q: &str) -> Option<String> {
    let tokens = fts_tokens(q);
    if tokens.is_empty() {
        return None;
    }
    let n = tokens.len();
    Some(
        tokens
            .iter()
            .enumerate()
            .map(|(i, t)| if i + 1 == n { format!("\"{t}\"*") } else { format!("\"{t}\"") })
            .collect::<Vec<_>>()
            .join(" OR "),
    )
}

#[cfg(debug_assertions)]
impl Store {
    pub fn debug_semantic(&self, qvec: &[f32]) {
        let mut best: HashMap<i64, (f32, i64)> = HashMap::new();
        for e in &self.index {
            let c = cosine(qvec, &e.vec);
            let entry = best.entry(e.item_id).or_insert((c, e.chunk_id));
            if c > entry.0 { *entry = (c, e.chunk_id); }
        }
        let mut v: Vec<_> = best.into_iter().collect();
        v.sort_by(|a, b| b.1.0.partial_cmp(&a.1.0).unwrap());
        for (item, (c, ch)) in v {
            let (title, text): (String, String) = self.conn.query_row("SELECT i.title, substr(c.text,1,60) FROM items i JOIN chunks c ON c.id=?2 WHERE i.id=?1", params![item, ch], |r| Ok((r.get(0)?, r.get(1)?))).unwrap();
            println!("   {c:.3} {} — {}", title.chars().take(30).collect::<String>(), text.replace('\n', " "));
        }
    }
}
