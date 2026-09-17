//! MCP server: agents and tools use the vault as memory.
//!
//! The running dot hosts a Streamable-HTTP MCP endpoint on 127.0.0.1 (JSON-RPC over
//! POST, bearer token, nothing listens off-machine) and writes `mcp.json` (port +
//! token) beside the vault. `blackhole.exe --mcp` is a stdio proxy to that endpoint,
//! so every client — Windows or WSL (which can run the Windows exe directly) — is
//! configured the same way: `blackhole.exe --mcp`. If the dot isn't running the
//! proxy starts it.
//!
//! Tools: `put` (swallow text, a file or a URL, tags), `retrieve` (hybrid / keyword /
//! semantic search with the best chunk per hit, tag and kind filters), `get` (one
//! item in full), `list_recent`, `forget`, `ask` (ask mode as a call), `notify`
//! (speech bubble with an optional click action; mutable in Settings). Resources:
//! `blackhole://item/{id}`. See FEATURES.md §9.

use crate::embed::Embedder;
use crate::ingest;
use crate::store::Store;
use serde_json::{json, Value};
use std::io::{BufRead, Read, Write};
use std::net::TcpStream;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::PostMessageW;

const DEFAULT_PORT: u16 = 47811;
const PROTOCOL: &str = "2025-03-26";

fn info_path() -> std::path::PathBuf {
    crate::config::data_dir().join("mcp.json")
}

fn token() -> String {
    // Random enough for a loopback secret; no crypto crate needed.
    let mut h = sha2::Sha256::new();
    use sha2::Digest;
    h.update(std::process::id().to_le_bytes());
    h.update(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0).to_le_bytes());
    h.update(std::env::var("USERNAME").unwrap_or_default().as_bytes());
    format!("{:x}", h.finalize())
}

struct Ctx {
    store: Arc<Mutex<Store>>,
    embedder: Arc<Embedder>,
    ask: Arc<crate::ask::AskEngine>,
    hwnd: usize,
}

/// Start the HTTP endpoint in a background thread. Returns the port.
pub fn start(store: Arc<Mutex<Store>>, embedder: Arc<Embedder>, ask: Arc<crate::ask::AskEngine>, hwnd: usize) -> Option<u16> {
    let server = tiny_http::Server::http(("127.0.0.1", DEFAULT_PORT)).or_else(|_| tiny_http::Server::http(("127.0.0.1", 0))).ok()?;
    let port = server.server_addr().to_ip().map(|a| a.port())?;
    let tok = token();
    let _ = std::fs::write(info_path(), json!({ "port": port, "token": tok, "pid": std::process::id() }).to_string());
    let ctx = Arc::new(Ctx { store, embedder, ask, hwnd });
    std::thread::spawn(move || {
        for mut req in server.incoming_requests() {
            let authed = req.headers().iter().any(|h| h.field.equiv("Authorization") && h.value.as_str().trim() == format!("Bearer {tok}"));
            if !authed {
                let _ = req.respond(tiny_http::Response::from_string("unauthorized").with_status_code(401));
                continue;
            }
            if req.method() != &tiny_http::Method::Post || !req.url().starts_with("/mcp") {
                let _ = req.respond(tiny_http::Response::from_string("POST /mcp").with_status_code(405));
                continue;
            }
            let mut body = String::new();
            let _ = req.as_reader().read_to_string(&mut body);
            let ctx = ctx.clone();
            match serde_json::from_str::<Value>(&body) {
                Ok(msg) => match dispatch(&ctx, &msg) {
                    Some(resp) => {
                        let hdr = tiny_http::Header::from_bytes("Content-Type", "application/json").unwrap();
                        let _ = req.respond(tiny_http::Response::from_string(resp.to_string()).with_header(hdr));
                    }
                    None => {
                        let _ = req.respond(tiny_http::Response::empty(202));
                    }
                },
                Err(e) => {
                    let resp = json!({ "jsonrpc": "2.0", "id": null, "error": { "code": -32700, "message": format!("parse error: {e}") } });
                    let _ = req.respond(tiny_http::Response::from_string(resp.to_string()).with_status_code(400));
                }
            }
        }
    });
    Some(port)
}

