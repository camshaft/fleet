//! The async poll-loop transport — the thin layer between the pure lib and the live GitHub + board REST I/O.
//! Kept out of `main.rs` so `main` reads as a wiring diagram. Not unit-tested (live network); every DECISION
//! it calls into (`sync::*`, `state::*`, the parsers) IS tested in the lib.
//!
//! ## Concurrency (operator directive: NO blocking IO in rust daemons)
//! All I/O is `async` on tokio — never a blocking call that pins a runtime thread. The two sync directions run
//! as INDEPENDENT concurrent loops under one [`tokio::join!`], sharing the async board + GitHub clients:
//! - **IN** ([`in_loop`]) polls each configured repo's issues/PRs and mirrors them to the board, then sleeps.
//! - **OUT** ([`out_loop`]) polls the board firehose and reflects authorized comments to GitHub, then sleeps.
//!
//! Because both futures are driven on the same task, OUT keeps flowing while IN awaits GitHub (and vice
//! versa): neither direction blocks the other, with no thread-per-direction. The shared cursor [`State`] lives
//! behind a [`Mutex`] locked only for the brief read/update around the network calls — the guard is NEVER held
//! across an `.await`, so the lock can't stall the runtime. The (rare, tiny) local state-file write is pushed
//! off the runtime via [`tokio::task::spawn_blocking`] so even that touch of fs IO never blocks a poll loop.
//! Per-repo IN is sequential-await for now (a handful of repos, well under GitHub's rate limit); bounded
//! per-repo concurrent fan-out is a non-breaking follow-up.
//!
//! ## Delivery semantics
//! - **IN (GitHub → board) is EXACTLY-ONCE.** `create_task`/`comment_task` (issues) and
//!   `create_review`/`append_review_log` (PRs → code reviews, BUILD 2a) carry the `external_link` and the
//!   board de-duplicates atomically on `(source, external_id)` (board-core #270 / Review entity #372),
//!   returning `created:false` / `appended:false` for an already-mirrored issue/comment/PR/log-entry. So a
//!   response-read-failed retry re-posts the same ref and the board returns the existing row instead of
//!   duplicating — no create→link race. PR review-status advances via the idempotent `set_review_status`
//!   (a same-status re-apply is a board-side no-op).
//! - **OUT (board → GitHub) is AT-LEAST-ONCE.** GitHub issue comments have no idempotency key, so a comment
//!   POST that succeeds while its response fails to read duplicates one comment when the reflect retries
//!   next tick. Inherent to the GitHub API; rare + non-fatal. The firehose cursor advances per
//!   terminally-handled event so nothing before the last success re-posts.

