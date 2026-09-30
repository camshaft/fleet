//! `slack-bridge` — the fleet's Slack↔board bridge adapter (library crate).
//!
//! This crate is the transport + sync end of approved design #141: agents coordinate through the
//! coordination board; the board auto-mirrors to Slack; Slack (including operator DMs) syncs back. The
//! board owns the bridge CORE (channel-map, external-identity, outbound-authz — board tasks #149/#150/#151);
//! this crate is TRANSPORT + SYNC only (Slack Socket Mode) and consumes those board primitives.
//!
//! The transport-agnostic core (board REST client, sync planning, channel map, relay-resilience) now lives
//! in the shared [`bridge_core`] crate, reused by every board↔external bridge (Slack here; the voice bridge
//! in #316). This crate is the Slack-SPECIFIC layer over it: TOML config, Slack-mrkdwn shaping, and the
//! async Socket Mode transport binary (behind the `transport` feature). The core types are re-exported here
//! for convenience so the daemon and the tests can use `slack_bridge::…` uniformly.
//!
//! - [`config`] — fail-soft config from a single **TOML file** (operator mandate #159: no env vars;
//!   only the file path is a `--config` CLI flag), including the localhost board REST base the firehose
//!   subscriber reads. Slack-specific (holds the Slack credentials + `[[channel_map]]`).
//! - [`format`] — board ↔ Slack message shaping: render an outbound-reflect as Slack mrkdwn (with
//!   external-author attribution, HTML-escaping, length-capping + a degraded plain variant that pairs with
//!   [`bridge_core::relay`]), and parse an operator's Slack line into a routed [`format::Intent`].
//! - re-exported from [`bridge_core`]: [`BoardClient`], the [`Event`]/[`OutboundReflect`] firehose types,
//!   the [`ChannelMap`]/[`ChannelLink`] map, the [`plan_outbound`]/[`plan_inbound`] sync planners, and the
//!   [`relay_plan`] escalation.
//!
//! Kept generic on purpose: Slack-specifics live in this adapter; a second external-source adapter
//! (the voice bridge #316, GitHub #136) drops in over the same [`bridge_core`].

pub mod config;
pub mod format;

pub use bridge_core::{
    external_author, parse_channel_links, plan_inbound, plan_outbound, relay_plan, BoardClient,
    ChannelLink, ChannelMap, Event, InboundPost, OutboundPost, OutboundReflect, RelayPlan,
    LINK_SOURCE, OUTBOUND_REFLECT, RELAY_QUEUE_WARN,
};
pub use config::{Config, SlackTokens};
pub use format::{
    help_text, is_valid_agent_name, parse_operator_message, render_outbound_reflect,
    render_outbound_reflect_plain, Intent,
};
