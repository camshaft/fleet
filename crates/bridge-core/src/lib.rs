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
//!
//! Source-agnostic on purpose: `external_author = "<source>:<id>"` (e.g. `slack:U123`, `voice:<speaker>`),
//! and the external channel is an opaque `String`. Slack-, voice-, or GitHub-specifics live in the
//! transport crate, never here.

pub mod board;
pub mod channel_config;
pub mod relay;
pub mod resolver;
pub mod sse;
pub mod sync;

pub use board::{
    build_identity_body, build_post_body, parse_channel_links, parse_channels, parse_events,
    BoardChannel, BoardClient, Event, OutboundReflect, LINK_KIND_CHANNEL, LINK_SOURCE,
    OUTBOUND_REFLECT,
};
pub use sse::{SseDecoder, SseFrame};
pub use channel_config::{bridged_channels, Bridged, BridgeConfig};
pub use relay::{relay_plan, RelayPlan, RELAY_DEGRADE_AFTER, RELAY_QUARANTINE_AFTER, RELAY_QUEUE_WARN};
pub use resolver::{ChannelLink, ChannelMap};
pub use sync::{external_author, plan_inbound, plan_outbound, InboundPost, OutboundPost};
