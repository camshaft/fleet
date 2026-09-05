//! `fleet` — the standalone multi-repo agent-fleet orchestrator.
//!
//! P1 CORE LIFT (in progress). Behavior is lifted from cadenza's `xtask/src/fleet.rs` in byte-identical
//! slices while the LIVE fleet keeps running on cadenza-xtask until the P3 cutover (see ../../DESIGN.md).
//! This slice = the FOUNDATION: the hub-config decouple + the registry types + the message-bus stamp dirs
//! + `heartbeat` (the first end-to-end comms command, proving the decouple works standalone).

// P1-SCAFFOLD ALLOW: the registry types + load/save/inbox are the foundation the NEXT lift slices wire
// (inbox/send/add/up). They are constructed+exercised by the tests already; drop this allow once the
// message-bus subcommands land and use them in non-test code.
#![allow(dead_code)]

use std::path::{Path, PathBuf};

use clap::{Parser, Subcommand};
use serde::{Deserialize, Serialize};

/// One agent's durable row in the runtime registry (the machine-local manifest that survives a reboot).
/// Lifted verbatim from cadenza fleet.rs so the registry.json format is byte-identical across the cutover.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Agent {
    /// Unique agent name — also the tmux window name and the inbox directory name.
    name: String,
    /// The role prompt body under `<hub>/loops/<role>.md` this agent runs.
    role: String,
    /// For a `vertical` agent, the feature it owns (e.g. `iterators`); empty otherwise.
    #[serde(default)]
    vertical: String,
    /// For a `vertical` agent, the subsystem the feature lives in; empty otherwise.
    #[serde(default)]
    area: String,
    /// The agent's git worktree (absolute path).
    worktree: String,
    /// The branch checked out in `worktree`.
    branch: String,
    /// The `/loop` interval the window drives the role at (e.g. `10m`).
    interval: String,
    /// The model alias this agent runs under (`opus` for most; `fable` for `breaker`).
    model: String,
    /// The reasoning-effort level the window launches with.
    #[serde(default = "default_effort")]
    effort: String,
    /// `active` (loop should be running) or `stopped` (removed — window kept for scrollback).
    status: String,
    /// Whether the window is launched with `--disallowedTools AskUserQuestion`.
    #[serde(default = "default_true")]
    disallow_ask: bool,
}

fn default_true() -> bool {
    true
}
fn default_effort() -> String {
    "high".to_string()
}

/// The on-disk runtime manifest: just the list of agents. Kept flat so it is easy to read + diff.
#[derive(Default, Serialize, Deserialize)]
struct Registry {
    #[serde(default)]
    agents: Vec<Agent>,
}

/// Filesystem anchors for the machine-local runtime state (the HUB). In the standalone fleet the hub is
/// selected by EXPLICIT config (`$FLEET_HUB`), decoupled from any one target repo's git dir — the central
/// change of the extraction. During P1 the default (when `$FLEET_HUB` is unset) reproduces cadenza's exact
/// behavior: resolve the hub via `git --git-common-dir` of the cwd, so a `fleet` binary run inside cadenza
/// resolves the SAME `<cadenza>/.claude/fleet` hub as cadenza-xtask does today → byte-identical.
struct Fleet {
    /// `<hub>/.claude/fleet` — the machine-local runtime state dir (registry, inbox, heartbeat, stop, …).
    root: PathBuf,
}

impl Fleet {
    /// Resolve the hub. `$FLEET_HUB` (the eventual `~/.fleet` multi-repo model) wins; otherwise fall back
    /// to git-common-dir discovery of the cwd (cadenza's current behavior) so P1 is byte-identical.
    fn resolve() -> Self {
        let hub = match std::env::var_os("FLEET_HUB") {
            Some(h) => PathBuf::from(h),
            None => {
                let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
                hub_root(&cwd).unwrap_or(cwd)
            }
        };
        Fleet {
            root: hub.join(".claude/fleet"),
        }
    }

    fn registry_path(&self) -> PathBuf {
        self.root.join("registry.json")
    }
    fn inbox(&self, agent: &str) -> PathBuf {
        self.root.join("inbox").join(agent)
    }
    fn stopfile(&self, agent: &str) -> PathBuf {
        self.root.join("stop").join(agent)
    }

    /// Load the runtime registry; empty on absent/malformed (never a hard fail — the manifest is recreated).
    fn load(&self) -> Registry {
        let p = self.registry_path();
        match std::fs::read_to_string(&p) {
            Ok(s) => serde_json::from_str(&s).unwrap_or_else(|e| {
                eprintln!("fleet: {p:?} is not valid JSON ({e}); starting from an empty registry");
                Registry::default()
            }),
            Err(_) => Registry::default(),
        }
    }

