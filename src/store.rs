//! The vault: SQLite with an FTS5 index over everything swallowed, plus
//! chunk embeddings for semantic search. Queries fuse both rankings.

use crate::embed::{cosine, DIM, MODEL_ID};
use rusqlite::{params, Connection, OptionalExtension};
use std::collections::HashMap;
use std::path::Path;

/// Bump when extraction or chunking changes in a way stored text should follow.
pub const TEXT_VERSION: &str = "4";

pub struct Store {
    /// Set at open when the stored text predates the current extraction/chunking.
    pub stale_text: bool,
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
    /// User tags, space-separated without the '#'.
    pub tags: String,
    /// When it was swallowed (unix seconds) — browse filters and row dates.
    pub added_at: i64,
}

/// Semantic matches must clear this cosine (bge: relevant ≈ 0.6+, noise ≈ 0.5)…
const MIN_COSINE: f32 = 0.45;
/// Absent gate: a query with at least this many content terms, none of which occur
/// anywhere in the vault, and no strong semantic match, has no answer here.
const ABSENT_MIN_TERMS: usize = 2;
const ABSENT_MAX_COSINE: f32 = 0.70;
// One threshold for every query length, on purpose. Measured on a 400-item synthetic
// vault (2026-09-16, RAG.md): two-word questions whose words are absent but whose
// meaning is present score 0.63–0.66, and nonsense scores 0.57–0.64 — the bands
// overlap outright ("photosynthesis chlorophyll" beat "funding allowance"), so a
// softer cosine rule for short queries would only stop the gate from ever firing.
// What short queries needed was the plural tolerance in `term_present`.
/// Minimum length for a query token to count as a content term.
const CONTENT_TERM_MIN_LEN: usize = 4;

/// English function words. In a small personal vault a word like "these" occurs in
/// few items and would otherwise pass for a distinctive term (measured: it flipped
/// two of nine test questions). Sorted for binary search.
const STOPWORDS: &[&str] = &[
    "about",
    "actually",
    "after",
    "again",
    "also",
    "another",
    "anything",
    "around",
    "back",
    "because",
    "been",
    "before",
    "being",
    "below",
    "best",
    "between",
    "both",
    "came",
    "cant",
    "come",
    "could",
    "days",
    "didnt",
    "does",
    "doesnt",
    "doing",
    "done",
    "dont",
    "down",
    "during",
    "each",
    "else",
    "even",
    "ever",
    "every",
    "from",
    "gets",
    "getting",
    "give",
    "given",
    "going",
    "good",
    "have",
    "having",
    "here",
    "hers",
    "however",
    "into",
    "isnt",
    "just",
    "keep",
    "kept",
    "know",
    "known",
    "last",
    "like",
    "little",
    "long",
    "made",
    "make",
    "many",
    "might",
    "mine",
    "more",
    "most",
    "much",
    "must",
    "myself",
    "need",
    "never",
    "next",
    "often",
    "once",
    "only",
    "other",
    "others",
    "over",
    "please",
    "really",
    "right",
    "said",
    "same",
    "seen",
    "shall",
    "should",
    "show",
    "since",
    "some",
    "something",
    "soon",
    "still",
    "such",
    "take",
    "taken",
    "tell",
    "than",
    "that",
    "their",
    "them",
    "then",
    "there",
    "these",
    "they",
    "thing",
    "things",
    "this",
    "those",
    "though",
    "through",
    "thus",
    "time",
    "took",
    "under",
    "until",
    "upon",
    "used",
    "using",
    "very",
    "want",
    "well",
    "went",
    "were",
    "what",
    "whats",
    "when",
    "where",
    "whether",
    "which",
    "while",
    "whom",
    "whose",
    "will",
    "with",
    "within",
    "without",
    "would",
    "youre",
    "your",
];

fn is_stopword(t: &str) -> bool {
    STOPWORDS.binary_search(&t).is_ok()
}

/// Lower-cased alphanumeric query tokens long enough to carry meaning, minus function words.
fn content_terms(query: &str) -> Vec<String> {
    let mut out: Vec<String> = query.split(|c: char| !c.is_alphanumeric()).filter(|t| t.len() >= CONTENT_TERM_MIN_LEN).map(|t| t.to_lowercase()).filter(|t| !is_stopword(t)).collect();
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
/// How many recent items a filtered browse looks at (`kind:pdf` with no query).
const BROWSE_POOL: usize = 600;

/// A failed ingest, kept so the panel can list, retry or dismiss it (FEATURES.md §3.5, §7).
#[derive(Clone, Debug)]
pub struct Failure {
    pub id: i64,
    /// Where it came from ("" for pasted text).
    pub path: String,
    pub title: String,
    pub error: String,
    pub at: i64,
}

/// Source extensions treated as code (the `kind:code` filter and the code styling
/// of snippets in the panel).
pub const CODE_EXTS: &[&str] = &[
    "rs", "py", "js", "mjs", "ts", "tsx", "jsx", "go", "c", "h", "cpp", "cc", "hpp", "cs", "java", "kt", "rb", "php", "swift", "lua", "sh", "bash", "ps1", "bat", "sql", "json", "toml", "yaml", "yml",
    "xml", "html", "css", "vim",
];

/// Is this file name a code file (by extension)?
pub fn is_code_name(name: &str) -> bool {
    let lower = name.to_lowercase();
    match lower.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => CODE_EXTS.contains(&ext),
        _ => false,
    }
}

/// Browse filters typed into the query: `kind:pdf`, `since:7d`, `source:invoice`.
/// They work like the `#tag` words — pulled out of the text, applied to the result set.
#[derive(Clone, Debug, Default)]
pub struct Filters {
    /// Item kinds to keep ("pdf", "note", "image", "text", "file", "docx"/"xlsx"/"pptx", "web"); empty = any.
    pub kinds: Vec<String>,
    /// `kind:code` — any item whose source/title looks like a code file.
    pub code: bool,
    /// Unix seconds: nothing older than this.
    pub since: Option<i64>,
    /// Lower-cased substring of the source path or the title.
    pub source: Option<String>,
    /// The filters as typed, for the status line.
    pub labels: Vec<String>,
}

impl Filters {
    pub fn is_empty(&self) -> bool {
        self.kinds.is_empty() && !self.code && self.since.is_none() && self.source.is_none()
    }

    /// "kind:pdf · since:7d" for the status line.
    pub fn describe(&self) -> String {
        self.labels.join(" \u{b7} ")
    }

    pub fn matches(&self, h: &Hit) -> bool {
        if !self.kinds.is_empty() && !self.kinds.iter().any(|k| *k == h.kind) {
            return false;
        }
        // A fetched or saved page is kind "web" — plain text now, so `kind:code` skips it
        // even when it came from a .html file.
        if self.code && (h.kind == "web" || !is_code_name(h.source.as_deref().unwrap_or(&h.title))) {
            return false;
        }
        if let Some(t) = self.since {
            if h.added_at < t {
                return false;
            }
        }
        if let Some(sub) = &self.source {
            let hay = format!("{} {}", h.source.as_deref().unwrap_or(""), h.title).to_lowercase();
            if !hay.contains(sub) {
                return false;
            }
        }
        true
    }
}

