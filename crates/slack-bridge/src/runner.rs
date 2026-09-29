//! Async transport helpers — the thin layer between the pure lib and slack-morphism / tokio. Kept out of
//! `main.rs` so `main` reads as a wiring diagram. Not unit-tested (live WebSocket + blocking board I/O
//! moved off the runtime via `spawn_blocking`); the pure decisions it calls into ARE tested in the lib.

use slack_bridge::board::BoardClient;
use slack_bridge::config::Config;
use slack_bridge::format::{render_outbound_reflect, render_outbound_reflect_plain};
use slack_bridge::{
    plan_inbound, plan_outbound, relay_plan, ChannelMap, Event, RelayPlan, SlackTokens,
    RELAY_QUEUE_WARN,
};
use slack_morphism::errors::SlackClientError;
use slack_morphism::prelude::*;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

type BoxErr = Box<dyn std::error::Error + Send + Sync>;

/// How many firehose events to pull per poll.
const POLL_LIMIT: usize = 100;
/// The outbound poll cadence.
const POLL_INTERVAL: Duration = Duration::from_secs(2);

fn hyper_client() -> Result<SlackHyperClient, BoxErr> {
    Ok(SlackClient::new(SlackClientHyperConnector::new()?))
}

// ── firehose cursor persistence ────────────────────────────────────────────────────────────────────

fn cursor_path(state_dir: &Path) -> PathBuf {
    state_dir.join("slack-bridge.cursor")
}

/// The persisted firehose cursor, or `None` if never written (first run).
fn load_cursor(state_dir: &Path) -> Option<i64> {
    std::fs::read_to_string(cursor_path(state_dir))
        .ok()
        .and_then(|s| s.trim().parse().ok())
}

/// Persist the cursor (best-effort — a write failure is logged, not fatal; the loop keeps working, worst
/// case re-posting from the last durable cursor after a restart).
fn save_cursor(state_dir: &Path, cursor: i64) {
    let path = cursor_path(state_dir);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Err(e) = std::fs::write(&path, cursor.to_string()) {
        tracing::warn!(error = %e, cursor, "failed to persist firehose cursor");
    }
}

/// Do one blocking firehose poll off the tokio runtime (ureq is blocking).
async fn poll_events(board_api: &str, since_seq: i64) -> Result<Vec<Event>, String> {
    let api = board_api.to_string();
    tokio::task::spawn_blocking(move || BoardClient::new(&api).poll_events(since_seq, POLL_LIMIT))
        .await
        .map_err(|e| format!("poll join failed: {e}"))?
}

/// First run (no cursor file): initialize the cursor at the current firehose head WITHOUT posting, so the
/// bridge doesn't replay the whole board backlog into Slack on first boot.
async fn initialize_cursor_at_head(board_api: &str) -> i64 {
    let mut since = 0i64;
    loop {
        match poll_events(board_api, since).await {
            Ok(evs) if evs.is_empty() => return since,
            // `since_seq` is exclusive so a non-empty page always has a max > since → this terminates.
            Ok(evs) => since = evs.iter().map(|e| e.seq).max().unwrap_or(since),
            Err(e) => {
                tracing::warn!(error = %e, "cursor init poll failed; starting at 0");
                return since;
            }
        }
    }
}

// ── OUTBOUND (board → Slack) ────────────────────────────────────────────────────────────────────────

