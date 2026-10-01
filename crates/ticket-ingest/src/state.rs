//! `state` — the daemon's persisted watermark cursor, a small JSON file in the config'd `state_dir`.
//!
//! One cursor, [`group_since`](State::group_since): a PER-GROUP last-seen `lastUpdatedDate` (RFC3339),
//! keyed by resolver group, for the IN direction. A group absent from the map = first run for it, so the
//! bridge INGESTS that group's backlog (the external-link dedup keeps it idempotent — doc #2706 A4/A5). The
//! cursor advances FORWARD ONLY so a clock skew or a re-poll never regresses it. Per-group so each configured
//! group scans and advances independently.
//!
//! RFC3339 timestamps sort lexicographically, so a plain string compare orders them — the same assumption the
//! sibling github-bridge `?since=` cursor makes.
//!
//! Kept in the lib (no logging deps) so it is unit-tested by `cargo test`; the daemon binary loads it at
//! startup and persists after each terminally-handled step. Load is **fail-soft**: a missing or malformed file
//! yields the default (as if first run) — never a crash.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// The daemon's persisted cursor state. Unknown JSON fields are ignored (forward compatible).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct State {
    /// Per-group last-seen `lastUpdatedDate` RFC3339 timestamp (IN), keyed by resolver group. A group absent =
    /// first run for it (ingest its backlog). `BTreeMap` for stable on-disk key ordering.
    #[serde(default)]
    pub group_since: BTreeMap<String, String>,
}

impl State {
    /// The state file path within `state_dir`.
    pub fn path(state_dir: &Path) -> PathBuf {
        state_dir.join("ticket-ingest.state.json")
    }

    /// The last-seen cursor for `group`, or `None` (first run for that group → ingest its backlog).
    pub fn since_for(&self, group: &str) -> Option<&str> {
        self.group_since.get(group).map(String::as_str)
    }

    /// Advance a group's cursor, FORWARD ONLY (a non-greater timestamp is ignored). Returns whether it changed
    /// (so the caller persists only on a real advance). RFC3339 sorts lexicographically.
    pub fn advance_group(&mut self, group: &str, newest: &str) -> bool {
        if self.since_for(group).is_none_or(|cur| newest > cur) {
            self.group_since
                .insert(group.to_string(), newest.to_string());
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
    /// the caller can log it best-effort (a write failure is not fatal — worst case a restart re-does the last
    /// idempotent step).
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
        let d = std::env::temp_dir().join(format!("ti-state-{}-{}-{}", tag, std::process::id(), n));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn missing_file_loads_default_first_run() {
        let s = State::load(&tmp_dir("missing"));
        assert_eq!(s, State::default());
        assert!(
            s.group_since.is_empty(),
            "first run: every group ingests its backlog"
        );
        assert!(s.since_for("grp-a").is_none());
    }

    #[test]
    fn save_then_load_round_trips() {
        let dir = tmp_dir("rt");
        let mut s = State::default();
        s.advance_group("grp-a", "2026-09-29T10:00:00Z");
        s.advance_group("grp-b", "2026-09-28T00:00:00Z");
        s.save(&dir).unwrap();
        assert_eq!(State::load(&dir), s);
    }

    #[test]
    fn advance_group_is_forward_only_and_per_group() {
        let mut s = State::default();
        assert!(
            s.advance_group("grp-a", "2026-09-29T10:00:00Z"),
            "first set advances"
        );
        assert_eq!(s.since_for("grp-a"), Some("2026-09-29T10:00:00Z"));
        assert!(
            !s.advance_group("grp-a", "2026-09-01T00:00:00Z"),
            "older timestamp ignored"
        );
        assert_eq!(
            s.since_for("grp-a"),
            Some("2026-09-29T10:00:00Z"),
            "cursor didn't regress"
        );
        assert!(
            s.advance_group("grp-a", "2026-09-30T00:00:00Z"),
            "newer advances"
        );
        // Independent per group.
        assert!(s.since_for("grp-b").is_none());
        assert!(s.advance_group("grp-b", "2026-01-01T00:00:00Z"));
        assert_eq!(
            s.since_for("grp-a"),
            Some("2026-09-30T00:00:00Z"),
            "grp-a unaffected by grp-b"
        );
    }

    #[test]
    fn malformed_file_loads_default_not_error() {
        let dir = tmp_dir("bad");
        std::fs::write(State::path(&dir), "{ not valid json").unwrap();
        assert_eq!(
            State::load(&dir),
            State::default(),
            "malformed → default, no crash"
        );
    }

    #[test]
    fn unknown_fields_are_ignored_forward_compat() {
        let dir = tmp_dir("fwd");
        std::fs::write(
            State::path(&dir),
            r#"{"group_since": {"grp-a": "t"}, "future": 1}"#,
        )
        .unwrap();
        let s = State::load(&dir);
        assert_eq!(s.since_for("grp-a"), Some("t"));
    }

    #[test]
    fn save_creates_missing_state_dir() {
        let dir = tmp_dir("mk").join("nested/deeper");
        assert!(!dir.exists());
        let mut s = State::default();
        s.advance_group("grp-a", "2026-01-01T00:00:00Z");
        s.save(&dir).unwrap();
        assert_eq!(
            State::load(&dir).since_for("grp-a"),
            Some("2026-01-01T00:00:00Z")
        );
    }
}