/// Midnight today, this Monday and the 1st of this month, in unix seconds (local clock).
fn day_starts() -> (i64, i64, i64) {
    let now = crate::util::now_secs();
    let st = unsafe { windows::Win32::System::SystemInformation::GetLocalTime() };
    let today = now - (st.wHour as i64 * 3600 + st.wMinute as i64 * 60 + st.wSecond as i64);
    // wDayOfWeek: 0 = Sunday. Weeks start on Monday.
    let back = (st.wDayOfWeek as i64 + 6) % 7;
    (today, today - back * 86400, today - (st.wDay as i64 - 1) * 86400)
}

/// "2026-09-16" → unix seconds at that date's midnight (UTC; close enough for a filter).
fn parse_date(s: &str) -> Option<i64> {
    let mut it = s.split('-');
    let (y, m, d) = (it.next()?.parse::<i64>().ok()?, it.next()?.parse::<i64>().ok()?, it.next()?.parse::<i64>().ok()?);
    if it.next().is_some() || !(1..=12).contains(&m) || !(1..=31).contains(&d) || !(1970..=3000).contains(&y) {
        return None;
    }
    // Days from the civil epoch (Howard Hinnant's algorithm).
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    Some((era * 146097 + doe - 719468) * 86400)
}

/// `since:7d` / `since:2w` / `since:today` / `since:week` / `since:2026-09-01` → a timestamp.
fn parse_since(v: &str) -> Option<i64> {
    let now = crate::util::now_secs();
    let (today, week, month) = day_starts();
    match v {
        "today" => return Some(today),
        "yesterday" => return Some(today - 86400),
        "week" | "thisweek" => return Some(week),
        "month" | "thismonth" => return Some(month),
        _ => {}
    }
    if v.contains('-') {
        return parse_date(v);
    }
    let (num, unit) = v.split_at(v.find(|c: char| !c.is_ascii_digit()).unwrap_or(v.len()));
    let n: i64 = num.parse().ok()?;
    let secs = match unit {
        "" | "d" | "day" | "days" => 86400,
        "h" | "hour" | "hours" => 3600,
        "w" | "week" | "weeks" => 7 * 86400,
        "m" | "month" | "months" => 30 * 86400,
        "y" | "year" | "years" => 365 * 86400,
        _ => return None,
    };
    Some(now - n * secs)
}

/// The stored kinds a `kind:` word means ("images" → image, "files" → file, "code" → any
/// code file). Returns None for a word that is not a kind, so it stays part of the query.
fn parse_kind(v: &str) -> Option<(Vec<String>, bool)> {
    let one = |k: &str| Some((vec![k.to_string()], false));
    match v {
        "pdf" | "pdfs" => one("pdf"),
        "note" | "notes" => one("note"),
        "image" | "images" | "img" | "picture" | "pictures" | "screenshot" | "screenshots" => one("image"),
        "text" | "txt" | "texts" => one("text"),
        "file" | "files" => one("file"),
        "code" => Some((Vec::new(), true)),
        "docx" | "word" => one("docx"),
        "xlsx" | "excel" | "sheet" | "sheets" | "spreadsheet" | "spreadsheets" => one("xlsx"),
        "pptx" | "powerpoint" | "slide" | "slides" | "deck" | "decks" | "presentation" | "presentations" => one("pptx"),
        "web" | "page" | "pages" | "url" | "urls" | "link" | "links" | "html" | "site" | "sites" => one("web"),
        "office" => Some((vec!["docx".into(), "xlsx".into(), "pptx".into()], false)),
        "doc" | "docs" | "document" | "documents" => Some((vec!["pdf".into(), "file".into(), "docx".into(), "xlsx".into(), "pptx".into()], false)),
        _ => None,
    }
}

