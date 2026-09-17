//! Web pages: fetching one over WinHTTP and reading it as text.
//!
//! Three pieces, all small:
//!   * `fetch` — a plain GET through WinHTTP (already in Windows, so no HTTP
//!     stack of our own): http/https only, 10 s timeouts, 5 MB cap, redirects
//!     followed, user-agent "Blackhole".
//!   * `readable` — HTML → the words a reader would see: script/style/nav/
//!     header/footer/aside dropped, the densest block of the page kept
//!     (text-to-tag ratio among the candidate containers), headings, paragraphs,
//!     list items and `<br>` turned into lines, entities decoded.
//!   * the hooks around them: a bare URL pasted or dropped on the dot, a `.url`
//!     shortcut, a saved `.html` file.
//!
//! Nothing here runs on its own: a fetch only ever happens for a URL the user
//! dropped or pasted, or one an agent explicitly `put`. Every fetch is logged.

use crate::office::{attr, decode, push_line, scan, Ev};
use std::ffi::c_void;
use windows::core::PCWSTR;
use windows::Win32::Networking::WinHttp::*;

/// Hard ceiling on a page, fetched or read off disk.
pub const MAX_BYTES: usize = 5 * 1024 * 1024;
/// Resolve / connect / send / receive timeout.
const TIMEOUT_MS: i32 = 10_000;

pub struct Page {
    /// Where the bytes finally came from (redirects followed).
    pub url: String,
    pub title: String,
    pub text: String,
}

// ------------------------------------------------------------------- URLs

/// A string that is nothing but an http(s) URL → that URL, trimmed.
pub fn bare_url(s: &str) -> Option<String> {
    let t = s.trim();
    if t.len() > 2048 || t.chars().any(char::is_whitespace) {
        return None;
    }
    let lower = t.to_ascii_lowercase();
    if !lower.starts_with("http://") && !lower.starts_with("https://") {
        return None;
    }
    // Needs a host: "https://" alone, or "https:///path", is not a page.
    let host = t.split("//").nth(1)?.split(['/', '?', '#']).next()?;
    (!host.is_empty()).then(|| t.to_string())
}

/// The URL inside a `.url` / `.website` / `.webloc` shortcut file.
/// Windows shortcuts are INI (`URL=…`); a macOS `.webloc` is a plist with the
/// URL in a `<string>` — both are one grep away.
pub fn url_from_shortcut(path: &std::path::Path) -> Option<String> {
    let text = std::fs::read(path).ok().map(|b| String::from_utf8_lossy(&b).into_owned())?;
    for line in text.lines() {
        let line = line.trim();
        if line.get(..4).is_some_and(|p| p.eq_ignore_ascii_case("URL=")) {
            if let Some(u) = bare_url(&line[4..]) {
                return Some(u);
            }
        }
        if let Some(open) = line.find("<string>") {
            let inner = &line[open + 8..];
            if let Some(end) = inner.find("</string>") {
                if let Some(u) = bare_url(&decode(&inner[..end])) {
                    return Some(u);
                }
            }
        }
    }
    None
}

/// "https://host:8080/a/b?c" → (secure, host, port, "/a/b?c").
fn split_url(url: &str) -> Result<(bool, String, u16, String), String> {
    let (scheme, rest) = url.split_once("://").ok_or("not an http(s) URL")?;
    let secure = match scheme.to_ascii_lowercase().as_str() {
        "https" => true,
        "http" => false,
        other => return Err(format!("{other}: only http and https are fetched")),
    };
    let cut = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let (authority, path) = rest.split_at(cut);
    // Credentials in the URL are dropped: nothing here should carry a secret.
    let authority = authority.rsplit('@').next().unwrap_or(authority);
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) if p.chars().all(|c| c.is_ascii_digit()) && !p.is_empty() => (h, p.parse().unwrap_or(0)),
        _ => (authority, if secure { 443 } else { 80 }),
    };
    if host.is_empty() {
        return Err("no host in the URL".into());
    }
    let path = if path.is_empty() { "/".to_string() } else { path.to_string() };
    Ok((secure, host.to_string(), port, path))
}

