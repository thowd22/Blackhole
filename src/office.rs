//! Office documents → plain text: .docx, .xlsx, .pptx.
//!
//! All three are zip containers full of XML, so this is two small pieces: a
//! read-only zip reader (central directory + `flate2`, which the PNG encoder
//! already pulls in, so no new crate is compiled) and a hand-written XML scanner
//! that hands back tag/text events. A per-format walker then turns those events
//! into lines: a Word paragraph is a line and a table row is cells joined by
//! " | ", a spreadsheet is one heading per sheet followed by its rows, a deck is
//! one "Slide n" heading per slide followed by its text.
//!
//! Deliberately not a full Office reader: no styles, no charts, no drawings, no
//! formulas (a formula cell contributes its cached value). Text is what the
//! vault searches, so text is all this extracts.

use std::cell::Cell;
use std::io::Read;

/// Refuse to inflate more than this from one zip member (zip-bomb guard).
const MAX_ENTRY: u64 = 64 * 1024 * 1024;
/// …and no more than this from the whole container, however many members it has.
/// Four times what an item can hold (`ingest::MAX_CONTENT`), so a real document is
/// never clipped, while a bomb of a hundred highly compressible parts stops here
/// instead of growing a multi-gigabyte String on the ingest worker.
const MAX_TOTAL: u64 = 16 * 1024 * 1024;
/// Most sheets / slides one document is read from.
const MAX_PARTS: usize = 512;

// ---------------------------------------------------------------- zip reader

struct Entry {
    name: String,
    method: u16,
    /// Compressed size from the central directory (trustworthy even with a data descriptor).
    csize: u64,
    local_offset: u64,
}

pub struct Zip<'a> {
    bytes: &'a [u8],
    entries: Vec<Entry>,
    /// What is left of the container's shared inflate budget.
    budget: Cell<u64>,
}

fn u16_at(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}
fn u32_at(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}

impl<'a> Zip<'a> {
    pub fn open(bytes: &'a [u8]) -> Result<Zip<'a>, String> {
        if bytes.len() < 22 {
            return Err("not a zip container".into());
        }
        // End of central directory: its signature lives in the last 64 KiB + 22 bytes.
        let tail = bytes.len().saturating_sub(66 * 1024);
        let eocd = (tail..=bytes.len() - 22).rev().find(|&i| u32_at(bytes, i) == 0x0605_4b50).ok_or("not a zip container")?;
        if u32_at(bytes, eocd + 16) == u32::MAX {
            return Err("zip64 archives are not supported".into());
        }
        let count = u16_at(bytes, eocd + 10) as usize;
        let mut off = u32_at(bytes, eocd + 16) as usize;
        let mut entries = Vec::with_capacity(count.min(4096));
        let mut seen = std::collections::HashSet::new();
        while off + 46 <= bytes.len() && u32_at(bytes, off) == 0x0201_4b50 {
            let nlen = u16_at(bytes, off + 28) as usize;
            let elen = u16_at(bytes, off + 30) as usize;
            let clen = u16_at(bytes, off + 32) as usize;
            if off + 46 + nlen > bytes.len() {
                break;
            }
            let name = String::from_utf8_lossy(&bytes[off + 46..off + 46 + nlen]).into_owned();
            // A name repeated in the central directory would be read (and inflated)
            // once per record while always resolving to the same member: keep the first.
            if seen.insert(name.clone()) {
                entries.push(Entry { name, method: u16_at(bytes, off + 10), csize: u32_at(bytes, off + 20) as u64, local_offset: u32_at(bytes, off + 42) as u64 });
            }
            off += 46 + nlen + elen + clen;
        }
        if entries.is_empty() {
            return Err("empty or unreadable zip container".into());
        }
        Ok(Zip { bytes, entries, budget: Cell::new(MAX_TOTAL) })
    }

    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.entries.iter().map(|e| e.name.as_str())
    }

    pub fn has(&self, name: &str) -> bool {
        self.entries.iter().any(|e| e.name == name)
    }

