#![allow(dead_code)]
//! End-to-end ask-mode evaluation on Windows: the real pipeline (embed →
//! absent gate → expansion → reranked context → LLM) over a vault snapshot and
//! question files with expected-answer regexes.
//!
//! Usage: askeval <model.onnx> <vault-snapshot.db> <questions.json> [more.json…]
//! Prints one line per question and a summary (answer accuracy, retrieval
//! Hit@1, first-token latency, tokens/s, peak working set, model size on disk).

#[path = "../config.rs"] mod config;
#[path = "../util.rs"] mod util;
#[path = "../runtime.rs"] mod runtime;
#[path = "../gpu.rs"] mod gpu;
#[path = "../embed.rs"] mod embed;
#[path = "../chunk.rs"] mod chunk;
#[path = "../store.rs"] mod store;
#[path = "../expand.rs"] mod expand;
#[path = "../rerank.rs"] mod rerank;
#[path = "../llm_ort.rs"] mod llm_ort;
#[path = "../ingest.rs"] mod ingest;
#[path = "../pdf_layout.rs"] mod pdf_layout;
#[path = "../ocr.rs"] mod ocr;
#[path = "../hotkeys.rs"] mod hotkeys;
#[path = "../theme.rs"] mod theme;

use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::Instant;

#[derive(serde::Deserialize)]
struct Q {
    id: String,
    question: String,
    #[serde(rename = "type")]
    kind: String,
    expected_items: Vec<i64>,
    must_match_regex: Option<String>,
}
#[derive(serde::Deserialize)]
struct QFile {
    questions: Vec<Q>,
}

