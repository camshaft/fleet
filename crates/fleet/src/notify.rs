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
use std::sync::Arc;

use serde_json::Value;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

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
///
/// `reactive_unaddressed` narrows the wake for a REACTIVE relay/responder agent (task_580). A reactive agent
/// is a member/subscriber of the channels it bridges and the tasks it watches, so under subscription =
/// notification (#384) it is woken on EVERY ambient post/comment there — a full harness tick it correctly
/// concludes "no action" on, during live chatter it is not addressed in (the frank + v-slack-bridge evidence).
/// This flag suppresses the two AMBIENT-CAPABLE subscription wakes (`channel.post` and `task.commented`) for a
/// recipient that is reactive AND that the board says this event does NOT address (not an @mention of it, not
/// a reply in a thread it is engaged in). It never gates `task.assigned` / `message.direct` — those inherently
/// address the recipient. It defaults false (a non-reactive agent, or a board not yet sending the
/// reactive/addressed hints), so this is additive and dormant — identical to the pre-task_580 wake set until
/// the board opts a reactive agent in. The board owns the complementary half (do not auto-subscribe a
/// bridge-owner to the channels it bridges); the two together cut the waste at both the wake layer and the
/// subscription source.
pub fn notification_prompt(
    event_type: &str,
    task_id: Option<i64>,
    event_seq: Option<i64>,
    channel_id: Option<i64>,
    subscribed: bool,
    reactive_unaddressed: bool,
    doc_ref: Option<&str>,
) -> Option<String> {
    match event_type {
        "task.assigned" => task_id.map(|id| format!("[notification] task #{id}")),
        // A document approval wakes its subscribers: the board delivers `document.approved` only to the doc's
        // subscribers, so delivery IS the subscription — wake-on-type, like a DM (task_1147). Render the
        // board's canonical typed ref verbatim as `[approval] doc_123`. The ref the board sends is ALREADY
        // `doc_<id>`, so NEVER prepend `doc ` — that doubles to `doc doc_123`.
        "document.approved" => doc_ref.map(|r| format!("[notification] [approval] {r}")),
        "message.direct" => event_seq.map(|seq| format!("[notification] message #{seq}")),
        // A reactive relay/responder member of this channel is not addressed by this post (task_580): suppress
        // the wake so ambient channel chatter it would conclude "no action" on does not cost a harness tick. It
        // still accrues in the durable inbox for the next poll, so nothing is lost — only the wasted wake is cut.
        "channel.post" if reactive_unaddressed => None,
        // A post to a channel the agent is a member of: the board only delivers `channel.post` to a channel's
        // subscribers/members (minus the actor), so delivery IS the subscription filter — the agent joined
        // because it cares (e.g. a `deploys`-channel waiter, #171). Wake-on-type for the same reason as above.
        "channel.post" => channel_id.map(|id| format!("[notification] channel #{id}")),
        // A comment wakes when the recipient has a DIRECT subscription to the target (#384, subscription =
        // notification): the collaboration case the zero-polling mandate targets. A firehose-only recipient
        // stays `subscribed=false` and accrues for poll (so a board-wide coordinator isn't woken per ticket).
        // A reactive recipient this comment does not address is suppressed for the same reason as channel.post.
        "task.commented" if subscribed && !reactive_unaddressed => {
            task_id.map(|id| format!("[notification] comment on task #{id}"))
        }
        // A fresh task CREATED in a target the recipient DIRECTLY subscribes to wakes it (task_1104): the
        // intake-triage case. board-triage subscribes to the intake projects (e.g. the uncategorized front
        // door), so a fresh operator-filed create wakes it at once instead of waiting out its ~30-min idle poll
        // — the "why did this take so long to get triaged" gap. Gated on `subscribed` for the same reason as a
        // comment: a direct project subscriber (the triager) wakes, a firehose-only board-wide coordinator does
        // not (it accrues for poll, so a create never loop-wakes a whole-board watcher). A reactive recipient
        // the create does not address is suppressed identically. An assigned-at-create task still wakes its
        // direct subscriber here (harmless: the triager sees it is already owned and moves on in one tick) — not
        // gating on assignment keeps this free of any new payload field and can never MISS a triage wake.
        "task.created" if subscribed && !reactive_unaddressed => {
            task_id.map(|id| format!("[notification] new task #{id}"))
        }
        // A firehose-only task.commented / task.created, plus task.status_changed / task.updated /
        // presence.updated and every other type, are INFORMATIONAL here — they accrue for the next poll and
        // never inject a wake.
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
    let subscribed = v
        .get("subscribed")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    // task_580: the two per-recipient hints that gate a REACTIVE agent's ambient wakes. `reactive` (board
    // metadata) marks a relay/responder; `addressed` is whether THIS event addresses it (an @mention, a reply
    // in a thread it is engaged in). We suppress only when the recipient is reactive AND the board EXPLICITLY
    // says the event does not address it (`addressed == Some(false)`). An absent `addressed` (a board that does
    // not yet send the signal) stays `None` -> NOT suppressed, so enabling `reactive` without the addressing
    // signal can never silently drop a legitimate addressed wake — it only narrows once the board sends both.
    let reactive = v.get("reactive").and_then(Value::as_bool).unwrap_or(false);
    let addressed = v.get("addressed").and_then(Value::as_bool);
    let reactive_unaddressed = reactive && addressed == Some(false);
    // The board's canonical typed id for a document event (task_1147): top-level `ref`, falling back to
    // `data.ref` depending on how the event body is shaped. Rendered verbatim (already `doc_<id>`).
    let doc_ref = v
        .get("ref")
        .and_then(Value::as_str)
        .or_else(|| v.get("data").and_then(|d| d.get("ref")).and_then(Value::as_str));
    let prompt = notification_prompt(
        event_type,
        task_id,
        event_seq,
        channel_id,
        subscribed,
        reactive_unaddressed,
        doc_ref,
    )?;
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
                    return Err(format!(
                        "tmux send-keys -l to {target} failed (window absent?)"
                    ));
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
fn classify_request(method: &str, url: &str) -> Incoming {
    let path = url.split('?').next().unwrap_or(url);
    if method.eq_ignore_ascii_case("GET") && matches!(path, "/health" | "/healthz" | "/") {
        Incoming::HealthProbe
    } else {
        Incoming::Webhook
    }
}

/// Parse an HTTP/1.x request line (`"METHOD SP REQUEST-URI SP HTTP-VERSION"`) into `(method, path)`. `None`
/// if the line doesn't have at least two whitespace-separated tokens (the HTTP-version token, if present, is
/// ignored — nothing here branches on HTTP/1.0 vs 1.1). Pure — unit-tested.
fn parse_request_line(line: &str) -> Option<(String, String)> {
    let mut parts = line.split_whitespace();
    let method = parts.next()?;
    let path = parts.next()?;
    Some((method.to_string(), path.to_string()))
}

/// Case-insensitive `Content-Length` lookup from a raw header block (one `Name: value` per line, request
/// line already stripped). Defaults to 0 when absent or unparseable — the body is empty or the sender is
/// malformed either way, and 0 just means "read no body" rather than hang waiting for one. Pure — unit-tested.
fn parse_content_length(headers: &str) -> usize {
    headers
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.trim()
                .eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse().ok())?
        })
        .unwrap_or(0)
}

