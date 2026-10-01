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
//! ([`parse_events`], [`build_task_body`], [`build_comment_body`], [`build_identity_body`]) that are
//! unit-tested without a network.

use reqwest::Client;
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
/// #149 slice 2 / #270). Distinct from the Slack adapter's `"slack"` so the two adapters' links never
/// collide. Sent as `external_link.source` on the idempotent create/comment calls; the board keys dedup on
/// `(source, external_id)` and assigns the `board_kind` (task vs comment) itself.
pub const LINK_SOURCE: &str = "github";

/// The external-link `source` for a GitHub PULL REQUEST mirrored as a board code review (BUILD 2, Review
/// entity #372). Distinct from [`LINK_SOURCE`] (`"github"`, issue↔task) so a PR's review link and its
/// conversation-comment log entries share one namespace that never collides with the issue-ingest links —
/// even though a PR comment's `comment_ref` string is shaped like an issue comment's. Sent as
/// `external_link.source` on `create_review` / `append_review_log` (both idempotent on `(source, external_id)`).
pub const REVIEW_LINK_SOURCE: &str = "github_pr";

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

/// The canonical external id for a PR inline diff-review comment (a *finding*): `owner/repo#rc<id>` (BUILD
/// 2b-2). The `rc` prefix keeps it in a separate id space from a conversation [`comment_ref`] (`#c<id>`) so
/// a review finding and a conversation comment never collide as review-log entries even if their GitHub ids
/// coincide.
pub fn review_comment_ref(repo: &str, comment_id: i64) -> String {
    format!("{repo}#rc{comment_id}")
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
    let v: Value = serde_json::from_str(body)
        .map_err(|e| format!("board /events: response was not JSON: {e}"))?;
    let arr = match v {
        Value::Array(a) => a,
        Value::Object(ref o) => match o.get("events") {
            Some(Value::Array(a)) => a.clone(),
            _ => {
                return Err(format!(
                    "board /events: object without an `events` array: {v}"
                ));
            }
        },
        other => {
            return Err(format!(
                "board /events: expected an array or {{events:[…]}}, got {other}"
            ));
        }
    };
    arr.into_iter()
        .map(|e| {
            serde_json::from_value::<Event>(e)
                .map_err(|err| format!("board /events: bad event: {err}"))
        })
        .collect()
}

/// One board project from `GET /projects`. Only the fields the bridge's repo->project mapping needs are
/// modeled; `metadata` stays a raw [`Value`] (the board carries arbitrary keys there, e.g. `kind`,
/// `author_identity`). Unknown top-level keys are ignored (forward compatible — the board adds project fields
/// the bridge doesn't care about).
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Project {
    /// The numeric project id — the `project_id` an ingested repo's mirrored tasks/reviews are created in.
    pub id: i64,
    /// The project's display name (e.g. `fleet`); not used for matching, kept for logging.
    #[serde(default)]
    pub name: String,
    /// Lifecycle status (`active` / `archived`). The mapping ingests ONLY `active` projects, so an archived
    /// project that still carries a `metadata.repo` (e.g. a consolidated-away project) can't shadow the live
    /// one for the same repo.
    #[serde(default)]
    pub status: String,
    /// Arbitrary board-assigned metadata; the bridge reads `metadata.repo` (the mapped GitHub repo).
    #[serde(default)]
    pub metadata: Value,
}

impl Project {
    /// The GitHub repo this project maps to, from `metadata.repo` — a full URL
    /// (`https://github.com/<owner>/<name>`) or a bare `owner/name`. `None` when the project carries no repo
    /// (an internal / pipeline project) or an empty one. The raw string; normalizing to `owner/name` for
    /// matching is the mapper's job (a later slice).
    pub fn repo(&self) -> Option<&str> {
        self.metadata
            .get("repo")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
    }

    /// Whether this project is `active` (the mapping ingests active projects only).
    pub fn is_active(&self) -> bool {
        self.status == "active"
    }
}