/// Pull `kind:`/`since:`/`source:` words out of a query; the rest is the search text.
/// An unknown value ("kind:banana") is left in the query rather than silently dropped.
pub fn parse_filters(query: &str) -> (Filters, String) {
    let mut f = Filters::default();
    let mut rest = Vec::new();
    for w in query.split_whitespace() {
        let lower = w.to_lowercase();
        let Some((key, value)) = lower.split_once(':') else {
            rest.push(w);
            continue;
        };
        let value = value.trim();
        if value.is_empty() {
            rest.push(w);
            continue;
        }
        match key {
            "kind" | "type" | "is" => match parse_kind(value) {
                Some((kinds, code)) => {
                    f.kinds.extend(kinds);
                    f.code |= code;
                    f.labels.push(format!("kind:{value}"));
                }
                None => rest.push(w),
            },
            "since" | "after" => match parse_since(value) {
                Some(t) => {
                    f.since = Some(f.since.map_or(t, |old: i64| old.max(t)));
                    f.labels.push(format!("since:{value}"));
                }
                None => rest.push(w),
            },
            "source" | "from" | "in" => {
                f.source = Some(value.to_string());
                f.labels.push(format!("source:{value}"));
            }
            _ => rest.push(w),
        }
    }
    (f, rest.join(" "))
}

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
            -- Things that fell in but could not be read (FEATURES.md §3.5): shown in the
            -- panel's undigested list, where they can be retried or dismissed.
            CREATE TABLE IF NOT EXISTS undigested (
                id    INTEGER PRIMARY KEY,
                path  TEXT NOT NULL DEFAULT '',
                title TEXT NOT NULL,
                error TEXT NOT NULL,
                at    INTEGER NOT NULL
            );
            "#,
        )?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        // Vectors from a different embedding model are useless: drop them and
        // let the ingest worker re-embed everything.
        let stamped: Option<String> = conn.query_row("SELECT value FROM meta WHERE key = 'embed_model'", [], |r| r.get(0)).optional()?;
        if stamped.as_deref() != Some(MODEL_ID) {
            conn.execute("DELETE FROM chunks", [])?;
            conn.execute("INSERT OR REPLACE INTO meta (key, value) VALUES ('embed_model', ?1)", params![MODEL_ID])?;
        }
        // Text pipeline changed (layout-aware PDFs, line breaks kept in chunks): every
        // item re-chunks, and the ingest worker re-extracts PDFs whose file is still around.
        // Notes named by the user keep their title across saves (migration: older vaults lack the column).
        let _ = conn.execute("ALTER TABLE items ADD COLUMN custom_title INTEGER NOT NULL DEFAULT 0", []);
        // Tags (stored as ",a,b," for LIKE matching) and the note's last cursor "row,col".
        let _ = conn.execute("ALTER TABLE items ADD COLUMN tags TEXT NOT NULL DEFAULT ''", []);
        let _ = conn.execute("ALTER TABLE items ADD COLUMN cursor TEXT NOT NULL DEFAULT ''", []);
        // Blackhole's own copy of a dropped image/PDF (source may move or vanish).
        let _ = conn.execute("ALTER TABLE items ADD COLUMN stored TEXT NOT NULL DEFAULT ''", []);
        let text_stamp: Option<String> = conn.query_row("SELECT value FROM meta WHERE key = 'text_version'", [], |r| r.get(0)).optional()?;
        let stale_text = text_stamp.as_deref() != Some(TEXT_VERSION);
        if stale_text {
            conn.execute("DELETE FROM chunks", [])?;
            conn.execute("INSERT OR REPLACE INTO meta (key, value) VALUES ('text_version', ?1)", params![TEXT_VERSION])?;
        }
        let mut store = Store { conn, index: Vec::new(), stale_text };
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
            tx.execute("INSERT INTO chunks (item_id, ord, text, vec) VALUES (?1, -1, ?2, ?3)", params![item_id, title, vec_to_blob(&vec)])?;
            let chunk_id = tx.last_insert_rowid();
            self.index.push(VecEntry { chunk_id, item_id, vec });
        }
        for (ord, (text, vec)) in chunks.iter().enumerate() {
            tx.execute("INSERT INTO chunks (item_id, ord, text, vec) VALUES (?1, ?2, ?3, ?4)", params![item_id, ord as i64, text, vec_to_blob(vec)])?;
            let chunk_id = tx.last_insert_rowid();
            self.index.push(VecEntry { chunk_id, item_id, vec: vec.clone() });
        }
        tx.commit()
    }

    /// Items that were swallowed before embeddings existed, whose embedding was
    /// interrupted, or that predate document units — used to backfill on startup.
    /// Items of one kind with their source paths, for re-extraction.
    pub fn items_with_source(&self, kind: &str) -> Vec<(i64, String)> {
        let Ok(mut st) = self.conn.prepare("SELECT id, source FROM items WHERE kind = ?1 AND source IS NOT NULL") else {
            return Vec::new();
        };
        st.query_map(params![kind], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))).map(|rows| rows.flatten().collect()).unwrap_or_default()
    }

    /// Replace an item's text (chunks are dropped; the ingest worker re-embeds it).
    pub fn update_content(&mut self, id: i64, content: &str) -> rusqlite::Result<()> {
        self.conn.execute("UPDATE items SET content = ?1 WHERE id = ?2", params![content, id])?;
        self.conn.execute("DELETE FROM chunks WHERE item_id = ?1", params![id])?;
        // items_fts is an external-content table; updates need a rebuild.
        self.conn.execute("INSERT INTO items_fts(items_fts) VALUES('rebuild')", [])?;
        self.index.retain(|e| e.item_id != id);
        Ok(())
    }

    pub fn unembedded(&self) -> Vec<(i64, String, String)> {
        self.conn
            .prepare("SELECT id, title, content FROM items WHERE id NOT IN (SELECT DISTINCT item_id FROM chunks WHERE ord = -1) ORDER BY id")
            .and_then(|mut s| s.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).map(|rows| rows.filter_map(|r| r.ok()).collect()))
            .unwrap_or_default()
    }

    /// Returns the new item id, or `None` when an identical item (same hash) is already inside.
    pub fn add(&self, title: &str, kind: &str, source: Option<&str>, content: &str, hash: &str, added_at: i64) -> rusqlite::Result<Option<i64>> {
        self.add_stored(title, kind, source, content, hash, added_at, None)
    }

    /// `stored`: path of Blackhole's own copy of the file, if one was made.
    pub fn add_stored(&self, title: &str, kind: &str, source: Option<&str>, content: &str, hash: &str, added_at: i64, stored: Option<&str>) -> rusqlite::Result<Option<i64>> {
        let n = self.conn.execute(
            "INSERT OR IGNORE INTO items (title, kind, source, content, hash, added_at, stored) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![title, kind, source, content, hash, added_at, stored.unwrap_or("")],
        )?;
        Ok((n > 0).then(|| self.conn.last_insert_rowid()))
    }

    /// One line per item — title and opening words — newest first, for questions
    /// about the files themselves ("which document is proof I paid").
    pub fn catalog(&self, max: usize) -> String {
        let Ok(mut st) = self.conn.prepare("SELECT title, kind, content FROM items ORDER BY added_at DESC LIMIT ?1") else {
            return String::new();
        };
        let Ok(rows) = st.query_map(params![max as i64], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?))) else {
            return String::new();
        };
        let mut out = String::new();
        for (title, kind, content) in rows.flatten() {
            let opening: String = content.split_whitespace().take(14).collect::<Vec<_>>().join(" ");
            let same = opening.starts_with(title.trim_end_matches('…')) || title.starts_with(&opening);
            if same || opening.is_empty() {
                out.push_str(&format!("- {title} [{kind}]\n"));
            } else {
                out.push_str(&format!("- {title} [{kind}]: {opening}…\n"));
            }
        }
        out
    }

    /// Chunks (including document units) across the whole vault — the size of the
    /// brute-force cosine scan, so the number that matters for search latency.
    #[allow(dead_code)] // askeval --scale
    pub fn chunk_count(&self) -> usize {
        self.index.len()
    }

    pub fn count(&self) -> i64 {
        self.conn.query_row("SELECT COUNT(*) FROM items", [], |r| r.get(0)).unwrap_or(0)
    }

    /// Hybrid search. Keyword (BM25) and semantic (cosine over chunks) hits are
    /// fused by score, not rank: a semantic hit contributes its normalised
    /// cosine, a keyword hit contributes KW_WEIGHT × coverage² × 1/(1+rank),
    /// where coverage is the fraction of query terms the item contains. So an
    /// exact phrase wins outright, a one-word coincidence barely registers.
    pub fn search(&self, query: &str, qvec: Option<&[f32]>, limit: usize) -> Vec<Hit> {
        // "#tag" words filter by tag, "kind:/since:/source:" words browse; whatever is
        // left is the search itself. An empty rest with filters is a browse (recent order).
        let (filters, without_filters) = parse_filters(query);
        let (tags, rest) = split_tags(&without_filters);
        if tags.is_empty() && filters.is_empty() {
            let mut hits = self.search_inner(query, qvec, limit);
            self.attach_tags(&mut hits);
            return hits;
        }
        // Filtered: cast a wider net, then keep what matches.
        let browsing = rest.trim().is_empty();
        let wide = if browsing { BROWSE_POOL } else { (limit * 6).max(200) };
        let mut hits = if browsing { self.recent(wide) } else { self.search_inner(&rest, qvec, wide) };
        if !browsing {
            self.attach_tags(&mut hits);
        }
        hits.retain(|h| tags.iter().all(|t| h.tags.split(' ').any(|x| x == t)) && filters.matches(h));
        hits.truncate(limit);
        hits
    }

    /// Fill `tags` on hits (one small query each; lists are short).
    fn attach_tags(&self, hits: &mut [Hit]) {
        let Ok(mut st) = self.conn.prepare("SELECT tags FROM items WHERE id = ?1") else { return };
        for h in hits.iter_mut() {
            let raw: String = st.query_row(params![h.id], |r| r.get(0)).unwrap_or_default();
            h.tags = raw.split(',').filter(|t| !t.is_empty()).collect::<Vec<_>>().join(" ");
        }
    }

    /// Set an item's tags from free text ("work, ideas" / "#work #ideas").
    pub fn set_tags(&mut self, id: i64, text: &str) -> rusqlite::Result<()> {
        let mut tags: Vec<String> = text.split(|c: char| c == ',' || c.is_whitespace()).map(|t| t.trim_start_matches('#').trim().to_lowercase()).filter(|t| !t.is_empty()).collect();
        tags.dedup();
        let stored = if tags.is_empty() { String::new() } else { format!(",{},", tags.join(",")) };
        self.conn.execute("UPDATE items SET tags = ?1 WHERE id = ?2", params![stored, id])?;
        Ok(())
    }

    /// An item's tags, space-separated without the '#'.
    pub fn tags_of(&self, id: i64) -> String {
        let raw: String = self.conn.query_row("SELECT tags FROM items WHERE id = ?1", params![id], |r| r.get(0)).unwrap_or_default();
        raw.split(',').filter(|t| !t.is_empty()).collect::<Vec<_>>().join(" ")
    }

    /// Semantic-only search (MCP `retrieve` with mode "semantic").
    pub fn semantic(&self, qvec: &[f32], limit: usize) -> Vec<Hit> {
        let mut hits: Vec<Hit> = self
            .search_semantic(qvec, limit)
            .into_iter()
            .map(|(mut h, _)| {
                h.via = "sem";
                h
            })
            .collect();
        self.attach_tags(&mut hits);
        hits
    }

    /// One item's row (title, kind, source, first 120 chars) with its tags.
    pub fn item(&self, id: i64) -> Option<Hit> {
        let mut h = self
            .conn
            .query_row("SELECT id, title, kind, source, substr(content, 1, 120), added_at FROM items WHERE id = ?1", params![id], |r| {
                Ok(Hit {
                    id: r.get(0)?,
                    title: r.get(1)?,
                    kind: r.get(2)?,
                    source: r.get(3)?,
                    snippet: r.get::<_, String>(4)?.replace(['\r', '\n'], " "),
                    via: "",
                    tags: String::new(),
                    added_at: r.get(5)?,
                })
            })
            .ok()?;
        h.tags = self.tags_of(id);
        Some(h)
    }

    pub fn set_cursor(&self, id: i64, row: i64, col: i64) {
        let _ = self.conn.execute("UPDATE items SET cursor = ?1 WHERE id = ?2", params![format!("{row},{col}"), id]);
    }

    pub fn cursor_of(&self, id: i64) -> Option<(i64, i64)> {
        let raw: String = self.conn.query_row("SELECT cursor FROM items WHERE id = ?1", params![id], |r| r.get(0)).ok()?;
        let (r, c) = raw.split_once(',')?;
        Some((r.parse().ok()?, c.parse().ok()?))
    }

    fn search_inner(&self, query: &str, qvec: Option<&[f32]>, limit: usize) -> Vec<Hit> {
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
                         ELSE substr(c.text, 1, 160) END, i.added_at
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
                            tags: String::new(),
                            added_at: r.get(4)?,
                        },
                        c,
                    ))
                })
                .ok()
            })
            .collect()
    }

    #[allow(dead_code)]
    /// The item's chunk nearest to `qvec` (for agents doing RAG over the vault).
    pub fn best_chunk(&self, item_id: i64, qvec: &[f32]) -> Option<String> {
        let best = self.index.iter().filter(|e| e.item_id == item_id).map(|e| (cosine(qvec, &e.vec), e.chunk_id)).max_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal))?;
        self.conn.query_row("SELECT text FROM chunks WHERE id = ?1 AND ord >= 0", params![best.1], |r| r.get(0)).ok()
    }

    /// Where to open an item: the original path while it exists, else Blackhole's copy.
    pub fn open_path(&self, id: i64) -> Option<String> {
        let (source, stored): (Option<String>, String) = self.conn.query_row("SELECT source, stored FROM items WHERE id = ?1", params![id], |r| Ok((r.get(0)?, r.get(1)?))).ok()?;
        match source {
            Some(s) if std::path::Path::new(&s).exists() => Some(s),
            _ if !stored.is_empty() && std::path::Path::new(&stored).exists() => Some(stored),
            other => other,
        }
    }

    pub fn set_stored(&self, id: i64, stored: &str) {
        let _ = self.conn.execute("UPDATE items SET stored = ?1 WHERE id = ?2", params![stored, id]);
    }

    pub fn id_by_hash(&self, hash: &str) -> Option<i64> {
        self.conn.query_row("SELECT id FROM items WHERE hash = ?1", params![hash], |r| r.get(0)).ok()
    }

    #[allow(dead_code)] // askeval
    pub fn title_of(&self, id: i64) -> Option<String> {
        self.conn.query_row("SELECT title FROM items WHERE id = ?1", params![id], |r| r.get(0)).ok()
    }

    /// Does any item contain this word?
    pub fn contains_term(&self, term: &str) -> bool {
        self.items_containing(term) > 0
    }

    /// Number of items whose text contains `term` (FTS5 token match).
    fn items_containing(&self, term: &str) -> i64 {
        self.conn.query_row("SELECT COUNT(*) FROM items_fts WHERE items_fts MATCH ?1", params![format!("\"{term}\"")], |r| r.get(0)).unwrap_or(0)
    }

    /// Highest cosine between the query vector and anything in the vault.
    pub fn best_cosine(&self, qvec: &[f32]) -> f32 {
        if qvec.len() != DIM {
            return 0.0;
        }
        self.index.iter().map(|e| cosine(qvec, &e.vec)).fold(0.0, f32::max)
    }

    /// Is this word in the vault, allowing for the one difference FTS5's unicode61
    /// tokenizer cannot see — the plural? "invoices totals" is about an invoice with
    /// a total, but neither word is a token of "invoice … total", and a two-word
    /// question refused on that basis is a plain bug. Both directions are covered:
    /// the query's singular forms, and a prefix match so a stored "resumes" answers
    /// a query for "resume".
    fn term_present(&self, term: &str) -> bool {
        if self.items_containing(term) > 0 {
            return true;
        }
        if stems(term).iter().any(|st| self.items_containing(st) > 0) {
            return true;
        }
        self.items_with_prefix(term) > 0
    }

    /// Number of items with a token starting with `term` (FTS5 prefix match).
    fn items_with_prefix(&self, term: &str) -> i64 {
        self.conn.query_row("SELECT COUNT(*) FROM items_fts WHERE items_fts MATCH ?1", params![format!("\"{term}\"*")], |r| r.get(0)).unwrap_or(0)
    }

    /// "Not in your vault": the query has ≥2 content terms, none of them (nor an
    /// obvious inflection of them) occurs in any item, and nothing matches strongly
    /// by meaning. Lexical on purpose — cosine thresholds and rank margins could not
    /// separate unanswerable questions (they score as high as real ones); this rule
    /// caught 2/2 with 0 false absents.
    pub fn absent(&self, query: &str, qvec: Option<&[f32]>) -> bool {
        let terms = content_terms(query);
        if terms.len() < ABSENT_MIN_TERMS {
            return false;
        }
        if terms.iter().any(|t| self.term_present(t)) {
            return false;
        }
        qvec.map(|v| self.best_cosine(v) < ABSENT_MAX_COSINE).unwrap_or(true)
    }

    /// Does any distinctive term of `text` occur in the vault? (Used so a model
    /// rewrite that lands on a literal word — "git" for "ssh checkout address" —
    /// can override the absent gate.)
    pub fn any_term_present(&self, text: &str) -> bool {
        content_terms(text).iter().any(|t| self.items_containing(t) > 0)
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
    /// `rerank::candidates()` of them, and the document with the highest logit
    /// becomes the top document. Documents it never saw keep their cheap order.
    /// (Rank-fusing the reranker with RRF instead measured a regression.)
    pub fn context_reranked(&self, query: &str, aux_query: &str, qvec: &[f32], budget_words: usize, rerank: Option<&dyn Fn(&str, &[String]) -> Option<Vec<f32>>>) -> Vec<(String, String)> {
        #[derive(Clone)]
        struct C {
            id: i64,
            item: i64,
            ord: i64,
            words: usize,
            score: f32,
            /// Contains one of the query's distinctive terms literally.
            literal: bool,
        }
        let mut all: Vec<(f32, i64)> = self.index.iter().map(|e| (cosine(qvec, &e.vec), e.chunk_id)).collect();
        all.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));

        // Distinctive query terms (names, numbers, jargon) that embeddings blur:
        // weight each by how rare it is across the vault, so "list", "with" or
        // "dates" don't drag random chunks up.
        let n_items = self.count().max(1) as f32;
        let own = content_terms(query);
        let mut terms: Vec<(String, f32)> = own
            .iter()
            .filter_map(|t| {
                let docs = self.items_containing(t);
                let frac = docs as f32 / n_items;
                // Present in more than a third of items: not distinctive.
                (frac <= 0.34).then(|| (t.clone(), 0.08 * (1.0 - frac)))
            })
            .collect();
        // Model-written rewrites of the question count half: useful when they land
        // on a literal word, not trusted enough to outvote the user's own words.
        for t in content_terms(aux_query) {
            if own.contains(&t) || terms.iter().any(|(x, _)| *x == t) {
                continue;
            }
            let frac = self.items_containing(&t) as f32 / n_items;
            if frac > 0.0 && frac <= 0.34 {
                terms.push((t, 0.04 * (1.0 - frac)));
            }
        }
        // Candidates: the 40 nearest by cosine, plus every chunk that literally
        // contains a distinctive term. A rare name in one chunk of a long document
        // ("raytheon" in a résumé) can sit far down the cosine list and would
        // otherwise never be seen by the boost or the reranker.
        let mut scored: Vec<(f32, i64)> = all.iter().take(40).copied().collect();
        if !terms.is_empty() {
            let mut literal: HashMap<i64, f32> = HashMap::new();
            if let Ok(mut st) = self.conn.prepare("SELECT id, text FROM chunks") {
                if let Ok(rows) = st.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))) {
                    for (id, text) in rows.flatten() {
                        let lower = text.to_lowercase();
                        if terms.iter().any(|(t, _)| lower.contains(t.as_str())) {
                            if let Some(&(c, _)) = all.iter().find(|&&(_, cid)| cid == id) {
                                literal.insert(id, c);
                            }
                        }
                    }
                }
            }
            for (id, c) in literal {
                if !scored.iter().any(|&(_, cid)| cid == id) {
                    scored.push((c, id));
                }
            }
        }
        let Ok(mut meta) = self.conn.prepare("SELECT item_id, ord, text FROM chunks WHERE id = ?1") else {
            return Vec::new();
        };
        // Terms rare enough (in at most two items) to act like proper nouns: a whole-word
        // hit on one of these earns the reranker bonus; "repo" inside "repositories" does not.
        let rare: Vec<&str> = own.iter().filter(|t| self.items_containing(t) <= 2 && self.items_containing(t) > 0).map(|t| t.as_str()).collect();
        let mut cands: Vec<C> = scored
            .iter()
            .filter_map(|&(c, id)| {
                meta.query_row(params![id], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?, r.get::<_, String>(2)?))).ok().map(|(item, ord, text)| {
                    let lower = text.to_lowercase();
                    let boost: f32 = terms.iter().filter(|(t, _)| lower.contains(t.as_str())).map(|(_, w)| *w).sum();
                    let literal = !rare.is_empty() && lower.split(|ch: char| !ch.is_alphanumeric()).any(|w| rare.contains(&w));
                    C { id, item, ord, words: text.split_whitespace().count(), score: c + boost, literal }
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
            // Chunks that literally contain a rare query word always get a hearing from the
            // reranker (up to 3), even when their cosine sits below the cut: "raytheon" in
            // one résumé chunk versus a form that is near everything.
            let k = crate::rerank::candidates();
            let mut head: Vec<&C> = cands.iter().take(k).collect();
            let literal: Vec<&C> = cands.iter().filter(|c| c.literal && !head.iter().any(|h| h.id == c.id)).take(3).collect();
            for l in literal {
                if head.len() >= k {
                    head.pop();
                }
                head.push(l);
            }
            let passages: Vec<String> = head
                .iter()
                .map(|c| {
                    if c.ord < 0 {
                        let (title, content): (String, String) =
                            self.conn.query_row("SELECT title, content FROM items WHERE id = ?1", params![c.item], |r| Ok((r.get(0)?, r.get(1)?))).unwrap_or_default();
                        format!("{title}: {}", content.split_whitespace().take(crate::rerank::DOC_UNIT_WORDS).collect::<Vec<_>>().join(" "))
                    } else {
                        meta.query_row(params![c.id], |r| r.get::<_, String>(2)).unwrap_or_default()
                    }
                })
                .collect();
            if let Some(logits) = rerank(query, &passages) {
                let mut best: HashMap<i64, f32> = HashMap::new();
                for (c, &l) in head.iter().zip(&logits) {
                    // The cross-encoder is weak on rare proper nouns ("raytheon" lost by 0.05
                    // to a form that is near everything); a literal hit gets a small bonus.
                    let l = if c.literal { l + 0.5 } else { l };
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
                "SELECT COUNT(*), COALESCE(SUM(length(text) - length(replace(replace(text, ' ', ''), char(10), '')) + 1), 0) FROM chunks WHERE item_id = ?1 AND ord >= 0",
                params![top.item],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap_or((0, 0));
        // Consecutive chunks share OVERLAP_WORDS, which the merge below removes again.
        let doc_words = (raw_words as usize).saturating_sub(crate::chunk::OVERLAP_WORDS * (n_chunks.max(1) as usize - 1));
        let whole = n_chunks > 0 && doc_words <= budget_words;
        if whole {
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
            // With the whole top document in, a stray one-line note from elsewhere is
            // more likely a decoy ("DevOps Engineer, Night Shift" next to a résumé).
            if whole && c.item != top.item && c.words < 15 {
                continue;
            }
            if !try_add(c.item, c.ord, &mut chosen, &mut used) {
                continue;
            }
            try_add(c.item, c.ord - 1, &mut chosen, &mut used);
            try_add(c.item, c.ord + 1, &mut chosen, &mut used);
            if used >= budget_words * 9 / 10 {
                break;
            }
        }

        // Emit the chosen document first, then the rest, each in document order, merging
        // runs of consecutive chunks into one block.
        let mut list: Vec<(i64, i64, String)> = chosen.into_values().collect();
        list.sort_by_key(|(item, ord, _)| (*item != top.item, *item, *ord));
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
                    block.1.push_str(crate::chunk::skip_words(&text, crate::chunk::OVERLAP_WORDS));
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
                    snippet(items_fts, 1, '\u{1}', '\u{2}', ' … ', 14), i.added_at
             FROM items_fts JOIN items i ON i.id = items_fts.rowid
             WHERE items_fts MATCH ?1
             ORDER BY bm25(items_fts, 3.0, 1.0)
             LIMIT ?2",
        ) {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        stmt.query_map(params![fts, limit as i64], |r| {
            Ok(Hit { id: r.get(0)?, title: r.get(1)?, kind: r.get(2)?, source: r.get(3)?, snippet: r.get(4)?, via: "kw", tags: String::new(), added_at: r.get(5)? })
        })
        .map(|rows| rows.filter_map(|r| r.ok()).collect())
        .unwrap_or_default()
    }

    pub fn recent(&self, limit: usize) -> Vec<Hit> {
        let mut hits = self.recent_inner(limit);
        self.attach_tags(&mut hits);
        hits
    }

    fn recent_inner(&self, limit: usize) -> Vec<Hit> {
        let mut stmt = match self.conn.prepare(
            "SELECT id, title, kind, source, substr(content, 1, 120), added_at
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
                tags: String::new(),
                added_at: r.get(5)?,
            })
        })
        .map(|rows| rows.filter_map(|r| r.ok()).collect())
        .unwrap_or_default()
    }

    /// The user's own notes (kind "note"), newest first, as list rows.
    pub fn notes(&self, limit: usize) -> Vec<Hit> {
        let mut hits = self.notes_inner(limit);
        self.attach_tags(&mut hits);
        hits
    }

    fn notes_inner(&self, limit: usize) -> Vec<Hit> {
        let Ok(mut stmt) = self.conn.prepare("SELECT id, title, kind, source, substr(content, 1, 160), added_at FROM items WHERE kind = 'note' ORDER BY added_at DESC, id DESC LIMIT ?1") else {
            return Vec::new();
        };
        stmt.query_map(params![limit as i64], |r| {
            Ok(Hit {
                id: r.get(0)?,
                title: r.get(1)?,
                kind: r.get(2)?,
                source: r.get(3)?,
                snippet: r.get::<_, String>(4)?.replace(['\r', '\n'], " "),
                via: "",
                tags: String::new(),
                added_at: r.get(5)?,
            })
        })
        .map(|rows| rows.filter_map(|r| r.ok()).collect())
        .unwrap_or_default()
    }

    /// A new, empty note; notes never dedupe against each other (unique hash).
    pub fn add_note(&self, now: i64) -> rusqlite::Result<Option<i64>> {
        let hash = format!("note:{now}:{}", std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.subsec_nanos()).unwrap_or(0));
        self.add("Untitled note", "note", None, "", &hash, now)
    }

    /// Rewrite a note's title and text; chunks are dropped so the caller re-embeds it.
    /// Rewrite a note's text; `title` applies unless the user named the note (`set_note_title`).
    pub fn update_note(&mut self, id: i64, title: &str, content: &str) -> rusqlite::Result<()> {
        self.conn
            .execute("UPDATE items SET title = CASE WHEN custom_title = 1 THEN title ELSE ?1 END, content = ?2, added_at = ?3 WHERE id = ?4", params![title, content, crate::util::now_secs(), id])?;
        self.conn.execute("DELETE FROM chunks WHERE item_id = ?1", params![id])?;
        self.conn.execute("INSERT INTO items_fts(items_fts) VALUES('rebuild')", [])?;
        self.index.retain(|e| e.item_id != id);
        Ok(())
    }

    /// Name a note explicitly (`:Name` in the editor); an empty name goes back to automatic titles.
    pub fn set_note_title(&mut self, id: i64, title: &str) -> rusqlite::Result<()> {
        let custom = !title.trim().is_empty();
        self.conn.execute("UPDATE items SET title = CASE WHEN ?1 THEN ?2 ELSE title END, custom_title = ?1 WHERE id = ?3", params![custom, title.trim(), id])?;
        self.conn.execute("INSERT INTO items_fts(items_fts) VALUES('rebuild')", [])?;
        Ok(())
    }

    pub fn content(&self, id: i64) -> Option<String> {
        self.conn.query_row("SELECT content FROM items WHERE id = ?1", params![id], |r| r.get(0)).optional().ok().flatten()
    }

    /// The item's text only while it is at most `max` bytes long — the size test runs in
    /// SQLite, so a multi-megabyte body is never copied out just to be thrown away.
    /// (`length()` on text counts characters, so the blob cast is what makes it bytes.)
    pub fn content_capped(&self, id: i64, max: usize) -> Option<String> {
        self.conn.query_row("SELECT content FROM items WHERE id = ?1 AND length(CAST(content AS BLOB)) <= ?2", params![id, max as i64], |r| r.get(0)).optional().ok().flatten()
    }

    // ---- undigested: things that could not be read (FEATURES.md §3.5) ----

    /// Remember a failed ingest. One row per path (a retry replaces the old error);
    /// pasted text (no path) always adds a row.
    pub fn record_failure(&self, path: &str, title: &str, error: &str, at: i64) {
        if !path.is_empty() {
            let _ = self.conn.execute("DELETE FROM undigested WHERE path = ?1", params![path]);
        }
        let _ = self.conn.execute("INSERT INTO undigested (path, title, error, at) VALUES (?1, ?2, ?3, ?4)", params![path, title, error, at]);
    }

    /// A path went down successfully: it is not undigested any more.
    pub fn clear_failure_path(&self, path: &str) {
        if !path.is_empty() {
            let _ = self.conn.execute("DELETE FROM undigested WHERE path = ?1", params![path]);
        }
    }

    pub fn dismiss_failure(&self, id: i64) {
        let _ = self.conn.execute("DELETE FROM undigested WHERE id = ?1", params![id]);
    }

    /// The undigested list, newest failure first.
    pub fn undigested(&self, limit: usize) -> Vec<Failure> {
        let Ok(mut st) = self.conn.prepare("SELECT id, path, title, error, at FROM undigested ORDER BY at DESC, id DESC LIMIT ?1") else {
            return Vec::new();
        };
        st.query_map(params![limit as i64], |r| Ok(Failure { id: r.get(0)?, path: r.get(1)?, title: r.get(2)?, error: r.get(3)?, at: r.get(4)? }))
            .map(|rows| rows.filter_map(|r| r.ok()).collect())
            .unwrap_or_default()
    }

    pub fn undigested_count(&self) -> i64 {
        self.conn.query_row("SELECT COUNT(*) FROM undigested", [], |r| r.get(0)).unwrap_or(0)
    }

    pub fn delete(&mut self, id: i64) -> rusqlite::Result<()> {
        self.conn.execute("DELETE FROM items WHERE id = ?1", params![id])?;
        self.index.retain(|e| e.item_id != id);
        Ok(())
    }
}

