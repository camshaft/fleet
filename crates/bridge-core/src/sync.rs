//! `sync` — the pure board↔external bidirectional sync PLANNING, independent of any live transport.
//!
//! This is where the sync *decisions* live so a transport binary stays a thin I/O shell:
//!   - [`plan_outbound`]: given a batch of firehose [`Event`]s + the current cursor + a board-channel →
//!     external-channel resolver, produce the posts to deliver (in `seq` order) and the new cursor.
//!   - [`plan_inbound`]: given an inbound external message + an external-channel → board-channel resolver,
//!     produce the attributed board post (`sender` = the bridge agent, `external_author` = `<source>:<id>`).
//!
//! The channel MAP is injected as a resolver closure, so this layer is decoupled from where the map comes
//! from AND stays generic across external sources — Slack, voice, GitHub all reuse it with their own
//! resolver + source string.
//!
//! Rendering is deliberately NOT done here: the outbound relay chooses the rich vs degraded render per its
//! per-message failure count (runtime state), so [`OutboundPost`] carries the raw [`OutboundReflect`] and
//! the transport renders at delivery time.

use crate::board::{build_post_body, Event, OutboundReflect};
use serde_json::Value;

/// Format the stable external-identity id the bridge attributes an inbound author with (board-core #149):
/// `"<source>:<id>"` (e.g. `slack:U123`, `voice:<speaker>`). Stable so the board can map it to a durable
/// external identity + resolve a display name (`external_author_name`, board-core #85).
pub fn external_author(source: &str, external_user_id: &str) -> String {
    format!("{source}:{external_user_id}")
}

/// A resolved outbound post: an authorized board reflect mapped to a concrete external channel. The
/// transport renders (rich / degraded) and delivers it, threading under a parent when the board post was a
/// reply.
#[derive(Debug, Clone, PartialEq)]
pub struct OutboundPost {
    /// The firehose event `seq` this post came from — the transport advances its persisted cursor to this
    /// after the post is terminally handled, so a restart resumes without re-posting or gapping.
    pub event_seq: i64,
    /// The external channel id to deliver into (resolved from the board `channel_id`).
    pub external_channel: String,
    /// The board reflect to render + deliver.
    pub reflect: OutboundReflect,
}

/// Plan the outbound posts from a batch of firehose events.
///
/// - Only `channel.outbound_reflect` events (board-core #150) whose board channel resolves to an external
///   channel become posts; everything else is skipped. Per #150 the event's existence IS the authorization
///   (the board already applied the outbound-author policy), so no re-checking here.
/// - The new cursor is the max `seq` across ALL events in the batch (even skipped ones), never less than
///   `cursor`, so a skipped/unmapped event is not reprocessed on the next poll.
///
/// Pure: `resolve` maps a board `channel_id` to an external channel id (`None` = unmapped → skip).
pub fn plan_outbound<F>(events: &[Event], cursor: i64, resolve: F) -> (Vec<OutboundPost>, i64)
where
    F: Fn(i64) -> Option<String>,
{
    let mut posts = Vec::new();
    let mut new_cursor = cursor;
    for ev in events {
        if ev.seq > new_cursor {
            new_cursor = ev.seq;
        }
        if let Some(reflect) = ev.as_outbound_reflect()
            && let Some(external_channel) = resolve(reflect.channel_id)
        {
            posts.push(OutboundPost {
                event_seq: ev.seq,
                external_channel,
                reflect,
            });
        }
    }
    (posts, new_cursor)
}

/// A resolved inbound post: an attributed board post to create, from an inbound external message.
#[derive(Debug, Clone, PartialEq)]
pub struct InboundPost {
    /// The board channel to post into (resolved from the external channel).
    pub board_channel_id: i64,
    /// The `POST /channels/:id/posts` JSON body (`sender` = bridge agent, `external_author` = `<source>:<id>`).
    pub body: Value,
}

