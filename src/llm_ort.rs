//! Ask mode on ONNX Runtime: Qwen2.5-Instruct exported to ONNX (int4 weights,
//! fp32 at the boundary). The graph is the standard HF decoder layout with a
//! growing `past_key_values` cache that we carry between steps.
//!
//! Hybrid execution: the prompt pass (hundreds of tokens, one big batch) runs
//! on DirectML where it is 5–6× faster than the CPU; decoding (one token at a
//! time) runs on the CPU provider, because DirectML's per-op dispatch cost on
//! this dynamic-shape graph makes it slower than the CPU for single tokens.
//! The cache produced by the GPU pass is bound straight to CPU memory.

use ort::ep::{DirectML, ExecutionProvider};
use ort::memory::{AllocationDevice, AllocatorType, MemoryInfo, MemoryType};
use ort::session::Session;
use ort::value::{DynValue, Tensor};
use std::borrow::Cow;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use tokenizers::Tokenizer;

static TOKENIZER_QWEN2: &[u8] = include_bytes!("../models/qwen2.5/tokenizer.json");

const IM_END: u32 = 151645;
const END_OF_TEXT: u32 = 151643;
const MAX_NEW_TOKENS: usize = 260;
const PREFILL_TIMELINE: &str = "Timeline:\n";

/// An excerpt handed to the model as grounding.
pub struct Source<'a> {
    pub title: &'a str,
    pub text: &'a str,
}

pub struct Llm {
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
    pub backend: &'static str,
    #[allow(dead_code)]
    pub name: String,
}

fn ok<T, E: std::fmt::Display>(r: Result<T, E>) -> anyhow::Result<T> {
    r.map_err(|e| anyhow::anyhow!("{e}"))
}

