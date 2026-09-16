//! Turns dropped things into text and feeds the vault on a background thread.

use crate::chunk::chunk;
use crate::embed::Embedder;
use crate::store::Store;
use crate::util::{now_secs, truncate};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};

/// What can fall in.
pub enum Input {
    Text(String),
    Files(Vec<PathBuf>),
}

pub struct Extracted {
    pub title: String,
    pub kind: &'static str,
    pub source: Option<String>,
    pub content: String,
}

/// Outcome of one batch, reported back to the dot.
pub struct Report {
    pub added: usize,
    pub duplicates: usize,
    pub failed: usize,
    /// One line per failure, e.g. "scan.pdf: could not read PDF".
    pub errors: Vec<String>,
}

const MAX_CONTENT: usize = 4 * 1024 * 1024;

const TEXT_EXTS: &[&str] = &[
    "txt", "md", "markdown", "rst", "csv", "tsv", "json", "yaml", "yml", "toml", "ini", "cfg",
    "conf", "log", "xml", "html", "htm", "css", "js", "ts", "jsx", "tsx", "rs", "py", "c", "h",
    "cpp", "hpp", "cs", "java", "kt", "go", "rb", "php", "sh", "ps1", "bat", "cmd", "sql", "lua",
    "swift", "m", "tex", "bib", "org", "srt", "vtt",
];

pub fn extract_text(text: &str) -> Extracted {
    let first = text.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("Text");
    Extracted {
        title: truncate(first, 80).to_string(),
        kind: "text",
        source: None,
        content: truncate(text, MAX_CONTENT).to_string(),
    }
}

pub fn extract_file(path: &Path) -> Result<Extracted, String> {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string());
    let ext = path
        .extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    let source = Some(path.display().to_string());

    if path.is_dir() {
        return Err(format!("{name}: folders are not supported yet"));
    }

    let (kind, content): (&'static str, String) = if ext == "pdf" {
        let bytes = std::fs::read(path).map_err(|e| format!("{name}: {e}"))?;
        // Layout-aware first (forms, tables); plain stream order if that fails.
        let text = match crate::pdf_layout::extract(&bytes) {
            Ok(t) if !t.trim().is_empty() => t,
            _ => pdf_extract::extract_text_from_mem(&bytes).map_err(|e| format!("{name}: could not read PDF ({e})"))?,
        };
        ("pdf", text)
    } else if TEXT_EXTS.contains(&ext.as_str()) {
        let bytes = std::fs::read(path).map_err(|e| format!("{name}: {e}"))?;
        ("file", String::from_utf8_lossy(&bytes).into_owned())
    } else if matches!(ext.as_str(), "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp") {
        // OCR / captioning lands with the model work; for now images are findable by name.
        ("image", String::new())
    } else {
        // Unknown binary: keep it findable by name and metadata only.
        let size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
        ("file", format!("{name} ({size} bytes)"))
    };

    let mut content = truncate(&content, MAX_CONTENT).to_string();
    if content.trim().is_empty() {
        content = name.clone();
    }
    Ok(Extracted { title: name, kind, source, content })
}

pub fn hash_of(e: &Extracted) -> String {
    let mut h = Sha256::new();
    h.update(e.kind.as_bytes());
    h.update(b"\0");
    if let Some(s) = &e.source {
        h.update(s.as_bytes());
    }
    h.update(b"\0");
    h.update(e.content.as_bytes());
    format!("{:x}", h.finalize())
}

/// Chunk + embed one item's text and store the vectors. The store lock is
/// only held for the final write so searches stay responsive meanwhile.
/// Each chunk is embedded with the item title in front so a fragment of a
/// résumé still "knows" it is from a résumé.
pub fn embed_item(store: &Mutex<Store>, embedder: &Embedder, item_id: i64, title: &str, content: &str) {
    let chunks: Vec<(String, Vec<f32>)> = chunk(content)
        .into_iter()
        .filter_map(|c| embedder.embed(&format!("{title}\n{c}")).ok().map(|v| (c, v)))
        .collect();
    // Document unit: the title on its own (see Store::add_chunks).
    let title_unit = embedder.embed(title).ok().map(|v| (title.to_string(), v));
    let _ = store.lock().unwrap().add_chunks(item_id, &chunks, title_unit);
}

/// Worker loop. `notify` is called after each batch with the outcome.
pub fn run(rx: Receiver<Input>, store: Arc<Mutex<Store>>, embedder: Arc<Embedder>, notify: impl Fn(Report)) {
    // Stored PDF text predates the current extractor: read the files again where they
    // still exist (missing ones keep their old text and just re-chunk).
    let stale = store.lock().unwrap().stale_text;
    if stale {
        let pdfs = store.lock().unwrap().items_with_source("pdf");
        let mut redone = 0;
        for (id, source) in pdfs {
            let path = Path::new(&source);
            if let Ok(e) = extract_file(path) {
                if store.lock().unwrap().update_content(id, &e.content).is_ok() {
                    redone += 1;
                }
            }
        }
        crate::util::log(&format!("text pipeline changed: re-extracted {redone} PDFs, re-chunking everything"));
    }
    // Items from before embeddings existed (or interrupted runs) get vectors now.
    let backlog = store.lock().unwrap().unembedded();
    for (id, title, content) in backlog {
        embed_item(&store, &embedder, id, &title, &content);
    }

    while let Ok(input) = rx.recv() {
        let mut report = Report { added: 0, duplicates: 0, failed: 0, errors: Vec::new() };
        let extracted: Vec<Result<Extracted, String>> = match input {
            Input::Text(t) => vec![Ok(extract_text(&t))],
            Input::Files(paths) => paths.iter().map(|p| extract_file(p)).collect(),
        };
        for e in extracted {
            match e {
                Ok(e) => {
                    let hash = hash_of(&e);
                    let added = store.lock().unwrap().add(&e.title, e.kind, e.source.as_deref(), &e.content, &hash, now_secs());
                    match added {
                        Ok(Some(id)) => {
                            report.added += 1;
                            embed_item(&store, &embedder, id, &e.title, &e.content);
                        }
                        Ok(None) => report.duplicates += 1,
                        Err(err) => {
                            report.failed += 1;
                            report.errors.push(format!("{}: {err}", e.title));
                        }
                    }
                }
                Err(msg) => {
                    report.failed += 1;
                    report.errors.push(msg);
                }
            }
        }
        notify(report);
    }
}
