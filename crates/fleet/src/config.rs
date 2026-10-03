//! `config` — TOML-file configuration for the fleet binary.
//!
//! Operator mandate seq-1377: the fleet binary is configured by a TOML file, not `FLEET_*` environment
//! variables. Every fleet-specific knob (tmux session, hub root, workspace root, board API base, launcher
//! path, this process's agent id) is read from the config here. The only environment still consulted is the
//! OS-standard `HOME`/`XDG_CONFIG_HOME` used to locate the config file (and as the workspace-root default) —
//! never a fleet knob. An absent config file or an absent key falls back to the same built-in default as the
//! pre-config binary, so a host with no config behaves exactly as before.

use std::path::PathBuf;
use std::sync::OnceLock;

use serde::Deserialize;

/// The fleet binary's settings. Every field is optional; a missing field uses the built-in default at its
/// use site. Loaded once from the TOML config file (see [`get`]).
#[derive(Debug, Default, Deserialize)]
pub struct Config {
    /// tmux session the fleet's windows live in (default `main`).
    pub session: Option<String>,
    /// File-hub root for legacy runtime state (heartbeat/inbox/registry); default = git-common-dir of cwd.
    pub hub: Option<String>,
    /// Per-agent workspace root for the `~/.fleet` model (default `$HOME/.fleet`).
    pub root: Option<String>,
    /// Board REST base URL (default the loopback front-door proxy).
    pub board_api: Option<String>,
    /// Path to the window launcher script (default `<hub>/.claude/fleet/window.sh`).
    pub window_sh: Option<String>,
    /// This process's agent id — the `fleet send` sender identity when not given explicitly.
    pub agent: Option<String>,
    /// This box's host id for host-affinity: `fleet up`/`watchdog` only manage agents whose `host` metadata
    /// matches this (unpinned agents are unaffected). Default = the system hostname.
    pub host: Option<String>,
    /// The fleet-tunnel health-probe URL (e.g. `http://127.0.0.1:8898/`). When set, `watchdog` GETs it each
    /// sweep and reports the wake-delivery path's health — a non-200 / unreachable probe means a wedged
    /// tunnel (event-wakes are silently not being delivered). Absent → the tunnel-health check is skipped
    /// (a host with no tunnel, e.g. the board host itself).
    pub tunnel_health_url: Option<String>,
    /// The systemd user units `fleet redeploy` restarts after rebuilding a stale binary (the long-running
    /// fleet daemons on this host). Absent → the built-in default set (see `redeploy`). A host with a
    /// different daemon set overrides it here.
    pub redeploy_services: Option<Vec<String>>,
    /// This deployment's operator board-agent id. A stale-task nudge/route never fires on a task assigned to
    /// this id — the operator's own tasks are their work queue, not a stalled deliverable. Absent → no
    /// operator exemption (the generic case: a fleet with no designated operator). The operator id is a
    /// deployment-specific value, so it is named here, never hard-coded in the fleet code.
    pub operator_id: Option<String>,
    /// task_627: the 1-based nudge round at which a stale task's nudge starts also tagging the router (the
    /// operator-accountable backstop) to make a call — chase an ETA, reassign, mark it blocked, or close it —
    /// rather than only pinging the owner. Round 1 is the owner's alone; this defaults to round 2 (the first
    /// unanswered round) at the use site. Config-tunable so the threshold is never hard-coded.
    pub nudge_pm_tag_round: Option<usize>,
    /// task_627: the 1-based nudge round at which a stale task escalates — the nudge tags the router to
    /// reassign it to a fresh agent (mint a helper if needed) rather than wait on the silent owner. Defaults to
    /// round 3 at the use site (= N). Takes precedence over the pm-tag round when both apply. Config-tunable.
    pub nudge_reassign_round: Option<usize>,
    /// task_1123: base directory under which local repo checkouts live, enabling the `dream-run` staleness
    /// detector per scope. The all-scopes runner maps each `repos/<org>-<name>` scope to `<base>/<org>/<name>`
    /// (falling back to `<base>/<slug>`); when the checkout exists the verified-dangling staleness detector
    /// runs against it, otherwise that scope stays corpus-only. Absent → no base, so every scope is corpus-only
    /// (the first-cut behavior). The checkout root is a deployment-specific path, so it is named here, never
    /// hard-coded; `dream-run --repo-root-base` overrides it.
    pub repo_checkout_base: Option<String>,
    /// task_1217: the board project id of the uncategorized intake inbox the watchdog's `--intake-watch` act
    /// sweeps each pass (dwell + state invariants; see `intake_sweep`). A deployment-specific id, so it is named
    /// here rather than hard-coded. Absent → the `--intake-watch` act is a no-op (zero blast radius on a host
    /// that has not opted the intake project in), so merely shipping the flag changes nothing until a host sets
    /// this and adds the flag to its watchdog `ExecStart`.
    pub intake_project: Option<i64>,
    /// task_695: the systemd user units the infra-observer samples (`systemctl --user is-active <unit>`); a unit
    /// in any non-active state files a project-28 self-improve task. Each unit name doubles as the signal id.
    /// Absent/empty → the systemd probe is a no-op (opt-in per host — a host declares only the units it owns).
    pub infra_systemd_units: Option<Vec<String>>,
    /// task_695: the cron/timer last-run stamps the infra-observer ages — a stamp older than its `max_age_secs`
    /// (or missing entirely) is a stale-liveness breach. Absent/empty → the cron-liveness probe is a no-op. The
    /// watched stamps are deployment-specific paths, so they are named here, never hard-coded.
    pub infra_cron_stamps: Option<Vec<InfraStampSignal>>,
}