/// Parse the JSON body of `GET /projects` into the project list. Like [`parse_events`], accept either a bare
/// array or a `{ "projects": [...] }` envelope. Returns the parse error text on a body that is neither.
pub fn parse_projects(body: &str) -> Result<Vec<Project>, String> {
    let v: Value = serde_json::from_str(body)
        .map_err(|e| format!("board /projects: response was not JSON: {e}"))?;
    let arr = match v {
        Value::Array(a) => a,
        Value::Object(ref o) => match o.get("projects") {
            Some(Value::Array(a)) => a.clone(),
            _ => {
                return Err(format!(
                    "board /projects: object without a `projects` array: {v}"
                ));
            }
        },
        other => {
            return Err(format!(
                "board /projects: expected an array or {{projects:[…]}}, got {other}"
            ));
        }
    };
    arr.into_iter()
        .map(|p| {
            serde_json::from_value::<Project>(p)
                .map_err(|err| format!("board /projects: bad project: {err}"))
        })
        .collect()
}

/// Build the JSON body for creating a mirrored board task from an ingested GitHub issue (`POST /tasks`).
/// `project_id` selects the board project; `created_by` is the bridge's own agent id; `external_author`
/// attributes the originating GitHub user (e.g. `github:octocat`); `external_id` is the issue ref
/// (`owner/repo#number`) that makes the create IDEMPOTENT (board-core #270): the board atomically
/// creates-or-returns-existing keyed on `(source, external_id)` and reports `created` in the response, so the
/// adapter never double-creates on a retry (no separate link-register call). Pure — unit-tested. Omits the
/// optional `external_author` when absent so the board applies its own default.
pub fn build_task_body(
    project_id: i64,
    title: &str,
    description: &str,
    created_by: &str,
    external_author: Option<&str>,
    external_id: &str,
) -> Value {
    let mut m = json!({
        "project_id": project_id,
        "title": title,
        "description": description,
        "created_by": created_by,
        "external_link": { "source": LINK_SOURCE, "external_id": external_id },
    });
    if let Some(ea) = external_author {
        m["external_author"] = json!(ea);
    }
    m
}

/// Build the JSON body for an attributed task comment (`POST /tasks/:id/comments`). `author` is the bridge's
/// own board agent id; `external_author` attributes the originating GitHub user; `external_id` is the comment
/// ref (`owner/repo#c<id>`) that makes the comment IDEMPOTENT (board-core #270, dedup keyed on
/// `(source, external_id)`). Pure — unit-tested. Omits the optional `external_author` when absent.
pub fn build_comment_body(
    author: &str,
    body: &str,
    external_author: Option<&str>,
    external_id: &str,
) -> Value {
    let mut m = json!({
        "author": author,
        "body": body,
        "external_link": { "source": LINK_SOURCE, "external_id": external_id },
    });
    if let Some(ea) = external_author {
        m["external_author"] = json!(ea);
    }
    m
}

/// Build the JSON body for creating a mirrored board code review from an ingested GitHub pull request
/// (`POST /reviews`, Review entity #372). Mirrors [`build_task_body`] but for the review entity: `kind` is
/// the review kind (`"code"`), `status` the initial review status (`open` / `approved` / `closed`), and the
/// `external_link` uses [`REVIEW_LINK_SOURCE`] so the create is IDEMPOTENT on `(source, external_id)` (the PR
/// ref `owner/repo#number`) — the board creates-or-returns-existing and reports `created`. Pure — unit-tested.
/// Omits the optional `external_author` when absent so the board applies its own default.
#[allow(clippy::too_many_arguments)]
pub fn build_review_body(
    project_id: i64,
    kind: &str,
    title: &str,
    description: &str,
    created_by: &str,
    external_author: Option<&str>,
    status: &str,
    external_id: &str,
) -> Value {
    let mut m = json!({
        "project_id": project_id,
        "kind": kind,
        "title": title,
        "description": description,
        "created_by": created_by,
        "status": status,
        "external_link": { "source": REVIEW_LINK_SOURCE, "external_id": external_id },
    });
    if let Some(ea) = external_author {
        m["external_author"] = json!(ea);
    }
    m
}

