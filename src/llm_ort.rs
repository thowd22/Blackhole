//! Ask mode on ONNX Runtime: Qwen2.5-Instruct exported to ONNX (int4 weights,
//! fp32 at the boundary). The graph is the standard HF decoder layout with a
//! growing `past_key_values` cache that we carry between steps.
//!
//! Hybrid execution: the prompt pass (hundreds of tokens, one big batch) runs
//! on DirectML where it is 5–6× faster than the CPU; decoding (one token at a
//! time) runs on the CPU provider, because DirectML's per-op dispatch cost on
//! this dynamic-shape graph makes it slower than the CPU for single tokens.
//! The cache produced by the GPU pass is bound straight to CPU memory.

use ort::ep::{DirectML, ExecutionProvider, CPU};
use ort::memory::{AllocationDevice, Allocator, AllocatorType, MemoryInfo, MemoryType};
use ort::session::builder::SessionBuilder;
use ort::session::Session;
use ort::AsPointer;
use ort::value::{DynValue, Tensor};
use std::borrow::Cow;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use tokenizers::Tokenizer;

static TOKENIZER_QWEN2: &[u8] = include_bytes!("../models/qwen2.5/tokenizer.json");

/// Chat-template families, detected from the tokenizer's special tokens so any
/// instruct export can be dropped next to the exe with its `tokenizer.json`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Family {
    /// Qwen2.5 / Qwen3 / most others: <|im_start|>role\n…<|im_end|>
    ChatMl,
    /// Llama 3.x: <|start_header_id|>role<|end_header_id|>…<|eot_id|>
    Llama3,
    /// Phi-3.5 / Phi-4-mini: <|role|>…<|end|>
    Phi,
    /// Gemma: <start_of_turn>role\n…<end_of_turn>
    Gemma,
}

/// Special-token strings that end a turn, per family; resolved to ids from the tokenizer.
const STOP_TOKENS: &[&str] = &["<|im_end|>", "<|endoftext|>", "<|eot_id|>", "<|end_of_text|>", "<|end|>", "<end_of_turn>", "<eos>"];
const MAX_NEW_TOKENS: usize = 260;
/// Thinking budget in tokens for models with a `<think>` mode (Qwen3); 0 = answer
/// directly. `BLACKHOLE_THINK` overrides. When the budget runs out the think block is
/// closed for the model and it answers from what it has.
const THINK_BUDGET: usize = 128;
/// Runtime switch (the right-click menu's "Think before answering"); env still overrides.
static THINK_ENABLED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);
pub fn set_thinking(on: bool) {
    THINK_ENABLED.store(on, Ordering::Relaxed);
}
fn think_budget() -> usize {
    if let Some(b) = std::env::var("BLACKHOLE_THINK").ok().and_then(|v| v.parse().ok()) {
        return b;
    }
    if THINK_ENABLED.load(Ordering::Relaxed) { THINK_BUDGET } else { 0 }
}
/// GPU-resident cache capacity (tokens) for the GPU-decode mode; BLACKHOLE_KV_CAP overrides.
const GPU_KV_CAPACITY: usize = 4096;

fn kv_capacity() -> usize {
    std::env::var("BLACKHOLE_KV_CAP").ok().and_then(|v| v.parse().ok()).unwrap_or(GPU_KV_CAPACITY)
}
/// Width of the static-shape decode session (`BLACKHOLE_SEQ`). Padding a step
/// out to a wider shape corrupts DirectML's GroupQueryAttention, so this is 1:
/// each decode step is exactly one token, and the prompt goes through the
/// dynamic-shape session instead.
const GPU_STATIC_SEQ: usize = 1;
fn static_seq() -> usize {
    std::env::var("BLACKHOLE_SEQ").ok().and_then(|v| v.parse().ok()).filter(|&n| n > 0).unwrap_or(GPU_STATIC_SEQ)
}
const PREFILL_TIMELINE: &str = "Timeline:\n";

/// An excerpt handed to the model as grounding.
pub struct Source<'a> {
    pub title: &'a str,
    pub text: &'a str,
}

pub struct Llm {
    family: Family,
    /// Qwen3-style models: an empty think block selects non-thinking mode.
    no_think: bool,
    /// Token id of `</think>` when the model has a thinking mode.
    end_think: Option<u32>,
    eos: Vec<u32>,
    /// Decode session (CPU provider); also does the prompt pass when there is no GPU.
    cpu: Session,
    /// Prompt-pass session on DirectML, if a GPU is available.
    gpu: Option<Session>,
    tok: Tokenizer,
    layers: usize,
    kv_heads: usize,
    head_dim: usize,
    /// HF-style exports take explicit `position_ids`; GenAI exports derive positions from the mask.
    wants_position_ids: bool,
    /// fp16 exports carry the KV cache as float16.
    kv_f16: bool,
    /// Decode on the GPU too (cache stays resident there); the CPU session is not loaded.
    gpu_decode: bool,
    /// The graph takes the position whose logits to return (`tools/logit_index.py`).
    has_logit_index: bool,
    /// Static-shape session: every step is exactly this many tokens (0 = dynamic shapes).
    seq: usize,
    /// During a padded prompt pass: how many of the step's tokens are real (logits come
    /// from the last real one; padding follows and is overwritten by the first decode step).
    prompt_live: Option<usize>,
    gpu_device: i32,
    /// Owns the GPU cache buffers' memory; must outlive them (freeing through a
    /// dropped allocator crashes), so it lives as long as the model.
    gpu_alloc: Option<Allocator>,
    pub backend: &'static str,
    #[allow(dead_code)]
    pub name: String,
}

