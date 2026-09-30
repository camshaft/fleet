//! `board` — the bridge's client for the coordination board's **token-less localhost REST** surface.
//!
//! A bridge daemon runs OUTSIDE a Claude session, so it can't use the in-session board MCP tools — it uses
//! the board's plain REST API. Two directions:
//!
//! - **OUT (board → external)**: subscribe to the board-wide event firehose and act on [`OUTBOUND_REFLECT`]
//!   (`channel.outbound_reflect`) events. Per board-core #150, the board has ALREADY applied the outbound
//!   authz — the mere *existence* of the event IS the authorization, so the bridge reflects every one it
//!   sees to the mapped external channel and never re-checks direction/authors. The firehose is
//!   `GET /events?since_seq=<seq>&limit=<n>` (append-only, ascending `seq`; poll with the last seq seen).
//! - **IN (external → board)**: post an attributed message via `POST /channels/:id/posts` with the bridge's
//!   own agent id as `sender` and the external identity as `external_author` (`<source>:<id>`, board-core
//!   #149). Because the bridge agent isn't in the channel's `outbound_authors`, its own inbound posts don't
//!   echo back out as `channel.outbound_reflect` events (no loop).
//!
//! The HTTP methods are thin **async** wrappers over reqwest (operator directive #370: async I/O on tokio,
//! not a thread-per-blocking-call model — the caller provides the runtime); all PARSING/SHAPING is factored
//! into pure, sync functions ([`parse_events`], [`Event::as_outbound_reflect`], [`build_post_body`],
//! [`build_identity_body`], [`parse_channel_links`]) that are unit-tested without a network or a runtime.

use crate::resolver::ChannelLink;
use crate::sse::{SseDecoder, SseFrame};
use reqwest::Client;
use serde::Deserialize;
use serde_json::{json, Value};

/// The firehose event type the bridge reflects OUT to the external channel (board-core #150).
pub const OUTBOUND_REFLECT: &str = "channel.outbound_reflect";

/// The default external-link/identity `source` for a Slack bridge. Other transports pass their own
/// (e.g. `"voice"`), so this is a convenience default, not a hard-coded assumption in the core paths.
pub const LINK_SOURCE: &str = "slack";
/// The external-link `board_kind` for a channel↔channel link (vs `task` for task-link adapters).
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

/// The payload of a `channel.outbound_reflect` event (board-core #150): a board post an authorized author
/// posted, to reflect OUT to the mapped external channel.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct OutboundReflect {
    /// The board channel the post lives in (→ resolved to an external channel via the channel map).
    pub channel_id: i64,
    /// The board post's own sequence number (for dedup / idempotency in the transport layer).
    pub post_seq: i64,
    /// The board author of the post (an agent id).
    pub author: String,
    /// The message body to reflect.
    pub body: String,
    /// The parent post seq when this is a threaded reply (→ an external threaded reply).
    #[serde(default)]
    pub reply_to: Option<i64>,
    /// The external-identity id when the post was itself attributed to an external human (board-core #149).
    #[serde(default)]
    pub external_author: Option<String>,
}

/// Parse the JSON body of `GET /events` into the event list. The board returns either a bare array or an
/// `{ "events": [...] }` envelope — accept both. Returns the parse error text on a body that is neither.
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

/// Decode one SSE frame's `data` payload into board events. The firehose sends one event per SSE message, so
/// a single-`Event` JSON object is the common case; fall back to the array / `{ "events": [...] }` envelope
/// [`parse_events`] accepts, for robustness against a batched frame. An empty `data` (a keepalive / id-only
/// cursor frame) yields no events. Pure — the frame's own `id` is the resume cursor and is tracked by the
/// caller independently, so an event whose `seq` is carried only by the SSE `id` still advances the stream.
pub fn frame_events(frame: &SseFrame) -> Result<Vec<Event>, String> {
    let data = frame.data.trim();
    if data.is_empty() {
        return Ok(Vec::new());
    }
    // A single event object is the expected board framing; the envelope forms are a robustness fallback.
    if let Ok(ev) = serde_json::from_str::<Event>(data) {
        return Ok(vec![ev]);
    }
    parse_events(data)
}

/// Build the JSON body for an inbound post (`POST /channels/:id/posts`). `sender` is the bridge's own board
/// agent id; `external_author` attributes the originating external identity (e.g. `slack:U123`,
/// `voice:<speaker>`); `reply_to` threads under a parent post. Pure — unit-tested. Omits the optional keys
/// when absent (rather than sending explicit nulls) so the board applies its own defaults.
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

