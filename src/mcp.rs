//! MCP server: agents and tools use the vault as memory.
//!
//! The running dot hosts a Streamable-HTTP MCP endpoint on 127.0.0.1 (JSON-RPC over
//! POST, bearer token, nothing listens off-machine) and writes `mcp.json` (port +
//! token) beside the vault. `blackhole.exe --mcp` is a stdio proxy to that endpoint,
//! so every client — Windows or WSL (which can run the Windows exe directly) — is
//! configured the same way: `blackhole.exe --mcp`. If the dot isn't running the
//! proxy starts it.
//!
//! Tools: `put` (swallow text or a file), `retrieve` (hybrid search with the best
//! chunk per hit), `notify` (speech bubble). See FEATURES.md §9.

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
    hwnd: usize,
}

/// Start the HTTP endpoint in a background thread. Returns the port.
pub fn start(store: Arc<Mutex<Store>>, embedder: Arc<Embedder>, hwnd: usize) -> Option<u16> {
    let server = tiny_http::Server::http(("127.0.0.1", DEFAULT_PORT))
        .or_else(|_| tiny_http::Server::http(("127.0.0.1", 0)))
        .ok()?;
    let port = server.server_addr().to_ip().map(|a| a.port())?;
    let tok = token();
    let _ = std::fs::write(info_path(), json!({ "port": port, "token": tok, "pid": std::process::id() }).to_string());
    let ctx = Arc::new(Ctx { store, embedder, hwnd });
    std::thread::spawn(move || {
        for mut req in server.incoming_requests() {
            let authed = req
                .headers()
                .iter()
                .any(|h| h.field.equiv("Authorization") && h.value.as_str().trim() == format!("Bearer {tok}"));
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
            "capabilities": { "tools": {} },
            "serverInfo": { "name": "blackhole", "version": env!("CARGO_PKG_VERSION") },
            "instructions": "Blackhole is the user's local vault of files and notes. Use retrieve to look things up, put to remember something, notify to show the user a speech bubble."
        })),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({ "tools": tools() })),
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
            "description": "Swallow something into the user's Blackhole vault so it can be searched and asked about later. Give text (a note, a snippet, a fact worth remembering) or the path of a file on this machine.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "text": { "type": "string", "description": "Content to store." },
                    "path": { "type": "string", "description": "Path of a file to swallow instead of text (Windows path, or /mnt/c/... from WSL)." },
                    "title": { "type": "string", "description": "Optional title; the first line of the text otherwise." }
                }
            }
        },
        {
            "name": "retrieve",
            "description": "Search the user's vault (keyword + semantic). Returns ranked items with a snippet and the best-matching passage, for grounding an answer in the user's own files.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": { "type": "string" },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 25, "default": 8 },
                    "mode": { "type": "string", "enum": ["hybrid", "keyword"], "default": "hybrid" }
                },
                "required": ["query"]
            }
        },
        {
            "name": "notify",
            "description": "Show the user a pixel speech bubble from the Blackhole dot (for finished work, reminders, questions).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "text": { "type": "string" },
                    "title": { "type": "string" }
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
        "notify" => notify(ctx, args),
        _ => Err(format!("unknown tool: {name}")),
    }
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
    let title = args.get("title").and_then(|v| v.as_str()).map(str::trim).filter(|t| !t.is_empty());
    let mut e = if let Some(path) = args.get("path").and_then(|v| v.as_str()) {
        ingest::extract_file(Path::new(&windows_path(path)))?
    } else if let Some(text) = args.get("text").and_then(|v| v.as_str()) {
        if text.trim().is_empty() {
            return Err("text is empty".into());
        }
        ingest::extract_text(text)
    } else {
        return Err("give text or path".into());
    };
    if let Some(t) = title {
        e.title = t.to_string();
    }
    post(ctx.hwnd, crate::drop::WM_DROP_SWALLOW, 0);
    let hash = ingest::hash_of(&e);
    let added = ctx.store.lock().unwrap().add(&e.title, e.kind, e.source.as_deref(), &e.content, &hash, crate::util::now_secs()).map_err(|e| e.to_string())?;
    let (id, duplicate) = match added {
        Some(id) => {
            ingest::embed_item(&ctx.store, &ctx.embedder, id, &e.title, &e.content);
            (id, false)
        }
        None => (ctx.store.lock().unwrap().id_by_hash(&hash).unwrap_or(0), true),
    };
    let report = ingest::Report { added: usize::from(!duplicate), duplicates: usize::from(duplicate), failed: 0, errors: Vec::new() };
    post(ctx.hwnd, crate::dot::WM_INGEST_DONE, Box::into_raw(Box::new(report)) as isize);
    Ok(json!({ "id": id, "title": e.title, "kind": e.kind, "words": e.content.split_whitespace().count(), "duplicate": duplicate }).to_string())
}

fn retrieve(ctx: &Ctx, args: &Value) -> Result<String, String> {
    let query = args.get("query").and_then(|v| v.as_str()).map(str::trim).filter(|q| !q.is_empty()).ok_or("query is required")?;
    let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(8).clamp(1, 25) as usize;
    let keyword_only = args.get("mode").and_then(|v| v.as_str()) == Some("keyword");
    let qvec = if keyword_only { None } else { ctx.embedder.embed(query).ok() };
    let store = ctx.store.lock().unwrap();
    let hits = store.search(query, qvec.as_deref(), limit);
    let items: Vec<Value> = hits
        .iter()
        .map(|h| {
            let passage = qvec.as_deref().and_then(|v| store.best_chunk(h.id, v)).unwrap_or_else(|| h.snippet.clone());
            json!({ "id": h.id, "title": h.title, "kind": h.kind, "source": h.source, "matched": h.via, "snippet": h.snippet, "passage": passage })
        })
        .collect();
    Ok(json!({ "query": query, "count": items.len(), "vault_items": store.count(), "hits": items }).to_string())
}

fn notify(ctx: &Ctx, args: &Value) -> Result<String, String> {
    let text = args.get("text").and_then(|v| v.as_str()).map(str::trim).filter(|t| !t.is_empty()).ok_or("text is required")?;
    let title = args.get("title").and_then(|v| v.as_str()).map(str::trim).filter(|t| !t.is_empty());
    let shown = match title {
        Some(t) => format!("{t}\n{text}"),
        None => text.to_string(),
    };
    post(ctx.hwnd, crate::dot::WM_NOTIFY, Box::into_raw(Box::new(shown)) as isize);
    Ok(json!({ "shown": true }).to_string())
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
