//! github-bridge daemon — a thin ASYNC poll loop that wires the pure core (`config` + `board` + `github`
//! + `sync` + `state`) into a live GitHub↔board sync. Behind the `daemon` feature.
//!
//! Runs on tokio (operator directive: NO blocking IO in rust daemons). The two sync directions run as
//! INDEPENDENT concurrent loops (see [`runner`]) so neither blocks the other, with no thread-per-direction:
//!   1. IN  (GitHub → board): poll issues + comments, ingest new issues as attributed tasks and new comments
//!      as attributed board comments (idempotent via the board's external_links).
//!   2. OUT (board → GitHub): poll the board firehose and post each authorized `task.outbound_reflect` as a
//!      comment on the linked GitHub issue.
//!
//! FAIL-SOFT: with no `github_token` in the config the process stays alive but idle (a restart picks up the
//! token once the operator provides it). Any GitHub/board/IO error is logged and retried next tick, never a
//! crash. This binary is intentionally thin — every decision lives in the unit-tested lib.

mod runner;

use clap::Parser;
use github_bridge::Config;
use github_bridge::config::DEFAULT_CONFIG_FILENAME;
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(name = "github-bridge", about = "Fleet GitHub↔board bridge daemon")]
struct Cli {
    /// Path to the TOML config file (token + wiring). This is the ONLY thing chosen outside the file — there
    /// is no env-var configuration (operator mandate #159).
    #[arg(long)]
    config: Option<PathBuf>,
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();
    let config_path = cli
        .config
        .unwrap_or_else(|| PathBuf::from(DEFAULT_CONFIG_FILENAME));
    let cfg = Config::load(&config_path);
    let repos = if cfg.repos.is_empty() {
        "<none>".to_string()
    } else {
        cfg.repos.join(",")
    };
    tracing::info!(
        config = %config_path.display(),
        board_api = %cfg.board_api,
        api_base = %cfg.api_base,
        bridge_agent = %cfg.bridge_agent,
        %repos,
        "github↔board bridge starting"
    );

    // Runs until SIGTERM/ctrl-c: the concurrent IN/OUT poll loops, or a dormant sleep when no token is
    // configured (fail-soft).
    runner::run(cfg).await;
}
