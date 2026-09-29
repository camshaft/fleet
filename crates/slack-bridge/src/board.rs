//! `board` — the bridge's client for the coordination board's **token-less localhost REST** surface.
//!
//! The bridge daemon runs OUTSIDE a Claude session, so it can't use the in-session board MCP tools — it
//! uses the board's plain REST API (the way the `fleet` orchestrator's `board.rs` reads `/board/api/*`).
//! Two directions:
//!
//! - **OUT (board → Slack)**: subscribe to the board-wide event firehose and act on
//!   [`OUTBOUND_REFLECT`] (`channel.outbound_reflect`) events. Per board-core #150, the board has ALREADY
//!   applied the concierge-only OUT authz — the mere *existence* of the event IS the authorization, so the
//!   bridge reflects every one it sees to the mapped Slack channel and never re-checks direction/authors.
//!   The firehose is `GET /events?since_seq=<seq>&limit=<n>` (append-only, ascending `seq`; poll with the
//!   last seq you saw). An SSE variant (`GET /stream`, resume via `Last-Event-ID`) exists too; this client
//!   implements the simpler poll and keeps the parsing pure so the transport loop owns the cursor.
//! - **IN (Slack → board)**: post an attributed message via `POST /channels/:id/posts` with the bridge's
//!   own agent id as `sender` and the Slack user as `external_author` (external-identity, board-core #149).
//!   Because the bridge agent isn't in the channel's `outbound_authors`, its own inbound posts don't echo
//!   back out as `channel.outbound_reflect` events (no loop).
//!
//! The HTTP methods are thin wrappers over ureq; all PARSING/SHAPING is factored into pure functions
//! ([`parse_events`], [`Event::as_outbound_reflect`], [`build_post_body`]) that are unit-tested without a
//! network. The board channel_id → Slack channel MAP is board-core #149 slice 2 (not landed yet); until
//! it does, an [`OutboundReflect`] carries the board `channel_id` and the transport layer resolves it.

use crate::resolver::ChannelLink;
use serde::Deserialize;
use serde_json::{Value, json};

/// The firehose event type the bridge reflects OUT to Slack (board-core #150).
pub const OUTBOUND_REFLECT: &str = "channel.outbound_reflect";

/// The external-link `source` this adapter owns (board-core #149 slice 2's generic external_link table,
/// shared with the GitHub adapter #136 which uses its own source).
pub const LINK_SOURCE: &str = "slack";
/// The external-link `board_kind` for a channel↔channel link (vs `task` for #136/#151 links).
pub const LINK_KIND_CHANNEL: &str = "channel";

/// One event from the board-wide firehose (`GET /events`): append-only, ascending `seq`, ALL types.
///
/// Only the envelope fields the bridge needs are modeled; `data` stays a raw [`Value`] and is decoded
/// per-type on demand (see [`Event::as_outbound_reflect`]). Unknown envelope keys are ignored (forward
/// compatible — the board may add event types/fields the bridge doesn't care about).
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Event {
    /// Monotonic append-only sequence number; the firehose cursor (`since_seq` / SSE `Last-Event-ID`).
    pub seq: i64,
    /// The event type discriminator, e.g. `channel.outbound_reflect`.
    #[serde(rename = "type")]
    pub kind: String,
    /// The acting agent/sender, when the event carries one.
    #[serde(default)]
    pub actor: Option<String>,
    /// The board channel this event is about, when applicable.
    #[serde(default)]
    pub channel_id: Option<i64>,
    /// RFC3339 timestamp the board stamped, when present.
    #[serde(default)]
    pub created_at: Option<String>,
    /// The per-type payload, decoded on demand.
    #[serde(default)]
    pub data: Value,
}

