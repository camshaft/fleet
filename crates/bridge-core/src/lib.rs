//! `bridge-core` — the transport-agnostic core shared by every board↔external-channel bridge.
//!
//! A fleet bridge mirrors a board channel ⟷ an external channel (a Slack DM, a voice session, …). The
//! board owns the bridge PRIMITIVES (channel-map, external-identity, outbound-authz — board tasks
//! #149/#150/#151); this crate is the reusable client + sync PLANNING over them, with everything
//! external-source-specific injected by the caller. A concrete bridge = this core + a per-transport layer
//! (Slack Socket Mode + mrkdwn in `slack-bridge`; audio wake→STT / synth→play in the voice bridge, #316).
//!
//! - [`board`] — the token-less localhost board REST client: poll the firehose (`GET /events`), post an
//!   attributed inbound message (`POST /channels/:id/posts`), read/register channel links
//!   (`/external-links`), and register an external identity's display name (`POST /external-identities`).
//!   Pure parsers/builders are unit-tested without a network.
//! - [`sync`] — the PURE bidirectional planning: firehose events → outbound posts (+ cursor advance);
//!   an inbound external message → an attributed board post. The board↔external channel map is injected as
//!   a resolver closure, so the core is decoupled from where the map comes from AND from the transport.
//! - [`resolver`] — the concrete bidirectional board↔external [`resolver::ChannelMap`] the planners take.
//! - [`relay`] — the outbound relay-resilience escalation ([`relay::relay_plan`]): a message that
//!   deterministically fails to deliver degrades then quarantines, so it never head-of-line-blocks the
//!   outbound loop. Transport-agnostic (the transport supplies the actual rich/degraded render).
//! - [`sse`] — a spec-compliant Server-Sent Events decoder so a bridge CONSUMES the board firehose as a push
//!   stream ([`board::BoardClient::stream_events`]) instead of polling `GET /events` on a timer (#363).
//! - [`mention`] — a pure text wake-word detector ([`mention::is_mentioned`]): a bridge rouses a named agent
//!   only when a message ADDRESSES it by name (#429), not on every message.
//! - [`echo`] — a board-origin marker ([`echo::mark_board_origin`] / [`echo::is_board_origin`]): stamp every
//!   reflected outbound message so the inbound poller drops it and the bridge never re-ingests its own posts
//!   (#430 echo-loop suppression).
//! - [`task_client`] — the shared TASK/AGENT REST surface (task_355): register-or-upsert with an optional
//!   `webhook_url`, subscribe to a project, and task CRUD. Complements [`board::BoardClient`] (the
//!   channel/firehose surface) — any non-session daemon needs this regardless of whether it bridges a
//!   channel. Extracted from the `kb` ingest workers so a future daemon port does not re-derive it.
//!
//! Source-agnostic on purpose: `external_author = "<source>:<id>"` (e.g. `slack:U123`, `voice:<speaker>`),
//! and the external channel is an opaque `String`. Slack-, voice-, or GitHub-specifics live in the
//! transport crate, never here.

pub mod board;
pub mod channel_config;
pub mod echo;
pub mod mention;
pub mod refs;
pub mod relay;
pub mod resolver;
pub mod sse;
pub mod sync;
pub mod task_client;

pub use board::{
    BoardChannel, BoardClient, Event, LINK_KIND_CHANNEL, LINK_SOURCE, OUTBOUND_REFLECT,
    OutboundReflect, build_identity_body, build_post_body, parse_channel_links, parse_channels,
    parse_events,
};
pub use channel_config::{BridgeConfig, Bridged, bridged_channels};
pub use echo::{MARK_EMOJI, MARK_EMOJI_SHORTCODE, is_board_origin, mark_board_origin};
pub use mention::{is_mentioned, mentioned_agent};
pub use refs::{REF_KINDS, RefMatch, RefScheme, find_refs, linkify};
pub use relay::{
    RELAY_DEGRADE_AFTER, RELAY_QUARANTINE_AFTER, RELAY_QUEUE_WARN, RelayPlan, relay_plan,
};
pub use resolver::{ChannelLink, ChannelMap};
pub use sse::{SseDecoder, SseFrame};
pub use sync::{InboundPost, OutboundPost, external_author, plan_inbound, plan_outbound};
pub use task_client::{Board as TaskBoard, Task as BoardTask};
