//! Liveness state + snapshot for the tunnel daemon's health probe.
//!
//! Now that agent wakes ride the tunnel, the failure mode that matters is a *silently-wedged*
//! socket: the process is up and the TCP connection looks established, but no frames are flowing,
//! so "process running" tells a watchdog nothing. The daemon stamps [`HealthState`] as frames
//! arrive from the board; a small loopback HTTP probe (in `main.rs`, behind the `transport`
//! feature) renders a [`HealthSnapshot`] as JSON with a single `ok` verdict so the notifier /
//! watchdog can tell a live wake path from a dead one.
//!
//! This module is pure (no async, no `transport` deps) so the liveness logic is unit-tested.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// Freshness floor: a board frame older than this (when no keepalive scaling applies) means the
/// socket is wedged. Also the fallback threshold before a keepalive has been negotiated.
const MIN_STALE_AFTER_SECS: u64 = 90;

/// Shared, cheaply-updatable liveness state. The serve loop flips `connected` around a connection's
/// lifetime and stamps `last_board_frame_ms` on every frame received from the board; the health
/// probe only reads. All fields are atomics so no lock sits on the hot receive path.
#[derive(Debug, Default)]
pub struct HealthState {
    connected: AtomicBool,
    /// Unix-ms of the last frame RECEIVED from the board (0 = none yet). A healthy socket receives
    /// at least a keepalive pong every `keepalive` seconds, so a stale value flags a wedged socket.
    last_board_frame_ms: AtomicU64,
    /// Negotiated keepalive seconds (0 until hello_ok); the freshness threshold scales off it.
    keepalive_secs: AtomicU64,
}

