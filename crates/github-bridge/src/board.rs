//! `board` — the bridge's client for the coordination board's **token-less localhost REST** surface.
//!
//! The bridge daemon runs OUTSIDE a Claude session, so it can't use the in-session board MCP tools — it uses
//! the board's plain REST API (the way the `fleet` orchestrator's `board.rs` reads `/board/api/*`). The two
//! directions this GitHub adapter drives over that surface:
//!
//! - **OUT (board → GitHub)**: subscribe to the board-wide event firehose (`GET /events?since_seq=<seq>`,
//!   append-only, ascending `seq`) and act on the authorized-reflect events (board-core #150). Per #150 the
//!   board has ALREADY applied the concierge-only OUT authz — the mere *existence* of the reflect event IS
//!   the authorization, so a later slice reflects each one it sees to the linked GitHub issue and never
//!   re-checks direction/authors. This slice lands the firehose *subscribe* (envelope + [`poll_events`]);
//!   decoding the task-comment reflect payload is the OUT-reflect slice (its exact event contract is being
//!   confirmed with v-task-board, since #150 shipped for channel posts and GitHub reflects a *task comment*).
//! - **IN (GitHub → board)**: create a mirrored board task per ingested issue (`POST /tasks`) and add
//!   attributed comments (`POST /tasks/:id/comments`) with the bridge's own agent id as author and the GitHub
//!   user as `external_author` (external-identity, board-core #149). The issue↔task and comment↔comment
//!   mapping is durably recorded in the board's generic `external_link` table (board-core #149 slice 2 /
//!   #151) so a restart is idempotent and never double-creates.
//!
//! The HTTP methods are thin wrappers over ureq; all PARSING/SHAPING is factored into pure functions
//! ([`parse_events`], [`parse_issue_links`], [`build_task_body`], [`build_comment_body`],
//! [`build_identity_body`]) that are unit-tested without a network.

use serde::Deserialize;
use serde_json::{Value, json};

/// A browser-like User-Agent for every board call. The default base is the loopback proxy (no Cloudflare),
/// but if `board_api` points at the PUBLIC endpoint, the CF edge 403s a non-browser UA
/// ("browser_signature_banned", fleet #209) — so send a browser-ish UA defensively; harmless on loopback.
/// (Matches the `fleet` orchestrator's board client.)
const BOARD_UA: &str = "Mozilla/5.0 (X11; Linux x86_64) github-bridge";

/// The firehose event type the bridge reflects OUT to GitHub (board-core #264): an authorized board TASK
/// COMMENT to mirror onto the linked GitHub issue. The board has ALREADY applied the per-link authz
/// (`external_links.metadata` `{direction, outbound_authors}`) — the mere existence of the event IS the
/// authorization, one event per authorized link, so the bridge reflects every one whose `source` is ours and
/// never re-checks direction/authors. (Distinct from Slack's channel-scoped `channel.outbound_reflect`.)
pub const TASK_OUTBOUND_REFLECT: &str = "task.outbound_reflect";

/// The external-link `source` this adapter owns in the board's generic `external_link` table (board-core
/// #149 slice 2). Distinct from the Slack adapter's `"slack"` so the two adapters' links never collide.
pub const LINK_SOURCE: &str = "github";
/// The external-link `board_kind` for an issue↔task link (an issue mirrors to a board TASK, not a channel).
pub const LINK_KIND_TASK: &str = "task";
/// The external-link `board_kind` for a synced GitHub comment. Recorded so a re-poll doesn't re-post an
/// already-mirrored comment (idempotent attributed-comment sync); `board_id` is the task the comment lives on.
pub const LINK_KIND_COMMENT: &str = "comment";

/// The canonical external id for a GitHub issue link: `owner/repo#number` (e.g. `camshaft/fleet#42`). Stable
/// and human-legible; the board's `external_link.external_id` for the issue↔task row.
pub fn issue_ref(repo: &str, number: i64) -> String {
    format!("{repo}#{number}")
}

