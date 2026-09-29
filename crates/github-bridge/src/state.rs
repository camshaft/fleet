//! `state` — the daemon's persisted cursors, a small JSON file in the config'd `state_dir`.
//!
//! Cursors, asymmetric on purpose:
//! - [`firehose_seq`](State::firehose_seq) — the board event-firehose cursor for the OUT direction. On first
//!   run it initializes at the firehose HEAD (skip backlog) so the bridge doesn't replay the whole board's
//!   comment history as GitHub posts. Global (OUT routes by the reflect's `external_id`, repo-agnostic).
//! - [`repo_since`](State::repo_since) — a PER-REPO GitHub `?since=` timestamp for the IN direction, keyed
//!   by `owner/name`. A repo absent from the map = first run for it, so the bridge INGESTS that repo's issue
//!   backlog (the issue↔task links keep it idempotent). Per-repo so each of the N configured repos scans and
//!   advances independently (#271 multi-repo).
//!
//! Kept in the lib (no logging deps) so it is unit-tested by `cargo test`; the daemon binary loads it at
//! startup and persists after each terminally-handled step. Load is **fail-soft**: a missing or malformed
//! file yields the default (as if first run) — never a crash.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// The daemon's persisted cursor state. Unknown JSON fields are ignored (forward compatible).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct State {
    /// The board firehose cursor (last event `seq` terminally handled OUT). `None` = never initialized.
    #[serde(default)]
    pub firehose_seq: Option<i64>,
    /// Per-repo GitHub issues `?since=` RFC3339 timestamp (IN), keyed by `owner/name`. A repo absent = first
    /// run for it (ingest its backlog). `BTreeMap` for stable on-disk key ordering.
    #[serde(default)]
    pub repo_since: BTreeMap<String, String>,
}

impl State {
    /// The state file path within `state_dir`.
    pub fn path(state_dir: &Path) -> PathBuf {
        state_dir.join("github-bridge.state.json")
    }

    /// The `?since=` cursor for `repo`, or `None` (first run for that repo → ingest its backlog).
    pub fn since_for(&self, repo: &str) -> Option<&str> {
        self.repo_since.get(repo).map(String::as_str)
    }

    /// Advance a repo's `?since=` cursor, FORWARD ONLY (a non-greater timestamp is ignored). Returns whether
    /// it changed (so the caller can persist only on a real advance). RFC3339 sorts lexicographically.
    pub fn advance_repo(&mut self, repo: &str, newest: &str) -> bool {
        if self.since_for(repo).is_none_or(|cur| newest > cur) {
            self.repo_since.insert(repo.to_string(), newest.to_string());
            true
        } else {
            false
        }
    }

    /// Load the state, **fail-soft**: a missing OR malformed file yields [`State::default`] (first-run
    /// semantics) rather than an error — the daemon must always start.
    pub fn load(state_dir: &Path) -> State {
        match std::fs::read_to_string(Self::path(state_dir)) {
            Ok(s) => serde_json::from_str(&s).unwrap_or_default(),
            Err(_) => State::default(),
        }
    }

    /// Persist the state (pretty JSON), creating `state_dir` if needed. Returns the error text on failure so
    /// the caller can log it best-effort (a write failure is not fatal — worst case a restart re-does the
    /// last idempotent step).
    pub fn save(&self, state_dir: &Path) -> Result<(), String> {
        let path = Self::path(state_dir);
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let body = serde_json::to_string_pretty(self).map_err(|e| e.to_string())?;
        std::fs::write(&path, body).map_err(|e| format!("write {}: {e}", path.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn tmp_dir(tag: &str) -> PathBuf {
        static N: AtomicU64 = AtomicU64::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let d = std::env::temp_dir().join(format!("gh-state-{}-{}-{}", tag, std::process::id(), n));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn state(seq: Option<i64>, since: &[(&str, &str)]) -> State {
        State {
            firehose_seq: seq,
            repo_since: since.iter().map(|(r, t)| (r.to_string(), t.to_string())).collect(),
        }
    }

    #[test]
    fn missing_file_loads_default_first_run() {
        let s = State::load(&tmp_dir("missing"));
        assert_eq!(s, State::default());
        assert!(s.firehose_seq.is_none(), "first run: firehose uninitialized");
        assert!(s.repo_since.is_empty(), "first run: every repo ingests its backlog");
        assert!(s.since_for("o/r").is_none());
    }

    #[test]
    fn save_then_load_round_trips() {
        let dir = tmp_dir("rt");
        let s = state(Some(1234), &[("o/a", "2026-09-29T10:00:00Z"), ("o/b", "2026-09-28T00:00:00Z")]);
        s.save(&dir).unwrap();
        assert_eq!(State::load(&dir), s);
    }

    #[test]
    fn advance_repo_is_forward_only_and_per_repo() {
        let mut s = State::default();
        assert!(s.advance_repo("o/a", "2026-09-29T10:00:00Z"), "first set advances");
        assert_eq!(s.since_for("o/a"), Some("2026-09-29T10:00:00Z"));
        assert!(!s.advance_repo("o/a", "2026-09-01T00:00:00Z"), "older timestamp ignored");
        assert_eq!(s.since_for("o/a"), Some("2026-09-29T10:00:00Z"), "cursor didn't regress");
        assert!(s.advance_repo("o/a", "2026-09-30T00:00:00Z"), "newer advances");
        // Independent per repo.
        assert!(s.since_for("o/b").is_none());
        assert!(s.advance_repo("o/b", "2026-01-01T00:00:00Z"));
        assert_eq!(s.since_for("o/a"), Some("2026-09-30T00:00:00Z"), "o/a unaffected by o/b");
    }

    #[test]
    fn malformed_file_loads_default_not_error() {
        let dir = tmp_dir("bad");
        std::fs::write(State::path(&dir), "{ not valid json").unwrap();
        assert_eq!(State::load(&dir), State::default(), "malformed → default, no crash");
    }

    #[test]
    fn unknown_fields_are_ignored_forward_compat() {
        // Notably, an OLD single-repo state file's `issues_since` is now an unknown field → ignored (that
        // repo simply re-scans once on the next tick, idempotent). Forward+backward compatible.
        let dir = tmp_dir("fwd");
        std::fs::write(
            State::path(&dir),
            r#"{"firehose_seq": 9, "issues_since": "legacy", "repo_since": {"o/a": "t"}, "future": 1}"#,
        )
        .unwrap();
        let s = State::load(&dir);
        assert_eq!(s.firehose_seq, Some(9));
        assert_eq!(s.since_for("o/a"), Some("t"));
    }

    #[test]
    fn save_creates_missing_state_dir() {
        let dir = tmp_dir("mk").join("nested/deeper");
        assert!(!dir.exists());
        state(Some(1), &[]).save(&dir).unwrap();
        assert_eq!(State::load(&dir).firehose_seq, Some(1));
    }
}