    /// One member's bytes, inflated if it is deflated. None if missing, unreadable, or
    /// once the container's shared inflate budget is spent.
    pub fn read(&self, name: &str) -> Option<Vec<u8>> {
        let budget = self.budget.get();
        if budget == 0 {
            return None;
        }
        let e = self.entries.iter().find(|e| e.name == name)?;
        let lo = e.local_offset as usize;
        if lo + 30 > self.bytes.len() || u32_at(self.bytes, lo) != 0x0403_4b50 {
            return None;
        }
        let nlen = u16_at(self.bytes, lo + 26) as usize;
        let elen = u16_at(self.bytes, lo + 28) as usize;
        let start = lo + 30 + nlen + elen;
        let end = start.checked_add(e.csize as usize)?;
        if end > self.bytes.len() {
            return None;
        }
        let raw = &self.bytes[start..end];
        let cap = budget.min(MAX_ENTRY);
        let out = match e.method {
            0 => raw[..(cap as usize).min(raw.len())].to_vec(),
            8 => {
                // The central directory's uncompressed size is the file's word, not a
                // fact: grow from the compressed size instead of trusting a claim of
                // 64 MiB, and stop at whatever budget is left.
                let mut out = Vec::with_capacity(e.csize.saturating_mul(4).min(1 << 20) as usize);
                flate2::read::DeflateDecoder::new(raw).take(cap).read_to_end(&mut out).ok()?;
                out
            }
            _ => return None,
        };
        self.budget.set(budget.saturating_sub(out.len() as u64));
        Some(out)
    }

    /// One member as text (Office parts are UTF-8).
    pub fn text(&self, name: &str) -> Option<String> {
        self.read(name).map(|b| String::from_utf8_lossy(&b).into_owned())
    }
}

// ------------------------------------------------------------- xml scanner

pub enum Ev<'a> {
    /// `<w:p …>` — name, then the raw attribute text.
    Open(&'a str, &'a str),
    /// `<w:br/>`
    Empty(&'a str, &'a str),
    /// `</w:p>`
    Close(&'a str),
    /// Character data, entities already decoded.
    Text(String),
}

/// Walk an XML document, calling `f` for every tag and text node. Comments,
/// processing instructions and doctypes are skipped; CDATA counts as text.
pub fn scan(xml: &str, mut f: impl FnMut(Ev)) {
    let b = xml.as_bytes();
    let mut i = 0;
    while i < b.len() {
        let Some(lt) = b[i..].iter().position(|&c| c == b'<').map(|p| p + i) else {
            emit_text(&xml[i..], &mut f);
            return;
        };
        if lt > i {
            emit_text(&xml[i..lt], &mut f);
        }
        if b[lt..].starts_with(b"<!--") {
            i = find(b, lt + 4, b"-->").map(|p| p + 3).unwrap_or(b.len());
            continue;
        }
        if b[lt..].starts_with(b"<![CDATA[") {
            let end = find(b, lt + 9, b"]]>").unwrap_or(b.len());
            f(Ev::Text(xml[lt + 9..end].to_string()));
            i = (end + 3).min(b.len());
            continue;
        }
        if b[lt..].starts_with(b"<?") || b[lt..].starts_with(b"<!") {
            i = b[lt..].iter().position(|&c| c == b'>').map(|p| p + lt + 1).unwrap_or(b.len());
            continue;
        }
        // A tag: find its '>' without stopping inside a quoted attribute value.
        let mut j = lt + 1;
        let mut quote = 0u8;
        while j < b.len() {
            match b[j] {
                c if c == quote => quote = 0,
                b'"' | b'\'' if quote == 0 => quote = b[j],
                b'>' if quote == 0 => break,
                _ => {}
            }
            j += 1;
        }
        if j >= b.len() {
            return;
        }
        let inner = &xml[lt + 1..j];
        i = j + 1;
        if let Some(name) = inner.strip_prefix('/') {
            f(Ev::Close(name.trim()));
            continue;
        }
        let (body, empty) = match inner.strip_suffix('/') {
            Some(s) => (s, true),
            None => (inner, false),
        };
        let cut = body.find([' ', '\t', '\r', '\n']).unwrap_or(body.len());
        let (name, attrs) = body.split_at(cut);
        if empty {
            f(Ev::Empty(name, attrs))
        } else {
            f(Ev::Open(name, attrs))
        }
    }
}

