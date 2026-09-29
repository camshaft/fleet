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
//! - [`format`] — board ↔ Slack message shaping: render an outbound-reflect as Slack mrkdwn (with
//!   external-author attribution, HTML-escaping, length-capping + a degraded plain variant / relay-plan
//!   resilience), and parse an operator's Slack line into a routed [`format::Intent`].
//! - [`sync`] — the pure bidirectional-sync planning (firehose events → Slack posts + cursor advance;
//!   inbound Slack → attributed board post), with the board↔Slack channel MAP injected as a resolver so
//!   the adapter stays decoupled from board-core #149 slice-2 and generic across external sources.
//! - [`resolver`] — the concrete board↔Slack channel MAP built from the TOML config (`[[channel_map]]`),
//!   providing the bidirectional lookups the sync planner takes; swappable for a board-backed map later.
//!
//! Later slices add the async transport binary (Socket Mode) that wires these together, behind the
//! `transport` feature.
//!
//! Kept generic on purpose: Slack-specifics live in this adapter; a second external-source adapter
//! (GitHub, #136) drops in over the same board core.

pub mod board;
pub mod config;
pub mod format;
pub mod resolver;
pub mod sync;

pub use board::{parse_channel_links, BoardClient, Event, OutboundReflect, OUTBOUND_REFLECT};
pub use config::{Config, SlackTokens};
pub use format::{
    help_text, is_valid_agent_name, parse_operator_message, relay_plan, render_outbound_reflect,
    render_outbound_reflect_plain, Intent, RelayPlan, RELAY_QUEUE_WARN,
};
pub use resolver::{ChannelLink, ChannelMap};
pub use sync::{plan_inbound, plan_outbound, slack_external_author, InboundPost, OutboundPost};