impl Event {
    /// Decode this event as a [`OutboundReflect`] iff it's a `channel.outbound_reflect` — else `None`
    /// (a different type, or a payload that doesn't match the expected shape). Never panics.
    pub fn as_outbound_reflect(&self) -> Option<OutboundReflect> {
        if self.kind != OUTBOUND_REFLECT {
            return None;
        }
        serde_json::from_value(self.data.clone()).ok()
    }
}

/// The payload of a `channel.outbound_reflect` event (board-core #150): a board post the concierge
/// authorized to reflect OUT to the mapped Slack channel.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct OutboundReflect {
    /// The board channel the post lives in (→ resolved to a Slack channel via the #149 slice-2 map).
    pub channel_id: i64,
    /// The board post's own sequence number (for dedup / idempotency in the transport layer).
    pub post_seq: i64,
    /// The board author of the post (an agent id).
    pub author: String,
    /// The message body to reflect.
    pub body: String,
    /// The parent post seq when this is a threaded reply (→ a Slack threaded reply).
    #[serde(default)]
    pub reply_to: Option<i64>,
    /// The external-identity id when the post was itself attributed to an external human (board-core #149).
    #[serde(default)]
    pub external_author: Option<String>,
}

/// Parse the JSON body of `GET /events` into the event list. The board returns either a bare array (as
/// `/agents` does) or an `{ "events": [...] }` envelope — accept both. Returns the parse error text on a
/// body that is neither.
pub fn parse_events(body: &str) -> Result<Vec<Event>, String> {
    let v: Value = serde_json::from_str(body)
        .map_err(|e| format!("board /events: response was not JSON: {e}"))?;
    let arr = match v {
        Value::Array(a) => a,
        Value::Object(ref o) => match o.get("events") {
            Some(Value::Array(a)) => a.clone(),
            _ => return Err(format!("board /events: object without an `events` array: {v}")),
        },
        other => return Err(format!("board /events: expected an array or {{events:[…]}}, got {other}")),
    };
    arr.into_iter()
        .map(|e| serde_json::from_value::<Event>(e).map_err(|err| format!("board /events: bad event: {err}")))
        .collect()
}

/// Build the JSON body for an inbound post (`POST /channels/:id/posts`). `sender` is the bridge's own
/// board agent id; `external_author` attributes the originating Slack user (e.g. `slack:U123`); `reply_to`
/// threads under a parent post. Pure — unit-tested. Omits the optional keys when absent (rather than
/// sending explicit nulls) so the board applies its own defaults.
pub fn build_post_body(
    sender: &str,
    body: &str,
    external_author: Option<&str>,
    reply_to: Option<i64>,
) -> Value {
    let mut m = json!({ "sender": sender, "body": body });
    if let Some(ea) = external_author {
        m["external_author"] = json!(ea);
    }
    if let Some(rt) = reply_to {
        m["reply_to"] = json!(rt);
    }
    m
}

/// One row of the board's generic `external_link` table (board-core #149 slice 2). Only the fields the
/// channel map needs are modeled; `external_parent_id`/`metadata` and any future columns are ignored.
#[derive(Debug, Clone, Deserialize)]
struct ExternalLink {
    source: String,
    /// The external side of the link — for a Slack channel link, the Slack channel id (e.g. `C123`).
    external_id: String,
    board_kind: String,
    /// The board side — for a channel link, the board `channel_id`.
    board_id: i64,
}

