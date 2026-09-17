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

/// Pairs scored per question, from a latency budget rather than a fixed number:
///
///     k = BUDGET_MS / per_pair,   per_pair = PAIR_MS_REF × REF_THREADS / threads()
///
/// The cross-encoder costs a fixed amount per pair at a fixed token length and
/// scales with the threads it gets, so a two-thread laptop should score fewer
/// pairs than a desktop rather than stalling the answer by half a second.
/// Accuracy sets the ceiling, not the budget: k=10 is where accuracy saturated
/// and k=5 lost two held-out questions, so the budget may only trade k down from
/// MAX to MIN on a slow machine — it never buys more pairs than the measured-good
/// number. (k=20's worst case hit 247 ms, which is what started this.)
///
/// PAIR_MS_REF: measured with `askeval --rerank-bench` — 8.7 ms/pair at 128 tokens,
/// batch 1, 6 intra-op threads on a 24-core Zen 4 (2026-09-16, see RAG.md). Treating
/// the scaling as linear in threads is deliberately pessimistic: k = 400 / (8.7 × 6 /
/// threads) still gives 10 at two threads and drops below it only on slower hardware.
const BUDGET_MS: f32 = 400.0;
const PAIR_MS_REF: f32 = 8.7;
const REF_THREADS: usize = 6;
const MIN_CANDIDATES: usize = 6;
const MAX_CANDIDATES: usize = 10;
const MAX_TOKENS: usize = 128;
/// Document units are scored as "title: first N words".
pub const DOC_UNIT_WORDS: usize = 60;

pub struct Reranker {
    session: Mutex<Session>,
    tok: Tokenizer,
}

/// Intra-op threads the session gets: ORT's default oversubscribes (measured slower),
/// and two cores are left for the UI thread and the embedder.
pub fn threads() -> usize {
    let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
    cores.saturating_sub(2).clamp(2, 6)
}

/// How many (query, passage) pairs one question may spend, for this machine.
/// Computed once; `BLACKHOLE_RERANK_K` overrides it for experiments.
pub fn candidates() -> usize {
    static K: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *K.get_or_init(|| {
        if let Some(k) = std::env::var("BLACKHOLE_RERANK_K").ok().and_then(|v| v.parse::<usize>().ok()).filter(|k| *k > 0) {
            return k;
        }
        let per_pair = PAIR_MS_REF * REF_THREADS as f32 / threads() as f32;
        let k = (BUDGET_MS / per_pair).floor().max(1.0) as usize;
        k.clamp(MIN_CANDIDATES, MAX_CANDIDATES)
    })
}

fn ok<T, E: std::fmt::Display>(r: Result<T, E>) -> anyhow::Result<T> {
    r.map_err(|e| anyhow::anyhow!("{e}"))
}

impl Reranker {
    pub fn load() -> anyhow::Result<Reranker> {
        let b = ok(Session::builder())?;
        let mut b = ok(b.with_intra_threads(threads()))?;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn k_stays_inside_the_range_accuracy_measured() {
        let k = candidates();
        assert!((MIN_CANDIDATES..=MAX_CANDIDATES).contains(&k), "k = {k}");
    }

    #[test]
    fn the_session_never_takes_every_core() {
        let t = threads();
        assert!((2..=6).contains(&t), "{t} intra-op threads");
    }

    #[test]
    fn the_budget_buys_the_measured_good_k_on_a_normal_machine() {
        // The formula, spelled out: two intra-op threads (a 4-core laptop) still
        // affords the full k; only slower hardware than that trades it down.
        let per_pair = PAIR_MS_REF * REF_THREADS as f32 / 2.0;
        assert!((BUDGET_MS / per_pair).floor() as usize >= MAX_CANDIDATES);
    }
}