// ------------------------------------------------------------------ WinHTTP

struct Handle(*mut c_void);
impl Drop for Handle {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe {
                let _ = WinHttpCloseHandle(self.0);
            }
        }
    }
}

fn last_error() -> String {
    let e = windows::core::Error::from_win32();
    let msg = e.message();
    if msg.is_empty() {
        format!("WinHTTP error {:#x}", e.code().0)
    } else {
        msg
    }
}

/// GET a URL. Returns (final URL after redirects, content-type, body bytes).
pub fn fetch(url: &str) -> Result<(String, String, Vec<u8>), String> {
    let (secure, host, port, path) = split_url(url)?;
    // These buffers must outlive every PCWSTR handed to WinHTTP.
    let (agent, verb, host_w, path_w) = (crate::util::wide("Blackhole"), crate::util::wide("GET"), crate::util::wide(&host), crate::util::wide(&path));
    unsafe {
        let session = Handle(WinHttpOpen(PCWSTR(agent.as_ptr()), WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY, PCWSTR::null(), PCWSTR::null(), 0));
        if session.0.is_null() {
            return Err(format!("could not start WinHTTP: {}", last_error()));
        }
        let _ = WinHttpSetTimeouts(session.0, TIMEOUT_MS, TIMEOUT_MS, TIMEOUT_MS, TIMEOUT_MS);
        // Let WinHTTP unpack gzip/deflate for us (Windows 8.1+; ignored if unsupported).
        let flags = (WINHTTP_DECOMPRESSION_FLAG_GZIP | WINHTTP_DECOMPRESSION_FLAG_DEFLATE).to_le_bytes();
        let _ = WinHttpSetOption(Some(session.0), WINHTTP_OPTION_DECOMPRESSION, Some(&flags));

        let connect = Handle(WinHttpConnect(session.0, PCWSTR(host_w.as_ptr()), port, 0));
        if connect.0.is_null() {
            return Err(format!("could not reach {host}: {}", last_error()));
        }
        let req = Handle(WinHttpOpenRequest(
            connect.0,
            PCWSTR(verb.as_ptr()),
            PCWSTR(path_w.as_ptr()),
            PCWSTR::null(),
            PCWSTR::null(),
            std::ptr::null(),
            if secure { WINHTTP_FLAG_SECURE } else { WINHTTP_OPEN_REQUEST_FLAGS(0) },
        ));
        if req.0.is_null() {
            return Err(format!("could not open the request: {}", last_error()));
        }
        // Follow redirects, but never silently downgrade https → http.
        let policy = WINHTTP_OPTION_REDIRECT_POLICY_DISALLOW_HTTPS_TO_HTTP.to_le_bytes();
        let _ = WinHttpSetOption(Some(req.0), WINHTTP_OPTION_REDIRECT_POLICY, Some(&policy));

        let headers: Vec<u16> = "Accept: text/html,application/xhtml+xml,text/plain;q=0.9,*/*;q=0.5\r\nAccept-Language: en\r\n".encode_utf16().collect();
        WinHttpSendRequest(req.0, Some(&headers), None, 0, 0, 0).map_err(|_| format!("request failed: {}", last_error()))?;
        WinHttpReceiveResponse(req.0, std::ptr::null_mut()).map_err(|_| format!("no response: {}", last_error()))?;

        let mut status: u32 = 0;
        let mut len = 4u32;
        let _ = WinHttpQueryHeaders(req.0, WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER, PCWSTR::null(), Some(&mut status as *mut u32 as *mut c_void), &mut len, std::ptr::null_mut());
        if !(200..300).contains(&status) {
            return Err(format!("HTTP {status}"));
        }
        let content_type = query_string(req.0, |h, buf, len| WinHttpQueryHeaders(h, WINHTTP_QUERY_CONTENT_TYPE, PCWSTR::null(), Some(buf), len, std::ptr::null_mut())).unwrap_or_default();
        let final_url = query_string(req.0, |h, buf, len| WinHttpQueryOption(h, WINHTTP_OPTION_URL, Some(buf), len)).unwrap_or_else(|| url.to_string());

        let mut body: Vec<u8> = Vec::new();
        let mut chunk = vec![0u8; 32 * 1024];
        loop {
            let mut read = 0u32;
            if WinHttpReadData(req.0, chunk.as_mut_ptr() as *mut c_void, chunk.len() as u32, &mut read).is_err() {
                return Err(format!("read failed: {}", last_error()));
            }
            if read == 0 {
                break;
            }
            body.extend_from_slice(&chunk[..read as usize]);
            if body.len() >= MAX_BYTES {
                body.truncate(MAX_BYTES);
                break;
            }
        }
        Ok((final_url, content_type, body))
    }
}