impl HealthState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Mark the board tunnel up/down (called around a connection's lifetime).
    pub fn set_connected(&self, connected: bool) {
        self.connected.store(connected, Ordering::Relaxed);
    }

    /// Record the keepalive negotiated in hello_ok (drives the staleness threshold).
    pub fn set_keepalive(&self, secs: u64) {
        self.keepalive_secs.store(secs, Ordering::Relaxed);
    }

    /// Stamp "a board frame just arrived" — called for every inbound frame (any frame proves the
    /// socket is alive), so the probe can measure how long the socket has been quiet.
    pub fn mark_board_frame(&self) {
        self.last_board_frame_ms.store(now_ms(), Ordering::Relaxed);
    }

    /// Point-in-time view for rendering. `upstream_reachable` is probed live by the caller (the
    /// probe does a bounded GET to the notifier), so it isn't stored here.
    pub fn snapshot(&self, upstream_reachable: bool, upstream: &str) -> HealthSnapshot {
        let last_ms = self.last_board_frame_ms.load(Ordering::Relaxed);
        let last_board_frame_age_secs = match last_ms {
            0 => None,
            ms => Some(now_ms().saturating_sub(ms) / 1000),
        };
        HealthSnapshot {
            connected: self.connected.load(Ordering::Relaxed),
            last_board_frame_age_secs,
            keepalive_secs: self.keepalive_secs.load(Ordering::Relaxed),
            upstream_reachable,
            upstream: upstream.to_string(),
        }
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// A point-in-time health view. [`HealthSnapshot::ok`] is the watchdog's single yes/no and drives
/// the probe's HTTP status (200 vs 503).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HealthSnapshot {
    pub connected: bool,
    pub last_board_frame_age_secs: Option<u64>,
    pub keepalive_secs: u64,
    pub upstream_reachable: bool,
    pub upstream: String,
}

impl HealthSnapshot {
    /// Board-frame staleness threshold: 3× the negotiated keepalive, floored at
    /// [`MIN_STALE_AFTER_SECS`] (also the value before any keepalive is negotiated). A board frame
    /// older than this means the socket is silently wedged.
    pub fn stale_after_secs(&self) -> u64 {
        self.keepalive_secs
            .saturating_mul(3)
            .max(MIN_STALE_AFTER_SECS)
    }

    /// Is the last board frame recent enough to trust the socket? `None` (never received one) =
    /// not fresh.
    pub fn board_fresh(&self) -> bool {
        matches!(self.last_board_frame_age_secs, Some(age) if age <= self.stale_after_secs())
    }

    /// The single liveness verdict: connected, a fresh board frame, and the upstream reachable.
    pub fn ok(&self) -> bool {
        self.connected && self.board_fresh() && self.upstream_reachable
    }

    /// Render as the probe's JSON body. Field names are the contract co-designed with
    /// v-fleet-tooling (`board_ws` / `last_board_frame_age_secs` / `upstream_reachable`), plus the
    /// negotiated `keepalive_secs` and the derived `ok` verdict.
    pub fn to_json(&self) -> String {
        serde_json::json!({
            "board_ws": if self.connected { "connected" } else { "disconnected" },
            "last_board_frame_age_secs": self.last_board_frame_age_secs,
            "keepalive_secs": self.keepalive_secs,
            "upstream_reachable": self.upstream_reachable,
            "upstream": self.upstream,
            "ok": self.ok(),
        })
        .to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(connected: bool, age: Option<u64>, keepalive: u64, reachable: bool) -> HealthSnapshot {
        HealthSnapshot {
            connected,
            last_board_frame_age_secs: age,
            keepalive_secs: keepalive,
            upstream_reachable: reachable,
            upstream: "http://127.0.0.1:8899".into(),
        }
    }

    #[test]
    fn ok_requires_connected_fresh_and_reachable() {
        assert!(snap(true, Some(5), 30, true).ok());
    }

    #[test]
    fn not_ok_when_disconnected() {
        assert!(!snap(false, Some(1), 30, true).ok());
    }

    #[test]
    fn not_ok_when_no_frame_seen_yet() {
        let s = snap(true, None, 30, true);
        assert!(!s.board_fresh());
        assert!(!s.ok());
    }

    #[test]
    fn not_ok_when_board_frame_is_stale() {
        // keepalive 30 → threshold 90s; a 120s-old frame is wedged.
        assert!(!snap(true, Some(120), 30, true).ok());
    }

    #[test]
    fn not_ok_when_upstream_unreachable() {
        assert!(!snap(true, Some(5), 30, false).ok());
    }

    #[test]
    fn stale_threshold_scales_with_keepalive_and_has_a_floor() {
        assert_eq!(snap(true, None, 0, true).stale_after_secs(), 90); // floor / pre-hello
        assert_eq!(snap(true, None, 10, true).stale_after_secs(), 90); // 3×10 < floor
        assert_eq!(snap(true, None, 60, true).stale_after_secs(), 180); // 3×60
    }

    #[test]
    fn json_shape_healthy() {
        let j = snap(true, Some(7), 30, true).to_json();
        assert!(j.contains(r#""board_ws":"connected""#));
        assert!(j.contains(r#""last_board_frame_age_secs":7"#));
        assert!(j.contains(r#""upstream_reachable":true"#));
        assert!(j.contains(r#""ok":true"#));
    }

    #[test]
    fn json_shape_disconnected_null_age() {
        let j = snap(false, None, 0, false).to_json();
        assert!(j.contains(r#""board_ws":"disconnected""#));
        assert!(j.contains(r#""last_board_frame_age_secs":null"#));
        assert!(j.contains(r#""ok":false"#));
    }

    #[test]
    fn state_transitions_feed_the_snapshot() {
        let st = HealthState::new();
        // fresh state: disconnected, no frame yet
        let s0 = st.snapshot(true, "http://u");
        assert!(!s0.connected && s0.last_board_frame_age_secs.is_none());
        // after connect + a board frame
        st.set_connected(true);
        st.set_keepalive(30);
        st.mark_board_frame();
        let s1 = st.snapshot(true, "http://u");
        assert!(s1.connected);
        assert_eq!(s1.keepalive_secs, 30);
        assert_eq!(s1.last_board_frame_age_secs, Some(0)); // just stamped
        assert!(s1.ok());
        // after disconnect
        st.set_connected(false);
        assert!(!st.snapshot(true, "http://u").ok());
    }
}