fn working_set_mb() -> f64 {
    use windows::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
    use windows::Win32::System::Threading::GetCurrentProcess;
    unsafe {
        let mut c = PROCESS_MEMORY_COUNTERS::default();
        let _ = GetProcessMemoryInfo(GetCurrentProcess(), &mut c, std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32);
        c.PeakWorkingSetSize as f64 / 1_048_576.0
    }
}

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 4 {
        eprintln!("usage: askeval <model.onnx> <vault.db> <questions.json>...");
        std::process::exit(2);
    }
    let model = Path::new(&args[1]);
    let db = Path::new(&args[2]);
    runtime::init()?;
    let embedder = Arc::new(embed::Embedder::load()?);
    // Work on a private copy so back-filling title units never touches the snapshot.
    let work = std::env::temp_dir().join("askeval-vault.db");
    std::fs::copy(db, &work)?;
    let store = Arc::new(Mutex::new(store::Store::open(&work)?));
    let backlog = store.lock().unwrap().unembedded(); // guard dropped before embedding re-locks
    for (id, title, content) in backlog {
        ingest::embed_item(&store, &embedder, id, &title, &content);
    }
    let reranker = rerank::Reranker::load()?;
    let t = Instant::now();
    let mut llm = llm_ort::Llm::load(model)?;
    let load_s = t.elapsed().as_secs_f32();
    let size_mb = llm_ort::model_size(model) as f64 / 1_048_576.0;
    println!("model {} | {:.0} MB on disk | loaded in {load_s:.1}s on {} | embeddings on {}", llm.name, size_mb, llm.backend, embedder.backend);

    let (mut n, mut correct, mut hit1, mut absent_ok, mut absent_total) = (0, 0, 0, 0, 0);
    let (mut ttft_sum, mut tps_sum, mut tps_n) = (0.0f32, 0.0f32, 0);
    for qf in &args[3..] {
        let file: QFile = serde_json::from_str(&std::fs::read_to_string(qf)?)?;
        for q in file.questions {
            let expansions: Vec<&str> = {
                let s = store.lock().unwrap();
                expand::expand(&q.question, |t| s.contains_term(t))
            };
            let mut qvec = embedder.embed(&expand::dense_query(&q.question, &expansions))?;
            let t_rw = Instant::now();
            let rewrites = if std::env::var("BLACKHOLE_REWRITE").map(|v| v == "1").unwrap_or(false) { llm.search_terms(&q.question) } else { Vec::new() };
            let rewrite_s = t_rw.elapsed().as_secs_f32();
            if !rewrites.is_empty() {
                let mut acc: Vec<f32> = qvec.iter().map(|v| v * 2.0).collect();
                for r in &rewrites {
                    if let Ok(v) = embedder.embed(r) {
                        for (a, b) in acc.iter_mut().zip(v) {
                            *a += b;
                        }
                    }
                }
                let norm = acc.iter().map(|v| v * v).sum::<f32>().sqrt().max(1e-6);
                qvec = acc.iter().map(|v| v / norm).collect();
            }
            let aux = rewrites.join(" ");
            let (absent, chunks) = {
                let s = store.lock().unwrap();
                let absent = s.absent(&q.question, Some(&qvec)) && !s.any_term_present(&aux);
                let hook = |qq: &str, ps: &[String]| reranker.score(qq, ps).ok();
                (absent, if absent { Vec::new() } else { s.context_reranked(&q.question, &aux, &qvec, std::env::var("BLACKHOLE_CTX_WORDS").ok().and_then(|v| v.parse().ok()).unwrap_or(1400), Some(&hook)) })
            };
            let is_absent_q = q.kind == "absent" || q.expected_items.is_empty();
            let top_title = chunks.first().map(|c| c.0.clone()).unwrap_or_default();
            let top_hit = {
                let s = store.lock().unwrap();
                q.expected_items.iter().any(|id| s.title_of(*id).map(|t| t == top_title).unwrap_or(false))
            };
            let mut chunks = chunks;
            if llm_ort::Llm::is_document_question(&q.question) {
                let cat = store.lock().unwrap().catalog(30);
                if !cat.is_empty() {
                    chunks.push(("List of everything in the vault".to_string(), cat));
                }
            }
            let (answer, ttft, tps) = if absent {
                ("[absent gate] That isn't in your vault.".to_string(), 0.0, 0.0)
            } else {
                let sources: Vec<llm_ort::Source> = chunks.iter().map(|(t, x)| llm_ort::Source { title: t, text: x }).collect();
                if std::env::var_os("BLACKHOLE_LLM_DEBUG").is_some() {
                    // What the model saw, per question, beside the exe (personal data: never committed).
                    let mut dump = format!("Q: {}\n\n", q.question);
                    for (i, (t, x)) in chunks.iter().enumerate() {
                        dump.push_str(&format!("--- [{}] {t} ({} words)\n{x}\n\n", i + 1, x.split_whitespace().count()));
                    }
                    let dir = std::env::current_exe().ok().and_then(|p| p.parent().map(|d| d.to_path_buf())).unwrap_or_default();
                    let _ = std::fs::write(dir.join(format!("context-{}.txt", q.id)), dump);
                }
                let t0 = Instant::now();
                let mut first: Option<f32> = None;
                let mut pieces = 0;
                let ans = llm.answer(&q.question, &sources, &AtomicBool::new(false), |_| {
                    if first.is_none() {
                        first = Some(t0.elapsed().as_secs_f32());
                    }
                    pieces += 1;
                })?;
                let total = t0.elapsed().as_secs_f32();
                let ttft = first.unwrap_or(total);
                let tps = if total > ttft { (pieces as f32 - 1.0).max(0.0) / (total - ttft) } else { 0.0 };
                (ans, ttft, tps)
            };
            let ok = match &q.must_match_regex {
                Some(rx) => regex::Regex::new(rx).map(|r| r.is_match(&answer)).unwrap_or(false),
                None => false,
            };
            n += 1;
            if is_absent_q {
                absent_total += 1;
                if ok { absent_ok += 1; }
            } else {
                if ok { correct += 1; }
                if top_hit { hit1 += 1; }
                ttft_sum += ttft;
                if tps > 0.0 { tps_sum += tps; tps_n += 1; }
            }
            let flat: String = answer.replace('\n', " ⏎ ").chars().take(150).collect();
            println!("{} {} [{}] doc:{} ttft {:.1}s {:.0}tok/s | {}", if ok { "✓" } else { "✗" }, q.id, q.kind, if top_hit { "✓" } else { "✗" }, ttft, tps, flat);
            if !rewrites.is_empty() {
                println!("   rewrites ({rewrite_s:.2}s): {rewrites:?}");
            }
            use std::io::Write;
            let _ = std::io::stdout().flush();
            if std::env::var_os("BLACKHOLE_LLM_DEBUG").is_some() {
                util::log(&format!("answer {}: {}", q.id, flat));
            }
        }
    }
    let answerable = n - absent_total;
    println!("SUMMARY model={} answers {}/{} retrieval-hit1 {}/{} absent {}/{} mean-ttft {:.2}s mean-tok/s {:.1} peak-ws {:.0} MB disk {:.0} MB load {:.1}s",
        llm.name, correct, answerable, hit1, answerable, absent_ok, absent_total,
        ttft_sum / answerable.max(1) as f32, if tps_n > 0 { tps_sum / tps_n as f32 } else { 0.0 }, working_set_mb(), size_mb, load_s);
    Ok(())
}
