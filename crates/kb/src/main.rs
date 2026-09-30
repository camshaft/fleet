//! `kb` — the knowledge-base binary: an MCP server (`kb serve`) + a terminal search CLI (`kb search`).
//!
//! A Python→Rust port of `camshaft/knowledge-base` (board task #157). Phase 1 = the always-on, agent-facing
//! path only: `kb serve` runs the MCP server over streamable-HTTP at `/mcp` (the exact task-board pattern),
//! and `kb search <query>` mirrors the Python `kb.cli`. The board-driven ingest workers stay Python until
//! phase 2. Config is a single TOML file (`--config`), never env vars (operator mandate seq-1377).

// Phase-2 ingest infra: the coordination-board REST client the board-driven workers (#238) use. Its own
// `allow(dead_code)` (see the module) covers being landed ahead of its callers.
mod board;
mod chunk;
mod config;
// Phase-2 ingest worker core: docs.rs rustdoc-JSON ingest (`kb crate-docs`), shared with the pipeline (#238).
mod crate_docs;
mod curate;
mod embed;
// Phase-2 ingest infra: file discovery + text/PDF extraction the inbox/pipeline workers build on. Its own
// `allow(dead_code)` (see the module) covers being landed ahead of its callers.
mod extract;
// Phase-2 drop-folder ingest worker (`kb inbox`).
mod inbox;
// Phase-2 ingest infra: the Kubo IPFS client the inbox worker pins ingested files with. `allow(dead_code)`
// covers the not-yet-used surface (e.g. `cat`, used by later workers).
#[allow(dead_code)]
mod ipfs;
mod mcp;
// Phase-2 board-driven ingest pipeline (`kb pipeline --role uploader|embedder`, #238). Ported incrementally;
// its own `allow(dead_code)` covers the routing core landing ahead of the handlers + reactive runtime.
mod pipeline;
// Phase-2 ingest infra: the inbound webhook receiver the board-driven workers register against. Its own
// `allow(dead_code)` (see the module) covers being landed ahead of its callers.
mod rerank;
mod search;
mod store;
mod webhook;

use std::path::PathBuf;
use std::process;

use axum::Router;
use clap::{Parser, Subcommand};
use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
};
use tower_http::cors::CorsLayer;
use tower_http::trace::TraceLayer;

use crate::store::Store;

#[derive(Parser)]
#[command(
    name = "kb",
    about = "Curated knowledge base: Qdrant vector search + MCP server"
)]
struct Cli {
    /// TOML config file (see config.example.toml). Omit for built-in defaults.
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the MCP server over streamable-HTTP at /mcp (the always-on agent-facing service).
    Serve,
    /// Search the knowledge base from the terminal (mirrors the Python `kb.cli`).
    Search {
        /// The query text.
        query: String,
        /// Collection to search; the configured default when omitted.
        #[arg(long)]
        collection: Option<String>,
        #[arg(long, default_value_t = 5)]
        limit: usize,
        /// Include outdated/superseded items (the Python `--all`).
        #[arg(long)]
        all: bool,
    },
    /// Drain the drop-folder inbox once: ingest + IPFS-pin each file, then delete (the Python `kb.inbox`).
    Inbox,
    /// Ingest a crate's docs.rs rustdoc JSON into `crate.<name>.<version>` (the Python `kb.crate_docs`).
    CrateDocs {
        /// Crate name as published on docs.rs.
        crate_name: String,
        /// Version to fetch; `latest` resolves to the newest release. The resolved crate_version names the
        /// collection, so `latest` and the explicit version it resolves to ingest into the same place.
        #[arg(long, default_value = "latest")]
        version: String,
        /// Print the would-be points as NDJSON (id + payload + vector fingerprint) on stdout WITHOUT writing
        /// to Qdrant — the crate-docs parity harness. No collection is created, nothing is upserted.
        #[arg(long)]
        dry_run: bool,
    },
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,kb=debug".into()),
        )
        .init();

    let cli = Cli::parse();
    // Record the --config override before the first config::get() (which loads once, lazily).
    config::set_path(cli.config.clone());

    let result = match cli.command {
        Command::Serve => serve().await,
        Command::Search {
            query,
            collection,
            limit,
            all,
        } => {
            // search() awaits Qdrant IO and runs embed/rerank off-reactor internally (#439).
            run_search(&query, collection, limit, all).await
        }
        Command::Inbox => inbox::run().await,
        Command::CrateDocs {
            crate_name,
            version,
            dry_run,
        } => crate_docs::ingest_crate(&crate_name, &version, dry_run)
            .await
            .map(|(chunks, collection)| {
                if dry_run {
                    // Points already printed as NDJSON on stdout; keep the summary on stderr so stdout stays
                    // pure NDJSON for the parity diff.
                    eprintln!(
                        "dry-run: {chunks} would-be points for {collection} (NDJSON on stdout; nothing written)"
                    );
                } else {
                    println!("ingested {chunks} chunks into {collection}");
                }
            }),
    };

    if let Err(e) = result {
        eprintln!("kb: {e}");
        process::exit(1);
    }
}

