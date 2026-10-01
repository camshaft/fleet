//! Bridge configuration — loaded from a single **TOML config file**. NO environment variables.
//!
//! Operator mandate seq-1377 / task #159 (fleet-wide): every daemon is configured via a TOML file, never
//! env vars. So ALL config values — the board REST base, the resolver groups to ingest, the intake project,
//! the poll cadence — live in one TOML file. Only the file PATH is chosen outside the file: the daemon takes
//! a `--config <path>` CLI flag (that is not env-var config), defaulting to [`DEFAULT_CONFIG_FILENAME`]. The
//! deploy delivers this file as the agenix-decrypted config; the dev file is gitignored.
//!
//! Unlike the Slack/GitHub adapters, this bridge holds NO secret of its own: the ticketing read side rides
//! the fleet's shared ticketing session (doc #2706 A2), so there is no token field here. A provisioned
//! programmatic identity is the follow-on hardening (task #895).
//!
//! The bridge **fails soft**: a missing OR malformed config file yields a valid *dormant* [`Config`]
//! (defaults, no resolver groups) — logged, never a crash — so it can be built, land, and run before the
//! operator has written the config. A [`Config`] with no resolver groups is valid: the caller logs
//! "nothing to ingest, idle" and the poll loop stays dormant.
//!
//! PUBLIC-REPO BOUNDARY (doc #2706 A9): resolver groups are opaque strings set only in the deploy config —
//! never defaulted or hardcoded here. This file carries no internal hostnames, group ids, or aliases.

use serde::Deserialize;
use std::path::{Path, PathBuf};

/// The localhost board REST base the ingest writer uses, when the config file omits it. The board loopback on
/// the deploy host (the same base the deployed slack/github bridges use): the daemon appends `/tasks`,
/// `/tasks/:id/comments`, `/external-identities`, `/external-links`. Override per-environment via `board_api`.
const DEFAULT_BOARD_API: &str = "http://127.0.0.1:8079/api";
/// The board project ingested tickets file into when the config omits it: the uncategorized intake project
/// (doc #2706 A6, operator routing directive). The bridge files every ticket here UNASSIGNED and board-triage
/// routes/assigns from there — the bridge never self-categorizes. Overridable via `project_id`.
const DEFAULT_PROJECT_ID: i64 = 29;
/// The watermark poll cadence in seconds, when the config omits it (doc #2706 A3 — the available cadence is a
/// poll; a push stream replaces it if one becomes reachable). Override via `poll_interval_secs`.
const DEFAULT_POLL_INTERVAL_SECS: u64 = 300;
/// This bridge's own board agent name (the author/actor of the writes it performs), when the config omits it.
const DEFAULT_BRIDGE_AGENT: &str = "ticket-ingest";

/// The config filename the daemon reads by default; override with the `--config <path>` CLI flag. NOT
/// discovered via any environment variable (mandate #159) — a fixed filename the deploy points `--config` at.
pub const DEFAULT_CONFIG_FILENAME: &str = "ticket-ingest.toml";

/// Fully-resolved bridge configuration (from the TOML file, with defaults applied). No secret field — the
/// ticketing read side rides the shared session (doc #2706 A2) — so a plain derived `Debug` is safe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// The opaque resolver-group ids whose tickets the bridge ingests. Empty ⇒ nothing to ingest (the bridge
    /// stays up but idle — a valid dormant state). Set ONLY in the deploy config (public-repo boundary); the
    /// initial scope is one group (Membrain), and adding another is a config-only change.
    pub resolver_groups: Vec<String>,
    /// The board project ingested tickets become tasks in — the uncategorized intake project by default.
    pub project_id: i64,
    /// The board REST base URL the ingest writer uses (localhost loopback by default).
    pub board_api: String,
    /// The watermark poll cadence, in seconds.
    pub poll_interval_secs: u64,
    /// This bridge's own board agent name (author/actor of its writes).
    pub bridge_agent: String,
    /// The bridge's local state dir (the persisted per-group watermark cursor). Defaults to the config dir.
    pub state_dir: PathBuf,
}

