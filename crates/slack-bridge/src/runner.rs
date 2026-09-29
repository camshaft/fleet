//! Async transport helpers — the thin layer between the pure lib and slack-morphism / tokio. Kept out of
//! `main.rs` so `main` reads as a wiring diagram. Not unit-tested (live WebSocket + blocking board I/O
//! moved off the runtime via `spawn_blocking`); the pure decisions it calls into ARE tested in the lib.

use slack_bridge::board::BoardClient;
use slack_bridge::config::Config;
use slack_bridge::format::{render_outbound_reflect, render_outbound_reflect_plain};
use slack_bridge::{
    plan_inbound, plan_outbound, relay_plan, ChannelLink, ChannelMap, Event, RelayPlan, SlackTokens,
    RELAY_QUEUE_WARN,
};
use slack_morphism::errors::SlackClientError;
use slack_morphism::prelude::*;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::Duration;

type BoxErr = Box<dyn std::error::Error + Send + Sync>;

/// The live channel map, shared across the outbound loop, the inbound listener, and the refresh task. A
/// std `RwLock` is fine because a guard is only ever held across PURE `sync` planning, never across an
/// `.await` (the network I/O happens after the guard is dropped).
pub type SharedMap = Arc<RwLock<ChannelMap>>;

/// How many firehose events to pull per poll.
const POLL_LIMIT: usize = 100;
/// The outbound poll cadence.
const POLL_INTERVAL: Duration = Duration::from_secs(2);
/// How often to re-read the board-registered channel links (board-core #149 slice 2) so a link registered
/// while the daemon is running — e.g. the #154 operator-DM wiring — is picked up WITHOUT a restart.
const MAP_REFRESH_INTERVAL: Duration = Duration::from_secs(60);

/// Fetch the board-registered channel links (best-effort) and merge with the static config
/// `[[channel_map]]` into a [`ChannelMap`]. Board links are the dynamic source of truth; config links are
/// applied AFTER so an explicit local override wins (`ChannelMap` is last-wins). A board read error is
/// fail-soft — falls back to the config links alone. Blocking board I/O runs off the runtime.
pub async fn fetch_channel_map(cfg: &Config) -> ChannelMap {
    let board_api = cfg.board_api.clone();
    let mut links: Vec<ChannelLink> =
        match tokio::task::spawn_blocking(move || BoardClient::new(&board_api).list_channel_links())
            .await
        {
            Ok(Ok(l)) => l,
            Ok(Err(e)) => {
                tracing::warn!(error = %e, "could not read board channel links — using config only");
                Vec::new()
            }
            Err(e) => {
                tracing::warn!(error = %e, "channel-link fetch task join failed — using config only");
                Vec::new()
            }
        };
    links.extend(cfg.channel_map.clone());
    ChannelMap::from_links(&links)
}

/// Periodically rebuild the shared channel map from the board (+ config) so a newly-registered link is
/// honored without a restart. Runs forever; each refresh is fail-soft (a fetch error keeps the last map).
pub async fn refresh_loop(cfg: Arc<Config>, map: SharedMap) {
    loop {
        tokio::time::sleep(MAP_REFRESH_INTERVAL).await;
        let fresh = fetch_channel_map(&cfg).await;
        let changed = map
            .read()
            .map(|cur| cur.len() != fresh.len())
            .unwrap_or(true);
        match map.write() {
            Ok(mut w) => {
                *w = fresh;
                if changed {
                    tracing::info!(channels = w.len(), "refreshed channel map from the board");
                }
            }
            Err(e) => tracing::warn!(error = %e, "channel map lock poisoned on refresh"),
        }
    }
}

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
pub async fn outbound_loop(cfg: Arc<Config>, tokens: SlackTokens, map: SharedMap) {
    if map.read().map(|m| m.is_empty()).unwrap_or(true) {
        // Not fatal / not an early exit: the map auto-refreshes, so a link registered later (e.g. the
        // #154 operator-DM wiring) starts flowing without a restart. Until then posts are simply empty.
        tracing::info!("channel map currently empty — outbound idle until a link is registered");
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

/// Resolve the OUT posts for a batch under a short-lived read guard (dropped before any `.await`).
fn plan_outbound_locked(
    map: &SharedMap,
    events: &[Event],
    cursor: i64,
) -> Result<(Vec<slack_bridge::OutboundPost>, i64), BoxErr> {
    let guard = map.read().map_err(|_| "channel map lock poisoned")?;
    Ok(plan_outbound(events, cursor, |cid| guard.board_to_slack(cid)))
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
    map: &SharedMap,
    cursor: &mut i64,
    failures: &mut HashMap<i64, u32>,
) -> Result<(), BoxErr> {
    let events = poll_events(&cfg.board_api, *cursor).await?;
    if events.is_empty() {
        return Ok(());
    }
    let (posts, batch_max) = plan_outbound_locked(map, &events, *cursor)?;
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
    map: SharedMap,
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
    map: SharedMap,
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
    // slice 2; until then an inbound reply posts top-level (reply_to = None). Resolve under a short-lived
    // read guard, dropped before the board post `.await`.
    let planned = {
        let Ok(guard) = state.map.read() else {
            tracing::warn!("inbound: channel map lock poisoned — dropping message");
            return;
        };
        plan_inbound(&channel, &user, text, None, &cfg.bridge_agent, |ch| {
            guard.slack_to_board(ch)
        })
    };
    let Some(plan) = planned else {
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