/// Find the `\r\n\r\n` header/body boundary in a raw byte buffer — the index of its first byte, or `None` if
/// the buffer doesn't contain one yet (more reads are needed). Pure — unit-tested.
fn find_header_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

/// A header block this large (no `\r\n\r\n` found yet) is not a real HTTP client — stop reading rather than
/// grow the buffer without bound.
const MAX_HEADER_BYTES: usize = 64 * 1024;
/// Cap a claimed `Content-Length` so a malformed/hostile value can't force an unbounded body read — every
/// real board webhook payload (a JSON event envelope) is tiny next to this.
const MAX_BODY_BYTES: usize = 10 * 1024 * 1024;

/// Read one HTTP/1.x request off `stream`: the request line, headers (kept only for `Content-Length`), and
/// exactly that many body bytes. `Content-Length` only — every sender here (the board's webhook POST, a
/// liveness `GET`) either sets it or has no body; chunked transfer-encoding is unsupported (the board never
/// sends it, so supporting it would be untested dead code). `Err` on a malformed request line or a read that
/// ends before the headers/body complete. Generic over `AsyncRead` so it is unit-testable against an in-memory
/// `tokio::io::duplex` pair, with no real socket needed.
async fn read_request<S: AsyncRead + Unpin>(
    stream: &mut S,
) -> std::io::Result<(String, String, String)> {
    use std::io::{Error, ErrorKind};
    let mut buf = Vec::new();
    let header_end = loop {
        if let Some(pos) = find_header_end(&buf) {
            break pos;
        }
        if buf.len() >= MAX_HEADER_BYTES {
            return Err(Error::new(
                ErrorKind::InvalidData,
                "fleet notify: request headers exceeded 64KiB",
            ));
        }
        let mut chunk = [0u8; 4096];
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            return Err(Error::new(
                ErrorKind::UnexpectedEof,
                "fleet notify: connection closed before headers completed",
            ));
        }
        buf.extend_from_slice(&chunk[..n]);
    };
    let head = String::from_utf8_lossy(&buf[..header_end]).into_owned();
    let mut lines = head.lines();
    let (method, path) = lines.next().and_then(parse_request_line).ok_or_else(|| {
        Error::new(
            ErrorKind::InvalidData,
            "fleet notify: malformed request line",
        )
    })?;
    let headers = lines.collect::<Vec<_>>().join("\n");
    let content_length = parse_content_length(&headers).min(MAX_BODY_BYTES);
    let mut body = buf[header_end + 4..].to_vec();
    while body.len() < content_length {
        let mut chunk = [0u8; 4096];
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            return Err(Error::new(
                ErrorKind::UnexpectedEof,
                "fleet notify: connection closed before body completed",
            ));
        }
        body.extend_from_slice(&chunk[..n]);
    }
    body.truncate(content_length);
    Ok((method, path, String::from_utf8_lossy(&body).into_owned()))
}