/// The raw TOML shape. Every field optional: absent resolver groups ⇒ dormant; non-set fields fall back to
/// built-in defaults. `#[serde(deny_unknown_fields)]` so a typo'd key surfaces (fail-soft: it makes the file
/// "malformed", which [`Config::load`] logs and treats as dormant rather than silently ignoring).
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileConfig {
    /// Singular sugar for a one-group config; folded into [`Config::resolver_groups`].
    resolver_group: Option<String>,
    /// The multi-group list; each an opaque resolver-group id. Folded together with `resolver_group`.
    #[serde(default)]
    resolver_groups: Vec<String>,
    project_id: Option<i64>,
    board_api: Option<String>,
    poll_interval_secs: Option<u64>,
    bridge_agent: Option<String>,
    state_dir: Option<String>,
}

impl Config {
    /// The ingest targets — one `(resolver_group, project_id)` per configured group. All groups share the one
    /// intake `project_id`. Empty when no group is configured (a valid dormant config — the bridge is up but
    /// mirrors nothing). The IN cursor is keyed per group, so each configured group scans/advances
    /// independently.
    pub fn ingest_targets(&self) -> Vec<(&str, i64)> {
        self.resolver_groups
            .iter()
            .map(|g| (g.as_str(), self.project_id))
            .collect()
    }

    /// Apply defaults to a parsed [`FileConfig`]. `base_dir` (the config file's directory) is the default
    /// `state_dir` when the file doesn't set one. Pure.
    fn from_file_config(file: FileConfig, base_dir: &Path) -> Config {
        let nonempty = |o: Option<String>| o.filter(|s| !s.is_empty());
        // Resolve groups: the `resolver_groups` list plus the singular `resolver_group` sugar, dropping
        // empties and de-duplicating in first-occurrence order (a copy-paste dup doesn't double-ingest).
        let mut groups: Vec<String> = file
            .resolver_groups
            .into_iter()
            .filter(|s| !s.is_empty())
            .collect();
        if let Some(g) = nonempty(file.resolver_group) {
            groups.push(g);
        }
        let mut seen = std::collections::HashSet::new();
        groups.retain(|g| seen.insert(g.clone()));
        Config {
            resolver_groups: groups,
            project_id: file.project_id.unwrap_or(DEFAULT_PROJECT_ID),
            board_api: nonempty(file.board_api).unwrap_or_else(|| DEFAULT_BOARD_API.to_string()),
            poll_interval_secs: file
                .poll_interval_secs
                .unwrap_or(DEFAULT_POLL_INTERVAL_SECS),
            bridge_agent: nonempty(file.bridge_agent)
                .unwrap_or_else(|| DEFAULT_BRIDGE_AGENT.to_string()),
            state_dir: nonempty(file.state_dir)
                .map(PathBuf::from)
                .unwrap_or_else(|| base_dir.to_path_buf()),
        }
    }

    /// Parse config from a TOML string, applying defaults. `base_dir` is the dir the config file lives in (the
    /// default `state_dir`). Pure — the unit-test entry point. Returns the parse error on malformed TOML (the
    /// fail-soft handling lives in [`Config::load`]).
    pub fn from_toml_str(text: &str, base_dir: &Path) -> Result<Config, toml::de::Error> {
        Ok(Self::from_file_config(toml::from_str(text)?, base_dir))
    }