/// The canonical external id for a synced GitHub comment: `owner/repo#c<comment_id>` (the comment id is
/// globally unique within GitHub, so the issue number isn't needed to disambiguate). The `external_link`
/// `external_id` for a `board_kind="comment"` row — the dedup key for attributed-comment sync.
pub fn comment_ref(repo: &str, comment_id: i64) -> String {
    format!("{repo}#c{comment_id}")
}

/// Parse an issue-link `external_id` back into `(repo, issue_number)` — the inverse of [`issue_ref`]. Used by
/// the OUT path to turn a `task.outbound_reflect`'s `external_id` into the repo + issue number to post to.
/// Splits on the LAST `#` (a repo name never contains `#`, the number always follows the final one) and
/// requires a valid trailing integer; returns `None` on any other shape (a comment ref `…#c<id>`, garbage).
pub fn parse_issue_ref(external_id: &str) -> Option<(String, i64)> {
    let (repo, num) = external_id.rsplit_once('#')?;
    if repo.is_empty() {
        return None;
    }
    num.parse::<i64>().ok().map(|n| (repo.to_string(), n))
}

/// One event from the board-wide firehose (`GET /events`): append-only, ascending `seq`, ALL types.
///
/// Only the envelope fields the bridge needs are modeled; `data` stays a raw [`Value`] and is decoded
/// per-type on demand. Unknown envelope keys are ignored (forward compatible — the board may add event
/// types/fields the bridge doesn't care about).
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Event {
    /// Monotonic append-only sequence number; the firehose cursor (`since_seq`).
    pub seq: i64,
    /// The event type discriminator, e.g. `channel.outbound_reflect` / `task.commented`.
    #[serde(rename = "type")]
    pub kind: String,
    /// The acting agent/sender, when the event carries one.
    #[serde(default)]
    pub actor: Option<String>,
    /// The board task this event is about, when applicable (a GitHub-adapter reflect is about a task).
    #[serde(default)]
    pub task_id: Option<i64>,
    /// The board channel this event is about, when applicable (present for channel-scoped events).
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
    /// Decode this event as a [`TaskReflect`] iff it's a `task.outbound_reflect` — else `None` (a different
    /// type, or a payload that doesn't match the expected shape). Never panics.
    pub fn as_task_reflect(&self) -> Option<TaskReflect> {
        if self.kind != TASK_OUTBOUND_REFLECT {
            return None;
        }
        serde_json::from_value(self.data.clone()).ok()
    }
}

/// The payload of a `task.outbound_reflect` event (board-core #264): a board task comment the board
/// authorized to reflect OUT, one event per authorized `external_link`. The bridge filters on
/// [`source`](TaskReflect::source) == [`LINK_SOURCE`] and posts [`body`](TaskReflect::body) to the GitHub
/// issue at [`external_id`](TaskReflect::external_id).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct TaskReflect {
    /// The board task the comment lives on.
    pub task_id: i64,
    /// The board comment's own id (for dedup / idempotency on the OUT side).
    pub comment_id: i64,
    /// The comment body to reflect.
    pub body: String,
    /// The board author of the comment (an agent id, e.g. `concierge`).
    pub author: String,
    /// The external-identity id when the comment was itself attributed to an external human (board-core #149).
    #[serde(default)]
    pub external_author: Option<String>,
    /// The external-link `source` this reflect is for — the bridge acts only on its own ([`LINK_SOURCE`]).
    pub source: String,
    /// The external side of the link — for GitHub, the issue ref `owner/repo#number` (see [`parse_issue_ref`]).
    pub external_id: String,
    /// The external parent, when the target is nested (e.g. an issue-vs-comment parenting); often absent.
    #[serde(default)]
    pub external_parent_id: Option<String>,
}

/// Parse the JSON body of `GET /events` into the event list. The board returns either a bare array or an
/// `{ "events": [...] }` envelope — accept both. Returns the parse error text on a body that is neither.
pub fn parse_events(body: &str) -> Result<Vec<Event>, String> {
    let v: Value =
        serde_json::from_str(body).map_err(|e| format!("board /events: response was not JSON: {e}"))?;
    let arr = match v {
        Value::Array(a) => a,
        Value::Object(ref o) => match o.get("events") {
            Some(Value::Array(a)) => a.clone(),
            _ => return Err(format!("board /events: object without an `events` array: {v}")),
        },
        other => {
            return Err(format!("board /events: expected an array or {{events:[…]}}, got {other}"));
        }
    };
    arr.into_iter()
        .map(|e| serde_json::from_value::<Event>(e).map_err(|err| format!("board /events: bad event: {err}")))
        .collect()
}