fn find(b: &[u8], from: usize, needle: &[u8]) -> Option<usize> {
    if b.len() < needle.len() {
        return None;
    }
    (from..=b.len() - needle.len()).find(|&i| &b[i..i + needle.len()] == needle)
}

fn emit_text(s: &str, f: &mut impl FnMut(Ev)) {
    if !s.is_empty() {
        f(Ev::Text(decode(s)));
    }
}

/// One attribute out of a tag's raw attribute text.
pub fn attr(attrs: &str, name: &str) -> Option<String> {
    let mut rest = attrs;
    while let Some(eq) = rest.find('=') {
        let key = rest[..eq].trim().trim_start_matches(|c: char| c == '/' || c.is_whitespace());
        let after = rest[eq + 1..].trim_start();
        let quote = after.as_bytes().first().copied()?;
        if quote != b'"' && quote != b'\'' {
            return None;
        }
        let end = after[1..].find(quote as char)? + 1;
        if key == name {
            return Some(decode(&after[1..end]));
        }
        rest = &after[end + 1..];
    }
    None
}

/// XML/HTML entities → characters: numeric references plus the named entities
/// that actually turn up in documents and web pages.
pub fn decode(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        let tail = &rest[amp..];
        // An entity is short; look no further than 12 bytes, on a char boundary.
        let mut cut = tail.len().min(12);
        while !tail.is_char_boundary(cut) {
            cut -= 1;
        }
        let Some(semi) = tail[..cut].find(';') else {
            out.push('&');
            rest = &tail[1..];
            continue;
        };
        let name = &tail[1..semi];
        let ch = if let Some(hex) = name.strip_prefix("#x").or_else(|| name.strip_prefix("#X")) {
            u32::from_str_radix(hex, 16).ok().and_then(char::from_u32).map(String::from)
        } else if let Some(dec) = name.strip_prefix('#') {
            dec.parse::<u32>().ok().and_then(char::from_u32).map(String::from)
        } else {
            named(name).map(String::from)
        };
        match ch {
            Some(c) => out.push_str(&c),
            None => out.push_str(&tail[..=semi]),
        }
        rest = &tail[semi + 1..];
    }
    out.push_str(rest);
    out
}

fn named(n: &str) -> Option<&'static str> {
    Some(match n {
        "amp" => "&",
        "lt" => "<",
        "gt" => ">",
        "quot" => "\"",
        "apos" => "'",
        "nbsp" | "ensp" | "emsp" | "thinsp" => " ",
        "mdash" => "\u{2014}",
        "ndash" => "\u{2013}",
        "hellip" => "\u{2026}",
        "rsquo" | "rsquor" => "\u{2019}",
        "lsquo" => "\u{2018}",
        "ldquo" => "\u{201c}",
        "rdquo" => "\u{201d}",
        "bull" => "\u{2022}",
        "middot" => "\u{b7}",
        "copy" => "\u{a9}",
        "reg" => "\u{ae}",
        "trade" => "\u{2122}",
        "deg" => "\u{b0}",
        "euro" => "\u{20ac}",
        "pound" => "\u{a3}",
        "times" => "\u{d7}",
        "laquo" => "\u{ab}",
        "raquo" => "\u{bb}",
        "shy" | "zwj" | "zwnj" => "",
        _ => return None,
    })
}

// -------------------------------------------------------------- the formats

/// Local name of a tag: "w:p" → "p".
fn local(tag: &str) -> &str {
    tag.rsplit(':').next().unwrap_or(tag)
}