/// Handle one accepted connection end to end: read the request, ack it `200` immediately (best-effort — a
/// wake is never worth wedging the endpoint the board POSTs to), then — for a webhook, not a liveness probe
/// — parse the body and inject the wake. `tmux_inject` shells out and sleeps (~600ms per event); running it
/// via `spawn_blocking` keeps that off this connection's async task so it never delays accepting the board's
/// next webhook POST or a concurrent liveness probe (the single blocking-accept-loop this replaced could not
/// overlap those at all).
async fn handle_connection(mut stream: TcpStream, session: Arc<str>) {
    let (method, path, body) = match read_request(&mut stream).await {
        Ok(parts) => parts,
        Err(e) => {
            eprintln!("fleet notify: dropping unreadable request: {e}");
            return;
        }
    };
    let _ = stream
        .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
        .await;
    if classify_request(&method, &path) == Incoming::HealthProbe {
        return;
    }
    match serde_json::from_str::<Value>(&body) {
        // presence/comment/other events yield no prompt and are silently dropped
        Ok(v) => {
            if let Some((recipient, prompt)) = payload_to_wake(&v) {
                tokio::task::spawn_blocking(move || {
                    match tmux_inject(&session, &recipient, &prompt) {
                        Ok(()) => eprintln!("woke {recipient}: {prompt}"),
                        Err(e) => eprintln!("inject failed for {recipient}: {e}"),
                    }
                });
            }
        }
        Err(e) => eprintln!("fleet notify: dropping unparseable webhook body: {e}"),
    }
}