/// One JSON-RPC message in, one response out (None for notifications).
fn dispatch(ctx: &Ctx, msg: &Value) -> Option<Value> {
    if let Some(batch) = msg.as_array() {
        let out: Vec<Value> = batch.iter().filter_map(|m| dispatch(ctx, m)).collect();
        return (!out.is_empty()).then(|| Value::Array(out));
    }
    let id = msg.get("id").cloned();
    let method = msg.get("method").and_then(|m| m.as_str()).unwrap_or("");
    let params = msg.get("params").cloned().unwrap_or(Value::Null);
    if method.starts_with("notifications/") {
        return None;
    }
    let result = match method {
        "initialize" => Ok(json!({
            "protocolVersion": params.get("protocolVersion").and_then(|v| v.as_str()).unwrap_or(PROTOCOL),
            "capabilities": { "tools": {}, "resources": {} },
            "serverInfo": { "name": "blackhole", "version": env!("CARGO_PKG_VERSION") },
            "instructions": "Blackhole is the user's local vault of files and notes. Use retrieve to look things up (get for an item in full), put to remember something, ask for a grounded answer from the local model, notify to show the user a speech bubble."
        })),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({ "tools": tools() })),
        "resources/list" => Ok(resources_list(ctx)),
        "resources/templates/list" => Ok(
            json!({ "resourceTemplates": [{ "uriTemplate": "blackhole://item/{id}", "name": "Vault item", "description": "The full text of one vault item (see retrieve / list_recent for ids).", "mimeType": "text/plain" }] }),
        ),
        "resources/read" => match params.get("uri").and_then(|v| v.as_str()).and_then(|u| u.strip_prefix("blackhole://item/")).and_then(|i| i.parse::<i64>().ok()) {
            Some(id) => match item_json(ctx, id, usize::MAX) {
                Some(v) => Ok(json!({ "contents": [{ "uri": format!("blackhole://item/{id}"), "mimeType": "text/plain", "text": v.get("content").and_then(|c| c.as_str()).unwrap_or("") }] })),
                None => Err((-32002, format!("no item {id}"))),
            },
            None => Err((-32602, "uri must be blackhole://item/<id>".to_string())),
        },
        "tools/call" => {
            let name = params.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let args = params.get("arguments").cloned().unwrap_or(json!({}));
            match call(ctx, name, &args) {
                Ok(text) => Ok(json!({ "content": [{ "type": "text", "text": text }], "isError": false })),
                Err(e) => Ok(json!({ "content": [{ "type": "text", "text": e }], "isError": true })),
            }
        }
        _ => Err((-32601, format!("method not found: {method}"))),
    };
    let id = id.unwrap_or(Value::Null);
    Some(match result {
        Ok(r) => json!({ "jsonrpc": "2.0", "id": id, "result": r }),
        Err((code, message)) => json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } }),
    })
}

