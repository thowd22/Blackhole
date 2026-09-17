#![allow(dead_code)]
//! Console bench: load an ONNX LLM on DirectML and time prompt + decode.
//! Usage: ortllm <model.onnx> [prompt words]

#[path = "../config.rs"] mod config;
#[path = "../hotkeys.rs"] mod hotkeys;
#[path = "../theme.rs"] mod theme;
#[path = "../util.rs"] mod util;
#[path = "../runtime.rs"] mod runtime;
#[path = "../gpu.rs"] mod gpu;
#[path = "../models.rs"] mod models;
#[path = "../llm_ort.rs"] mod llm_ort;

use std::io::Write;
use std::sync::atomic::AtomicBool;
use std::time::Instant;

fn main() -> anyhow::Result<()> {
    let model = std::env::args().nth(1).expect("model path");
    let words: usize = std::env::args().nth(2).and_then(|s| s.parse().ok()).unwrap_or(900);
    let dir = runtime::init()?;
    println!("runtime from {}", dir.display());
    let t = Instant::now();
    let mut llm = llm_ort::Llm::load(std::path::Path::new(&model))?;
    println!("loaded on {} in {:.1}s", llm.backend, t.elapsed().as_secs_f32());

    let filler: String = (0..words).map(|i| format!("word{} ", i % 50)).collect();
    let text = format!("Tyler Howd Maxar Technologies, Principal MLOps Engineer 10/2021-Pres. Amazon Web Services, Senior DevOps Engineer 4/2021-10/2021. Raytheon, Storage Solutions Architect 4/2020-4/2021. {filler}");
    let sources = [llm_ort::Source { title: "resume.pdf", text: &text }];
    let qs: Vec<String> = match std::env::args().nth(3) { Some(q) => vec![q], None => vec!["What is my most recent job title?".into(), "Where did I work before Maxar?".into()] };
    for q in qs.iter().map(String::as_str) {
        let t = Instant::now();
        let mut first = None;
        let mut n = 0;
        print!("\n> {q}\n");
        let ans = llm.answer(q, &sources, &AtomicBool::new(false), |tok| {
            if first.is_none() { first = Some(t.elapsed()); }
            n += 1;
            print!("{tok}");
            let _ = std::io::stdout().flush();
        })?;
        let total = t.elapsed();
        let ftt = first.unwrap_or(total);
        println!("\n-- first token {:.2}s, {} pieces in {:.2}s ({:.1} tok/s), {} chars", ftt.as_secs_f32(), n, total.as_secs_f32(), n as f32 / (total - ftt).as_secs_f32().max(0.01), ans.len());
    }
    Ok(())
}
