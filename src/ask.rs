//! Ask mode plumbing: retrieval + generation on a worker thread, streaming
//! tokens back to the search panel as window messages.

use crate::embed::Embedder;
use crate::expand;
use crate::llm_ort::{Llm, Source};
use crate::rerank::Reranker;
use crate::store::Store;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::PostMessageW;

/// lparam = Box<String> (a token, or a status line); wparam = job id.
pub const WM_ASK_TOKEN: u32 = 0x8011;
pub const WM_ASK_STATUS: u32 = 0x8012;
/// lparam = Box<String> final status; wparam = job id.
pub const WM_ASK_DONE: u32 = 0x8013;

/// Words of excerpt context per question (≈1.4× that in tokens).
const CONTEXT_WORDS: usize = 1400;

pub struct AskEngine {
    model_path: Option<PathBuf>,
    llm: Mutex<Option<Llm>>,
    /// Cross-encoder for the ask path; loaded with the LLM, dropped with it.
    reranker: Mutex<Option<Reranker>>,
    /// A warm-up load is in flight (so repeated clicks don't spawn more).
    warming: AtomicBool,
    store: Arc<Mutex<Store>>,
    embedder: Arc<Embedder>,
}

pub struct Job {
    pub id: u64,
    pub cancel: Arc<AtomicBool>,
}

impl AskEngine {
    pub fn new(model_path: Option<PathBuf>, store: Arc<Mutex<Store>>, embedder: Arc<Embedder>) -> AskEngine {
        AskEngine { model_path, llm: Mutex::new(None), reranker: Mutex::new(None), warming: AtomicBool::new(false), store, embedder }
    }

    /// Drop the loaded model to free its memory. Returns false if a generation
    /// is still holding it (the caller retries later).
    pub fn unload(&self) -> bool {
        match self.llm.try_lock() {
            Ok(mut guard) => {
                if guard.take().is_some() {
                    crate::util::log("llm unloaded (idle)");
                }
                if let Ok(mut r) = self.reranker.try_lock() {
                    r.take();
                }
                crate::ocr::unload();
                true
            }
            Err(_) => false,
        }
    }

    /// Load the reranker if it isn't; None if it cannot load (ask degrades to the cheap pipeline).
    fn ensure_reranker(&self) {
        let mut r = self.reranker.lock().unwrap();
        if r.is_none() {
            let t = Instant::now();
            match Reranker::load() {
                Ok(x) => {
                    crate::util::log(&format!("reranker loaded in {:.2}s", t.elapsed().as_secs_f32()));
                    *r = Some(x);
                }
                Err(e) => crate::util::log(&format!("reranker failed to load: {e}")),
            }
        }
    }

    /// Start loading the model in the background (called when the search panel
    /// opens) so the first question doesn't pay the load time.
    pub fn preload(self: &Arc<Self>) {
        let Some(path) = self.model_path.clone() else { return };
        if self.warming.swap(true, Ordering::AcqRel) {
            return;
        }
        let engine = self.clone();
        std::thread::spawn(move || {
            engine.ensure_reranker();
            let mut guard = engine.llm.lock().unwrap();
            if guard.is_none() {
                let t = Instant::now();
                match Llm::load(&path) {
                    Ok(l) => {
                        crate::util::log(&format!("llm preloaded in {:.1}s", t.elapsed().as_secs_f32()));
                        *guard = Some(l);
                    }
                    Err(e) => crate::util::log(&format!("llm preload failed: {e}")),
                }
            }
            engine.warming.store(false, Ordering::Release);
        });
    }


    /// Kick off an answer for `question`; messages go to `target`.
    pub fn ask(self: &Arc<Self>, question: String, target: HWND, id: u64) -> Job {
        let cancel = Arc::new(AtomicBool::new(false));
        let engine = self.clone();
        let flag = cancel.clone();
        let hwnd = target.0 as usize;
        std::thread::spawn(move || engine.run(question, hwnd, id, flag));
        Job { id, cancel }
    }