fn ok<T, E: std::fmt::Display>(r: Result<T, E>) -> anyhow::Result<T> {
    r.map_err(|e| anyhow::anyhow!("{e}"))
}

/// Size of a model = graph file + every external data file beside it
/// (`<name>.onnx.data`, `<name>.onnx_data`, `<name>.onnx_data_1`, …).
pub fn model_size(p: &Path) -> u64 {
    let Some(stem) = p.file_name().map(|n| n.to_string_lossy().into_owned()) else { return 0 };
    let dir = match p.parent() {
        Some(d) if !d.as_os_str().is_empty() => d,
        _ => Path::new("."),
    };
    let Ok(rd) = std::fs::read_dir(dir) else { return 0 };
    rd.flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with(&stem))
        .map(|e| e.metadata().map(|m| m.len()).unwrap_or(0))
        .sum()
}

/// A model's tokenizer: `tokenizer.json` beside it, else the built-in Qwen2.5 one.
fn tokenizer_for(path: &Path) -> anyhow::Result<Tokenizer> {
    let beside = path.parent().unwrap_or(Path::new(".")).join("tokenizer.json");
    let bytes = std::fs::read(&beside).unwrap_or_else(|_| TOKENIZER_QWEN2.to_vec());
    Tokenizer::from_bytes(&bytes).map_err(|e| anyhow::anyhow!("{e}"))
}

fn detect_family(tok: &Tokenizer) -> Family {
    let has = |t: &str| tok.token_to_id(t).is_some();
    if has("<|start_header_id|>") {
        Family::Llama3
    } else if has("<start_of_turn>") {
        Family::Gemma
    } else if has("<|im_start|>") {
        Family::ChatMl
    } else if has("<|user|>") {
        Family::Phi
    } else {
        Family::ChatMl
    }
}

/// Find an ONNX LLM: next to the exe, then in the vault folder. Largest model wins.
pub fn find_model(extra_dir: &Path) -> Option<PathBuf> {
    let mut dirs = vec![extra_dir.to_path_buf()];
    if let Ok(exe) = std::env::current_exe() {
        if let Some(d) = exe.parent() {
            dirs.insert(0, d.to_path_buf());
        }
    }
    let mut best: Option<(u64, PathBuf)> = None;
    for d in dirs {
        // The folder itself and one level of subfolders (one folder per model keeps
        // a model's graph, external data and tokenizer.json together).
        let Ok(rd) = std::fs::read_dir(&d) else { continue };
        let mut places = vec![d.clone()];
        places.extend(rd.flatten().map(|e| e.path()).filter(|p| p.is_dir()));
        for place in places {
            let Ok(rd) = std::fs::read_dir(&place) else { continue };
            for e in rd.flatten() {
                let p = e.path();
                if p.extension().map(|x| x.eq_ignore_ascii_case("onnx")).unwrap_or(false) {
                    let size = model_size(&p);
                    if best.as_ref().map(|b| size > b.0).unwrap_or(true) {
                        best = Some((size, p));
                    }
                }
            }
        }
        if best.is_some() {
            break;
        }
    }
    best.map(|b| b.1)
}

/// DirectML on the adapter chosen by `gpu::preferred()`.
pub fn dml_provider(device: i32) -> DirectML {
    DirectML::default().with_device_id(device)
}

/// Memory knobs, comma-separated in BLACKHOLE_LLM_OPTS: `arena0` (no CPU arena),
/// `prepack0` (no weight prepacking), `pattern0` (no memory pattern), `devinit`
/// (initializers allocated on the device for the DirectML session).
fn opt(name: &str) -> bool {
    std::env::var("BLACKHOLE_LLM_OPTS").map(|v| v.split(',').any(|o| o.trim() == name)).unwrap_or(false)
}

/// Pin a symbolic input dimension so DirectML can fuse the graph at load time.
fn override_dim(b: &mut SessionBuilder, name: &str, value: i64) -> anyhow::Result<()> {
    let cname = std::ffi::CString::new(name)?;
    let api = ort::api();
    let status = unsafe { (api.AddFreeDimensionOverrideByName)(b.ptr_mut(), cname.as_ptr(), value) };
    if !status.0.is_null() {
        let msg = unsafe { std::ffi::CStr::from_ptr((api.GetErrorMessage)(status.0)) }.to_string_lossy().into_owned();
        unsafe { (api.ReleaseStatus)(status.0) };
        anyhow::bail!("free dimension override {name}={value}: {msg}");
    }
    Ok(())
}

