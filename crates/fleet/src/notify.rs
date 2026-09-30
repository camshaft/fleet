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

/// Map a board webhook event to the wake prompt to inject, or `None` to ignore the event. The wake model is
/// SUBSCRIPTION = NOTIFICATION (operator directive, #386): if an agent is subscribed to a target it is woken
/// on that target's activity — comments included — and unsubscribe is the opt-out. So an event wakes the
/// recipient's live session when either (a) it is a direct-delivery type they cannot be a passive bystander
/// of — a `task.assigned` (new work), a `message.direct` (addressed to them), a `channel.post` (a channel
/// they are a member of) — or (b) it is a `task.commented` on a target the recipient has a DIRECT
/// subscription to (`subscribed == true`). A firehose-only recipient (present via a whole-board subscription,
/// not a direct one) is NOT woken on a comment — it accrues in the durable inbox for the next poll, so a
/// board-wide coordinator isn't woken on every ticket. Presence churn and the agent's own actions never wake.
/// Pure — unit-tested.
///
/// `subscribed` is the board's per-recipient hint (#384, superseding the earlier `actionable` field): `true`
/// when the recipient has a DIRECT subscription to this event's target, `false` when they are present only
/// via the whole-board firehose. It is what lets `task.commented` wake a collaborator — two agents conversing
/// on a task must not wait out each other's poll interval (the operator's zero-polling mandate) — while a
/// firehose bystander still drops to poll. This deliberately retires the #215 anti-FYI-drain gate (the
/// operator accepts the noise trade, with unsubscribe + auto-subscribe as the noise control). `task.assigned`
/// / `message.direct` / `channel.post` stay wake-on-type: their delivery already IS the subscription (a DM in
/// particular has no subscribable target), and keeping them type-gated also preserves their wake for a
/// pre-#384 payload that carries no `subscribed` field yet.
pub fn notification_prompt(
    event_type: &str,
    task_id: Option<i64>,
    event_seq: Option<i64>,
    channel_id: Option<i64>,
    subscribed: bool,
) -> Option<String> {
    match event_type {
        "task.assigned" => task_id.map(|id| format!("[notification] task #{id}")),
        "message.direct" => event_seq.map(|seq| format!("[notification] message #{seq}")),
        // A post to a channel the agent is a member of: the board only delivers `channel.post` to a channel's
        // subscribers/members (minus the actor), so delivery IS the subscription filter — the agent joined
        // because it cares (e.g. a `deploys`-channel waiter, #171). Wake-on-type for the same reason as above.
        "channel.post" => channel_id.map(|id| format!("[notification] channel #{id}")),
        // A comment wakes when the recipient has a DIRECT subscription to the target (#384, subscription =
        // notification): the collaboration case the zero-polling mandate targets. A firehose-only recipient
        // stays `subscribed=false` and accrues for poll (so a board-wide coordinator isn't woken per ticket).
        "task.commented" if subscribed => task_id.map(|id| format!("[notification] comment on task #{id}")),
        // A firehose-only task.commented, plus task.status_changed / task.updated / presence.updated and
        // every other type, are INFORMATIONAL here — they accrue for the next poll and never inject a wake.
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
    // #384: the board's per-recipient subscription hint (true = direct subscription to the target). Absent on
    // a pre-#384 payload -> `false`, which leaves every type-gated wake (assign/dm/channel) intact and simply
    // keeps a comment dropping-to-poll.
    let subscribed = v.get("subscribed").and_then(Value::as_bool).unwrap_or(false);
    let prompt = notification_prompt(event_type, task_id, event_seq, channel_id, subscribed)?;
    Some((recipient.to_string(), prompt))
}

/// Delay after the literal paste — and between the two submit `Enter`s — that lets a full-screen TUI composer
/// commit the pasted text before a submitting keystroke arrives. See [`submit_steps`] for why.
const INJECT_SETTLE: std::time::Duration = std::time::Duration::from_millis(300);

/// One step of the wake-injection sequence built by [`submit_steps`].
#[derive(Debug, PartialEq, Eq)]
enum InjectStep<'a> {
    /// `send-keys -l <text>`: paste the text literally (no character is read as a key binding).
    Literal(&'a str),
    /// `send-keys Enter`: a submit keystroke.
    Enter,
    /// Sleep [`INJECT_SETTLE`] to let the composer commit the preceding input before the next keystroke.
    Settle,
}

/// The ordered steps to inject `text` as a SUBMITTED prompt: paste the text, settle, `Enter`, settle,
/// `Enter`. The settle + second `Enter` exist because a full-screen TUI composer (the codex harness) processes
/// a bracketed paste asynchronously — an `Enter` sent immediately after the paste can arrive before the
/// composer has committed the text and be dropped, leaving the prompt sitting unsubmitted; settling lets the
/// paste commit, and the second `Enter` is a belt-and-suspenders submit if the first still raced. This is
/// harmless for the claude harness (the proven wake path): its `Enter` submits the now-committed prompt, and
/// the second `Enter` lands on an empty composer, where `Enter` is a no-op — so the wake still fires exactly
/// once. Pure — unit-tested.
fn submit_steps(text: &str) -> Vec<InjectStep<'_>> {
    vec![
        InjectStep::Literal(text),
        InjectStep::Settle,
        InjectStep::Enter,
        InjectStep::Settle,
        InjectStep::Enter,
    ]
}