/// Build the JSON body for creating a mirrored board task from an ingested GitHub issue (`POST /tasks`).
/// `project_id` selects the board project; `created_by` is the bridge's own agent id; `external_author`
/// attributes the originating GitHub user (e.g. `github:octocat`). Pure — unit-tested. Omits the optional key
/// when absent (rather than sending an explicit null) so the board applies its own defaults.
pub fn build_task_body(
    project_id: i64,
    title: &str,
    description: &str,
    created_by: &str,
    external_author: Option<&str>,
) -> Value {
    let mut m = json!({
        "project_id": project_id,
        "title": title,
        "description": description,
        "created_by": created_by,
    });
    if let Some(ea) = external_author {
        m["external_author"] = json!(ea);
    }
    m
}

/// Build the JSON body for an attributed task comment (`POST /tasks/:id/comments`). `author` is the bridge's
/// own board agent id; `external_author` attributes the originating GitHub user. Pure — unit-tested. Omits
/// the optional key when absent so the board applies its own defaults.
pub fn build_comment_body(author: &str, body: &str, external_author: Option<&str>) -> Value {
    let mut m = json!({ "author": author, "body": body });
    if let Some(ea) = external_author {
        m["external_author"] = json!(ea);
    }
    m
}

/// Build the JSON body for an external-identity upsert (`POST /external-identities`, board-core #149): map a
/// stable identity `id` (e.g. `github:octocat`) + `source` to a human `display_name`. The board resolves this
/// to `external_author_name` alongside the stable `external_author` key on read (board-core #85), so agents
/// see WHO posted rather than a bare id. Pure — unit-tested. Idempotent server-side.
pub fn build_identity_body(id: &str, source: &str, display_name: &str) -> Value {
    json!({ "id": id, "source": source, "display_name": display_name })
}

/// One row of the board's generic `external_link` table (board-core #149 slice 2). Only the fields the
/// issue↔task map needs are modeled; any future columns are ignored.
#[derive(Debug, Clone, Deserialize)]
struct ExternalLink {
    source: String,
    /// The external side — for an issue link, the GitHub issue ref (`owner/repo#number`).
    external_id: String,
    board_kind: String,
    /// The board side — for an issue link, the board task id.
    board_id: i64,
}

/// A resolved GitHub-issue ↔ board-task link (board-core #149 slice 2 / #151). Lets ingest be idempotent: an
/// issue already linked to a task is updated in place rather than re-created.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IssueTaskLink {
    /// The board task id.
    pub board_task_id: i64,
    /// The GitHub issue ref (`owner/repo#number`).
    pub issue_ref: String,
}

/// Parse the JSON body of `GET /external-links` into the GitHub issue↔task links. Accepts a bare array or an
/// `{ "external_links": [...] }` / `{ "links": [...] }` envelope. Only `source == "github"` +
/// `board_kind == "task"` rows become [`IssueTaskLink`]s (defense-in-depth even though we filter in the
/// query); other rows (e.g. the Slack adapter's channel links) are skipped.
pub fn parse_issue_links(body: &str) -> Result<Vec<IssueTaskLink>, String> {
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
        let link: ExternalLink =
            serde_json::from_value(row).map_err(|e| format!("board /external-links: bad row: {e}"))?;
        if link.source == LINK_SOURCE && link.board_kind == LINK_KIND_TASK {
            links.push(IssueTaskLink {
                board_task_id: link.board_id,
                issue_ref: link.external_id,
            });
        }
    }
    Ok(links)
}