/// The terminal search command — a port of the Python `kb/cli.py` `main`.
async fn run_search(
    query: &str,
    collection: Option<String>,
    limit: usize,
    all: bool,
) -> Result<(), String> {
    let cfg = config::get();
    let collection = collection.unwrap_or_else(|| cfg.default_collection.clone());
    let store = Store::connect()?;
    let results = search::search(&store, &collection, query, limit, all, None).await?;
    if results.is_empty() {
        println!("(no results)");
        return Ok(());
    }
    for r in &results {
        let p = &r.payload;
        let path = p.get("path").and_then(|v| v.as_str()).unwrap_or("?");
        let page = match p.get("page") {
            Some(serde_json::Value::Number(n)) => n.as_i64().filter(|&v| v != 0),
            _ => None,
        };
        let loc = match page {
            Some(pg) => format!("{path} p.{pg}"),
            None => path.to_string(),
        };
        let title = p.get("title").and_then(|v| v.as_str()).unwrap_or("?");
        let kind = p.get("kind").and_then(|v| v.as_str()).unwrap_or("");
        println!(
            "[{:.3}] {title} — {loc}  (id={}, {kind})",
            r.final_score, r.id
        );
        let text = p.get("text").and_then(|v| v.as_str()).unwrap_or("");
        // First 280 chars (by char, matching Python slicing), trimmed.
        let snippet: String = text.chars().take(280).collect();
        println!("  {}\n", snippet.trim());
    }
    Ok(())
}

/// Warm the embedder + reranker so the first query isn't a ~25s cold model load mid-conversation — the
/// Python `_warm`. Best-effort: a warmup failure is logged, not fatal (the server still starts).
fn warm() {
    match embed::embed_query("warmup")
        .and_then(|_| rerank::rerank("warmup", &["warmup".to_string()]))
    {
        Ok(_) => tracing::info!("models warmed"),
        Err(e) => tracing::warn!("warmup skipped: {e}"),
    }
}

/// Run the MCP server — the Python `server.main`. Warms models, then serves streamable-HTTP at `/mcp`.
async fn serve() -> Result<(), String> {
    let cfg = config::get();

    // Warm on a blocking thread (model load is CPU-heavy) before binding, matching the Python startup order.
    tokio::task::spawn_blocking(warm)
        .await
        .map_err(|e| format!("warmup task panicked: {e}"))?;

    let ct = tokio_util::sync::CancellationToken::new();
    // rmcp defaults to a loopback-only Host allowlist (DNS-rebinding protection). The Python server accepted
    // any Host (LAN agents connect directly to 0.0.0.0), so apply the configured allowlist: `["*"]` disables
    // the check (the default, preserving Python behaviour), a non-empty list restricts to it.
    let mut mcp_config =
        StreamableHttpServerConfig::default().with_cancellation_token(ct.child_token());
    if cfg.mcp_allowed_hosts.iter().any(|h| h == "*") {
        tracing::warn!(
            "MCP Host validation disabled (mcp_allowed_hosts = [\"*\"]); any Host accepted"
        );
        mcp_config = mcp_config.disable_allowed_hosts();
    } else if !cfg.mcp_allowed_hosts.is_empty() {
        tracing::info!("MCP allowed hosts: {:?}", cfg.mcp_allowed_hosts);
        mcp_config = mcp_config.with_allowed_hosts(cfg.mcp_allowed_hosts.clone());
    }
    let mcp_service = StreamableHttpService::new(
        || Ok(mcp::Kb::new()),
        LocalSessionManager::default().into(),
        mcp_config,
    );

    let router = Router::new()
        .nest_service("/mcp", mcp_service)
        .layer(CorsLayer::permissive())
        .layer(TraceLayer::new_for_http());

    let addr = format!("{}:{}", cfg.mcp_host, cfg.mcp_port);
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .map_err(|e| format!("bind {addr} failed: {e}"))?;
    tracing::info!("kb MCP listening on http://{addr}/mcp");

    axum::serve(listener, router)
        .with_graceful_shutdown(async move {
            let _ = tokio::signal::ctrl_c().await;
            ct.cancel();
        })
        .await
        .map_err(|e| format!("server error: {e}"))
}