/// Parse the JSON body of `GET /external-links` into the board↔Slack CHANNEL links. Accepts a bare array
/// or an `{ "external_links": [...] }` / `{ "links": [...] }` envelope. Only `source == "slack"` +
/// `board_kind == "channel"` rows become [`ChannelLink`]s (defense-in-depth even though we filter in the
/// query); other rows (e.g. `board_kind == "task"`) are skipped.
pub fn parse_channel_links(body: &str) -> Result<Vec<ChannelLink>, String> {
    let v: Value = serde_json::from_str(body)
        .map_err(|e| format!("board /external-links: response was not JSON: {e}"))?;
    let arr = match v {
        Value::Array(a) => a,
        Value::Object(ref o) => match o.get("external_links").or_else(|| o.get("links")) {
            Some(Value::Array(a)) => a.clone(),
            _ => return Err(format!("board /external-links: object without a links array: {v}")),
        },
        other => {
            return Err(format!(
                "board /external-links: expected an array or {{external_links:[…]}}, got {other}"
            ));
        }
    };
    let mut links = Vec::new();
    for row in arr {
        let link: ExternalLink = serde_json::from_value(row)
            .map_err(|e| format!("board /external-links: bad row: {e}"))?;
        if link.source == LINK_SOURCE && link.board_kind == LINK_KIND_CHANNEL {
            links.push(ChannelLink {
                board_channel_id: link.board_id,
                slack_channel: link.external_id,
            });
        }
    }
    Ok(links)
}

/// A handle to the board's token-less localhost REST API (stateless — each call is one request). The
/// firehose cursor (`since_seq`) is owned by the caller (the transport loop), not this client.
pub struct BoardClient {
    base: String,
    agent: ureq::Agent,
}

impl BoardClient {
    /// Build a client against the board REST base (e.g. `http://127.0.0.1:8880/board/api`). No network
    /// round-trip — the REST API is sessionless. A trailing slash on `base_api` is trimmed so path joins
    /// don't double up.
    pub fn new(base_api: &str) -> Self {
        BoardClient {
            base: base_api.trim_end_matches('/').to_string(),
            agent: ureq::agent(),
        }
    }

    /// Poll the firehose for events after `since_seq` (exclusive), up to `limit`. Returns them in
    /// ascending `seq` order; an empty vec when nothing is newer.
    pub fn poll_events(&self, since_seq: i64, limit: usize) -> Result<Vec<Event>, String> {
        let url = format!("{}/events?since_seq={}&limit={}", self.base, since_seq, limit);
        let resp = self
            .agent
            .get(&url)
            .set("accept", "application/json")
            .call()
            .map_err(|e| format!("board GET /events failed: {e}"))?;
        let raw = resp
            .into_string()
            .map_err(|e| format!("board GET /events read failed: {e}"))?;
        parse_events(&raw)
    }

    /// Post an inbound (Slack → board) message into board channel `channel_id`, attributed to
    /// `external_author` (the Slack user) with the bridge as `sender`. `reply_to` threads under a parent.
    pub fn post_message(
        &self,
        channel_id: i64,
        sender: &str,
        body: &str,
        external_author: Option<&str>,
        reply_to: Option<i64>,
    ) -> Result<(), String> {
        self.post_raw(channel_id, &build_post_body(sender, body, external_author, reply_to))
    }

    /// Post a pre-built post body (as produced by [`build_post_body`] / [`crate::sync::plan_inbound`]) to
    /// board channel `channel_id`. The transport uses this so it posts exactly the tested planner output.
    pub fn post_raw(&self, channel_id: i64, body: &Value) -> Result<(), String> {
        let url = format!("{}/channels/{}/posts", self.base, channel_id);
        self.agent
            .post(&url)
            .set("content-type", "application/json")
            .send_string(&body.to_string())
            .map_err(|e| format!("board POST /channels/{channel_id}/posts failed: {e}"))?;
        Ok(())
    }

    /// Read the board-registered Slack channel links (board-core #149 slice 2). The transport merges these
    /// with the static config `[[channel_map]]` to build the live [`crate::resolver::ChannelMap`], so the
    /// operator-DM channel and any board-registered mapping resolve without a config edit.
    pub fn list_channel_links(&self) -> Result<Vec<ChannelLink>, String> {
        let url = format!(
            "{}/external-links?source={LINK_SOURCE}&board_kind={LINK_KIND_CHANNEL}",
            self.base
        );
        let resp = self
            .agent
            .get(&url)
            .set("accept", "application/json")
            .call()
            .map_err(|e| format!("board GET /external-links failed: {e}"))?;
        let raw = resp
            .into_string()
            .map_err(|e| format!("board GET /external-links read failed: {e}"))?;
        parse_channel_links(&raw)
    }

