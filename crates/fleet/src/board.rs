//! `board` — the ORCHESTRATOR's read-only view of the task board, over its plain REST API.
//!
//! Per the board-offload rearchitecture (../../DESIGN.md), the board holds the agent roster, charters, and
//! the message bus. AGENTS coordinate through their OWN in-session board MCP tools — the fleet does NOT
//! wrap those. The ONLY board access outside a Claude session is the `fleet` orchestrator itself
//! (spin-up / reconcile / watchdog), and only to READ the roster + a charter so it knows what to launch.
//!
//! Transport is the board's **REST API** (a plain `GET`/`PATCH` returning JSON), not MCP: for a
//! non-session process a REST call is far simpler than the MCP SSE handshake (initialize → session-id →
//! notifications/initialized → tools/call → parse `data:` frames). Base URL: `$FLEET_BOARD_API` (default
//! the local front-door proxy `…/board/api`).
//!
//! The client reads the roster (`list_agents` / `get_agent`) and writes ONE thing: an agent's metadata bag
//! (`patch_metadata`), the orchestrator's migration primitive for making an agent spin-up-ready (declaring
//! its `repos`, its loop `interval`). That write is an ORCHESTRATOR act, not an agent-facing wrapper — an
//! agent still coordinates through its own in-session board MCP; the fleet only sets the launch-shaping
//! metadata the board can't infer.

use serde_json::Value;

const DEFAULT_BASE: &str = "http://127.0.0.1:8880/board/api";

/// A browser-like User-Agent for every board call. The default base is the loopback proxy (no Cloudflare),
/// but if `config.board_api` points at the PUBLIC endpoint, the CF edge 403s a non-browser UA
/// ("browser_signature_banned", #209) — so send a browser-ish UA defensively; harmless on loopback.
const BOARD_UA: &str = "Mozilla/5.0 (X11; Linux x86_64) fleet-orchestrator";

/// The board query path for an OPEN observation task tagged `observes=<target>` in a project — the #290
/// idempotency check. Both `meta_key` and `meta_value` must be present for the board to filter on metadata
/// (either alone is inert). Agent ids are kebab-case with no URL-special characters, so no encoding is
/// needed (as with `open_task_count`'s assignee). Pure — unit-tested.
fn open_observation_query(project_id: i64, observes: &str) -> String {
    format!("/tasks?project_id={project_id}&status=todo&meta_key=observes&meta_value={observes}")
}

/// A handle to the board's REST API (stateless — each call is one `GET`).
pub struct Board {
    base: String,
    agent: ureq::Agent,
}

impl Board {
    /// The board REST base URL (`config.board_api`, else the local front-door proxy).
    pub fn base_url() -> String {
        crate::config::get()
            .board_api
            .clone()
            .unwrap_or_else(|| DEFAULT_BASE.to_string())
    }

    /// Build a client. No network round-trip — the REST API is sessionless, so there is no handshake.
    pub fn connect() -> Result<Board, String> {
        Ok(Board { base: Self::base_url(), agent: ureq::agent() })
    }

    fn get_json(&self, path: &str) -> Result<Value, String> {
        let url = format!("{}{}", self.base, path);
        let resp = self
            .agent
            .get(&url)
            .set("accept", "application/json")
            .set("user-agent", BOARD_UA)
            .call()
            .map_err(|e| format!("board GET {path} failed: {e}"))?;
        let raw = resp
            .into_string()
            .map_err(|e| format!("board GET {path} read failed: {e}"))?;
        serde_json::from_str(&raw).map_err(|e| format!("board GET {path}: response was not JSON: {e}"))
    }

    /// The full agent roster (each record: id/charter/display_name/kind/status/metadata/…).
    pub fn list_agents(&self) -> Result<Vec<Value>, String> {
        match self.get_json("/agents")? {
            Value::Array(a) => Ok(a),
            other => Err(format!("board /agents: expected an array, got {other}")),
        }
    }

    /// One agent's full board record (charter + metadata), or an error if absent (a 404 GET).
    pub fn get_agent(&self, agent: &str) -> Result<Value, String> {
        self.get_json(&format!("/agents/{agent}"))
    }

