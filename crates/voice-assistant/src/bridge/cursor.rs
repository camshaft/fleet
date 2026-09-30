//! Firehose cursor persistence for the OUTBOUND poll loop.
//!
//! The voice bridge polls the board firehose (`GET /events?since_seq=`) for replies to speak; the cursor is
//! the last handled event `seq`. Persisting it to the state dir means a restart resumes where it left off
//! instead of re-speaking the backlog or gapping. Best-effort: a write failure is logged, never fatal —
//! worst case a restart re-speaks from the last durable cursor. Same discipline as slack-bridge's cursor.

use std::path::{Path, PathBuf};

/// The cursor file path within the state dir.
fn cursor_path(state_dir: &Path) -> PathBuf {
    state_dir.join("voice-bridge.cursor")
}

/// The persisted firehose cursor, or `None` if never written (first run). A malformed file reads as `None`
/// (treated as a first run) rather than crashing.
pub fn load_cursor(state_dir: &Path) -> Option<i64> {
    std::fs::read_to_string(cursor_path(state_dir))
        .ok()
        .and_then(|s| s.trim().parse().ok())
}

/// Persist the cursor (best-effort — a write failure is logged, not fatal). Creates the state dir if absent.
pub fn save_cursor(state_dir: &Path, cursor: i64) {
    let path = cursor_path(state_dir);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Err(e) = std::fs::write(&path, cursor.to_string()) {
        eprintln!("[voice-bridge] failed to persist firehose cursor {cursor}: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn tmp_dir(tag: &str) -> PathBuf {
        static N: AtomicU64 = AtomicU64::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let d = std::env::temp_dir().join(format!("voice-cursor-{}-{}-{}", tag, std::process::id(), n));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn absent_cursor_is_none() {
        let d = tmp_dir("absent");
        assert_eq!(load_cursor(&d), None);
    }

    #[test]
    fn round_trips_a_cursor() {
        let d = tmp_dir("round");
        save_cursor(&d, 4242);
        assert_eq!(load_cursor(&d), Some(4242));
    }

    #[test]
    fn latest_write_wins() {
        let d = tmp_dir("latest");
        save_cursor(&d, 1);
        save_cursor(&d, 2);
        assert_eq!(load_cursor(&d), Some(2));
    }

    #[test]
    fn malformed_cursor_reads_as_first_run() {
        let d = tmp_dir("bad");
        std::fs::write(cursor_path(&d), "not a number").unwrap();
        assert_eq!(load_cursor(&d), None);
    }

    #[test]
    fn save_creates_a_missing_state_dir() {
        let d = tmp_dir("mkdir").join("nested");
        save_cursor(&d, 7);
        assert_eq!(load_cursor(&d), Some(7));
    }
}
