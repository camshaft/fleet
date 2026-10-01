//! Bridge configuration — loaded from a single **TOML config file**. NO environment variables.
//!
//! Operator mandate seq-1377 / task #159 (fleet-wide): every daemon is configured via a TOML file, never
//! env vars ("no env var BS — a pain to maintain"). So ALL config *values* — Slack credentials, the
//! channel, the board REST base, wiring — live in one TOML file. Only the file *path* is chosen outside
//! the file: the daemon takes a `--config <path>` CLI flag (that is not env-var config), defaulting to
//! [`DEFAULT_CONFIG_FILENAME`]. The deploy (task #153) delivers this file as the agenix-decrypted secret
//! (mode 0400, out of the repo); the dev file is gitignored.
//!
//! The bridge must **fail soft**: a missing OR malformed config file yields a valid *dormant* [`Config`]
//! (defaults, no tokens) — logged, never a crash — so it can be built, land, and run before the operator
//! has written the config / created the Slack app. A [`Config`] whose [`Config::tokens`] returns `None`
//! is valid: the caller logs "tokens absent, idle" and the transport loop stays dormant, retrying.

use bridge_core::ChannelLink;
use serde::Deserialize;
use std::fmt;
use std::path::{Path, PathBuf};

/// The localhost board REST base the firehose subscriber reads, used when the config file omits it.
/// This is the board front-door on the deploy host: port 8079, path `/api` (the daemon appends
/// `/events`, `/channels/:id/posts`, `/external-links`). Override per-environment via config `board_api`.
const DEFAULT_BOARD_API: &str = "http://127.0.0.1:8079/api";
const DEFAULT_DEFAULT_TO: &str = "concierge";
const DEFAULT_BRIDGE_AGENT: &str = "slack-bridge";

/// The config filename the daemon reads by default; override with the `--config <path>` CLI flag. This
/// is NOT discovered via any environment variable (mandate #159) — it's a fixed filename the deploy
/// points `--config` at (the agenix-decrypted secret) and dev runs pass explicitly.
pub const DEFAULT_CONFIG_FILENAME: &str = "slack-bridge.toml";

/// Redact a secret for `Debug`: keep only the `xoxb-`/`xapp-` style prefix so logs stay diagnosable
/// without ever printing the token body. SECURITY: these structs hold live Slack credentials; a stray
/// `{:?}`/`dbg!`/panic-format must not leak them, so `Debug` is hand-rolled to redact.
fn redact(secret: &str) -> String {
    match secret.split_once('-') {
        Some((prefix, _)) if !prefix.is_empty() => format!("{prefix}-***"),
        _ if secret.is_empty() => "<unset>".to_string(),
        _ => "***".to_string(),
    }
}

fn redact_opt(secret: &Option<String>) -> String {
    match secret {
        Some(s) => redact(s),
        None => "<none>".to_string(),
    }
}

/// The two Slack credentials the Socket Mode client needs. Present together or not at all — a bridge
/// with only one token can't run, so [`Config::tokens`] yields `Some` only when BOTH are set.
///
/// NOTE: `Debug` is REDACTING (no derive) — see [`redact`].
#[derive(Clone, PartialEq, Eq)]
pub struct SlackTokens {
    /// Bot User OAuth Token (`xoxb-…`) — used for `chat.postMessage` etc.
    pub bot_token: String,
    /// App-Level Token (`xapp-…`, scope `connections:write`) — enables Socket Mode.
    pub app_token: String,
}

impl fmt::Debug for SlackTokens {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SlackTokens")
            .field("bot_token", &redact(&self.bot_token))
            .field("app_token", &redact(&self.app_token))
            .finish()
    }
}

