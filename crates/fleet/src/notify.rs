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

/// Map a board webhook event to the wake prompt to inject, or `None` to ignore the event. Only ACTIONABLE
/// events inject a live-session loop-wake: a `task.assigned` (new work for the recipient) and a
/// `message.direct` (someone is asking). INFORMATIONAL events — `task.commented`, `task.status_changed`, and
/// the like — do NOT wake: the board still delivers them to the recipient's durable inbox, where they accrue
/// for the agent's next poll (poll is the primary channel). Presence churn and the agent's own actions never
/// wake either. Pure — unit-tested.
///
/// This gates on event TYPE (#215). The board delivers `task.commented` to a task's subscribers/assignee/
/// CREATOR (minus the actor), and a creator can't leave that fan-out — so waking on every comment meant a
/// stood-down/idle agent was loop-woken by pure FYI comments on tasks it merely opened (an observed drain of
/// opus ticks). Superseding the earlier #145 "wake a subscriber on any comment" behavior: a comment now
/// accrues for the next poll, and a genuinely actionable ask arrives as a `message.direct` (which still
/// wakes) or via the per-task mute opt-out (board `mute_task`, #90). A per-recipient "this comment is a
/// question/mention" wake would need a board-side actionability hint on the event — a DEFERRED enhancement,
/// not needed for the type-based gate.
pub fn notification_prompt(
    event_type: &str,
    task_id: Option<i64>,
    event_seq: Option<i64>,
    channel_id: Option<i64>,
) -> Option<String> {
    match event_type {
        "task.assigned" => task_id.map(|id| format!("[notification] task #{id}")),
        "message.direct" => event_seq.map(|seq| format!("[notification] message #{seq}")),
        // A post to a channel the agent SUBSCRIBED to is actionable: the board only delivers `channel.post`
        // to a channel's subscribers/members (minus the actor), so delivery IS the subscription filter — the
        // agent opted in because it cares (e.g. a `deploys`-channel waiter, #171). Same "opt-in = actionable"
        // principle as the #215 gating, so it wakes (unlike an un-opted-in task.commented FYI).
        "channel.post" => channel_id.map(|id| format!("[notification] channel #{id}")),
        // task.commented / task.status_changed / task.updated / presence.updated and every other type are
        // INFORMATIONAL — they accrue for the next poll and never inject a wake.
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
    let channel_id = v.get("channel_id").and_then(Value::as_i64);
    let prompt = notification_prompt(event_type, task_id, event_seq, channel_id)?;
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

/// How the notifier handles one incoming request. The board POSTs webhook events; a supervisor (a systemd
/// service health check, `fleet board-health`, a monitor) probes liveness with a plain `GET`. Classifying up
/// front lets a liveness probe get a clean `200` without being read as a webhook — which would log a spurious
/// "unparseable body" line and is indistinguishable from a real event.
#[derive(Debug, PartialEq, Eq)]
enum Incoming {
    /// A liveness probe (`GET /health`, `/healthz`, or `/`) — answer `200` and read nothing.
    HealthProbe,
    /// A board webhook event — read the body and map it to a wake (the default for any other request).
    Webhook,
}

/// Classify an incoming request by method + path (query string ignored): a `GET` to `/health`, `/healthz`, or
/// `/` is a liveness probe; every other request is a webhook. Pure — unit-tested.
fn classify_request(method: &tiny_http::Method, url: &str) -> Incoming {
    let path = url.split('?').next().unwrap_or(url);
    if *method == tiny_http::Method::Get && matches!(path, "/health" | "/healthz" | "/") {
        Incoming::HealthProbe
    } else {
        Incoming::Webhook
    }
}

/// Run the notifier: bind a local HTTP endpoint and, for each board webhook POST, inject the wake prompt
/// into the recipient agent's tmux window in `session`. Blocks (a long-running daemon). Best-effort: every
/// request is answered `200` immediately, and a payload that is unparseable or not actionable is logged and
/// dropped (a wake is never worth wedging the endpoint the board POSTs to). A supervisor liveness-probes the
/// daemon with a `GET` to `/health` (see [`classify_request`]), answered `200` without webhook parsing.
pub fn serve(port: u16, session: &str) -> Result<(), String> {
    let server = tiny_http::Server::http(("127.0.0.1", port))
        .map_err(|e| format!("fleet notify: bind 127.0.0.1:{port}: {e}"))?;
    eprintln!("fleet notify: listening on http://127.0.0.1:{port} — waking session '{session}' on board webhooks (GET /health for liveness)");
    for mut req in server.incoming_requests() {
        // A supervisor's liveness probe gets a clean 200 and is never read as a webhook.
        if classify_request(req.method(), req.url()) == Incoming::HealthProbe {
            let _ = req.respond(tiny_http::Response::from_string("ok"));
            continue;
        }
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
    fn classify_request_routes_get_health_paths_to_a_probe_else_webhook() {
        use tiny_http::Method;
        // A GET to a health path (query string ignored) is a liveness probe.
        assert_eq!(classify_request(&Method::Get, "/health"), Incoming::HealthProbe);
        assert_eq!(classify_request(&Method::Get, "/healthz"), Incoming::HealthProbe);
        assert_eq!(classify_request(&Method::Get, "/"), Incoming::HealthProbe);
        assert_eq!(classify_request(&Method::Get, "/health?probe=1"), Incoming::HealthProbe);
        // The board POSTs webhooks — never a probe, even to a health path.
        assert_eq!(classify_request(&Method::Post, "/"), Incoming::Webhook);
        assert_eq!(classify_request(&Method::Post, "/health"), Incoming::Webhook);
        // A non-health GET is treated as a webhook (the default), not a probe.
        assert_eq!(classify_request(&Method::Get, "/webhook"), Incoming::Webhook);
        assert_eq!(classify_request(&Method::Get, "/events"), Incoming::Webhook);
    }

    #[test]
    fn prompt_wakes_only_on_actionable_assignment_and_dm_not_informational() {
        // ACTIONABLE → wake: a new assignment, and a direct message.
        assert_eq!(notification_prompt("task.assigned", Some(42), None, None).as_deref(), Some("[notification] task #42"));
        assert_eq!(notification_prompt("message.direct", None, Some(438), None).as_deref(), Some("[notification] message #438"));
        // ACTIONABLE → wake: a post to a channel the agent subscribed to (delivery = subscription; #171).
        assert_eq!(notification_prompt("channel.post", None, Some(9), Some(7)).as_deref(), Some("[notification] channel #7"));
        assert_eq!(notification_prompt("channel.post", None, Some(9), None), None, "no channel_id → can't form a prompt");
        // INFORMATIONAL → NO wake (accrues for the next poll): a comment or a status change on a task the
        // agent merely created/subscribes to must not loop-wake a stood-down/idle session (#215).
        assert_eq!(notification_prompt("task.commented", Some(42), Some(9), None), None, "a comment accrues for poll, never wakes");
        assert_eq!(notification_prompt("task.status_changed", Some(42), Some(9), None), None);
        // an assignment without a task_id, or a DM without a seq, can't form a prompt
        assert_eq!(notification_prompt("task.assigned", None, Some(1), None), None);
        assert_eq!(notification_prompt("message.direct", Some(1), None, None), None);
        // presence churn and other non-actionable event types are ignored
        assert_eq!(notification_prompt("presence.updated", None, Some(3), None), None);
        assert_eq!(notification_prompt("task.updated", Some(5), None, None), None);
    }

    #[test]
    fn payload_to_wake_pulls_recipient_and_prompt_or_none() {
        let assign = serde_json::json!({"recipient":"v-bolero","type":"task.assigned","task_id":7,"event_seq":100});
        assert_eq!(payload_to_wake(&assign), Some(("v-bolero".into(), "[notification] task #7".into())));
        let dm = serde_json::json!({"recipient":"v-capmeshd","type":"message.direct","channel_id":1,"event_seq":438});
        assert_eq!(payload_to_wake(&dm), Some(("v-capmeshd".into(), "[notification] message #438".into())));
        // missing recipient / informational type / missing ids -> None (no wake)
        assert_eq!(payload_to_wake(&serde_json::json!({"type":"task.assigned","task_id":7})), None);
        assert_eq!(payload_to_wake(&serde_json::json!({"recipient":"x","type":"presence.updated"})), None);
        // an FYI comment delivered to a subscriber/creator does NOT wake (it accrues for poll) — #215
        assert_eq!(payload_to_wake(&serde_json::json!({"recipient":"x","type":"task.commented","task_id":7,"event_seq":9})), None);
        // a channel.post (tunnel payload carries recipient + channel_id) wakes the subscriber — #171
        let post = serde_json::json!({"recipient":"waiter","type":"channel.post","channel_id":7,"event_seq":51,"data":{"body":"deploy…","from":"deployer"}});
        assert_eq!(payload_to_wake(&post), Some(("waiter".into(), "[notification] channel #7".into())));
    }
}
