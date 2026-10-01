//! `task_client` — the shared client for a non-session Rust daemon's board TASK/AGENT surface (task_355):
//! register-or-upsert itself (optionally with a `webhook_url`), subscribe to a project, and read/write
//! tasks. This is the REST counterpart to the in-session board MCP tools, for a process that runs outside a
//! Claude session and so cannot use them.
//!
//! Complements [`crate::board`] ([`crate::board::BoardClient`]), which is the channel/firehose/identity
//! surface for a board↔external-channel bridge; this module is the task/agent surface any non-session
//! daemon needs regardless of whether it bridges a channel. Originally written for the `kb` ingest workers
//! (ported from Python); extracted here so a future daemon port (voice-assistant, github-bridge, …) gets
//! register/webhook + task CRUD for free instead of re-deriving the REST-vs-MCP distinction and the
//! register route from scratch (observed independently by two sibling ports in one session).
//!
//! HTTP is async `reqwest` — operator directive #439 (no blocking IO on the tokio runtime). All body
//! SHAPING is factored into pure `build_*` functions, unit-tested without a live board. Identity: the board
//! keys writes on an explicit caller id, so every write carries the caller's own agent id — `created_by` on
//! create, `actor` on update (so the caller isn't notified of its own change), `author` on a comment.

use serde::Deserialize;
use serde_json::{Value, json};

/// One board task — the fields a non-session daemon acts on. `metadata` is kept as a raw [`Value`] because
/// the board returns it as a JSON object on some routes and a JSON-encoded string on others; [`Task::props`]
/// normalizes both to an object.
#[derive(Debug, Clone, Deserialize)]
pub struct Task {
    pub id: i64,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub assignee: Option<String>,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub metadata: Value,
}

impl Task {
    /// The task's props as an object, accepting either the object form or the JSON-encoded-string form the
    /// board returns depending on the route. Returns an empty object when absent or unparseable.
    pub fn props(&self) -> serde_json::Map<String, Value> {
        match &self.metadata {
            Value::Object(m) => m.clone(),
            Value::String(s) => serde_json::from_str(s).unwrap_or_default(),
            _ => serde_json::Map::new(),
        }
    }
}

/// A handle to the board REST API's task/agent surface. Stateless — each call is one request. Carries the
/// caller's agent id so writes are attributed to it.
pub struct Board {
    base: String,
    agent_id: String,
    http: reqwest::Client,
}

impl Board {
    /// Build a client against an explicit REST base. The caller supplies its own configured board URL (a
    /// crate-specific `connect()` convenience, if wanted, belongs in the caller's own module).
    pub fn with_base(base: &str, agent_id: impl Into<String>) -> Board {
        Board {
            base: base.trim_end_matches('/').to_string(),
            agent_id: agent_id.into(),
            http: reqwest::Client::new(),
        }
    }

    /// Register (upsert) this worker, optionally with a `webhook_url` the board POSTs events to — `POST
    /// /agents`. Idempotent: creates the record if new, merges if it exists.
    pub async fn register(
        &self,
        webhook_url: Option<&str>,
        metadata: &Value,
    ) -> Result<(), String> {
        let url = format!("{}/agents", self.base);
        let body = build_register_body(&self.agent_id, webhook_url, metadata);
        self.http
            .post(&url)
            .json(&body)
            .send()
            .await
            .and_then(reqwest::Response::error_for_status)
            .map_err(|e| format!("board POST /agents failed: {e}"))?;
        Ok(())
    }

    /// Subscribe this worker to a whole project's task events — `POST /subscriptions`. A worker is already
    /// auto-subscribed to tasks it creates or is assigned, so this is for watching a queue project it does
    /// not own every task in.
    pub async fn subscribe_project(&self, project_id: i64) -> Result<(), String> {
        let url = format!("{}/subscriptions", self.base);
        let body = build_subscribe_body(&self.agent_id, project_id);
        self.http
            .post(&url)
            .json(&body)
            .send()
            .await
            .and_then(reqwest::Response::error_for_status)
            .map_err(|e| format!("board POST /subscriptions failed: {e}"))?;
        Ok(())
    }

    /// List tasks filtered by assignee and/or status — `GET /tasks`. The startup catch-up a worker runs
    /// before going event-reactive.
    pub async fn list_tasks(
        &self,
        assignee: Option<&str>,
        status: Option<&str>,
    ) -> Result<Vec<Task>, String> {
        let url = format!("{}/tasks", self.base);
        let mut query: Vec<(&str, &str)> = Vec::new();
        if let Some(a) = assignee {
            query.push(("assignee", a));
        }
        if let Some(s) = status {
            query.push(("status", s));
        }
        let text = self
            .http
            .get(&url)
            .query(&query)
            .send()
            .await
            .and_then(reqwest::Response::error_for_status)
            .map_err(|e| format!("board GET /tasks failed: {e}"))?
            .text()
            .await
            .map_err(|e| format!("board GET /tasks read failed: {e}"))?;
        parse_tasks(&text)
    }

