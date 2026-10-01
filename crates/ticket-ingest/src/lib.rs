//! `ticket-ingest` — the fleet's ticketing-source -> board ingest bridge (library crate).
//!
//! This crate is the read + sync end of approved design doc #2706 (task #784): an external ticketing
//! source's tickets mirror IN to coordination-board tasks so an agent can surface the ticket context around
//! an ops issue an operator is debugging. It is the THIRD adapter over the shared board ingest surface (Slack
//! is `crates/slack-bridge`, GitHub is `crates/github-bridge`): the board owns the ingest PRIMITIVES
//! (external-links/idempotency, external-identity, project routing) and this crate CONSUMES them via the
//! shared [`bridge_core`] board client, never reimplements them. Mirrored tickets file into the uncategorized
//! intake project UNASSIGNED; the bridge never routes or assigns (board-triage owns that).
//!
//! SOURCE-AGNOSTIC on purpose (public-repo boundary, doc #2706 A9): the external ticketing source is the
//! opaque [`sim::SOURCE`] token and ingest targets are opaque resolver-group strings set in deploy config. No
//! internal hostnames, resolver-group ids, aliases, or ticket content live in this public repo.
//!
//! The pure, transport-agnostic core (unit-tested here; wired to the live poll loop by the daemon binary):
//! - [`config`] — fail-soft config from a single **TOML file** (operator mandate #159: no env vars; only the
//!   file path is a `--config` CLI flag): the localhost board REST base, the resolver groups to ingest, the
//!   intake `project_id`, and the poll cadence. No daemon-held secret yet — the ticketing read side rides the
//!   fleet's shared ticketing session (doc #2706 A2; a provisioned programmatic identity is the follow-on,
//!   task #895).
//! - [`state`] — the per-group watermark cursor (last-seen `lastUpdatedDate`, RFC3339, advanced forward only),
//!   fail-soft load. The IN cursor that makes ingest lossless and resumable (doc #2706 A5).
//! - [`sim`] — the source-agnostic ticket domain model and the PURE "new since the cursor" selection the poll
//!   loop drives, plus the external-link namespacing ([`sim::SOURCE`]). The LIVE ticketing read transport is a
//!   follow-on slice (task #891); this is the tested seam it plugs into.
//!
//! The daemon binary (`src/main.rs` + `src/runner.rs`, behind the `daemon` feature) is a thin ASYNC
//! watermark poll loop on tokio that wires these together (operator directive #439: no blocking IO in rust
//! daemons — an async timer, not a `thread::sleep` loop) and is exercised live, not unit-tested; the gate is
//! this lib's `cargo test`.

pub mod config;
pub mod sim;
pub mod state;

#[cfg(feature = "daemon")]
pub mod runner;

pub use config::{Config, DEFAULT_CONFIG_FILENAME};
pub use sim::{SOURCE, Ticket, new_since, newest_timestamp};
pub use state::State;
