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
    /// sha256 of the file bytes for images/PDFs: identity independent of what OCR read.
    pub bytes_hash: Option<String>,
    /// Blackhole's own copy of the file (images/PDFs), so it can always be opened.
    pub stored: Option<String>,
}

/// Outcome of one batch, reported back to the dot.
pub struct Report {
    pub added: usize,
    /// Same file again, but its text came out differently (better OCR, edited PDF): rewritten.
    pub updated: usize,
    pub duplicates: usize,
    pub failed: usize,
    /// One line per failure, e.g. "scan.pdf: could not read PDF".
    pub errors: Vec<String>,
}

const MAX_CONTENT: usize = 4 * 1024 * 1024;

const TEXT_EXTS: &[&str] = &[
    "txt", "md", "markdown", "rst", "csv", "tsv", "json", "yaml", "yml", "toml", "ini", "cfg",
    "conf", "log", "xml", "css", "js", "ts", "jsx", "tsx", "rs", "py", "c", "h",
    "cpp", "hpp", "cs", "java", "kt", "go", "rb", "php", "sh", "ps1", "bat", "cmd", "sql", "lua",
    "swift", "m", "tex", "bib", "org", "srt", "vtt",
];

pub fn extract_text(text: &str) -> Extracted {
    // Pasted or dropped a bare URL: the page behind it is what was meant, not the
    // seven words of the link. A fetch that fails keeps the text.
    if let Some(url) = crate::web::bare_url(text) {
        match extract_url(&url) {
            Ok(e) => return e,
            Err(err) => crate::util::log(&format!("web: {err}")),
        }
    }
    let first = text.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("Text");
    Extracted {
        title: truncate(first, 80).to_string(),
        kind: "text",
        source: None,
        content: truncate(text, MAX_CONTENT).to_string(),
        bytes_hash: None,
        stored: None,
    }
}

/// Fetch a page and turn it into an item: the URL is its identity, so the same
/// page swallowed again refreshes the text it had instead of piling up copies.
pub fn extract_url(url: &str) -> Result<Extracted, String> {
    let page = crate::web::fetch_page(url)?;
    let content = format!("{}\n\n{}", page.url, page.text);
    Ok(Extracted {
        title: truncate(page.title.trim(), 120).to_string(),
        kind: "web",
        source: Some(page.url.clone()),
        content: truncate(&content, MAX_CONTENT).to_string(),
        bytes_hash: Some(sha_hex(page.url.as_bytes())),
        stored: None,
    })
}

fn sha_hex(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    format!("{:x}", h.finalize())
}

/// Keep our own copy of an image/PDF under `<data dir>\files\<hash>.<ext>` unless the
/// file already lives in the data dir (screenshots, pasted images). Returns the copy's path.
fn stored_copy(path: &Path, bytes: &[u8], hash: &str, ext: &str) -> Option<String> {
    let data = crate::config::data_dir();
    if path.starts_with(&data) {
        return Some(path.display().to_string());
    }
    let dir = data.join("files");
    std::fs::create_dir_all(&dir).ok()?;
    let dest = dir.join(format!("{}.{ext}", &hash[..24]));
    if !dest.exists() {
        std::fs::write(&dest, bytes).ok()?;
    }
    Some(dest.display().to_string())
}

