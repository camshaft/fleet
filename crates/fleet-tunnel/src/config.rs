//! The daemon's TOML config (operator mandate #159 — no env-var config). See config.example.toml.

use serde::Deserialize;
use std::fmt;
use std::path::{Path, PathBuf};

const DEFAULT_UPSTREAM: &str = "http://127.0.0.1:8899";

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
    /// Agent ids this host serves; the board keys live tunnels by this set.
    #[serde(default)]
    pub agents: Vec<String>,
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
        assert!(cfg.token.is_none());
        assert!(cfg.cf_credentials().is_none());
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