/// Run a WinHTTP query that fills a wide-char buffer (length in bytes).
fn query_string(h: *mut c_void, f: impl Fn(*mut c_void, *mut c_void, *mut u32) -> windows::core::Result<()>) -> Option<String> {
    let mut buf = vec![0u16; 2048];
    let mut len = (buf.len() * 2) as u32;
    f(h, buf.as_mut_ptr() as *mut c_void, &mut len).ok()?;
    let chars = (len as usize / 2).min(buf.len());
    let s = crate::util::from_wide(&buf[..chars]);
    (!s.trim().is_empty()).then(|| s.trim().to_string())
}

/// Bytes → text, honouring the charset if it is one of the two that matter.
fn decode_body(bytes: &[u8], content_type: &str) -> String {
    let head = String::from_utf8_lossy(&bytes[..bytes.len().min(4096)]).to_ascii_lowercase();
    let declared = content_type.to_ascii_lowercase();
    let latin1 = |needle: &str| declared.contains(needle) || head.contains(needle);
    if latin1("windows-1252") || latin1("iso-8859-1") || latin1("latin1") {
        // Every byte is a code point in Latin-1; the 0x80..0x9f window is cp1252 punctuation.
        return bytes.iter().map(|&b| cp1252(b)).collect();
    }
    String::from_utf8_lossy(bytes).into_owned()
}

fn cp1252(b: u8) -> char {
    const HIGH: [char; 32] = [
        '\u{20ac}', '\u{81}', '\u{201a}', '\u{192}', '\u{201e}', '\u{2026}', '\u{2020}', '\u{2021}', '\u{2c6}', '\u{2030}', '\u{160}', '\u{2039}', '\u{152}', '\u{8d}', '\u{17d}', '\u{8f}', '\u{90}',
        '\u{2018}', '\u{2019}', '\u{201c}', '\u{201d}', '\u{2022}', '\u{2013}', '\u{2014}', '\u{2dc}', '\u{2122}', '\u{161}', '\u{203a}', '\u{153}', '\u{9d}', '\u{17e}', '\u{178}',
    ];
    if (0x80..0xa0).contains(&b) {
        HIGH[(b - 0x80) as usize]
    } else {
        b as char
    }
}

/// Fetch a URL and read it as a page. Text/plain comes through as it is; other
/// content types are refused (the vault stores words, not binaries).
pub fn fetch_page(url: &str) -> Result<Page, String> {
    let url = bare_url(url).ok_or("not an http(s) URL")?;
    crate::util::log(&format!("web: GET {url}"));
    let (final_url, content_type, body) = fetch(&url)?;
    let ct = content_type.to_ascii_lowercase();
    let text = decode_body(&body, &content_type);
    crate::util::log(&format!("web: {} bytes, {} <- {final_url}", body.len(), if ct.is_empty() { "no content-type" } else { ct.as_str() }));
    if ct.contains("html") || ct.contains("xml") || (ct.is_empty() && text.trim_start().starts_with('<')) {
        let (title, text) = readable(&text);
        if text.trim().is_empty() {
            return Err(format!("{final_url}: no readable text on the page"));
        }
        return Ok(Page { title: title.unwrap_or_else(|| host_of(&final_url)), url: final_url, text });
    }
    if ct.starts_with("text/") || ct.contains("json") || ct.is_empty() {
        return Ok(Page { title: last_segment(&final_url), url: final_url, text });
    }
    Err(format!("{final_url}: {ct} is not text"))
}