/// A PNG (e.g. a pasted clipboard bitmap) written into the vault's files folder.
pub fn store_png(rgb: &[u8], w: u32, h: u32, stem: &str) -> Result<PathBuf, String> {
    let dir = crate::config::data_dir().join("files");
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let mut path = dir.join(format!("{stem}.png"));
    let mut n = 1;
    while path.exists() {
        n += 1;
        path = dir.join(format!("{stem} ({n}).png"));
    }
    let file = std::fs::File::create(&path).map_err(|e| e.to_string())?;
    let mut enc = png::Encoder::new(std::io::BufWriter::new(file), w, h);
    enc.set_color(png::ColorType::Rgb);
    enc.set_depth(png::BitDepth::Eight);
    let mut writer = enc.write_header().map_err(|e| e.to_string())?;
    writer.write_image_data(rgb).map_err(|e| e.to_string())?;
    Ok(path)
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
    if !path.exists() {
        return Err(format!("{name}: no such file ({})", path.display()));
    }

    let mut bytes_hash = None;
    let mut stored = None;
    let (kind, content): (&'static str, String) = if ext == "pdf" {
        let bytes = std::fs::read(path).map_err(|e| format!("{name}: {e}"))?;
        let hash = sha_hex(&bytes);
        stored = stored_copy(path, &bytes, &hash, "pdf");
        bytes_hash = Some(hash);
        // Layout-aware first (forms, tables); plain stream order if that fails.
        let mut text = match crate::pdf_layout::extract(&bytes) {
            Ok(t) if !t.trim().is_empty() => t,
            _ => pdf_extract::extract_text_from_mem(&bytes).unwrap_or_default(),
        };
        // A scan has (almost) no glyphs: read the page images instead.
        if text.split_whitespace().count() < 20 {
            if let Some(ocr) = crate::ocr::read_pdf_images(&bytes) {
                text = if text.trim().is_empty() { ocr } else { format!("{text}\n\n{ocr}") };
            }
        }
        if text.trim().is_empty() {
            return Err(format!("{name}: no readable text (vector text, JPEG or raw scans are supported)"));
        }
        ("pdf", text)
    } else if crate::office::is_office(&ext) {
        // docx / xlsx / pptx: zip containers of XML (see office.rs). Like a PDF, the
        // bytes are the identity and a copy is kept so the file can always be opened.
        let bytes = std::fs::read(path).map_err(|e| format!("{name}: {e}"))?;
        let hash = sha_hex(&bytes);
        stored = stored_copy(path, &bytes, &hash, &ext);
        bytes_hash = Some(hash);
        let text = crate::office::extract(&ext, &bytes).map_err(|e| format!("{name}: {e}"))?;
        (crate::office::kind_of(&ext), text)
    } else if matches!(ext.as_str(), "url" | "website" | "webloc") {
        // A saved shortcut: what the user kept is the page, so fetch it.
        let url = crate::web::url_from_shortcut(path).ok_or_else(|| format!("{name}: no URL in this shortcut"))?;
        return extract_url(&url).map_err(|e| format!("{name}: {e}"));
    } else if matches!(ext.as_str(), "html" | "htm" | "xhtml") {
        // A saved page: read it the way a browser would show it, without going online.
        let bytes = std::fs::read(path).map_err(|e| format!("{name}: {e}"))?;
        let hash = sha_hex(&bytes);
        let html = String::from_utf8_lossy(&bytes);
        let (title, text) = crate::web::readable(&html);
        let title = title.filter(|t| !t.trim().is_empty()).unwrap_or_else(|| name.clone());
        let content = if text.trim().is_empty() { html.into_owned() } else { text };
        return Ok(Extracted {
            title: truncate(&title, 120).to_string(),
            kind: "web",
            source,
            content: truncate(&content, MAX_CONTENT).to_string(),
            bytes_hash: Some(hash),
            stored: None,
        });
    } else if TEXT_EXTS.contains(&ext.as_str()) {
        let bytes = std::fs::read(path).map_err(|e| format!("{name}: {e}"))?;
        ("file", String::from_utf8_lossy(&bytes).into_owned())
    } else if matches!(ext.as_str(), "png" | "jpg" | "jpeg") {
        // OCR: the picture's words become its text (PP-OCR on the GPU, ~0.5 s). The file's
        // bytes are the item's identity, so the same picture dropped again is recognised
        // even if the OCR reads it differently — and then its text is refreshed.
        let bytes = std::fs::read(path).map_err(|e| format!("{name}: {e}"))?;
        let hash = sha_hex(&bytes);
        let stored = stored_copy(path, &bytes, &hash, if ext == "png" { "png" } else { "jpg" });
        let title = capture_title(path).unwrap_or_else(|| name.clone());
        let text = crate::ocr::read_file(path).unwrap_or_default();
        let content = if text.trim().is_empty() { title.clone() } else { format!("{title}\n\n{text}") };
        return Ok(Extracted { content: truncate(&content, MAX_CONTENT).to_string(), title, kind: "image", source, bytes_hash: Some(hash), stored });
    } else if matches!(ext.as_str(), "gif" | "webp" | "bmp") {
        // OCR / captioning lands with the model work; for now images are findable by name.
        if let Some(title) = capture_title(path) {
            // Our own screenshot (see screenshot.rs): "Screenshot 2026-09-16 14-03-22 412×188".
            // The title doubles as the content until OCR makes the pixels searchable.
            return Ok(Extracted { content: title.clone(), title, kind: "image", source, bytes_hash: None, stored: None });
        }
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
    Ok(Extracted { title: name, kind, source, content, bytes_hash, stored })
}

/// Title for a PNG the screenshot tool wrote under `<data dir>\captures`; None for any other file.
fn capture_title(path: &Path) -> Option<String> {
    let dir = crate::config::data_dir().join("captures");
    if path.parent() != Some(dir.as_path()) {
        return None;
    }
    let stem = path.file_stem()?.to_string_lossy();
    // Width and height straight from the IHDR chunk (bytes 16..24, big-endian).
    let mut head = [0u8; 24];
    std::io::Read::read_exact(&mut std::fs::File::open(path).ok()?, &mut head).ok()?;
    if &head[..8] != b"\x89PNG\r\n\x1a\n" {
        return None;
    }
    let w = u32::from_be_bytes(head[16..20].try_into().ok()?);
    let h = u32::from_be_bytes(head[20..24].try_into().ok()?);
    Some(format!("Screenshot {stem} {w}\u{d7}{h}"))
}

pub fn hash_of(e: &Extracted) -> String {
    let mut h = Sha256::new();
    h.update(e.kind.as_bytes());
    h.update(b"\0");
    if let Some(b) = &e.bytes_hash {
        // Files with bytes: the bytes are the identity, not the path or the extracted text.
        h.update(b.as_bytes());
        return format!("{:x}", h.finalize());
    }
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

/// Name to show for a failed ingest: the file name, or the first words of the message.
pub fn failure_title(path: &str, msg: &str) -> String {
    let name = Path::new(path).file_name().map(|n| n.to_string_lossy().into_owned());
    name.unwrap_or_else(|| msg.split(':').next().unwrap_or("dropped text").trim().to_string())
}

/// Extractors prefix their errors with the file name; the list shows that separately.
pub fn strip_title<'a>(msg: &'a str, title: &str) -> &'a str {
    msg.strip_prefix(title).map(|r| r.trim_start_matches(':').trim()).unwrap_or(msg)
}