/// Parse the JSON body of `GET /external-links?...&board_kind=comment` into the set of synced comment refs
/// (the `external_id`s). Accepts the same array / envelope shapes as [`parse_issue_links`]. Only
/// `source == "github"` + `board_kind == "comment"` rows contribute.
pub fn parse_comment_refs(body: &str) -> Result<std::collections::HashSet<String>, String> {
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
    let mut refs = std::collections::HashSet::new();
    for row in arr {
        let link: ExternalLink =
            serde_json::from_value(row).map_err(|e| format!("board /external-links: bad row: {e}"))?;
        if link.source == LINK_SOURCE && link.board_kind == LINK_KIND_COMMENT {
            refs.insert(link.external_id);
        }
    }
    Ok(refs)
}

/// A handle to the board's token-less localhost REST API (stateless — each call is one request). The firehose
/// cursor (`since_seq`) is owned by the caller (the poll loop), not this client.
pub struct BoardClient {
    base: String,
    agent: ureq::Agent,
}

impl BoardClient {
    /// Build a client against the board REST base (e.g. `http://127.0.0.1:8079/api`). No network
    /// round-trip — the REST API is sessionless. A trailing slash on `base_api` is trimmed so path joins
    /// don't double up.
    pub fn new(base_api: &str) -> Self {
        BoardClient { base: base_api.trim_end_matches('/').to_string(), agent: ureq::agent() }
    }

    /// Poll the firehose for events after `since_seq` (exclusive), up to `limit`. Returns them in ascending
    /// `seq` order; an empty vec when nothing is newer.
    pub fn poll_events(&self, since_seq: i64, limit: usize) -> Result<Vec<Event>, String> {
        let url = format!("{}/events?since_seq={}&limit={}", self.base, since_seq, limit);
        let resp = self
            .agent
            .get(&url)
            .set("accept", "application/json")
            .set("user-agent", BOARD_UA)
            .call()
            .map_err(|e| format!("board GET /events failed: {e}"))?;
        let raw = resp.into_string().map_err(|e| format!("board GET /events read failed: {e}"))?;
        parse_events(&raw)
    }

    /// Read the board-registered GitHub issue↔task links (board-core #149 slice 2). Ingest reads these to
    /// stay idempotent — an issue already linked to a task is updated, not re-created.
    pub fn list_issue_links(&self) -> Result<Vec<IssueTaskLink>, String> {
        let url = format!("{}/external-links?source={LINK_SOURCE}&board_kind={LINK_KIND_TASK}", self.base);
        let resp = self
            .agent
            .get(&url)
            .set("accept", "application/json")
            .set("user-agent", BOARD_UA)
            .call()
            .map_err(|e| format!("board GET /external-links failed: {e}"))?;
        let raw =
            resp.into_string().map_err(|e| format!("board GET /external-links read failed: {e}"))?;
        parse_issue_links(&raw)
    }

    /// Register (idempotent on `(source, external_id)`) a GitHub issue ↔ board task link (board-core #149
    /// slice 2). Called by ingest right after it creates the mirrored task, so a later poll finds the link
    /// and doesn't re-create.
    pub fn register_issue_link(&self, board_task_id: i64, issue_ref: &str) -> Result<(), String> {
        self.register_link(LINK_KIND_TASK, issue_ref, board_task_id)
    }

    /// Register (idempotent) a synced-comment link so a re-poll doesn't re-post the comment. `board_id` is
    /// the task the comment lives on; `comment_ref` is [`comment_ref`]'s `owner/repo#c<id>`.
    pub fn register_comment_link(&self, board_task_id: i64, comment_ref: &str) -> Result<(), String> {
        self.register_link(LINK_KIND_COMMENT, comment_ref, board_task_id)
    }

    /// Register a `source="github"` external link of the given `board_kind` (shared by issue + comment
    /// links). Idempotent on `(source, external_id)` server-side.
    fn register_link(&self, board_kind: &str, external_id: &str, board_id: i64) -> Result<(), String> {
        let url = format!("{}/external-links", self.base);
        let body = json!({
            "source": LINK_SOURCE,
            "external_id": external_id,
            "board_kind": board_kind,
            "board_id": board_id,
        })
        .to_string();
        self.post_json(&url, &body, "POST /external-links")
    }