    /// Answer about one text only (a note): no retrieval, the note is the whole context.
    pub fn ask_about(self: &Arc<Self>, question: String, title: String, text: String, target: HWND, id: u64) -> Job {
        let cancel = Arc::new(AtomicBool::new(false));
        let engine = self.clone();
        let flag = cancel.clone();
        let hwnd = target.0 as usize;
        std::thread::spawn(move || engine.run_about(question, title, text, hwnd, id, flag));
        Job { id, cancel }
    }

    fn run_about(&self, question: String, title: String, text: String, hwnd: usize, id: u64, cancel: Arc<AtomicBool>) {
        let post = |msg: u32, text: String| unsafe {
            let boxed = Box::into_raw(Box::new(text));
            let _ = PostMessageW(Some(HWND(hwnd as *mut _)), msg, WPARAM(id as usize), LPARAM(boxed as isize));
        };
        let Some(path) = &self.model_path else {
            post(WM_ASK_DONE, "No model found next to blackhole.exe.".into());
            return;
        };
        let mut guard = self.llm.lock().unwrap();
        if cancel.load(Ordering::Relaxed) {
            return;
        }
        if guard.is_none() {
            post(WM_ASK_STATUS, "loading model…".into());
            match Llm::load(path) {
                Ok(l) => *guard = Some(l),
                Err(e) => {
                    post(WM_ASK_DONE, format!("Could not load model: {e}"));
                    return;
                }
            }
        }
        let llm = guard.as_mut().unwrap();
        // Keep the note within the cache: ~1,400 words is what ask mode uses too.
        let words: Vec<&str> = text.split_whitespace().collect();
        let clipped = if words.len() > 1400 { words[..1400].join(" ") } else { text.clone() };
        let sources = [Source { title: &title, text: &clipped }];
        post(WM_ASK_STATUS, format!("thinking over this note ({} words) on {}…  Esc to stop", words.len(), llm.backend));
        let result = llm.answer(&question, &sources, &cancel, |tok| post(WM_ASK_TOKEN, tok.to_string()));
        let status = match result {
            Ok(_) if cancel.load(Ordering::Relaxed) => "stopped".to_string(),
            Ok(_) => format!("from \"{title}\""),
            Err(e) => format!("error: {e}"),
        };
        post(WM_ASK_DONE, status);
    }