fn host_of(url: &str) -> String {
    url.split("//").nth(1).and_then(|r| r.split('/').next()).unwrap_or(url).to_string()
}

fn last_segment(url: &str) -> String {
    let path = url.split("//").nth(1).unwrap_or(url);
    let seg = path.split(['?', '#']).next().unwrap_or(path).trim_end_matches('/').rsplit('/').next().unwrap_or("");
    if seg.is_empty() || seg == host_of(url) {
        host_of(url)
    } else {
        seg.to_string()
    }
}

// ------------------------------------------------------------ HTML → text

/// Tags whose content is never prose.
const DROP: &[&str] = &["script", "style", "noscript", "svg", "iframe", "template", "canvas", "math", "head", "title"];
/// Page furniture: present on every page, part of none of them.
const CHROME: &[&str] = &["nav", "header", "footer", "aside", "form", "select", "button", "figcaption", "dialog"];
/// Containers that could hold the article.
const BOXES: &[&str] = &["article", "main", "section", "div", "td", "blockquote", "body"];

enum Tok {
    Start(String, String),
    End(String),
    Text(String),
}

struct Node {
    name: String,
    start: usize,
    end: usize,
    /// Characters of text inside, and opening tags inside: their ratio is the density.
    text: usize,
    tags: usize,
    /// role="main" / an id or class that says "content": a nudge, not a rule.
    hinted: bool,
}

/// A page's `<title>` and its readable text.
pub fn readable(html: &str) -> (Option<String>, String) {
    let stripped = strip_raw(html);
    let title = title_of(&stripped);
    let toks = tokenize(&stripped);
    let nodes = tree(&toks);
    let (start, end) = pick(&nodes, toks.len());
    let mut out = render(&toks[start..end]);
    // A page whose body is all chrome (or one big table) can come back empty:
    // fall back to the whole document rather than storing nothing.
    if out.split_whitespace().count() < 20 && (start, end) != (0, toks.len()) {
        out = render(&toks);
    }
    (title, out)
}

/// Remove `<script>`…`</script>` and friends outright: their content is not XML
/// and would confuse any scanner.
fn strip_raw(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let lower = html.to_ascii_lowercase();
    let mut i = 0;
    while i < html.len() {
        let Some(lt) = lower[i..].find('<').map(|p| p + i) else {
            out.push_str(&html[i..]);
            break;
        };
        out.push_str(&html[i..lt]);
        let tag = &lower[lt..];
        let raw = ["script", "style", "noscript", "svg", "template"]
            .iter()
            .find(|t| tag.len() > t.len() + 1 && tag[1..].starts_with(**t) && matches!(tag.as_bytes()[1 + t.len()], b'>' | b' ' | b'\t' | b'\n' | b'\r' | b'/'));
        match raw {
            Some(t) => {
                let close = format!("</{t}");
                match lower[lt..].find(&close) {
                    Some(rel) => {
                        let after = lt + rel;
                        i = lower[after..].find('>').map(|p| after + p + 1).unwrap_or(html.len());
                    }
                    None => break,
                }
            }
            None => {
                out.push('<');
                i = lt + 1;
            }
        }
    }
    out
}

fn title_of(html: &str) -> Option<String> {
    let mut title = String::new();
    let mut in_title = false;
    let mut done = false;
    scan(html, |ev| match ev {
        Ev::Open(t, _) if !done && t.eq_ignore_ascii_case("title") => in_title = true,
        Ev::Close(t) if in_title && t.eq_ignore_ascii_case("title") => {
            in_title = false;
            done = true;
        }
        Ev::Text(s) if in_title => title.push_str(&s),
        _ => {}
    });
    let title = collapse(&title);
    (!title.is_empty()).then_some(title)
}