/// Build the JSON body for an external-identity upsert (`POST /external-identities`, board-core #149):
/// map a stable identity `id` (e.g. `slack:U123`) + `source` to a human `display_name`. The board resolves
/// this to `external_author_name` alongside the stable `external_author` key on read (board-core #85), so
/// agents see WHO posted rather than a bare id. Pure — unit-tested. Idempotent server-side (a null
/// `display_name` preserves the prior value; we only ever send one when we've resolved a name).
pub fn build_identity_body(id: &str, source: &str, display_name: &str) -> Value {
    json!({ "id": id, "source": source, "display_name": display_name })
}

/// One row of the board's generic `external_link` table (board-core #149 slice 2). Only the fields the
/// channel map needs are modeled; `external_parent_id`/`metadata` and any future columns are ignored.
#[derive(Debug, Clone, Deserialize)]
struct ExternalLink {
    source: String,
    /// The external side of the link — for a channel link, the external channel id (e.g. `C123`).
    external_id: String,
    board_kind: String,
    /// The board side — for a channel link, the board `channel_id`.
    board_id: i64,
}

/// Parse the JSON body of `GET /external-links` into the board↔external CHANNEL links. Accepts a bare array
/// or an `{ "external_links": [...] }` / `{ "links": [...] }` envelope. Only rows matching `source` +
/// `board_kind == "channel"` become [`ChannelLink`]s (defense-in-depth even though we filter in the query);
/// other rows (e.g. `board_kind == "task"`, a different source) are skipped.
pub fn parse_channel_links(body: &str, source: &str) -> Result<Vec<ChannelLink>, String> {
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
        if link.source == source && link.board_kind == LINK_KIND_CHANNEL {
            links.push(ChannelLink {
                board_channel_id: link.board_id,
                external_channel: link.external_id,
            });
        }
    }
    Ok(links)
}

/// One board channel as returned by `GET /channels`. Only the fields a bridge needs are modeled; `metadata`
/// stays a raw [`Value`] (the board sometimes stores it as an object and sometimes as a JSON-encoded string)
/// so a transport can pull its own config key out of it. Unknown fields are ignored (forward compatible).
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct BoardChannel {
    /// The board channel id.
    pub id: i64,
    /// The channel name.
    #[serde(default)]
    pub name: String,
    /// The channel's metadata bag (per-channel config lives here, e.g. a bridge's `*_bridge_config`).
    #[serde(default)]
    pub metadata: Value,
}

/// Parse the JSON body of `GET /channels` into [`BoardChannel`]s. Accepts a bare array or a
/// `{ "channels": [...] }` envelope. Pure — unit-tested without a network. A bridge reads its per-channel
/// config out of each channel's `metadata` (see e.g. the membrain bridge's `slack_bridge_config`).
pub fn parse_channels(body: &str) -> Result<Vec<BoardChannel>, String> {
    let v: Value =
        serde_json::from_str(body).map_err(|e| format!("board /channels: response was not JSON: {e}"))?;
    let arr = match v {
        Value::Array(a) => a,
        Value::Object(ref o) => match o.get("channels") {
            Some(Value::Array(a)) => a.clone(),
            _ => return Err(format!("board /channels: object without a `channels` array: {v}")),
        },
        other => return Err(format!("board /channels: expected an array or {{channels:[…]}}, got {other}")),
    };
    arr.into_iter()
        .map(|c| serde_json::from_value::<BoardChannel>(c).map_err(|e| format!("board /channels: bad channel: {e}")))
        .collect()
}

/// A handle to the board's token-less localhost REST API (stateless — each call is one request). The
/// firehose cursor (`since_seq`) is owned by the caller (the transport loop), not this client.
pub struct BoardClient {
    base: String,
    http: Client,
}

impl BoardClient {
    /// Build a client against the board REST base (e.g. `http://127.0.0.1:8079/api`). No network round-trip
    /// — the REST API is sessionless, and the reqwest `Client` is constructed without a runtime. A trailing
    /// slash on `base_api` is trimmed so path joins don't double.
    pub fn new(base_api: &str) -> Self {
        BoardClient {
            base: base_api.trim_end_matches('/').to_string(),
            http: Client::new(),
        }
    }