pub fn push_line(out: &mut String, line: &str) {
    let line = line.trim_end();
    if line.trim().is_empty() {
        // At most one blank line between blocks, and never a leading one.
        if !out.is_empty() && !out.ends_with("\n\n") {
            out.push('\n');
        }
        return;
    }
    out.push_str(line);
    out.push('\n');
}

/// True for the extensions `extract` understands.
pub fn is_office(ext: &str) -> bool {
    matches!(ext, "docx" | "docm" | "xlsx" | "xlsm" | "pptx" | "pptm")
}

/// The vault kind for an office extension: "docx" / "xlsx" / "pptx".
pub fn kind_of(ext: &str) -> &'static str {
    match ext {
        "xlsx" | "xlsm" => "xlsx",
        "pptx" | "pptm" => "pptx",
        _ => "docx",
    }
}

/// Dispatch on the extension; `bytes` is the whole file.
pub fn extract(ext: &str, bytes: &[u8]) -> Result<String, String> {
    let zip = Zip::open(bytes).map_err(|e| format!("not a readable .{ext} file: {e}"))?;
    let text = match kind_of(ext) {
        "xlsx" => xlsx(&zip),
        "pptx" => pptx(&zip),
        _ => docx(&zip),
    }?;
    if text.trim().is_empty() {
        return Err(format!("no text in this .{ext} file"));
    }
    Ok(text)
}

/// Word: paragraphs as lines, table rows as cells joined by " | ".
pub fn docx(zip: &Zip) -> Result<String, String> {
    let xml = zip.text("word/document.xml").ok_or("no word/document.xml (not a .docx?)")?;
    let mut out = String::new();
    // A paragraph under construction, plus the table row/cell it may sit in.
    let mut para = String::new();
    let mut cell = String::new();
    let mut row: Vec<String> = Vec::new();
    let mut in_cell = 0usize;
    let mut in_text = false;
    let mut deleted = 0usize;
    scan(&xml, |ev| match ev {
        Ev::Open(t, _) => match local(t) {
            "t" => in_text = true,
            "tc" => {
                in_cell += 1;
                cell.clear();
            }
            "tr" => row.clear(),
            // Tracked deletions keep their text in <w:delText>: read the document as it stands.
            "delText" => deleted += 1,
            _ => {}
        },
        Ev::Empty(t, _) => match local(t) {
            "tab" => para.push('\t'),
            "br" | "cr" => para.push('\n'),
            _ => {}
        },
        Ev::Close(t) => match local(t) {
            "t" => in_text = false,
            "delText" => deleted = deleted.saturating_sub(1),
            "p" => {
                let line = std::mem::take(&mut para);
                if in_cell > 0 {
                    if !line.trim().is_empty() {
                        if !cell.is_empty() {
                            cell.push(' ');
                        }
                        cell.push_str(line.trim());
                    }
                } else {
                    for l in line.split('\n') {
                        push_line(&mut out, l);
                    }
                }
            }
            "tc" => {
                in_cell = in_cell.saturating_sub(1);
                row.push(std::mem::take(&mut cell));
            }
            "tr" => {
                let line = std::mem::take(&mut row).join(" | ");
                push_line(&mut out, &line);
            }
            _ => {}
        },
        Ev::Text(s) => {
            if in_text && deleted == 0 {
                para.push_str(&s);
            }
        }
    });
    if !para.trim().is_empty() {
        push_line(&mut out, &para);
    }
    Ok(out)
}

/// Excel: one heading per sheet, then each row's cells joined by " | ".
pub fn xlsx(zip: &Zip) -> Result<String, String> {
    let shared = zip.text("xl/sharedStrings.xml").map(|x| shared_strings(&x)).unwrap_or_default();
    let mut out = String::new();
    for (name, part) in sheets(zip) {
        let Some(xml) = zip.text(&part) else { continue };
        push_line(&mut out, &format!("## {name}"));
        sheet_rows(&xml, &shared, &mut out);
        push_line(&mut out, "");
    }
    if out.trim().is_empty() {
        return Err("no worksheet rows in this .xlsx".into());
    }
    Ok(out)
}