fn tools() -> Value {
    json!([
        {
            "name": "put",
            "description": "Swallow something into the user's Blackhole vault so it can be searched and asked about later. Give text (a note, a snippet, a fact worth remembering), the path of a file on this machine, or the URL of a page to fetch and store as readable text.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "text": { "type": "string", "description": "Content to store." },
                    "path": { "type": "string", "description": "Path of a file to swallow instead of text (Windows path, or /mnt/c/... from WSL). Reads .pdf, .docx, .xlsx, .pptx, images (OCR), .html and plain text." },
                    "url": { "type": "string", "description": "http(s) URL to fetch (10 s, 5 MB cap) and store as the page's readable text; the URL is the item's identity, so putting it again refreshes it." },
                    "title": { "type": "string", "description": "Optional title; the first line of the text otherwise." },
                    "tags": { "type": "array", "items": { "type": "string" }, "description": "Optional tags (\"work\", \"ideas\"); searchable as #tag." },
                    "kind": { "type": "string", "enum": ["note"], "description": "\"note\" stores the text as an editable note in the Notes tab instead of a plain swallowed text." }
                }
            }
        },
        {
            "name": "retrieve",
            "description": "Search the user's vault. Returns ranked items with a snippet and the best-matching passage, for grounding an answer in the user's own files. Use get for an item's full text.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": { "type": "string" },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 25, "default": 8 },
                    "mode": { "type": "string", "enum": ["hybrid", "keyword", "semantic"], "default": "hybrid", "description": "hybrid = keyword + semantic fused; keyword = exact words only; semantic = embeddings only." },
                    "tags": { "type": "array", "items": { "type": "string" }, "description": "Only items carrying all of these tags." },
                    "kind": { "type": "string", "description": "Only this kind: note, text, pdf, image, md, code…" }
                },
                "required": ["query"]
            }
        },
        {
            "name": "get",
            "description": "One vault item in full: title, kind, tags, the path of the stored file if any, and its text.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "id": { "type": "integer", "description": "Item id from retrieve / list_recent." },
                    "max_chars": { "type": "integer", "default": 20000, "description": "Truncate the text after this many characters." }
                },
                "required": ["id"]
            }
        },
        {
            "name": "list_recent",
            "description": "The most recently swallowed items, newest first.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "limit": { "type": "integer", "minimum": 1, "maximum": 100, "default": 20 },
                    "kind": { "type": "string", "description": "Only this kind (e.g. note)." }
                }
            }
        },
        {
            "name": "forget",
            "description": "Delete one item from the vault (its text, chunks and stored copy). Ask the user before forgetting something they did not tell you to remove.",
            "inputSchema": {
                "type": "object",
                "properties": { "id": { "type": "integer" } },
                "required": ["id"]
            }
        },
        {
            "name": "ask",
            "description": "Ask Blackhole's local model a question grounded in the vault (its own retrieval + reranking). Slower than retrieve (seconds); returns the answer and the source titles. Says so when nothing in the vault matches.",
            "inputSchema": {
                "type": "object",
                "properties": { "question": { "type": "string" } },
                "required": ["question"]
            }
        },
        {
            "name": "notify",
            "description": "Show the user a pixel speech bubble from the Blackhole dot (for finished work, reminders, questions). Optionally clickable: open the vault search with a query, or an http(s) URL.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "text": { "type": "string" },
                    "title": { "type": "string" },
                    "action": {
                        "type": "object",
                        "description": "What a click on the bubble does: { \"open_search\": \"query\" } or { \"open_url\": \"https://…\" }.",
                        "properties": { "open_search": { "type": "string" }, "open_url": { "type": "string" } }
                    },
                    "timeout_ms": { "type": "integer", "minimum": 1000, "maximum": 60000, "description": "How long the bubble stays (default depends on length)." }
                },
                "required": ["text"]
            }
        }
    ])
}

fn call(ctx: &Ctx, name: &str, args: &Value) -> Result<String, String> {
    match name {
        "put" => put(ctx, args),
        "retrieve" => retrieve(ctx, args),
        "get" => get(ctx, args),
        "list_recent" => list_recent(ctx, args),
        "forget" => forget(ctx, args),
        "ask" => ask(ctx, args),
        "notify" => notify(ctx, args),
        _ => Err(format!("unknown tool: {name}")),
    }
}

fn id_arg(args: &Value) -> Result<i64, String> {
    args.get("id").and_then(|v| v.as_i64().or_else(|| v.as_str().and_then(|s| s.parse().ok()))).ok_or_else(|| "id is required".to_string())
}

/// "a, b" / ["a", "b"] → "a b" for `Store::set_tags`.
fn tags_arg(args: &Value) -> Option<String> {
    match args.get("tags")? {
        Value::Array(a) => Some(a.iter().filter_map(|v| v.as_str()).collect::<Vec<_>>().join(" ")),
        Value::String(s) => Some(s.clone()),
        _ => None,
    }
    .filter(|t| !t.trim().is_empty())
}

/// Keyword snippets carry the panel's highlight markers (\u{1}/\u{2}); agents get plain text.
fn plain(s: &str) -> String {
    s.replace(['\u{1}', '\u{2}'], "")
}