    /// Count an agent's OPEN (non-terminal) assigned tasks — the `/tasks?assignee=<id>` list minus anything
    /// `done`/`cancelled`. The watchdog uses this to spot an agent sitting on assigned work while idling on a
    /// long loop interval. (Agent ids are kebab-case with no URL-special chars, so no query-encoding needed.)
    pub fn open_task_count(&self, assignee: &str) -> Result<usize, String> {
        let tasks = match self.get_json(&format!("/tasks?assignee={assignee}"))? {
            Value::Array(a) => a,
            other => return Err(format!("board /tasks: expected an array, got {other}")),
        };
        Ok(tasks
            .iter()
            .filter(|t| {
                let s = t.get("status").and_then(Value::as_str).unwrap_or("");
                s != "done" && s != "cancelled"
            })
            .count())
    }

    /// Fetch a custom workspace-kind resource (`GET /api/workspace-kinds/{kind}`) → `Some(record)`, or `None`
    /// when the kind is not defined (a 404). `spin-up` consumes this for an agent whose `metadata.workspace_kind`
    /// names a board-defined environment: the record's `setup_script` materializes the workspace and its
    /// free-form `config` object carries the launch hints (cwd/pre_trust/env) the consumer reads.
    pub fn get_workspace_kind(&self, kind: &str) -> Result<Option<Value>, String> {
        let url = format!("{}/workspace-kinds/{}", self.base, kind);
        match self
            .agent
            .get(&url)
            .set("accept", "application/json")
            .set("user-agent", BOARD_UA)
            .call()
        {
            Ok(resp) => {
                let raw = resp
                    .into_string()
                    .map_err(|e| format!("board GET /workspace-kinds/{kind} read failed: {e}"))?;
                serde_json::from_str(&raw)
                    .map(Some)
                    .map_err(|e| format!("board GET /workspace-kinds/{kind}: response was not JSON: {e}"))
            }
            Err(ureq::Error::Status(404, _)) => Ok(None),
            Err(e) => Err(format!("board GET /workspace-kinds/{kind} failed: {e}")),
        }
    }

    /// Create a task via `POST /api/tasks` → its numeric `id`. `metadata` is a free-form JSON object (stamp
    /// an idempotency tag here, e.g. `{"observes": "<target>"}`); `parent_id` links it as a child of another
    /// task (the observation-task → proposal-children tree, #290). The watchdog creates the observation task
    /// this way; `Err` on a non-2xx response.
    pub fn create_task(
        &self,
        project_id: i64,
        title: &str,
        description: &str,
        created_by: &str,
        metadata: Value,
        parent_id: Option<i64>,
    ) -> Result<i64, String> {
        let url = format!("{}/tasks", self.base);
        let mut body = serde_json::json!({
            "project_id": project_id,
            "title": title,
            "description": description,
            "created_by": created_by,
            "metadata": metadata,
        });
        if let Some(p) = parent_id {
            body["parent_id"] = serde_json::json!(p);
        }
        let resp = self
            .agent
            .post(&url)
            .set("content-type", "application/json")
            .set("user-agent", BOARD_UA)
            .send_string(&body.to_string())
            .map_err(|e| format!("board POST /tasks failed: {e}"))?;
        let raw = resp
            .into_string()
            .map_err(|e| format!("board POST /tasks read failed: {e}"))?;
        let v: Value = serde_json::from_str(&raw)
            .map_err(|e| format!("board POST /tasks: response was not JSON: {e}"))?;
        v.get("id")
            .and_then(Value::as_i64)
            .ok_or_else(|| format!("board POST /tasks: no numeric id in response {v}"))
    }

    /// The #290 idempotency check: the numeric id of an OPEN observation task tagged `observes=<target>` in
    /// `project_id`, or `None` if none is open. A non-empty result means an observation for that target is
    /// already in flight (or a crashed observer left one open) → reuse it rather than creating a duplicate.
    /// Uses the board's server-side metadata filter (both `meta_key` and `meta_value` set together).
    pub fn open_observation_task(&self, project_id: i64, observes: &str) -> Result<Option<i64>, String> {
        let path = open_observation_query(project_id, observes);
        let tasks = match self.get_json(&path)? {
            Value::Array(a) => a,
            other => return Err(format!("board {path}: expected an array, got {other}")),
        };
        Ok(tasks.iter().find_map(|t| t.get("id").and_then(Value::as_i64)))
    }

