//! The daemon's TOML config (operator mandate #159 — no env-var config). See config.example.toml.

use serde::Deserialize;
use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

const DEFAULT_UPSTREAM: &str = "http://127.0.0.1:8899";

/// Default cadence for re-deriving the served-agent set on a live connection (#449): often enough that a
/// newly-spun/relocated agent starts receiving event-wakes within ~1.5min without a manual tunnel restart,
/// rare enough that the `fleet served-set` subprocess is negligible.
const DEFAULT_SERVED_REFRESH_SECS: u64 = 90;

/// Parsed daemon config. Keys mirror the original Python daemon 1:1.
#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    /// REQUIRED. Board websocket URL to dial (ws:// on-LAN, wss:// off-LAN public gateway).
    /// Defaulted so an absent value produces our clean "required" error, not serde's "missing field".
    #[serde(default)]
    pub board_ws: String,
    /// This host's id in the hello frame. Falls back to the system hostname when unset/empty.
    #[serde(default)]
    pub host_id: Option<String>,
    /// Agent ids this host serves; the board keys live tunnels by this set. Used verbatim UNLESS
    /// `agents_cmd` is set and yields a non-empty list, in which case this is the fallback.
    #[serde(default)]
    pub agents: Vec<String>,
    /// Optional command to DERIVE the served-agent set at each (re)connect — stdout is read one agent
    /// id per line. When set, it overrides the static `agents` list, so a moved/new agent window is
    /// picked up hands-free on the next reconnect (no static-list edit). On command failure or empty
    /// output the daemon falls back to `agents`. Example: `agents_cmd = "fleet served-set"`.
    #[serde(default)]
    pub agents_cmd: Option<String>,
    /// Optional per-host bearer sent in the hello frame.
    #[serde(default)]
    pub token: Option<String>,
    /// Local upstream base URL the daemon forwards board requests to (the notifier).
    #[serde(default = "default_upstream")]
    pub upstream: String,
    /// Cloudflare Access service-token id (off-LAN / public-gateway dial).
    #[serde(default)]
    pub cf_client_id: Option<String>,
    /// Cloudflare Access service-token secret.
    #[serde(default)]
    pub cf_client_secret: Option<String>,
    /// Optional loopback address for the health/liveness HTTP probe (e.g. "127.0.0.1:8898").
    /// Unset = probe disabled and the daemon binds no inbound port (its default posture).
    #[serde(default)]
    pub health_addr: Option<String>,
    /// How often (seconds) to re-derive the served-agent set on a LIVE connection and, if it changed,
    /// reconnect to re-register (#449). Only meaningful with `agents_cmd` (a static list never changes).
    /// Unset = the built-in default; `0` disables the periodic refresh (fall back to refresh-only-on-reconnect).
    #[serde(default)]
    pub served_refresh_secs: Option<u64>,
}

fn default_upstream() -> String {
    DEFAULT_UPSTREAM.to_string()
}

impl Config {
    /// Parse + validate a config from a TOML string.
    pub fn from_toml_str(s: &str) -> Result<Self, ConfigError> {
        let cfg: Config = toml::from_str(s).map_err(ConfigError::Parse)?;
        cfg.validate()?;
        Ok(cfg)
    }

    /// Read + parse + validate a config from a TOML file.
    pub fn from_toml_path(path: &Path) -> Result<Self, ConfigError> {
        let s =
            std::fs::read_to_string(path).map_err(|e| ConfigError::Read(path.to_path_buf(), e))?;
        Self::from_toml_str(&s)
    }

    fn validate(&self) -> Result<(), ConfigError> {
        if self.board_ws.trim().is_empty() {
            return Err(ConfigError::MissingBoardWs);
        }
        Ok(())
    }

    /// The hello-frame host id: the configured value, else the system hostname.
    pub fn host_id_or_hostname(&self) -> String {
        self.host_id
            .clone()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(hostname)
    }

    /// Upstream base URL without a trailing slash (paths are appended verbatim).
    pub fn upstream_trimmed(&self) -> &str {
        self.upstream.trim_end_matches('/')
    }

    /// The `agents_cmd` split into an argv (whitespace-separated, no shell), or `None` when unset/blank.
    /// Whitespace-only tokens are dropped; an all-blank command is treated as unset. Pure — unit-tested.
    pub fn agents_cmd_argv(&self) -> Option<Vec<String>> {
        let cmd = self.agents_cmd.as_deref()?;
        let argv: Vec<String> = cmd.split_whitespace().map(str::to_string).collect();
        if argv.is_empty() { None } else { Some(argv) }
    }

    /// The health-probe bind address (trimmed, non-empty), or None when the probe is disabled.
    /// Returned as a string; the transport shell parses it to a `SocketAddr`.
    pub fn health_bind(&self) -> Option<&str> {
        self.health_addr
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
    }

