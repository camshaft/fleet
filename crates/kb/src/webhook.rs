//! `webhook` — the inbound event receiver for the board-driven workers (the reactive half of #235).
//!
//! Each deployed worker registers a `webhook_url` (see `board::Board::register`) on its reserved port
//! (uploader 8075 / embedder 8074 / crate-docs 8078); the board POSTs task events there. This module runs a
//! small axum server that parses each event, decides whether it is actionable for this worker, dedups so the
//! same task is not dispatched twice concurrently (the Python `_busy` lock), and hands the task id to the
//! worker loop over a channel.
//!
//! The event-classification + dedup core is pure and unit-tested; only the axum wiring needs a runtime. If
//! webhook delivery ever proves unreliable, the same `parse_event`/`actionable_task` core can be driven from
//! an SSE consume of `GET /events` instead (the fleet's other reactive substrate) without changing callers.

// Ported ahead of its callers (the phase-2 workers), so the surface reads as dead code until they land.
#![allow(dead_code)]

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::post;
use serde_json::Value;
use tokio::sync::mpsc::Sender;

/// A board event as delivered to the webhook (the same shape as an inbox notification): a type
/// discriminator, the task it concerns, and the event `data` payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebhookEvent {
    pub event_type: String,
    pub task_id: Option<i64>,
    pub data: Value,
}

/// Parse a board webhook POST body into an event. The board posts a single event object with `type`, a
/// top-level `task_id`, and a `data` object (mirroring the notification shape).
pub fn parse_event(body: &str) -> Result<WebhookEvent, String> {
    let v: Value =
        serde_json::from_str(body).map_err(|e| format!("webhook: body was not JSON: {e}"))?;
    let event_type = v
        .get("type")
        .and_then(Value::as_str)
        .ok_or_else(|| format!("webhook: event has no `type`: {v}"))?
        .to_string();
    let task_id = v.get("task_id").and_then(Value::as_i64);
    let data = v.get("data").cloned().unwrap_or(Value::Null);
    Ok(WebhookEvent {
        event_type,
        task_id,
        data,
    })
}

/// Decide whether an event should trigger this worker, returning the task id to process. A worker acts on a
/// task newly assigned to it (`task.assigned`) or created already assigned to it (`task.created`); the
/// assignee lives in the event `data`. Other event types (comments, unrelated assignments) are ignored here
/// — the worker re-reads the task with `board::Board::get_task` before doing real work.
pub fn actionable_task(event: &WebhookEvent, agent_id: &str) -> Option<i64> {
    match event.event_type.as_str() {
        "task.assigned" | "task.created" => {
            let assignee = event.data.get("assignee").and_then(Value::as_str);
            if assignee == Some(agent_id) {
                event.task_id
            } else {
                None
            }
        }
        _ => None,
    }
}

/// Tracks the tasks currently being processed so a duplicate event for an in-flight task is dropped rather
/// than dispatched twice (the Python `_busy` set + lock). A [`BusyGuard`] releases the claim on drop, so the
/// worker just holds the guard for the lifetime of its processing.
pub struct BusySet {
    inner: Mutex<HashSet<i64>>,
}

impl BusySet {
    pub fn new() -> Arc<BusySet> {
        Arc::new(BusySet {
            inner: Mutex::new(HashSet::new()),
        })
    }

    /// Claim a task if it isn't already in flight. Returns a guard that releases the claim on drop, or `None`
    /// if the task is already being processed.
    pub fn claim(self: &Arc<Self>, id: i64) -> Option<BusyGuard> {
        let mut set = self.inner.lock().unwrap();
        if set.insert(id) {
            Some(BusyGuard {
                set: Arc::clone(self),
                id,
            })
        } else {
            None
        }
    }

    /// Whether a task is currently claimed (for tests / diagnostics).
    fn is_busy(&self, id: i64) -> bool {
        self.inner.lock().unwrap().contains(&id)
    }
}

/// Releases its task's busy-claim when dropped. Held by the worker for the duration of processing.
pub struct BusyGuard {
    set: Arc<BusySet>,
    id: i64,
}

impl Drop for BusyGuard {
    fn drop(&mut self) {
        self.set.inner.lock().unwrap().remove(&self.id);
    }
}

/// Shared state for the receiver's request handler.
struct AppState {
    agent_id: String,
    busy: Arc<BusySet>,
    tx: Sender<(i64, BusyGuard)>,
}