/// `<si>` elements in order; each one's `<t>` runs concatenated.
fn shared_strings(xml: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_text = false;
    scan(xml, |ev| match ev {
        Ev::Open(t, _) => match local(t) {
            "si" => cur.clear(),
            "t" => in_text = true,
            _ => {}
        },
        Ev::Close(t) => match local(t) {
            "t" => in_text = false,
            "si" => out.push(std::mem::take(&mut cur)),
            _ => {}
        },
        Ev::Text(s) => {
            if in_text {
                cur.push_str(&s);
            }
        }
        _ => {}
    });
    out
}

/// Sheet (display name, zip part) pairs in workbook order; falls back to the
/// worksheet parts sorted by number when the workbook rels are unreadable.
fn sheets(zip: &Zip) -> Vec<(String, String)> {
    let rels: Vec<(String, String)> = zip
        .text("xl/_rels/workbook.xml.rels")
        .map(|xml| {
            let mut v = Vec::new();
            scan(&xml, |ev| {
                if let Ev::Open(t, a) | Ev::Empty(t, a) = ev {
                    if local(t) == "Relationship" {
                        if let (Some(id), Some(target)) = (attr(a, "Id"), attr(a, "Target")) {
                            v.push((id, target));
                        }
                    }
                }
            });
            v
        })
        .unwrap_or_default();
    let mut out: Vec<(String, String)> = Vec::new();
    if let Some(xml) = zip.text("xl/workbook.xml") {
        scan(&xml, |ev| {
            if let Ev::Open(t, a) | Ev::Empty(t, a) = ev {
                if local(t) == "sheet" {
                    let n = out.len() + 1;
                    let name = attr(a, "name").unwrap_or_else(|| format!("Sheet {n}"));
                    let target =
                        attr(a, "r:id").or_else(|| attr(a, "id")).and_then(|id| rels.iter().find(|(i, _)| *i == id).map(|(_, t)| t.clone())).unwrap_or_else(|| format!("worksheets/sheet{n}.xml"));
                    let part = if let Some(abs) = target.strip_prefix('/') { abs.to_string() } else { format!("xl/{}", target.trim_start_matches("./")) };
                    out.push((name, part));
                }
            }
        });
    }
    out.retain(|(_, part)| zip.has(part));
    if out.is_empty() {
        let mut parts: Vec<String> = zip.names().filter(|n| n.starts_with("xl/worksheets/sheet") && n.ends_with(".xml")).map(str::to_string).collect();
        parts.sort_by_key(|n| part_number(n));
        out = parts.iter().enumerate().map(|(i, p)| (format!("Sheet {}", i + 1), p.clone())).collect();
    }
    out.truncate(MAX_PARTS);
    out
}

/// "xl/worksheets/sheet12.xml" → 12 (for ordering parts by their number).
fn part_number(name: &str) -> u32 {
    let stem = name.rsplit('/').next().unwrap_or(name);
    stem.chars().skip_while(|c| !c.is_ascii_digit()).take_while(char::is_ascii_digit).collect::<String>().parse().unwrap_or(0)
}