/// Run the notifier: bind a local HTTP endpoint and, for each board webhook POST, inject the wake prompt
/// into the recipient agent's tmux window in `session`. Blocks (a long-running daemon) — but each connection
/// runs on its own tokio task ([`handle_connection`]), so a slow wake injection on one event never stalls the
/// board's next webhook POST or a liveness probe arriving concurrently. Best-effort: every request is
/// answered `200` immediately, and a payload that is unparseable or not actionable is logged and dropped. A
/// supervisor liveness-probes the daemon with a `GET` to `/health` (see [`classify_request`]).
pub fn serve(port: u16, session: &str) -> Result<(), String> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("fleet notify: build tokio runtime: {e}"))?;
    rt.block_on(serve_async(port, session))
}

async fn serve_async(port: u16, session: &str) -> Result<(), String> {
    let listener = TcpListener::bind(("127.0.0.1", port))
        .await
        .map_err(|e| format!("fleet notify: bind 127.0.0.1:{port}: {e}"))?;
    eprintln!(
        "fleet notify: listening on http://127.0.0.1:{port} — waking session '{session}' on board webhooks (GET /health for liveness)"
    );
    let session: Arc<str> = Arc::from(session);
    loop {
        let (stream, _) = listener
            .accept()
            .await
            .map_err(|e| format!("fleet notify: accept: {e}"))?;
        tokio::spawn(handle_connection(stream, session.clone()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn submit_steps_pastes_then_settles_and_double_enters() {
        let steps = submit_steps("[notification] message #5");
        // Paste the literal text first, so no character is read as a key binding.
        assert_eq!(
            steps.first(),
            Some(&InjectStep::Literal("[notification] message #5"))
        );
        // A settle must separate the paste from the FIRST Enter — the codex composer commits the paste in that
        // window, so the submitting Enter is not dropped racing the async paste.
        let first_enter = steps
            .iter()
            .position(|s| *s == InjectStep::Enter)
            .expect("has an Enter");
        assert!(
            steps[..first_enter].contains(&InjectStep::Settle),
            "a settle precedes the first Enter so the paste has committed"
        );
        // Two Enters submit: the second is the belt-and-suspenders that lands a codex wake if the first raced,
        // and is a no-op at claude's (now-empty) composer — so a claude wake still fires exactly once.
        assert_eq!(
            steps.iter().filter(|s| **s == InjectStep::Enter).count(),
            2,
            "double-Enter submit"
        );
        // The very last step is an Enter (the submit), never a trailing settle.
        assert_eq!(steps.last(), Some(&InjectStep::Enter));
    }

    #[test]
    fn classify_request_routes_get_health_paths_to_a_probe_else_webhook() {
        // A GET to a health path (query string ignored) is a liveness probe.
        assert_eq!(classify_request("GET", "/health"), Incoming::HealthProbe);
        assert_eq!(classify_request("GET", "/healthz"), Incoming::HealthProbe);
        assert_eq!(classify_request("GET", "/"), Incoming::HealthProbe);
        assert_eq!(
            classify_request("GET", "/health?probe=1"),
            Incoming::HealthProbe
        );
        assert_eq!(
            classify_request("get", "/health"),
            Incoming::HealthProbe,
            "method match is case-insensitive"
        );
        // The board POSTs webhooks — never a probe, even to a health path.
        assert_eq!(classify_request("POST", "/"), Incoming::Webhook);
        assert_eq!(classify_request("POST", "/health"), Incoming::Webhook);
        // A non-health GET is treated as a webhook (the default), not a probe.
        assert_eq!(classify_request("GET", "/webhook"), Incoming::Webhook);
        assert_eq!(classify_request("GET", "/events"), Incoming::Webhook);
    }

    #[test]
    fn parse_request_line_splits_method_and_path_and_ignores_the_version_token() {
        assert_eq!(
            parse_request_line("POST /webhook HTTP/1.1"),
            Some(("POST".into(), "/webhook".into()))
        );
        assert_eq!(
            parse_request_line("GET / HTTP/1.0"),
            Some(("GET".into(), "/".into()))
        );
        // The HTTP-version token is optional for parsing purposes — two tokens are enough.
        assert_eq!(
            parse_request_line("GET /health"),
            Some(("GET".into(), "/health".into()))
        );
        assert_eq!(
            parse_request_line(""),
            None,
            "an empty line has no method token"
        );
        assert_eq!(
            parse_request_line("GET"),
            None,
            "a lone method has no path token"
        );
    }

    #[test]
    fn parse_content_length_is_case_insensitive_and_defaults_to_zero() {
        assert_eq!(parse_content_length("Content-Length: 42\r\nOther: x"), 42);
        assert_eq!(
            parse_content_length("content-length: 7"),
            7,
            "header name match is case-insensitive"
        );
        assert_eq!(parse_content_length("CONTENT-LENGTH: 3"), 3);
        assert_eq!(
            parse_content_length("Other: x"),
            0,
            "absent header defaults to 0"
        );
        assert_eq!(
            parse_content_length("Content-Length: not-a-number"),
            0,
            "unparseable value defaults to 0"
        );
        assert_eq!(parse_content_length(""), 0);
    }

    #[test]
    fn find_header_end_locates_the_crlf_crlf_boundary() {
        assert_eq!(find_header_end(b"GET / HTTP/1.1\r\n\r\nbody"), Some(14));
        assert_eq!(
            find_header_end(b"GET / HTTP/1.1\r\nHost: x\r\n\r\n"),
            Some(23)
        );
        assert_eq!(
            find_header_end(b"GET / HTTP/1.1\r\nHost: x"),
            None,
            "no body separator read yet"
        );
    }

    #[tokio::test]
    async fn read_request_parses_a_post_with_a_body_by_content_length() {
        let (mut client, mut server) = tokio::io::duplex(1024);
        client
            .write_all(b"POST /webhook HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: 10\r\n\r\n{\"a\":true}EXTRA")
            .await
            .unwrap();
        let (method, path, body) = read_request(&mut server).await.unwrap();
        assert_eq!(method, "POST");
        assert_eq!(path, "/webhook");
        // Exactly Content-Length bytes — "EXTRA" (the next pipelined request, if any) is left unread.
        assert_eq!(body, "{\"a\":true}");
        drop(client);
    }

    #[tokio::test]
    async fn read_request_parses_a_get_with_no_body() {
        let (mut client, mut server) = tokio::io::duplex(1024);
        client
            .write_all(b"GET /health HTTP/1.1\r\nHost: x\r\n\r\n")
            .await
            .unwrap();
        drop(client); // close the write end so a short/absent body never hangs the read
        let (method, path, body) = read_request(&mut server).await.unwrap();
        assert_eq!(method, "GET");
        assert_eq!(path, "/health");
        assert_eq!(body, "");
    }

    #[tokio::test]
    async fn read_request_errors_when_the_connection_closes_before_headers_complete() {
        let (mut client, mut server) = tokio::io::duplex(1024);
        client
            .write_all(b"GET /health HTTP/1.1\r\nHost: x")
            .await
            .unwrap();
        drop(client);
        assert!(read_request(&mut server).await.is_err());
    }

    #[test]
    fn prompt_wakes_direct_delivery_types_and_subscribed_comments_not_firehose() {
        // Direct-delivery types wake on TYPE — the recipient can't be a passive bystander of them — so they
        // wake regardless of the `subscribed` hint (here `false`). A DM in particular has no subscribable
        // target, so it MUST stay type-gated.
        assert_eq!(
            notification_prompt("task.assigned", Some(42), None, None, false, false, None).as_deref(),
            Some("[notification] task #42")
        );
        assert_eq!(
            notification_prompt("message.direct", None, Some(438), None, false, false, None).as_deref(),
            Some("[notification] message #438")
        );
        // A post to a channel the agent is a member of (delivery = membership; #171) wakes on type.
        assert_eq!(
            notification_prompt("channel.post", None, Some(9), Some(7), false, false, None).as_deref(),
            Some("[notification] channel #7")
        );
        assert_eq!(
            notification_prompt("channel.post", None, Some(9), None, false, false, None),
            None,
            "no channel_id → can't form a prompt"
        );
        // A comment on a target the recipient DIRECTLY subscribes to wakes (#384, subscription = notification)
        // — the collaboration case the zero-polling mandate targets.
        assert_eq!(
            notification_prompt("task.commented", Some(42), Some(9), None, true, false, None).as_deref(),
            Some("[notification] comment on task #42")
        );
        assert_eq!(
            notification_prompt("task.commented", None, Some(9), None, true, false, None),
            None,
            "no task_id → can't form a prompt even when subscribed"
        );
        // NO wake (accrues for the next poll): a comment from a FIREHOSE-only recipient (no direct
        // subscription) must not loop-wake a board-wide coordinator on every ticket; a status change never
        // wakes at all.
        assert_eq!(
            notification_prompt("task.commented", Some(42), Some(9), None, false, false, None),
            None,
            "a firehose-only comment accrues for poll, never wakes"
        );
        assert_eq!(
            notification_prompt("task.status_changed", Some(42), Some(9), None, true, false, None),
            None,
            "status change never wakes, even when subscribed"
        );
        // an assignment without a task_id, or a DM without a seq, can't form a prompt
        assert_eq!(
            notification_prompt("task.assigned", None, Some(1), None, false, false, None),
            None
        );
        assert_eq!(
            notification_prompt("message.direct", Some(1), None, None, false, false, None),
            None
        );
        // presence churn and other event types are ignored
        assert_eq!(
            notification_prompt("presence.updated", None, Some(3), None, true, false, None),
            None
        );
        assert_eq!(
            notification_prompt("task.updated", Some(5), None, None, true, false, None),
            None
        );
    }

    #[test]
    fn task_created_wakes_a_direct_subscriber_not_a_firehose_recipient() {
        // task_1104: a fresh create in a target the recipient DIRECTLY subscribes to wakes it (the intake-triage
        // case — board-triage subscribes to the intake projects and must triage a fresh create at once, not wait
        // out its idle poll). Gated on `subscribed` exactly like task.commented.
        assert_eq!(
            notification_prompt("task.created", Some(1182), Some(9), None, true, false, None).as_deref(),
            Some("[notification] new task #1182"),
            "a create on a directly-subscribed target wakes the triager"
        );
        // A firehose-only recipient (subscribed=false) does NOT wake on a create — it accrues for poll, so a
        // create never loop-wakes a board-wide coordinator present via the whole-board firehose.
        assert_eq!(
            notification_prompt("task.created", Some(1182), Some(9), None, false, false, None),
            None,
            "a firehose-only create accrues for poll, never wakes"
        );
        // A reactive recipient the create does not address is suppressed, same as an ambient channel.post/comment.
        assert_eq!(
            notification_prompt("task.created", Some(1182), Some(9), None, true, true, None),
            None,
            "a reactive subscriber a create does not address is not woken (task_580 gate applies)"
        );
        // No task_id → can't form a prompt even when subscribed.
        assert_eq!(
            notification_prompt("task.created", None, Some(9), None, true, false, None),
            None
        );
        // End-to-end through payload_to_wake: a subscribed create wakes the intake triager; a firehose one does not.
        let subbed = serde_json::json!({"recipient":"board-triage","type":"task.created","task_id":1182,"event_seq":9,"subscribed":true});
        assert_eq!(
            payload_to_wake(&subbed),
            Some(("board-triage".into(), "[notification] new task #1182".into()))
        );
        let firehose = serde_json::json!({"recipient":"board-pm","type":"task.created","task_id":1182,"event_seq":9,"subscribed":false});
        assert_eq!(payload_to_wake(&firehose), None, "a firehose create does not wake");
        // A pre-#384 payload with no `subscribed` field defaults false → drops to poll (never a spurious wake).
        let no_hint = serde_json::json!({"recipient":"x","type":"task.created","task_id":1182,"event_seq":9});
        assert_eq!(payload_to_wake(&no_hint), None);
    }

    #[test]
    fn reactive_unaddressed_suppresses_only_ambient_subscription_wakes() {
        // task_580: a reactive relay/responder is woken on every ambient channel.post / task.commented it is a
        // member/subscriber of. The reactive_unaddressed gate suppresses exactly those two ambient-capable
        // subscription wakes — and ONLY those — when the recipient is reactive and the event does not address it.
        // channel.post: woken when not gated, SUPPRESSED when reactive_unaddressed.
        assert_eq!(
            notification_prompt("channel.post", None, Some(9), Some(7), false, false, None).as_deref(),
            Some("[notification] channel #7"),
            "ungated channel.post still wakes a member"
        );
        assert_eq!(
            notification_prompt("channel.post", None, Some(9), Some(7), false, true, None),
            None,
            "a reactive member not addressed by the post is NOT woken (task_580)"
        );
        // task.commented: a direct-subscribed comment wakes, but is SUPPRESSED when reactive_unaddressed.
        assert_eq!(
            notification_prompt("task.commented", Some(42), Some(9), None, true, false, None).as_deref(),
            Some("[notification] comment on task #42"),
            "ungated subscribed comment still wakes"
        );
        assert_eq!(
            notification_prompt("task.commented", Some(42), Some(9), None, true, true, None),
            None,
            "a reactive subscriber not addressed by the comment is NOT woken (task_580)"
        );
        // The gate NEVER touches the inherently-addressed direct-delivery types: an assignment or a DM to a
        // reactive agent still wakes even when reactive_unaddressed is set (those address it by definition).
        assert_eq!(
            notification_prompt("task.assigned", Some(42), None, None, false, true, None).as_deref(),
            Some("[notification] task #42"),
            "an assignment addresses the recipient — reactive gate must not suppress it"
        );
        assert_eq!(
            notification_prompt("message.direct", None, Some(438), None, false, true, None).as_deref(),
            Some("[notification] message #438"),
            "a DM addresses the recipient — reactive gate must not suppress it"
        );
    }

    #[test]
    fn payload_reactive_gate_requires_reactive_and_explicit_not_addressed() {
        // The suppression engages only when the payload says reactive=true AND addressed=false. A reactive post
        // the board does NOT mark addressed-either-way (addressed absent) must still wake — so enabling reactive
        // without the addressing signal never silently drops a legitimate wake; it only narrows once both land.
        let base = |extra: serde_json::Value| {
            let mut v = serde_json::json!({"recipient":"frank","type":"channel.post","channel_id":7,"event_seq":9});
            v.as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            v
        };
        // reactive + explicitly not addressed -> suppressed.
        assert_eq!(
            payload_to_wake(&base(
                serde_json::json!({"reactive":true,"addressed":false})
            )),
            None,
            "reactive + addressed=false suppresses the ambient wake"
        );
        // reactive but addressed -> wakes (an @mention of the reactive agent IS actionable).
        assert_eq!(
            payload_to_wake(&base(serde_json::json!({"reactive":true,"addressed":true}))),
            Some(("frank".into(), "[notification] channel #7".into())),
            "a reactive agent the post addresses still wakes"
        );
        // reactive but no addressing signal at all -> wakes (fail-safe: never drop a wake on a half-rolled-out board).
        assert_eq!(
            payload_to_wake(&base(serde_json::json!({"reactive":true}))),
            Some(("frank".into(), "[notification] channel #7".into())),
            "reactive without an addressed hint falls back to waking"
        );
        // not reactive, addressed=false -> wakes (a normal member is unaffected by the reactive gate).
        assert_eq!(
            payload_to_wake(&base(serde_json::json!({"addressed":false}))),
            Some(("frank".into(), "[notification] channel #7".into())),
            "a non-reactive member is never gated by addressing"
        );
    }

    #[test]
    fn payload_to_wake_pulls_recipient_and_prompt_or_none() {
        let assign = serde_json::json!({"recipient":"v-bolero","type":"task.assigned","task_id":7,"event_seq":100});
        assert_eq!(
            payload_to_wake(&assign),
            Some(("v-bolero".into(), "[notification] task #7".into()))
        );
        let dm = serde_json::json!({"recipient":"v-capmeshd","type":"message.direct","channel_id":1,"event_seq":438});
        assert_eq!(
            payload_to_wake(&dm),
            Some(("v-capmeshd".into(), "[notification] message #438".into()))
        );
        // missing recipient / informational type / missing ids -> None (no wake)
        assert_eq!(
            payload_to_wake(&serde_json::json!({"type":"task.assigned","task_id":7})),
            None
        );
        assert_eq!(
            payload_to_wake(&serde_json::json!({"recipient":"x","type":"presence.updated"})),
            None
        );
        // a comment to a FIREHOSE-only recipient does NOT wake (it accrues for poll). A pre-#384 payload has
        // no `subscribed` field → defaults false → drops-to-poll.
        assert_eq!(
            payload_to_wake(
                &serde_json::json!({"recipient":"x","type":"task.commented","task_id":7,"event_seq":9})
            ),
            None
        );
        assert_eq!(
            payload_to_wake(
                &serde_json::json!({"recipient":"x","type":"task.commented","task_id":7,"event_seq":9,"subscribed":false})
            ),
            None
        );
        // a comment to a recipient with a DIRECT subscription to the task (#384) DOES wake.
        let subbed = serde_json::json!({"recipient":"v-effects","type":"task.commented","task_id":7,"event_seq":9,"subscribed":true});
        assert_eq!(
            payload_to_wake(&subbed),
            Some((
                "v-effects".into(),
                "[notification] comment on task #7".into()
            ))
        );
        // a channel.post (tunnel payload carries recipient + channel_id) wakes the subscriber — #171
        let post = serde_json::json!({"recipient":"waiter","type":"channel.post","channel_id":7,"event_seq":51,"data":{"body":"deploy…","from":"deployer"}});
        assert_eq!(
            payload_to_wake(&post),
            Some(("waiter".into(), "[notification] channel #7".into()))
        );
    }

    #[test]
    fn document_approved_wakes_with_the_canonical_ref_verbatim_not_doubled() {
        // task_1147: a document.approved wakes its subscribers with "[approval] <ref>", rendering the board's
        // canonical typed ref verbatim. The board already sends ref="doc_<id>", so the prompt must NOT prepend
        // "doc " — the regression this guards is the doubled "doc doc_123".
        assert_eq!(
            notification_prompt(
                "document.approved",
                None,
                None,
                None,
                false,
                false,
                Some("doc_123")
            )
            .as_deref(),
            Some("[notification] [approval] doc_123")
        );
        // end-to-end: top-level `ref`.
        let top = serde_json::json!({"recipient":"charter-steward","type":"document.approved","ref":"doc_104"});
        assert_eq!(
            payload_to_wake(&top),
            Some((
                "charter-steward".into(),
                "[notification] [approval] doc_104".into()
            ))
        );
        // end-to-end: nested `data.ref` (the alternate body shape) resolves the same.
        let nested = serde_json::json!({"recipient":"x","type":"document.approved","data":{"ref":"doc_55","title":"t"}});
        assert_eq!(
            payload_to_wake(&nested),
            Some(("x".into(), "[notification] [approval] doc_55".into()))
        );
        // never doubled: the rendered prompt carries exactly one "doc_" and no "doc doc".
        let (_, prompt) = payload_to_wake(&top).unwrap();
        assert!(!prompt.contains("doc doc"), "ref is rendered verbatim, not re-prefixed");
        // a document.approved with no ref at all cannot be rendered -> no wake (defensive, not a doubled id).
        assert_eq!(
            payload_to_wake(&serde_json::json!({"recipient":"x","type":"document.approved"})),
            None
        );
    }
}