fn hit_json(h: &crate::store::Hit) -> Value {
    json!({ "id": h.id, "title": h.title, "kind": h.kind, "source": h.source, "tags": h.tags.split(' ').filter(|t| !t.is_empty()).collect::<Vec<_>>(), "snippet": plain(&h.snippet) })
}

fn item_json(ctx: &Ctx, id: i64, max_chars: usize) -> Option<Value> {
    let store = ctx.store.lock().unwrap();
    let h = store.item(id)?;
    let content = store.content(id).unwrap_or_default();
    let truncated = content.chars().count() > max_chars;
    let shown: String = content.chars().take(max_chars).collect();
    let mut v = hit_json(&h);
    v["path"] = json!(store.open_path(id));
    v["chars"] = json!(content.chars().count());
    v["truncated"] = json!(truncated);
    v["content"] = json!(shown);
    Some(v)
}

fn resources_list(ctx: &Ctx) -> Value {
    let store = ctx.store.lock().unwrap();
    let list: Vec<Value> = store
        .recent(100)
        .iter()
        .map(|h| json!({ "uri": format!("blackhole://item/{}", h.id), "name": h.title, "description": format!("{} · {}", h.kind, h.snippet.chars().take(80).collect::<String>()), "mimeType": "text/plain" }))
        .collect();
    json!({ "resources": list })
}

fn get(ctx: &Ctx, args: &Value) -> Result<String, String> {
    let id = id_arg(args)?;
    let max = args.get("max_chars").and_then(|v| v.as_u64()).unwrap_or(20000).max(200) as usize;
    item_json(ctx, id, max).map(|v| v.to_string()).ok_or_else(|| format!("no item with id {id}"))
}

fn list_recent(ctx: &Ctx, args: &Value) -> Result<String, String> {
    let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(20).clamp(1, 100) as usize;
    let kind = args.get("kind").and_then(|v| v.as_str()).map(str::to_string);
    let store = ctx.store.lock().unwrap();
    let mut hits = store.recent(if kind.is_some() { limit * 5 } else { limit });
    if let Some(k) = &kind {
        hits.retain(|h| &h.kind == k);
        hits.truncate(limit);
    }
    Ok(json!({ "count": hits.len(), "vault_items": store.count(), "items": hits.iter().map(hit_json).collect::<Vec<_>>() }).to_string())
}

fn forget(ctx: &Ctx, args: &Value) -> Result<String, String> {
    let id = id_arg(args)?;
    let mut store = ctx.store.lock().unwrap();
    let title = store.title_of(id).ok_or_else(|| format!("no item with id {id}"))?;
    store.delete(id).map_err(|e| e.to_string())?;
    drop(store);
    // The panel re-reads the vault on its next refresh; tell the dot so an open list updates.
    let report = ingest::Report { added: 0, updated: 0, duplicates: 0, failed: 0, errors: Vec::new() };
    post(ctx.hwnd, crate::dot::WM_INGEST_DONE, Box::into_raw(Box::new(report)) as isize);
    Ok(json!({ "forgotten": id, "title": title }).to_string())
}

fn ask(ctx: &Ctx, args: &Value) -> Result<String, String> {
    let question = args.get("question").and_then(|v| v.as_str()).map(str::trim).filter(|q| !q.is_empty()).ok_or("question is required")?;
    let t = std::time::Instant::now();
    let (answer, sources) = ctx.ask.answer_blocking(question)?;
    Ok(json!({ "question": question, "answer": answer, "sources": sources, "seconds": (t.elapsed().as_secs_f32() * 10.0).round() / 10.0 }).to_string())
}

fn post(hwnd: usize, msg: u32, lparam: isize) {
    unsafe {
        let _ = PostMessageW(Some(HWND(hwnd as *mut _)), msg, WPARAM(0), LPARAM(lparam));
    }
}

