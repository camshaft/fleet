//! `notify` — the fleet's event-driven wake injector (the P2 notification-wake, co-designed with
//! v-task-board; see ../../DESIGN.md).
//!
//! The board push-fires a best-effort HTTP POST to each agent's registered `webhook_url` for every inbox
//! event, carrying `recipient`, `type`, `task_id`, `event_seq`, … (never polling). Division of labor: the
//! board emits events; the FLEET owns the wake, because the wake target is a tmux window only the host with
//! tmux access can reach. So this is ONE long-running endpoint, registered as each board-backed agent's
//! `webhook_url`; on each POST it maps the event to a wake prompt and `tmux send-keys` injects it into the
//! recipient's window — `[notification] task #<task_id>` for a task assignment, `[notification] message
//! #<event_seq>` for a direct message — so the agent reacts to the event instead of polling.

use std::process::Command;

use serde_json::Value;

/// Map a board webhook event to the wake prompt to inject, or `None` to ignore the event. An assignment, a
/// direct message, or a COMMENT on a task the agent subscribes to wakes it (routing the loop to the task /
/// message); presence churn and the agent's own actions do not. Comments are safe to wake on because the
/// board only delivers `task.commented` to a task's subscribers/assignee/creator (minus the actor), so the
/// subscription IS the filter — this satisfies #145 ("notify a subscribed agent when its task is commented on
/// + drive the loop wake"). Pure — unit-tested.
pub fn notification_prompt(event_type: &str, task_id: Option<i64>, event_seq: Option<i64>) -> Option<String> {
    match event_type {
        "task.assigned" => task_id.map(|id| format!("[notification] task #{id}")),
        // A comment on a subscribed task → wake and point the loop at the task (where the new comment is).
        "task.commented" => task_id.map(|id| format!("[notification] task #{id}")),
        "message.direct" => event_seq.map(|seq| format!("[notification] message #{seq}")),
        _ => None,
    }
}

/// Extract `(recipient, wake-prompt)` from a webhook payload, or `None` if the event is not actionable or
/// is missing the recipient/type. Pure — unit-tested.
pub fn payload_to_wake(v: &Value) -> Option<(String, String)> {
    let recipient = v.get("recipient").and_then(Value::as_str)?;
    let event_type = v.get("type").and_then(Value::as_str)?;
    let task_id = v.get("task_id").and_then(Value::as_i64);
    let event_seq = v.get("event_seq").and_then(Value::as_i64);
    let prompt = notification_prompt(event_type, task_id, event_seq)?;
    Some((recipient.to_string(), prompt))
}

/// Inject `text` as a submitted prompt into tmux window `session:window`. Sends the text literally (`-l`,
/// so no character is read as a key binding) then a separate `Enter` to submit — the same wake path the
/// file-hub nudge uses. `Err` if the window is absent or tmux is unreachable.
pub fn tmux_inject(session: &str, window: &str, text: &str) -> Result<(), String> {
    let target = format!("{session}:{window}");
    let sent = Command::new("tmux")
        .args(["send-keys", "-t", &target, "-l", text])
        .status()
        .map_err(|e| format!("tmux send-keys -t {target}: {e}"))?;
    if !sent.success() {
        return Err(format!("tmux send-keys -l to {target} failed (window absent?)"));
    }
    Command::new("tmux")
        .args(["send-keys", "-t", &target, "Enter"])
        .status()
        .map_err(|e| format!("tmux send-keys Enter -t {target}: {e}"))?;
    Ok(())
}

/// Run the notifier: bind a local HTTP endpoint and, for each board webhook POST, inject the wake prompt
/// into the recipient agent's tmux window in `session`. Blocks (a long-running daemon). Best-effort: every
/// request is answered `200` immediately, and a payload that is unparseable or not actionable is logged and
/// dropped (a wake is never worth wedging the endpoint the board POSTs to).
pub fn serve(port: u16, session: &str) -> Result<(), String> {
    let server = tiny_http::Server::http(("127.0.0.1", port))
        .map_err(|e| format!("fleet notify: bind 127.0.0.1:{port}: {e}"))?;
    eprintln!("fleet notify: listening on http://127.0.0.1:{port} — waking session '{session}' on board webhooks");
    for mut req in server.incoming_requests() {
        let mut body = String::new();
        let _ = req.as_reader().read_to_string(&mut body);
        let _ = req.respond(tiny_http::Response::from_string("ok")); // ack the best-effort POST first
        match serde_json::from_str::<Value>(&body) {
            // presence/comment/other events yield no prompt and are silently dropped
            Ok(v) => {
                if let Some((recipient, prompt)) = payload_to_wake(&v) {
                    match tmux_inject(session, &recipient, &prompt) {
                        Ok(()) => eprintln!("woke {recipient}: {prompt}"),
                        Err(e) => eprintln!("inject failed for {recipient}: {e}"),
                    }
                }
            }
            Err(e) => eprintln!("fleet notify: dropping unparseable webhook body: {e}"),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_wakes_on_assignment_comment_and_dm() {
        assert_eq!(notification_prompt("task.assigned", Some(42), None).as_deref(), Some("[notification] task #42"));
        // a comment on a subscribed task wakes and points the loop at the task (#145)
        assert_eq!(notification_prompt("task.commented", Some(42), Some(9)).as_deref(), Some("[notification] task #42"));
        assert_eq!(notification_prompt("message.direct", None, Some(438)).as_deref(), Some("[notification] message #438"));
        // an assignment/comment without a task_id, or a DM without a seq, can't form a prompt
        assert_eq!(notification_prompt("task.assigned", None, Some(1)), None);
        assert_eq!(notification_prompt("task.commented", None, Some(2)), None);
        assert_eq!(notification_prompt("message.direct", Some(1), None), None);
        // presence churn and other non-actionable event types are ignored
        assert_eq!(notification_prompt("presence.updated", None, Some(3)), None);
        assert_eq!(notification_prompt("task.updated", Some(5), None), None);
    }

    #[test]
    fn payload_to_wake_pulls_recipient_and_prompt_or_none() {
        let assign = serde_json::json!({"recipient":"v-bolero","type":"task.assigned","task_id":7,"event_seq":100});
        assert_eq!(payload_to_wake(&assign), Some(("v-bolero".into(), "[notification] task #7".into())));
        let dm = serde_json::json!({"recipient":"v-capmeshd","type":"message.direct","channel_id":1,"event_seq":438});
        assert_eq!(payload_to_wake(&dm), Some(("v-capmeshd".into(), "[notification] message #438".into())));
        // missing recipient / non-actionable type / missing ids -> None
        assert_eq!(payload_to_wake(&serde_json::json!({"type":"task.assigned","task_id":7})), None);
        assert_eq!(payload_to_wake(&serde_json::json!({"recipient":"x","type":"presence.updated"})), None);
    }
}