    /// How often to re-derive the served set on a live connection and reconnect if it changed (#449).
    /// Only when `agents_cmd` derives the set dynamically — a static `agents` list never changes under us,
    /// so there is nothing to refresh. Default [`DEFAULT_SERVED_REFRESH_SECS`] when `agents_cmd` is set;
    /// `served_refresh_secs = 0` disables it. `None` = disabled (static list, or explicitly off). Pure.
    pub fn served_refresh_interval(&self) -> Option<Duration> {
        self.agents_cmd_argv()?; // no dynamic derivation → the set can't change under us → nothing to poll
        let secs = self.served_refresh_secs.unwrap_or(DEFAULT_SERVED_REFRESH_SECS);
        (secs > 0).then(|| Duration::from_secs(secs))
    }

    /// The Cloudflare Access service-token pair, if both are present + non-empty.
    pub fn cf_credentials(&self) -> Option<(String, String)> {
        match (&self.cf_client_id, &self.cf_client_secret) {
            (Some(id), Some(secret)) if !id.trim().is_empty() && !secret.trim().is_empty() => {
                Some((id.clone(), secret.clone()))
            }
            _ => None,
        }
    }
}

/// The system hostname, read without relying on environment variables (mandate #159).
fn hostname() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "fleet-host".to_string())
}

/// Parse an `agents_cmd`'s stdout into a served-agent list: one id per line, trimmed, blanks dropped,
/// de-duplicated (order preserved). `fleet served-set` already emits sorted/unique ids, but the dedup +
/// trim keep this robust to any command. Pure — unit-tested.
pub fn parse_agent_lines(stdout: &str) -> Vec<String> {
    let mut seen = std::collections::BTreeSet::new();
    stdout
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .filter(|l| seen.insert(l.to_string()))
        .map(str::to_string)
        .collect()
}

/// Whether two served-agent lists differ as SETS (order-insensitive) — the trigger to reconnect and
/// re-register the fresh set (#449). `parse_agent_lines` already de-dups + preserves order, but comparing
/// as sets is robust to any ordering difference between two derivations, so a mere re-order never forces a
/// needless reconnect. Pure — unit-tested.
pub fn served_set_differs(a: &[String], b: &[String]) -> bool {
    use std::collections::BTreeSet;
    a.iter().collect::<BTreeSet<_>>() != b.iter().collect::<BTreeSet<_>>()
}

/// Config load errors.
#[derive(Debug)]
pub enum ConfigError {
    Read(PathBuf, std::io::Error),
    Parse(toml::de::Error),
    MissingBoardWs,
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConfigError::Read(p, e) => write!(f, "cannot read config {}: {e}", p.display()),
            ConfigError::Parse(e) => write!(f, "invalid TOML: {e}"),
            ConfigError::MissingBoardWs => write!(f, "`board_ws` is required"),
        }
    }
}

impl std::error::Error for ConfigError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn minimal_config_applies_defaults() {
        let cfg = Config::from_toml_str(r#"board_ws = "ws://127.0.0.1:8079/tunnel/ws""#).unwrap();
        assert_eq!(cfg.board_ws, "ws://127.0.0.1:8079/tunnel/ws");
        assert_eq!(cfg.upstream, DEFAULT_UPSTREAM);
        assert!(cfg.agents.is_empty());
        assert!(
            cfg.agents_cmd.is_none(),
            "absent agents_cmd → use the static list"
        );
        assert!(cfg.token.is_none());
        assert!(cfg.cf_credentials().is_none());
        assert!(cfg.health_bind().is_none()); // probe disabled by default
    }

    #[test]
    fn health_addr_parses_and_trims() {
        let cfg = Config::from_toml_str(
            r#"
            board_ws = "ws://x/tunnel/ws"
            health_addr = "  127.0.0.1:8898  "
            "#,
        )
        .unwrap();
        assert_eq!(cfg.health_bind(), Some("127.0.0.1:8898"));
        // an empty/whitespace value is treated as disabled
        let off = Config::from_toml_str(
            r#"
            board_ws = "ws://x/tunnel/ws"
            health_addr = "   "
            "#,
        )
        .unwrap();
        assert!(off.health_bind().is_none());
    }