/// `static_shapes`: Some((seq, capacity)) pins every symbolic dimension. With
/// dynamic shapes DirectML dispatches the ~400 decoder ops one by one (~0.4 ms of
/// CPU work each); with static shapes it compiles the layer stack into one fused
/// operator and a decode step costs a single dispatch.
fn gpu_session(path: &Path, device: i32, static_shapes: Option<(usize, usize)>) -> anyhow::Result<Session> {
    let b = ok(Session::builder())?;
    let b = ok(b.with_execution_providers([dml_provider(device).build().error_on_failure()]))?;
    let mut b = ok(b.with_memory_pattern(false))?;
    if let Some((seq, cap)) = static_shapes {
        override_dim(&mut b, "batch_size", 1)?;
        override_dim(&mut b, "sequence_length", seq as i64)?;
        override_dim(&mut b, "past_sequence_length", cap as i64)?;
        override_dim(&mut b, "total_sequence_length", cap as i64)?;
    }
    if opt("devinit") {
        b = ok(b.with_device_allocated_initializers())?;
    }
    // DirectML-specific session knobs (BLACKHOLE_LLM_OPTS): `capture` records the decode
    // step's command list once and replays it (needs static shapes and device-bound I/O),
    // `nofusion` turns off DML graph fusion, `spin` lets the CPU spin-wait on the GPU.
    if opt("capture") {
        b = ok(b.with_config_entry("ep.dml.enable_graph_capture", "1"))?;
    }
    if opt("nofusion") {
        b = ok(b.with_config_entry("ep.dml.disable_graph_fusion", "1"))?;
    }
    if opt("spin") {
        b = ok(b.with_config_entry("ep.dml.enable_cpu_sync_spinning", "1"))?;
    }
    // `profile`: per-node timings to <exe dir>\llm-profile*.json (ORT's chrome-trace format).
    if opt("profile") {
        let dir = std::env::current_exe().ok().and_then(|p| p.parent().map(|d| d.to_path_buf())).unwrap_or_default();
        b = ok(b.with_profiling(dir.join("llm-profile")))?;
    }
    ok(b.commit_from_file(path))
}

fn cpu_session(path: &Path) -> anyhow::Result<Session> {
    let mut b = ok(Session::builder())?;
    if opt("arena0") {
        b = ok(b.with_execution_providers([CPU::default().with_arena_allocator(false).build()]))?;
    }
    if opt("prepack0") {
        b = ok(b.with_prepacking(false))?;
    }
    if opt("pattern0") {
        b = ok(b.with_memory_pattern(false))?;
    }
    ok(b.commit_from_file(path))
}

impl Llm {
    pub fn load(path: &Path) -> anyhow::Result<Llm> {
        let force_cpu = std::env::var_os("BLACKHOLE_CPU").is_some();
        // Decode on DirectML too (default when a GPU is present: ~100 tok/s on a static-shape
        // session, no CPU session in memory). BLACKHOLE_DECODE=cpu keeps the old hybrid path.
        let want_gpu_decode = std::env::var("BLACKHOLE_DECODE").map(|v| v != "cpu").unwrap_or(true);
        let adapter = if force_cpu { None } else { crate::gpu::preferred() };
        // Graph-only files are small; a `logit_index` input marks a graph prepared for static shapes.
        let has_logit_index = std::fs::metadata(path).map(|m| m.len() < 64 << 20).unwrap_or(false)
            && std::fs::read(path).map(|b| b.windows(11).any(|w| w == b"logit_index")).unwrap_or(false);
        let seq = if want_gpu_decode && has_logit_index { static_seq() } else { 0 };
        let static_shapes = if seq > 0 { Some((seq, kv_capacity())) } else { None };
        let mut gpu_device = 0;
        let gpu = match adapter {
            Some(a) if DirectML::default().is_available().unwrap_or(false) => match gpu_session(path, a.index, None) {
                Ok(s) => {
                    crate::util::log(&format!("llm: prompt pass on DirectML adapter {} ({}, {} MB)", a.index, a.name, a.vram_mb));
                    gpu_device = a.index;
                    Some(s)
                }
                Err(e) => {
                    crate::util::log(&format!("llm: DirectML session failed, prompt pass on CPU: {e}"));
                    None
                }
            },
            _ => None,
        };
        let mut gpu = gpu;
        let gpu_decode = want_gpu_decode && gpu.is_some();
        // Pure GPU decode. With a static-shape graph the dynamic DirectML session runs the
        // prompt pass and a second, static one-token session decodes (~20x faster per step
        // than dynamic dispatch); both write the same GPU cache buffers. Without static
        // shapes the one dynamic session does both. Otherwise the CPU session decodes.
        let cpu = if gpu_decode && seq > 0 {
            gpu_session(path, gpu_device, static_shapes)?
        } else if gpu_decode {
            gpu.take().unwrap()
        } else {
            cpu_session(path)?
        };
        let gpu_alloc = if gpu_decode {
            let mem = ok(MemoryInfo::new(AllocationDevice::DIRECTML, 0, AllocatorType::Device, MemoryType::Default))?;
            Some(ok(Allocator::new(&cpu, mem))?)
        } else {
            None
        };
        let backend = if gpu_decode { "DirectML" } else if gpu.is_some() { "DirectML + CPU" } else { "CPU" };
        let _ = &gpu_device;
        let seq = if gpu_decode { seq } else { 0 };
        if seq > 0 {
            crate::util::log(&format!("llm: static shapes, {seq}-token steps, cache capacity {}", kv_capacity()));
        }
        let session = &cpu;
        // Cache geometry from the graph itself: count past_key_values.N.key inputs, read their dims.
        let mut layers = 0;
        let mut kv_heads = 2;
        let mut head_dim = 128;
        let mut wants_position_ids = false;
        let mut kv_f16 = false;
        for input in session.inputs() {
            let name = input.name();
            if name == "position_ids" {
                wants_position_ids = true;
            }
            if name == "past_key_values.0.key" {
                if let ort::value::ValueType::Tensor { ty, .. } = input.dtype() {
                    kv_f16 = *ty == ort::value::TensorElementType::Float16;
                }
            }
            if name.starts_with("past_key_values.") && name.ends_with(".key") {
                layers += 1;
                if let Some(dims) = input.dtype().tensor_shape() {
                    if dims.len() == 4 {
                        if dims[1] > 0 { kv_heads = dims[1] as usize; }
                        if dims[3] > 0 { head_dim = dims[3] as usize; }
                    }
                }
            }
        }
        if layers == 0 {
            anyhow::bail!("not a decoder with past_key_values inputs");
        }
        let tok = tokenizer_for(path)?;
        let family = detect_family(&tok);
        let no_think = family == Family::ChatMl && tok.token_to_id("<think>").is_some();
        let eos: Vec<u32> = STOP_TOKENS.iter().filter_map(|t| tok.token_to_id(t)).collect();
        let end_think = if no_think { tok.token_to_id("</think>") } else { None };
        crate::util::log(&format!("llm: {} layout, {family:?} template{}, {} stop tokens, kv {}", if wants_position_ids { "HF" } else { "GenAI" }, if no_think { " (no-think)" } else { "" }, eos.len(), if kv_f16 { "f16" } else { "f32" }));
        let name = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        Ok(Llm { family, no_think, end_think, eos, cpu, gpu, tok, layers, kv_heads, head_dim, wants_position_ids, kv_f16, gpu_decode, has_logit_index, seq, prompt_live: None, gpu_device, gpu_alloc, backend, name })
    }

