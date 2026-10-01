//! `config` — TOML-file configuration for the kb binary.
//!
//! A port of the Python `kb/config.py`, converted from `KB_*` environment variables to a single TOML file
//! (operator mandate seq-1377: daemons are configured by TOML, not env vars). Every knob is optional; a
//! missing file or key falls back to the same built-in default the Python used, so a host with no config
//! behaves identically. Loaded once into a process-global (see [`get`]); the `--config <path>` override is
//! recorded via [`set_path`] before the first read. The only environment consulted is the OS-standard
//! `HOME`/`XDG_CONFIG_HOME` used to LOCATE the file — never a kb knob.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::OnceLock;

use serde::Deserialize;

/// The kb binary's settings. Every field is optional; a missing field uses the built-in default at its use
/// site. Field names/defaults mirror the Python `KB_*` env vars one-for-one.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Qdrant base URL (was `KB_QDRANT_URL`).
    pub qdrant_url: String,
    /// Embedding model id — MUST stay `BAAI/bge-large-en-v1.5` to match the live 1024-dim vectors.
    pub embed_model: String,
    /// "cpu" or "gpu" (was `KB_EMBED_DEVICE`). Same model → same vectors either way.
    pub embed_device: String,
    /// Cap the CUDA memory arena (bytes) on GPU so a bulk ingest can't starve other GPU users. 0 = uncapped.
    pub embed_gpu_mem_limit: u64,
    /// Embed in small batches so one big file can't request a multi-GB allocation (was `KB_EMBED_BATCH`).
    pub embed_batch: usize,

    /// Cross-encoder rerank of the top candidates (was `KB_RERANK`).
    pub rerank_enabled: bool,
    /// Reranker model id (was `KB_RERANK_MODEL`).
    pub rerank_model: String,
    /// How many vector candidates to rerank (was `KB_RERANK_CANDIDATES`).
    pub rerank_candidates: usize,

    /// Where fastembed caches downloaded model files (bge-large + reranker). Empty = fastembed's own
    /// default (`FASTEMBED_CACHE_DIR` env, else `.fastembed_cache` in the process CWD). This exists because
    /// fastembed 4.9 uses TWO different cache mechanisms — the embedder honors `HF_HOME`, but the reranker
    /// only reads `FASTEMBED_CACHE_DIR`/CWD — so under a hardened service (read-only CWD) the reranker
    /// download fails. Setting one explicit dir here (passed to BOTH via `with_cache_dir`) makes the cache
    /// location deterministic and env-independent. The deployed role points this at a writable StateDirectory.
    pub cache_dir: String,

    /// MCP server bind host (was `KB_MCP_HOST`).
    pub mcp_host: String,
    /// MCP server bind port (was `KB_MCP_PORT`).
    pub mcp_port: u16,
    /// rmcp's DNS-rebinding Host allowlist. rmcp defaults to loopback-only, but the Python server accepted
    /// any Host (it binds 0.0.0.0 and LAN agents connect directly), so the default here is `["*"]` to
    /// preserve that. Set an explicit list to restrict, or `["*"]` to disable the check.
    pub mcp_allowed_hosts: Vec<String>,

    /// Kubo (go-ipfs) HTTP RPC base URL for the phase-2 inbox worker's pinning (was `KB_IPFS_API`). Points at
    /// a local node's `/api/v0` RPC; only the ingest workers use it, so the always-on server ignores it.
    pub ipfs_url: String,

    /// Coordination-board REST base URL for the phase-2 board-driven workers (was `TB_MCP_URL`, now the REST
    /// API not MCP). The deployed workers reach the board on the deployment host; only the workers use it.
    pub board_url: String,

    /// Drop-folder the `kb inbox` worker drains (was `KB_INBOX_DIR`). Each file is ingested + IPFS-pinned
    /// then deleted; the first path component is its collection.
    pub inbox_dir: String,
    /// Collection for a file dropped directly in the inbox root, with no subfolder (was
    /// `KB_INBOX_DEFAULT_COLLECTION`); also the `_sanitize` empty-name fallback.
    pub inbox_default_collection: String,
    /// IPFS HTTP gateway base used to build a pinned file's `ipfs_url` payload as `{gateway}/ipfs/{cid}`
    /// (was `KB_IPFS_GATEWAY`). Citation URL only; not part of the point id.
    pub ipfs_gateway: String,

    /// OCR-fallback threshold for image-only PDF pages (task_40). A PDF page whose extracted text has FEWER
    /// than this many non-whitespace chars is treated as image-only and rendered + OCR'd (via the `tesseract`
    /// binary on PATH). 0 DISABLES OCR entirely — the default, so there is no behavior change and no new
    /// runtime dependency unless a role opts in. OCR text is net-new (image-only pages yield ~nothing today),
    /// so enabling it never drifts existing vectors — it only adds text where there was none.
    pub pdf_ocr_min_chars: usize,

    /// Default collection for the CLI + single-collection tool calls (was `KB_DEFAULT_COLLECTION`).
    pub default_collection: String,
    /// Where `kb_remember` / `kb_supersede` write by default (was `KB_MEMORY_COLLECTION`).
    pub memory_collection: String,

    /// Ranking-blend weights. Relevance dominates; these nudge ordering by curation signals.
    pub w_quality: f64,
    pub w_authority: f64,
    pub w_votes: f64,
    pub w_recency: f64,
    /// Memory recency half-life in days (was `KB_RECENCY_HALFLIFE_DAYS`).
    pub recency_halflife_days: f64,

    /// Provenance weight in [0,1] by item kind (manual/doc/memory). Default 0.5 for unknown kinds.
    pub authority: HashMap<String, f64>,
}