    #[test]
    fn missing_board_ws_is_an_error() {
        assert!(matches!(
            Config::from_toml_str("agents = []"),
            Err(ConfigError::MissingBoardWs)
        ));
        assert!(matches!(
            Config::from_toml_str(r#"board_ws = "   ""#),
            Err(ConfigError::MissingBoardWs)
        ));
    }

    #[test]
    fn full_config_parses() {
        let cfg = Config::from_toml_str(
            r#"
            board_ws = "wss://green-machine.camshaft.dev/tunnel/ws"
            host_id = "dev-desk"
            agents = ["a", "b", "c"]
            token = "sekret"
            upstream = "http://127.0.0.1:9000/"
            cf_client_id = "cid"
            cf_client_secret = "csec"
            "#,
        )
        .unwrap();
        assert_eq!(cfg.host_id_or_hostname(), "dev-desk");
        assert_eq!(cfg.agents, vec!["a", "b", "c"]);
        assert_eq!(cfg.token.as_deref(), Some("sekret"));
        // trailing slash trimmed so `path` (which starts with /) appends cleanly.
        assert_eq!(cfg.upstream_trimmed(), "http://127.0.0.1:9000");
        assert_eq!(cfg.cf_credentials(), Some(("cid".into(), "csec".into())));
    }

    #[test]
    fn agents_cmd_argv_splits_or_none() {
        let none = Config::from_toml_str(r#"board_ws = "ws://x/tunnel/ws""#).unwrap();
        assert_eq!(none.agents_cmd_argv(), None, "absent → static list");
        let set = Config::from_toml_str(
            r#"
            board_ws = "ws://x/tunnel/ws"
            agents_cmd = "  fleet   served-set  "
            "#,
        )
        .unwrap();
        assert_eq!(
            set.agents_cmd_argv(),
            Some(vec!["fleet".into(), "served-set".into()])
        );
        let blank = Config::from_toml_str(
            r#"
            board_ws = "ws://x/tunnel/ws"
            agents_cmd = "   "
            "#,
        )
        .unwrap();
        assert_eq!(
            blank.agents_cmd_argv(),
            None,
            "all-blank command → treated as unset"
        );
    }

    #[test]
    fn parse_agent_lines_trims_drops_blanks_and_dedups() {
        let out = "v-a\n v-b \n\n  \nv-a\nv-c\n";
        assert_eq!(parse_agent_lines(out), vec!["v-a", "v-b", "v-c"]);
        assert!(
            parse_agent_lines("   \n\n").is_empty(),
            "no ids → empty (caller falls back to static)"
        );
    }

    #[test]
    fn served_set_differs_is_order_insensitive_and_detects_membership_change() {
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        // Same members, different order → NOT different (no needless reconnect).
        assert!(!served_set_differs(&s(&["a", "b", "c"]), &s(&["c", "a", "b"])));
        assert!(!served_set_differs(&s(&["a"]), &s(&["a"])));
        // A NEW agent (the #449 case: george just appeared) → different → reconnect.
        assert!(served_set_differs(&s(&["a", "b"]), &s(&["a", "b", "george"])));
        // A removed agent → different.
        assert!(served_set_differs(&s(&["a", "b"]), &s(&["a"])));
        // Empty vs non-empty.
        assert!(served_set_differs(&[], &s(&["a"])));
    }

    #[test]
    fn served_refresh_interval_only_with_agents_cmd_and_respects_disable() {
        // Static list (no agents_cmd) → nothing to refresh, even if a value is set.
        let static_list = Config::from_toml_str(
            r#"
            board_ws = "ws://x/tunnel/ws"
            agents = ["a"]
            served_refresh_secs = 30
            "#,
        )
        .unwrap();
        assert_eq!(static_list.served_refresh_interval(), None, "no agents_cmd → static set never changes");
        // agents_cmd set, no explicit secs → the built-in default.
        let dyn_default = Config::from_toml_str(
            r#"
            board_ws = "ws://x/tunnel/ws"
            agents_cmd = "fleet served-set"
            "#,
        )
        .unwrap();
        assert_eq!(
            dyn_default.served_refresh_interval(),
            Some(Duration::from_secs(DEFAULT_SERVED_REFRESH_SECS))
        );
        // agents_cmd set, explicit override.
        let dyn_override = Config::from_toml_str(
            r#"
            board_ws = "ws://x/tunnel/ws"
            agents_cmd = "fleet served-set"
            served_refresh_secs = 45
            "#,
        )
        .unwrap();
        assert_eq!(dyn_override.served_refresh_interval(), Some(Duration::from_secs(45)));
        // 0 disables the periodic refresh (back to refresh-only-on-reconnect).
        let disabled = Config::from_toml_str(
            r#"
            board_ws = "ws://x/tunnel/ws"
            agents_cmd = "fleet served-set"
            served_refresh_secs = 0
            "#,
        )
        .unwrap();
        assert_eq!(disabled.served_refresh_interval(), None, "0 → disabled");
    }

    #[test]
    fn partial_cf_credentials_are_ignored() {
        let cfg = Config::from_toml_str(
            r#"
            board_ws = "ws://x/tunnel/ws"
            cf_client_id = "only-id"
            "#,
        )
        .unwrap();
        assert!(cfg.cf_credentials().is_none());
    }
}
