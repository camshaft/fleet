//! `sim` — the source-agnostic ticket domain model and the PURE selection the watermark poll loop drives.
//!
//! SOURCE-AGNOSTIC (public-repo boundary, doc #2706 A9): the external ticketing source is the opaque
//! [`SOURCE`] token used to namespace board external-links and external-identities (`sim:<id>`), the same way
//! `bridge-core` namespaces `slack:`/`github:`/`voice:`. No internal hostnames, resolver-group ids, or ticket
//! content live in this public repo — a [`Ticket`] is just the handful of neutral fields the ingest acts on.
//!
//! The LIVE ticketing read transport (how a group's tickets are fetched past the cursor) is a follow-on slice
//! (task #891); the idempotent board ingest is task #892. This module is the TESTED SEAM they plug into: given
//! a batch the transport returned and the per-group cursor, [`new_since`] selects the tickets to ingest and
//! [`newest_timestamp`] gives the value to advance the cursor to. Both pure, so they gate under `cargo test`
//! without a live source.

use serde::Deserialize;

/// The opaque source token for this bridge's external-link / external-identity namespacing. External ids are
/// `sim:<ticket-id>` for the ticket task and `sim:<handle>` for a human correspondent's identity, keeping
/// them distinct from fleet agents (doc #2706 A4). A short neutral token — no internal hostname.
pub const SOURCE: &str = "sim";

/// One ingested ticket — the neutral fields the board ingest acts on. The live transport (task #891) maps the
/// source's response into this; everything source-specific stays in that transport, never here. Extra JSON
/// fields are ignored so the transport can carry more without changing this shape.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Ticket {
    /// The source-stable ticket id — the external-link `external_id` (`sim:<id>`), the idempotency key.
    pub id: String,
    /// The ticket's last-updated time, RFC3339 (sorts lexicographically) — the watermark the cursor tracks.
    pub last_updated: String,
    /// The ticket title, mirrored to the board task title.
    #[serde(default)]
    pub title: String,
    /// The ticket status (open/resolved/…), mirrored so the board task reflects the ticket's state.
    #[serde(default)]
    pub status: String,
}

/// Select the tickets to ingest from a polled batch: those strictly newer than `cursor` (the group's last-seen
/// `lastUpdatedDate`). `cursor == None` is first run → every ticket is new (ingest the backlog; the
/// external-link dedup keeps it idempotent). RFC3339 sorts lexicographically, so a `>` string compare orders
/// by time. Strictly-greater (not `>=`) so a re-poll of the exact watermark ticket is not re-ingested.
///
/// Order-preserving and side-effect free: the caller decides ingest order and persists the cursor only after a
/// terminally-handled step.
pub fn new_since<'a>(batch: &'a [Ticket], cursor: Option<&str>) -> Vec<&'a Ticket> {
    batch
        .iter()
        .filter(|t| cursor.is_none_or(|cur| t.last_updated.as_str() > cur))
        .collect()
}

/// The newest `last_updated` across a batch — the value to advance the cursor to after the batch is
/// terminally handled. `None` for an empty batch (nothing to advance to). Max by lexicographic order (RFC3339).
pub fn newest_timestamp(batch: &[Ticket]) -> Option<&str> {
    batch.iter().map(|t| t.last_updated.as_str()).max()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ticket(id: &str, updated: &str) -> Ticket {
        Ticket {
            id: id.to_string(),
            last_updated: updated.to_string(),
            title: format!("ticket {id}"),
            status: "open".to_string(),
        }
    }

    #[test]
    fn deserialize_ignores_extra_fields_and_defaults_optionals() {
        let t: Ticket = serde_json::from_str(
            r#"{"id":"T1","last_updated":"2026-09-30T00:00:00Z","extra":"ignored"}"#,
        )
        .unwrap();
        assert_eq!(t.id, "T1");
        assert_eq!(t.last_updated, "2026-09-30T00:00:00Z");
        assert_eq!(t.title, "", "absent title defaults empty");
        assert_eq!(t.status, "");
    }

    #[test]
    fn first_run_ingests_the_whole_backlog() {
        let batch = vec![
            ticket("T1", "2026-09-28T00:00:00Z"),
            ticket("T2", "2026-09-30T00:00:00Z"),
        ];
        let got = new_since(&batch, None);
        assert_eq!(got.len(), 2, "cursor None → every ticket is new");
    }

    #[test]
    fn new_since_is_strictly_newer_than_the_cursor() {
        let batch = vec![
            ticket("old", "2026-09-01T00:00:00Z"),
            ticket("at-watermark", "2026-09-15T00:00:00Z"),
            ticket("new", "2026-09-20T00:00:00Z"),
        ];
        let got = new_since(&batch, Some("2026-09-15T00:00:00Z"));
        let ids: Vec<&str> = got.iter().map(|t| t.id.as_str()).collect();
        assert_eq!(
            ids,
            ["new"],
            "the exact watermark is not re-ingested; older dropped"
        );
    }

    #[test]
    fn newest_timestamp_is_the_max_and_none_when_empty() {
        let batch = vec![
            ticket("a", "2026-09-10T00:00:00Z"),
            ticket("b", "2026-09-29T12:00:00Z"),
            ticket("c", "2026-09-15T00:00:00Z"),
        ];
        assert_eq!(newest_timestamp(&batch), Some("2026-09-29T12:00:00Z"));
        assert_eq!(
            newest_timestamp(&[]),
            None,
            "empty batch → nothing to advance to"
        );
    }
}