    /// Register (idempotent on `(source, external_id)`) a board channel ↔ Slack channel link (board-core
    /// #149 slice 2). Used by the cutover (#154) to wire the operator-DM board channel to the operator's
    /// Slack DM without a static config entry.
    pub fn register_channel_link(&self, board_channel_id: i64, slack_channel: &str) -> Result<(), String> {
        let url = format!("{}/external-links", self.base);
        let body = json!({
            "source": LINK_SOURCE,
            "external_id": slack_channel,
            "board_kind": LINK_KIND_CHANNEL,
            "board_id": board_channel_id,
        })
        .to_string();
        self.agent
            .post(&url)
            .set("content-type", "application/json")
            .send_string(&body)
            .map_err(|e| format!("board POST /external-links failed: {e}"))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_events_accepts_a_bare_array() {
        let body = r#"[
            {"seq": 1, "type": "channel.post", "actor": "concierge", "channel_id": 7, "data": {}},
            {"seq": 2, "type": "channel.outbound_reflect", "channel_id": 7,
             "data": {"channel_id": 7, "post_seq": 42, "author": "concierge", "body": "hi"}}
        ]"#;
        let evs = parse_events(body).unwrap();
        assert_eq!(evs.len(), 2);
        assert_eq!(evs[0].seq, 1);
        assert_eq!(evs[1].kind, OUTBOUND_REFLECT);
    }

    #[test]
    fn parse_events_accepts_an_events_envelope() {
        let body = r#"{"events": [{"seq": 5, "type": "x", "data": null}]}"#;
        let evs = parse_events(body).unwrap();
        assert_eq!(evs.len(), 1);
        assert_eq!(evs[0].seq, 5);
    }

    #[test]
    fn parse_events_rejects_non_array_json() {
        assert!(parse_events(r#"{"nope": 1}"#).is_err());
        assert!(parse_events("not json at all").is_err());
    }

    #[test]
    fn parse_events_tolerates_unknown_envelope_keys() {
        // Forward compatible: an event with extra keys the bridge doesn't model still parses.
        let body = r#"[{"seq": 9, "type": "t", "actor": "a", "created_at": "2026-09-29T00:00:00Z",
                        "channel_id": 3, "data": {"k": 1}, "future_field": "ignored"}]"#;
        let evs = parse_events(body).unwrap();
        assert_eq!(evs[0].seq, 9);
        assert_eq!(evs[0].created_at.as_deref(), Some("2026-09-29T00:00:00Z"));
    }