use github_bridge::board::{BoardClient, LINK_SOURCE, build_repo_project_map, parse_issue_ref};
use github_bridge::config::Config;
use github_bridge::{
    GithubClient, Issue, PER_PAGE, PrReviewStatus, State, github_external_author,
    latest_review_decision, plan_comment_ingest, plan_issue_ingest, plan_outbound,
    plan_pr_comment_log, plan_pr_finding_log, plan_pr_review_ingest, refine_open_status,
};
use std::collections::HashSet;
use std::future::Future;
use std::sync::Mutex;
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
/// error is logged and retried. Returns on SIGTERM/ctrl-c for a clean shutdown under a supervisor.
pub async fn run(cfg: Config) {
    let Some(token) = cfg.token().map(str::to_string) else {
        tracing::warn!("no github_token in config — idle until provided (a restart picks it up)");
        tokio::select! {
            _ = async { loop { tokio::time::sleep(DORMANT_INTERVAL).await; } } => {},
            _ = shutdown_signal() => tracing::info!("shutdown signal — exiting (was dormant)"),
        }
        return;
    };

    let board = BoardClient::new(&cfg.board_api);
    let gh = GithubClient::new(&cfg.api_base, &token);

    // Our own GitHub login, fetched once — lets IN comment ingest skip comments the bridge itself posted
    // (loop-safety). Best-effort: a GitHub App token may 403 on /user; then the self-filter is simply off
    // (dedup links still prevent re-posting).
    let self_login = match gh.viewer_login().await {
        Ok(l) => {
            tracing::info!(login = %l, "authenticated to GitHub");
            Some(l)
        }
        Err(e) => {
            tracing::warn!(error = %e, "could not resolve GitHub login — IN comment self-filter disabled");
            None
        }
    };

    // The cursor state is shared by IN (per-repo `?since=`) and OUT (firehose seq). The two directions touch
    // disjoint fields, but both persist the one file, so a Mutex keeps the on-disk snapshot consistent. Locked
    // only briefly around each step — never across an `.await` (see the module doc).
    let state = Mutex::new(State::load(&cfg.state_dir));

    // First run: initialize the firehose cursor at HEAD so OUT skips the board backlog. IN intentionally
    // leaves the per-repo cursors empty so each repo DOES ingest its existing issue backlog (idempotent).
    let needs_head_init = { state.lock().unwrap().firehose_seq.is_none() };
    if needs_head_init {
        let head = initialize_firehose_head(&board).await;
        let snapshot = {
            let mut s = state.lock().unwrap();
            s.firehose_seq = Some(head);
            s.clone()
        };
        persist(&cfg, &snapshot).await;
        tracing::info!(
            head,
            "initialized firehose cursor at head — skipping board backlog"
        );
    }

    tracing::info!(
        interval_secs = POLL_INTERVAL.as_secs(),
        "entering concurrent IN/OUT poll loops"
    );
    // IN and OUT run concurrently forever; `join!` drives both on this task so neither blocks the other. The
    // select lets a SIGTERM/ctrl-c win over the (never-returning) loops for a clean exit.
    tokio::select! {
        _ = async {
            tokio::join!(
                in_loop(&cfg, &gh, &board, self_login.as_deref(), &state),
                out_loop(&cfg, &gh, &board, &state),
            );
        } => {},
        _ = shutdown_signal() => tracing::info!("shutdown signal — exiting poll loops"),
    }
}

/// Resolve once a SIGTERM or ctrl-c arrives — the daemon's clean-shutdown trigger (so a supervisor's stop
/// isn't a hard kill mid-write). Unix-only, matching the deploy target.
async fn shutdown_signal() {
    use tokio::signal::unix::{SignalKind, signal};
    let mut term = match signal(SignalKind::terminate()) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(error = %e, "could not install SIGTERM handler — ctrl-c only");
            let _ = tokio::signal::ctrl_c().await;
            return;
        }
    };
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {},
        _ = term.recv() => {},
    }
}

