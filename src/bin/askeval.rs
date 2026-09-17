#![allow(dead_code)]
//! End-to-end ask-mode evaluation on Windows: the real pipeline (embed →
//! absent gate → expansion → reranked context → LLM) over a vault snapshot and
//! question files with expected-answer regexes.
//!
//! Usage: askeval <model.onnx> <vault-snapshot.db> <questions.json> [more.json…]
//! Prints one line per question and a summary (answer accuracy, retrieval
//! Hit@1, first-token latency, tokens/s, peak working set, model size on disk).
//!
//! Two model-free modes, for tuning retrieval without the LLM:
//!   askeval --rerank-bench [pairs]        cross-encoder cost per (query, passage)
//!                                         pair on this CPU, and the k it affords.
//!   askeval --absent <vault.db> <q>…      what the absent gate decides for each
//!                                         question, and why (terms, presence, cosine).
//!   askeval --scale <dir> <items.json> <questions.json>
//!                                         build a synthetic vault (eval/gen_vault.py)
//!                                         and measure live-search latency and Hit@1.

#[path = "../chunk.rs"]
mod chunk;
#[path = "../config.rs"]
mod config;
#[path = "../embed.rs"]
mod embed;
#[path = "../expand.rs"]
mod expand;
#[path = "../gpu.rs"]
mod gpu;
#[path = "../hotkeys.rs"]
mod hotkeys;
#[path = "../ingest.rs"]
mod ingest;
#[path = "../llm_ort.rs"]
mod llm_ort;
#[path = "../models.rs"]
mod models;
#[path = "../ocr.rs"]
mod ocr;
#[path = "../office.rs"]
mod office;
#[path = "../pdf_layout.rs"]
mod pdf_layout;
#[path = "../rerank.rs"]
mod rerank;
#[path = "../runtime.rs"]
mod runtime;
#[path = "../store.rs"]
mod store;
#[path = "../theme.rs"]
mod theme;
#[path = "../util.rs"]
mod util;
#[path = "../web.rs"]
mod web;

use std::path::{Path, PathBuf};
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

/// Per-pair cost of the cross-encoder on this machine, and the k a latency budget
/// buys. Pairs are the real shape: a question against a ~100-word passage, which
/// the tokenizer truncates to rerank::MAX_TOKENS anyway.
fn rerank_bench(pairs: usize) -> anyhow::Result<()> {
    runtime::init()?;
    let t = Instant::now();
    let reranker = rerank::Reranker::load()?;
    let load_ms = t.elapsed().as_secs_f32() * 1000.0;
    let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
    let query = "which document says what I paid for the customs entry and to whom";
    let filler: String = (0..90).map(|i| format!("word{} ", i % 37)).collect();
    let passages: Vec<String> = (0..pairs).map(|i| format!("Entry {i}: IMPORTING CARRIER: TRANQUIL ACE 0124A. FROM PORT OF: KOBE, JAPAN. total paid 1240.00 to Zephyr Logistics. {filler}")).collect();
    // Warm-up: the first run pays for arena allocation and shape compilation.
    reranker.score(query, &passages[..pairs.min(3)])?;
    let mut runs: Vec<f32> = Vec::new();
    for _ in 0..5 {
        let t = Instant::now();
        reranker.score(query, &passages)?;
        runs.push(t.elapsed().as_secs_f32() * 1000.0 / pairs as f32);
    }
    runs.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let median = runs[runs.len() / 2];
    let threads = cores.saturating_sub(2).clamp(2, 6);
    println!("reranker loaded in {load_ms:.0} ms | {cores} cores, {threads} intra-op threads | {pairs} pairs/run");
    println!("per pair: median {median:.1} ms (runs: {})", runs.iter().map(|r| format!("{r:.1}")).collect::<Vec<_>>().join(", "));
    for budget in [200.0f32, 300.0, 400.0, 600.0] {
        println!("  budget {budget:.0} ms -> k = {}", (budget / median).floor().max(1.0) as usize);
    }
    println!("k in this build: {} (rerank::candidates())", rerank::candidates());
    Ok(())
}

/// What the absent gate decides for each question, and the evidence behind it.
fn absent_report(db: &Path, questions: &[String]) -> anyhow::Result<()> {
    runtime::init()?;
    let embedder = embed::Embedder::load()?;
    let store = store::Store::open(db)?;
    println!("{:<44} {:>7} {:>9} {:>7}", "question", "terms", "best cos", "absent");
    let (mut absent_n, mut n) = (0, 0);
    for q in questions {
        let qvec = embedder.embed(q)?;
        let cos = store.best_cosine(&qvec);
        let absent = store.absent(q, Some(&qvec));
        n += 1;
        if absent {
            absent_n += 1;
        }
        let words = q.split_whitespace().count();
        println!("{:<44} {words:>7} {cos:>9.3} {:>7}", q.chars().take(44).collect::<String>(), if absent { "YES" } else { "no" });
    }
    println!("{absent_n}/{n} refused");
    Ok(())
}

#[derive(serde::Deserialize)]
struct ScaleItem {
    title: String,
    kind: String,
    text: String,
}
#[derive(serde::Deserialize)]
struct ScaleQ {
    q: String,
    expect_title: String,
}