    #[test]
    fn as_outbound_reflect_decodes_the_payload() {
        let body = r#"[{"seq": 2, "type": "channel.outbound_reflect", "channel_id": 7,
            "data": {"channel_id": 7, "post_seq": 42, "author": "concierge", "body": "ship it",
                     "reply_to": 40, "external_author": "slack:U9"}}]"#;
        let ev = &parse_events(body).unwrap()[0];
        let r = ev.as_outbound_reflect().expect("decodes");
        assert_eq!(r.channel_id, 7);
        assert_eq!(r.post_seq, 42);
        assert_eq!(r.author, "concierge");
        assert_eq!(r.body, "ship it");
        assert_eq!(r.reply_to, Some(40));
        assert_eq!(r.external_author.as_deref(), Some("slack:U9"));
    }

    #[test]
    fn as_outbound_reflect_none_for_other_types() {
        let body = r#"[{"seq": 1, "type": "channel.post", "data": {"body": "x"}}]"#;
        assert!(parse_events(body).unwrap()[0].as_outbound_reflect().is_none());
    }

    #[test]
    fn as_outbound_reflect_none_for_malformed_payload() {
        // Right type, wrong payload shape (missing required fields) → None, never a panic.
        let body = r#"[{"seq": 1, "type": "channel.outbound_reflect", "data": {"body": "x"}}]"#;
        assert!(parse_events(body).unwrap()[0].as_outbound_reflect().is_none());
    }

    #[test]
    fn as_outbound_reflect_defaults_optional_fields() {
        let body = r#"[{"seq": 3, "type": "channel.outbound_reflect",
            "data": {"channel_id": 1, "post_seq": 8, "author": "a", "body": "b"}}]"#;
        let r = parse_events(body).unwrap()[0].as_outbound_reflect().unwrap();
        assert_eq!(r.reply_to, None);
        assert_eq!(r.external_author, None);
    }

    #[test]
    fn build_post_body_minimal_omits_optionals() {
        let v = build_post_body("slack-bridge", "hello", None, None);
        assert_eq!(v["sender"], "slack-bridge");
        assert_eq!(v["body"], "hello");
        assert!(v.get("external_author").is_none(), "no explicit null");
        assert!(v.get("reply_to").is_none(), "no explicit null");
    }

    #[test]
    fn build_post_body_includes_attribution_and_thread() {
        let v = build_post_body("slack-bridge", "hi", Some("slack:U1"), Some(12));
        assert_eq!(v["external_author"], "slack:U1");
        assert_eq!(v["reply_to"], 12);
    }

    #[test]
    fn client_new_trims_trailing_slash() {
        let c = BoardClient::new("http://x/board/api/");
        assert_eq!(c.base, "http://x/board/api");
    }

    // ── external-links / channel map (board-core #149 slice 2) ──────────────────────────────────────

    #[test]
    fn parse_channel_links_bare_array() {
        let body = r#"[
            {"source": "slack", "external_id": "C7", "board_kind": "channel", "board_id": 7},
            {"source": "slack", "external_id": "C8", "board_kind": "channel", "board_id": 8,
             "external_parent_id": null, "metadata": {"note": "ignored"}}
        ]"#;
        let links = parse_channel_links(body).unwrap();
        assert_eq!(links.len(), 2);
        assert_eq!(links[0].board_channel_id, 7);
        assert_eq!(links[0].slack_channel, "C7");
        assert_eq!(links[1].board_channel_id, 8);
    }

    #[test]
    fn parse_channel_links_envelope_forms() {
        let a = parse_channel_links(
            r#"{"external_links": [{"source":"slack","external_id":"C1","board_kind":"channel","board_id":1}]}"#,
        )
        .unwrap();
        assert_eq!(a.len(), 1);
        let b = parse_channel_links(
            r#"{"links": [{"source":"slack","external_id":"C2","board_kind":"channel","board_id":2}]}"#,
        )
        .unwrap();
        assert_eq!(b[0].board_channel_id, 2);
    }

    #[test]
    fn parse_channel_links_skips_non_channel_and_non_slack_rows() {
        // A task link and a different-source row must be filtered out — only slack/channel rows map.
        let body = r#"[
            {"source": "slack",  "external_id": "C7",   "board_kind": "channel", "board_id": 7},
            {"source": "slack",  "external_id": "T99",  "board_kind": "task",    "board_id": 99},
            {"source": "github", "external_id": "org/r#5", "board_kind": "task", "board_id": 5}
        ]"#;
        let links = parse_channel_links(body).unwrap();
        assert_eq!(links.len(), 1, "only the slack/channel row survives");
        assert_eq!(links[0].slack_channel, "C7");
    }

    #[test]
    fn parse_channel_links_rejects_non_array() {
        assert!(parse_channel_links(r#"{"nope": 1}"#).is_err());
        assert!(parse_channel_links("not json").is_err());
    }

    #[test]
    fn parse_channel_links_empty_is_ok() {
        assert!(parse_channel_links("[]").unwrap().is_empty());
    }
}