    /// Fetch one task — `GET /tasks/{id}`.
    pub async fn get_task(&self, id: i64) -> Result<Task, String> {
        let url = format!("{}/tasks/{}", self.base, id);
        self.http
            .get(&url)
            .send()
            .await
            .and_then(reqwest::Response::error_for_status)
            .map_err(|e| format!("board GET /tasks/{id} failed: {e}"))?
            .json::<Task>()
            .await
            .map_err(|e| format!("board GET /tasks/{id} decode failed: {e}"))
    }

    /// Create a task in `project_id` — `POST /tasks`, attributed to this worker. Returns the created task.
    pub async fn create_task(
        &self,
        project_id: i64,
        title: &str,
        description: &str,
        metadata: &Value,
    ) -> Result<Task, String> {
        let url = format!("{}/tasks", self.base);
        let body = build_create_task_body(project_id, title, description, &self.agent_id, metadata);
        self.http
            .post(&url)
            .json(&body)
            .send()
            .await
            .and_then(reqwest::Response::error_for_status)
            .map_err(|e| format!("board POST /tasks failed: {e}"))?
            .json::<Task>()
            .await
            .map_err(|e| format!("board POST /tasks decode failed: {e}"))
    }

    /// Update a task's status and/or assignee — `PATCH /tasks/{id}`. `actor` is this worker so it isn't
    /// notified of its own change.
    pub async fn update_task(
        &self,
        id: i64,
        status: Option<&str>,
        assignee: Option<&str>,
    ) -> Result<(), String> {
        let url = format!("{}/tasks/{}", self.base, id);
        let body = build_update_task_body(status, assignee, &self.agent_id);
        self.http
            .patch(&url)
            .json(&body)
            .send()
            .await
            .and_then(reqwest::Response::error_for_status)
            .map_err(|e| format!("board PATCH /tasks/{id} failed: {e}"))?;
        Ok(())
    }

    /// Merge props into a task's metadata — `PATCH /tasks/{id}/props`. The body IS the props object.
    pub async fn set_task_props(&self, id: i64, props: &Value) -> Result<(), String> {
        let url = format!("{}/tasks/{}/props", self.base, id);
        self.http
            .patch(&url)
            .json(props)
            .send()
            .await
            .and_then(reqwest::Response::error_for_status)
            .map_err(|e| format!("board PATCH /tasks/{id}/props failed: {e}"))?;
        Ok(())
    }

    /// Add a comment to a task — `POST /tasks/{id}/comments`, authored by this worker.
    pub async fn comment_task(&self, id: i64, body: &str) -> Result<(), String> {
        let url = format!("{}/tasks/{}/comments", self.base, id);
        let payload = build_comment_body(body, &self.agent_id);
        self.http
            .post(&url)
            .json(&payload)
            .send()
            .await
            .and_then(reqwest::Response::error_for_status)
            .map_err(|e| format!("board POST /tasks/{id}/comments failed: {e}"))?;
        Ok(())
    }
}

/// `POST /agents` body (RegisterAgentBody). Omits `webhook_url` when absent so a re-register doesn't touch it.
pub fn build_register_body(agent_id: &str, webhook_url: Option<&str>, metadata: &Value) -> Value {
    let mut body = json!({ "agent_id": agent_id, "metadata": metadata });
    if let Some(w) = webhook_url {
        body["webhook_url"] = json!(w);
    }
    body
}

/// `POST /subscriptions` body (SubscribeBody): a subscriber + exactly one scope (here, a project).
fn build_subscribe_body(subscriber: &str, project_id: i64) -> Value {
    json!({ "subscriber": subscriber, "project_id": project_id })
}

/// `POST /tasks` body (CreateTaskBody), attributed via `created_by`.
fn build_create_task_body(
    project_id: i64,
    title: &str,
    description: &str,
    created_by: &str,
    metadata: &Value,
) -> Value {
    json!({
        "project_id": project_id,
        "title": title,
        "description": description,
        "created_by": created_by,
        "metadata": metadata,
    })
}