/// Scale test: fill an isolated vault with the generator's items, then run the
/// questions through the panel's own live-search path (embed the query, hybrid
/// search) and report p50/p95 latency and Hit@1. No LLM is involved.
fn scale(dir: &Path, items_json: &Path, questions_json: &Path) -> anyhow::Result<()> {
    let items: Vec<ScaleItem> = serde_json::from_str(&std::fs::read_to_string(items_json)?)?;
    let questions: Vec<ScaleQ> = serde_json::from_str(&std::fs::read_to_string(questions_json)?)?;
    std::fs::create_dir_all(dir)?;
    let db = dir.join("vault.db");
    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{suffix}", db.display()));
    }
    runtime::init()?;
    let embedder = Arc::new(embed::Embedder::load()?);
    let store = Arc::new(Mutex::new(store::Store::open(&db)?));

    let t = Instant::now();
    let mut words = 0usize;
    for (i, it) in items.iter().enumerate() {
        words += it.text.split_whitespace().count();
        let id =
            store.lock().unwrap().add(&it.title, &it.kind, None, &it.text, &format!("scale-{i}"), util::now_secs() - (items.len() - i) as i64)?.ok_or_else(|| anyhow::anyhow!("duplicate item {i}"))?;
        ingest::embed_item(&store, &embedder, id, &it.title, &it.text);
        if (i + 1) % 50 == 0 {
            println!("  ingested {}/{} … {:.0}s", i + 1, items.len(), t.elapsed().as_secs_f32());
        }
    }
    let ingest_s = t.elapsed().as_secs_f32();
    let (n_items, n_chunks) = {
        let s = store.lock().unwrap();
        (s.count(), s.chunk_count())
    };
    println!("vault: {n_items} items, {n_chunks} chunks, {words} words — ingested + embedded in {ingest_s:.0}s ({} on {})", db.display(), embedder.backend);

    // Live search: exactly what the panel does per keystroke (search.rs), 40 hits.
    let mut embed_ms: Vec<f32> = Vec::new();
    let mut search_ms: Vec<f32> = Vec::new();
    let mut total_ms: Vec<f32> = Vec::new();
    let mut hit1 = 0;
    let mut hit3 = 0;
    for q in &questions {
        let t0 = Instant::now();
        let qvec = embedder.embed(&q.q)?;
        let t1 = Instant::now();
        let hits = store.lock().unwrap().search(&q.q, Some(&qvec), 40);
        let t2 = Instant::now();
        embed_ms.push((t1 - t0).as_secs_f32() * 1000.0);
        search_ms.push((t2 - t1).as_secs_f32() * 1000.0);
        total_ms.push((t2 - t0).as_secs_f32() * 1000.0);
        let top = hits.first().map(|h| h.title.clone()).unwrap_or_default();
        if top == q.expect_title {
            hit1 += 1;
        }
        if hits.iter().take(3).any(|h| h.title == q.expect_title) {
            hit3 += 1;
        } else {
            println!("  miss: {:?} -> {:?} (wanted {:?})", q.q, top, q.expect_title);
        }
    }
    let pct = |v: &mut Vec<f32>, p: f32| {
        v.sort_by(|a, b| a.partial_cmp(b).unwrap());
        v[((v.len() as f32 - 1.0) * p).round() as usize]
    };
    println!(
        "SCALE items={n_items} chunks={n_chunks} queries={} | embed p50 {:.1} ms p95 {:.1} ms | search p50 {:.1} ms p95 {:.1} ms | total p50 {:.1} ms p95 {:.1} ms | Hit@1 {hit1}/{} Hit@3 {hit3}/{}",
        questions.len(),
        pct(&mut embed_ms, 0.5),
        pct(&mut embed_ms, 0.95),
        pct(&mut search_ms, 0.5),
        pct(&mut search_ms, 0.95),
        pct(&mut total_ms, 0.5),
        pct(&mut total_ms, 0.95),
        questions.len(),
        questions.len()
    );
    Ok(())
}

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(|a| a == "--rerank-bench").unwrap_or(false) {
        return rerank_bench(args.get(2).and_then(|n| n.parse().ok()).unwrap_or(10));
    }
    if args.get(1).map(|a| a == "--absent").unwrap_or(false) {
        let db = args.get(2).map(PathBuf::from).ok_or_else(|| anyhow::anyhow!("usage: askeval --absent <vault.db> <question>…"))?;
        return absent_report(&db, &args[3..]);
    }
    if args.get(1).map(|a| a == "--scale").unwrap_or(false) {
        if args.len() < 5 {
            anyhow::bail!("usage: askeval --scale <vault-dir> <items.json> <questions.json>");
        }
        return scale(Path::new(&args[2]), Path::new(&args[3]), Path::new(&args[4]));
    }
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
                (
                    absent,
                    if absent { Vec::new() } else { s.context_reranked(&q.question, &aux, &qvec, std::env::var("BLACKHOLE_CTX_WORDS").ok().and_then(|v| v.parse().ok()).unwrap_or(1400), Some(&hook)) },
                )
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
                if ok {
                    absent_ok += 1;
                }
            } else {
                if ok {
                    correct += 1;
                }
                if top_hit {
                    hit1 += 1;
                }
                ttft_sum += ttft;
                if tps > 0.0 {
                    tps_sum += tps;
                    tps_n += 1;
                }
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
    println!(
        "SUMMARY model={} answers {}/{} retrieval-hit1 {}/{} absent {}/{} mean-ttft {:.2}s mean-tok/s {:.1} peak-ws {:.0} MB disk {:.0} MB load {:.1}s",
        llm.name,
        correct,
        answerable,
        hit1,
        answerable,
        absent_ok,
        absent_total,
        ttft_sum / answerable.max(1) as f32,
        if tps_n > 0 { tps_sum / tps_n as f32 } else { 0.0 },
        working_set_mb(),
        size_mb,
        load_s
    );
    Ok(())
}
