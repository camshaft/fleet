//! The blocking poll-loop transport — the thin layer between the pure lib and the live GitHub + board REST
//! I/O. Kept out of `main.rs` so `main` reads as a wiring diagram. Not unit-tested (live network); every
//! DECISION it calls into (`sync::*`, `state::*`, the parsers) IS tested in the lib.
//!
//! ## Delivery semantics — AT-LEAST-ONCE (with a narrow, inherent duplicate window)
//! Both directions dedup on the board's durable `external_links` (issue↔task, synced-comment refs) plus a
//! persisted cursor, so the STEADY state is exactly-once: a re-poll of a known issue/comment is a no-op, and
//! the firehose cursor advances per terminally-handled event. The one residual window is a write that
//! SUCCEEDS on the remote but whose RESPONSE we fail to read (a network blip after GitHub/board committed):
//! we then retry and duplicate — a second GitHub comment (OUT), or a second board task/comment (IN, worse).
//! This cannot be closed adapter-side: GitHub issue comments have no idempotency key, and `create_task`
//! records its issue↔task link only AFTER it returns, so a create-succeeded-read-failed re-creates. The
//! real fix is a board-core idempotent "create/comment keyed on the external link" primitive (routed to
//! v-task-board); until then the bridge is at-least-once and this window is accepted as rare + non-fatal.

use github_bridge::board::{parse_issue_ref, BoardClient, LINK_SOURCE};
use github_bridge::config::Config;
use github_bridge::{
    github_external_author, issue_ref, plan_comment_ingest, plan_issue_ingest, plan_outbound, GithubClient,
    Issue, State, PER_PAGE,
};
use std::collections::{HashMap, HashSet};
use std::thread;
use std::time::Duration;

/// The poll cadence. GitHub's authenticated rate limit is 5000 req/hr; a 15s loop over a handful of pages is
/// comfortably under it while keeping the board↔GitHub round-trip snappy.
const POLL_INTERVAL: Duration = Duration::from_secs(15);
/// The sleep between checks while dormant (no token) — long, since only a restart picks up a new token.
const DORMANT_INTERVAL: Duration = Duration::from_secs(3600);
/// How many firehose events to pull per board poll.
const POLL_LIMIT: usize = 100;
/// A safety cap on pagination so a pathological repo can't spin forever in one tick.
const MAX_PAGES: usize = 100;

/// Run the daemon forever. Fail-soft: with no token the process idles (a restart picks one up); any per-tick
/// error is logged and retried.
pub fn run(cfg: Config) {
    let Some(token) = cfg.token().map(str::to_string) else {
        tracing::warn!("no github_token in config — idle until provided (a restart picks it up)");
        loop {
            thread::sleep(DORMANT_INTERVAL);
        }
    };

    let board = BoardClient::new(&cfg.board_api);
    let gh = GithubClient::new(&cfg.api_base, &token);

    // Our own GitHub login, fetched once — lets IN comment ingest skip comments the bridge itself posted
    // (loop-safety). Best-effort: a GitHub App token may 403 on /user; then the self-filter is simply off
    // (dedup links still prevent re-posting).
    let self_login = match gh.viewer_login() {
        Ok(l) => {
            tracing::info!(login = %l, "authenticated to GitHub");
            Some(l)
        }
        Err(e) => {
            tracing::warn!(error = %e, "could not resolve GitHub login — IN comment self-filter disabled");
            None
        }
    };

    let mut state = State::load(&cfg.state_dir);

    // First run: initialize the firehose cursor at HEAD so OUT skips the board backlog. IN intentionally
    // leaves the per-repo cursors empty so each repo DOES ingest its existing issue backlog (idempotent).
    if state.firehose_seq.is_none() {
        let head = initialize_firehose_head(&board);
        state.firehose_seq = Some(head);
        persist(&cfg, &state);
        tracing::info!(head, "initialized firehose cursor at head — skipping board backlog");
    }

    tracing::info!(interval_secs = POLL_INTERVAL.as_secs(), "entering poll loop");
    loop {
        // IN: each configured repo scans independently (per-repo cursor), sequentially — a handful of repos
        // on the ~15s cadence stays well under GitHub's 5000/hr. A per-repo error doesn't stop the others.
        for (repo, project_id) in cfg.ingest_targets() {
            if let Err(e) =
                in_tick_repo(&cfg, &gh, &board, repo, project_id, self_login.as_deref(), &mut state)
            {
                tracing::warn!(error = %e, %repo, "IN tick error for repo (will retry next tick)");
            }
        }
        if let Err(e) = out_tick(&cfg, &gh, &board, &mut state) {
            tracing::warn!(error = %e, "OUT tick error (will retry next tick)");
        }
        thread::sleep(POLL_INTERVAL);
    }
}

