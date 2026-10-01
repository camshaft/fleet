//! Bridge configuration — loaded from a single **TOML config file**. NO environment variables.
//!
//! Operator mandate seq-1377 / task #159 (fleet-wide): every daemon is configured via a TOML file, never
//! env vars ("no env var BS — a pain to maintain"). So ALL config *values* — the GitHub token, the repo to
//! ingest, the board REST base, wiring — live in one TOML file. Only the file *path* is chosen outside the
//! file: the daemon takes a `--config <path>` CLI flag (that is not env-var config), defaulting to
//! [`DEFAULT_CONFIG_FILENAME`]. The deploy delivers this file as the agenix-decrypted secret (mode 0400, out
//! of the repo, via a `roles/github-bridge.nix` in camshaft/dotfiles); the dev file is gitignored.
//!
//! The bridge must **fail soft**: a missing OR malformed config file yields a valid *dormant* [`Config`]
//! (defaults, no token) — logged, never a crash — so it can be built, land, and run before the operator has
//! written the config / minted the GitHub token. A [`Config`] whose [`Config::token`] returns `None` is
//! valid: the caller logs "token absent, idle" and the poll loop stays dormant, retrying.
//!
//! Unlike the Slack adapter (two tokens, Socket Mode), GitHub authenticates with a SINGLE token (a PAT or a
//! GitHub App installation token) against a plain REST base — so the run precondition is just that one token.

use serde::Deserialize;
use std::fmt;
use std::path::{Path, PathBuf};

/// The localhost board REST base the firehose subscriber reads, used when the config file omits it. This is
/// the board loopback on the deploy host (the same base the deployed slack-bridge uses; the deploy host has
/// no Caddy front-door): the daemon appends `/events`, `/tasks`, `/tasks/:id/comments`, `/external-links`,
/// `/external-identities`. Override per-environment via config `board_api`.
const DEFAULT_BOARD_API: &str = "http://127.0.0.1:8079/api";
/// The GitHub REST API base, used when the config omits it. Overridable so the same adapter works against a
/// GitHub Enterprise Server (`https://ghe.example.com/api/v3`) as well as public GitHub.
const DEFAULT_API_BASE: &str = "https://api.github.com";
const DEFAULT_DEFAULT_TO: &str = "concierge";
const DEFAULT_BRIDGE_AGENT: &str = "github-bridge";

/// The config filename the daemon reads by default; override with the `--config <path>` CLI flag. This is
/// NOT discovered via any environment variable (mandate #159) — it's a fixed filename the deploy points
/// `--config` at (the agenix-decrypted secret) and dev runs pass explicitly.
pub const DEFAULT_CONFIG_FILENAME: &str = "github-bridge.toml";

