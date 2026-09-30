//! `bridge` — the voice daemon's board-bridge layer, built on the shared `bridge-core` crate (#316/#342).
//!
//! The re-architected voice assistant is a thin transport bridge (Doc #18): the audio loop is the only
//! voice-specific part, and everything board-facing reuses `bridge-core` (the REST client, the pure
//! `plan_inbound`/`plan_outbound` planners, the channel map, relay resilience). This module holds the
//! voice-side pieces that sit ABOVE that contract:
//!   - [`render`] — a board reply -> plain speakable text (Markdown stripped) for the TTS engine.
//!   - [`reply`]  — the pure OUTBOUND planner: firehose events -> ordered [`reply::SpokenReply`]s + cursor.
//!   - [`cursor`] — firehose cursor persistence so a restart resumes without re-speaking or gapping.
//!
//! INBOUND (a finalized transcript -> an attributed board post) reuses `bridge_core::plan_inbound` +
//! `BoardClient::post_raw` directly, with `source = "voice"` and `external_author = "voice:<speaker>"`.
//! The async poll/post/speak shell that drives these lives with the audio loop (PR-2), off in the
//! `runtime` feature; this module stays pure and unit-tested in the default build.

pub mod cursor;
pub mod render;
pub mod reply;

pub use cursor::{load_cursor, save_cursor};
pub use render::render_reply;
pub use reply::{plan_replies, SpokenReply};