/// Persist state best-effort (a write failure is logged, not fatal — a restart re-does the last idempotent
/// step at worst).
fn persist(cfg: &Config, state: &State) {
    if let Err(e) = state.save(&cfg.state_dir) {
        tracing::warn!(error = %e, "failed to persist state");
    }
}

/// Walk the current firehose to its head (first empty page) without acting — the first-run OUT cursor.
fn initialize_firehose_head(board: &BoardClient) -> i64 {
    let mut since = 0i64;
    loop {
        match board.poll_events(since, POLL_LIMIT) {
            Ok(evs) if evs.is_empty() => return since,
            // `since_seq` is exclusive, so a non-empty page always has max > since → this terminates.
            Ok(evs) => since = evs.iter().map(|e| e.seq).max().unwrap_or(since),
            Err(e) => {
                tracing::warn!(error = %e, "firehose head-init poll failed; starting at 0");
                return since;
            }
        }
    }
}

/// Collect all pages of a paginated GitHub list (stops at the first short page, capped at [`MAX_PAGES`]).
fn collect_pages<T>(mut fetch: impl FnMut(usize) -> Result<Vec<T>, String>) -> Result<Vec<T>, String> {
    let mut all = Vec::new();
    for page in 1..=MAX_PAGES {
        let batch = fetch(page)?;
        let full = batch.len() >= PER_PAGE;
        all.extend(batch);
        if !full {
            return Ok(all);
        }
    }
    tracing::warn!("pagination hit the {MAX_PAGES}-page cap — truncating this tick");
    Ok(all)
}

/// The newest `updated_at` across a batch (RFC3339 sorts lexicographically), ignoring empties — the next IN
/// cursor. `None` when the batch has no usable timestamp.
fn newest_timestamp(issues: &[Issue]) -> Option<String> {
    issues.iter().map(|i| i.updated_at.clone()).filter(|s| !s.is_empty()).max()
}

/// Best-effort: attach a GitHub author's login as their board external-identity display name (so readers see
/// a clean name alongside the stable `github:<login>` key). Register-once per tick via `seen`.
fn register_author(board: &BoardClient, login: &str, seen: &mut HashSet<String>) {
    if login.is_empty() || !seen.insert(login.to_string()) {
        return;
    }
    let id = github_external_author(login);
    if let Err(e) = board.upsert_external_identity(&id, LINK_SOURCE, login) {
        tracing::debug!(error = %e, %login, "external-identity upsert failed (non-fatal)");
    }
}

