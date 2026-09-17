#![windows_subsystem = "windows"]
//! Blackhole — a tiny pixel-art black hole that swallows anything you drop on it
//! and lets you search it all with a click.

mod ask;
mod bubble;
mod chunk;
mod config;
mod dot;
mod embed;
mod expand;
mod drop;
mod ingest;
mod pdf_layout;
mod gpu;
mod llm_ort;
mod mcp;
mod hotkeys;
mod theme;
mod nvim;
mod ocr;
mod office;
mod rerank;
mod runtime;
mod screenshot;
mod search;
mod sprite;
mod startup;
mod store;
mod tray;
mod util;
mod web;

use std::sync::{mpsc, Arc, Mutex};
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::System::Ole::OleInitialize;
use windows::Win32::UI::HiDpi::{SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2};
use windows::Win32::UI::WindowsAndMessaging::*;

fn main() {
    // `blackhole.exe --mcp`: stdio MCP proxy to the running dot (starts one if needed).
    if std::env::args().any(|a| a == "--mcp") {
        mcp::run_stdio_proxy();
        return;
    }
    // `--ocr <image or pdf>`: print what the OCR reads and how long it took (benchmarks).
    if let Some(i) = std::env::args().position(|a| a == "--ocr") {
        if let Some(path) = std::env::args().nth(i + 1) {
            if runtime::init().is_err() {
                eprintln!("runtime failed");
                std::process::exit(1);
            }
            let t = std::time::Instant::now();
            let p = std::path::Path::new(&path);
            let text = if p.extension().map(|e| e.eq_ignore_ascii_case("pdf")).unwrap_or(false) {
                std::fs::read(p).ok().and_then(|b| ocr::read_pdf_images(&b))
            } else {
                ocr::read_file(p)
            };
            match text {
                Some(t2) => { println!("{t2}"); eprintln!("[{:.0} ms]", t.elapsed().as_secs_f32() * 1000.0); std::process::exit(0) }
                None => { eprintln!("no text / could not read"); std::process::exit(2) }
            }
        }
    }
    // `--selftest` (CI smoke test): unpack the runtime, embed a sentence, open a scratch
    // vault, add and search one item. Exit code 0 means the bundle works.
    if std::env::args().any(|a| a == "--selftest") {
        std::process::exit(selftest());
    }
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        if OleInitialize(None).is_err() {
            return;
        }

        let db = config::data_dir().join("vault.db");
        let store = match store::Store::open(&db) {
            Ok(s) => Arc::new(Mutex::new(s)),
            Err(e) => {
                let msg = util::wide(&format!("Could not open the vault:\n{e}"));
                MessageBoxW(None, windows::core::PCWSTR(msg.as_ptr()), windows::core::w!("Blackhole"), MB_ICONERROR);
                return;
            }
        };

        if let Err(e) = runtime::init() {
            let msg = util::wide(&format!("Could not start the ONNX Runtime:\n{e}"));
            MessageBoxW(None, windows::core::PCWSTR(msg.as_ptr()), windows::core::w!("Blackhole"), MB_ICONERROR);
            return;
        }
        let t0 = std::time::Instant::now();
        let embedder = match embed::Embedder::load() {
            Ok(e) => {
                util::log(&format!("embeddings on {} (loaded in {:?})", e.backend, t0.elapsed()));
                Arc::new(e)
            }
            Err(e) => {
                let msg = util::wide(&format!("Could not load the embedding model:\n{e}"));
                MessageBoxW(None, windows::core::PCWSTR(msg.as_ptr()), windows::core::w!("Blackhole"), MB_ICONERROR);
                return;
            }
        };

        // The LLM is optional and loaded lazily on the first question.
        let model = llm_ort::find_model(&config::data_dir());
        let ask = Arc::new(ask::AskEngine::new(model, store.clone(), embedder.clone()));

        let (tx, rx) = mpsc::channel();
        let hwnd = dot::Dot::create(tx, store.clone(), embedder.clone(), ask.clone());
        if hwnd.is_invalid() {
            return;
        }
        match mcp::start(store.clone(), embedder.clone(), ask, hwnd.0 as usize) {
            Some(port) => util::log(&format!("mcp: listening on 127.0.0.1:{port}")),
            None => util::log("mcp: could not bind a local port"),
        }

        // The worker owns a raw HWND value; posting to it is thread-safe.
        let target = hwnd.0 as usize;
        let worker_store = store.clone();
        let worker_embedder = embedder.clone();
        std::thread::spawn(move || {
            ingest::run(rx, worker_store, worker_embedder, |report| {
                let boxed = Box::into_raw(Box::new(report));
                let _ = PostMessageW(Some(HWND(target as *mut _)), dot::WM_INGEST_DONE, WPARAM(0), LPARAM(boxed as isize));
            });
        });

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

fn selftest() -> i32 {
    let step = |name: &str, r: Result<(), String>| -> bool {
        match r {
            Ok(()) => { println!("ok    {name}"); true }
            Err(e) => { println!("FAIL  {name}: {e}"); false }
        }
    };
    let t0 = std::time::Instant::now();
    if !step("runtime", runtime::init().map(|_| ()).map_err(|e| e.to_string())) {
        return 1;
    }
    let embedder = match embed::Embedder::load() {
        Ok(e) => { println!("ok    embedder on {}", e.backend); e }
        Err(e) => { println!("FAIL  embedder: {e}"); return 1; }
    };
    let vec = match embedder.embed("a small black hole that eats files") {
        Ok(v) if v.len() == embed::DIM => { println!("ok    embed ({} dims)", v.len()); v }
        Ok(v) => { println!("FAIL  embed: {} dims", v.len()); return 1; }
        Err(e) => { println!("FAIL  embed: {e}"); return 1; }
    };
    let dir = std::env::temp_dir().join(format!("blackhole-selftest-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let db = dir.join("vault.db");
    let ok = (|| -> Result<(), String> {
        let store = std::sync::Mutex::new(store::Store::open(&db).map_err(|e| e.to_string())?);
        let e = ingest::extract_text("The quick violet fox jumped over the accretion disk.");
        let id = store.lock().unwrap().add(&e.title, e.kind, None, &e.content, &ingest::hash_of(&e), util::now_secs()).map_err(|e| e.to_string())?.ok_or("not added")?;
        ingest::embed_item(&store, &embedder, id, &e.title, &e.content);
        let hits = store.lock().unwrap().search("violet fox", Some(&vec), 5);
        if hits.first().map(|h| h.id) != Some(id) {
            return Err(format!("search returned {} hits, first {:?}", hits.len(), hits.first().map(|h| h.id)));
        }
        Ok(())
    })();
    let _ = std::fs::remove_dir_all(&dir);
    if !step("vault add + search", ok) {
        return 1;
    }
    println!("selftest passed in {:.1}s", t0.elapsed().as_secs_f32());
    0
}
