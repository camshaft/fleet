//! `voice-assistant` binary — the audio+ML daemon. Two modes:
//!   - default: load config, start the board webhook, run the voice loop.
//!   - `scope-guard <projects_dir>`: the `PreToolUse` hook the brain installs via `--settings`; reads the
//!     hook payload on stdin and prints a deny/allow decision. It's the same binary re-invoked (so there's
//!     no second artifact to deploy).
//!
//! Only `--config` is a CLI flag; every other knob is in the TOML file (operator mandate: no env vars).

use std::path::PathBuf;

use clap::Parser;
use voice_assistant::{config, runtime, scope};

#[derive(Parser)]
#[command(
    name = "voice-assistant",
    about = "Local voice loop: wake → STT → Claude → TTS"
)]
struct Cli {
    /// Path to the TOML config (else $XDG_CONFIG_HOME/voice-assistant/config.toml, else ~/.config/…).
    #[arg(long, value_name = "PATH")]
    config: Option<PathBuf>,

    /// Internal: run as the filesystem-scope `PreToolUse` hook (reads payload on stdin). Not for direct
    /// use — the brain invokes `voice-assistant scope-guard <projects_dir>`.
    #[arg(long = "scope-guard", value_name = "PROJECTS_DIR", hide = true)]
    scope_guard: Option<PathBuf>,
}

fn main() {
    let cli = Cli::parse();

    // Hook mode: decide on the payload and exit, before touching config/audio.
    if let Some(projects_dir) = cli.scope_guard {
        scope::run_from_stdin(&projects_dir);
        return;
    }

    config::set_path(cli.config);
    let cfg = match config::load() {
        Ok(c) => c.clone(),
        Err(e) => {
            eprintln!("[voice-assistant] {e}");
            std::process::exit(1);
        }
    };

    // How the brain re-invokes us as the FS-scope hook: our own path + the subcommand + the projects dir.
    let self_exe = std::env::current_exe()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| "voice-assistant".to_string());
    let scope_guard_command = format!(
        "{self_exe} --scope-guard {}",
        cfg.brain.projects_dir.display()
    );

    if let Err(e) = runtime::run(cfg, scope_guard_command) {
        eprintln!("[voice-assistant] fatal: {e}");
        std::process::exit(1);
    }
}
