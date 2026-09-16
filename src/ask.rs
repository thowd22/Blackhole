//! Ask mode plumbing: retrieval + generation on a worker thread, streaming
//! tokens back to the search panel as window messages.

use crate::embed::Embedder;
use crate::llm_ort::{Llm, Source};
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
        AskEngine { model_path, llm: Mutex::new(None), warming: AtomicBool::new(false), store, embedder }
    }

    /// Drop the loaded model to free its memory. Returns false if a generation
    /// is still holding it (the caller retries later).
    pub fn unload(&self) -> bool {
        match self.llm.try_lock() {
            Ok(mut guard) => {
                if guard.take().is_some() {
                    crate::util::log("llm unloaded (idle)");
                }
                true
            }
            Err(_) => false,
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
        let Ok(qvec) = self.embedder.embed(&question) else {
            post(WM_ASK_DONE, "Could not embed the question.".into());
            return;
        };
        let (absent, chunks) = {
            let store = self.store.lock().unwrap();
            let absent = store.absent(&question, Some(&qvec));
            (absent, if absent { Vec::new() } else { store.context(&question, &qvec, CONTEXT_WORDS) })
        };
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