fn tokenize(html: &str) -> Vec<Tok> {
    let mut toks = Vec::new();
    let mut drop_depth = 0usize;
    scan(html, |ev| match ev {
        Ev::Open(t, a) => {
            let name = t.to_ascii_lowercase();
            if DROP.contains(&name.as_str()) {
                drop_depth += 1;
            } else if drop_depth == 0 {
                toks.push(Tok::Start(name, a.to_string()));
            }
        }
        Ev::Empty(t, a) => {
            let name = t.to_ascii_lowercase();
            if drop_depth == 0 && !DROP.contains(&name.as_str()) {
                toks.push(Tok::Start(name.clone(), a.to_string()));
                toks.push(Tok::End(name));
            }
        }
        Ev::Close(t) => {
            let name = t.to_ascii_lowercase();
            if DROP.contains(&name.as_str()) {
                drop_depth = drop_depth.saturating_sub(1);
            } else if drop_depth == 0 {
                toks.push(Tok::End(name));
            }
        }
        Ev::Text(s) => {
            // Whitespace between two inline tags is the space between two words:
            // keep it as one, so "</b> and <b>" does not become "and".
            if drop_depth == 0 {
                toks.push(Tok::Text(if s.trim().is_empty() { " ".to_string() } else { s }));
            }
        }
    });
    toks
}

/// Void elements never close, so they must not stay on the stack.
fn void(name: &str) -> bool {
    matches!(name, "br" | "hr" | "img" | "input" | "meta" | "link" | "source" | "col" | "area" | "base" | "embed" | "param" | "track" | "wbr")
}

/// Build just enough of a tree to measure each container's text and tag counts.
fn tree(toks: &[Tok]) -> Vec<Node> {
    let mut nodes: Vec<Node> = Vec::new();
    let mut stack: Vec<usize> = Vec::new();
    for (i, tok) in toks.iter().enumerate() {
        match tok {
            Tok::Start(name, attrs) if !void(name) => {
                if let Some(&top) = stack.last() {
                    nodes[top].tags += 1;
                }
                let hint = format!("{} {}", attr(attrs, "id").unwrap_or_default(), attr(attrs, "class").unwrap_or_default()).to_ascii_lowercase();
                let hinted = attr(attrs, "role").as_deref() == Some("main") || ["article", "content", "post", "story", "entry", "markdown", "prose"].iter().any(|k| hint.contains(k));
                nodes.push(Node { name: name.clone(), start: i, end: toks.len(), text: 0, tags: 0, hinted });
                stack.push(nodes.len() - 1);
            }
            Tok::Start(_, _) => {
                if let Some(&top) = stack.last() {
                    nodes[top].tags += 1;
                }
            }
            Tok::End(name) => {
                if let Some(pos) = stack.iter().rposition(|&n| nodes[n].name == *name) {
                    // Everything opened inside it and never closed ends here too.
                    for idx in stack.drain(pos..).collect::<Vec<_>>().into_iter().rev() {
                        nodes[idx].end = i + 1;
                        let (text, tags) = (nodes[idx].text, nodes[idx].tags);
                        if let Some(&parent) = stack.last() {
                            nodes[parent].text += text;
                            nodes[parent].tags += tags;
                        }
                    }
                }
            }
            Tok::Text(s) => {
                if let Some(&top) = stack.last() {
                    nodes[top].text += s.trim().len();
                }
            }
        }
    }
    // Containers still open at the end of the document (common in real HTML) keep
    // whatever they gathered and run to the last token.
    nodes
}

