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
use ort::session::Session;
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
/// GPU-resident cache capacity (tokens) for the GPU-decode mode.
const GPU_KV_CAPACITY: usize = 4096;
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

fn gpu_session(path: &Path, device: i32) -> anyhow::Result<Session> {
    let b = ok(Session::builder())?;
    let b = ok(b.with_execution_providers([dml_provider(device).build().error_on_failure()]))?;
    let mut b = ok(b.with_memory_pattern(false))?;
    if opt("devinit") {
        b = ok(b.with_device_allocated_initializers())?;
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
        // BLACKHOLE_DECODE=gpu: decode on DirectML as well and skip the CPU session
        // entirely (roughly halves RAM; per-token cost is DirectML's dispatch overhead).
        let want_gpu_decode = std::env::var("BLACKHOLE_DECODE").map(|v| v == "gpu").unwrap_or(false);
        let adapter = if force_cpu { None } else { crate::gpu::preferred() };
        let mut gpu_device = 0;
        let gpu = match adapter {
            Some(a) if DirectML::default().is_available().unwrap_or(false) => match gpu_session(path, a.index) {
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
        let gpu_decode = want_gpu_decode && gpu.is_some();
        // The CPU session is only needed when it decodes (or when there is no GPU).
        let cpu = if gpu_decode { gpu_session(path, gpu_device)? } else { cpu_session(path)? };
        let gpu_alloc = if gpu_decode {
            let mem = ok(MemoryInfo::new(AllocationDevice::DIRECTML, 0, AllocatorType::Device, MemoryType::Default))?;
            Some(ok(Allocator::new(&cpu, mem))?)
        } else {
            None
        };
        let backend = if gpu_decode { "DirectML" } else if gpu.is_some() { "DirectML + CPU" } else { "CPU" };
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
        crate::util::log(&format!("llm: {} layout, {family:?} template{}, {} stop tokens, kv {}", if wants_position_ids { "HF" } else { "GenAI" }, if no_think { " (no-think)" } else { "" }, eos.len(), if kv_f16 { "f16" } else { "f32" }));
        let name = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        Ok(Llm { family, no_think, eos, cpu, gpu, tok, layers, kv_heads, head_dim, wants_position_ids, kv_f16, gpu_decode, gpu_device, gpu_alloc, backend, name })
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
        let temporal = Self::is_temporal(question);
        let system = if temporal {
            concat!(
                "You are Blackhole, a search assistant for the user's own files. Answer from the excerpts provided; do not mention excerpts or their numbers. ",
                "The question is about order or time: first list every relevant entry with its dates from newest to oldest under 'Timeline:', then answer on a new line starting with 'Answer:', reading the answer off the timeline. ",
                "Only if the excerpts contain nothing relevant, reply: I couldn't find that in your files.",
            )
        } else {
            concat!(
                "You are Blackhole, a search assistant for the user's own files. Answer from the excerpts provided; do not mention excerpts or their numbers. ",
                "Answer directly in one or two plain sentences, quoting names, numbers and dates exactly as written. ",
                "Only if the excerpts contain nothing relevant, reply: I couldn't find that in your files.",
            )
        };
        let mut user = String::new();
        for (i, s) in sources.iter().enumerate() {
            user.push_str(&format!("Excerpt {} (from {}):\n{}\n\n", i + 1, s.title, s.text.trim()));
        }
        user.push_str(&format!("Question: {}", question.trim()));

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
            p.push_str("<think>\n\n</think>\n\n");
        }
        if temporal {
            p.push_str(PREFILL_TIMELINE);
        }
        p
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
        let shape = [1usize, self.kv_heads, GPU_KV_CAPACITY, self.head_dim];
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
        past: &[DynValue],
    ) -> anyhow::Result<(u32, Vec<DynValue>)> {
        let mut binding = ok(gpu.create_binding())?;
        ok(binding.bind_input("input_ids", input_ids))?;
        ok(binding.bind_input("attention_mask", mask))?;
        if wants_position_ids {
            ok(binding.bind_input("position_ids", pos))?;
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
        let input_ids = ok(Tensor::from_array(([1usize, n], ids.iter().map(|&i| i as i64).collect::<Vec<_>>())))?;
        let mask = ok(Tensor::from_array(([1usize, total], vec![1i64; total])))?;
        let pos = ok(Tensor::from_array(([1usize, n], (past_len..total).map(|p| p as i64).collect::<Vec<_>>())))?;
        let layers = self.layers;

        if self.gpu_decode {
            let dbg = std::env::var_os("BLACKHOLE_LLM_DEBUG").is_some();
            if dbg {
                crate::util::log(&format!("gpu-decode step: n={n} past_len={past_len} past_values={}", past.len()));
            }
            // Cache stays on the GPU between steps; only the logits come back.
            let mut binding = ok(self.cpu.create_binding())?;
            ok(binding.bind_input("input_ids", &input_ids))?;
            ok(binding.bind_input("attention_mask", &mask))?;
            if self.wants_position_ids {
                ok(binding.bind_input("position_ids", &pos))?;
            }
            if total > GPU_KV_CAPACITY {
                anyhow::bail!("prompt of {total} tokens exceeds the GPU cache capacity of {GPU_KV_CAPACITY}");
            }
            // The fixed-capacity buffers are the cache: DirectML's GQA appends the new rows into
            // them in place (GenAI's shared-buffer convention), so the `present` outputs are
            // allocated by ORT and ignored, and the same buffers are fed again next step.
            for (i, kv) in past.iter().enumerate() {
                ok(binding.bind_input(Self::kv_name(i, "past_key_values"), kv))?;
            }
            let cpu_mem = ok(MemoryInfo::new(AllocationDevice::CPU, 0, AllocatorType::Device, MemoryType::Default))?;
            let gpu_mem = ok(MemoryInfo::new(AllocationDevice::DIRECTML, 0, AllocatorType::Device, MemoryType::Default))?;
            let _ = self.gpu_device;
            ok(binding.bind_output_to_device("logits", &cpu_mem))?;
            for i in 0..layers * 2 {
                ok(binding.bind_output_to_device(Self::kv_name(i, "present"), &gpu_mem))?;
            }
            if dbg {
                crate::util::log("gpu-decode: bound, running");
            }
            let outputs = ok(self.cpu.run_binding(&binding))?;
            if dbg {
                crate::util::log("gpu-decode: ran");
            }
            let next = Self::argmax(&outputs["logits"])?;
            drop(outputs);
            drop(binding);
            if dbg {
                crate::util::log(&format!("gpu-decode: next={next}"));
            }
            return Ok((next, past));
        }
        if past_len == 0 {
            if let Some(gpu) = self.gpu.as_mut() {
                match Self::prompt_pass_gpu(gpu, layers, self.wants_position_ids, &input_ids, &mask, &pos, &past) {
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
        mut on_token: impl FnMut(&str),
    ) -> anyhow::Result<String> {
        let prompt = self.prompt(question, sources);
        let enc = self.tok.encode(prompt, false).map_err(|e| anyhow::anyhow!("{e}"))?;
        let prompt_ids: Vec<u32> = enc.get_ids().to_vec();
        if std::env::var_os("BLACKHOLE_LLM_DEBUG").is_some() {
            let head: Vec<String> = prompt_ids.iter().take(12).map(|i| i.to_string()).collect();
            let tail: Vec<String> = prompt_ids.iter().rev().take(8).rev().map(|i| i.to_string()).collect();
            crate::util::log(&format!("llm prompt: {} tokens, head [{}] tail [{}] eos {:?}", prompt_ids.len(), head.join(" "), tail.join(" "), self.eos));
        }

        let mut out = String::new();
        if Self::is_temporal(question) {
            on_token(PREFILL_TIMELINE);
            out.push_str(PREFILL_TIMELINE);
        }

        let past = if self.gpu_decode { self.device_kv()? } else { self.empty_past()? };
        let (mut next, mut past) = self.step(&prompt_ids, 0, past)?;
        let mut len = prompt_ids.len();
        let mut generated: Vec<u32> = Vec::new();
        // Decode the whole sequence each step and emit the new suffix: decoding a
        // slice of tokens on its own drops/adds leading spaces ("7. 73").
        let mut emitted = String::new();
        for _ in 0..MAX_NEW_TOKENS {
            if cancel.load(Ordering::Relaxed) || self.eos.contains(&next) {
                break;
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