    fn run(&self, question: String, hwnd: usize, id: u64, cancel: Arc<AtomicBool>) {
        let post = |msg: u32, text: String| unsafe {
            let boxed = Box::into_raw(Box::new(text));
            let _ = PostMessageW(Some(HWND(hwnd as *mut _)), msg, WPARAM(id as usize), LPARAM(boxed as isize));
        };

        let Some(path) = &self.model_path else {
            post(WM_ASK_DONE, "No model found. Put a Qwen2.5-Instruct ONNX file (e.g. qwen2.5-1.5b-instruct-q4.onnx) next to blackhole.exe.".into());
            return;
        };

        // Only one generation at a time; a cancelled predecessor lets go quickly.
        let mut guard = self.llm.lock().unwrap();
        if cancel.load(Ordering::Relaxed) {
            return;
        }
        if guard.is_none() {
            post(WM_ASK_STATUS, "loading model…".into());
            match Llm::load(path) {
                Ok(l) => *guard = Some(l),
                Err(e) => {
                    post(WM_ASK_DONE, format!("Could not load model: {e}"));
                    return;
                }
            }
        }
        let llm = guard.as_mut().unwrap();

        post(WM_ASK_STATUS, "reading your files…".into());
        let started = Instant::now();
        // Dense query = question + vault-anchored synonyms (ask mode only).
        let expansions: Vec<&str> = {
            let store = self.store.lock().unwrap();
            expand::expand(&question, |t| store.contains_term(t))
        };
        if !expansions.is_empty() {
            crate::util::log(&format!("expansion: {expansions:?}"));
        }
        let Ok(mut qvec) = self.embedder.embed(&expand::dense_query(&question, &expansions)) else {
            post(WM_ASK_DONE, "Could not embed the question.".into());
            return;
        };
        // Multi-query: the model rewrites the question into the words a file would
        // contain; their embeddings are folded into the query vector (question weighted
        // double) and their literal terms feed the boost and the absent gate.
        let rewrites = if std::env::var("BLACKHOLE_REWRITE").map(|v| v == "1").unwrap_or(false) { llm.search_terms(&question) } else { Vec::new() };
        if !rewrites.is_empty() {
            crate::util::log(&format!("rewrites: {rewrites:?}"));
            let mut acc: Vec<f32> = qvec.iter().map(|v| v * 2.0).collect();
            for r in &rewrites {
                if let Ok(v) = self.embedder.embed(r) {
                    for (a, b) in acc.iter_mut().zip(v) {
                        *a += b;
                    }
                }
            }
            let norm = acc.iter().map(|v| v * v).sum::<f32>().sqrt().max(1e-6);
            qvec = acc.iter().map(|v| v / norm).collect();
        }
        let aux = rewrites.join(" ");
        self.ensure_reranker();
        let reranker = self.reranker.lock().unwrap();
        let rerank_fn = |q: &str, passages: &[String]| -> Option<Vec<f32>> {
            reranker.as_ref().and_then(|r| r.score(q, passages).ok())
        };
        let (absent, chunks) = {
            let store = self.store.lock().unwrap();
            let absent = store.absent(&question, Some(&qvec)) && !store.any_term_present(&aux);
            let hook: Option<&dyn Fn(&str, &[String]) -> Option<Vec<f32>>> = if reranker.is_some() { Some(&rerank_fn) } else { None };
            (absent, if absent { Vec::new() } else { store.context_reranked(&question, &aux, &qvec, CONTEXT_WORDS, hook) })
        };
        drop(reranker);
        if absent {
            // None of the question's words occur anywhere in the vault: say so instead of
            // letting the model invent an answer from unrelated excerpts.
            crate::util::log(&format!("absent gate: {question:?}"));
            post(WM_ASK_DONE, "That isn't in your vault — none of those words appear in anything I've swallowed.".into());
            return;
        }
        if chunks.is_empty() {
            post(WM_ASK_DONE, "Nothing inside yet — drop something on me first.".into());
            return;
        }
        let mut chunks = chunks;
        if Llm::is_document_question(&question) {
            let cat = self.store.lock().unwrap().catalog(30);
            if !cat.is_empty() {
                chunks.push(("List of everything in the vault".to_string(), cat));
            }
        }
        let sources: Vec<Source> = chunks.iter().map(|(t, x)| Source { title: t, text: x }).collect();
        // Leave a trace of what the model saw, for debugging odd answers.
        let mut log = format!("Q: {question}\n\n");
        for (i, (t, x)) in chunks.iter().enumerate() {
            log.push_str(&format!("--- [{}] {t} ({} words)\n{x}\n\n", i + 1, x.split_whitespace().count()));
        }
        let _ = std::fs::write(crate::config::data_dir().join("last_ask.txt"), &log);

        let words: usize = sources.iter().map(|s| s.text.split_whitespace().count()).sum();
        post(WM_ASK_STATUS, format!("thinking over {} excerpts ({words} words) on {}…  Esc to stop", sources.len(), llm.backend));
        let result = llm.answer(&question, &sources, &cancel, |tok| post(WM_ASK_TOKEN, tok.to_string()));
        let status = match result {
            Ok(_) if cancel.load(Ordering::Relaxed) => "stopped".to_string(),
            Ok(_) => {
                let docs: Vec<&str> = {
                    let mut seen = Vec::new();
                    for (t, _) in &chunks {
                        if !seen.contains(&t.as_str()) {
                            seen.push(t.as_str());
                        }
                    }
                    seen
                };
                format!("from {} in {:.1}s", docs.join(", "), started.elapsed().as_secs_f32())
            }
            Err(e) => format!("error: {e}"),
        };
        post(WM_ASK_DONE, status);
    }
}
