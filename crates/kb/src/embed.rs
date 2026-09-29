//! `embed` — local embeddings via the `fastembed` crate. Port of Python `kb/embed.py`.
//!
//! The model is loaded once (a process-global behind a `Mutex`, the Rust analogue of Python's
//! `@lru_cache`). PROVEN: `fastembed` `BGELargeENV15` reproduces the live Qdrant's Python-fastembed vectors
//! at cosine 1.0 — so switching the embedder to Rust needs no re-ingest. The Python code splits
//! `embed_query` (query side) from `embed_docs` (passage side); for bge-large this fastembed version applies
//! NO query instruction (its dense `query_embed` falls back to plain `embed`), so both sides call the same
//! `embed()` here — matching Python exactly.
//!
//! DEVICE: "cpu" (default, and what the always-on server runs) or "gpu". The CUDA execution provider honours
//! the same arena cap + `kSameAsRequested` strategy the Python used to keep a bulk ingest from starving other
//! GPU users. Actually reaching the GPU requires an ort build with the CUDA EP available at runtime (a
//! deploy/phase-2 concern); on CPU the device knobs are inert. The cross-process GPU ingest lock from Python
//! (`gpu_lock`, a no-op on CPU) is deferred to the phase-2 ingest port.

use std::sync::{Mutex, OnceLock};

use fastembed::{EmbeddingModel, ExecutionProviderDispatch, InitOptions, TextEmbedding};
use ort::execution_providers::{ArenaExtendStrategy, CPUExecutionProvider, CUDAExecutionProvider};

use crate::config;

static MODEL: OnceLock<Mutex<TextEmbedding>> = OnceLock::new();
static DIM: OnceLock<usize> = OnceLock::new();

/// Map the configured model id to fastembed's `EmbeddingModel`. Only the models the KB actually uses are
/// wired; the live DB is `BAAI/bge-large-en-v1.5` and must stay so (its vectors are 1024-dim).
fn model_kind(id: &str) -> Result<EmbeddingModel, String> {
    match id {
        "BAAI/bge-large-en-v1.5" => Ok(EmbeddingModel::BGELargeENV15),
        "BAAI/bge-base-en-v1.5" => Ok(EmbeddingModel::BGEBaseENV15),
        "BAAI/bge-small-en-v1.5" => Ok(EmbeddingModel::BGESmallENV15),
        other => Err(format!(
            "unsupported embed_model {other:?} — the live KB is BAAI/bge-large-en-v1.5 (1024-dim); \
             changing it would invalidate every existing vector"
        )),
    }
}

/// Build the execution-provider list for the configured device — the Rust analogue of Python's provider
/// selection (CPU explicit, or CUDA with the arena cap so a bulk ingest leaves VRAM for other GPU users).
fn providers(cfg: &config::Config) -> Vec<ExecutionProviderDispatch> {
    if matches!(cfg.embed_device.to_lowercase().as_str(), "gpu" | "cuda") {
        let mut cuda = CUDAExecutionProvider::default()
            .with_device_id(0)
            .with_arena_extend_strategy(ArenaExtendStrategy::SameAsRequested);
        if cfg.embed_gpu_mem_limit > 0 {
            cuda = cuda.with_memory_limit(cfg.embed_gpu_mem_limit as usize);
        }
        // CPU EP kept as a fallback, mirroring the Python provider list.
        vec![cuda.build(), CPUExecutionProvider::default().build()]
    } else {
        // Force CPU explicitly: an ort built with CUDA would otherwise default to the GPU and put the
        // "CPU" embedder on the GPU anyway (the exact bug the Python comment guards against).
        vec![CPUExecutionProvider::default().build()]
    }
}

/// The process-global embedder, initialized on first use. Errors (bad model id, model download failure)
/// surface as a `String` from the first caller.
fn model() -> Result<&'static Mutex<TextEmbedding>, String> {
    if let Some(m) = MODEL.get() {
        return Ok(m);
    }
    let cfg = config::get();
    let kind = model_kind(&cfg.embed_model)?;
    let opts = InitOptions::new(kind)
        .with_execution_providers(providers(cfg))
        .with_show_download_progress(true);
    let te = TextEmbedding::try_new(opts).map_err(|e| format!("embedder init failed: {e}"))?;
    // Racing initializers: whoever wins `set` provides the model; the loser's is dropped.
    let _ = MODEL.set(Mutex::new(te));
    Ok(MODEL.get().expect("just set"))
}

/// Embed passages (document side) — the Python `embed_docs`. Batches per `embed_batch`.
pub fn embed_docs(texts: &[String]) -> Result<Vec<Vec<f32>>, String> {
    if texts.is_empty() {
        return Ok(vec![]);
    }
    let batch = config::get().embed_batch;
    let m = model()?;
    let guard = m
        .lock()
        .map_err(|_| "embedder mutex poisoned".to_string())?;
    guard
        .embed(texts.to_vec(), Some(batch))
        .map_err(|e| format!("embed_docs failed: {e}"))
}

/// Embed a single query (query side) — the Python `embed_query`. Same code path as documents for bge in
/// this fastembed version (no query instruction is applied).
pub fn embed_query(text: &str) -> Result<Vec<f32>, String> {
    let mut v = embed_docs(&[text.to_string()])?;
    v.pop()
        .ok_or_else(|| "embed_query produced no vector".to_string())
}

/// Embedding dimensionality (probed once) — the Python `dim()`. Used to create new collections.
pub fn dim() -> Result<usize, String> {
    if let Some(d) = DIM.get() {
        return Ok(*d);
    }
    let d = embed_query("dimension probe")?.len();
    let _ = DIM.set(d);
    Ok(d)
}
