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
    /// The graph ask mode loads; the Settings tab can point it at another model.
    model_path: Mutex<Option<PathBuf>>,
    /// The loaded model is no longer the configured one: drop it before the next answer.
    stale: AtomicBool,
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
        AskEngine { model_path: Mutex::new(model_path), stale: AtomicBool::new(false), llm: Mutex::new(None), reranker: Mutex::new(None), warming: AtomicBool::new(false), store, embedder }
    }

    /// Switch models (Settings → "Ask model"). The loaded one is dropped now if it is
    /// idle, otherwise just before the next answer, and the new one loads on demand.
    pub fn set_model(&self, path: Option<PathBuf>) {
        let mut guard = self.model_path.lock().unwrap();
        if *guard == path {
            return;
        }
        *guard = path;
        drop(guard);
        self.stale.store(true, Ordering::Release);
        if self.unload() {
            self.stale.store(false, Ordering::Release);
        }
    }

    fn path(&self) -> Option<PathBuf> {
        self.model_path.lock().unwrap().clone()
    }

    /// Called with the model lock held: throw away a model the user switched away from.
    fn drop_if_stale(&self, guard: &mut Option<Llm>) {
        if self.stale.swap(false, Ordering::AcqRel) {
            guard.take();
        }
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
        let Some(path) = self.path() else { return };
        if self.warming.swap(true, Ordering::AcqRel) {
            return;
        }
        let engine = self.clone();
        std::thread::spawn(move || {
            engine.ensure_reranker();
            let mut guard = engine.llm.lock().unwrap();
            engine.drop_if_stale(&mut guard);
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
        let Some(path) = self.path() else {
            post(WM_ASK_DONE, "No model found next to blackhole.exe.".into());
            return;
        };
        let mut guard = self.llm.lock().unwrap();
        if cancel.load(Ordering::Relaxed) {
            return;
        }
        self.drop_if_stale(&mut guard);
        if guard.is_none() {
            post(WM_ASK_STATUS, "loading model…".into());
            match Llm::load(&path) {
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

    /// Retrieval for a question: expansion, dense query, reranked context, the vault
    /// catalog for document questions. `Err` carries the user-facing reason there is
    /// nothing to answer from (absent gate, empty vault, embedding failure).
    fn gather(&self, llm: &mut Llm, question: &str) -> Result<Vec<(String, String)>, String> {
        self.gather_from(&self.store, Some(llm), question)
    }

    /// The same retrieval over any vault, with the model optional: the self-check
    /// runs it over a scratch vault, and without an LLM (no query rewrites, and the
    /// excerpts are the answer) it still says what the pipeline would have seen.
    fn gather_from(&self, store_arc: &Mutex<Store>, llm: Option<&mut Llm>, question: &str) -> Result<Vec<(String, String)>, String> {
        // Dense query = question + vault-anchored synonyms (ask mode only).
        let expansions: Vec<&str> = {
            let store = store_arc.lock().unwrap();
            expand::expand(question, |t| store.contains_term(t))
        };
        if !expansions.is_empty() {
            crate::util::log(&format!("expansion: {expansions:?}"));
        }
        let Ok(mut qvec) = self.embedder.embed(&expand::dense_query(question, &expansions)) else {
            return Err("Could not embed the question.".into());
        };
        // Multi-query: the model rewrites the question into the words a file would
        // contain; their embeddings are folded into the query vector (question weighted
        // double) and their literal terms feed the boost and the absent gate.
        let rewrites = match llm {
            Some(l) if std::env::var("BLACKHOLE_REWRITE").map(|v| v == "1").unwrap_or(false) => l.search_terms(question),
            _ => Vec::new(),
        };
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
        let rerank_fn = |q: &str, passages: &[String]| -> Option<Vec<f32>> { reranker.as_ref().and_then(|r| r.score(q, passages).ok()) };
        let (absent, chunks) = {
            let store = store_arc.lock().unwrap();
            let absent = store.absent(question, Some(&qvec)) && !store.any_term_present(&aux);
            let hook: Option<&dyn Fn(&str, &[String]) -> Option<Vec<f32>>> = if reranker.is_some() { Some(&rerank_fn) } else { None };
            (absent, if absent { Vec::new() } else { store.context_reranked(question, &aux, &qvec, CONTEXT_WORDS, hook) })
        };
        drop(reranker);
        if absent {
            // None of the question's words occur anywhere in the vault: say so instead of
            // letting the model invent an answer from unrelated excerpts.
            crate::util::log(&format!("absent gate: {question:?}"));
            return Err("That isn't in your vault — none of those words appear in anything I've swallowed.".into());
        }
        if chunks.is_empty() {
            return Err("Nothing inside yet — drop something on me first.".into());
        }
        let mut chunks = chunks;
        if Llm::is_document_question(question) {
            let cat = store_arc.lock().unwrap().catalog(30);
            if !cat.is_empty() {
                chunks.push(("List of everything in the vault".to_string(), cat));
            }
        }
        // Leave a trace of what the model saw, for debugging odd answers.
        let mut log = format!("Q: {question}\n\n");
        for (i, (t, x)) in chunks.iter().enumerate() {
            log.push_str(&format!("--- [{}] {t} ({} words)\n{x}\n\n", i + 1, x.split_whitespace().count()));
        }
        let _ = std::fs::write(crate::config::data_dir().join("last_ask.txt"), &log);
        Ok(chunks)
    }

    /// Ask mode as a plain call (the MCP `ask` tool): retrieval + generation on the
    /// caller's thread. Returns the answer and the titles it drew on.
    pub fn answer_blocking(&self, question: &str) -> Result<(String, Vec<String>), String> {
        let path = self.path().ok_or("No model found next to blackhole.exe; ask mode is off (search still works).")?;
        let mut guard = self.llm.lock().unwrap();
        self.drop_if_stale(&mut guard);
        if guard.is_none() {
            *guard = Some(Llm::load(&path).map_err(|e| format!("Could not load model: {e}"))?);
        }
        let llm = guard.as_mut().unwrap();
        let chunks = self.gather(llm, question)?;
        let sources: Vec<Source> = chunks.iter().map(|(t, x)| Source { title: t, text: x }).collect();
        let answer = llm.answer(question, &sources, &AtomicBool::new(false), |_| {}).map_err(|e| e.to_string())?;
        let mut docs: Vec<String> = Vec::new();
        for (t, _) in &chunks {
            if !docs.contains(t) {
                docs.push(t.clone());
            }
        }
        Ok((answer.trim().to_string(), docs))
    }

    fn run(&self, question: String, hwnd: usize, id: u64, cancel: Arc<AtomicBool>) {
        let post = |msg: u32, text: String| unsafe {
            let boxed = Box::into_raw(Box::new(text));
            let _ = PostMessageW(Some(HWND(hwnd as *mut _)), msg, WPARAM(id as usize), LPARAM(boxed as isize));
        };

        let Some(path) = self.path() else {
            post(WM_ASK_DONE, "No model found. Settings \u{2192} \"Download the default model\" fetches one, or put an instruct ONNX folder next to blackhole.exe.".into());
            return;
        };

        // Only one generation at a time; a cancelled predecessor lets go quickly.
        let mut guard = self.llm.lock().unwrap();
        if cancel.load(Ordering::Relaxed) {
            return;
        }
        self.drop_if_stale(&mut guard);
        if guard.is_none() {
            post(WM_ASK_STATUS, "loading model…".into());
            match Llm::load(&path) {
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
        let chunks = match self.gather(llm, &question) {
            Ok(c) => c,
            Err(msg) => {
                post(WM_ASK_DONE, msg);
                return;
            }
        };
        let sources: Vec<Source> = chunks.iter().map(|(t, x)| Source { title: t, text: x }).collect();
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

    /// Right-click → Self-check: run a set of questions through the real pipeline and
    /// report how many answers contained what they should.
    ///
    /// `<data>\questions.json` (`[{"q": …, "expect": ["substring", …]}, …]`) is checked
    /// against the user's own vault; without that file a built-in five-question set runs
    /// against a scratch vault built for the occasion, so the answer is known and nothing
    /// depends on what the user happens to have swallowed. A question passes when any of
    /// its expected substrings occurs in the answer (they are alternative phrasings).
    /// With no model installed the check still runs, scoring the retrieved excerpts
    /// instead of an answer — that half of the pipeline is what it mostly tests anyway.
    pub fn self_check(&self) -> SelfCheck {
        let started = Instant::now();
        let (questions, scratch, source) = match load_questions() {
            Some(qs) => {
                let n = qs.len();
                (qs, None, format!("{n} questions from questions.json"))
            }
            None => match self.scratch_vault() {
                Ok(store) => (sanity_questions(), Some(store), "built-in sanity set over a scratch vault".to_string()),
                Err(e) => return SelfCheck { total: 0, passed: 0, mode: "failed", source: format!("could not build a scratch vault: {e}"), report: None, secs: 0.0 },
            },
        };
        let store: &Mutex<Store> = scratch.as_ref().unwrap_or(&self.store);
        // One model load for the whole run (if there is one at all).
        let mut guard = self.llm.lock().unwrap();
        self.drop_if_stale(&mut guard);
        if guard.is_none() {
            if let Some(path) = self.path() {
                match Llm::load(&path) {
                    Ok(l) => *guard = Some(l),
                    Err(e) => crate::util::log(&format!("self-check: no model ({e})")),
                }
            }
        }
        let mode = if guard.is_some() { "answers" } else { "retrieval only — no model" };
        let mut passed = 0;
        let mut report = format!("Blackhole self-check — {source}, {mode}\n\n");
        for (question, expect) in &questions {
            let text = match guard.as_mut() {
                Some(llm) => match self.gather_from(store, Some(llm), question) {
                    Ok(chunks) => {
                        let sources: Vec<Source> = chunks.iter().map(|(t, x)| Source { title: t, text: x }).collect();
                        llm.answer(question, &sources, &AtomicBool::new(false), |_| {}).unwrap_or_else(|e| format!("error: {e}"))
                    }
                    Err(msg) => msg,
                },
                None => match self.gather_from(store, None, question) {
                    Ok(chunks) => chunks.iter().map(|(t, x)| format!("{t}: {x}")).collect::<Vec<_>>().join("\n"),
                    Err(msg) => msg,
                },
            };
            let hay = text.to_lowercase();
            let ok = expect.iter().any(|e| hay.contains(&e.to_lowercase()));
            if ok {
                passed += 1;
            }
            let flat: String = text.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(240).collect();
            report.push_str(&format!("{} {question}\n   expected any of {expect:?}\n   got: {flat}\n\n", if ok { "PASS" } else { "FAIL" }));
        }
        drop(guard);
        let secs = started.elapsed().as_secs_f32();
        report.push_str(&format!("{passed}/{} in {secs:.1}s\n", questions.len()));
        if let Some(scratch) = scratch {
            drop(scratch); // close the scratch vault before deleting it
            let _ = std::fs::remove_dir_all(crate::config::data_dir().join("selfcheck"));
        }
        let path = crate::config::data_dir().join("selfcheck.txt");
        let written = std::fs::write(&path, &report).is_ok();
        crate::util::log(&format!("self-check: {passed}/{} ({mode}) in {secs:.1}s", questions.len()));
        SelfCheck { total: questions.len(), passed, mode, source, report: written.then_some(path), secs }
    }

    /// A small synthetic vault with known answers, embedded like anything else.
    fn scratch_vault(&self) -> Result<Mutex<Store>, String> {
        let dir = crate::config::data_dir().join("selfcheck");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let store = Mutex::new(Store::open(&dir.join("vault.db")).map_err(|e| e.to_string())?);
        for (i, (title, body)) in SANITY_ITEMS.iter().enumerate() {
            let id = store.lock().unwrap().add(title, "text", None, body, &format!("selfcheck-{i}"), crate::util::now_secs()).map_err(|e| e.to_string())?.ok_or("duplicate in the scratch vault")?;
            crate::ingest::embed_item(&store, &self.embedder, id, title, body);
        }
        Ok(store)
    }
}

/// What a self-check run found; the dot turns this into a bubble.
pub struct SelfCheck {
    pub total: usize,
    pub passed: usize,
    /// "answers", "retrieval only — no model", or "failed".
    pub mode: &'static str,
    pub source: String,
    pub report: Option<PathBuf>,
    pub secs: f32,
}

impl SelfCheck {
    /// One bubble's worth of result.
    pub fn summary(&self) -> String {
        if self.total == 0 {
            return format!("Self-check couldn't run: {}", self.source);
        }
        let mut s = format!("Self-check: {}/{} — {} ({}, {:.1}s)", self.passed, self.total, self.source, self.mode, self.secs);
        if self.report.is_some() {
            s.push_str("\nDetails in selfcheck.txt, beside the vault.");
        }
        s
    }
}

/// Five facts with unique names, so a wrong retrieval cannot accidentally pass.
const SANITY_ITEMS: [(&str, &str); 5] = [
    (
        "Zephyr invoice",
        "Invoice ZL-4471 from Zephyr Logistics. Customs entry for the vessel Tranquil Ace, voyage 0124A, arriving from the port of Kobe, Japan.\nTotal paid: 1240.00 USD by wire on 3 March.",
    ),
    (
        "Quillfeather resume",
        "Ada Quillfeather.\nPrincipal MLOps Engineer at Maxar Technologies, 2021 to present.\nStorage Solutions Architect at Raytheon, 2020 to 2021.\nSenior DevOps Engineer at Amazon Web Services, 2019 to 2020.",
    ),
    (
        "Bellhaven handbook",
        "Bellhaven Works staff handbook. Vacation policy: twenty-four days a year, carried over only with written approval.\nThe office manager approves expense claims under 500 USD.",
    ),
    (
        "Marrowgate repo notes",
        "The Marrowgate service is cloned over ssh from git@github.com:bellhaven/marrowgate.git and needs Rust 1.79 to build. Its config lives in /etc/marrowgate/config.toml.",
    ),
    (
        "Thistledown recipe",
        "Thistledown pie: four apples, 200 g butter, two spoons of cinnamon. Bake at 180 C for forty minutes. Serves six people.",
    ),
];

/// The built-in questions, each with the words a right answer contains.
fn sanity_questions() -> Vec<(String, Vec<String>)> {
    [
        ("How much was the Zephyr invoice for?", vec!["1240", "1,240"]),
        ("Which vessel did the customs entry cover?", vec!["Tranquil Ace", "Tranquil"]),
        ("Where did Ada Quillfeather work before Maxar?", vec!["Raytheon"]),
        ("How many vacation days does the Bellhaven handbook give?", vec!["twenty-four", "24"]),
        ("What temperature do I bake the Thistledown pie at?", vec!["180"]),
    ]
    .into_iter()
    .map(|(q, e): (&str, Vec<&str>)| (q.to_string(), e.into_iter().map(str::to_string).collect()))
    .collect()
}

/// `<data>\questions.json`: [{"q": "…", "expect": ["…"]}, …]. Absent or unreadable → None.
fn load_questions() -> Option<Vec<(String, Vec<String>)>> {
    #[derive(serde::Deserialize)]
    struct Item {
        q: String,
        #[serde(default)]
        expect: Vec<String>,
    }
    let path = crate::config::data_dir().join("questions.json");
    let text = std::fs::read_to_string(path).ok()?;
    let items: Vec<Item> = serde_json::from_str(&text).ok()?;
    let out: Vec<(String, Vec<String>)> = items.into_iter().filter(|i| !i.q.trim().is_empty() && !i.expect.is_empty()).map(|i| (i.q, i.expect)).collect();
    (!out.is_empty()).then_some(out)
}
