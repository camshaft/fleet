//! `retry` — capped exponential backoff for reconnecting to a flaky resource.
//!
//! The audio device (a USB mic) can be absent at startup or hot-unplugged mid-run; the operator's
//! requirement is that the daemon NEVER crash-loops on that — it stays up and keeps retrying to (re)open
//! the device (task #239). This pure helper is the backoff schedule those retry loops use; keeping it out
//! of the `runtime` feature means the default `cargo test` gates the timing math even though the cpal open
//! loop itself only builds with the native audio backend.

use std::time::Duration;

/// The first wait after a failed open — short, so a device that appears quickly is picked up promptly.
pub const INITIAL_BACKOFF: Duration = Duration::from_millis(500);

/// The ceiling on the wait between retries — bounded so a long-absent device is still polled regularly
/// (and the logs don't imply the daemon has given up).
pub const MAX_BACKOFF: Duration = Duration::from_secs(5);

/// The next backoff given the current one: double it, capped at [`MAX_BACKOFF`]. A zero `current` (the
/// initial state, before any failure) yields [`INITIAL_BACKOFF`]. Monotonic and saturating — it never
/// overflows and never exceeds the cap, so a retry loop can call it indefinitely.
pub fn next_backoff(current: Duration) -> Duration {
    if current.is_zero() {
        return INITIAL_BACKOFF;
    }
    current.saturating_mul(2).min(MAX_BACKOFF)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_start_yields_the_initial_backoff() {
        assert_eq!(next_backoff(Duration::ZERO), INITIAL_BACKOFF);
    }

    #[test]
    fn doubles_until_it_reaches_the_cap() {
        let mut b = next_backoff(Duration::ZERO); // 500ms
        assert_eq!(b, Duration::from_millis(500));
        b = next_backoff(b); // 1s
        assert_eq!(b, Duration::from_secs(1));
        b = next_backoff(b); // 2s
        assert_eq!(b, Duration::from_secs(2));
        b = next_backoff(b); // 4s
        assert_eq!(b, Duration::from_secs(4));
        b = next_backoff(b); // 8s → capped at 5s
        assert_eq!(b, MAX_BACKOFF);
    }

    #[test]
    fn stays_at_the_cap_once_reached() {
        assert_eq!(next_backoff(MAX_BACKOFF), MAX_BACKOFF);
        // A current value already above the cap is clamped down, not grown.
        assert_eq!(next_backoff(Duration::from_secs(60)), MAX_BACKOFF);
    }

    #[test]
    fn never_overflows_at_extreme_input() {
        // saturating_mul keeps a pathological input finite and capped, so the loop can't panic.
        assert_eq!(next_backoff(Duration::MAX), MAX_BACKOFF);
    }
}