/// Plausible singulars of a word, for the absent gate's presence check:
/// "queries" → "query", "invoices" → "invoice", "boxes" → "box".
fn stems(t: &str) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(b) = t.strip_suffix("ies") {
        if b.len() >= 2 {
            out.push(format!("{b}y"));
        }
    }
    for suf in ["s", "es"] {
        if let Some(b) = t.strip_suffix(suf) {
            if b.len() >= 3 && !out.contains(&b.to_string()) {
                out.push(b.to_string());
            }
        }
    }
    out
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
    q.split_whitespace().map(|t| t.replace('"', "")).filter(|t| !t.is_empty()).collect()
}

fn to_fts_query(q: &str) -> Option<String> {
    let tokens = fts_tokens(q);
    if tokens.is_empty() {
        return None;
    }
    let n = tokens.len();
    Some(tokens.iter().enumerate().map(|(i, t)| if i + 1 == n { format!("\"{t}\"*") } else { format!("\"{t}\"") }).collect::<Vec<_>>().join(" OR "))
}

#[cfg(debug_assertions)]
impl Store {
    pub fn debug_semantic(&self, qvec: &[f32]) {
        let mut best: HashMap<i64, (f32, i64)> = HashMap::new();
        for e in &self.index {
            let c = cosine(qvec, &e.vec);
            let entry = best.entry(e.item_id).or_insert((c, e.chunk_id));
            if c > entry.0 {
                *entry = (c, e.chunk_id);
            }
        }
        let mut v: Vec<_> = best.into_iter().collect();
        v.sort_by(|a, b| b.1 .0.partial_cmp(&a.1 .0).unwrap());
        for (item, (c, ch)) in v {
            let (title, text): (String, String) =
                self.conn.query_row("SELECT i.title, substr(c.text,1,60) FROM items i JOIN chunks c ON c.id=?2 WHERE i.id=?1", params![item, ch], |r| Ok((r.get(0)?, r.get(1)?))).unwrap();
            println!("   {c:.3} {} — {}", title.chars().take(30).collect::<String>(), text.replace('\n', " "));
        }
    }
}