/// `<row>` → one line of cells joined by " | ". Shared strings resolved, inline
/// strings taken as they are, numbers written exactly as the file stores them.
fn sheet_rows(xml: &str, shared: &[String], out: &mut String) {
    let mut row: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut cell_type = String::new();
    let mut in_v = false;
    let mut in_t = false;
    scan(xml, |ev| match ev {
        Ev::Open(t, a) => match local(t) {
            "row" => row.clear(),
            "c" => {
                cell_type = attr(a, "t").unwrap_or_default();
                cur.clear();
            }
            "v" => in_v = true,
            "t" => in_t = true,
            _ => {}
        },
        Ev::Empty(t, _) => {
            // An empty cell still occupies a column: keep the row's shape.
            if local(t) == "c" {
                row.push(String::new());
            }
        }
        Ev::Close(t) => match local(t) {
            "v" => in_v = false,
            "t" => in_t = false,
            "c" => {
                let text = std::mem::take(&mut cur);
                let value = if cell_type == "s" { text.trim().parse::<usize>().ok().and_then(|i| shared.get(i).cloned()).unwrap_or_default() } else { text };
                row.push(value.replace(['\r', '\n'], " ").trim().to_string());
            }
            "row" => {
                while row.last().map(|c| c.is_empty()).unwrap_or(false) {
                    row.pop();
                }
                if !row.is_empty() {
                    let line = std::mem::take(&mut row).join(" | ");
                    push_line(out, &line);
                }
                row.clear();
            }
            _ => {}
        },
        Ev::Text(s) => {
            // <v> carries numbers and shared-string indexes, <is><t> inline strings.
            if in_v || (in_t && cell_type != "s") {
                cur.push_str(&s);
            }
        }
    });
}

/// PowerPoint: slides in order, each headed "Slide n", then its text runs.
pub fn pptx(zip: &Zip) -> Result<String, String> {
    let mut slides: Vec<String> = zip.names().filter(|n| n.starts_with("ppt/slides/slide") && n.ends_with(".xml")).map(str::to_string).collect();
    if slides.is_empty() {
        return Err("no slides in this .pptx".into());
    }
    slides.sort_by_key(|n| part_number(n));
    slides.truncate(MAX_PARTS);
    let mut out = String::new();
    for (i, part) in slides.iter().enumerate() {
        let Some(xml) = zip.text(part) else { continue };
        push_line(&mut out, &format!("Slide {}", i + 1));
        slide_text(&xml, &mut out);
        // Speaker notes are the slide's words too.
        let notes = format!("ppt/notesSlides/notesSlide{}.xml", part_number(part));
        if let Some(xml) = zip.text(&notes) {
            let mut n = String::new();
            slide_text(&xml, &mut n);
            // The notes part repeats the slide number as its own paragraph; drop bare numbers.
            let body: Vec<&str> = n.lines().filter(|l| !l.trim().is_empty() && l.trim().parse::<u32>().is_err()).collect();
            if !body.is_empty() {
                push_line(&mut out, &format!("Notes: {}", body.join(" ")));
            }
        }
        push_line(&mut out, "");
    }
    Ok(out)
}

/// `<a:p>` paragraphs of `<a:t>` runs; a table on a slide reads as rows.
fn slide_text(xml: &str, out: &mut String) {
    let mut para = String::new();
    let mut cell = String::new();
    let mut row: Vec<String> = Vec::new();
    let mut in_cell = 0usize;
    let mut in_text = false;
    scan(xml, |ev| match ev {
        Ev::Open(t, _) => match local(t) {
            "t" => in_text = true,
            "tc" => {
                in_cell += 1;
                cell.clear();
            }
            "tr" => row.clear(),
            _ => {}
        },
        Ev::Empty(t, _) => {
            if local(t) == "br" {
                para.push('\n');
            }
        }
        Ev::Close(t) => match local(t) {
            "t" => in_text = false,
            "p" => {
                let line = std::mem::take(&mut para);
                if in_cell > 0 {
                    if !line.trim().is_empty() {
                        if !cell.is_empty() {
                            cell.push(' ');
                        }
                        cell.push_str(line.trim());
                    }
                } else {
                    for l in line.split('\n') {
                        push_line(out, l);
                    }
                }
            }
            "tc" => {
                in_cell = in_cell.saturating_sub(1);
                row.push(std::mem::take(&mut cell));
            }
            "tr" => {
                let line = std::mem::take(&mut row).join(" | ");
                push_line(out, &line);
            }
            _ => {}
        },
        Ev::Text(s) => {
            if in_text {
                para.push_str(&s);
            }
        }
    });
    if !para.trim().is_empty() {
        push_line(out, &para);
    }
}
