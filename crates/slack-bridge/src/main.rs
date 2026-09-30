//! slack-bridge daemon — the async Socket Mode transport that wires the pure core (this crate's `config` +
//! `format`, plus `bridge_core`'s board client / sync planning / channel map) into a live board↔Slack sync.
//! Behind the `transport` feature.
//!
//! Two concurrent jobs on one tokio runtime:
//!   1. OUTBOUND (board → Slack): poll the firehose, reflect authorized `channel.outbound_reflect` posts
//!      to the mapped Slack channel, advancing a persisted cursor.
//!   2. INBOUND (Slack → board): a Socket Mode listener; an operator's Slack message in a mapped channel
//!      is posted to the board channel, attributed to the Slack user.
//!
//! FAIL-SOFT: with no Slack tokens in the config the process stays alive but idle (a restart picks up
//! tokens once the operator provides them). Any Slack/IO error is logged and retried, never a crash.
//!
//! This binary is intentionally thin — every decision lives in the unit-tested lib. It is not itself
//! unit-tested (live WebSocket); the gate is the lib's `cargo test`.

mod runner;

use clap::Parser;
use slack_bridge::config::DEFAULT_CONFIG_FILENAME;
use slack_bridge::Config;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::Duration;

#[derive(Parser, Debug)]
#[command(name = "slack-bridge", about = "Fleet Slack↔board bridge daemon")]
struct Cli {
    /// Path to the TOML config file (credentials + wiring). This is the ONLY thing chosen outside the
    /// file — there is no env-var configuration (operator mandate #159).
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
    let cfg = Arc::new(Config::load(&config_path));
    let map: runner::SharedMap = Arc::new(RwLock::new(runner::fetch_channel_map(&cfg).await));
    let channels = map.read().map(|m| m.len()).unwrap_or(0);
    tracing::info!(
        config = %config_path.display(),
        board_api = %cfg.board_api,
        bridge_agent = %cfg.bridge_agent,
        default_to = %cfg.default_to,
        channels,
        "slack↔board bridge starting"
    );

    match cfg.tokens() {
        Some(tokens) => {
            tracing::info!("slack tokens present — starting Socket Mode + the outbound reflect loop");
            let outbound =
                tokio::spawn(runner::outbound_loop(cfg.clone(), tokens.clone(), map.clone()));
            // Keep the channel map fresh so a link registered while running (e.g. the #154 operator-DM
            // wiring) is honored without a restart.
            let refresh = tokio::spawn(runner::refresh_loop(cfg.clone(), map.clone()));
            // The inbound Socket Mode listener blocks until the socket closes / a fatal error.
            if let Err(e) = runner::run_socket_mode(cfg.clone(), tokens, map.clone()).await {
                tracing::error!(error = %e, "socket mode listener exited");
            }
            outbound.abort();
            refresh.abort();
        }
        None => {
            tracing::warn!(
                "no Slack tokens in {} (set bot_token + app_token) — idle until provided",
                config_path.display()
            );
            // Dormant: stay alive so a restart picks up tokens once the operator writes them.
            loop {
                tokio::time::sleep(Duration::from_secs(3600)).await;
            }
        }
    }
}