/// Run the webhook receiver on `port`, delivering actionable, deduped task ids (each paired with a busy
/// guard the worker drops when done) to `tx`. Binds `0.0.0.0:port` — the board reaches a deployed
/// worker on the host's own address. Returns when the process is signalled.
pub async fn run_receiver(
    port: u16,
    agent_id: String,
    busy: Arc<BusySet>,
    tx: Sender<(i64, BusyGuard)>,
) -> Result<(), String> {
    let state = Arc::new(AppState { agent_id, busy, tx });
    let router = Router::new().route("/", post(handle)).with_state(state);
    let addr = format!("0.0.0.0:{port}");
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .map_err(|e| format!("webhook bind {addr} failed: {e}"))?;
    tracing::info!("kb worker webhook listening on http://{addr}/");
    axum::serve(listener, router)
        .with_graceful_shutdown(async move {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await
        .map_err(|e| format!("webhook server error: {e}"))
}

/// The POST handler: parse, classify, dedup, enqueue. Always answers 200 — a webhook delivery is fire-and-
/// forget from the board's side, and a malformed or irrelevant event is simply ignored (logged at debug).
async fn handle(State(st): State<Arc<AppState>>, body: String) -> StatusCode {
    match parse_event(&body) {
        Ok(event) => {
            if let Some(id) = actionable_task(&event, &st.agent_id)
                && let Some(guard) = st.busy.claim(id)
            {
                // Channel full/closed means the worker is gone or backed up; drop the guard (releasing the
                // claim) so a later redelivery can retry.
                if st.tx.send((id, guard)).await.is_err() {
                    tracing::warn!("webhook: worker channel closed; dropping task {id}");
                }
            }
        }
        Err(e) => tracing::debug!("webhook: ignoring unparseable event: {e}"),
    }
    StatusCode::OK
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_event_reads_type_task_id_and_data() {
        let e = parse_event(
            r#"{"type":"task.assigned","task_id":42,"data":{"assignee":"kb-embedder"}}"#,
        )
        .unwrap();
        assert_eq!(e.event_type, "task.assigned");
        assert_eq!(e.task_id, Some(42));
        assert_eq!(e.data["assignee"], "kb-embedder");
    }

    #[test]
    fn parse_event_missing_type_is_error_missing_data_is_null() {
        assert!(parse_event(r#"{"task_id":1}"#).is_err());
        assert!(parse_event("not json").is_err());
        let e = parse_event(r#"{"type":"task.created","task_id":1}"#).unwrap();
        assert_eq!(e.data, Value::Null);
    }

    #[test]
    fn actionable_only_for_my_assigned_or_created_tasks() {
        let mine_assigned = WebhookEvent {
            event_type: "task.assigned".into(),
            task_id: Some(7),
            data: json!({"assignee": "kb-embedder"}),
        };
        assert_eq!(actionable_task(&mine_assigned, "kb-embedder"), Some(7));

        let mine_created = WebhookEvent {
            event_type: "task.created".into(),
            task_id: Some(8),
            data: json!({"assignee": "kb-embedder"}),
        };
        assert_eq!(actionable_task(&mine_created, "kb-embedder"), Some(8));

        // Assigned to someone else.
        let other = WebhookEvent {
            event_type: "task.assigned".into(),
            task_id: Some(9),
            data: json!({"assignee": "someone-else"}),
        };
        assert_eq!(actionable_task(&other, "kb-embedder"), None);

        // An unrelated event type (a comment) is not actionable even if it names me.
        let comment = WebhookEvent {
            event_type: "task.commented".into(),
            task_id: Some(10),
            data: json!({"assignee": "kb-embedder"}),
        };
        assert_eq!(actionable_task(&comment, "kb-embedder"), None);
    }

    #[test]
    fn busy_set_dedups_and_guard_releases_on_drop() {
        let busy = BusySet::new();
        let g1 = busy.claim(5);
        assert!(g1.is_some());
        assert!(busy.is_busy(5));
        // A second claim while in flight is refused.
        assert!(busy.claim(5).is_none());
        // A different task can be claimed concurrently.
        let g2 = busy.claim(6);
        assert!(g2.is_some());
        // Dropping the first guard releases task 5, so it can be claimed again.
        drop(g1);
        assert!(!busy.is_busy(5));
        assert!(busy.claim(5).is_some());
    }
}