/// From WSL, `/mnt/c/Users/...` names a Windows file; the exe runs on Windows.
fn windows_path(p: &str) -> String {
    if let Some(rest) = p.strip_prefix("/mnt/") {
        let mut it = rest.splitn(2, '/');
        if let (Some(drive), Some(tail)) = (it.next(), it.next()) {
            if drive.len() == 1 {
                return format!("{}:\\{}", drive.to_ascii_uppercase(), tail.replace('/', "\\"));
            }
        }
    }
    p.to_string()
}

fn put(ctx: &Ctx, args: &Value) -> Result<String, String> {
    if crate::config::load().paused {
        return Err("Blackhole is paused and is not swallowing anything. Resume it from the dot's right-click menu (\"Pause swallowing\") or the Settings tab, then try again.".into());
    }
    let title = args.get("title").and_then(|v| v.as_str()).map(str::trim).filter(|t| !t.is_empty());
    let tags = tags_arg(args);
    if args.get("kind").and_then(|v| v.as_str()) == Some("note") {
        // An editable note: created like the panel does it, named if a title was given.
        let text = args.get("text").and_then(|v| v.as_str()).map(str::trim).filter(|t| !t.is_empty()).ok_or("a note needs text")?;
        let now = crate::util::now_secs();
        let id = ctx.store.lock().unwrap().add_note(now).map_err(|e| e.to_string())?.ok_or("could not create the note")?;
        let auto: String = text.lines().map(|l| l.trim().trim_start_matches('#').trim()).find(|l| !l.is_empty()).unwrap_or("Note").chars().take(80).collect();
        {
            let mut st = ctx.store.lock().unwrap();
            st.update_note(id, &auto, text).map_err(|e| e.to_string())?;
            if let Some(t) = title {
                st.set_note_title(id, t).map_err(|e| e.to_string())?;
            }
            if let Some(t) = &tags {
                st.set_tags(id, t).map_err(|e| e.to_string())?;
            }
        }
        ingest::embed_item(&ctx.store, &ctx.embedder, id, title.unwrap_or(&auto), text);
        let report = ingest::Report { added: 1, updated: 0, duplicates: 0, failed: 0, errors: Vec::new() };
        post(ctx.hwnd, crate::dot::WM_INGEST_DONE, Box::into_raw(Box::new(report)) as isize);
        return Ok(json!({ "id": id, "title": title.unwrap_or(&auto), "kind": "note", "words": text.split_whitespace().count(), "duplicate": false }).to_string());
    }
    let mut e = if let Some(url) = args.get("url").and_then(|v| v.as_str()).map(str::trim).filter(|u| !u.is_empty()) {
        // Explicitly asked for: the only way a page is fetched without the user
        // dropping or pasting it (see web.rs). The URL lands in log.txt either way.
        ingest::extract_url(url)?
    } else if let Some(path) = args.get("path").and_then(|v| v.as_str()) {
        let path = windows_path(path);
        match ingest::extract_file(Path::new(&path)) {
            Ok(e) => e,
            Err(msg) => {
                // Unreadable: remember it in the undigested list (FEATURES.md §3.5) before failing.
                let title = ingest::failure_title(&path, &msg);
                ctx.store.lock().unwrap().record_failure(&path, &title, ingest::strip_title(&msg, &title), crate::util::now_secs());
                return Err(msg);
            }
        }
    } else if let Some(text) = args.get("text").and_then(|v| v.as_str()) {
        if text.trim().is_empty() {
            return Err("text is empty".into());
        }
        ingest::extract_text(text)
    } else {
        return Err("give text, path or url".into());
    };
    if let Some(t) = title {
        e.title = t.to_string();
    }
    post(ctx.hwnd, crate::drop::WM_DROP_SWALLOW, 0);
    let hash = ingest::hash_of(&e);
    let added = ctx.store.lock().unwrap().add_stored(&e.title, e.kind, e.source.as_deref(), &e.content, &hash, crate::util::now_secs(), e.stored.as_deref()).map_err(|e| e.to_string())?;
    let (id, duplicate) = match added {
        Some(id) => {
            ingest::embed_item(&ctx.store, &ctx.embedder, id, &e.title, &e.content);
            (id, false)
        }
        None => {
            // Same bytes as before: keep one item, but refresh its text if the read changed.
            let id = ctx.store.lock().unwrap().id_by_hash(&hash).unwrap_or(0);
            let changed = id != 0 && ctx.store.lock().unwrap().content(id).as_deref() != Some(e.content.as_str());
            if changed && ctx.store.lock().unwrap().update_note(id, &e.title, &e.content).is_ok() {
                ingest::embed_item(&ctx.store, &ctx.embedder, id, &e.title, &e.content);
            }
            (id, true)
        }
    };
    if let (Some(t), true) = (&tags, id != 0) {
        let _ = ctx.store.lock().unwrap().set_tags(id, t);
    }
    if let Some(src) = &e.source {
        ctx.store.lock().unwrap().clear_failure_path(src);
    }
    let report = ingest::Report { added: usize::from(!duplicate), updated: 0, duplicates: usize::from(duplicate), failed: 0, errors: Vec::new() };
    post(ctx.hwnd, crate::dot::WM_INGEST_DONE, Box::into_raw(Box::new(report)) as isize);
    Ok(json!({ "id": id, "title": e.title, "kind": e.kind, "words": e.content.split_whitespace().count(), "duplicate": duplicate }).to_string())
}