/// Size of a model = graph file + its external data file (`<name>.onnx.data`), if any.
fn model_size(p: &Path) -> u64 {
    let graph = std::fs::metadata(p).map(|m| m.len()).unwrap_or(0);
    let mut data = p.as_os_str().to_owned();
    data.push(".data");
    graph + std::fs::metadata(PathBuf::from(data)).map(|m| m.len()).unwrap_or(0)
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
        let Ok(rd) = std::fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            let p = e.path();
            if p.extension().map(|x| x.eq_ignore_ascii_case("onnx")).unwrap_or(false) {
                let size = model_size(&p);
                if best.as_ref().map(|b| size > b.0).unwrap_or(true) {
                    best = Some((size, p));
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

fn gpu_session(path: &Path, device: i32) -> anyhow::Result<Session> {
    let b = ok(Session::builder())?;
    let b = ok(b.with_execution_providers([dml_provider(device).build().error_on_failure()]))?;
    let mut b = ok(b.with_memory_pattern(false))?;
    ok(b.commit_from_file(path))
}

fn cpu_session(path: &Path) -> anyhow::Result<Session> {
    let mut b = ok(Session::builder())?;
    ok(b.commit_from_file(path))
}

impl Llm {
    pub fn load(path: &Path) -> anyhow::Result<Llm> {
        let force_cpu = std::env::var_os("BLACKHOLE_CPU").is_some();
        let cpu = cpu_session(path)?;
        let adapter = if force_cpu { None } else { crate::gpu::preferred() };
        let gpu = match adapter {
            Some(a) if DirectML::default().is_available().unwrap_or(false) => match gpu_session(path, a.index) {
                Ok(s) => {
                    crate::util::log(&format!("llm: prompt pass on DirectML adapter {} ({}, {} MB)", a.index, a.name, a.vram_mb));
                    Some(s)
                }
                Err(e) => {
                    crate::util::log(&format!("llm: DirectML session failed, prompt pass on CPU: {e}"));
                    None
                }
            },
            _ => None,
        };
        let backend = if gpu.is_some() { "DirectML + CPU" } else { "CPU" };
        let session = &cpu;
        // Cache geometry from the graph itself: count past_key_values.N.key inputs, read their dims.
        let mut layers = 0;
        let mut kv_heads = 2;
        let mut head_dim = 128;
        let mut wants_position_ids = false;
        for input in session.inputs() {
            let name = input.name();
            if name == "position_ids" {
                wants_position_ids = true;
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
        let tok = Tokenizer::from_bytes(TOKENIZER_QWEN2).map_err(|e| anyhow::anyhow!("{e}"))?;
        let name = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        Ok(Llm { cpu, gpu, tok, layers, kv_heads, head_dim, wants_position_ids, backend, name })
    }

    fn is_temporal(question: &str) -> bool {
        let q = question.to_lowercase();
        ["before", "after", "previous", "prior", "next", "recent", "latest", "first", "last", "earliest", "newest", "oldest", "when", "order", "current"]
            .iter()
            .any(|k| q.split(|c: char| !c.is_alphanumeric()).any(|w| w == *k))
    }

    fn prompt(question: &str, sources: &[Source]) -> String {
        let mut p = String::new();
        p.push_str(concat!(
            "<|im_start|>system\n",
            "You are Blackhole, a search assistant for the user's own files. Answer from the excerpts provided; do not mention excerpts or their numbers. ",
            "If the question involves order or time, first write a line 'Timeline:' listing every relevant entry with its dates from newest to oldest, then answer on a new line starting with 'Answer:', reading the answer off the timeline. ",
            "Otherwise answer directly in one or two plain sentences. ",
            "Only if the excerpts contain nothing relevant, reply: I couldn't find that in your files.",
            "<|im_end|>\n",
        ));
        p.push_str("<|im_start|>user\n");
        for (i, s) in sources.iter().enumerate() {
            p.push_str(&format!("Excerpt {} (from {}):\n{}\n\n", i + 1, s.title, s.text.trim()));
        }
        p.push_str(&format!("Question: {}<|im_end|>\n<|im_start|>assistant\n", question.trim()));
        if Self::is_temporal(question) {
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

    fn argmax(logits: &DynValue) -> anyhow::Result<u32> {
        let (shape, data) = ok(logits.try_extract_tensor::<f32>())?;
        let vocab = *shape.last().unwrap_or(&(data.len() as i64)) as usize;
        let last = &data[data.len() - vocab..];
        Ok(last.iter().enumerate().fold((0usize, f32::NEG_INFINITY), |b, (i, &x)| if x > b.1 { (i, x) } else { b }).0 as u32)
    }

    fn empty_past(&self) -> anyhow::Result<Vec<DynValue>> {
        (0..self.layers * 2)
            .map(|_| Ok(ok(Tensor::<f32>::from_array(([1usize, self.kv_heads, 0, self.head_dim], Vec::<f32>::new())))?.into_dyn()))
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
        let prompt = Self::prompt(question, sources);
        let enc = self.tok.encode(prompt, false).map_err(|e| anyhow::anyhow!("{e}"))?;
        let prompt_ids: Vec<u32> = enc.get_ids().to_vec();

        let mut out = String::new();
        if Self::is_temporal(question) {
            on_token(PREFILL_TIMELINE);
            out.push_str(PREFILL_TIMELINE);
        }

        let past = self.empty_past()?;
        let (mut next, mut past) = self.step(&prompt_ids, 0, past)?;
        let mut len = prompt_ids.len();
        let mut generated: Vec<u32> = Vec::new();
        let mut decoded_upto = 0;
        for _ in 0..MAX_NEW_TOKENS {
            if cancel.load(Ordering::Relaxed) || next == IM_END || next == END_OF_TEXT {
                break;
            }
            generated.push(next);
            // Greedy decoding can fall into a loop; a small model repeating a whole
            // line, or the same token over and over, is never going anywhere useful.
            let stuck = generated.len() >= 8 && generated[generated.len() - 8..].iter().all(|&t| t == next);
            if stuck {
                break;
            }
            if let Ok(text) = self.tok.decode(&generated[decoded_upto..], true) {
                if !text.contains('\u{FFFD}') {
                    if text.contains('\n') && Self::repeats_line(&out, &text) {
                        break;
                    }
                    on_token(&text);
                    out.push_str(&text);
                    decoded_upto = generated.len();
                }
            }
            let (n, p) = self.step(&[next], len, past)?;
            next = n;
            past = p;
            len += 1;
        }
        Ok(out)
    }
}