/// The IN loop: each tick, resolve the repo->project targets from the LIVE board project map (so a newly
/// mapped repo/project is picked up without a restart), then scan each target repo (per-repo cursor),
/// sequentially — a handful of repos on the ~15s cadence stays well under GitHub's 5000/hr. A per-repo error
/// doesn't stop the others. Runs concurrently with [`out_loop`]; the awaits yield so OUT keeps flowing.
async fn in_loop(
    cfg: &Config,
    gh: &GithubClient,
    board: &BoardClient,
    self_login: Option<&str>,
    state: &Mutex<State>,
) {
    loop {
        for (repo, project_id) in resolve_targets(cfg, board).await {
            if let Err(e) = in_tick_repo(cfg, gh, board, &repo, project_id, self_login, state).await
            {
                tracing::warn!(error = %e, %repo, "IN tick error for repo (will retry next tick)");
            }
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

/// Resolve this pass's ingest targets from the LIVE board project map (each project's `metadata.repo`), so a
/// newly-mapped repo/project is picked up without a restart — nothing hardcoded. Falls back to the static
/// `project_id` config when the board exposes no usable mapping (empty result) or the projects fetch fails, so
/// the bridge still ingests against a pre-metadata board.
async fn resolve_targets(cfg: &Config, board: &BoardClient) -> Vec<(String, i64)> {
    match board.list_projects().await {
        Ok(projects) => {
            let targets = cfg.resolve_ingest_targets(&build_repo_project_map(&projects));
            if !targets.is_empty() {
                return targets;
            }
            let fallback = static_targets(cfg);
            if !fallback.is_empty() {
                tracing::warn!(
                    "no repo matched the board project map; using static project_id config this pass"
                );
            }
            fallback
        }
        Err(e) => {
            tracing::warn!(error = %e, "GET /projects failed; using static project_id config this pass");
            static_targets(cfg)
        }
    }
}

/// The legacy static ingest targets (one `project_id` for all configured repos), as owned tuples — the
/// fallback when the board project map is unavailable or empty.
fn static_targets(cfg: &Config) -> Vec<(String, i64)> {
    cfg.ingest_targets()
        .into_iter()
        .map(|(r, id)| (r.to_string(), id))
        .collect()
}

/// The OUT loop: each tick, drain the board firehose and reflect authorized comments to GitHub. Runs
/// concurrently with [`in_loop`].
async fn out_loop(cfg: &Config, gh: &GithubClient, board: &BoardClient, state: &Mutex<State>) {
    loop {
        if let Err(e) = out_tick(cfg, gh, board, state).await {
            tracing::warn!(error = %e, "OUT tick error (will retry next tick)");
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

/// Persist state best-effort, OFF the runtime: the (tiny, infrequent) local file write runs on a blocking
/// thread so it never stalls a poll loop. A write failure is logged, not fatal — a restart re-does the last
/// idempotent step at worst.
async fn persist(cfg: &Config, state: &State) {
    let dir = cfg.state_dir.clone();
    let snapshot = state.clone();
    match tokio::task::spawn_blocking(move || snapshot.save(&dir)).await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => tracing::warn!(error = %e, "failed to persist state"),
        Err(e) => tracing::warn!(error = %e, "state-persist task failed to join"),
    }
}

/// Walk the current firehose to its head (first empty page) without acting — the first-run OUT cursor.
async fn initialize_firehose_head(board: &BoardClient) -> i64 {
    let mut since = 0i64;
    loop {
        match board.poll_events(since, POLL_LIMIT).await {
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
/// `fetch` is an async page-getter — each page is awaited to completion before the next, so a borrowing
/// closure (e.g. `|page| gh.list_issues(repo, since, page)`) is fine.
async fn collect_pages<T, F, Fut>(mut fetch: F) -> Result<Vec<T>, String>
where
    F: FnMut(usize) -> Fut,
    Fut: Future<Output = Result<Vec<T>, String>>,
{
    let mut all = Vec::new();
    for page in 1..=MAX_PAGES {
        let batch = fetch(page).await?;
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
    issues
        .iter()
        .map(|i| i.updated_at.clone())
        .filter(|s| !s.is_empty())
        .max()
}

/// The bare GitHub login from a `github:<login>` external-author id (empty when absent — a ghost author).
fn author_login(external_author: Option<&str>) -> &str {
    external_author
        .and_then(|a| a.strip_prefix("github:"))
        .unwrap_or("")
}

/// Best-effort: attach a GitHub author's login as their board external-identity display name (so readers see
/// a clean name alongside the stable `github:<login>` key). Register-once per tick via `seen`.
async fn register_author(board: &BoardClient, login: &str, seen: &mut HashSet<String>) {
    if login.is_empty() || !seen.insert(login.to_string()) {
        return;
    }
    let id = github_external_author(login);
    if let Err(e) = board
        .upsert_external_identity(&id, LINK_SOURCE, login)
        .await
    {
        tracing::debug!(error = %e, %login, "external-identity upsert failed (non-fatal)");
    }
}

/// BUILD 2b: refine an OPEN PR's review status (draft → `open`; ready → `in_review` / `changes_requested`)
/// by fetching the Pulls API (the `draft` flag) + the Reviews API (the latest decisive verdict). Best-effort:
/// on any fetch error the status stays the base `Open` so a transient GitHub failure never wedges the tick or
/// regresses a review. Only called for PRs whose 2a base status is `Open` (closed PRs are already terminal).
async fn refine_pr_open_status(gh: &GithubClient, repo: &str, number: i64) -> PrReviewStatus {
    let draft = match gh.get_pull(repo, number).await {
        Ok(p) => p.draft,
        Err(e) => {
            tracing::debug!(error = %e, %repo, number, "2b: get_pull failed; leaving status open");
            return PrReviewStatus::Open;
        }
    };
    let reviews = collect_pages(|page| gh.list_pull_reviews(repo, number, page))
        .await
        .unwrap_or_default();
    refine_open_status(
        PrReviewStatus::Open,
        draft,
        latest_review_decision(&reviews),
    )
}

/// IN, for ONE repo: poll its issues + comments (the issues poll returns PRs too, `state=all`). Real issues
/// become attributed board tasks + attributed board comments; pull requests become board code reviews
/// (BUILD 2a/2b) — a `create_review` per PR, its status advanced (an open PR refined via the Pulls + Reviews
/// APIs into open/in_review/changes_requested, a merged PR → approved, a closed-unmerged PR → closed), its
/// conversation comments logged to the review, and its inline diff-review comments logged as findings (2b-2).
/// All idempotent via the board's external_links (#270 / Review entity #372). Advances this repo's cursor.
async fn in_tick_repo(
    cfg: &Config,
    gh: &GithubClient,
    board: &BoardClient,
    repo: &str,
    project_id: i64,
    self_login: Option<&str>,
    state: &Mutex<State>,
) -> Result<(), String> {
    let since = { state.lock().unwrap().since_for(repo).map(str::to_string) };
    let issues = collect_pages(|page| gh.list_issues(repo, since.as_deref(), page)).await?;
    if issues.is_empty() {
        return Ok(());
    }

    // One create per non-PR issue; the board de-duplicates on the issue link (#270) and returns the existing
    // task with created=false, so re-polling a known issue is a cheap no-op with no duplicate. The returned
    // task id is what its comments attach to — no separate link lookup.
    let mut seen_authors = HashSet::new();
    for tc in &plan_issue_ingest(&issues, repo).creates {
        let (task_id, created) = board
            .create_task(
                project_id,
                &tc.title,
                &tc.description,
                &cfg.bridge_agent,
                tc.external_author.as_deref(),
                &tc.issue_ref,
            )
            .await?;
        if created {
            tracing::info!(issue = %tc.issue_ref, task_id, "ingested GitHub issue → board task");
        }
        register_author(
            board,
            author_login(tc.external_author.as_deref()),
            &mut seen_authors,
        )
        .await;

        // Sync this issue's comments (the board de-dupes each on its comment link, #270).
        let comments = collect_pages(|page| {
            gh.list_issue_comments(repo, tc.issue_number, since.as_deref(), page)
        })
        .await?;
        for c in &comments {
            register_author(board, &c.author, &mut seen_authors).await;
        }
        for post in &plan_comment_ingest(&comments, repo, task_id, self_login).posts {
            let created_c = board
                .comment_task(
                    task_id,
                    &cfg.bridge_agent,
                    &post.body,
                    post.external_author.as_deref(),
                    &post.comment_ref,
                )
                .await?;
            if created_c {
                tracing::info!(comment = %post.comment_ref, task_id, "ingested GitHub comment → board comment");
            }
        }
    }

    // PR reviews (BUILD 2a): mirror each pull request as a board code review, advance its status, and log the
    // PR's conversation comments to the review. The issues poll already returned the PRs (`state=all`);
    // create_review + append_review_log are idempotent board-side (#372), so a re-poll is a cheap no-op.
    for rc in &plan_pr_review_ingest(&issues, repo).creates {
        // BUILD 2b: refine an OPEN PR into draft(→open)/in_review/changes_requested via the Pulls + Reviews
        // APIs. A closed PR's 2a status is already terminal (approved/closed), so skip the extra calls.
        let status = if rc.status == PrReviewStatus::Open {
            refine_pr_open_status(gh, repo, rc.pr_number).await
        } else {
            rc.status
        };
        let status = status.as_board_status();
        let (review_id, created) = board
            .create_review(
                project_id,
                "code",
                &rc.title,
                &rc.description,
                &cfg.bridge_agent,
                rc.external_author.as_deref(),
                status,
                &rc.external_id,
            )
            .await?;
        if created {
            tracing::info!(pr = %rc.external_id, review_id, status, "ingested GitHub PR → board code review");
        } else {
            // Existing review: advance its status (open → in_review/changes_requested/approved/closed);
            // idempotent no-op if unchanged.
            board.set_review_status(review_id, status).await?;
        }
        register_author(
            board,
            author_login(rc.external_author.as_deref()),
            &mut seen_authors,
        )
        .await;

        // Log this PR's conversation comments to the review (a PR IS an issue, so the same comments endpoint;
        // diff/review comments are BUILD 2b). The board de-dupes each on its entry link.
        let comments = collect_pages(|page| {
            gh.list_issue_comments(repo, rc.pr_number, since.as_deref(), page)
        })
        .await?;
        for c in &comments {
            register_author(board, &c.author, &mut seen_authors).await;
        }
        for entry in &plan_pr_comment_log(&comments, repo, self_login) {
            let appended = board
                .append_review_log(
                    review_id,
                    "comment",
                    &entry.body,
                    entry.external_author.as_deref(),
                    &entry.external_id,
                )
                .await?;
            if appended {
                tracing::info!(comment = %entry.external_id, review_id, "logged GitHub PR comment → review log");
            }
        }

        // Inline diff-review comments → finding-type review-log entries (BUILD 2b-2). Distinct endpoint +
        // ref namespace (owner/repo#rc<id>) from the conversation comments above; board de-dupes each.
        let findings = collect_pages(|page| {
            gh.list_pull_review_comments(repo, rc.pr_number, since.as_deref(), page)
        })
        .await?;
        for c in &findings {
            register_author(board, &c.author, &mut seen_authors).await;
        }
        for entry in &plan_pr_finding_log(&findings, repo, self_login) {
            let appended = board
                .append_review_log(
                    review_id,
                    "finding",
                    &entry.body,
                    entry.external_author.as_deref(),
                    &entry.external_id,
                )
                .await?;
            if appended {
                tracing::info!(finding = %entry.external_id, review_id, "logged GitHub PR review finding → review log");
            }
        }
    }

    // Advance this repo's IN cursor forward only (persist off-runtime only on a real advance).
    if let Some(newest) = newest_timestamp(&issues) {
        let snapshot = {
            let mut s = state.lock().unwrap();
            s.advance_repo(repo, &newest).then(|| s.clone())
        };
        if let Some(snapshot) = snapshot {
            persist(cfg, &snapshot).await;
        }
    }
    Ok(())
}

/// OUT: poll the board firehose and post each authorized `task.outbound_reflect` (source=github) as a comment
/// on the linked GitHub issue, advancing the persisted firehose cursor past terminally-handled events.
async fn out_tick(
    cfg: &Config,
    gh: &GithubClient,
    board: &BoardClient,
    state: &Mutex<State>,
) -> Result<(), String> {
    let cursor = { state.lock().unwrap().firehose_seq.unwrap_or(0) };
    let events = board.poll_events(cursor, POLL_LIMIT).await?;
    if events.is_empty() {
        return Ok(());
    }
    let (posts, batch_max) = plan_outbound(&events, cursor);
    for post in &posts {
        let Some((repo, number)) = parse_issue_ref(&post.external_id) else {
            tracing::warn!(external_id = %post.external_id, "OUT: reflect external_id is not an issue ref — skipping past it");
            advance_firehose(cfg, state, post.event_seq).await; // terminal — don't wedge the queue
            continue;
        };
        match gh.post_issue_comment(&repo, number, &post.body).await {
            Ok(id) => {
                tracing::info!(issue = %post.external_id, github_comment_id = id, board_comment_id = post.comment_id, "reflected board comment → GitHub issue");
                advance_firehose(cfg, state, post.event_seq).await;
            }
            Err(e) => {
                // Leave the cursor at the last success so this reflect (+ the rest) retries next tick.
                tracing::warn!(error = %e, issue = %post.external_id, "OUT: GitHub comment post failed — retry next tick");
                return Ok(());
            }
        }
    }
    // All posts terminally handled — advance past any trailing non-reflect / other-source events too.
    let trailing = {
        let mut s = state.lock().unwrap();
        (batch_max > s.firehose_seq.unwrap_or(0)).then(|| {
            s.firehose_seq = Some(batch_max);
            s.clone()
        })
    };
    if let Some(snapshot) = trailing {
        persist(cfg, &snapshot).await;
    }
    Ok(())
}

/// Advance the firehose cursor to `seq` (forward is implicit — OUT handles events in seq order) and persist
/// the snapshot off-runtime. Factored out so the two OUT advance sites don't hold the lock across the write.
async fn advance_firehose(cfg: &Config, state: &Mutex<State>, seq: i64) {
    let snapshot = {
        let mut s = state.lock().unwrap();
        s.firehose_seq = Some(seq);
        s.clone()
    };
    persist(cfg, &snapshot).await;
}
