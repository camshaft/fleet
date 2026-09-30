//! The blocking poll-loop transport — the thin layer between the pure lib and the live GitHub + board REST
//! I/O. Kept out of `main.rs` so `main` reads as a wiring diagram. Not unit-tested (live network); every
//! DECISION it calls into (`sync::*`, `state::*`, the parsers) IS tested in the lib.
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

use github_bridge::board::{parse_issue_ref, BoardClient, LINK_SOURCE};
use github_bridge::config::Config;
use github_bridge::{
    github_external_author, latest_review_decision, plan_comment_ingest, plan_issue_ingest, plan_outbound,
    plan_pr_comment_log, plan_pr_finding_log, plan_pr_review_ingest, refine_open_status, GithubClient, Issue,
    PrReviewStatus, State, PER_PAGE,
};
use std::collections::HashSet;
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

/// The bare GitHub login from a `github:<login>` external-author id (empty when absent — a ghost author).
fn author_login(external_author: Option<&str>) -> &str {
    external_author.and_then(|a| a.strip_prefix("github:")).unwrap_or("")
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

/// BUILD 2b: refine an OPEN PR's review status (draft → `open`; ready → `in_review` / `changes_requested`)
/// by fetching the Pulls API (the `draft` flag) + the Reviews API (the latest decisive verdict). Best-effort:
/// on any fetch error the status stays the base `Open` so a transient GitHub failure never wedges the tick or
/// regresses a review. Only called for PRs whose 2a base status is `Open` (closed PRs are already terminal).
fn refine_pr_open_status(gh: &GithubClient, repo: &str, number: i64) -> PrReviewStatus {
    let draft = match gh.get_pull(repo, number) {
        Ok(p) => p.draft,
        Err(e) => {
            tracing::debug!(error = %e, %repo, number, "2b: get_pull failed; leaving status open");
            return PrReviewStatus::Open;
        }
    };
    let reviews = collect_pages(|page| gh.list_pull_reviews(repo, number, page)).unwrap_or_default();
    refine_open_status(PrReviewStatus::Open, draft, latest_review_decision(&reviews))
}

/// IN, for ONE repo: poll its issues + comments (the issues poll returns PRs too, `state=all`). Real issues
/// become attributed board tasks + attributed board comments; pull requests become board code reviews
/// (BUILD 2a/2b) — a `create_review` per PR, its status advanced (an open PR refined via the Pulls + Reviews
/// APIs into open/in_review/changes_requested, a merged PR → approved, a closed-unmerged PR → closed), its
/// conversation comments logged to the review, and its inline diff-review comments logged as findings (2b-2).
/// All idempotent via the board's external_links (#270 / Review entity #372). Advances this repo's cursor.
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

    // One create per non-PR issue; the board de-duplicates on the issue link (#270) and returns the existing
    // task with created=false, so re-polling a known issue is a cheap no-op with no duplicate. The returned
    // task id is what its comments attach to — no separate link lookup.
    let mut seen_authors = HashSet::new();
    for tc in &plan_issue_ingest(&issues, repo).creates {
        let (task_id, created) = board.create_task(
            project_id,
            &tc.title,
            &tc.description,
            &cfg.bridge_agent,
            tc.external_author.as_deref(),
            &tc.issue_ref,
        )?;
        if created {
            tracing::info!(issue = %tc.issue_ref, task_id, "ingested GitHub issue → board task");
        }
        register_author(board, author_login(tc.external_author.as_deref()), &mut seen_authors);

        // Sync this issue's comments (the board de-dupes each on its comment link, #270).
        let comments =
            collect_pages(|page| gh.list_issue_comments(repo, tc.issue_number, since.as_deref(), page))?;
        for c in &comments {
            register_author(board, &c.author, &mut seen_authors);
        }
        for post in &plan_comment_ingest(&comments, repo, task_id, self_login).posts {
            let created_c = board.comment_task(
                task_id,
                &cfg.bridge_agent,
                &post.body,
                post.external_author.as_deref(),
                &post.comment_ref,
            )?;
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
            refine_pr_open_status(gh, repo, rc.pr_number)
        } else {
            rc.status
        };
        let status = status.as_board_status();
        let (review_id, created) = board.create_review(
            project_id,
            "code",
            &rc.title,
            &rc.description,
            &cfg.bridge_agent,
            rc.external_author.as_deref(),
            status,
            &rc.external_id,
        )?;
        if created {
            tracing::info!(pr = %rc.external_id, review_id, status, "ingested GitHub PR → board code review");
        } else {
            // Existing review: advance its status (open → in_review/changes_requested/approved/closed);
            // idempotent no-op if unchanged.
            board.set_review_status(review_id, status)?;
        }
        register_author(board, author_login(rc.external_author.as_deref()), &mut seen_authors);

        // Log this PR's conversation comments to the review (a PR IS an issue, so the same comments endpoint;
        // diff/review comments are BUILD 2b). The board de-dupes each on its entry link.
        let comments =
            collect_pages(|page| gh.list_issue_comments(repo, rc.pr_number, since.as_deref(), page))?;
        for c in &comments {
            register_author(board, &c.author, &mut seen_authors);
        }
        for entry in &plan_pr_comment_log(&comments, repo, self_login) {
            let appended = board.append_review_log(
                review_id,
                "comment",
                &entry.body,
                entry.external_author.as_deref(),
                &entry.external_id,
            )?;
            if appended {
                tracing::info!(comment = %entry.external_id, review_id, "logged GitHub PR comment → review log");
            }
        }

        // Inline diff-review comments → finding-type review-log entries (BUILD 2b-2). Distinct endpoint +
        // ref namespace (owner/repo#rc<id>) from the conversation comments above; board de-dupes each.
        let findings =
            collect_pages(|page| gh.list_pull_review_comments(repo, rc.pr_number, since.as_deref(), page))?;
        for c in &findings {
            register_author(board, &c.author, &mut seen_authors);
        }
        for entry in &plan_pr_finding_log(&findings, repo, self_login) {
            let appended = board.append_review_log(
                review_id,
                "finding",
                &entry.body,
                entry.external_author.as_deref(),
                &entry.external_id,
            )?;
            if appended {
                tracing::info!(finding = %entry.external_id, review_id, "logged GitHub PR review finding → review log");
            }
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
