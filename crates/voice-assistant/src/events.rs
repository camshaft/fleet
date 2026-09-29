//! `events` — reactive board events. The assistant registers on the task board with a webhook URL and,
//! as the creator of the tasks it files, is auto-subscribed to them, so the board PUSHES status changes
//! (no polling). The webhook receiver only ENQUEUES events; the single audio/brain loop drains the
//! queue at the next idle moment (so there's no mic/brain concurrency). This is what lets the assistant
//! queue work ("let me get back to you") and proactively answer when it's ready.
//!
//! Split for testability: [`is_proactive`] and [`proactive_title`] are pure event classification (unit
//! tested here); [`WebhookReceiver`] wraps the `tiny_http` server + an `mpsc` channel; [`register`]
//! POSTs the board MCP `register_agent` call. The pure classifier is the important, tested seam — the
//! HTTP pieces are thin.

use std::sync::mpsc::{Receiver, Sender};

use serde_json::Value;

/// A board event the loop should react to (a task the assistant queued just finished).
#[derive(Debug, Clone)]
pub struct ProactiveEvent {
    /// The finished task's id (for logging).
    pub task_id: Option<i64>,
    /// The finished task's title (spoken into the proactive prompt), or a generic fallback.
    pub title: String,
}

/// True if a raw webhook event should wake the loop: a task the assistant is subscribed to reaching
/// `done`. Pure — the whole reaction policy lives here, mirroring the Python `_Handler.do_POST` check.
pub fn is_proactive(event: &Value) -> bool {
    event.get("type").and_then(Value::as_str) == Some("task.status_changed")
        && event
            .get("data")
            .and_then(|d| d.get("to"))
            .and_then(Value::as_str)
            == Some("done")
}

/// The title to speak for a proactive event, falling back to a generic phrase (ported from the Python
/// `_handle_proactive` title default).
pub fn proactive_title(event: &Value) -> String {
    event
        .get("data")
        .and_then(|d| d.get("title"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .unwrap_or("the work")
        .to_string()
}

/// Parse a raw webhook body into a [`ProactiveEvent`], or `None` if it isn't an actionable event. Pure.
pub fn parse_event(body: &[u8]) -> Option<ProactiveEvent> {
    let event: Value = serde_json::from_slice(body).ok()?;
    if !is_proactive(&event) {
        return None;
    }
    Some(ProactiveEvent {
        task_id: event.get("task_id").and_then(Value::as_i64),
        title: proactive_title(&event),
    })
}

/// The webhook receiver: a `tiny_http` server on its own thread that ACKs every POST and pushes any
/// actionable event onto the channel the loop drains. Best-effort — if the port can't bind, the
/// assistant still works as a plain voice loop (the `Option` return mirrors the Python graceful skip).
pub struct WebhookReceiver {
    pub events: Receiver<ProactiveEvent>,
}

impl WebhookReceiver {
    /// Start the receiver bound to `host:port`. Returns `None` (and logs) if the bind fails.
    pub fn start(host: &str, port: u16) -> Option<Self> {
        let (tx, rx): (Sender<ProactiveEvent>, Receiver<ProactiveEvent>) =
            std::sync::mpsc::channel();
        let server = match tiny_http::Server::http((host, port)) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("[events] webhook server not started: {e}");
                return None;
            }
        };
        std::thread::spawn(move || {
            for mut req in server.incoming_requests() {
                let mut body = Vec::new();
                let _ = req.as_reader().read_to_end(&mut body);
                // ACK immediately (the board treats a non-2xx as a delivery failure).
                let _ = req.respond(tiny_http::Response::from_string("ok"));
                if let Some(ev) = parse_event(&body) {
                    let _ = tx.send(ev);
                }
            }
        });
        Some(Self { events: rx })
    }

    /// Non-blocking drain of all queued events (called at the loop's idle moments).
    pub fn drain(&self) -> Vec<ProactiveEvent> {
        self.events.try_iter().collect()
    }
}

/// Register the assistant on the task board with a webhook URL, via the board MCP `register_agent`
/// tool over HTTP JSON-RPC. Best-effort: an unreachable board is logged, not fatal (the Python
/// `board(...)` swallows errors the same way).
pub fn register(mcp_url: &str, agent: &str, display_name: &str, webhook_url: &str) {
    let payload = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": {
            "name": "register_agent",
            "arguments": {
                "agent_id": agent,
                "display_name": display_name,
                "kind": "assistant",
                "webhook_url": webhook_url,
            }
        }
    });
    let res = ureq::post(mcp_url)
        .set("content-type", "application/json")
        .set("accept", "application/json, text/event-stream")
        .send_string(&payload.to_string());
    match res {
        Ok(_) => eprintln!("[events] registered {agent} on the board (webhook {webhook_url})"),
        Err(e) => eprintln!("[events] board register_agent failed (non-fatal): {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn done_status_change_is_proactive() {
        let ev = json!({"type": "task.status_changed", "data": {"to": "done", "title": "ingest"}});
        assert!(is_proactive(&ev));
        assert_eq!(proactive_title(&ev), "ingest");
    }

    #[test]
    fn non_done_and_other_types_are_ignored() {
        assert!(!is_proactive(
            &json!({"type": "task.status_changed", "data": {"to": "in_progress"}})
        ));
        assert!(!is_proactive(
            &json!({"type": "task.assigned", "data": {"to": "done"}})
        ));
        assert!(!is_proactive(&json!({"type": "comment.created"})));
    }

    #[test]
    fn title_falls_back_when_absent_or_empty() {
        assert_eq!(proactive_title(&json!({"data": {}})), "the work");
        assert_eq!(proactive_title(&json!({"data": {"title": ""}})), "the work");
    }

    #[test]
    fn parse_event_extracts_id_and_title() {
        let body =
            br#"{"type":"task.status_changed","task_id":42,"data":{"to":"done","title":"docs"}}"#;
        let ev = parse_event(body).expect("actionable");
        assert_eq!(ev.task_id, Some(42));
        assert_eq!(ev.title, "docs");
    }

    #[test]
    fn parse_event_rejects_garbage_and_non_actionable() {
        assert!(parse_event(b"not json").is_none());
        assert!(parse_event(br#"{"type":"comment.created"}"#).is_none());
    }
}