/// The main block: among the containers holding a good share of the page's text,
/// the one with the highest text-to-tag ratio (a menu is all tags, prose is not).
fn pick(nodes: &[Node], total: usize) -> (usize, usize) {
    let candidates: Vec<&Node> = nodes.iter().filter(|n| BOXES.contains(&n.name.as_str()) && n.text > 0).collect();
    if candidates.is_empty() {
        return (0, total);
    }
    let best_text = candidates.iter().map(|n| n.text).max().unwrap_or(0);
    if best_text < 200 {
        return (0, total);
    }
    let score = |n: &Node| {
        let ratio = n.text as f64 / (n.tags as f64 + 1.0);
        ratio * if n.hinted { 1.5 } else { 1.0 }
    };
    let mut pool: Vec<&Node> = candidates.into_iter().filter(|n| n.text * 5 >= best_text * 2).collect();
    pool.sort_by(|a, b| score(b).partial_cmp(&score(a)).unwrap_or(std::cmp::Ordering::Equal).then(b.text.cmp(&a.text)));
    match pool.first() {
        Some(n) => (n.start, n.end),
        None => (0, total),
    }
}

/// Whitespace collapsed to single spaces.
fn collapse(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Tokens → lines: headings keep their level as '#'s, list items get "- ",
/// table cells on one row are joined by " | ".
fn render(toks: &[Tok]) -> String {
    let mut out = String::new();
    let mut cur = String::new();
    let mut skip = 0usize;
    let mut pending: Option<&'static str> = None;
    let mut heading: Option<usize> = None;
    let flush = |cur: &mut String, out: &mut String, pending: &mut Option<&'static str>, heading: &mut Option<usize>| {
        let line = collapse(cur);
        cur.clear();
        if line.is_empty() {
            *pending = None;
            *heading = None;
            return;
        }
        let line = match (*heading, *pending) {
            (Some(level), _) => format!("{} {line}", "#".repeat(level.clamp(1, 6))),
            (None, Some(p)) => format!("{p}{line}"),
            _ => line,
        };
        if heading.is_some() {
            push_line(out, "");
        }
        push_line(out, &line);
        *pending = None;
        *heading = None;
    };
    for tok in toks {
        match tok {
            Tok::Start(name, _) => {
                if skip > 0 {
                    if !void(name) && CHROME.contains(&name.as_str()) {
                        skip += 1;
                    }
                    continue;
                }
                if CHROME.contains(&name.as_str()) {
                    flush(&mut cur, &mut out, &mut pending, &mut heading);
                    skip = 1;
                    continue;
                }
                match name.as_str() {
                    "td" | "th" => {
                        if !cur.trim().is_empty() {
                            cur.push_str(" | ");
                        }
                    }
                    "br" => flush(&mut cur, &mut out, &mut pending, &mut heading),
                    "li" => {
                        flush(&mut cur, &mut out, &mut pending, &mut heading);
                        pending = Some("- ");
                    }
                    "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                        flush(&mut cur, &mut out, &mut pending, &mut heading);
                        heading = name[1..].parse::<usize>().ok();
                    }
                    "p" | "div" | "tr" | "ul" | "ol" | "table" | "section" | "article" | "blockquote" | "pre" | "dd" | "dt" | "hr" | "figure" => flush(&mut cur, &mut out, &mut pending, &mut heading),
                    _ => {}
                }
            }
            Tok::End(name) => {
                if skip > 0 {
                    if CHROME.contains(&name.as_str()) {
                        skip -= 1;
                    }
                    continue;
                }
                if matches!(
                    name.as_str(),
                    "p" | "div" | "tr" | "li" | "ul" | "ol" | "table" | "section" | "article" | "blockquote" | "pre" | "dd" | "dt" | "h1" | "h2" | "h3" | "h4" | "h5" | "h6" | "figure"
                ) {
                    flush(&mut cur, &mut out, &mut pending, &mut heading);
                }
            }
            Tok::Text(s) => {
                // Verbatim: the document's own spacing decides whether an inline
                // element runs into its neighbour ("(<em>x</em>)" has no spaces).
                if skip == 0 {
                    cur.push_str(s);
                }
            }
        }
    }
    flush(&mut cur, &mut out, &mut pending, &mut heading);
    out
}