/// Fully-resolved bridge configuration (from the TOML file, with defaults applied).
///
/// NOTE: `Debug` is REDACTING (no derive) so the token fields never print raw.
#[derive(Clone, PartialEq, Eq)]
pub struct Config {
    /// Bot token, if set in the file. `None` = fail-soft dormant mode.
    pub bot_token: Option<String>,
    /// App-level token, if set.
    pub app_token: Option<String>,
    /// The Slack channel ID the bridge posts board→operator messages into (e.g. `C0123ABCD`). Optional:
    /// without it the bridge is inbound-only (DMs) and can't mirror to a default channel.
    pub channel: Option<String>,
    /// The board REST base URL the firehose subscriber reads (localhost front-door proxy by default).
    pub board_api: String,
    /// Default recipient when the operator gives no `@agent` (the concierge).
    pub default_to: String,
    /// This bridge's own board agent name.
    pub bridge_agent: String,
    /// The bridge's local state dir (persisted thread-map etc.). Defaults to the config file's dir.
    pub state_dir: PathBuf,
    /// The board↔Slack channel links (TOML `[[channel_map]]`). Empty = no channels mirrored (dormant),
    /// which is valid. Superseded by a board-backed map once board-core #149 slice 2 lands.
    pub channel_map: Vec<ChannelLink>,
}

impl fmt::Debug for Config {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Config")
            .field("bot_token", &redact_opt(&self.bot_token))
            .field("app_token", &redact_opt(&self.app_token))
            .field("channel", &self.channel)
            .field("board_api", &self.board_api)
            .field("default_to", &self.default_to)
            .field("bridge_agent", &self.bridge_agent)
            .field("state_dir", &self.state_dir)
            .field("channel_map", &self.channel_map)
            .finish()
    }
}

/// The raw TOML shape. Every field optional: secrets absent → dormant; non-secret fields fall back to
/// built-in defaults. `#[serde(deny_unknown_fields)]` so a typo'd key is surfaced (fail-soft: it makes
/// the file "malformed", which [`Config::load`] logs and treats as dormant rather than silently ignoring).
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileConfig {
    bot_token: Option<String>,
    app_token: Option<String>,
    channel: Option<String>,
    board_api: Option<String>,
    default_to: Option<String>,
    bridge_agent: Option<String>,
    state_dir: Option<String>,
    #[serde(default)]
    channel_map: Vec<ChannelLink>,
}

impl Config {
    /// The tokens if and only if BOTH are present — the precondition for starting the transport.
    pub fn tokens(&self) -> Option<SlackTokens> {
        match (&self.bot_token, &self.app_token) {
            (Some(b), Some(a)) if !b.is_empty() && !a.is_empty() => Some(SlackTokens {
                bot_token: b.clone(),
                app_token: a.clone(),
            }),
            _ => None,
        }
    }

    /// Apply defaults to a parsed [`FileConfig`]. `base_dir` (the config file's directory) is the default
    /// `state_dir` when the file doesn't set one. Pure.
    fn from_file_config(file: FileConfig, base_dir: &Path) -> Config {
        let nonempty = |o: Option<String>| o.filter(|s| !s.is_empty());
        Config {
            bot_token: nonempty(file.bot_token),
            app_token: nonempty(file.app_token),
            channel: nonempty(file.channel),
            board_api: nonempty(file.board_api).unwrap_or_else(|| DEFAULT_BOARD_API.to_string()),
            default_to: nonempty(file.default_to)
                .unwrap_or_else(|| DEFAULT_DEFAULT_TO.to_string()),
            bridge_agent: nonempty(file.bridge_agent)
                .unwrap_or_else(|| DEFAULT_BRIDGE_AGENT.to_string()),
            state_dir: nonempty(file.state_dir)
                .map(PathBuf::from)
                .unwrap_or_else(|| base_dir.to_path_buf()),
            channel_map: file.channel_map,
        }
    }

    /// Parse config from a TOML string, applying defaults. `base_dir` is the dir the config file lives in
    /// (the default `state_dir`). Pure — the unit-test entry point. Returns the parse error on malformed
    /// TOML (the fail-soft handling lives in [`Config::load`]).
    pub fn from_toml_str(text: &str, base_dir: &Path) -> Result<Config, toml::de::Error> {
        Ok(Self::from_file_config(toml::from_str(text)?, base_dir))
    }