/// Build the JSON body for appending an entry to a review's log (`POST /reviews/:id/log`, Review entity
/// #372). `log_type` is the entry type (`"comment"` for a mirrored PR conversation comment); `external_id`
/// (the comment ref `owner/repo#c<id>`) makes the append IDEMPOTENT on `(source, external_id)` under
/// [`REVIEW_LINK_SOURCE`]. Pure — unit-tested. Omits the optional `external_author` when absent.
pub fn build_review_log_body(
    log_type: &str,
    body: &str,
    external_author: Option<&str>,
    external_id: &str,
) -> Value {
    let mut m = json!({
        "type": log_type,
        "body": body,
        "external_link": { "source": REVIEW_LINK_SOURCE, "external_id": external_id },
    });
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

/// A handle to the board's token-less localhost REST API (stateless — each call is one request). ASYNC over
/// reqwest (operator directive: NO blocking IO — the caller provides the tokio runtime; constructing the
/// client needs no runtime, only sending does). The firehose cursor (`since_seq`) is owned by the caller (the
/// poll/stream loop), not this client.
pub struct BoardClient {
    base: String,
    http: Client,
}

impl BoardClient {
    /// Build a client against the board REST base (e.g. `http://127.0.0.1:8079/api`). No network round-trip —
    /// the REST API is sessionless and the reqwest `Client` is constructed without a runtime. A trailing slash
    /// on `base_api` is trimmed so path joins don't double up.
    pub fn new(base_api: &str) -> Self {
        BoardClient {
            base: base_api.trim_end_matches('/').to_string(),
            http: Client::new(),
        }
    }

    /// GET a URL and return the raw body text, mapping any transport/status error to a labeled `Err`.
    async fn get_text(&self, url: &str, label: &str) -> Result<String, String> {
        self.http
            .get(url)
            .header("accept", "application/json")
            .header("user-agent", BOARD_UA)
            .send()
            .await
            .and_then(reqwest::Response::error_for_status)
            .map_err(|e| format!("board {label} failed: {e}"))?
            .text()
            .await
            .map_err(|e| format!("board {label} read failed: {e}"))
    }

    /// POST a JSON body and return the raw response text, mapping any transport/status error to a labeled
    /// `Err`. Shared by every write method.
    async fn post_text(&self, url: &str, body: String, label: &str) -> Result<String, String> {
        self.http
            .post(url)
            .header("content-type", "application/json")
            .header("user-agent", BOARD_UA)
            .body(body)
            .send()
            .await
            .and_then(reqwest::Response::error_for_status)
            .map_err(|e| format!("board {label} failed: {e}"))?
            .text()
            .await
            .map_err(|e| format!("board {label} read failed: {e}"))
    }

    /// Poll the firehose for events after `since_seq` (exclusive), up to `limit`. Returns them in ascending
    /// `seq` order; an empty vec when nothing is newer.
    pub async fn poll_events(&self, since_seq: i64, limit: usize) -> Result<Vec<Event>, String> {
        let url = format!(
            "{}/events?since_seq={}&limit={}",
            self.base, since_seq, limit
        );
        let raw = self.get_text(&url, "GET /events").await?;
        parse_events(&raw)
    }

    /// List the board's projects (`GET /projects`) — the source of the dynamic repo->project mapping
    /// (each project's `metadata.repo`). Meant to be read each IN pass so a newly-created or newly-mapped
    /// project is picked up WITHOUT a daemon restart (the mapping is nothing hardcoded).
    pub async fn list_projects(&self) -> Result<Vec<Project>, String> {
        let url = format!("{}/projects", self.base);
        let raw = self.get_text(&url, "GET /projects").await?;
        parse_projects(&raw)
    }

    /// Create a mirrored board task from an ingested issue (`POST /tasks`), IDEMPOTENT on the issue link
    /// (board-core #270): passing `external_id` (the issue ref) makes the board create-or-return-existing in
    /// one transaction. Returns `(task_id, created)` — `created == false` means the issue was already ingested
    /// and the returned id is the existing task, so the caller short-circuits with no duplicate and no
    /// separate link-register call.
    pub async fn create_task(
        &self,
        project_id: i64,
        title: &str,
        description: &str,
        created_by: &str,
        external_author: Option<&str>,
        external_id: &str,
    ) -> Result<(i64, bool), String> {
        let url = format!("{}/tasks", self.base);
        let body = build_task_body(
            project_id,
            title,
            description,
            created_by,
            external_author,
            external_id,
        )
        .to_string();
        let raw = self.post_text(&url, body, "POST /tasks").await?;
        let v: Value = serde_json::from_str(&raw)
            .map_err(|e| format!("board POST /tasks: response was not JSON: {e}"))?;
        let id = v
            .get("id")
            .and_then(Value::as_i64)
            .ok_or_else(|| format!("board POST /tasks: no numeric id in response {v}"))?;
        // `created` is #270-only; default true if a pre-#270 board omits it (then link-idempotency is off,
        // but the daemon documents at-least-once for that case).
        let created = v.get("created").and_then(Value::as_bool).unwrap_or(true);
        Ok((id, created))
    }

    /// Add an attributed comment to a board task (`POST /tasks/:id/comments`), IDEMPOTENT on the comment link
    /// (board-core #270): `external_id` (the comment ref) dedups server-side. `author` = the bridge agent,
    /// `external_author` = the GitHub user (`github:<login>`). Returns `created` (false = already synced).
    pub async fn comment_task(
        &self,
        task_id: i64,
        author: &str,
        body: &str,
        external_author: Option<&str>,
        external_id: &str,
    ) -> Result<bool, String> {
        let url = format!("{}/tasks/{}/comments", self.base, task_id);
        let payload = build_comment_body(author, body, external_author, external_id).to_string();
        let raw = self
            .post_text(&url, payload, &format!("POST /tasks/{task_id}/comments"))
            .await?;
        let v: Value = serde_json::from_str(&raw).map_err(|e| {
            format!("board POST /tasks/{task_id}/comments: response was not JSON: {e}")
        })?;
        Ok(v.get("created").and_then(Value::as_bool).unwrap_or(true))
    }

    /// Create a mirrored board code review from an ingested pull request (`POST /reviews`, Review entity
    /// #372), IDEMPOTENT on the PR link: passing `external_id` (the PR ref) makes the board
    /// create-or-return-existing in one transaction. Returns `(review_id, created)` — `created == false` means
    /// the PR was already mirrored and the returned id is the existing review (the caller then advances its
    /// status via [`set_review_status`](Self::set_review_status)).
    #[allow(clippy::too_many_arguments)]
    pub async fn create_review(
        &self,
        project_id: i64,
        kind: &str,
        title: &str,
        description: &str,
        created_by: &str,
        external_author: Option<&str>,
        status: &str,
        external_id: &str,
    ) -> Result<(i64, bool), String> {
        let url = format!("{}/reviews", self.base);
        let body = build_review_body(
            project_id,
            kind,
            title,
            description,
            created_by,
            external_author,
            status,
            external_id,
        )
        .to_string();
        let raw = self.post_text(&url, body, "POST /reviews").await?;
        let v: Value = serde_json::from_str(&raw)
            .map_err(|e| format!("board POST /reviews: response was not JSON: {e}"))?;
        let id = v
            .get("id")
            .and_then(Value::as_i64)
            .ok_or_else(|| format!("board POST /reviews: no numeric id in response {v}"))?;
        let created = v.get("created").and_then(Value::as_bool).unwrap_or(true);
        Ok((id, created))
    }

    /// Advance a review's status (`POST /reviews/:id/status`, Review entity #372). Idempotent server-side —
    /// re-applying the same status is a no-op — so the poll loop can call it every time a PR is re-seen to
    /// advance `open` → `approved`/`closed` without tracking prior state itself.
    pub async fn set_review_status(&self, review_id: i64, status: &str) -> Result<(), String> {
        let url = format!("{}/reviews/{}/status", self.base, review_id);
        let body = json!({ "status": status }).to_string();
        self.post_text(&url, body, &format!("POST /reviews/{review_id}/status"))
            .await?;
        Ok(())
    }

    /// Append an entry to a review's log (`POST /reviews/:id/log`, Review entity #372), IDEMPOTENT on the
    /// entry link (`external_id` = the comment ref): a re-append of the same comment is a no-op. Used by IN
    /// to mirror a PR's conversation comments as `comment`-type log entries. Returns `appended` (false =
    /// already logged).
    pub async fn append_review_log(
        &self,
        review_id: i64,
        log_type: &str,
        body: &str,
        external_author: Option<&str>,
        external_id: &str,
    ) -> Result<bool, String> {
        let url = format!("{}/reviews/{}/log", self.base, review_id);
        let payload =
            build_review_log_body(log_type, body, external_author, external_id).to_string();
        let raw = self
            .post_text(&url, payload, &format!("POST /reviews/{review_id}/log"))
            .await?;
        let v: Value = serde_json::from_str(&raw).map_err(|e| {
            format!("board POST /reviews/{review_id}/log: response was not JSON: {e}")
        })?;
        Ok(v.get("appended").and_then(Value::as_bool).unwrap_or(true))
    }

    /// Upsert (idempotent on `id`) an external identity's display name (board-core #149; live independent of
    /// the #85 rendering redeploy). The inbound path calls this to attach a resolved GitHub display name to
    /// the stable `github:<login>` key, so board readers see `external_author_name` instead of a bare id.
    /// Best-effort at the call site (fail-soft — a failure just leaves the name absent, readers fall back).
    pub async fn upsert_external_identity(
        &self,
        id: &str,
        source: &str,
        display_name: &str,
    ) -> Result<(), String> {
        let url = format!("{}/external-identities", self.base);
        let body = build_identity_body(id, source, display_name).to_string();
        self.post_text(&url, body, "POST /external-identities")
            .await?;
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
        let r = parse_events(body).unwrap()[0]
            .as_task_reflect()
            .expect("decodes");
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
        assert_eq!(
            parse_issue_ref("camshaft/fleet#42"),
            Some(("camshaft/fleet".to_string(), 42))
        );
        assert_eq!(
            parse_issue_ref(&issue_ref("o/r", 7)),
            Some(("o/r".to_string(), 7))
        );
    }

    #[test]
    fn parse_issue_ref_rejects_non_issue_shapes() {
        assert!(
            parse_issue_ref("camshaft/fleet#c555").is_none(),
            "a comment ref is not an issue ref"
        );
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

    // ── projects (repo->project mapping source, GET /projects) ────────────────────────────────────

    #[test]
    fn parse_projects_accepts_bare_array_and_reads_repo_and_status() {
        let body = r#"[
            {"id": 21, "name": "fleet", "status": "active",
             "metadata": {"repo": "https://github.com/camshaft/fleet"}},
            {"id": 29, "name": "uncategorized", "status": "active", "metadata": {"kind": "pipeline"}}
        ]"#;
        let ps = parse_projects(body).unwrap();
        assert_eq!(ps.len(), 2);
        assert_eq!(ps[0].id, 21);
        assert_eq!(ps[0].name, "fleet");
        assert_eq!(ps[0].repo(), Some("https://github.com/camshaft/fleet"));
        assert!(ps[0].is_active());
        assert_eq!(ps[1].repo(), None, "a pipeline project carries no repo");
    }

    #[test]
    fn parse_projects_accepts_envelope_and_tolerates_unknown_keys() {
        let body = r#"{"projects": [
            {"id": 7, "name": "etude", "status": "archived",
             "metadata": {"repo": "https://github.com/camshaft/etude"},
             "task_counts": {"done": 16}, "ref": "project_7"}
        ]}"#;
        let ps = parse_projects(body).unwrap();
        assert_eq!(ps.len(), 1);
        assert_eq!(ps[0].id, 7);
        assert!(
            !ps[0].is_active(),
            "archived is not active — can't shadow the live mapping"
        );
        assert_eq!(ps[0].repo(), Some("https://github.com/camshaft/etude"));
    }

    #[test]
    fn project_repo_is_none_when_metadata_absent_or_empty() {
        let body = r#"[
            {"id": 1, "name": "a", "status": "active", "metadata": {}},
            {"id": 2, "name": "b", "status": "active", "metadata": {"repo": ""}},
            {"id": 3, "name": "c", "status": "active"}
        ]"#;
        let ps = parse_projects(body).unwrap();
        assert_eq!(ps[0].repo(), None, "empty metadata object → no repo");
        assert_eq!(ps[1].repo(), None, "empty repo string is not a repo");
        assert_eq!(
            ps[2].repo(),
            None,
            "absent metadata defaults to null → no repo"
        );
    }

    #[test]
    fn parse_projects_rejects_non_array_json() {
        assert!(parse_projects(r#"{"nope": 1}"#).is_err());
        assert!(parse_projects("not json at all").is_err());
    }

    #[test]
    fn parse_projects_empty_is_ok() {
        assert!(parse_projects("[]").unwrap().is_empty());
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
    fn review_comment_ref_is_distinct_from_conversation_and_issue_refs() {
        assert_eq!(
            review_comment_ref("camshaft/fleet", 900),
            "camshaft/fleet#rc900"
        );
        // A finding and a conversation comment with the same numeric id must not collide.
        assert_ne!(review_comment_ref("o/r", 5), comment_ref("o/r", 5));
        assert_ne!(review_comment_ref("o/r", 5), issue_ref("o/r", 5));
    }

    // ── pure body builders (idempotent create/comment, board-core #270) ─────────────────────────────

    #[test]
    fn build_task_body_shape_attribution_and_external_link() {
        let v = build_task_body(
            16,
            "Fix the thing",
            "as reported on GitHub",
            "github-bridge",
            Some("github:octocat"),
            "camshaft/fleet#42",
        );
        assert_eq!(v["project_id"], 16);
        assert_eq!(v["title"], "Fix the thing");
        assert_eq!(v["description"], "as reported on GitHub");
        assert_eq!(v["created_by"], "github-bridge");
        assert_eq!(v["external_author"], "github:octocat");
        assert_eq!(v["external_link"]["source"], "github");
        assert_eq!(v["external_link"]["external_id"], "camshaft/fleet#42");
    }

    #[test]
    fn build_task_body_omits_external_author_but_always_links() {
        let v = build_task_body(1, "t", "d", "github-bridge", None, "o/r#1");
        assert!(v.get("external_author").is_none(), "no explicit null");
        assert_eq!(
            v["external_link"]["external_id"], "o/r#1",
            "link always present for idempotency"
        );
    }

    #[test]
    fn build_comment_body_shape_attribution_and_external_link() {
        let v = build_comment_body("github-bridge", "a reply", Some("github:hubot"), "o/r#c555");
        assert_eq!(v["author"], "github-bridge");
        assert_eq!(v["body"], "a reply");
        assert_eq!(v["external_author"], "github:hubot");
        assert_eq!(v["external_link"]["source"], "github");
        assert_eq!(v["external_link"]["external_id"], "o/r#c555");
    }

    #[test]
    fn build_comment_body_omits_external_author_but_always_links() {
        let v = build_comment_body("github-bridge", "internal note", None, "o/r#c1");
        assert!(v.get("external_author").is_none(), "no explicit null");
        assert_eq!(v["external_link"]["external_id"], "o/r#c1");
    }

    // ── review body builders (create_review / append_review_log, Review entity #372) ────────────────

    #[test]
    fn build_review_body_shape_attribution_and_github_pr_link() {
        let v = build_review_body(
            16,
            "code",
            "Add gizmo",
            "the PR body",
            "github-bridge",
            Some("github:octocat"),
            "approved",
            "camshaft/fleet#88",
        );
        assert_eq!(v["project_id"], 16);
        assert_eq!(v["kind"], "code");
        assert_eq!(v["title"], "Add gizmo");
        assert_eq!(v["description"], "the PR body");
        assert_eq!(v["created_by"], "github-bridge");
        assert_eq!(v["status"], "approved");
        assert_eq!(v["external_author"], "github:octocat");
        assert_eq!(
            v["external_link"]["source"], "github_pr",
            "PR reviews use the github_pr link namespace"
        );
        assert_eq!(v["external_link"]["external_id"], "camshaft/fleet#88");
    }

    #[test]
    fn build_review_body_omits_external_author_but_always_links() {
        let v = build_review_body(1, "code", "t", "d", "github-bridge", None, "open", "o/r#1");
        assert!(v.get("external_author").is_none(), "no explicit null");
        assert_eq!(
            v["external_link"]["external_id"], "o/r#1",
            "link always present for idempotency"
        );
    }

    #[test]
    fn build_review_log_body_shape_and_github_pr_link() {
        let v = build_review_log_body(
            "comment",
            "a review remark",
            Some("github:hubot"),
            "o/r#c777",
        );
        assert_eq!(v["type"], "comment");
        assert_eq!(v["body"], "a review remark");
        assert_eq!(v["external_author"], "github:hubot");
        assert_eq!(v["external_link"]["source"], "github_pr");
        assert_eq!(v["external_link"]["external_id"], "o/r#c777");
    }

    #[test]
    fn build_review_log_body_omits_external_author_but_always_links() {
        let v = build_review_log_body("comment", "internal", None, "o/r#c1");
        assert!(v.get("external_author").is_none());
        assert_eq!(v["external_link"]["external_id"], "o/r#c1");
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