    /// Questions about the files themselves rather than their contents: "which
    /// document…", "is there a note…", "do I have a paper about…".
    pub fn is_document_question(question: &str) -> bool {
        let q = question.to_lowercase();
        let words: Vec<&str> = q.split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).collect();
        let noun = words.iter().any(|w| matches!(*w, "file" | "files" | "doc" | "docs" | "document" | "documents" | "paper" | "papers" | "note" | "notes" | "pdf" | "pdfs" | "snippet" | "receipt"));
        let ask = q.starts_with("which") || q.starts_with("what file") || q.starts_with("is there") || q.starts_with("do i have") || q.starts_with("are there") || q.contains("which file") || q.contains("which doc");
        noun && ask
    }

    fn is_temporal(question: &str) -> bool {
        let q = question.to_lowercase();
        ["before", "after", "previous", "prior", "next", "recent", "latest", "first", "last", "earliest", "newest", "oldest", "when", "order", "current"]
            .iter()
            .any(|k| q.split(|c: char| !c.is_alphanumeric()).any(|w| w == *k))
    }

    fn prompt(&self, question: &str, sources: &[Source]) -> String {
        // The timeline scratchpad is prefilled for time/order questions only; telling the
        // model about it in general made it write one for every question.
        // With thinking on, the model orders dates in its private reasoning; the visible
        // Timeline scratchpad is only for direct mode.
        let temporal = Self::is_temporal(question) && !self.thinking();
        let system = if temporal {
            concat!(
                "You are Blackhole, a search assistant for the user's own files. Answer from the excerpts provided; do not mention excerpts or their numbers. ",
                "The question is about order or time: first list every relevant entry with its dates from newest to oldest under 'Timeline:', then answer on a new line starting with 'Answer:', reading the answer off the timeline. ",
                "If none of the excerpts answers the question, reply exactly: I couldn't find that in your files.",
            )
        } else {
            concat!(
                "You are Blackhole, a search assistant for the user's own files. Answer from the excerpts provided; do not mention excerpts or their numbers. ",
                "Answer directly in one or two plain sentences, quoting names, numbers and dates exactly as written. ",
                "If asked which file or document contains something, name it by its exact title, then give the key facts from it that answer the question. ",
                "Forms and tables appear as rows with ' | ' between columns; a label's value is in the same column of the next row. ",
                "If none of the excerpts answers the question, reply exactly: I couldn't find that in your files.",
            )
        };
        let mut user = String::new();
        for (i, s) in sources.iter().enumerate() {
            user.push_str(&format!("Excerpt {} (from {}):\n{}\n\n", i + 1, s.title, s.text.trim()));
        }
        user.push_str(&format!("Question: {}", question.trim()));
        let mut p = self.chat(system, &user);
        if temporal {
            p.push_str(PREFILL_TIMELINE);
        }
        p
    }

    /// Reasoning before answering: only for models with a think mode, and only when budgeted.
    pub fn thinking(&self) -> bool {
        self.end_think.is_some() && think_budget() > 0
    }

    /// One system + user turn in this model family's chat template, ready for the assistant.
    fn chat(&self, system: &str, user: &str) -> String {
        let mut p = match self.family {
            Family::ChatMl => format!("<|im_start|>system\n{system}<|im_end|>\n<|im_start|>user\n{user}<|im_end|>\n<|im_start|>assistant\n"),
            Family::Llama3 => format!(
                "<|begin_of_text|><|start_header_id|>system<|end_header_id|>\n\n{system}<|eot_id|><|start_header_id|>user<|end_header_id|>\n\n{user}<|eot_id|><|start_header_id|>assistant<|end_header_id|>\n\n"
            ),
            Family::Phi => format!("<|system|>{system}<|end|><|user|>{user}<|end|><|assistant|>"),
            // Gemma has no system role: fold the instructions into the user turn.
            Family::Gemma => format!("<bos><start_of_turn>user\n{system}\n\n{user}<end_of_turn>\n<start_of_turn>model\n"),
        };
        if self.no_think {
            // Empty think block = answer directly; an open one = reason first.
            p.push_str(if self.thinking() { "<think>\n" } else { "<think>\n\n</think>\n\n" });
        }
        p
    }

    /// Alternative search queries for retrieval: the words a file would literally
    /// contain for what the user asked colloquially ("ssh checkout address" →
    /// "git clone url", "github repository"). ~0.3 s on the GPU; empty on failure.
    pub fn search_terms(&mut self, question: &str) -> Vec<String> {
        let system = concat!(
            "You write search queries over a person's own files: documents, receipts, résumés, notes, code snippets, saved links. ",
            "Given a question, write 3 short alternative search queries, one per line, using the literal words, names, formats or jargon such a file would actually contain. ",
            "Output only the three queries.",
        );
        let prompt = self.chat(system, &format!("Question: {}", question.trim()));
        let out = match self.generate(prompt, None, 48, &AtomicBool::new(false), |_| {}) {
            Ok(o) => o,
            Err(_) => return Vec::new(),
        };
        let q_lower = question.to_lowercase();
        let mut terms: Vec<String> = Vec::new();
        for line in out.lines() {
            let t = line.trim().trim_start_matches(|c: char| c.is_ascii_digit() || matches!(c, '-' | '*' | '•' | '.' | ')' | ' ')).trim_matches(|c| matches!(c, '"' | '`' | '\'')).trim();
            let tl = t.to_lowercase();
            // Preambles ("Here are three alternative search queries:") and echoes of the question.
            if t.is_empty() || t.ends_with(':') || tl.contains("search quer") || t.split_whitespace().count() > 10 || tl == q_lower || terms.iter().any(|x| x == t) {
                continue;
            }
            terms.push(t.to_string());
            if terms.len() == 3 {
                break;
            }
        }
        terms
    }

    fn kv_name(i: usize, prefix: &str) -> String {
        format!("{prefix}.{}.{}", i / 2, if i % 2 == 0 { "key" } else { "value" })
    }

    /// Would appending `piece` complete a line identical to an earlier one?
    fn repeats_line(out: &str, piece: &str) -> bool {
        let combined = format!("{out}{piece}");
        let mut lines: Vec<&str> = combined.lines().map(str::trim).filter(|l| l.len() > 12).collect();
        let Some(last) = lines.pop() else { return false };
        // Only judge the line just completed (piece ended it).
        piece.ends_with('\n') && lines.iter().any(|l| *l == last)
    }

    /// Greedy pick over the last position's logits (fp32 or fp16 exports).
    fn argmax(logits: &DynValue) -> anyhow::Result<u32> {
        fn pick<T: Copy + Into<f32>>(shape: &[i64], data: &[T]) -> u32 {
            let vocab = *shape.last().unwrap_or(&(data.len() as i64)) as usize;
            let last = &data[data.len() - vocab..];
            last.iter().enumerate().fold((0usize, f32::NEG_INFINITY), |b, (i, &x)| { let x: f32 = x.into(); if x > b.1 { (i, x) } else { b } }).0 as u32
        }
        if let Ok((shape, data)) = logits.try_extract_tensor::<f32>() {
            return Ok(pick(shape, data));
        }
        let (shape, data) = ok(logits.try_extract_tensor::<half::f16>())?;
        Ok(pick(shape, data))
    }

    /// Fixed-capacity KV buffers on the GPU, bound as both `past` and `present`
    /// every step (GenAI's shared-buffer convention). DirectML's
    /// GroupQueryAttention writes the new rows in place; separately allocated,
    /// growing `present` outputs come back corrupted on DirectML.
    fn device_kv(&self) -> anyhow::Result<Vec<DynValue>> {
        let alloc = self.gpu_alloc.as_ref().ok_or_else(|| anyhow::anyhow!("no GPU allocator"))?;
        let shape = [1usize, self.kv_heads, kv_capacity(), self.head_dim];
        (0..self.layers * 2)
            .map(|_| {
                Ok(if self.kv_f16 {
                    ok(Tensor::<half::f16>::new(alloc, shape))?.into_dyn()
                } else {
                    ok(Tensor::<f32>::new(alloc, shape))?.into_dyn()
                })
            })
            .collect()
    }

    fn empty_past(&self) -> anyhow::Result<Vec<DynValue>> {
        let shape = [1usize, self.kv_heads, 0, self.head_dim];
        (0..self.layers * 2)
            .map(|_| {
                Ok(if self.kv_f16 {
                    ok(Tensor::<half::f16>::from_array((shape, Vec::<half::f16>::new())))?.into_dyn()
                } else {
                    ok(Tensor::<f32>::from_array((shape, Vec::<f32>::new())))?.into_dyn()
                })
            })
            .collect()
    }

    fn prompt_pass_gpu(
        gpu: &mut Session,
        layers: usize,
        wants_position_ids: bool,
        input_ids: &Tensor<i64>,
        mask: &Tensor<i64>,
        pos: &Tensor<i64>,
        logit_index: Option<&Tensor<i64>>,
        past: &[DynValue],
    ) -> anyhow::Result<(u32, Vec<DynValue>)> {
        let mut binding = ok(gpu.create_binding())?;
        ok(binding.bind_input("input_ids", input_ids))?;
        ok(binding.bind_input("attention_mask", mask))?;
        if wants_position_ids {
            ok(binding.bind_input("position_ids", pos))?;
        }
        if let Some(li) = logit_index {
            ok(binding.bind_input("logit_index", li))?;
        }
        for (i, kv) in past.iter().enumerate() {
            ok(binding.bind_input(Self::kv_name(i, "past_key_values"), kv))?;
        }
        // Everything comes back to the CPU: the decode loop runs there.
        let cpu_mem = ok(MemoryInfo::new(AllocationDevice::CPU, 0, AllocatorType::Device, MemoryType::Default))?;
        ok(binding.bind_output_to_device("logits", &cpu_mem))?;
        for i in 0..layers * 2 {
            ok(binding.bind_output_to_device(Self::kv_name(i, "present"), &cpu_mem))?;
        }
        let mut outputs = ok(gpu.run_binding(&binding))?;
        let next = Self::argmax(&outputs["logits"])?;
        let present = (0..layers * 2)
            .map(|i| outputs.remove(Self::kv_name(i, "present")).ok_or_else(|| anyhow::anyhow!("missing present output")))
            .collect::<anyhow::Result<Vec<_>>>()?;
        Ok((next, present))
    }

    /// One forward pass over `ids` at position `past_len`. The cache values in
    /// `past` are consumed and the new ones returned (always in CPU memory).
    /// The prompt pass (`past_len == 0`) goes to the GPU when there is one.
    fn step(&mut self, ids: &[u32], past_len: usize, past: Vec<DynValue>) -> anyhow::Result<(u32, Vec<DynValue>)> {
        let n = ids.len();
        let total = past_len + n;
        // Static-shape sessions see exactly `seq` tokens a step: the live ones first, then
        // padding. The pads land in cache rows past the live length, where the next step
        // overwrites them, and the causal mask keeps live tokens from ever attending to them.
        // The prompt pass (past_len == 0) uses the dynamic session when there is one.
        let dynamic_prompt = n > 1 && self.gpu_decode && self.gpu.is_some();
        let width = if self.seq > 0 && !dynamic_prompt { self.seq } else { n };
        if n > width {
            anyhow::bail!("{n} tokens in one step, static width is {width}");
        }
        let pad = self.eos.first().copied().unwrap_or(0) as i64;
        let mut ids64: Vec<i64> = ids.iter().map(|&i| i as i64).collect();
        ids64.resize(width, pad);
        let input_ids = ok(Tensor::from_array(([1usize, width], ids64)))?;
        let mask = ok(Tensor::from_array(([1usize, total], vec![1i64; total])))?;
        let pos = ok(Tensor::from_array(([1usize, width], (past_len..past_len + width).map(|p| p as i64).collect::<Vec<_>>())))?;
        let live = self.prompt_live.filter(|&l| l > 0 && l <= n).unwrap_or(n);
        let logit_index = ok(Tensor::from_array(([1usize], vec![live as i64 - 1])))?;
        let layers = self.layers;

        if self.gpu_decode {
            let dbg = std::env::var_os("BLACKHOLE_LLM_DEBUG").is_some();
            if dbg {
                crate::util::log(&format!("gpu-decode step: n={n} width={width} past_len={past_len} past_values={}", past.len()));
            }
            // Cache stays on the GPU between steps; only the logits come back.
            let session = if dynamic_prompt { self.gpu.as_mut().unwrap() } else { &mut self.cpu };
            let mut binding = ok(session.create_binding())?;
            ok(binding.bind_input("input_ids", &input_ids))?;
            if self.wants_position_ids {
                ok(binding.bind_input("position_ids", &pos))?;
            }
            if self.has_logit_index {
                ok(binding.bind_input("logit_index", &logit_index))?;
            }
            let cap = kv_capacity();
            let occupied = past_len + width;
            if occupied > cap {
                anyhow::bail!("{occupied} tokens exceed the GPU cache capacity of {cap}");
            }
            // Fixed-capacity mask (ones for the occupied positions, pads included so GQA appends
            // the step's rows after the live ones): GroupQueryAttention reads the live length
            // from ReduceSum(mask) and the buffer length from Shape(mask), so every step has the
            // same shapes.
            let mut full = vec![0i64; cap];
            full[..occupied].fill(1);
            let mask = ok(Tensor::from_array(([1usize, cap], full)))?;
            ok(binding.bind_input("attention_mask", &mask))?;
            // GenAI's shared-buffer convention: fixed-capacity cache buffers bound as BOTH past
            // and present, so GroupQueryAttention writes the new rows in place. bind_input keeps
            // a reference; bind_output takes the value and hands it back via outputs.remove().
            for (i, kv) in past.iter().enumerate() {
                ok(binding.bind_input(Self::kv_name(i, "past_key_values"), kv))?;
            }
            for (i, kv) in past.into_iter().enumerate() {
                ok(binding.bind_output(Self::kv_name(i, "present"), kv))?;
            }
            let cpu_mem = ok(MemoryInfo::new(AllocationDevice::CPU, 0, AllocatorType::Device, MemoryType::Default))?;
            let _ = self.gpu_device;
            ok(binding.bind_output_to_device("logits", &cpu_mem))?;
            if dbg {
                crate::util::log("gpu-decode: bound, running");
            }
            let mut outputs = ok(session.run_binding(&binding))?;
            if dbg {
                crate::util::log("gpu-decode: ran");
            }
            let next = Self::argmax(&outputs["logits"])?;
            let past = (0..layers * 2)
                .map(|i| outputs.remove(Self::kv_name(i, "present")).ok_or_else(|| anyhow::anyhow!("missing present output")))
                .collect::<anyhow::Result<Vec<_>>>()?;
            if dbg {
                crate::util::log(&format!("gpu-decode: next={next}"));
            }
            return Ok((next, past));
        }
        if past_len == 0 {
            if let Some(gpu) = self.gpu.as_mut() {
                match Self::prompt_pass_gpu(gpu, layers, self.wants_position_ids, &input_ids, &mask, &pos, self.has_logit_index.then_some(&logit_index), &past) {
                    Ok(r) => return Ok(r),
                    Err(e) => {
                        // A kernel this export doesn't support on DirectML: do the pass on the CPU from now on.
                        crate::util::log(&format!("llm: DirectML prompt pass failed, switching to CPU: {e}"));
                        self.gpu = None;
                    }
                }
            }
        }
        let mut inputs: Vec<(Cow<'static, str>, ort::session::SessionInputValue<'static>)> = Vec::with_capacity(3 + layers * 2);
        inputs.push(("input_ids".into(), input_ids.into()));
        inputs.push(("attention_mask".into(), mask.into()));
        if self.wants_position_ids {
            inputs.push(("position_ids".into(), pos.into()));
        }
        if self.has_logit_index {
            inputs.push(("logit_index".into(), logit_index.into()));
        }
        for (i, kv) in past.into_iter().enumerate() {
            inputs.push((Self::kv_name(i, "past_key_values").into(), kv.into()));
        }
        let mut outputs = ok(self.cpu.run(inputs))?;
        let next = Self::argmax(&outputs["logits"])?;
        let present = (0..layers * 2)
            .map(|i| outputs.remove(Self::kv_name(i, "present")).ok_or_else(|| anyhow::anyhow!("missing present output")))
            .collect::<anyhow::Result<Vec<_>>>()?;
        Ok((next, present))
    }

    /// Generate an answer, streaming decoded text through `on_token`.
    pub fn answer(
        &mut self,
        question: &str,
        sources: &[Source],
        cancel: &AtomicBool,
        on_token: impl FnMut(&str),
    ) -> anyhow::Result<String> {
        let prompt = self.prompt(question, sources);
        let prefill = (Self::is_temporal(question) && !self.thinking()).then_some(PREFILL_TIMELINE);
        let think = self.thinking();
        self.generate_ex(prompt, prefill, MAX_NEW_TOKENS, think, cancel, on_token)
    }

    /// Greedy generation from a finished prompt; `prefill` is text already in the
    /// prompt that counts as output (echoed to `on_token` first).
    fn generate(
        &mut self,
        prompt: String,
        prefill: Option<&str>,
        max_new: usize,
        cancel: &AtomicBool,
        on_token: impl FnMut(&str),
    ) -> anyhow::Result<String> {
        self.generate_ex(prompt, prefill, max_new, false, cancel, on_token)
    }

    /// `think`: the prompt ends inside an open `<think>` block; tokens up to `</think>`
    /// (or the budget, after which `</think>` is forced) are kept private, the rest streams.
    fn generate_ex(
        &mut self,
        prompt: String,
        prefill: Option<&str>,
        max_new: usize,
        think: bool,
        cancel: &AtomicBool,
        mut on_token: impl FnMut(&str),
    ) -> anyhow::Result<String> {
        let enc = self.tok.encode(prompt, false).map_err(|e| anyhow::anyhow!("{e}"))?;
        let prompt_ids: Vec<u32> = enc.get_ids().to_vec();
        if std::env::var_os("BLACKHOLE_LLM_DEBUG").is_some() {
            let head: Vec<String> = prompt_ids.iter().take(12).map(|i| i.to_string()).collect();
            let tail: Vec<String> = prompt_ids.iter().rev().take(8).rev().map(|i| i.to_string()).collect();
            crate::util::log(&format!("llm prompt: {} tokens, head [{}] tail [{}] eos {:?}", prompt_ids.len(), head.join(" "), tail.join(" "), self.eos));
        }

        let mut out = String::new();
        if let Some(pre) = prefill {
            on_token(pre);
            out.push_str(pre);
        }

        let mut past = if self.gpu_decode { self.device_kv()? } else { self.empty_past()? };
        let mut next = 0u32;
        let mut done = 0;
        // DirectML's GroupQueryAttention drops off its metacommand path when the prompt is
        // ~45–64 % of the cache capacity (10 s instead of 1 s for a 2,100-token prompt at
        // 4,096). Longer prompts are fast, so a prompt in the band is padded past it: real
        // tokens never attend the pads (causal), the logits come from the last real token,
        // and the first decode step overwrites the pads' cache rows.
        let real_len = prompt_ids.len();
        let mut prompt_ids = prompt_ids;
        if self.gpu_decode && self.gpu.is_some() && std::env::var("BLACKHOLE_PAD_BAND").map(|v| v != "0").unwrap_or(true) {
            let cap = kv_capacity() as f32;
            let (lo, hi) = ((cap * 0.42) as usize, (cap * 0.66) as usize);
            if real_len >= lo && real_len < hi {
                let target = (hi + 63) / 64 * 64;
                let pad = self.eos.first().copied().unwrap_or(0);
                prompt_ids.resize(target, pad);
                if std::env::var_os("BLACKHOLE_LLM_DEBUG").is_some() {
                    crate::util::log(&format!("llm prompt padded {real_len} -> {target} (metacommand band {lo}..{hi} at capacity {cap})"));
                }
            }
        }
        self.prompt_live = Some(real_len);
        // Dynamic shapes take the whole prompt in one pass; static ones in `seq`-token chunks.
        let mut chunk = if self.seq > 0 && !(self.gpu_decode && self.gpu.is_some()) { self.seq } else { prompt_ids.len().max(1) };
        // BLACKHOLE_PROMPT_CHUNK: feed the dynamic prompt session in pieces (experiment).
        if let Some(c) = std::env::var("BLACKHOLE_PROMPT_CHUNK").ok().and_then(|v| v.parse::<usize>().ok()).filter(|&c| c > 0) {
            if self.gpu_decode && self.gpu.is_some() {
                chunk = c;
            }
        }
        while done < prompt_ids.len() {
            let end = (done + chunk).min(prompt_ids.len());
            let (n, p) = self.step(&prompt_ids[done..end], done, past)?;
            next = n;
            past = p;
            done = end;
        }
        self.prompt_live = None;
        let mut len = real_len;
        let mut generated: Vec<u32> = Vec::new();
        // Decode the whole sequence each step and emit the new suffix: decoding a
        // slice of tokens on its own drops/adds leading spaces ("7. 73").
        let mut emitted = String::new();
        let budget = think_budget();
        let mut in_think = think;
        let mut think_tokens: Vec<u32> = Vec::new();
        let end_think = self.end_think;
        let total_cap = if think { max_new + budget + 4 } else { max_new };
        for _ in 0..total_cap {
            if cancel.load(Ordering::Relaxed) || self.eos.contains(&next) {
                break;
            }
            if in_think {
                let closed = Some(next) == end_think;
                if !closed {
                    think_tokens.push(next);
                }
                if closed || think_tokens.len() >= budget {
                    // Out of the think block: naturally, or forced by feeding `</think>`.
                    let mut tail: Vec<u32> = if closed { Vec::new() } else { end_think.into_iter().collect() };
                    tail.extend(self.tok.encode("\n\n", false).map(|e| e.get_ids().to_vec()).unwrap_or_default());
                    let (n, p) = self.step(&[next], len, past)?;
                    next = n;
                    past = p;
                    len += 1;
                    for &t in &tail {
                        let (n, p) = self.step(&[t], len, past)?;
                        next = n;
                        past = p;
                        len += 1;
                    }
                    in_think = false;
                    if std::env::var_os("BLACKHOLE_LLM_DEBUG").is_some() {
                        let text = self.tok.decode(&think_tokens, true).unwrap_or_default();
                        crate::util::log(&format!("llm think ({} tokens{}): {}", think_tokens.len(), if closed { "" } else { ", budget hit" }, text.replace('\n', " ⏎ ").chars().take(400).collect::<String>()));
                    }
                    continue;
                }
                let (n, p) = self.step(&[next], len, past)?;
                next = n;
                past = p;
                len += 1;
                continue;
            }
            generated.push(next);
            // Greedy decoding can fall into a loop; a small model repeating a whole
            // line, or the same token over and over, is never going anywhere useful.
            let stuck = generated.len() >= 8 && generated[generated.len() - 8..].iter().all(|&t| t == next);
            if stuck {
                break;
            }
            if let Ok(full) = self.tok.decode(&generated, true) {
                if !full.ends_with('\u{FFFD}') && full.len() > emitted.len() && full.starts_with(&emitted) {
                    let text = full[emitted.len()..].to_string();
                    if text.contains('\n') && Self::repeats_line(&out, &text) {
                        break;
                    }
                    on_token(&text);
                    out.push_str(&text);
                    emitted = full;
                }
            }
            let (n, p) = self.step(&[next], len, past)?;
            next = n;
            past = p;
            len += 1;
        }
        if std::env::var_os("BLACKHOLE_LLM_DEBUG").is_some() {
            crate::util::log(&format!("llm out ({} tokens): {}", generated.len(), out.replace('\n', " ⏎ ").chars().take(200).collect::<String>()));
        }
        Ok(out)
    }
}

impl Drop for Llm {
    fn drop(&mut self) {
        // ORT only writes the profile once the session ends it.
        if opt("profile") {
            match self.cpu.end_profiling() {
                Ok(p) => crate::util::log(&format!("llm: profile written to {p}")),
                Err(e) => crate::util::log(&format!("llm: end_profiling failed: {e}")),
            }
        }
    }
}