/// Worker loop. `notify` is called after each batch with the outcome.
pub fn run(rx: Receiver<Input>, store: Arc<Mutex<Store>>, embedder: Arc<Embedder>, notify: impl Fn(Report)) {
    // Stored PDF text predates the current extractor: read the files again where they
    // still exist (missing ones keep their old text and just re-chunk).
    let stale = store.lock().unwrap().stale_text;
    if stale {
        let mut pdfs = store.lock().unwrap().items_with_source("pdf");
        pdfs.extend(store.lock().unwrap().items_with_source("image"));
        let mut redone = 0;
        for (id, source) in pdfs {
            let path = Path::new(&source);
            if let Ok(e) = extract_file(path) {
                if store.lock().unwrap().update_content(id, &e.content).is_ok() {
                    if let Some(s) = &e.stored {
                        store.lock().unwrap().set_stored(id, s);
                    }
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
        let mut report = Report { added: 0, updated: 0, duplicates: 0, failed: 0, errors: Vec::new() };
        // Each result keeps the path it came from: a failure is remembered in the vault's
        // undigested list (FEATURES.md §3.5) so the panel can show, retry or dismiss it.
        let extracted: Vec<(String, Result<Extracted, String>)> = match input {
            Input::Text(t) => vec![(String::new(), Ok(extract_text(&t)))],
            Input::Files(paths) => paths.iter().map(|p| (p.display().to_string(), extract_file(p))).collect(),
        };
        for (path, e) in extracted {
            match e {
                Ok(e) => {
                    store.lock().unwrap().clear_failure_path(&path);
                    let hash = hash_of(&e);
                    let added = store.lock().unwrap().add_stored(&e.title, e.kind, e.source.as_deref(), &e.content, &hash, now_secs(), e.stored.as_deref());
                    match added {
                        Ok(Some(id)) => {
                            report.added += 1;
                            embed_item(&store, &embedder, id, &e.title, &e.content);
                        }
                        Ok(None) => {
                            // Seen before. The extraction still ran (OCR may have improved, a PDF
                            // may have been edited in place): rewrite the text if it changed.
                            let existing = store.lock().unwrap().id_by_hash(&hash);
                            let changed = existing.map(|id| store.lock().unwrap().content(id).as_deref() != Some(e.content.as_str())).unwrap_or(false);
                            match existing {
                                Some(id) if changed => {
                                    if store.lock().unwrap().update_note(id, &e.title, &e.content).is_ok() {
                                        embed_item(&store, &embedder, id, &e.title, &e.content);
                                        report.updated += 1;
                                    }
                                }
                                _ => report.duplicates += 1,
                            }
                        }
                        Err(err) => {
                            report.failed += 1;
                            report.errors.push(format!("{}: {err}", e.title));
                            store.lock().unwrap().record_failure(&path, &e.title, &err.to_string(), now_secs());
                        }
                    }
                }
                Err(msg) => {
                    report.failed += 1;
                    let title = failure_title(&path, &msg);
                    store.lock().unwrap().record_failure(&path, &title, strip_title(&msg, &title), now_secs());
                    report.errors.push(msg);
                }
            }
        }
        notify(report);
    }
}
