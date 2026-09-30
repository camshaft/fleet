//! `voice-assistant` binary — the audio daemon + board bridge. Loads config and runs the voice loop:
//! wake → STT → post the transcript to a board voice channel → speak George's reply (Doc #18 / #316).
//!
//! Only `--config` is a CLI flag; every other knob is in the TOML file (operator mandate: no env vars).

use std::path::PathBuf;

use clap::Parser;
use voice_assistant::{config, runtime};

#[derive(Parser)]
#[command(
    name = "voice-assistant",
    about = "Local voice bridge: wake → STT → board → spoken reply"
)]
struct Cli {
    /// Path to the TOML config (else $XDG_CONFIG_HOME/voice-assistant/config.toml, else ~/.config/…).
    #[arg(long, value_name = "PATH")]
    config: Option<PathBuf>,
}

fn main() {
    let cli = Cli::parse();

    config::set_path(cli.config);
    let cfg = match config::load() {
        Ok(c) => c.clone(),
        Err(e) => {
            eprintln!("[voice-assistant] {e}");
            std::process::exit(1);
        }
    };

    if let Err(e) = runtime::run(cfg) {
        eprintln!("[voice-assistant] fatal: {e}");
        std::process::exit(1);
    }
}