    /// Poll the firehose for events after `since_seq` (exclusive), up to `limit`. Returns them in ascending
    /// `seq` order; an empty vec when nothing is newer.
    pub async fn poll_events(&self, since_seq: i64, limit: usize) -> Result<Vec<Event>, String> {
        let url = format!("{}/events?since_seq={}&limit={}", self.base, since_seq, limit);
        let raw = self
            .http
            .get(&url)
            .header("accept", "application/json")
            .send()
            .await
            .map_err(|e| format!("board GET /events failed: {e}"))?
            .error_for_status()
            .map_err(|e| format!("board GET /events failed: {e}"))?
            .text()
            .await
            .map_err(|e| format!("board GET /events read failed: {e}"))?;
        parse_events(&raw)
    }

    /// CONSUME the firehose as a Server-Sent Events push stream (operator directive #363) instead of polling
    /// `poll_events` on a timer: open `GET /events` with `Accept: text/event-stream`, resuming after
    /// `since_seq` via the `Last-Event-ID` header, and invoke `on_event(seq, Event)` for each decoded event as
    /// the board pushes it. `seq` is the SSE frame id when present (the authoritative resume cursor — see
    /// [`Event::seq`]), else the event's own `seq`; the caller persists it so a reconnect resumes exactly.
    ///
    /// Returns `Ok(())` when the server closes the stream (the caller reconnects, or falls back to polling);
    /// an `Err` on a connect/transport failure. A frame whose `data` fails to decode is skipped (logged to
    /// stderr) rather than propagated, so one malformed event can't wedge the stream — the same fail-soft
    /// posture as the poll loop. Between events this awaits the socket (the point of the push model).
    pub async fn stream_events<F>(&self, since_seq: i64, mut on_event: F) -> Result<(), String>
    where
        F: FnMut(i64, Event),
    {
        let url = format!("{}/events", self.base);
        let mut req = self
            .http
            .get(&url)
            .header("accept", "text/event-stream");
        // Resume from the last seq the caller durably saw (0 = from the current head, no Last-Event-ID).
        if since_seq > 0 {
            req = req.header("last-event-id", since_seq.to_string());
        }
        let mut resp = req
            .send()
            .await
            .map_err(|e| format!("board GET /events (SSE) failed: {e}"))?
            .error_for_status()
            .map_err(|e| format!("board GET /events (SSE) failed: {e}"))?;

        let mut decoder = SseDecoder::new();
        while let Some(chunk) = resp
            .chunk()
            .await
            .map_err(|e| format!("board /events (SSE) read failed: {e}"))?
        {
            for frame in decoder.push(&chunk) {
                // The frame id is the resume cursor when present; else fall back to a decoded event's seq.
                let frame_seq: Option<i64> = frame.id.as_deref().and_then(|s| s.parse().ok());
                match frame_events(&frame) {
                    Ok(events) => {
                        for ev in events {
                            let seq = frame_seq.unwrap_or(ev.seq);
                            on_event(seq, ev);
                        }
                    }
                    Err(e) => {
                        // Skip a malformed frame but still advance the cursor past it (fail-soft — one bad
                        // event never wedges the stream), matching the poll loop's batch-advance behavior.
                        eprintln!("board /events (SSE): skipping undecodable frame (id={:?}): {e}", frame.id);
                    }
                }
            }
        }
        Ok(())
    }

    /// Post an inbound (external → board) message into board channel `channel_id`, attributed to
    /// `external_author` (the external identity) with the bridge as `sender`. `reply_to` threads a parent.
    pub async fn post_message(
        &self,
        channel_id: i64,
        sender: &str,
        body: &str,
        external_author: Option<&str>,
        reply_to: Option<i64>,
    ) -> Result<(), String> {
        self.post_raw(channel_id, &build_post_body(sender, body, external_author, reply_to))
            .await
    }

    /// Post a pre-built post body (as produced by [`build_post_body`] / [`crate::sync::plan_inbound`]) to
    /// board channel `channel_id`. The transport uses this so it posts exactly the tested planner output.
    pub async fn post_raw(&self, channel_id: i64, body: &Value) -> Result<(), String> {
        let url = format!("{}/channels/{}/posts", self.base, channel_id);
        self.http
            .post(&url)
            .header("content-type", "application/json")
            .body(body.to_string())
            .send()
            .await
            .map_err(|e| format!("board POST /channels/{channel_id}/posts failed: {e}"))?
            .error_for_status()
            .map_err(|e| format!("board POST /channels/{channel_id}/posts failed: {e}"))?;
        Ok(())
    }