/// task_695: a declared file-liveness signal the infra-observer ages — a cron/timer last-run stamp whose file
/// mtime must stay fresher than `max_age_secs`. `path` doubles as the signal id (and the `metadata.observed_signal`
/// dedup key when a breach files a task). A deployment-specific path + budget, so both are config-declared.
#[derive(Debug, Clone, Deserialize)]
pub struct InfraStampSignal {
    /// The stamp file whose mtime is the last-run time (also the signal id).
    pub path: String,
    /// The liveness budget in seconds — a mtime older than this (or an absent file) is a breach.
    pub max_age_secs: u64,
}

static CONFIG: OnceLock<Config> = OnceLock::new();
static PATH_OVERRIDE: OnceLock<Option<PathBuf>> = OnceLock::new();

/// The default config path: `$XDG_CONFIG_HOME/fleet/config.toml`, else `$HOME/.config/fleet/config.toml`,
/// else `None`. `HOME`/`XDG_CONFIG_HOME` are OS-standard locators, not fleet knobs.
fn default_path() -> Option<PathBuf> {
    if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME").filter(|s| !s.is_empty()) {
        return Some(PathBuf::from(xdg).join("fleet/config.toml"));
    }
    std::env::var_os("HOME")
        .filter(|s| !s.is_empty())
        .map(|h| PathBuf::from(h).join(".config/fleet/config.toml"))
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
                        "fleet: config {} is not valid TOML; using defaults",
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
    fn parse_reads_every_knob() {
        let cfg = parse(
            r#"
            session = "fleet-main"
            hub = "/srv/hub"
            root = "/home/x/.fleet"
            board_api = "http://board.local/api"
            window_sh = "/opt/fleet/window.sh"
            agent = "v-fleet-tooling"
            host = "host-b"
            tunnel_health_url = "http://127.0.0.1:8898/"
            operator_id = "operator"
            nudge_pm_tag_round = 2
            nudge_reassign_round = 3
            repo_checkout_base = "/home/x/Projects"
            intake_project = 29
            infra_systemd_units = ["fleet-watchdog.service", "fleet-sync.timer"]

            [[infra_cron_stamps]]
            path = "/srv/hub/.claude/fleet/watchdog/dream-run.stamp"
            max_age_secs = 7200
            "#,
        );
        assert_eq!(cfg.operator_id.as_deref(), Some("operator"));
        assert_eq!(cfg.repo_checkout_base.as_deref(), Some("/home/x/Projects"));
        assert_eq!(cfg.intake_project, Some(29));
        assert_eq!(
            cfg.infra_systemd_units.as_deref(),
            Some(
                &[
                    "fleet-watchdog.service".to_string(),
                    "fleet-sync.timer".to_string()
                ][..]
            )
        );
        let stamps = cfg.infra_cron_stamps.as_deref().expect("stamps parsed");
        assert_eq!(stamps.len(), 1);
        assert_eq!(
            stamps[0].path,
            "/srv/hub/.claude/fleet/watchdog/dream-run.stamp"
        );
        assert_eq!(stamps[0].max_age_secs, 7200);
        assert_eq!(cfg.nudge_pm_tag_round, Some(2));
        assert_eq!(cfg.nudge_reassign_round, Some(3));
        assert_eq!(
            cfg.tunnel_health_url.as_deref(),
            Some("http://127.0.0.1:8898/")
        );
        assert_eq!(cfg.host.as_deref(), Some("host-b"));
        assert_eq!(cfg.session.as_deref(), Some("fleet-main"));
        assert_eq!(cfg.hub.as_deref(), Some("/srv/hub"));
        assert_eq!(cfg.root.as_deref(), Some("/home/x/.fleet"));
        assert_eq!(cfg.board_api.as_deref(), Some("http://board.local/api"));
        assert_eq!(cfg.window_sh.as_deref(), Some("/opt/fleet/window.sh"));
        assert_eq!(cfg.agent.as_deref(), Some("v-fleet-tooling"));
    }

    #[test]
    fn parse_empty_and_partial_default_the_rest() {
        let empty = parse("");
        assert!(empty.session.is_none() && empty.board_api.is_none());
        let partial = parse(r#"session = "s""#);
        assert_eq!(partial.session.as_deref(), Some("s"));
        assert!(
            partial.hub.is_none(),
            "unset keys stay None → use the built-in default"
        );
    }

    #[test]
    fn parse_invalid_toml_is_defaults_not_a_panic() {
        let cfg = parse("this is = = not toml");
        assert!(cfg.session.is_none());
    }
}