    /// Load config from the TOML file at `path`, **fail-soft**: a missing OR malformed file yields a dormant
    /// [`Config`] (defaults, no groups) — a malformed file is logged via `eprintln!` (no structured logger at
    /// config-load time) — rather than an error or crash. `state_dir` defaults to the config file's own dir.
    pub fn load(path: &Path) -> Config {
        let base_dir = path.parent().unwrap_or_else(|| Path::new("."));
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(_) => return Self::from_file_config(FileConfig::default(), base_dir), // absent = dormant
        };
        match toml::from_str::<FileConfig>(&text) {
            Ok(fc) => Self::from_file_config(fc, base_dir),
            Err(e) => {
                eprintln!("ticket-ingest: ignoring malformed {}: {e}", path.display());
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
        PathBuf::from("/etc/ticket-ingest")
    }

    fn tmp_dir(tag: &str) -> PathBuf {
        static N: AtomicU64 = AtomicU64::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let d = std::env::temp_dir().join(format!("ti-cfg-{}-{}-{}", tag, std::process::id(), n));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn empty_toml_is_dormant_but_valid() {
        let cfg = Config::from_toml_str("", &base()).unwrap();
        assert!(
            cfg.resolver_groups.is_empty(),
            "no groups → nothing to ingest"
        );
        assert!(cfg.ingest_targets().is_empty());
        assert_eq!(
            cfg.project_id, 29,
            "defaults to the uncategorized intake project"
        );
        assert_eq!(cfg.poll_interval_secs, 300);
        assert_eq!(cfg.bridge_agent, "ticket-ingest");
        assert_eq!(cfg.board_api, "http://127.0.0.1:8079/api");
        assert_eq!(
            cfg.state_dir,
            base(),
            "state_dir defaults to the config file's dir"
        );
    }

    #[test]
    fn ingest_targets_one_per_group_sharing_the_intake_project() {
        let toml = r#"
            resolver_groups = ["grp-a", "grp-b"]
        "#;
        let cfg = Config::from_toml_str(toml, &base()).unwrap();
        assert_eq!(cfg.resolver_groups, ["grp-a", "grp-b"]);
        assert_eq!(cfg.ingest_targets(), vec![("grp-a", 29), ("grp-b", 29)]);
    }

    #[test]
    fn singular_group_is_sugar_and_merges_deduped() {
        // `resolver_group` folds into the list; a dup across list+singular collapses; empties dropped; order kept.
        let toml = r#"
            resolver_groups = ["g1", "", "g2", "g1"]
            resolver_group = "g2"
        "#;
        let cfg = Config::from_toml_str(toml, &base()).unwrap();
        assert_eq!(
            cfg.resolver_groups,
            ["g1", "g2"],
            "empties dropped, dups collapsed, first-occurrence order"
        );
    }

    #[test]
    fn full_toml_sets_every_field() {
        let toml = r#"
            resolver_group = "grp-x"
            project_id = 7
            board_api = "http://board.local/api"
            poll_interval_secs = 60
            bridge_agent = "ti2"
            state_dir = "/var/lib/ticket-ingest"
        "#;
        let cfg = Config::from_toml_str(toml, &base()).unwrap();
        assert_eq!(cfg.ingest_targets(), vec![("grp-x", 7)]);
        assert_eq!(cfg.board_api, "http://board.local/api");
        assert_eq!(cfg.poll_interval_secs, 60);
        assert_eq!(cfg.bridge_agent, "ti2");
        assert_eq!(cfg.state_dir, PathBuf::from("/var/lib/ticket-ingest"));
    }

    #[test]
    fn empty_string_values_fall_back_to_defaults() {
        // An explicitly-empty non-secret string must not blank out the default (treated as unset).
        let cfg =
            Config::from_toml_str("bridge_agent = \"\"\nboard_api = \"\"\n", &base()).unwrap();
        assert_eq!(cfg.bridge_agent, "ticket-ingest");
        assert_eq!(cfg.board_api, "http://127.0.0.1:8079/api");
    }

    #[test]
    fn unknown_key_is_a_parse_error() {
        // deny_unknown_fields: a typo'd key is surfaced, not silently dropped. (load() turns this into a
        // fail-soft dormant config; the pure parser returns the error.)
        assert!(Config::from_toml_str("resolvergroups = [\"g\"]\n", &base()).is_err());
    }

    #[test]
    fn load_reads_a_real_file_and_defaults_state_dir_to_its_parent() {
        let dir = tmp_dir("load");
        let path = dir.join("ticket-ingest.toml");
        std::fs::write(&path, "resolver_group = \"grp-a\"\nproject_id = 3\n").unwrap();
        let cfg = Config::load(&path);
        assert_eq!(cfg.ingest_targets(), vec![("grp-a", 3)]);
        assert_eq!(
            cfg.state_dir, dir,
            "state_dir defaults to the config file's dir"
        );
    }

    #[test]
    fn load_missing_file_is_dormant_not_fatal() {
        let dir = tmp_dir("missing");
        let cfg = Config::load(&dir.join("nope.toml"));
        assert!(cfg.resolver_groups.is_empty());
        assert_eq!(cfg.bridge_agent, "ticket-ingest");
    }

    #[test]
    fn load_malformed_file_is_dormant_not_fatal() {
        let dir = tmp_dir("bad");
        let path = dir.join("ticket-ingest.toml");
        std::fs::write(&path, "this is not = = valid toml [[[").unwrap();
        let cfg = Config::load(&path);
        assert!(
            cfg.resolver_groups.is_empty(),
            "malformed → dormant, no crash"
        );
        assert_eq!(cfg.project_id, 29);
    }
}
