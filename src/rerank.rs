//! Cross-encoder reranker for ask mode: mxbai-rerank-xsmall-v1 (int8, ~84 MB)
//! on ONNX Runtime. Scores (query, passage) pairs jointly, which single-vector
//! embeddings cannot do; measured +2 Hit@1 out of sample over the cheap
//! pipeline (18/18 on dev + held-out) at ~11 ms per pair on a 12-core CPU.
//!
//! CPU, batch size 1, intra-op threads pinned: ORT's default oversubscribes
//! and pads batches to the longest pair — both measured slower.

use ort::session::Session;
use ort::value::Tensor;
use std::sync::Mutex;
use tokenizers::{Tokenizer, TruncationParams};

static MODEL_BYTES: &[u8] = include_bytes!("../models/rerank-mxbai-int8/model.onnx");
static TOKENIZER_JSON: &[u8] = include_bytes!("../models/rerank-mxbai-int8/tokenizer.json");

/// Pairs scored per question. k=5 lost two held-out questions; k=20's worst
/// case hit 247 ms. k=10 was the point where accuracy saturated.
pub const CANDIDATES: usize = 10;
const MAX_TOKENS: usize = 128;
/// Document units are scored as "title: first N words".
pub const DOC_UNIT_WORDS: usize = 60;

pub struct Reranker {
    session: Mutex<Session>,
    tok: Tokenizer,
}

fn ok<T, E: std::fmt::Display>(r: Result<T, E>) -> anyhow::Result<T> {
    r.map_err(|e| anyhow::anyhow!("{e}"))
}

impl Reranker {
    pub fn load() -> anyhow::Result<Reranker> {
        let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
        let threads = cores.saturating_sub(2).clamp(2, 6);
        let b = ok(Session::builder())?;
        let mut b = ok(b.with_intra_threads(threads))?;
        let session = ok(b.commit_from_memory(MODEL_BYTES))?;
        let mut tok = Tokenizer::from_bytes(TOKENIZER_JSON).map_err(|e| anyhow::anyhow!("{e}"))?;
        tok.with_truncation(Some(TruncationParams { max_length: MAX_TOKENS, ..Default::default() }))
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        Ok(Reranker { session: Mutex::new(session), tok })
    }

    /// Relevance logit for each passage (higher = more relevant). Not
    /// calibrated across queries; only the order within one query is meaningful.
    pub fn score(&self, query: &str, passages: &[String]) -> anyhow::Result<Vec<f32>> {
        let mut session = self.session.lock().unwrap();
        let mut out = Vec::with_capacity(passages.len());
        for p in passages {
            let enc = self.tok.encode((query, p.as_str()), true).map_err(|e| anyhow::anyhow!("{e}"))?;
            let ids: Vec<i64> = enc.get_ids().iter().map(|&i| i as i64).collect();
            let mask: Vec<i64> = enc.get_attention_mask().iter().map(|&m| m as i64).collect();
            let n = ids.len();
            let outputs = ok(session.run(ort::inputs![
                "input_ids" => ok(Tensor::from_array(([1usize, n], ids)))?,
                "attention_mask" => ok(Tensor::from_array(([1usize, n], mask)))?,
            ]))?;
            let (_shape, logits) = ok(outputs["logits"].try_extract_tensor::<f32>())?;
            out.push(logits.first().copied().unwrap_or(f32::NEG_INFINITY));
        }
        Ok(out)
    }
}