    /// Read the set of already-synced GitHub comment refs (`board_kind="comment"`) — the dedup set the
    /// attributed-comment sync passes to `sync::plan_comment_ingest` so a re-poll never double-posts.
    pub fn list_comment_refs(&self) -> Result<std::collections::HashSet<String>, String> {
        let url =
            format!("{}/external-links?source={LINK_SOURCE}&board_kind={LINK_KIND_COMMENT}", self.base);
        let resp = self
            .agent
            .get(&url)
            .set("accept", "application/json")
            .set("user-agent", BOARD_UA)
            .call()
            .map_err(|e| format!("board GET /external-links (comments) failed: {e}"))?;
        let raw = resp
            .into_string()
            .map_err(|e| format!("board GET /external-links (comments) read failed: {e}"))?;
        parse_comment_refs(&raw)
    }

    /// Create a mirrored board task from an ingested issue (`POST /tasks`), returning its numeric `id`. The
    /// caller then records the issue↔task link via [`Self::register_issue_link`].
    pub fn create_task(
        &self,
        project_id: i64,
        title: &str,
        description: &str,
        created_by: &str,
        external_author: Option<&str>,
    ) -> Result<i64, String> {
        let url = format!("{}/tasks", self.base);
        let body = build_task_body(project_id, title, description, created_by, external_author).to_string();
        let resp = self
            .agent
            .post(&url)
            .set("content-type", "application/json")
            .set("user-agent", BOARD_UA)
            .send_string(&body)
            .map_err(|e| format!("board POST /tasks failed: {e}"))?;
        let raw = resp.into_string().map_err(|e| format!("board POST /tasks read failed: {e}"))?;
        let v: Value =
            serde_json::from_str(&raw).map_err(|e| format!("board POST /tasks: response was not JSON: {e}"))?;
        v.get("id")
            .and_then(Value::as_i64)
            .ok_or_else(|| format!("board POST /tasks: no numeric id in response {v}"))
    }

    /// Add an attributed comment to a board task (`POST /tasks/:id/comments`): `author` = the bridge agent,
    /// `external_author` = the GitHub user (`github:<login>`).
    pub fn comment_task(
        &self,
        task_id: i64,
        author: &str,
        body: &str,
        external_author: Option<&str>,
    ) -> Result<(), String> {
        let url = format!("{}/tasks/{}/comments", self.base, task_id);
        let payload = build_comment_body(author, body, external_author).to_string();
        self.post_json(&url, &payload, "POST /tasks/:id/comments")
    }

    /// Upsert (idempotent on `id`) an external identity's display name (board-core #149; live independent of
    /// the #85 rendering redeploy). The inbound path calls this to attach a resolved GitHub display name to
    /// the stable `github:<login>` key, so board readers see `external_author_name` instead of a bare id.
    /// Best-effort at the call site (fail-soft — a failure just leaves the name absent, readers fall back).
    pub fn upsert_external_identity(&self, id: &str, source: &str, display_name: &str) -> Result<(), String> {
        let url = format!("{}/external-identities", self.base);
        let body = build_identity_body(id, source, display_name).to_string();
        self.post_json(&url, &body, "POST /external-identities")
    }

    /// POST a JSON body, mapping any transport error to a labeled `Err`. Shared by the write methods.
    fn post_json(&self, url: &str, body: &str, label: &str) -> Result<(), String> {
        self.agent
            .post(url)
            .set("content-type", "application/json")
            .set("user-agent", BOARD_UA)
            .send_string(body)
            .map_err(|e| format!("board {label} failed: {e}"))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── firehose parsing ─────────────────────────────────────────────────────────────────────────

    #[test]
    fn parse_events_accepts_a_bare_array() {
        let body = r#"[
            {"seq": 1, "type": "task.commented", "actor": "concierge", "task_id": 7, "data": {}},
            {"seq": 2, "type": "task.outbound_reflect", "task_id": 7,
             "data": {"task_id": 7, "comment_id": 42, "author": "concierge", "body": "hi",
                      "source": "github", "external_id": "camshaft/fleet#3"}}
        ]"#;
        let evs = parse_events(body).unwrap();
        assert_eq!(evs.len(), 2);
        assert_eq!(evs[0].seq, 1);
        assert_eq!(evs[0].task_id, Some(7));
        assert_eq!(evs[1].kind, TASK_OUTBOUND_REFLECT);
    }

