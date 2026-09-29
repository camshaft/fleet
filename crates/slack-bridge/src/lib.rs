//! `slack-bridge` — the fleet's Slack↔board bridge adapter (library crate).
//!
//! This crate is the transport + sync end of approved design #141: agents coordinate through the
//! coordination board; the board auto-mirrors to Slack; Slack (including operator DMs) syncs back. The
//! board owns the bridge CORE (channel-map, external-identity, outbound-authz — board tasks #149/#150/#151);
//! this crate is TRANSPORT + SYNC only (Slack Socket Mode) and consumes those board primitives.
//!
//! The pure, transport-agnostic core (unit-tested here, wired to the live Slack async transport by the
//! daemon binary in a later slice behind the `transport` feature):
//! - [`config`] — fail-soft config from a single **TOML file** (operator mandate #159: no env vars;
//!   only the file path is a `--config` CLI flag), including the localhost board REST base the firehose
//!   subscriber reads.
//!
//! Later slices add: `board` (firehose subscribe + post/read over the board's localhost REST/MCP),
//! `format` (board-event ↔ Slack mrkdwn shaping with external-author attribution), and the async
//! transport binary that wires them together.
//!
//! Kept generic on purpose: Slack-specifics live in this adapter; a second external-source adapter
//! (GitHub, #136) drops in over the same board core.

pub mod config;

pub use config::{Config, SlackTokens};
