//! `relay` — the outbound relay-resilience escalation, transport-agnostic.
//!
//! A message that DETERMINISTICALLY fails to deliver must never head-of-line-block the outbound relay. In
//! the reference bridge this once wedged the whole queue (~11h): the pump delivered in order and stopped on
//! the first delivery error, and a single message that reliably failed (a content/format quirk — a shorter
//! truncation of the SAME message delivered fine) blocked everything behind it, and re-blocked identically
//! after a restart. The relay's contract: (1) retry a transport/transient fault in place (order preserved);
//! (2) past [`RELAY_DEGRADE_AFTER`] CONTENT-class failures, deliver a degraded variant; (3) past
//! [`RELAY_QUARANTINE_AFTER`], give up (preserve the message out of band — it's still on the board) and keep
//! draining. The queue-never-wedges guarantee comes from SKIPPING PAST a failing message, so quarantine can
//! afford to be very patient (thresholds are in polls, ~2s each) — a too-eager quarantine false-drops a good
//! message during a brief blip.
//!
//! This is pure policy over a per-message CONTENT-failure count; the transport supplies the actual rich vs
//! degraded render and classifies its own errors (content vs transient).

/// CONTENT-class delivery failures after which the relay drops the rich render and falls back to the
/// degraded variant. A handful of full-fidelity retries first, so a SHORT transient blip is ridden out.
pub const RELAY_DEGRADE_AFTER: u32 = 5;
/// CONTENT-class delivery failures after which the relay gives up and dead-letters the message (preserved
/// out of band). ~5 minutes of SUSTAINED failure — far longer than any normal transient window — so only a
/// genuinely-undeliverable message is dropped, never a good one caught in a blip.
pub const RELAY_QUARANTINE_AFTER: u32 = 155;
/// Relay queue depth at/above which the pump should log a backlog warning, so a wedge/backlog is VISIBLE.
pub const RELAY_QUEUE_WARN: usize = 25;

/// What the relay should do with a message given how many CONTENT-class delivery failures it has had.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelayPlan {
    /// Deliver the normal (rich) render.
    Normal,
    /// Deliver the degraded variant — the rich render keeps failing on content.
    Degraded,
    /// Give up: dead-letter the message and move on.
    Quarantine,
}

/// Decide the relay plan from the count of CONTENT-class failures so far (transient/transport failures are
/// retried in place and do NOT advance this count). Pure.
pub fn relay_plan(content_failures: u32) -> RelayPlan {
    if content_failures >= RELAY_QUARANTINE_AFTER {
        RelayPlan::Quarantine
    } else if content_failures >= RELAY_DEGRADE_AFTER {
        RelayPlan::Degraded
    } else {
        RelayPlan::Normal
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escalates_normal_then_degraded_then_quarantine() {
        assert_eq!(relay_plan(0), RelayPlan::Normal);
        assert_eq!(relay_plan(RELAY_DEGRADE_AFTER - 1), RelayPlan::Normal);
        assert_eq!(relay_plan(RELAY_DEGRADE_AFTER), RelayPlan::Degraded);
        assert_eq!(relay_plan(RELAY_QUARANTINE_AFTER - 1), RelayPlan::Degraded);
        assert_eq!(relay_plan(RELAY_QUARANTINE_AFTER), RelayPlan::Quarantine);
    }

    #[test]
    fn thresholds_are_ordered() {
        // Compile-time: degrade must escalate before quarantine (both are consts, so assert at const-eval).
        const _: () = assert!(RELAY_DEGRADE_AFTER < RELAY_QUARANTINE_AFTER);
    }
}
