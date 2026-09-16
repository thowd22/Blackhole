//! On-device embeddings: bge-small-en-v1.5 (384-dim, CLS-pooled) on ONNX
//! Runtime — DirectML when a GPU is available, CPU otherwise. The ONNX model
//! and vocab are compiled into the binary.

use ort::ep::DirectML;
use ort::ep::ExecutionProvider;
use ort::session::Session;
use ort::value::Tensor;
use std::collections::HashMap;
use std::sync::Mutex;

pub const DIM: usize = 384;
/// Stamped into the vault; changing the model invalidates stored vectors.
pub const MODEL_ID: &str = "bge-small-en-v1.5/cls/v1";
const MAX_TOKENS: usize = 256;
/// Sequences are padded up to one of these lengths so DirectML compiles a
/// handful of shapes instead of one per input.
const BUCKETS: [usize; 4] = [32, 64, 128, MAX_TOKENS];

static MODEL_BYTES: &[u8] = include_bytes!("../models/bge-small-en-v1.5/model.onnx");
static VOCAB_TXT: &str = include_str!("../models/bge-small-en-v1.5/vocab.txt");

pub struct Embedder {
    session: Mutex<Session>,
    tok: WordPiece,
    /// "DirectML" or "CPU".
    pub backend: &'static str,
}

/// ort's builder errors carry non-Send state; flatten them to text for anyhow.
pub(crate) fn ok<T, E: std::fmt::Display>(r: Result<T, E>) -> anyhow::Result<T> {
    r.map_err(|e| anyhow::anyhow!("{e}"))
}

fn gpu_session(device: i32) -> anyhow::Result<Session> {
    let b = ok(Session::builder())?;
    let b = ok(b.with_execution_providers([DirectML::default().with_device_id(device).build().error_on_failure()]))?;
    let mut b = ok(b.with_memory_pattern(false))?;
    ok(b.commit_from_memory(MODEL_BYTES))
}

fn cpu_session() -> anyhow::Result<Session> {
    let b = ok(Session::builder())?;
    let mut b = ok(b.with_intra_threads(4))?;
    ok(b.commit_from_memory(MODEL_BYTES))
}

impl Embedder {
    /// Try the GPU first; fall back to the CPU provider of the same runtime.
    pub fn load() -> anyhow::Result<Embedder> {
        let tok = WordPiece::new(VOCAB_TXT);
        // BLACKHOLE_CPU: skip the GPU (CI runners, debugging).
        let force_cpu = std::env::var_os("BLACKHOLE_CPU").is_some();
        if let Some(a) = crate::gpu::preferred().filter(|_| !force_cpu) {
            if DirectML::default().is_available().unwrap_or(false) {
                if let Ok(session) = gpu_session(a.index) {
                    return Ok(Embedder { session: Mutex::new(session), tok, backend: "DirectML" });
                }
            }
        }
        Ok(Embedder { session: Mutex::new(cpu_session()?), tok, backend: "CPU" })
    }

    /// CLS-pooled, L2-normalised sentence embedding (bge convention).
    pub fn embed(&self, text: &str) -> anyhow::Result<Vec<f32>> {
        let ids = self.tok.encode(text, MAX_TOKENS);
        let real = ids.len();
        let n = BUCKETS.iter().copied().find(|&b| b >= real).unwrap_or(MAX_TOKENS);
        let mut input_ids: Vec<i64> = ids.iter().map(|&i| i as i64).collect();
        input_ids.resize(n, 0);
        let mut mask = vec![1i64; real];
        mask.resize(n, 0);
        let types = vec![0i64; n];

        let mut session = self.session.lock().unwrap();
        let outputs = ok(session.run(ort::inputs![
            "input_ids" => ok(Tensor::from_array(([1usize, n], input_ids)))?,
            "attention_mask" => ok(Tensor::from_array(([1usize, n], mask)))?,
            "token_type_ids" => ok(Tensor::from_array(([1usize, n], types)))?,
        ]))?;
        let (_shape, data) = ok(outputs["last_hidden_state"].try_extract_tensor::<f32>())?; // [1, n, DIM]
        let mut v: Vec<f32> = data[..DIM].to_vec(); // [CLS] token
        let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt().max(1e-6);
        for x in &mut v {
            *x /= norm;
        }
        Ok(v)
    }
}

pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

/// Minimal BERT uncased WordPiece tokenizer: basic tokenisation (lowercase,
/// punctuation split, accent strip) then greedy longest-match subwords.
struct WordPiece {
    vocab: HashMap<String, u32>,
    cls: u32,
    sep: u32,
    unk: u32,
}

impl WordPiece {
    fn new(vocab_txt: &str) -> WordPiece {
        let vocab: HashMap<String, u32> = vocab_txt
            .lines()
            .enumerate()
            .map(|(i, w)| (w.trim_end().to_string(), i as u32))
            .collect();
        let get = |k: &str| *vocab.get(k).expect("special token missing from vocab");
        WordPiece { cls: get("[CLS]"), sep: get("[SEP]"), unk: get("[UNK]"), vocab }
    }

    fn encode(&self, text: &str, max: usize) -> Vec<u32> {
        let mut ids = vec![self.cls];
        'outer: for word in basic_tokenize(text) {
            let chars: Vec<char> = word.chars().collect();
            if chars.len() > 100 {
                ids.push(self.unk);
                continue;
            }
            let mut start = 0;
            let mut pieces = Vec::new();
            while start < chars.len() {
                let mut end = chars.len();
                let mut found = None;
                while start < end {
                    let mut s: String = chars[start..end].iter().collect();
                    if start > 0 {
                        s = format!("##{s}");
                    }
                    if let Some(&id) = self.vocab.get(&s) {
                        found = Some(id);
                        break;
                    }
                    end -= 1;
                }
                match found {
                    Some(id) => pieces.push(id),
                    None => {
                        pieces = vec![self.unk];
                        break;
                    }
                }
                start = end;
            }
            for id in pieces {
                if ids.len() >= max - 1 {
                    break 'outer;
                }
                ids.push(id);
            }
        }
        ids.push(self.sep);
        ids
    }
}

fn basic_tokenize(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    for ch in text.chars() {
        let Some(c) = strip_accent(ch) else { continue };
        if c.is_whitespace() || c.is_control() {
            if !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
            }
        } else if c.is_ascii_punctuation() || (!c.is_alphanumeric() && !c.is_whitespace()) || is_cjk(c) {
            if !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
            }
            out.push(c.to_lowercase().collect());
        } else {
            cur.extend(c.to_lowercase());
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

fn is_cjk(c: char) -> bool {
    matches!(c as u32, 0x4E00..=0x9FFF | 0x3400..=0x4DBF | 0x20000..=0x2A6DF | 0xF900..=0xFAFF)
}

/// Fold common Latin accents to ASCII (uncased BERT strips accents); drop combining marks.
fn strip_accent(c: char) -> Option<char> {
    if (c as u32) < 0x80 {
        return Some(c);
    }
    if matches!(c as u32, 0x300..=0x36F) {
        return None; // combining diacritic
    }
    const FROM: &str = "àáâãäåèéêëìíîïòóôõöùúûüýÿñçÀÁÂÃÄÅÈÉÊËÌÍÎÏÒÓÔÕÖÙÚÛÜÝÑÇ";
    const TO: &str = "aaaaaaeeeeiiiiooooouuuuyyncAAAAAAEEEEIIIIOOOOOUUUUYNC";
    if let Some(i) = FROM.chars().position(|f| f == c) {
        return TO.chars().nth(i);
    }
    Some(c)
}