/// Plan the board post for an inbound external message. `None` when the external channel doesn't map to a
/// board channel (the message is not mirrored). The post is attributed: `sender` = the bridge's own agent id
/// (so it isn't in `outbound_authors` and won't echo back OUT), `external_author` = `<source>:<user>`.
/// `reply_to` is the parent board post seq when the external message is a threaded reply.
///
/// Text is posted as-is (no `@agent`/operator-line parsing) — that routing is a per-channel concern kept
/// out of the generic sync so every transport reuses this unchanged.
pub fn plan_inbound<F>(
    external_channel: &str,
    source: &str,
    external_user_id: &str,
    text: &str,
    reply_to: Option<i64>,
    bridge_agent: &str,
    resolve_channel: F,
) -> Option<InboundPost>
where
    F: Fn(&str) -> Option<i64>,
{
    let board_channel_id = resolve_channel(external_channel)?;
    let ext_author = external_author(source, external_user_id);
    let body = build_post_body(bridge_agent, text, Some(&ext_author), reply_to);
    Some(InboundPost {
        board_channel_id,
        body,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev_reflect(seq: i64, channel_id: i64, post_seq: i64, body: &str) -> Event {
        Event {
            seq,
            kind: crate::board::OUTBOUND_REFLECT.to_string(),
            actor: Some("concierge".into()),
            channel_id: Some(channel_id),
            created_at: None,
            data: serde_json::json!({
                "channel_id": channel_id,
                "post_seq": post_seq,
                "author": "concierge",
                "body": body,
            }),
        }
    }

    fn ev_other(seq: i64) -> Event {
        Event {
            seq,
            kind: "channel.post".into(),
            actor: None,
            channel_id: Some(1),
            created_at: None,
            data: Value::Null,
        }
    }

    // ── plan_outbound ─────────────────────────────────────────────────────────────────────────────

    #[test]
    fn outbound_maps_reflect_events_to_posts() {
        let events = [ev_reflect(10, 7, 100, "hi"), ev_reflect(11, 8, 101, "yo")];
        let (posts, cursor) = plan_outbound(&events, 5, |cid| match cid {
            7 => Some("C7".into()),
            8 => Some("C8".into()),
            _ => None,
        });
        assert_eq!(posts.len(), 2);
        assert_eq!(posts[0].external_channel, "C7");
        assert_eq!(posts[0].reflect.body, "hi");
        assert_eq!(posts[0].event_seq, 10, "carries the firehose event seq");
        assert_eq!(posts[1].external_channel, "C8");
        assert_eq!(posts[1].event_seq, 11);
        assert_eq!(cursor, 11, "cursor advances to the max seq");
    }

    #[test]
    fn outbound_skips_non_reflect_events_but_advances_cursor() {
        let events = [ev_other(20), ev_reflect(21, 7, 5, "x")];
        let (posts, cursor) = plan_outbound(&events, 0, |_| Some("C7".into()));
        assert_eq!(posts.len(), 1, "only the reflect event becomes a post");
        assert_eq!(cursor, 21, "cursor advances past the skipped event too");
    }

    #[test]
    fn outbound_skips_unmapped_channels_but_advances_cursor() {
        let events = [ev_reflect(30, 99, 1, "orphan")];
        let (posts, cursor) = plan_outbound(&events, 10, |_| None);
        assert!(posts.is_empty());
        assert_eq!(cursor, 30, "unmapped event still advances the cursor (no reprocess loop)");
    }

    #[test]
    fn outbound_empty_batch_keeps_cursor() {
        let (posts, cursor) = plan_outbound(&[], 42, |_| Some("C".into()));
        assert!(posts.is_empty());
        assert_eq!(cursor, 42);
    }

    #[test]
    fn outbound_cursor_never_regresses_on_out_of_order_or_stale_seq() {
        let events = [ev_reflect(3, 7, 1, "old")];
        let (_posts, cursor) = plan_outbound(&events, 100, |_| Some("C7".into()));
        assert_eq!(cursor, 100, "cursor is monotonic — a lower seq doesn't regress it");
    }

    #[test]
    fn outbound_preserves_reply_to_and_external_author() {
        let mut ev = ev_reflect(40, 7, 200, "threaded");
        ev.data["reply_to"] = serde_json::json!(199);
        ev.data["external_author"] = serde_json::json!("slack:U5");
        let (posts, _) = plan_outbound(&[ev], 0, |_| Some("C7".into()));
        assert_eq!(posts[0].reflect.reply_to, Some(199));
        assert_eq!(posts[0].reflect.external_author.as_deref(), Some("slack:U5"));
    }

    // ── plan_inbound ──────────────────────────────────────────────────────────────────────────────

    #[test]
    fn inbound_builds_an_attributed_board_post() {
        let post = plan_inbound("C7", "slack", "U123", "hello fleet", None, "slack-bridge", |ch| {
            (ch == "C7").then_some(7)
        })
        .expect("mapped");
        assert_eq!(post.board_channel_id, 7);
        assert_eq!(post.body["sender"], "slack-bridge");
        assert_eq!(post.body["body"], "hello fleet");
        assert_eq!(post.body["external_author"], "slack:U123");
        assert!(post.body.get("reply_to").is_none());
    }

    #[test]
    fn inbound_is_source_agnostic() {
        // A voice transport reuses plan_inbound verbatim with its own source + speaker id.
        let post = plan_inbound("voice-1", "voice", "cameron", "hey assistant", None, "voice-bridge", |_| {
            Some(88)
        })
        .expect("mapped");
        assert_eq!(post.board_channel_id, 88);
        assert_eq!(post.body["sender"], "voice-bridge");
        assert_eq!(post.body["external_author"], "voice:cameron");
    }

    #[test]
    fn inbound_threads_a_reply() {
        let post = plan_inbound("C7", "slack", "U1", "re: that", Some(88), "slack-bridge", |_| Some(7))
            .expect("mapped");
        assert_eq!(post.body["reply_to"], 88);
    }

    #[test]
    fn inbound_unmapped_channel_is_skipped() {
        assert!(plan_inbound("Cnope", "slack", "U1", "x", None, "slack-bridge", |_| None).is_none());
    }

    #[test]
    fn external_author_is_stable_source_prefixed() {
        assert_eq!(external_author("slack", "U0ABC"), "slack:U0ABC");
        assert_eq!(external_author("voice", "cameron"), "voice:cameron");
    }
}