/// Poll the firehose and reflect authorized `channel.outbound_reflect` posts to the mapped Slack channel,
/// advancing a persisted cursor. Needs at least one channel link — without one there's nothing to mirror.
pub async fn outbound_loop(cfg: Arc<Config>, tokens: SlackTokens, map: Arc<ChannelMap>) {
    if map.is_empty() {
        tracing::warn!("no channel_map links — outbound reflect disabled (inbound still works)");
        return;
    }
    let client = match hyper_client() {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(error = %e, "outbound: could not build slack client");
            return;
        }
    };
    let bot = SlackApiToken::new(tokens.bot_token.clone().into());

    let mut cursor = match load_cursor(&cfg.state_dir) {
        Some(c) => c,
        None => {
            let head = initialize_cursor_at_head(&cfg.board_api).await;
            save_cursor(&cfg.state_dir, head);
            tracing::info!(head, "initialized firehose cursor at head — skipping backlog");
            head
        }
    };

    // event_seq → count of CONTENT-class post failures, so a deterministically un-postable reflect
    // escalates degrade→quarantine across ticks without blocking the cursor forever. In-memory: a restart
    // resets it, which is fine (bounded re-clear, never the ~11h wedge).
    let mut failures: HashMap<i64, u32> = HashMap::new();

    loop {
        if let Err(e) = outbound_tick(&cfg, &client, &bot, &map, &mut cursor, &mut failures).await {
            tracing::warn!(error = %e, "outbound tick error (will retry)");
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

/// Classify a Slack post error: CONTENT (the message itself is un-postable — an API error like
/// `internal_error`/`msg_too_long`) vs TRANSIENT (transport/HTTP/rate-limit — retry in place). Only content
/// failures advance a message toward degrade/quarantine; `ratelimited` is transient.
fn is_content_post_error(e: &SlackClientError) -> bool {
    match e {
        SlackClientError::ApiError(api) => api.code != "ratelimited",
        _ => false,
    }
}

async fn outbound_tick(
    cfg: &Config,
    client: &SlackHyperClient,
    bot: &SlackApiToken,
    map: &ChannelMap,
    cursor: &mut i64,
    failures: &mut HashMap<i64, u32>,
) -> Result<(), BoxErr> {
    let events = poll_events(&cfg.board_api, *cursor).await?;
    if events.is_empty() {
        return Ok(());
    }
    let (posts, batch_max) = plan_outbound(&events, *cursor, |cid| map.board_to_slack(cid));
    if posts.len() >= RELAY_QUEUE_WARN {
        tracing::warn!(depth = posts.len(), "outbound reflect backlog this batch");
    }

    let session = client.open_session(bot);
    for post in posts {
        let fails = failures.get(&post.event_seq).copied().unwrap_or(0);
        let plan = relay_plan(fails);
        if plan == RelayPlan::Quarantine {
            tracing::warn!(
                event_seq = post.event_seq, content_failures = fails,
                "outbound: quarantining un-postable reflect — advancing past it (full text stays on the board)"
            );
            failures.remove(&post.event_seq);
            *cursor = post.event_seq; // terminal — advance past it so the queue never wedges
            save_cursor(&cfg.state_dir, *cursor);
            continue;
        }
        let text = if plan == RelayPlan::Degraded {
            render_outbound_reflect_plain(&post.reflect)
        } else {
            render_outbound_reflect(&post.reflect)
        };
        let req = SlackApiChatPostMessageRequest::new(
            post.slack_channel.clone().into(),
            SlackMessageContent::new().with_text(text),
        );
        match session.chat_post_message(&req).await {
            Ok(_) => {
                failures.remove(&post.event_seq);
                *cursor = post.event_seq;
                save_cursor(&cfg.state_dir, *cursor);
            }
            Err(e) if is_content_post_error(&e) => {
                let n = fails + 1;
                failures.insert(post.event_seq, n);
                tracing::warn!(
                    event_seq = post.event_seq, attempt = n, ?plan, error = %e,
                    "outbound: content post error — will degrade then quarantine; not advancing past it"
                );
                // Stop here: retry this reflect (+ the rest) next tick. Cursor stayed at the last
                // successfully-posted event, so nothing before it re-posts.
                return Ok(());
            }
            Err(e) => {
                tracing::warn!(error = %e, "outbound: transient post error — retry next tick");
                return Ok(());
            }
        }
    }
    // Every post was terminally handled — advance past any trailing non-reflect / unmapped events too.
    if batch_max > *cursor {
        *cursor = batch_max;
        save_cursor(&cfg.state_dir, *cursor);
    }
    Ok(())
}

// ── INBOUND (Slack → board) ──────────────────────────────────────────────────────────────────────────

/// Run the Socket Mode listener until the socket closes. Push message events are routed via
/// [`handle_message`]. State is shared into the callback through the listener's user state.
pub async fn run_socket_mode(
    cfg: Arc<Config>,
    tokens: SlackTokens,
    map: Arc<ChannelMap>,
) -> Result<(), BoxErr> {
    let client = Arc::new(hyper_client()?);
    let callbacks = SlackSocketModeListenerCallbacks::new().with_push_events(on_push_event);
    let listener_environment = Arc::new(
        SlackClientEventsListenerEnvironment::new(client.clone())
            .with_user_state(BridgeState { cfg, map }),
    );
    let listener = SlackClientSocketModeListener::new(
        &SlackClientSocketModeConfig::new(),
        listener_environment,
        callbacks,
    );
    let app_token = SlackApiToken::new(tokens.app_token.clone().into());
    listener.listen_for(&app_token).await?;
    listener.serve().await;
    Ok(())
}

/// Shared state handed to the push-events callback: the config + the channel map. The inbound path posts
/// to the board (never to Slack — that's the outbound loop's job), so no bot token here.
#[derive(Clone)]
struct BridgeState {
    cfg: Arc<Config>,
    map: Arc<ChannelMap>,
}

async fn on_push_event(
    event: SlackPushEventCallback,
    _client: Arc<SlackHyperClient>,
    states: SlackClientEventsUserState,
) -> Result<(), BoxErr> {
    if let SlackEventCallbackBody::Message(msg) = event.event {
        let state = {
            let read = states.read().await;
            read.get_user_state::<BridgeState>().cloned()
        };
        let Some(state) = state else { return Ok(()) };
        handle_message(&state, msg).await;
    }
    Ok(())
}

/// Turn an operator's Slack message into an attributed board post. Skips the bot's own posts, edits/other
/// subtypes, empty text, and messages in an unmapped Slack channel.
async fn handle_message(state: &BridgeState, msg: SlackMessageEvent) {
    if msg.sender.bot_id.is_some() || msg.subtype.is_some() {
        return;
    }
    let Some(channel) = msg.origin.channel.as_ref().map(|c| c.to_string()) else {
        return;
    };
    let text = msg
        .content
        .as_ref()
        .and_then(|c| c.text.clone())
        .unwrap_or_default();
    let text = text.trim();
    if text.is_empty() {
        return;
    }
    let Some(user) = msg.sender.user.as_ref().map(|u| u.to_string()) else {
        return;
    };
    if user.is_empty() {
        return;
    }

    let cfg = &state.cfg;
    // reply_to threading (Slack thread_ts → board parent seq) needs the thread link from board-core #151
    // slice 2; until then an inbound reply posts top-level (reply_to = None).
    let Some(plan) = plan_inbound(&channel, &user, text, None, &cfg.bridge_agent, |ch| {
        state.map.slack_to_board(ch)
    }) else {
        tracing::debug!(%channel, "inbound: unmapped Slack channel — ignored");
        return;
    };

    let board_api = cfg.board_api.clone();
    let board_channel = plan.board_channel_id;
    let body = plan.body.clone();
    let res = tokio::task::spawn_blocking(move || {
        BoardClient::new(&board_api).post_raw(board_channel, &body)
    })
    .await;
    match res {
        Ok(Ok(())) => {
            tracing::info!(%channel, board_channel, %user, "inbound: posted Slack message to board")
        }
        Ok(Err(e)) => tracing::warn!(error = %e, "inbound: board post failed"),
        Err(e) => tracing::warn!(error = %e, "inbound: post task join failed"),
    }
}
