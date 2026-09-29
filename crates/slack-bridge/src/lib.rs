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
//! - [`board`] — the token-less localhost board REST client: poll the event firehose (`GET /events`) and
//!   act on `channel.outbound_reflect` events (board-core #150), and post attributed inbound messages
//!   (`POST /channels/:id/posts`, board-core #149). Pure parsers unit-tested without a network.
//!
//! Later slices add: `format` (board-event ↔ Slack mrkdwn shaping with external-author attribution) and
//! the async transport binary (Socket Mode) that wires them together, behind the `transport` feature.
//!
//! Kept generic on purpose: Slack-specifics live in this adapter; a second external-source adapter
//! (GitHub, #136) drops in over the same board core.

pub mod board;
pub mod config;

pub use board::{BoardClient, Event, OutboundReflect, OUTBOUND_REFLECT};
pub use config::{Config, SlackTokens};
