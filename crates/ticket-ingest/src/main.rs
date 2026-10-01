//! `ticket-ingest` daemon binary — the thin async shell (behind the `daemon` feature).
//!
//! Parses the one CLI flag the operator mandate allows outside the TOML file (`--config <path>`, mandate
//! #159), initializes structured logging, loads the fail-soft [`Config`], and hands off to the async poll
//! loop in [`ticket_ingest::runner`]. All real logic is in the lib (gated by `cargo test`); this is wiring.

use clap::Parser;
use std::path::PathBuf;
use ticket_ingest::{Config, DEFAULT_CONFIG_FILENAME, runner};

/// `ticket-ingest` — mirror an external ticketing source's tickets onto the coordination board.
#[derive(Parser, Debug)]
#[command(
    name = "ticket-ingest",
    about = "Ticketing-source -> board ingest bridge"
)]
struct Args {
    /// Path to the TOML config file (the ONLY setting chosen outside the file; mandate #159).
    #[arg(long, default_value = DEFAULT_CONFIG_FILENAME)]
    config: PathBuf,
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let args = Args::parse();
    let config = Config::load(&args.config);
    tracing::info!(config = ?config, "loaded config");

    if let Err(e) = runner::run(config).await {
        tracing::error!(error = %e, "ticket-ingest exited with an error");
        std::process::exit(1);
    }
}