/// Inject `text` as a submitted prompt into tmux window `session:window`, following [`submit_steps`] (paste
/// literally, settle so a TUI composer commits the paste, then submit — with a second settle+`Enter` as a
/// harmless-for-claude belt-and-suspenders submit that also lands a codex wake). `Err` if the window is absent
/// or tmux is unreachable.
pub fn tmux_inject(session: &str, window: &str, text: &str) -> Result<(), String> {
    let target = format!("{session}:{window}");
    for step in submit_steps(text) {
        match step {
            InjectStep::Literal(t) => {
                let sent = Command::new("tmux")
                    .args(["send-keys", "-t", &target, "-l", t])
                    .status()
                    .map_err(|e| format!("tmux send-keys -t {target}: {e}"))?;
                if !sent.success() {
                    return Err(format!("tmux send-keys -l to {target} failed (window absent?)"));
                }
            }
            InjectStep::Enter => {
                Command::new("tmux")
                    .args(["send-keys", "-t", &target, "Enter"])
                    .status()
                    .map_err(|e| format!("tmux send-keys Enter -t {target}: {e}"))?;
            }
            InjectStep::Settle => std::thread::sleep(INJECT_SETTLE),
        }
    }
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
    fn submit_steps_pastes_then_settles_and_double_enters() {
        let steps = submit_steps("[notification] message #5");
        // Paste the literal text first, so no character is read as a key binding.
        assert_eq!(steps.first(), Some(&InjectStep::Literal("[notification] message #5")));
        // A settle must separate the paste from the FIRST Enter — the codex composer commits the paste in that
        // window, so the submitting Enter is not dropped racing the async paste.
        let first_enter = steps.iter().position(|s| *s == InjectStep::Enter).expect("has an Enter");
        assert!(
            steps[..first_enter].contains(&InjectStep::Settle),
            "a settle precedes the first Enter so the paste has committed"
        );
        // Two Enters submit: the second is the belt-and-suspenders that lands a codex wake if the first raced,
        // and is a no-op at claude's (now-empty) composer — so a claude wake still fires exactly once.
        assert_eq!(steps.iter().filter(|s| **s == InjectStep::Enter).count(), 2, "double-Enter submit");
        // The very last step is an Enter (the submit), never a trailing settle.
        assert_eq!(steps.last(), Some(&InjectStep::Enter));
    }

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
    fn prompt_wakes_direct_delivery_types_and_subscribed_comments_not_firehose() {
        // Direct-delivery types wake on TYPE — the recipient can't be a passive bystander of them — so they
        // wake regardless of the `subscribed` hint (here `false`). A DM in particular has no subscribable
        // target, so it MUST stay type-gated.
        assert_eq!(notification_prompt("task.assigned", Some(42), None, None, false).as_deref(), Some("[notification] task #42"));
        assert_eq!(notification_prompt("message.direct", None, Some(438), None, false).as_deref(), Some("[notification] message #438"));
        // A post to a channel the agent is a member of (delivery = membership; #171) wakes on type.
        assert_eq!(notification_prompt("channel.post", None, Some(9), Some(7), false).as_deref(), Some("[notification] channel #7"));
        assert_eq!(notification_prompt("channel.post", None, Some(9), None, false), None, "no channel_id → can't form a prompt");
        // A comment on a target the recipient DIRECTLY subscribes to wakes (#384, subscription = notification)
        // — the collaboration case the zero-polling mandate targets.
        assert_eq!(notification_prompt("task.commented", Some(42), Some(9), None, true).as_deref(), Some("[notification] comment on task #42"));
        assert_eq!(notification_prompt("task.commented", None, Some(9), None, true), None, "no task_id → can't form a prompt even when subscribed");
        // NO wake (accrues for the next poll): a comment from a FIREHOSE-only recipient (no direct
        // subscription) must not loop-wake a board-wide coordinator on every ticket; a status change never
        // wakes at all.
        assert_eq!(notification_prompt("task.commented", Some(42), Some(9), None, false), None, "a firehose-only comment accrues for poll, never wakes");
        assert_eq!(notification_prompt("task.status_changed", Some(42), Some(9), None, true), None, "status change never wakes, even when subscribed");
        // an assignment without a task_id, or a DM without a seq, can't form a prompt
        assert_eq!(notification_prompt("task.assigned", None, Some(1), None, false), None);
        assert_eq!(notification_prompt("message.direct", Some(1), None, None, false), None);
        // presence churn and other event types are ignored
        assert_eq!(notification_prompt("presence.updated", None, Some(3), None, true), None);
        assert_eq!(notification_prompt("task.updated", Some(5), None, None, true), None);
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
        // a comment to a FIREHOSE-only recipient does NOT wake (it accrues for poll). A pre-#384 payload has
        // no `subscribed` field → defaults false → drops-to-poll.
        assert_eq!(payload_to_wake(&serde_json::json!({"recipient":"x","type":"task.commented","task_id":7,"event_seq":9})), None);
        assert_eq!(payload_to_wake(&serde_json::json!({"recipient":"x","type":"task.commented","task_id":7,"event_seq":9,"subscribed":false})), None);
        // a comment to a recipient with a DIRECT subscription to the task (#384) DOES wake.
        let subbed = serde_json::json!({"recipient":"v-effects","type":"task.commented","task_id":7,"event_seq":9,"subscribed":true});
        assert_eq!(payload_to_wake(&subbed), Some(("v-effects".into(), "[notification] comment on task #7".into())));
        // a channel.post (tunnel payload carries recipient + channel_id) wakes the subscriber — #171
        let post = serde_json::json!({"recipient":"waiter","type":"channel.post","channel_id":7,"event_seq":51,"data":{"body":"deploy…","from":"deployer"}});
        assert_eq!(payload_to_wake(&post), Some(("waiter".into(), "[notification] channel #7".into())));
    }
}
