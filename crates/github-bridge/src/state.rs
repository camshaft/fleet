//! `state` — the daemon's persisted cursors, a small JSON file in the config'd `state_dir`.
//!
//! Two independent cursors, asymmetric on purpose:
//! - [`firehose_seq`](State::firehose_seq) — the board event-firehose cursor for the OUT direction. On first
//!   run it initializes at the firehose HEAD (skip backlog) so the bridge doesn't replay the whole board's
//!   comment history as GitHub posts.
//! - [`issues_since`](State::issues_since) — the GitHub `?since=` timestamp for the IN direction. On first
//!   run it is absent, so the bridge INGESTS the existing issue backlog (mirroring the current issue set);
//!   the issue↔task links keep that idempotent.
//!
//! Kept in the lib (no logging deps) so it is unit-tested by `cargo test`; the daemon binary loads it at
//! startup and persists after each terminally-handled step. Load is **fail-soft**: a missing or malformed
//! file yields the default (as if first run) — never a crash.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// The daemon's persisted cursor state. Unknown JSON fields are ignored (forward compatible).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct State {
    /// The board firehose cursor (last event `seq` terminally handled OUT). `None` = never initialized.
    #[serde(default)]
    pub firehose_seq: Option<i64>,
    /// The GitHub issues `?since=` RFC3339 timestamp (IN). `None` = first run (ingest the backlog).
    #[serde(default)]
    pub issues_since: Option<String>,
}

impl State {
    /// The state file path within `state_dir`.
    pub fn path(state_dir: &Path) -> PathBuf {
        state_dir.join("github-bridge.state.json")
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

    #[test]
    fn missing_file_loads_default_first_run() {
        let s = State::load(&tmp_dir("missing"));
        assert_eq!(s, State::default());
        assert!(s.firehose_seq.is_none(), "first run: firehose uninitialized");
        assert!(s.issues_since.is_none(), "first run: ingest the backlog");
    }

    #[test]
    fn save_then_load_round_trips() {
        let dir = tmp_dir("rt");
        let s = State { firehose_seq: Some(1234), issues_since: Some("2026-09-29T10:00:00Z".into()) };
        s.save(&dir).unwrap();
        assert_eq!(State::load(&dir), s);
    }

    #[test]
    fn malformed_file_loads_default_not_error() {
        let dir = tmp_dir("bad");
        std::fs::write(State::path(&dir), "{ not valid json").unwrap();
        assert_eq!(State::load(&dir), State::default(), "malformed → default, no crash");
    }

    #[test]
    fn unknown_fields_are_ignored_forward_compat() {
        let dir = tmp_dir("fwd");
        std::fs::write(
            State::path(&dir),
            r#"{"firehose_seq": 9, "issues_since": "t", "future_cursor": "ignored"}"#,
        )
        .unwrap();
        let s = State::load(&dir);
        assert_eq!(s.firehose_seq, Some(9));
        assert_eq!(s.issues_since.as_deref(), Some("t"));
    }

    #[test]
    fn save_creates_missing_state_dir() {
        let dir = tmp_dir("mk").join("nested/deeper");
        assert!(!dir.exists());
        State { firehose_seq: Some(1), issues_since: None }.save(&dir).unwrap();
        assert_eq!(State::load(&dir).firehose_seq, Some(1));
    }
}