/// `PATCH /tasks/{id}` body (UpdateTaskBody). Omits an unchanged field entirely; `actor` suppresses the
/// self-notification.
fn build_update_task_body(status: Option<&str>, assignee: Option<&str>, actor: &str) -> Value {
    let mut body = json!({ "actor": actor });
    if let Some(s) = status {
        body["status"] = json!(s);
    }
    if let Some(a) = assignee {
        body["assignee"] = json!(a);
    }
    body
}

/// `POST /tasks/{id}/comments` body (CommentBody), attributed via `author`.
fn build_comment_body(body: &str, author: &str) -> Value {
    json!({ "body": body, "author": author })
}

/// Parse `GET /tasks` — the board returns either a bare array or a `{ "tasks": [...] }` envelope; accept
/// both (the same fail-explicit shape as [`crate::board::parse_events`]).
fn parse_tasks(body: &str) -> Result<Vec<Task>, String> {
    let v: Value = serde_json::from_str(body)
        .map_err(|e| format!("board /tasks: response was not JSON: {e}"))?;
    let arr = match v {
        Value::Array(a) => a,
        Value::Object(ref o) => match o.get("tasks") {
            Some(Value::Array(a)) => a.clone(),
            _ => return Err(format!("board /tasks: object without a `tasks` array: {v}")),
        },
        other => {
            return Err(format!(
                "board /tasks: expected an array or {{tasks:[…]}}, got {other}"
            ));
        }
    };
    arr.into_iter()
        .map(|t| {
            serde_json::from_value::<Task>(t).map_err(|e| format!("board /tasks: bad task: {e}"))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_body_omits_webhook_when_absent_and_includes_it_when_present() {
        let none = build_register_body("kb-uploader", None, &json!({"role": "worker"}));
        assert_eq!(none["agent_id"], "kb-uploader");
        assert_eq!(none["metadata"]["role"], "worker");
        assert!(none.get("webhook_url").is_none());

        let some = build_register_body(
            "kb-uploader",
            Some("http://127.0.0.1:8075/wake"),
            &json!({}),
        );
        assert_eq!(some["webhook_url"], "http://127.0.0.1:8075/wake");
    }

    #[test]
    fn update_body_omits_unchanged_fields_and_carries_actor() {
        let only_status = build_update_task_body(Some("done"), None, "kb-embedder");
        assert_eq!(only_status["status"], "done");
        assert_eq!(only_status["actor"], "kb-embedder");
        assert!(only_status.get("assignee").is_none());

        let reassign = build_update_task_body(None, Some("kb-embedder"), "kb-uploader");
        assert_eq!(reassign["assignee"], "kb-embedder");
        assert!(reassign.get("status").is_none());
    }

    #[test]
    fn create_and_comment_bodies_are_attributed() {
        let c = build_create_task_body(
            21,
            "ingest x",
            "desc",
            "kb-uploader",
            &json!({"source": "docs.rs"}),
        );
        assert_eq!(c["project_id"], 21);
        assert_eq!(c["created_by"], "kb-uploader");
        assert_eq!(c["metadata"]["source"], "docs.rs");

        let cm = build_comment_body("done", "kb-embedder");
        assert_eq!(cm["body"], "done");
        assert_eq!(cm["author"], "kb-embedder");
    }

    #[test]
    fn subscribe_body_scopes_to_project() {
        let s = build_subscribe_body("kb-embedder", 21);
        assert_eq!(s["subscriber"], "kb-embedder");
        assert_eq!(s["project_id"], 21);
    }

    #[test]
    fn parse_tasks_accepts_array_and_envelope() {
        let arr = parse_tasks(r#"[{"id":1,"status":"todo","title":"a"}]"#).unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0].id, 1);
        assert_eq!(arr[0].status, "todo");

        let env = parse_tasks(r#"{"tasks":[{"id":2,"title":"b"}]}"#).unwrap();
        assert_eq!(env[0].id, 2);

        assert!(parse_tasks(r#"{"nope":1}"#).is_err());
        assert!(parse_tasks("not json").is_err());
    }

    #[test]
    fn task_props_normalizes_object_and_string_forms() {
        let obj: Task = serde_json::from_str(r#"{"id":1,"metadata":{"ipfs_cid":"bafy"}}"#).unwrap();
        assert_eq!(obj.props().get("ipfs_cid").unwrap(), "bafy");

        let strf: Task =
            serde_json::from_str(r#"{"id":1,"metadata":"{\"ipfs_cid\":\"bafy\"}"}"#).unwrap();
        assert_eq!(strf.props().get("ipfs_cid").unwrap(), "bafy");

        let empty: Task = serde_json::from_str(r#"{"id":1}"#).unwrap();
        assert!(empty.props().is_empty());
    }
}