    /// Read the board-registered channel links for `source` (board-core #149 slice 2). The transport merges
    /// these with any static config links to build the live [`crate::resolver::ChannelMap`].
    pub async fn list_channel_links(&self, source: &str) -> Result<Vec<ChannelLink>, String> {
        let url = format!(
            "{}/external-links?source={source}&board_kind={LINK_KIND_CHANNEL}",
            self.base
        );
        let raw = self
            .http
            .get(&url)
            .header("accept", "application/json")
            .send()
            .await
            .map_err(|e| format!("board GET /external-links failed: {e}"))?
            .error_for_status()
            .map_err(|e| format!("board GET /external-links failed: {e}"))?
            .text()
            .await
            .map_err(|e| format!("board GET /external-links read failed: {e}"))?;
        parse_channel_links(&raw, source)
    }

    /// List all board channels (`GET /channels`) with their metadata. A bridge filters these by its own
    /// per-channel config key in `metadata` (e.g. the membrain bridge's `slack_bridge_config.bridge_instance`)
    /// to discover the channels it manages — live, without a restart.
    pub async fn list_channels(&self) -> Result<Vec<BoardChannel>, String> {
        let url = format!("{}/channels", self.base);
        let raw = self
            .http
            .get(&url)
            .header("accept", "application/json")
            .send()
            .await
            .map_err(|e| format!("board GET /channels failed: {e}"))?
            .error_for_status()
            .map_err(|e| format!("board GET /channels failed: {e}"))?
            .text()
            .await
            .map_err(|e| format!("board GET /channels read failed: {e}"))?;
        parse_channels(&raw)
    }

    /// Register (idempotent on `(source, external_id)`) a board channel ↔ external channel link (board-core
    /// #149 slice 2).
    pub async fn register_channel_link(
        &self,
        board_channel_id: i64,
        source: &str,
        external_channel: &str,
    ) -> Result<(), String> {
        let url = format!("{}/external-links", self.base);
        let body = json!({
            "source": source,
            "external_id": external_channel,
            "board_kind": LINK_KIND_CHANNEL,
            "board_id": board_channel_id,
        })
        .to_string();
        self.http
            .post(&url)
            .header("content-type", "application/json")
            .body(body)
            .send()
            .await
            .map_err(|e| format!("board POST /external-links failed: {e}"))?
            .error_for_status()
            .map_err(|e| format!("board POST /external-links failed: {e}"))?;
        Ok(())
    }

    /// Upsert (idempotent on `id`) an external identity's display name (board-core #149; live independent of
    /// the #85 rendering redeploy). Attaches a resolved display name to the stable `<source>:<id>` key so
    /// board readers see `external_author_name` instead of a bare id. Best-effort at the call site.
    pub async fn upsert_external_identity(
        &self,
        id: &str,
        source: &str,
        display_name: &str,
    ) -> Result<(), String> {
        let url = format!("{}/external-identities", self.base);
        self.http
            .post(&url)
            .header("content-type", "application/json")
            .body(build_identity_body(id, source, display_name).to_string())
            .send()
            .await
            .map_err(|e| format!("board POST /external-identities failed: {e}"))?
            .error_for_status()
            .map_err(|e| format!("board POST /external-identities failed: {e}"))?;
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
    fn build_identity_body_shape() {
        let v = build_identity_body("slack:U0ALLK04G3T", "slack", "Cameron Bytheway");
        assert_eq!(v["id"], "slack:U0ALLK04G3T");
        assert_eq!(v["source"], "slack");
        assert_eq!(v["display_name"], "Cameron Bytheway");
    }

    #[test]
    fn client_new_trims_trailing_slash() {
        let c = BoardClient::new("http://x/api/");
        assert_eq!(c.base, "http://x/api");
    }

    #[test]
    fn parse_channel_links_bare_array() {
        let body = r#"[
            {"source": "slack", "external_id": "C7", "board_kind": "channel", "board_id": 7},
            {"source": "slack", "external_id": "C8", "board_kind": "channel", "board_id": 8,
             "external_parent_id": null, "metadata": {"note": "ignored"}}
        ]"#;
        let links = parse_channel_links(body, "slack").unwrap();
        assert_eq!(links.len(), 2);
        assert_eq!(links[0].board_channel_id, 7);
        assert_eq!(links[0].external_channel, "C7");
        assert_eq!(links[1].board_channel_id, 8);
    }

    #[test]
    fn parse_channel_links_envelope_forms() {
        let a = parse_channel_links(
            r#"{"external_links": [{"source":"slack","external_id":"C1","board_kind":"channel","board_id":1}]}"#,
            "slack",
        )
        .unwrap();
        assert_eq!(a.len(), 1);
        let b = parse_channel_links(
            r#"{"links": [{"source":"slack","external_id":"C2","board_kind":"channel","board_id":2}]}"#,
            "slack",
        )
        .unwrap();
        assert_eq!(b[0].board_channel_id, 2);
    }