fn retrieve(ctx: &Ctx, args: &Value) -> Result<String, String> {
    let query = args.get("query").and_then(|v| v.as_str()).map(str::trim).filter(|q| !q.is_empty()).ok_or("query is required")?;
    let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(8).clamp(1, 25) as usize;
    let mode = args.get("mode").and_then(|v| v.as_str()).unwrap_or("hybrid");
    let qvec = if mode == "keyword" { None } else { ctx.embedder.embed(query).ok() };
    let tags: Vec<String> =
        tags_arg(args).map(|t| t.split(|c: char| c == ',' || c.is_whitespace()).map(|x| x.trim_start_matches('#').to_lowercase()).filter(|x| !x.is_empty()).collect()).unwrap_or_default();
    let kind = args.get("kind").and_then(|v| v.as_str()).map(str::to_string);
    let store = ctx.store.lock().unwrap();
    let filtered = !tags.is_empty() || kind.is_some();
    let want = if filtered { limit * 4 } else { limit };
    let mut hits = if mode == "semantic" {
        match qvec.as_deref() {
            Some(v) => store.semantic(v, want),
            None => return Err("could not embed the query".into()),
        }
    } else {
        store.search(query, qvec.as_deref(), want)
    };
    if filtered {
        hits.retain(|h| tags.iter().all(|t| h.tags.split(' ').any(|x| x == t)) && kind.as_ref().is_none_or(|k| &h.kind == k));
        hits.truncate(limit);
    }
    let items: Vec<Value> = hits
        .iter()
        .map(|h| {
            let passage = qvec.as_deref().and_then(|v| store.best_chunk(h.id, v)).unwrap_or_else(|| h.snippet.clone());
            let mut v = hit_json(h);
            v["matched"] = json!(h.via);
            v["passage"] = json!(plain(&passage));
            v
        })
        .collect();
    Ok(json!({ "query": query, "mode": mode, "count": items.len(), "vault_items": store.count(), "hits": items }).to_string())
}