    #[test]
    fn as_task_reflect_decodes_the_payload() {
        let body = r#"[{"seq": 5, "type": "task.outbound_reflect", "task_id": 7,
            "data": {"task_id": 7, "comment_id": 42, "author": "concierge", "body": "ship it",
                     "external_author": "github:octocat", "source": "github",
                     "external_id": "camshaft/fleet#3", "external_parent_id": null}}]"#;
        let r = parse_events(body).unwrap()[0].as_task_reflect().expect("decodes");
        assert_eq!(r.task_id, 7);
        assert_eq!(r.comment_id, 42);
        assert_eq!(r.author, "concierge");
        assert_eq!(r.body, "ship it");
        assert_eq!(r.external_author.as_deref(), Some("github:octocat"));
        assert_eq!(r.source, "github");
        assert_eq!(r.external_id, "camshaft/fleet#3");
        assert_eq!(r.external_parent_id, None);
    }

    #[test]
    fn as_task_reflect_none_for_other_types_and_bad_payload() {
        // Wrong type → None.
        let other = r#"[{"seq": 1, "type": "task.commented", "data": {"body": "x"}}]"#;
        assert!(parse_events(other).unwrap()[0].as_task_reflect().is_none());
        // Right type, missing required fields → None, never a panic.
        let bad = r#"[{"seq": 1, "type": "task.outbound_reflect", "data": {"body": "x"}}]"#;
        assert!(parse_events(bad).unwrap()[0].as_task_reflect().is_none());
    }

    #[test]
    fn parse_issue_ref_round_trips_issue_ref() {
        assert_eq!(parse_issue_ref("camshaft/fleet#42"), Some(("camshaft/fleet".to_string(), 42)));
        assert_eq!(parse_issue_ref(&issue_ref("o/r", 7)), Some(("o/r".to_string(), 7)));
    }

    #[test]
    fn parse_issue_ref_rejects_non_issue_shapes() {
        assert!(parse_issue_ref("camshaft/fleet#c555").is_none(), "a comment ref is not an issue ref");
        assert!(parse_issue_ref("no-hash").is_none());
        assert!(parse_issue_ref("#5").is_none(), "empty repo");
        assert!(parse_issue_ref("o/r#").is_none(), "no number");
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
                        "task_id": 3, "data": {"k": 1}, "future_field": "ignored"}]"#;
        let evs = parse_events(body).unwrap();
        assert_eq!(evs[0].seq, 9);
        assert_eq!(evs[0].created_at.as_deref(), Some("2026-09-29T00:00:00Z"));
    }

    #[test]
    fn parse_events_empty_is_ok() {
        assert!(parse_events("[]").unwrap().is_empty());
    }

    // ── issue↔task links (board-core #149 slice 2 / #151) ─────────────────────────────────────────

    #[test]
    fn issue_ref_is_owner_repo_hash_number() {
        assert_eq!(issue_ref("camshaft/fleet", 42), "camshaft/fleet#42");
    }

    #[test]
    fn comment_ref_is_owner_repo_hash_c_id() {
        assert_eq!(comment_ref("camshaft/fleet", 555), "camshaft/fleet#c555");
        // Distinct from an issue ref so the two link kinds never collide on external_id.
        assert_ne!(comment_ref("o/r", 5), issue_ref("o/r", 5));
    }

    #[test]
    fn parse_issue_links_bare_array() {
        let body = r#"[
            {"source": "github", "external_id": "camshaft/fleet#7", "board_kind": "task", "board_id": 7},
            {"source": "github", "external_id": "camshaft/fleet#8", "board_kind": "task", "board_id": 8,
             "metadata": {"note": "ignored"}}
        ]"#;
        let links = parse_issue_links(body).unwrap();
        assert_eq!(links.len(), 2);
        assert_eq!(links[0].board_task_id, 7);
        assert_eq!(links[0].issue_ref, "camshaft/fleet#7");
        assert_eq!(links[1].board_task_id, 8);
    }

    #[test]
    fn parse_issue_links_envelope_forms() {
        let a = parse_issue_links(
            r#"{"external_links": [{"source":"github","external_id":"o/r#1","board_kind":"task","board_id":1}]}"#,
        )
        .unwrap();
        assert_eq!(a.len(), 1);
        let b = parse_issue_links(
            r#"{"links": [{"source":"github","external_id":"o/r#2","board_kind":"task","board_id":2}]}"#,
        )
        .unwrap();
        assert_eq!(b[0].board_task_id, 2);
    }

    #[test]
    fn parse_issue_links_skips_non_github_and_non_task_rows() {
        // A Slack channel link and a github non-task row must be filtered out — only github/task rows map.
        let body = r#"[
            {"source": "github", "external_id": "o/r#7", "board_kind": "task",    "board_id": 7},
            {"source": "github", "external_id": "o/r#9", "board_kind": "channel", "board_id": 9},
            {"source": "slack",  "external_id": "C7",    "board_kind": "channel", "board_id": 7}
        ]"#;
        let links = parse_issue_links(body).unwrap();
        assert_eq!(links.len(), 1, "only the github/task row survives");
        assert_eq!(links[0].issue_ref, "o/r#7");
    }

    #[test]
    fn parse_issue_links_rejects_non_array() {
        assert!(parse_issue_links(r#"{"nope": 1}"#).is_err());
        assert!(parse_issue_links("not json").is_err());
    }

    #[test]
    fn parse_comment_refs_collects_only_github_comment_rows() {
        let body = r#"[
            {"source": "github", "external_id": "o/r#c1", "board_kind": "comment", "board_id": 7},
            {"source": "github", "external_id": "o/r#c2", "board_kind": "comment", "board_id": 7},
            {"source": "github", "external_id": "o/r#5",  "board_kind": "task",    "board_id": 7},
            {"source": "slack",  "external_id": "x",      "board_kind": "comment", "board_id": 7}
        ]"#;
        let refs = parse_comment_refs(body).unwrap();
        assert_eq!(refs.len(), 2, "only github/comment rows");
        assert!(refs.contains("o/r#c1") && refs.contains("o/r#c2"));
        assert!(!refs.contains("o/r#5"), "task row excluded");
    }

    #[test]
    fn parse_comment_refs_empty_and_error() {
        assert!(parse_comment_refs("[]").unwrap().is_empty());
        assert!(parse_comment_refs("not json").is_err());
    }

    #[test]
    fn parse_issue_links_empty_is_ok() {
        assert!(parse_issue_links("[]").unwrap().is_empty());
    }

    // ── pure body builders ────────────────────────────────────────────────────────────────────────

    #[test]
    fn build_task_body_shape_and_attribution() {
        let v = build_task_body(16, "Fix the thing", "as reported on GitHub", "github-bridge", Some("github:octocat"));
        assert_eq!(v["project_id"], 16);
        assert_eq!(v["title"], "Fix the thing");
        assert_eq!(v["description"], "as reported on GitHub");
        assert_eq!(v["created_by"], "github-bridge");
        assert_eq!(v["external_author"], "github:octocat");
    }

    #[test]
    fn build_task_body_omits_external_author_when_absent() {
        let v = build_task_body(1, "t", "d", "github-bridge", None);
        assert!(v.get("external_author").is_none(), "no explicit null");
    }

    #[test]
    fn build_comment_body_shape_and_attribution() {
        let v = build_comment_body("github-bridge", "a reply", Some("github:hubot"));
        assert_eq!(v["author"], "github-bridge");
        assert_eq!(v["body"], "a reply");
        assert_eq!(v["external_author"], "github:hubot");
    }

    #[test]
    fn build_comment_body_omits_external_author_when_absent() {
        let v = build_comment_body("github-bridge", "internal note", None);
        assert!(v.get("external_author").is_none(), "no explicit null");
    }

    #[test]
    fn build_identity_body_shape() {
        let v = build_identity_body("github:octocat", "github", "The Octocat");
        assert_eq!(v["id"], "github:octocat");
        assert_eq!(v["source"], "github");
        assert_eq!(v["display_name"], "The Octocat");
    }

    #[test]
    fn client_new_trims_trailing_slash() {
        let c = BoardClient::new("http://x/board/api/");
        assert_eq!(c.base, "http://x/board/api");
    }
}