    /// Load config from the TOML file at `path`, **fail-soft**: a missing OR malformed file yields a
    /// dormant [`Config`] (defaults, no tokens) — a malformed file is logged via `eprintln!` (no
    /// structured logger at config-load time) — rather than an error or crash. `state_dir` defaults to
    /// the config file's own directory.
    pub fn load(path: &Path) -> Config {
        let base_dir = path.parent().unwrap_or_else(|| Path::new("."));
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(_) => return Self::from_file_config(FileConfig::default(), base_dir), // absent = dormant
        };
        match toml::from_str::<FileConfig>(&text) {
            Ok(fc) => Self::from_file_config(fc, base_dir),
            Err(e) => {
                eprintln!("slack-bridge: ignoring malformed {}: {e}", path.display());
                Self::from_file_config(FileConfig::default(), base_dir)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn base() -> PathBuf {
        PathBuf::from("/etc/slack-bridge")
    }

    fn tmp_dir(tag: &str) -> PathBuf {
        static N: AtomicU64 = AtomicU64::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let d = std::env::temp_dir().join(format!("slack-cfg-{}-{}-{}", tag, std::process::id(), n));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn empty_toml_is_dormant_but_valid() {
        let cfg = Config::from_toml_str("", &base()).unwrap();
        assert!(cfg.tokens().is_none(), "no tokens → dormant");
        assert_eq!(cfg.default_to, "concierge");
        assert_eq!(cfg.bridge_agent, "slack-bridge");
        assert_eq!(cfg.state_dir, base(), "state_dir defaults to the config file's dir");
        assert_eq!(
            cfg.board_api, "http://127.0.0.1:8079/api",
            "board_api defaults to the local board front-door"
        );
    }

    #[test]
    fn only_one_token_is_still_dormant() {
        let cfg = Config::from_toml_str("bot_token = \"xoxb-1\"\n", &base()).unwrap();
        assert!(cfg.tokens().is_none(), "one token is not enough to run");
        assert_eq!(cfg.bot_token.as_deref(), Some("xoxb-1"));
    }

    #[test]
    fn both_tokens_yield_tokens() {
        let cfg =
            Config::from_toml_str("bot_token = \"xoxb-1\"\napp_token = \"xapp-2\"\n", &base())
                .unwrap();
        let t = cfg.tokens().expect("both present");
        assert_eq!(t.bot_token, "xoxb-1");
        assert_eq!(t.app_token, "xapp-2");
    }

    #[test]
    fn full_toml_sets_every_field() {
        let toml = r#"
            bot_token = "xoxb-b"
            app_token = "xapp-a"
            channel = "C123"
            board_api = "http://board.local/api"
            default_to = "pr-sync"
            bridge_agent = "sb2"
            state_dir = "/var/lib/slack-bridge"
        "#;
        let cfg = Config::from_toml_str(toml, &base()).unwrap();
        assert!(cfg.tokens().is_some());
        assert_eq!(cfg.channel.as_deref(), Some("C123"));
        assert_eq!(cfg.board_api, "http://board.local/api");
        assert_eq!(cfg.default_to, "pr-sync");
        assert_eq!(cfg.bridge_agent, "sb2");
        assert_eq!(cfg.state_dir, PathBuf::from("/var/lib/slack-bridge"));
    }

    #[test]
    fn parses_channel_map_array_of_tables() {
        let toml = r#"
            bot_token = "xoxb-b"
            app_token = "xapp-a"
            [[channel_map]]
            board_channel_id = 7
            slack_channel = "C7"
            [[channel_map]]
            board_channel_id = 8
            slack_channel = "C8"
        "#;
        let cfg = Config::from_toml_str(toml, &base()).unwrap();
        assert_eq!(cfg.channel_map.len(), 2);
        assert_eq!(cfg.channel_map[0].board_channel_id, 7);
        // The deployed TOML uses the legacy `slack_channel` key (parsed via the bridge_core serde alias).
        assert_eq!(cfg.channel_map[0].external_channel, "C7");
        assert_eq!(cfg.channel_map[1].board_channel_id, 8);
    }

    #[test]
    fn channel_map_defaults_to_empty_when_absent() {
        let cfg = Config::from_toml_str("bot_token = \"xoxb-b\"\n", &base()).unwrap();
        assert!(cfg.channel_map.is_empty());
    }

    #[test]
    fn empty_string_values_fall_back_to_defaults() {
        // An explicitly-empty non-secret string must not blank out the default (treated as unset).
        let cfg = Config::from_toml_str("default_to = \"\"\nbot_token = \"\"\n", &base()).unwrap();
        assert_eq!(cfg.default_to, "concierge");
        assert!(cfg.bot_token.is_none(), "empty token string is not a token");
    }

    #[test]
    fn unknown_key_is_a_parse_error() {
        // deny_unknown_fields: a typo'd key is surfaced, not silently dropped. (load() turns this into
        // a fail-soft dormant config; the pure parser returns the error.)
        assert!(Config::from_toml_str("bot_tokn = \"xoxb-typo\"\n", &base()).is_err());
    }

    // ── load(): fail-soft file handling ──────────────────────────────────────────────────────────

    #[test]
    fn load_reads_a_real_file_and_defaults_state_dir_to_its_parent() {
        let dir = tmp_dir("load");
        let path = dir.join("slack-bridge.toml");
        std::fs::write(&path, "bot_token = \"xoxb-f\"\napp_token = \"xapp-f\"\nchannel = \"Cfile\"\n")
            .unwrap();
        let cfg = Config::load(&path);
        assert!(cfg.tokens().is_some());
        assert_eq!(cfg.channel.as_deref(), Some("Cfile"));
        assert_eq!(cfg.state_dir, dir, "state_dir defaults to the config file's dir");
    }

    #[test]
    fn load_missing_file_is_dormant_not_fatal() {
        let dir = tmp_dir("missing");
        let cfg = Config::load(&dir.join("nope.toml"));
        assert!(cfg.tokens().is_none());
        assert_eq!(cfg.default_to, "concierge");
    }

    #[test]
    fn load_malformed_file_is_dormant_not_fatal() {
        let dir = tmp_dir("bad");
        let path = dir.join("slack-bridge.toml");
        std::fs::write(&path, "this is not = = valid toml [[[").unwrap();
        let cfg = Config::load(&path);
        assert!(cfg.tokens().is_none(), "malformed → dormant, no crash");
        assert_eq!(cfg.default_to, "concierge");
    }

    // ── SECURITY: redacting Debug ────────────────────────────────────────────────────────────────

    #[test]
    fn debug_redacts_secrets() {
        let t = SlackTokens {
            bot_token: "xoxb-SECRETBODY".into(),
            app_token: "xapp-SECRETBODY".into(),
        };
        let dbg = format!("{t:?}");
        assert!(!dbg.contains("SECRETBODY"), "token body must not appear: {dbg}");
        assert!(
            dbg.contains("xoxb-***") && dbg.contains("xapp-***"),
            "prefix kept: {dbg}"
        );

        let cfg = Config {
            bot_token: Some("xoxb-SECRETBODY".into()),
            app_token: Some("xapp-SECRETBODY".into()),
            channel: Some("D0X".into()),
            board_api: "http://127.0.0.1:8079/api".into(),
            default_to: "concierge".into(),
            bridge_agent: "slack-bridge".into(),
            state_dir: PathBuf::from("/tmp/f"),
            channel_map: Vec::new(),
        };
        let dbg = format!("{cfg:?}");
        assert!(!dbg.contains("SECRETBODY"), "config Debug must not leak tokens: {dbg}");
        assert!(dbg.contains("D0X"), "non-secret fields still shown");
    }

    #[test]
    fn redact_never_leaks_a_malformed_secret_body() {
        // The invariant: ANY secret shape is redacted, not just a well-formed `xoxb-…`. The fallback
        // arms (no hyphen, empty prefix) are the security-critical ones — a refactor that regressed them
        // would leak a body-with-no-prefix. Pin that the body NEVER appears for every degenerate shape.
        let t = SlackTokens {
            bot_token: "xoxbNOHYPHENSECRET".into(),  // no '-' → whole thing is the "body"
            app_token: "-LEADINGHYPHENSECRET".into(), // empty prefix → not kept
        };
        let dbg = format!("{t:?}");
        assert!(
            !dbg.contains("NOHYPHENSECRET") && !dbg.contains("LEADINGHYPHENSECRET"),
            "no malformed-token body may leak: {dbg}"
        );
        assert!(dbg.contains("***"), "a malformed secret still redacts to ***: {dbg}");

        // An EMPTY secret is distinguishable as `<unset>` (not a leak — nothing to hide) so an operator
        // can tell "not configured" from "configured but redacted".
        let empty = SlackTokens {
            bot_token: String::new(),
            app_token: String::new(),
        };
        assert!(format!("{empty:?}").contains("<unset>"));
    }
}