fn notify(ctx: &Ctx, args: &Value) -> Result<String, String> {
    let text = args.get("text").and_then(|v| v.as_str()).map(str::trim).filter(|t| !t.is_empty()).ok_or("text is required")?;
    let title = args.get("title").and_then(|v| v.as_str()).map(str::trim).filter(|t| !t.is_empty());
    if !crate::config::load().agent_notify {
        return Ok(json!({ "shown": false, "reason": "agent bubbles are off in Blackhole's Settings" }).to_string());
    }
    let shown = match title {
        Some(t) => format!("{t}\n{text}"),
        None => text.to_string(),
    };
    let action = match args.get("action") {
        Some(a) => {
            if let Some(q) = a.get("open_search").and_then(|v| v.as_str()).map(str::trim).filter(|q| !q.is_empty()) {
                Some(crate::dot::NoticeAction::Search(q.to_string()))
            } else if let Some(u) = a.get("open_url").and_then(|v| v.as_str()).map(str::trim) {
                if !(u.starts_with("http://") || u.starts_with("https://")) {
                    return Err("open_url must be an http(s) URL".into());
                }
                Some(crate::dot::NoticeAction::Url(u.to_string()))
            } else {
                return Err("action must be { open_search: query } or { open_url: url }".into());
            }
        }
        None => None,
    };
    let timeout_ms = args.get("timeout_ms").and_then(|v| v.as_u64()).map(|t| t as u32);
    let clickable = action.is_some();
    let n = crate::dot::Notice { text: shown, quiet: false, action, timeout_ms };
    post(ctx.hwnd, crate::dot::WM_NOTICE, Box::into_raw(Box::new(n)) as isize);
    Ok(json!({ "shown": true, "clickable": clickable }).to_string())
}

// ---------------------------------------------------------------------------
// `blackhole.exe --mcp`: stdio ⇄ the running dot's HTTP endpoint.

fn read_info() -> Option<(u16, String)> {
    let v: Value = serde_json::from_str(&std::fs::read_to_string(info_path()).ok()?).ok()?;
    Some((v.get("port")?.as_u64()? as u16, v.get("token")?.as_str()?.to_string()))
}

fn connect(port: u16) -> Option<TcpStream> {
    TcpStream::connect_timeout(&std::net::SocketAddr::from(([127, 0, 0, 1], port)), Duration::from_millis(800)).ok()
}

/// One HTTP POST; returns (status, body).
fn http_post(port: u16, tok: &str, body: &str) -> std::io::Result<(u16, String)> {
    let mut s = connect(port).ok_or_else(|| std::io::Error::new(std::io::ErrorKind::ConnectionRefused, "Blackhole is not running"))?;
    s.set_read_timeout(Some(Duration::from_secs(600)))?;
    write!(
        s,
        "POST /mcp HTTP/1.1\r\nHost: 127.0.0.1\r\nAuthorization: Bearer {tok}\r\nContent-Type: application/json\r\nAccept: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )?;
    let mut raw = Vec::new();
    s.read_to_end(&mut raw)?;
    let text = String::from_utf8_lossy(&raw);
    let (head, rest) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    let status: u16 = head.split_whitespace().nth(1).and_then(|c| c.parse().ok()).unwrap_or(0);
    Ok((status, rest.to_string()))
}

/// Make sure a dot is running; start one from this exe if not. Returns (port, token).
fn ensure_running() -> Result<(u16, String), String> {
    if let Some((port, tok)) = read_info() {
        if connect(port).is_some() {
            return Ok((port, tok));
        }
    }
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    std::process::Command::new(exe).spawn().map_err(|e| format!("could not start Blackhole: {e}"))?;
    for _ in 0..60 {
        std::thread::sleep(Duration::from_millis(500));
        if let Some((port, tok)) = read_info() {
            if connect(port).is_some() {
                return Ok((port, tok));
            }
        }
    }
    Err("Blackhole did not start".into())
}

pub fn run_stdio_proxy() {
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    let mut endpoint = ensure_running().ok();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let id = serde_json::from_str::<Value>(line).ok().and_then(|v| v.get("id").cloned());
        if endpoint.is_none() {
            endpoint = ensure_running().ok();
        }
        let reply = match &endpoint {
            Some((port, tok)) => match http_post(*port, tok, line) {
                Ok((202, _)) => None,
                Ok((_, body)) if !body.trim().is_empty() => Some(body),
                Ok((status, _)) => Some(json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32000, "message": format!("Blackhole answered HTTP {status}") } }).to_string()),
                Err(e) => {
                    endpoint = None;
                    Some(json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32000, "message": e.to_string() } }).to_string())
                }
            },
            None => id.map(|id| json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32000, "message": "Blackhole is not running and could not be started" } }).to_string()),
        };
        if let Some(r) = reply {
            let _ = writeln!(stdout, "{}", r.trim());
            let _ = stdout.flush();
        }
    }
}