    /// Merge `metadata` into an agent's board record via `PATCH /agents/<id>`. The board merges at the KEY
    /// level, so only the keys present in `metadata` change — every other metadata key is preserved. `Err`
    /// on a non-2xx response (e.g. an unknown agent).
    pub fn patch_metadata(&self, agent: &str, metadata: Value) -> Result<(), String> {
        let url = format!("{}/agents/{}", self.base, agent);
        let body = serde_json::json!({ "metadata": metadata }).to_string();
        self.agent
            .request("PATCH", &url)
            .set("content-type", "application/json")
            .set("user-agent", BOARD_UA)
            .send_string(&body)
            .map_err(|e| format!("board PATCH /agents/{agent} failed: {e}"))?;
        Ok(())
    }

    /// Set an agent's board `status` + `status_message` via `PATCH /agents/{id}` (the same endpoint
    /// `patch_metadata` uses; verified to accept a `status` field). Used by `fleet spin-down` to mark a
    /// board-native agent `offline` so `up-board` leaves it stood down (offline + no window → never
    /// auto-launched) while its record stays intact for a later `spin-up`. `Err` on a non-2xx response.
    pub fn set_status(&self, agent: &str, status: &str, status_message: &str) -> Result<(), String> {
        let url = format!("{}/agents/{}", self.base, agent);
        let body =
            serde_json::json!({ "status": status, "status_message": status_message }).to_string();
        self.agent
            .request("PATCH", &url)
            .set("content-type", "application/json")
            .set("user-agent", BOARD_UA)
            .send_string(&body)
            .map_err(|e| format!("board PATCH /agents/{agent} (status) failed: {e}"))?;
        Ok(())
    }

    /// Create-or-get a channel by name (`POST /channels`, idempotent — posting an existing name returns it),
    /// returning its numeric `id`. `created_by` attributes the creation. Used to resolve a channel name → id
    /// before posting (the board posts by id, not name).
    pub fn create_or_get_channel(&self, name: &str, created_by: &str) -> Result<i64, String> {
        let url = format!("{}/channels", self.base);
        let body = serde_json::json!({ "name": name, "created_by": created_by }).to_string();
        let resp = self
            .agent
            .post(&url)
            .set("content-type", "application/json")
            .set("user-agent", BOARD_UA)
            .send_string(&body)
            .map_err(|e| format!("board POST /channels ({name}) failed: {e}"))?;
        let raw = resp
            .into_string()
            .map_err(|e| format!("board POST /channels read failed: {e}"))?;
        let v: Value = serde_json::from_str(&raw)
            .map_err(|e| format!("board POST /channels: response was not JSON: {e}"))?;
        v.get("id")
            .and_then(Value::as_i64)
            .ok_or_else(|| format!("board POST /channels ({name}): no numeric id in response {v}"))
    }

    /// Post a message to a channel by numeric id (`POST /channels/{id}/posts`). `sender` is the authoring
    /// agent id. `Err` on a non-2xx response.
    pub fn post_to_channel(&self, channel_id: i64, sender: &str, body: &str) -> Result<(), String> {
        let url = format!("{}/channels/{}/posts", self.base, channel_id);
        let payload = serde_json::json!({ "sender": sender, "body": body }).to_string();
        self.agent
            .post(&url)
            .set("content-type", "application/json")
            .set("user-agent", BOARD_UA)
            .send_string(&payload)
            .map_err(|e| format!("board POST /channels/{channel_id}/posts failed: {e}"))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_url_defaults_to_the_local_front_door_proxy() {
        // The default is the front-door /board/api proxy, not the board's own unreachable port.
        assert!(DEFAULT_BASE.ends_with("/board/api"));
        assert!(DEFAULT_BASE.starts_with("http://"));
    }

    #[test]
    fn open_observation_query_sets_project_status_and_both_meta_params() {
        // The #290 idempotency query: project + open-status + the metadata tag (both meta params present).
        let q = open_observation_query(28, "v-example");
        assert_eq!(q, "/tasks?project_id=28&status=todo&meta_key=observes&meta_value=v-example");
        assert!(q.contains("meta_key=observes") && q.contains("meta_value=v-example"), "both meta params set");
    }

    #[test]
    fn connect_is_sessionless_and_uses_the_configured_base() {
        // SAFETY: no network — connect() only builds the handle (REST is stateless).
        let b = Board { base: "http://x/board/api".into(), agent: ureq::agent() };
        assert_eq!(b.base, "http://x/board/api");
    }
}
