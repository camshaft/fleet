//! `runner` — the daemon's async watermark poll loop (behind the `daemon` feature).
//!
//! A thin ASYNC loop on tokio (operator directive #439: no blocking IO in rust daemons — a `tokio::time`
//! timer, not a `thread::sleep` loop). It wires the tested lib together: load the per-group cursor
//! ([`crate::State`]), build the shared [`bridge_core::TaskBoard`] client, register + subscribe to the intake
//! project, then on each tick poll every configured group for tickets newer than its cursor and ingest them,
//! advancing the cursor forward only. It runs until a shutdown signal (SIGINT/SIGTERM).
//!
//! SCAFFOLD STATE (task #890): the wiring, the client, and the cursor loop are live here. The LIVE ticketing
//! read ([`poll_group`] fetching a group's tickets past the cursor) is the follow-on slice (task #891) and
//! the idempotent board ingest via external-link is task #892 — those are the clearly-marked seams below. The
//! loop runs dormant until they land: it logs its targets and sleeps, never fabricating ingest.

use crate::{Config, State, sim::Ticket};
use bridge_core::TaskBoard;
use serde_json::json;
use std::time::Duration;

/// Run the bridge until shutdown. Builds the board client, registers + subscribes to the intake project, then
/// polls on the configured cadence. Returns `Err` only on an unrecoverable startup failure; per-tick errors
/// are logged and retried on the next tick (fail-soft — a transient board/source blip never kills the daemon).
pub async fn run(config: Config) -> Result<(), String> {
    let targets = config.ingest_targets();
    if targets.is_empty() {
        tracing::warn!("no resolver groups configured — bridge is up but idle (dormant)");
    } else {
        tracing::info!(
            groups = targets.len(),
            project_id = config.project_id,
            "ticket-ingest starting"
        );
    }

    let board = TaskBoard::with_base(&config.board_api, &config.bridge_agent);
    // Register this bridge and subscribe to the intake project so board-triage routing events are visible.
    // Best-effort: a registration/subscription blip must not stop the daemon from starting its poll loop.
    if let Err(e) = board
        .register(
            None,
            &json!({ "role": "ticket-ingest", "source": crate::sim::SOURCE }),
        )
        .await
    {
        tracing::warn!(error = %e, "register failed — continuing");
    }
    if let Err(e) = board.subscribe_project(config.project_id).await {
        tracing::warn!(error = %e, project_id = config.project_id, "subscribe failed — continuing");
    }

    let mut state = State::load(&config.state_dir);
    let mut tick = tokio::time::interval(Duration::from_secs(config.poll_interval_secs));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            _ = tick.tick() => poll_tick(&config, &board, &mut state).await,
            _ = shutdown_signal() => {
                tracing::info!("shutdown signal — persisting cursor and exiting");
                if let Err(e) = state.save(&config.state_dir) {
                    tracing::warn!(error = %e, "final cursor save failed");
                }
                return Ok(());
            }
        }
    }
}

/// One poll tick across every configured group. Per-group errors are logged and skipped (one bad group never
/// starves the others); the cursor is persisted after a group terminally advances.
async fn poll_tick(config: &Config, board: &TaskBoard, state: &mut State) {
    for (group, project_id) in config.ingest_targets() {
        let cursor = state.since_for(group);
        match poll_group(group, cursor).await {
            Ok(batch) => {
                let fresh = crate::sim::new_since(&batch, cursor);
                if fresh.is_empty() {
                    tracing::debug!(group, "no new tickets this tick");
                    continue;
                }
                // TODO(task #892): idempotent board ingest via external-link (`sim:<id>`) into `project_id`,
                // UNASSIGNED — get-or-create the ticket task then update its mirrored state; dedup each
                // correspondence comment on its external id. For now the ingest write is the pending seam.
                let _ = (board, project_id);
                tracing::info!(
                    group,
                    new = fresh.len(),
                    "tickets to ingest (ingest write: task #892)"
                );
                if let Some(newest) = crate::sim::newest_timestamp(&batch)
                    && state.advance_group(group, newest)
                    && let Err(e) = state.save(&config.state_dir)
                {
                    tracing::warn!(error = %e, group, "cursor save failed — will re-poll (idempotent)");
                }
            }
            Err(e) => tracing::warn!(error = %e, group, "poll failed — retrying next tick"),
        }
    }
}

/// Fetch a group's tickets updated after `cursor` from the live ticketing source.
///
/// SEAM (task #891): the live ticketing read transport lands here — a watermark query sorted by
/// `lastUpdatedDate` against the cursor, mapping each result into a [`Ticket`]. Until then it returns an empty
/// batch (the bridge runs dormant) rather than fabricating tickets.
async fn poll_group(_group: &str, _cursor: Option<&str>) -> Result<Vec<Ticket>, String> {
    Ok(Vec::new())
}

/// Resolve on the first SIGINT or SIGTERM so the daemon drains and persists its cursor on a clean shutdown.
async fn shutdown_signal() {
    use tokio::signal::unix::{SignalKind, signal};
    let mut term = match signal(SignalKind::terminate()) {
        Ok(s) => s,
        Err(_) => {
            // No SIGTERM handler (unusual) — fall back to Ctrl-C only.
            let _ = tokio::signal::ctrl_c().await;
            return;
        }
    };
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = term.recv() => {}
    }
}
