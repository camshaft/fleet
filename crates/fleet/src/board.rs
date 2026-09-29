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

    /// Merge `metadata` into an agent's board record via `PATCH /agents/<id>`. The board merges at the KEY
    /// level, so only the keys present in `metadata` change — every other metadata key is preserved. `Err`
    /// on a non-2xx response (e.g. an unknown agent).
    pub fn patch_metadata(&self, agent: &str, metadata: Value) -> Result<(), String> {
        let url = format!("{}/agents/{}", self.base, agent);
        let body = serde_json::json!({ "metadata": metadata }).to_string();
        self.agent
            .request("PATCH", &url)
            .set("content-type", "application/json")
            .send_string(&body)
            .map_err(|e| format!("board PATCH /agents/{agent} failed: {e}"))?;
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
    fn connect_is_sessionless_and_uses_the_configured_base() {
        // SAFETY: no network — connect() only builds the handle (REST is stateless).
        let b = Board { base: "http://x/board/api".into(), agent: ureq::agent() };
        assert_eq!(b.base, "http://x/board/api");
    }
}