impl Default for Config {
    fn default() -> Self {
        // These are the EXACT Python defaults from kb/config.py — do not drift them (they define ranking
        // and the vector space the live DB was built in).
        let mut authority = HashMap::new();
        authority.insert("manual".to_string(), 1.0);
        authority.insert("doc".to_string(), 0.8);
        authority.insert("memory".to_string(), 0.5);
        // NEW additive kind (not a Python default, so no drift to the above): operator tenets are durable
        // law, so kind="tenet" gets top authority 1.0 by default AND is non-decaying (recency_score returns
        // 1.0 for any non-"memory" kind). See the task_538 tenets store; written via kb_remember(kind="tenet").
        authority.insert("tenet".to_string(), 1.0);
        Self {
            qdrant_url: "http://localhost:6333".to_string(),
            embed_model: "BAAI/bge-large-en-v1.5".to_string(),
            embed_device: "cpu".to_string(),
            embed_gpu_mem_limit: 0,
            embed_batch: 16,
            rerank_enabled: true,
            rerank_model: "BAAI/bge-reranker-base".to_string(),
            rerank_candidates: 40,
            cache_dir: String::new(),
            ipfs_url: "http://127.0.0.1:5001".to_string(),
            board_url: "http://127.0.0.1:8079/api".to_string(),
            inbox_dir: "/data/kb-inbox".to_string(),
            inbox_default_collection: "inbox".to_string(),
            // Generic loopback default (NOT the host-specific green-machine.lan), matching the other
            // 127.0.0.1 defaults -- camshaft/fleet is the public-extraction repo and must stay host-neutral
            // (task_727). This is a citation-URL base, not ranking/vector-affecting, so drifting it from the
            // host-specific value is safe. The green deployment overrides it to green-machine.lan:8080 in the
            // kb-inbox/uploader/embedder role TOMLs, so live behavior is unchanged; only a config-less/local
            // run sees this default.
            ipfs_gateway: "http://127.0.0.1:8080".to_string(),
            pdf_ocr_min_chars: 0, // OCR disabled by default (opt-in per role)
            mcp_host: "0.0.0.0".to_string(),
            mcp_port: 8077,
            mcp_allowed_hosts: vec!["*".to_string()],
            default_collection: "voron_manuals".to_string(),
            memory_collection: "memory".to_string(),
            w_quality: 0.15,
            w_authority: 0.10,
            w_votes: 0.15,
            w_recency: 0.05,
            recency_halflife_days: 90.0,
            authority,
        }
    }
}