/// Redact a secret for `Debug`: keep only a short recognizable head (up to and including the first `_`, e.g.
/// `ghp_***` / `github_***`) so logs stay diagnosable without ever printing the token body. SECURITY: this
/// struct holds a live GitHub credential; a stray `{:?}`/`dbg!`/panic-format must not leak it, so `Debug` is
/// hand-rolled to redact. GitHub tokens use `_` as their type separator (`ghp_`, `gho_`, `ghs_`,
/// `github_pat_`), unlike Slack's `-`.
fn redact(secret: &str) -> String {
    match secret.split_once('_') {
        Some((prefix, _)) if !prefix.is_empty() => format!("{prefix}_***"),
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

/// Fully-resolved bridge configuration (from the TOML file, with defaults applied).
///
/// NOTE: `Debug` is REDACTING (no derive) so the `github_token` field never prints raw.
#[derive(Clone, PartialEq, Eq)]
pub struct Config {
    /// The GitHub token (PAT or App installation token), if set in the file. `None` = fail-soft dormant mode.
    pub github_token: Option<String>,
    /// The `owner/name` repositories whose issues the bridge ingests (e.g. `camshaft/fleet`). Empty ⇒
    /// nothing to ingest (the bridge stays up but idle — a valid state). Resolved from the config `repos`
    /// list plus the singular `repo` sugar, de-duplicated in first-occurrence order.
    pub repos: Vec<String>,
    /// The board project id that ingested issues become tasks in. Optional: without it ingest is dormant
    /// (the bridge can't decide where to create the mirrored tasks).
    pub project_id: Option<i64>,
    /// The board REST base URL the firehose subscriber + task writer use (localhost front-door by default).
    pub board_api: String,
    /// The GitHub REST API base (public GitHub by default; override for GitHub Enterprise Server).
    pub api_base: String,
    /// Default recipient when a routed message has no explicit target (the concierge).
    pub default_to: String,
    /// This bridge's own board agent name (the `author`/`sender` of the writes it performs).
    pub bridge_agent: String,
    /// The bridge's local state dir (persisted firehose cursor etc.). Defaults to the config file's dir.
    pub state_dir: PathBuf,
}

impl fmt::Debug for Config {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Config")
            .field("github_token", &redact_opt(&self.github_token))
            .field("repos", &self.repos)
            .field("project_id", &self.project_id)
            .field("board_api", &self.board_api)
            .field("api_base", &self.api_base)
            .field("default_to", &self.default_to)
            .field("bridge_agent", &self.bridge_agent)
            .field("state_dir", &self.state_dir)
            .finish()
    }
}

/// The raw TOML shape. Every field optional: the secret absent → dormant; non-secret fields fall back to
/// built-in defaults. `#[serde(deny_unknown_fields)]` so a typo'd key is surfaced (fail-soft: it makes the
/// file "malformed", which [`Config::load`] logs and treats as dormant rather than silently ignoring).
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileConfig {
    github_token: Option<String>,
    /// Path to a file holding ONLY the GitHub token (an agenix secret). When set, [`Config::load`] reads the
    /// token from it (trimmed) — so the deploy keeps just the bare PAT encrypted while the rest of the config
    /// stays non-secret (secret-surface minimization, board-core #272). Overrides inline `github_token`; a
    /// relative path resolves against the config file's dir. Missing/empty ⇒ fail-soft dormant.
    github_token_file: Option<String>,
    /// Singular sugar for a one-repo config; folded into [`Config::repos`].
    repo: Option<String>,
    /// The multi-repo list; each `owner/name`. Folded together with `repo`.
    #[serde(default)]
    repos: Vec<String>,
    project_id: Option<i64>,
    board_api: Option<String>,
    api_base: Option<String>,
    default_to: Option<String>,
    bridge_agent: Option<String>,
    state_dir: Option<String>,
}

impl Config {
    /// The GitHub token iff present + non-empty — the precondition for starting the poll loop.
    pub fn token(&self) -> Option<&str> {
        self.github_token.as_deref().filter(|s| !s.is_empty())
    }

    /// The ingest targets — one `(repo, project_id)` per configured repo, iff a `project_id` is set and at
    /// least one repo is configured. Empty when either is absent: a token without repos/project is a valid
    /// (dormant) config — the bridge is up but mirrors nothing. All repos share the one `project_id` (v1);
    /// per-repo project mapping is a non-breaking future extension.
    pub fn ingest_targets(&self) -> Vec<(&str, i64)> {
        match self.project_id {
            Some(p) if !self.repos.is_empty() => self.repos.iter().map(|r| (r.as_str(), p)).collect(),
            _ => Vec::new(),
        }
    }

    /// Apply defaults to a parsed [`FileConfig`]. `base_dir` (the config file's directory) is the default
    /// `state_dir` when the file doesn't set one. Pure.
    fn from_file_config(file: FileConfig, base_dir: &Path) -> Config {
        let nonempty = |o: Option<String>| o.filter(|s| !s.is_empty());
        // Resolve repos: the `repos` list plus the singular `repo` sugar, dropping empties and de-duplicating
        // in first-occurrence order (a copy-paste dup doesn't double-ingest).
        let mut repos: Vec<String> = file.repos.into_iter().filter(|s| !s.is_empty()).collect();
        if let Some(r) = nonempty(file.repo) {
            repos.push(r);
        }
        let mut seen = std::collections::HashSet::new();
        repos.retain(|r| seen.insert(r.clone()));
        Config {
            github_token: nonempty(file.github_token),
            repos,
            project_id: file.project_id,
            board_api: nonempty(file.board_api).unwrap_or_else(|| DEFAULT_BOARD_API.to_string()),
            api_base: nonempty(file.api_base).unwrap_or_else(|| DEFAULT_API_BASE.to_string()),
            default_to: nonempty(file.default_to).unwrap_or_else(|| DEFAULT_DEFAULT_TO.to_string()),
            bridge_agent: nonempty(file.bridge_agent)
                .unwrap_or_else(|| DEFAULT_BRIDGE_AGENT.to_string()),
            state_dir: nonempty(file.state_dir)
                .map(PathBuf::from)
                .unwrap_or_else(|| base_dir.to_path_buf()),
        }
    }

    /// Parse config from a TOML string, applying defaults. `base_dir` is the dir the config file lives in
    /// (the default `state_dir`). Pure — the unit-test entry point. Returns the parse error on malformed TOML
    /// (the fail-soft handling lives in [`Config::load`]).
    pub fn from_toml_str(text: &str, base_dir: &Path) -> Result<Config, toml::de::Error> {
        Ok(Self::from_file_config(toml::from_str(text)?, base_dir))
    }

    /// Resolve `github_token_file` (IO — kept out of the pure [`Self::from_toml_str`]): when set, read the
    /// bare token from that file (trimmed) and use it as `github_token`, overriding any inline value. A
    /// relative path resolves against `base_dir` (an absolute path is used as-is). A missing/empty/unreadable
    /// file is logged and leaves the token as-is (usually `None` in the split model ⇒ fail-soft dormant).
    fn resolve_token_file(mut file: FileConfig, base_dir: &Path) -> FileConfig {
        if let Some(rel) = file.github_token_file.as_deref().filter(|s| !s.is_empty()) {
            let path = base_dir.join(rel); // PathBuf::join: an absolute `rel` replaces base_dir
            match std::fs::read_to_string(&path) {
                Ok(s) if !s.trim().is_empty() => file.github_token = Some(s.trim().to_string()),
                Ok(_) => {
                    eprintln!("github-bridge: token file {} is empty — running dormant", path.display())
                }
                Err(e) => eprintln!(
                    "github-bridge: cannot read token file {}: {e} — running dormant",
                    path.display()
                ),
            }
        }
        file
    }

    /// Load config from the TOML file at `path`, **fail-soft**: a missing OR malformed file yields a dormant
    /// [`Config`] (defaults, no token) — a malformed file is logged via `eprintln!` (no structured logger at
    /// config-load time) — rather than an error or crash. When the file sets `github_token_file`, the bare
    /// token is read from that path. `state_dir` defaults to the config file's own dir.
    pub fn load(path: &Path) -> Config {
        let base_dir = path.parent().unwrap_or_else(|| Path::new("."));
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(_) => return Self::from_file_config(FileConfig::default(), base_dir), // absent = dormant
        };
        match toml::from_str::<FileConfig>(&text) {
            Ok(fc) => Self::from_file_config(Self::resolve_token_file(fc, base_dir), base_dir),
            Err(e) => {
                eprintln!("github-bridge: ignoring malformed {}: {e}", path.display());
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
        PathBuf::from("/etc/github-bridge")
    }

    fn tmp_dir(tag: &str) -> PathBuf {
        static N: AtomicU64 = AtomicU64::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let d = std::env::temp_dir().join(format!("gh-cfg-{}-{}-{}", tag, std::process::id(), n));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn empty_toml_is_dormant_but_valid() {
        let cfg = Config::from_toml_str("", &base()).unwrap();
        assert!(cfg.token().is_none(), "no token → dormant");
        assert!(cfg.ingest_targets().is_empty(), "no repo/project → nothing to ingest");
        assert!(cfg.repos.is_empty());
        assert_eq!(cfg.default_to, "concierge");
        assert_eq!(cfg.bridge_agent, "github-bridge");
        assert_eq!(cfg.state_dir, base(), "state_dir defaults to the config file's dir");
        assert_eq!(
            cfg.board_api, "http://127.0.0.1:8079/api",
            "board_api defaults to the deploy-host board loopback"
        );
        assert_eq!(cfg.api_base, "https://api.github.com", "api_base defaults to public GitHub");
    }

    #[test]
    fn token_present_is_enough_to_run_even_without_a_repo() {
        // A token with no repo/project is a valid, running-but-idle config (bridge up, mirrors nothing).
        let cfg = Config::from_toml_str("github_token = \"ghp_abc\"\n", &base()).unwrap();
        assert_eq!(cfg.token(), Some("ghp_abc"));
        assert!(cfg.ingest_targets().is_empty());
    }

    #[test]
    fn ingest_targets_needs_both_repos_and_project() {
        let only_repo = Config::from_toml_str(
            "github_token = \"ghp_a\"\nrepo = \"camshaft/fleet\"\n",
            &base(),
        )
        .unwrap();
        assert!(only_repo.ingest_targets().is_empty(), "repo without project is not a target");

        let only_project =
            Config::from_toml_str("github_token = \"ghp_a\"\nproject_id = 16\n", &base()).unwrap();
        assert!(only_project.ingest_targets().is_empty(), "project without repos is not a target");

        let both = Config::from_toml_str(
            "github_token = \"ghp_a\"\nrepo = \"camshaft/fleet\"\nproject_id = 16\n",
            &base(),
        )
        .unwrap();
        assert_eq!(both.ingest_targets(), vec![("camshaft/fleet", 16)]);
    }

    #[test]
    fn repos_list_maps_all_repos_to_one_project() {
        let toml = r#"
            github_token = "ghp_a"
            repos = ["camshaft/fleet", "camshaft/dotfiles", "camshaft/s2n-quic"]
            project_id = 16
        "#;
        let cfg = Config::from_toml_str(toml, &base()).unwrap();
        assert_eq!(cfg.repos, ["camshaft/fleet", "camshaft/dotfiles", "camshaft/s2n-quic"]);
        assert_eq!(
            cfg.ingest_targets(),
            vec![("camshaft/fleet", 16), ("camshaft/dotfiles", 16), ("camshaft/s2n-quic", 16)]
        );
    }

    #[test]
    fn singular_repo_is_sugar_and_merges_deduped() {
        // `repo` folds into the list; a dup across `repos`+`repo` collapses; empties dropped; order kept.
        let toml = r#"
            github_token = "ghp_a"
            project_id = 3
            repos = ["o/a", "", "o/b", "o/a"]
            repo = "o/b"
        "#;
        let cfg = Config::from_toml_str(toml, &base()).unwrap();
        assert_eq!(cfg.repos, ["o/a", "o/b"], "empties dropped, dups collapsed, first-occurrence order");
    }

    #[test]
    fn full_toml_sets_every_field() {
        let toml = r#"
            github_token = "github_pat_XYZ"
            repo = "camshaft/fleet"
            project_id = 16
            board_api = "http://board.local/api"
            api_base = "https://ghe.example.com/api/v3"
            default_to = "pr-sync"
            bridge_agent = "gb2"
            state_dir = "/var/lib/github-bridge"
        "#;
        let cfg = Config::from_toml_str(toml, &base()).unwrap();
        assert_eq!(cfg.token(), Some("github_pat_XYZ"));
        assert_eq!(cfg.ingest_targets(), vec![("camshaft/fleet", 16)]);
        assert_eq!(cfg.board_api, "http://board.local/api");
        assert_eq!(cfg.api_base, "https://ghe.example.com/api/v3");
        assert_eq!(cfg.default_to, "pr-sync");
        assert_eq!(cfg.bridge_agent, "gb2");
        assert_eq!(cfg.state_dir, PathBuf::from("/var/lib/github-bridge"));
    }

    #[test]
    fn empty_string_values_fall_back_to_defaults() {
        // An explicitly-empty non-secret string must not blank out the default (treated as unset).
        let cfg =
            Config::from_toml_str("default_to = \"\"\ngithub_token = \"\"\n", &base()).unwrap();
        assert_eq!(cfg.default_to, "concierge");
        assert!(cfg.token().is_none(), "empty token string is not a token");
    }

    #[test]
    fn unknown_key_is_a_parse_error() {
        // deny_unknown_fields: a typo'd key is surfaced, not silently dropped. (load() turns this into a
        // fail-soft dormant config; the pure parser returns the error.)
        assert!(Config::from_toml_str("githubtoken = \"ghp_typo\"\n", &base()).is_err());
    }

    // ── load(): fail-soft file handling ──────────────────────────────────────────────────────────

    #[test]
    fn load_reads_a_real_file_and_defaults_state_dir_to_its_parent() {
        let dir = tmp_dir("load");
        let path = dir.join("github-bridge.toml");
        std::fs::write(&path, "github_token = \"ghp_f\"\nrepo = \"o/r\"\nproject_id = 3\n").unwrap();
        let cfg = Config::load(&path);
        assert_eq!(cfg.token(), Some("ghp_f"));
        assert_eq!(cfg.ingest_targets(), vec![("o/r", 3)]);
        assert_eq!(cfg.state_dir, dir, "state_dir defaults to the config file's dir");
    }

    #[test]
    fn load_missing_file_is_dormant_not_fatal() {
        let dir = tmp_dir("missing");
        let cfg = Config::load(&dir.join("nope.toml"));
        assert!(cfg.token().is_none());
        assert_eq!(cfg.default_to, "concierge");
    }

    #[test]
    fn load_malformed_file_is_dormant_not_fatal() {
        let dir = tmp_dir("bad");
        let path = dir.join("github-bridge.toml");
        std::fs::write(&path, "this is not = = valid toml [[[").unwrap();
        let cfg = Config::load(&path);
        assert!(cfg.token().is_none(), "malformed → dormant, no crash");
        assert_eq!(cfg.default_to, "concierge");
    }

    // ── github_token_file (bare-token split, board-core #272) ────────────────────────────────────

    #[test]
    fn load_reads_token_from_token_file() {
        let dir = tmp_dir("tokfile");
        std::fs::write(dir.join("gh.token"), "  ghp_FROMFILE\n").unwrap(); // whitespace trimmed
        let path = dir.join("github-bridge.toml");
        std::fs::write(
            &path,
            "github_token_file = \"gh.token\"\nrepo = \"o/r\"\nproject_id = 3\n",
        )
        .unwrap();
        let cfg = Config::load(&path);
        assert_eq!(cfg.token(), Some("ghp_FROMFILE"), "token read + trimmed from the referenced file");
        assert_eq!(cfg.ingest_targets(), vec![("o/r", 3)], "non-secret settings still apply");
    }

    #[test]
    fn token_file_overrides_inline_token() {
        let dir = tmp_dir("tokoverride");
        std::fs::write(dir.join("gh.token"), "ghp_FILEWINS").unwrap();
        let path = dir.join("github-bridge.toml");
        std::fs::write(&path, "github_token = \"ghp_inline\"\ngithub_token_file = \"gh.token\"\n")
            .unwrap();
        assert_eq!(Config::load(&path).token(), Some("ghp_FILEWINS"), "the secret file wins over inline");
    }

    #[test]
    fn missing_or_empty_token_file_is_dormant_not_fatal() {
        let dir = tmp_dir("toknofile");
        let path = dir.join("github-bridge.toml");
        std::fs::write(&path, "github_token_file = \"absent.token\"\n").unwrap();
        assert!(Config::load(&path).token().is_none(), "missing token file → dormant, no crash");

        std::fs::write(dir.join("empty.token"), "   \n").unwrap();
        std::fs::write(&path, "github_token_file = \"empty.token\"\n").unwrap();
        assert!(Config::load(&path).token().is_none(), "empty token file → dormant");
    }

    // ── SECURITY: redacting Debug ────────────────────────────────────────────────────────────────

    #[test]
    fn debug_redacts_the_token() {
        let cfg = Config {
            github_token: Some("ghp_SECRETBODY".into()),
            repos: vec!["camshaft/fleet".into()],
            project_id: Some(16),
            board_api: "http://127.0.0.1:8079/api".into(),
            api_base: "https://api.github.com".into(),
            default_to: "concierge".into(),
            bridge_agent: "github-bridge".into(),
            state_dir: PathBuf::from("/tmp/f"),
        };
        let dbg = format!("{cfg:?}");
        assert!(!dbg.contains("SECRETBODY"), "config Debug must not leak the token: {dbg}");
        assert!(dbg.contains("ghp_***"), "prefix kept: {dbg}");
        assert!(dbg.contains("camshaft/fleet"), "non-secret fields still shown");
    }

    #[test]
    fn redact_never_leaks_a_malformed_token_body() {
        // The invariant: ANY token shape is redacted, not just a well-formed `ghp_…`. The fallback arms
        // (no separator, empty prefix, empty string) are the security-critical ones.
        assert_eq!(redact("ghp_SECRET"), "ghp_***");
        assert_eq!(redact("github_pat_SECRET_MORE"), "github_***");
        assert!(!redact("NOSEPARATORSECRET").contains("SECRET"), "no-separator token redacts to ***");
        assert_eq!(redact("NOSEPARATORSECRET"), "***");
        assert!(!redact("_LEADINGSEP").contains("LEADING"), "empty-prefix token redacts to ***");
        assert_eq!(redact("_LEADINGSEP"), "***");
        // An EMPTY token is distinguishable as `<unset>` (not a leak) so an operator can tell "not
        // configured" from "configured but redacted".
        assert_eq!(redact(""), "<unset>");
        assert_eq!(redact_opt(&None), "<none>");
    }
}