    /// Persist the registry pretty-printed via temp+rename so a concurrent reader never sees a half-write.
    fn save(&self, reg: &Registry) {
        std::fs::create_dir_all(&self.root).expect("create .claude/fleet");
        let json = serde_json::to_string_pretty(reg).expect("serialize registry");
        let tmp = self.registry_path().with_extension("json.tmp");
        std::fs::write(&tmp, json).expect("write registry tmp");
        std::fs::rename(&tmp, self.registry_path()).expect("rename registry into place");
    }
}

/// `<hub>/.git` → `<hub>` via `git --git-common-dir` (cadenza's current hub discovery; the P1 default).
fn hub_root(dir: &Path) -> Option<PathBuf> {
    let out = std::process::Command::new("git")
        .current_dir(dir)
        .args(["rev-parse", "--path-format=absolute", "--git-common-dir"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let common = String::from_utf8(out.stdout).ok()?;
    PathBuf::from(common.trim()).parent().map(Path::to_path_buf)
}

/// Refresh an agent's liveness touch-file (`<hub>/.claude/fleet/heartbeat/<name>`), or report STOPPED if a
/// stop-file exists. Returns `true` iff stopped. A per-agent touch-file (not a registry rewrite) keeps the
/// tick cheap + avoids contending with `add`/`remove`. Lifted verbatim from cadenza fleet.rs.
fn heartbeat_refresh_liveness(fleet: &Fleet, name: &str) -> bool {
    if fleet.stopfile(name).exists() {
        return true;
    }
    let dir = fleet.root.join("heartbeat");
    std::fs::create_dir_all(&dir).ok();
    std::fs::write(dir.join(name), "tick\n").ok();
    false
}

fn heartbeat(fleet: &Fleet, name: &str) {
    if heartbeat_refresh_liveness(fleet, name) {
        println!("STOPPED");
        std::process::exit(2);
    }
    println!("ok");
}

#[derive(Parser)]
#[command(
    name = "fleet",
    about = "Standalone multi-repo agent-fleet orchestrator"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Refresh an agent's liveness stamp (prints `ok`, or `STOPPED`+exit 2 if a stop-file exists).
    Heartbeat {
        /// The agent name.
        name: String,
    },
}

fn main() {
    let cli = Cli::parse();
    let fleet = Fleet::resolve();
    match cli.cmd {
        Cmd::Heartbeat { name } => heartbeat(&fleet, &name),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    // A UNIQUE hub dir per call (pid + a monotonic counter) so parallel tests never share/clobber state.
    fn tmp_hub() -> (PathBuf, Fleet) {
        static N: AtomicU64 = AtomicU64::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let base = std::env::temp_dir().join(format!("fleet-test-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let fleet = Fleet {
            root: base.join(".claude/fleet"),
        };
        (base, fleet)
    }

    #[test]
    fn heartbeat_writes_a_liveness_stamp_and_reports_not_stopped() {
        let (base, fleet) = tmp_hub();
        assert!(
            !heartbeat_refresh_liveness(&fleet, "a1"),
            "no stop-file → not stopped"
        );
        assert!(
            fleet.root.join("heartbeat/a1").exists(),
            "liveness touch-file written"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn heartbeat_reports_stopped_when_a_stopfile_exists() {
        let (base, fleet) = tmp_hub();
        std::fs::create_dir_all(fleet.root.join("stop")).unwrap();
        std::fs::write(fleet.stopfile("a2"), "").unwrap();
        assert!(
            heartbeat_refresh_liveness(&fleet, "a2"),
            "stop-file present → stopped"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn registry_round_trips_through_save_load() {
        let (base, fleet) = tmp_hub();
        let reg = Registry {
            agents: vec![Agent {
                name: "v-x".into(),
                role: "vertical".into(),
                vertical: "iterators".into(),
                area: "rcdzc".into(),
                worktree: "/wt/v-x".into(),
                branch: "topic/v-x".into(),
                interval: "10m".into(),
                model: "opus".into(),
                effort: "high".into(),
                status: "active".into(),
                disallow_ask: true,
            }],
        };
        fleet.save(&reg);
        let back = fleet.load();
        assert_eq!(back.agents.len(), 1);
        assert_eq!(back.agents[0].name, "v-x");
        assert!(
            fleet.inbox(&back.agents[0].name).ends_with("v-x"),
            "inbox path is <hub>/inbox/<name>"
        );
        let _ = std::fs::remove_dir_all(&base);
    }
}
