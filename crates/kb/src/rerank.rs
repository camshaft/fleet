//! `rerank` — cross-encoder reranker (CPU via `fastembed`). Port of Python `kb/rerank.py`.
//!
//! Reorders the top vector candidates by a query↔passage relevance model, returning a probability in [0,1]
//! per input document IN INPUT ORDER (the caller zips scores back onto its candidates). Forced onto CPU:
//! with a CUDA-capable ort, onnxruntime would otherwise default to the GPU and the reranker would silently
//! grab several GB of VRAM — reranking ~40 candidates on CPU is a few tens of ms. The model loads once.

use std::sync::{Mutex, OnceLock};

use fastembed::{RerankInitOptions, RerankerModel, TextRerank};
use ort::execution_providers::CPUExecutionProvider;

use crate::config;

static MODEL: OnceLock<Mutex<TextRerank>> = OnceLock::new();

fn model_kind(id: &str) -> Result<RerankerModel, String> {
    match id {
        "BAAI/bge-reranker-base" => Ok(RerankerModel::BGERerankerBase),
        "BAAI/bge-reranker-v2-m3" => Ok(RerankerModel::BGERerankerV2M3),
        other => Err(format!("unsupported rerank_model {other:?}")),
    }
}

fn model() -> Result<&'static Mutex<TextRerank>, String> {
    if let Some(m) = MODEL.get() {
        return Ok(m);
    }
    let cfg = config::get();
    let kind = model_kind(&cfg.rerank_model)?;
    let mut opts = RerankInitOptions::new(kind)
        .with_execution_providers(vec![CPUExecutionProvider::default().build()])
        .with_show_download_progress(true);
    // fastembed's reranker only reads `FASTEMBED_CACHE_DIR`/CWD for its cache (it does NOT honor HF_HOME),
    // so under a hardened service (read-only CWD) the download fails unless we point it at a writable dir.
    if !cfg.cache_dir.is_empty() {
        opts = opts.with_cache_dir(std::path::PathBuf::from(&cfg.cache_dir));
    }
    let tr = TextRerank::try_new(opts).map_err(|e| format!("reranker init failed: {e}"))?;
    let _ = MODEL.set(Mutex::new(tr));
    Ok(MODEL.get().expect("just set"))
}

/// Sigmoid — the Python `_sigmoid`. Maps a raw cross-encoder logit to a probability in (0,1).
fn sigmoid(x: f64) -> f64 {
    1.0 / (1.0 + (-x).exp())
}

/// Relevance probability in [0,1] for each doc w.r.t. the query, in the SAME order as `docs` — the Python
/// `rerank`. Empty input → empty output.
pub fn rerank(query: &str, docs: &[String]) -> Result<Vec<f64>, String> {
    if docs.is_empty() {
        return Ok(vec![]);
    }
    let m = model()?;
    let guard = m
        .lock()
        .map_err(|_| "reranker mutex poisoned".to_string())?;
    // return_documents=false: we only need scores; we already hold the texts. `rerank` takes `AsRef<str>`,
    // so pass borrowed slices rather than cloning the strings.
    let refs: Vec<&str> = docs.iter().map(String::as_str).collect();
    let results = guard
        .rerank(query, refs, false, None)
        .map_err(|e| format!("rerank failed: {e}"))?;
    // fastembed may return results sorted by score; restore input order via each result's `index`, so the
    // caller can zip these onto its candidate list positionally (exactly as the Python does).
    let mut scored = vec![0.0f64; docs.len()];
    for r in results {
        if r.index < scored.len() {
            scored[r.index] = sigmoid(r.score as f64);
        }
    }
    Ok(scored)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sigmoid_matches_reference() {
        assert!((sigmoid(0.0) - 0.5).abs() < 1e-12);
        assert!(sigmoid(10.0) > 0.999);
        assert!(sigmoid(-10.0) < 0.001);
    }

    #[test]
    fn empty_docs_empty_scores() {
        // No model load on the empty path (mirrors Python's early return).
        assert!(rerank("q", &[]).unwrap().is_empty());
    }
}
