//! The pure OUTBOUND reply planner: firehose events -> speakable replies.
//!
//! The audio loop's OUTBOUND side (board -> voice) polls the board firehose and speaks any authorized
//! `channel.outbound_reflect` on a mapped voice channel — George's reply to the current turn AND any
//! proactive message it posts unprompted flow through the same path. This module is the PURE decision: it
//! reuses [`bridge_core::plan_outbound`] to select + order the reflects for mapped channels and advance the
//! cursor, then renders each to speakable text. The async poll/speak shell (PR-2) is a thin wrapper over
//! this. Pure and unit-tested — no board I/O, no audio.

use bridge_core::{plan_outbound, Event};

use super::render::render_reply;

/// One reply resolved from the firehose, ready to speak. `event_seq` is the firehose seq it came from — the
/// speak loop advances its persisted cursor to this once the reply is terminally handled, so a restart
/// resumes without re-speaking or gapping.
#[derive(Debug, Clone, PartialEq)]
pub struct SpokenReply {
    pub event_seq: i64,
    pub text: String,
}

/// Plan the spoken replies from a batch of firehose events, advancing the cursor. `resolve` maps a board
/// channel id to the external voice channel (`None` = not a voice channel -> skipped). The cursor advances
/// to the batch max (even past skipped events) so nothing reprocesses, matching [`plan_outbound`].
///
/// A reflect whose body renders empty (e.g. a code-only post) is dropped from the spoken output but its
/// event still advances the cursor — there's nothing to say, and we must not re-poll it forever.
pub fn plan_replies<F>(events: &[Event], cursor: i64, resolve: F) -> (Vec<SpokenReply>, i64)
where
    F: Fn(i64) -> Option<String>,
{
    let (posts, new_cursor) = plan_outbound(events, cursor, resolve);
    let replies = posts
        .into_iter()
        .filter_map(|p| {
            let text = render_reply(&p.reflect);
            (!text.is_empty()).then_some(SpokenReply {
                event_seq: p.event_seq,
                text,
            })
        })
        .collect();
    (replies, new_cursor)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bridge_core::board::OUTBOUND_REFLECT;

    fn reflect_event(seq: i64, channel_id: i64, body: &str) -> Event {
        Event {
            seq,
            kind: OUTBOUND_REFLECT.to_string(),
            actor: Some("george".into()),
            channel_id: Some(channel_id),
            created_at: None,
            data: serde_json::json!({
                "channel_id": channel_id,
                "post_seq": seq * 10,
                "author": "george",
                "body": body,
            }),
        }
    }

    fn other_event(seq: i64) -> Event {
        Event {
            seq,
            kind: "channel.post".into(),
            actor: None,
            channel_id: Some(1),
            created_at: None,
            data: serde_json::Value::Null,
        }
    }

    // The one voice channel is board channel 7 -> external "voice:green".
    fn resolve(cid: i64) -> Option<String> {
        (cid == 7).then(|| "voice:green".to_string())
    }

    #[test]
    fn maps_reflects_to_rendered_replies_and_advances_cursor() {
        let events = [
            reflect_event(10, 7, "The build is **green**."),
            reflect_event(11, 7, "See [the PR](http://x)."),
        ];
        let (replies, cursor) = plan_replies(&events, 5, resolve);
        assert_eq!(replies.len(), 2);
        assert_eq!(replies[0].text, "The build is green.");
        assert_eq!(replies[0].event_seq, 10);
        assert_eq!(replies[1].text, "See the PR.");
        assert_eq!(cursor, 11);
    }

    #[test]
    fn skips_non_reflect_and_unmapped_but_advances_cursor() {
        let events = [
            other_event(20),
            reflect_event(21, 99, "orphan channel"), // 99 is not a voice channel
        ];
        let (replies, cursor) = plan_replies(&events, 0, resolve);
        assert!(replies.is_empty());
        assert_eq!(cursor, 21, "cursor advances past skipped/unmapped events");
    }

    #[test]
    fn empty_render_is_dropped_but_cursor_still_advances() {
        // A code-only reply renders to nothing; there's nothing to speak, but we must not re-poll it.
        let events = [reflect_event(30, 7, "```\nls -la\n```")];
        let (replies, cursor) = plan_replies(&events, 0, resolve);
        assert!(replies.is_empty(), "nothing speakable");
        assert_eq!(cursor, 30, "but the cursor advances so it isn't reprocessed");
    }

    #[test]
    fn empty_batch_keeps_cursor() {
        let (replies, cursor) = plan_replies(&[], 42, resolve);
        assert!(replies.is_empty());
        assert_eq!(cursor, 42);
    }
}
