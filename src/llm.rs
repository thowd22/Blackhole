//! Ask mode: a small instruct LLM (Qwen2.5 or Qwen3, GGUF) run through candle
//! on the CPU. The weights live next to the exe (`*.gguf`); the tokenizers are
//! compiled in and picked by the GGUF's architecture. Generation streams
//! tokens through a callback and can be cancelled.

use candle_core::quantized::gguf_file;
use candle_core::{Device, Tensor};
use candle_transformers::generation::LogitsProcessor;
use candle_transformers::models::quantized_qwen2::ModelWeights as Qwen2;
use candle_transformers::models::quantized_qwen3::ModelWeights as Qwen3;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use tokenizers::Tokenizer;

static TOKENIZER_QWEN2: &[u8] = include_bytes!("../models/qwen2.5/tokenizer.json");
static TOKENIZER_QWEN3: &[u8] = include_bytes!("../models/qwen3/tokenizer.json");

const IM_END: u32 = 151645;
const END_OF_TEXT: u32 = 151643;
const MAX_NEW_TOKENS: usize = 260;
const PREFILL_TIMELINE: &str = "Timeline:\n";

enum Model {
    Qwen2(Qwen2),
    Qwen3(Qwen3),
}

impl Model {
    fn forward(&mut self, x: &Tensor, pos: usize) -> candle_core::Result<Tensor> {
        match self {
            Model::Qwen2(m) => m.forward(x, pos),
            Model::Qwen3(m) => m.forward(x, pos),
        }
    }
    fn clear_kv_cache(&mut self) {
        match self {
            Model::Qwen2(m) => m.clear_kv_cache(),
            Model::Qwen3(m) => m.clear_kv_cache(),
        }
    }
}

pub struct Llm {
    model: Model,
    tok: Tokenizer,
    #[allow(dead_code)]
    pub name: String,
}

/// An excerpt handed to the model as grounding.
pub struct Source<'a> {
    pub title: &'a str,
    pub text: &'a str,
}

/// Find a GGUF model: next to the exe, then in the vault folder. Picks the
/// largest file so a user who drops in a bigger model gets it used.
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
            if p.extension().map(|x| x.eq_ignore_ascii_case("gguf")).unwrap_or(false) {
                let size = e.metadata().map(|m| m.len()).unwrap_or(0);
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

impl Llm {
    pub fn load(path: &Path) -> anyhow::Result<Llm> {
        let mut file = std::fs::File::open(path)?;
        let content = gguf_file::Content::read(&mut file)?;
        let arch = content
            .metadata
            .get("general.architecture")
            .and_then(|v| v.to_string().ok())
            .cloned()
            .unwrap_or_default();
        let (model, tok_json) = match arch.as_str() {
            "qwen2" => (Model::Qwen2(Qwen2::from_gguf(content, &mut file, &Device::Cpu)?), TOKENIZER_QWEN2),
            "qwen3" => (Model::Qwen3(Qwen3::from_gguf(content, &mut file, &Device::Cpu)?), TOKENIZER_QWEN3),
            other => anyhow::bail!("unsupported model architecture '{other}' (expected qwen2 or qwen3)"),
        };
        let tok = Tokenizer::from_bytes(tok_json).map_err(|e| anyhow::anyhow!("{e}"))?;
        let name = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        Ok(Llm { model, tok, name })
    }

    /// Questions about order or time get a forced "Timeline:" scratchpad first;
    /// small models reason about before/after far better after listing dates.
    fn is_temporal(question: &str) -> bool {
        let q = question.to_lowercase();
        ["before", "after", "previous", "prior", "next", "recent", "latest", "first", "last", "earliest", "newest", "oldest", "when", "order", "current"]
            .iter()
            .any(|k| q.split(|c: char| !c.is_alphanumeric()).any(|w| w == *k))
    }

    fn prompt(&self, question: &str, sources: &[Source]) -> String {
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
        if matches!(self.model, Model::Qwen3(_)) {
            // Empty think block = Qwen3 non-thinking mode; keeps latency the same as Qwen2.5.
            p.push_str("<think>\n\n</think>\n\n");
        }
        if Self::is_temporal(question) {
            p.push_str(PREFILL_TIMELINE);
        }
        p
    }

    /// Generate an answer, calling `on_token` with each decoded piece.
    /// Stops early when `cancel` is set. Returns the full answer.
    pub fn answer(
        &mut self,
        question: &str,
        sources: &[Source],
        cancel: &AtomicBool,
        mut on_token: impl FnMut(&str),
    ) -> anyhow::Result<String> {
        let prompt = self.prompt(question, sources);
        let enc = self.tok.encode(prompt, false).map_err(|e| anyhow::anyhow!("{e}"))?;
        let mut tokens: Vec<u32> = enc.get_ids().to_vec();
        self.model.clear_kv_cache();

        // Qwen2.5 answers factual questions most reliably greedy; Qwen3 degenerates
        // (repeated digits, loops) without sampling, per its model card.
        let mut sampler = match self.model {
            Model::Qwen2(_) => LogitsProcessor::new(42, None, None),
            Model::Qwen3(_) => LogitsProcessor::new(42, Some(0.7), Some(0.8)),
        };
        let mut out = String::new();
        let mut decoded_upto = 0; // tokens already turned into text
        let mut generated: Vec<u32> = Vec::new();

        // Prompt pass, then one token at a time.
        let input = Tensor::new(tokens.as_slice(), &Device::Cpu)?.unsqueeze(0)?;
        let mut logits = self.model.forward(&input, 0)?.squeeze(0)?;
        for _ in 0..MAX_NEW_TOKENS {
            if cancel.load(Ordering::Relaxed) {
                break;
            }
            let next = sampler.sample(&logits)?;
            if next == IM_END || next == END_OF_TEXT {
                break;
            }
            generated.push(next);
            // Decode incrementally; only emit once the pending bytes form valid text.
            if let Ok(text) = self.tok.decode(&generated[decoded_upto..], true) {
                if !text.contains('\u{FFFD}') {
                    on_token(&text);
                    out.push_str(&text);
                    decoded_upto = generated.len();
                }
            }
            let pos = tokens.len();
            tokens.push(next);
            let input = Tensor::new(&[next], &Device::Cpu)?.unsqueeze(0)?;
            logits = self.model.forward(&input, pos)?.squeeze(0)?;
        }
        Ok(out)
    }
}