    #[test]
    fn parse_channel_links_skips_non_channel_and_other_source_rows() {
        let body = r#"[
            {"source": "slack",  "external_id": "C7",   "board_kind": "channel", "board_id": 7},
            {"source": "slack",  "external_id": "T99",  "board_kind": "task",    "board_id": 99},
            {"source": "voice",  "external_id": "V1",   "board_kind": "channel", "board_id": 5}
        ]"#;
        let links = parse_channel_links(body, "slack").unwrap();
        assert_eq!(links.len(), 1, "only the slack/channel row survives the slack filter");
        assert_eq!(links[0].external_channel, "C7");
        // A voice bridge filtering source="voice" gets its own row, not slack's.
        let vlinks = parse_channel_links(body, "voice").unwrap();
        assert_eq!(vlinks.len(), 1);
        assert_eq!(vlinks[0].external_channel, "V1");
    }

    #[test]
    fn parse_channel_links_rejects_non_array() {
        assert!(parse_channel_links(r#"{"nope": 1}"#, "slack").is_err());
        assert!(parse_channel_links("not json", "slack").is_err());
    }

    #[test]
    fn parse_channel_links_empty_is_ok() {
        assert!(parse_channel_links("[]", "slack").unwrap().is_empty());
    }

    #[test]
    fn parse_channels_bare_array_and_envelope() {
        let body = r#"[
            {"id": 30, "name": "operator-dm", "metadata": {"outbound_authors": ["concierge"]}},
            {"id": 31, "name": "bare"}
        ]"#;
        let chans = parse_channels(body).unwrap();
        assert_eq!(chans.len(), 2);
        assert_eq!(chans[0].id, 30);
        assert_eq!(chans[0].name, "operator-dm");
        assert_eq!(chans[0].metadata["outbound_authors"][0], "concierge");
        assert_eq!(chans[1].id, 31, "a channel with no metadata still parses");
        assert!(chans[1].metadata.is_null());

        let env = parse_channels(r#"{"channels": [{"id": 7}]}"#).unwrap();
        assert_eq!(env[0].id, 7);
    }

    #[test]
    fn parse_channels_rejects_non_channel_json() {
        assert!(parse_channels(r#"{"nope": 1}"#).is_err());
        assert!(parse_channels("not json").is_err());
    }

    #[test]
    fn frame_events_decodes_a_single_event_object() {
        let frame = SseFrame {
            id: Some("7".into()),
            event: Some("message".into()),
            data: r#"{"seq":7,"type":"channel.outbound_reflect","channel_id":3,
                     "data":{"channel_id":3,"post_seq":9,"author":"frank","body":"hi"}}"#
                .into(),
        };
        let evs = frame_events(&frame).unwrap();
        assert_eq!(evs.len(), 1);
        assert_eq!(evs[0].seq, 7);
        assert_eq!(evs[0].kind, OUTBOUND_REFLECT);
        assert_eq!(evs[0].as_outbound_reflect().unwrap().author, "frank");
    }

    #[test]
    fn frame_events_empty_data_yields_nothing() {
        // A keepalive / id-only cursor frame carries no event.
        let frame = SseFrame { id: Some("12".into()), event: None, data: String::new() };
        assert!(frame_events(&frame).unwrap().is_empty());
        // Whitespace-only data is also treated as empty.
        let ws = SseFrame { data: "   \n ".into(), ..Default::default() };
        assert!(frame_events(&ws).unwrap().is_empty());
    }

    #[test]
    fn frame_events_falls_back_to_array_and_envelope() {
        // A batched frame: an array of events.
        let arr = SseFrame {
            data: r#"[{"seq":1,"type":"a","data":null},{"seq":2,"type":"b","data":null}]"#.into(),
            ..Default::default()
        };
        let evs = frame_events(&arr).unwrap();
        assert_eq!(evs.len(), 2);
        assert_eq!(evs[1].seq, 2);
        // And the {events:[...]} envelope.
        let env = SseFrame { data: r#"{"events":[{"seq":5,"type":"x","data":null}]}"#.into(), ..Default::default() };
        assert_eq!(frame_events(&env).unwrap()[0].seq, 5);
    }

    #[test]
    fn frame_events_surfaces_a_bad_payload() {
        let bad = SseFrame { data: "not json".into(), ..Default::default() };
        assert!(frame_events(&bad).is_err());
    }
}
