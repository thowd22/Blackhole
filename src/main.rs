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
mod rerank;
mod runtime;
mod search;
mod sprite;
mod startup;
mod store;
mod tray;
mod util;

use std::sync::{mpsc, Arc, Mutex};
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::System::Ole::OleInitialize;
use windows::Win32::UI::HiDpi::{SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2};
use windows::Win32::UI::WindowsAndMessaging::*;

fn main() {
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
        let hwnd = dot::Dot::create(tx, store.clone(), embedder.clone(), ask);
        if hwnd.is_invalid() {
            return;
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