/// ("#work #ideas rest of query") → (["work","ideas"], "rest of query").
pub fn split_tags(query: &str) -> (Vec<String>, String) {
    let mut tags = Vec::new();
    let mut rest = Vec::new();
    for w in query.split_whitespace() {
        match w.strip_prefix('#').filter(|t| !t.is_empty()) {
            Some(t) => tags.push(t.to_lowercase()),
            None => rest.push(w),
        }
    }
    (tags, rest.join(" "))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Anything in here may log; keep that (and any other stray write) out of the
    /// user's real vault directory.
    fn isolate() {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| {
            let dir = std::env::temp_dir().join(format!("blackhole-unit-{}", std::process::id()));
            let _ = std::fs::create_dir_all(&dir);
            std::env::set_var("BLACKHOLE_DATA_DIR", &dir);
        });
    }

    fn store() -> Store {
        isolate();
        Store::open(Path::new(":memory:")).expect("in-memory vault")
    }

    fn add(s: &Store, title: &str, content: &str) -> i64 {
        s.add(title, "note", None, content, &format!("hash-{title}-{}", content.len()), 1_700_000_000).expect("insert").expect("new item")
    }

    /// A unit vector along axis `i` — cosine 1 with itself, 0 with any other.
    fn axis(i: usize) -> Vec<f32> {
        let mut v = vec![0.0; DIM];
        v[i] = 1.0;
        v
    }

    /// A unit vector whose cosine with `axis(i)` is exactly `c`.
    fn near(i: usize, c: f32) -> Vec<f32> {
        let mut v = vec![0.0; DIM];
        v[i] = c;
        v[DIM - 1] = (1.0 - c * c).sqrt();
        v
    }

    // ---- tags ----------------------------------------------------------

    #[test]
    fn split_tags_separates_hash_words_from_the_query() {
        let (tags, rest) = split_tags("#work #Ideas rest of query");
        assert_eq!(tags, vec!["work", "ideas"]);
        assert_eq!(rest, "rest of query");
    }

    #[test]
    fn split_tags_on_a_plain_query_changes_nothing() {
        let (tags, rest) = split_tags("plain query");
        assert!(tags.is_empty());
        assert_eq!(rest, "plain query");
    }

    #[test]
    fn a_bare_hash_is_not_a_tag() {
        let (tags, rest) = split_tags("# not a tag");
        assert!(tags.is_empty());
        assert_eq!(rest, "# not a tag");
    }

    #[test]
    fn a_tag_only_query_leaves_an_empty_rest() {
        let (tags, rest) = split_tags("#work");
        assert_eq!(tags, vec!["work"]);
        assert_eq!(rest, "");
    }

    #[test]
    fn tags_round_trip_through_free_text() {
        let mut s = store();
        let id = add(&s, "Note", "body text");
        s.set_tags(id, "#Work, ideas").unwrap();
        assert_eq!(s.tags_of(id), "work ideas");
        s.set_tags(id, "").unwrap();
        assert_eq!(s.tags_of(id), "");
    }

    #[test]
    fn a_tag_filters_the_search() {
        let mut s = store();
        let a = add(&s, "Alpha", "shared word vessel");
        let _b = add(&s, "Beta", "shared word vessel too");
        s.set_tags(a, "work").unwrap();
        let hits = s.search("#work vessel", None, 10);
        assert_eq!(hits.iter().map(|h| h.id).collect::<Vec<_>>(), vec![a]);
    }

    // ---- fusion --------------------------------------------------------

    #[test]
    fn term_coverage_is_the_fraction_of_query_words_an_item_has() {
        let s = store();
        let both = add(&s, "Manifest", "the tranquil ace sailed from kobe");
        let one = add(&s, "Recipe", "kobe beef with lime");
        let cov = s.term_coverage("tranquil kobe", [both, one].into_iter());
        assert!((cov[&both] - 1.0).abs() < 1e-6, "{cov:?}");
        assert!((cov[&one] - 0.5).abs() < 1e-6, "{cov:?}");
    }

    #[test]
    fn an_item_matching_every_query_word_outranks_a_one_word_coincidence() {
        let s = store();
        let both = add(&s, "Manifest", "the tranquil ace sailed from kobe to oakland");
        let one = add(&s, "Recipe", "kobe beef with lime and salt");
        let hits = s.search("tranquil kobe", None, 10);
        assert_eq!(hits[0].id, both);
        assert!(hits.iter().any(|h| h.id == one), "the weak match still shows up");
        assert_eq!(hits[0].via, "kw");
    }

    #[test]
    fn a_keyword_and_semantic_hit_on_the_same_item_fuse_into_both() {
        let mut s = store();
        let id = add(&s, "Manifest", "the tranquil ace sailed from kobe");
        s.add_chunks(id, &[("the tranquil ace sailed from kobe".to_string(), axis(3))], None).unwrap();
        let hits = s.search("tranquil ace", Some(&axis(3)), 10);
        assert_eq!(hits[0].id, id);
        assert_eq!(hits[0].via, "both");
    }

    #[test]
    fn an_item_that_only_matches_by_meaning_still_shows_up() {
        let mut s = store();
        let id = add(&s, "Manifest", "the tranquil ace sailed from kobe");
        s.add_chunks(id, &[("the tranquil ace sailed from kobe".to_string(), axis(3))], None).unwrap();
        let hits = s.search("completely different words", Some(&axis(3)), 10);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].via, "sem");
    }

    #[test]
    fn a_weak_cosine_is_not_a_hit_at_all() {
        let mut s = store();
        let id = add(&s, "Manifest", "vessel voyage");
        s.add_chunks(id, &[("vessel voyage".to_string(), near(3, 0.30))], None).unwrap();
        assert!(s.search("nothing in common", Some(&axis(3)), 10).is_empty());
        assert!(s.semantic(&axis(3), 10).is_empty());
    }

    #[test]
    fn semantic_results_stay_within_a_window_of_the_best_one() {
        let mut s = store();
        let strong = add(&s, "Strong", "aaa");
        let weak = add(&s, "Weak", "bbb");
        s.add_chunks(strong, &[("aaa".to_string(), near(3, 0.95))], None).unwrap();
        s.add_chunks(weak, &[("bbb".to_string(), near(3, 0.50))], None).unwrap();
        let hits = s.semantic(&axis(3), 10);
        assert_eq!(hits.iter().map(|h| h.id).collect::<Vec<_>>(), vec![strong], "the tail is cut off");
    }

    #[test]
    fn an_empty_query_falls_back_to_the_most_recent_items() {
        let s = store();
        add(&s, "One", "first");
        add(&s, "Two", "second");
        let hits = s.search("   ", None, 10);
        assert_eq!(hits.len(), 2);
    }

    #[test]
    fn the_limit_is_respected() {
        let s = store();
        for i in 0..8 {
            add(&s, &format!("Item {i}"), &format!("vessel voyage number {i}"));
        }
        assert_eq!(s.search("vessel", None, 3).len(), 3);
    }

    #[test]
    fn quotes_in_a_query_cannot_break_the_fts_expression() {
        let s = store();
        add(&s, "Manifest", "the tranquil ace");
        let hits = s.search("\"tranquil\" ace", None, 5);
        assert_eq!(hits.len(), 1);
        assert_eq!(to_fts_query("a b").unwrap(), "\"a\" OR \"b\"*");
        assert!(to_fts_query("   ").is_none());
        assert_eq!(fts_tokens("say \"what\""), vec!["say", "what"]);
    }

    #[test]
    fn deleting_an_item_takes_it_out_of_both_indexes() {
        let mut s = store();
        let id = add(&s, "Manifest", "the tranquil ace");
        s.add_chunks(id, &[("the tranquil ace".to_string(), axis(3))], None).unwrap();
        assert_eq!(s.count(), 1);
        s.delete(id).unwrap();
        assert_eq!(s.count(), 0);
        assert!(s.search("tranquil", Some(&axis(3)), 5).is_empty());
        assert_eq!(s.best_cosine(&axis(3)), 0.0);
    }

    #[test]
    fn the_same_thing_twice_is_swallowed_once() {
        let s = store();
        let first = s.add("Note", "note", None, "body", "same-hash", 1).unwrap();
        let again = s.add("Note", "note", None, "body", "same-hash", 2).unwrap();
        assert!(first.is_some());
        assert_eq!(again, None);
        assert_eq!(s.count(), 1);
    }

    #[test]
    fn the_catalog_lists_titles_with_their_opening_words() {
        let s = store();
        add(&s, "Manifest", "the tranquil ace sailed from kobe");
        let cat = s.catalog(10);
        assert!(cat.starts_with("- Manifest [note]: the tranquil ace"), "{cat:?}");
    }

    // ---- the absent gate ------------------------------------------------

    /// A small vault whose words are known, for the one- and two-word cases below.
    fn absent_vault() -> Store {
        let s = store();
        add(&s, "Invoice 4471", "Invoice total paid 1240.00 to Zephyr Logistics on 3 March. Payment by wire.");
        add(&s, "Resume.pdf", "Principal MLOps Engineer at Maxar Technologies. Storage Solutions Architect at Raytheon.");
        add(&s, "Handbook", "Vacation policy: twenty days a year. Queries go to the office manager.");
        s
    }

    #[test]
    fn one_word_queries_are_never_refused() {
        let s = absent_vault();
        // Too little evidence either way: one word that happens to be missing is not
        // a reason to tell the user their vault has nothing.
        for q in ["raytheon", "quasar", "invoices", "wombat"] {
            assert!(!s.absent(q, None), "{q:?} was refused on one word");
        }
    }

    #[test]
    fn two_word_queries_whose_words_are_in_the_vault_are_not_refused() {
        let s = absent_vault();
        for q in ["zephyr logistics", "vacation policy", "maxar technologies", "payment wire"] {
            assert!(!s.absent(q, None), "{q:?} was refused although its words are there");
        }
    }

    #[test]
    fn a_plural_is_not_a_missing_word() {
        let s = absent_vault();
        // "invoice"/"invoices", "total"/"totals", "query"/"queries": FTS5's tokenizer
        // does not stem, and refusing these was a plain bug.
        for q in ["invoices totals", "vacations policies", "engineers architects"] {
            assert!(!s.absent(q, None), "{q:?} was refused over a plural");
        }
    }

    #[test]
    fn a_singular_finds_a_stored_plural() {
        let s = absent_vault();
        assert!(!s.absent("query manager", None), "a stored \"queries\" answers \"query\"");
    }

    #[test]
    fn two_words_that_really_are_missing_are_refused() {
        let s = absent_vault();
        for q in ["quantum teleportation", "gravitational lensing", "quokka husbandry"] {
            assert!(s.absent(q, None), "{q:?} should have been refused");
        }
    }

    #[test]
    fn a_very_strong_semantic_match_vetoes_the_refusal() {
        let mut s = absent_vault();
        let id = add(&s, "Shipment", "carrier and sailing details");
        // Neither word is in the vault, but the meaning is right there.
        s.add_chunks(id, &[("carrier and sailing details".to_string(), near(7, 0.95))], None).unwrap();
        assert!(!s.absent("vessel voyage", Some(&axis(7))), "semantic evidence must veto a refusal");
        // …while a middling cosine does not: measured, that band is pure noise.
        assert!(s.absent("vessel voyage", Some(&near(50, 0.65))));
    }

    #[test]
    fn contains_term_is_exact_and_covers_the_title() {
        let s = absent_vault();
        assert!(s.contains_term("raytheon"));
        assert!(s.contains_term("handbook"), "titles are indexed too");
        assert!(!s.contains_term("raytheons"));
        assert!(s.any_term_present("something about raytheon"));
        assert!(!s.any_term_present("nothing familiar herein"));
    }

    #[test]
    fn best_cosine_is_the_nearest_thing_in_the_vault() {
        let mut s = store();
        let id = add(&s, "Item", "body");
        s.add_chunks(id, &[("body".to_string(), near(5, 0.80))], None).unwrap();
        assert!((s.best_cosine(&axis(5)) - 0.80).abs() < 1e-5);
        assert_eq!(s.best_cosine(&[0.0, 1.0]), 0.0, "a wrong-width vector is not a match");
    }

    // ---- context ---------------------------------------------------------

    #[test]
    fn context_puts_the_top_document_first_and_stays_inside_the_budget() {
        let mut s = store();
        let near_doc = add(&s, "Manifest", "the tranquil ace sailed from kobe to oakland");
        let far_doc = add(&s, "Recipe", "kobe beef with lime and salt");
        s.add_chunks(near_doc, &[("the tranquil ace sailed from kobe to oakland".to_string(), near(9, 0.90))], None).unwrap();
        s.add_chunks(far_doc, &[("kobe beef with lime and salt".to_string(), near(9, 0.50))], None).unwrap();
        let out = s.context_reranked("tranquil ace", "", &axis(9), 100, None);
        assert_eq!(out[0].0, "Manifest");
        assert!(out.iter().map(|(_, t)| t.split_whitespace().count()).sum::<usize>() <= 100);
    }

    #[test]
    fn the_reranker_can_overrule_the_cheap_pipeline_on_the_top_document() {
        let mut s = store();
        let cheap_winner = add(&s, "Manifest", "the tranquil ace sailed from kobe to oakland");
        let rerank_winner = add(&s, "Recipe", "kobe beef with lime and salt");
        s.add_chunks(cheap_winner, &[("the tranquil ace sailed from kobe to oakland".to_string(), near(9, 0.90))], None).unwrap();
        s.add_chunks(rerank_winner, &[("kobe beef with lime and salt".to_string(), near(9, 0.55))], None).unwrap();
        // Score by hand: whatever mentions beef wins.
        let hook = |_q: &str, ps: &[String]| -> Option<Vec<f32>> { Some(ps.iter().map(|p| if p.contains("beef") { 9.0 } else { 0.0 }).collect()) };
        let out = s.context_reranked("tranquil ace", "", &axis(9), 100, Some(&hook));
        assert_eq!(out[0].0, "Recipe");
    }

    #[test]
    fn context_of_an_empty_vault_is_empty() {
        let s = store();
        assert!(s.context_reranked("anything", "", &axis(1), 100, None).is_empty());
    }

    #[test]
    fn stems_cover_the_common_plurals() {
        assert!(stems("invoices").contains(&"invoice".to_string()), "{:?}", stems("invoices"));
        assert!(stems("queries").contains(&"query".to_string()), "{:?}", stems("queries"));
        assert!(stems("boxes").contains(&"box".to_string()), "{:?}", stems("boxes"));
        assert!(stems("totals").contains(&"total".to_string()), "{:?}", stems("totals"));
        assert!(stems("ace").is_empty(), "too short to strip");
    }
}