/// IN, for ONE repo: poll its issues + comments, create attributed tasks for new issues and attributed board
/// comments for new comments (idempotent via the board's external_links). Advances this repo's cursor.
fn in_tick_repo(
    cfg: &Config,
    gh: &GithubClient,
    board: &BoardClient,
    repo: &str,
    project_id: i64,
    self_login: Option<&str>,
    state: &mut State,
) -> Result<(), String> {
    let since = state.since_for(repo).map(str::to_string);
    let issues = collect_pages(|page| gh.list_issues(repo, since.as_deref(), page))?;
    if issues.is_empty() {
        return Ok(());
    }

    // Create tasks for new (non-PR, unlinked) issues.
    let plan = plan_issue_ingest(&issues, repo, &board.list_issue_links()?);
    for tc in &plan.creates {
        let task_id = board.create_task(
            project_id,
            &tc.title,
            &tc.description,
            &cfg.bridge_agent,
            tc.external_author.as_deref(),
        )?;
        board.register_issue_link(task_id, &tc.issue_ref)?;
        tracing::info!(issue = %tc.issue_ref, task_id, "ingested GitHub issue → board task");
    }

    // Sync comments on the polled issues (links now include any task just created).
    let task_of: HashMap<String, i64> =
        board.list_issue_links()?.into_iter().map(|l| (l.issue_ref, l.board_task_id)).collect();
    let mut synced = board.list_comment_refs()?;
    let mut seen_authors = HashSet::new();
    for issue in &issues {
        if issue.is_pull_request {
            continue;
        }
        register_author(board, &issue.author, &mut seen_authors);
        let iref = issue_ref(repo, issue.number);
        let Some(&task_id) = task_of.get(&iref) else {
            tracing::warn!(issue = %iref, "issue has no linked task yet — syncing its comments next tick");
            continue;
        };
        let comments = collect_pages(|page| gh.list_issue_comments(repo, issue.number, since.as_deref(), page))?;
        if comments.is_empty() {
            continue;
        }
        for c in &comments {
            register_author(board, &c.author, &mut seen_authors);
        }
        let cplan = plan_comment_ingest(&comments, repo, task_id, self_login, &synced);
        for post in &cplan.posts {
            board.comment_task(task_id, &cfg.bridge_agent, &post.body, post.external_author.as_deref())?;
            board.register_comment_link(task_id, &post.comment_ref)?;
            synced.insert(post.comment_ref.clone()); // dedup within this same tick too
            tracing::info!(comment = %post.comment_ref, task_id, "ingested GitHub comment → board comment");
        }
    }

    // Advance this repo's IN cursor forward only.
    if let Some(newest) = newest_timestamp(&issues)
        && state.advance_repo(repo, &newest)
    {
        persist(cfg, state);
    }
    Ok(())
}

/// OUT: poll the board firehose and post each authorized `task.outbound_reflect` (source=github) as a comment
/// on the linked GitHub issue, advancing the persisted firehose cursor past terminally-handled events.
fn out_tick(cfg: &Config, gh: &GithubClient, board: &BoardClient, state: &mut State) -> Result<(), String> {
    let cursor = state.firehose_seq.unwrap_or(0);
    let events = board.poll_events(cursor, POLL_LIMIT)?;
    if events.is_empty() {
        return Ok(());
    }
    let (posts, batch_max) = plan_outbound(&events, cursor);
    for post in &posts {
        let Some((repo, number)) = parse_issue_ref(&post.external_id) else {
            tracing::warn!(external_id = %post.external_id, "OUT: reflect external_id is not an issue ref — skipping past it");
            state.firehose_seq = Some(post.event_seq); // terminal — don't wedge the queue
            persist(cfg, state);
            continue;
        };
        match gh.post_issue_comment(&repo, number, &post.body) {
            Ok(id) => {
                tracing::info!(issue = %post.external_id, github_comment_id = id, board_comment_id = post.comment_id, "reflected board comment → GitHub issue");
                state.firehose_seq = Some(post.event_seq);
                persist(cfg, state);
            }
            Err(e) => {
                // Leave the cursor at the last success so this reflect (+ the rest) retries next tick.
                tracing::warn!(error = %e, issue = %post.external_id, "OUT: GitHub comment post failed — retry next tick");
                return Ok(());
            }
        }
    }
    // All posts terminally handled — advance past any trailing non-reflect / other-source events too.
    if batch_max > state.firehose_seq.unwrap_or(0) {
        state.firehose_seq = Some(batch_max);
        persist(cfg, state);
    }
    Ok(())
}