impl Config {
    /// Provenance weight for a kind, defaulting to 0.5 — the Python `authority_for`.
    pub fn authority_for(&self, kind: &str) -> f64 {
        self.authority.get(kind).copied().unwrap_or(0.5)
    }
}

static CONFIG: OnceLock<Config> = OnceLock::new();
static PATH_OVERRIDE: OnceLock<Option<PathBuf>> = OnceLock::new();

/// The default config path: `$XDG_CONFIG_HOME/kb/config.toml`, else `$HOME/.config/kb/config.toml`, else
/// `None`. `HOME`/`XDG_CONFIG_HOME` are OS-standard locators, not kb knobs.
fn default_path() -> Option<PathBuf> {
    if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME").filter(|s| !s.is_empty()) {
        return Some(PathBuf::from(xdg).join("kb/config.toml"));
    }
    std::env::var_os("HOME")
        .filter(|s| !s.is_empty())
        .map(|h| PathBuf::from(h).join(".config/kb/config.toml"))
}

/// Record the `--config <path>` override before the first [`get`]. A no-op once the config is loaded.
pub fn set_path(path: Option<PathBuf>) {
    let _ = PATH_OVERRIDE.set(path);
}

/// Parse a config from TOML text — the all-defaults config if it doesn't parse. Pure; unit-tested.
fn parse(toml_text: &str) -> Config {
    toml::from_str(toml_text).unwrap_or_default()
}

/// The loaded config (parsed once). Reads the `--config` override else the default path; an absent file
/// yields the all-defaults config, and an unparseable file is reported to stderr then treated as defaults.
pub fn get() -> &'static Config {
    CONFIG.get_or_init(|| {
        let path = PATH_OVERRIDE.get().cloned().flatten().or_else(default_path);
        let Some(path) = path else {
            return Config::default();
        };
        match std::fs::read_to_string(&path) {
            Ok(text) => {
                if toml::from_str::<Config>(&text).is_err() {
                    eprintln!(
                        "kb: config {} is not valid TOML; using defaults",
                        path.display()
                    );
                }
                parse(&text)
            }
            Err(_) => Config::default(), // absent file → defaults (the common case)
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_python() {
        let c = Config::default();
        assert_eq!(c.embed_model, "BAAI/bge-large-en-v1.5");
        assert_eq!(c.mcp_port, 8077);
        assert_eq!(c.rerank_candidates, 40);
        assert!(c.rerank_enabled);
        assert_eq!(c.authority_for("manual"), 1.0);
        assert_eq!(c.authority_for("doc"), 0.8);
        assert_eq!(c.authority_for("memory"), 0.5);
        assert_eq!(c.authority_for("tenet"), 1.0); // operator tenets: top authority, non-decaying
        assert_eq!(c.authority_for("unknown"), 0.5);
        assert_eq!(c.pdf_ocr_min_chars, 0); // OCR off by default (task_40)
        // Host-neutral default (task_727): the public-extraction repo must not hardcode green-machine.lan.
        // Deployments override via the role TOML; a config-less run gets loopback.
        assert_eq!(c.ipfs_gateway, "http://127.0.0.1:8080");
    }

    #[test]
    fn parse_partial_keeps_other_defaults() {
        let c = parse(
            r#"
            embed_device = "gpu"
            embed_gpu_mem_limit = 4294967296
            mcp_port = 8076
            pdf_ocr_min_chars = 12
            "#,
        );
        assert_eq!(c.embed_device, "gpu");
        assert_eq!(c.embed_gpu_mem_limit, 4294967296);
        assert_eq!(c.mcp_port, 8076);
        assert_eq!(c.pdf_ocr_min_chars, 12); // parses when set (opt-in)
        // untouched keys keep the Python defaults
        assert_eq!(c.embed_model, "BAAI/bge-large-en-v1.5");
        assert_eq!(c.w_quality, 0.15);
    }

    #[test]
    fn parse_invalid_is_defaults_not_panic() {
        let c = parse("this is = = not toml");
        assert_eq!(c.mcp_port, 8077);
    }
}
