//! `sync` — the pure ingest PLANNING, independent of the live GitHub REST + board HTTP transports.
//!
//! This is where the IN-direction (GitHub → board) *decisions* live so the daemon poll loop (a later slice)
//! stays a thin shell that just does I/O:
//!   - [`plan_issue_ingest`]: given a batch of GitHub [`Issue`]s + the issue↔task links already recorded on
//!     the board, produce the mirrored board tasks to CREATE — skipping pull requests and any issue already
//!     linked (idempotent: a re-poll of the same issue is a no-op).
//!   - [`plan_comment_ingest`]: given an issue's GitHub [`IssueComment`]s + the comment refs already synced +
//!     the bridge's own GitHub login, produce the attributed board comments to POST — skipping comments the
//!     bridge itself authored (loop-safety: an OUT-reflected comment on GitHub must not re-ingest) and any
//!     comment already synced.
//!
//! Attribution (board-core #149): a GitHub author becomes `external_author = github:<login>`, so board
//! readers see WHO wrote it, not the bridge. A ghost (deleted) author has no login, so it stays unattributed
//! (the bridge is the sole `author`) rather than fabricating an identity.
//!
//! Everything here is pure + unit-tested; the daemon feeds it what it read and executes what it returns via
//! the idempotent `board::create_task` / `board::comment_task` (each carrying the `external_link`, board-core
//! #270), so dedup is the board's job and this layer never tracks already-ingested state itself.

use crate::board::{comment_ref, issue_ref, Event, TaskReflect, LINK_SOURCE};
use crate::github::{github_external_author, Issue, IssueComment};

/// A mirrored board task to create from a GitHub issue (the daemon calls the idempotent `board::create_task`
/// passing [`TaskCreate::issue_ref`] as the `external_link`, so the board de-dupes + links atomically).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskCreate {
    /// The idempotency/link key (`owner/repo#number`) passed as the create's `external_link`.
    pub issue_ref: String,
    /// The GitHub issue number (for logging / the back-reference).
    pub issue_number: i64,
    /// The board task title (the issue title).
    pub title: String,
    /// The board task description (issue body + a GitHub back-reference footer).
    pub description: String,
    /// The attributed GitHub author (`github:<login>`), or `None` for a ghost (deleted) account.
    pub external_author: Option<String>,
}

/// The plan for a batch of ingested issues: the tasks to create (in input order).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct IssueIngestPlan {
    pub creates: Vec<TaskCreate>,
}

/// An attributed board comment to post on a mirrored task (the daemon calls `board::comment_task` then
/// records a `board_kind="comment"` link on [`CommentPost::comment_ref`] so it isn't re-posted).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommentPost {
    /// The board task the comment lands on.
    pub board_task_id: i64,
    /// The comment body (verbatim GitHub Markdown).
    pub body: String,
    /// The attributed GitHub author (`github:<login>`), or `None` for a ghost account.
    pub external_author: Option<String>,
    /// The dedup/link key (`owner/repo#c<id>`) recorded after the post.
    pub comment_ref: String,
}

/// The plan for a batch of ingested comments on one issue: the attributed board comments to post.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CommentIngestPlan {
    pub posts: Vec<CommentPost>,
}

/// Render the board task description for a mirrored issue: the issue body, then a footer back-referencing the
/// GitHub issue (number, author, state, URL) so a board reader can trace it to source. Pure.
pub fn render_task_description(repo: &str, issue: &Issue) -> String {
    let author = if issue.author.is_empty() { "(unknown)" } else { issue.author.as_str() };
    let body = if issue.body.is_empty() { "_(no description)_" } else { issue.body.as_str() };
    format!(
        "{body}\n\n---\nMirrored from GitHub {repo}#{number} · by @{author} · state: {state}\n{url}",
        number = issue.number,
        state = issue.state,
        url = issue.html_url,
    )
}

/// The attributed external author for a GitHub login, or `None` when the login is empty (a ghost account —
/// left unattributed rather than fabricating a `github:` identity for a nonexistent user).
fn attribution(login: &str) -> Option<String> {
    (!login.is_empty()).then(|| github_external_author(login))
}

/// Plan the board tasks to create from a batch of GitHub issues.
///
/// - Pull requests are skipped (the issues endpoint returns them; they are not board tasks).
/// - Every remaining (non-PR) issue becomes a [`TaskCreate`]; de-duplication against already-ingested issues
///   is the BOARD's job now (`board::create_task` is idempotent on the issue link, board-core #270), so this
///   planner no longer needs the existing links — a `created:false` response is the "already there" signal.
/// - Order is preserved so the daemon creates oldest-updated first (matching the `?sort=updated&asc` poll).
pub fn plan_issue_ingest(issues: &[Issue], repo: &str) -> IssueIngestPlan {
    let mut creates = Vec::new();
    for issue in issues {
        if issue.is_pull_request {
            continue;
        }
        creates.push(TaskCreate {
            title: issue.title.clone(),
            description: render_task_description(repo, issue),
            external_author: attribution(&issue.author),
            issue_number: issue.number,
            issue_ref: issue_ref(repo, issue.number),
        });
    }
    IssueIngestPlan { creates }
}

/// Plan the attributed board comments to post from a batch of an issue's GitHub comments.
///
/// - A comment authored by the bridge's own GitHub account (`self_login`) is skipped — loop-safety, so a
///   comment the bridge reflected OUT to GitHub isn't re-ingested back IN as a *new* board comment. (The
///   board side is separately loop-safe via the bridge's `author` ∉ `outbound_authors`; this guards the
///   GitHub side, which link-dedup alone can't — a reflected comment is a genuinely new GitHub comment id.)
/// - De-duplication of already-synced comments is the BOARD's job (`board::comment_task` is idempotent on the
///   comment link, board-core #270 — a `created:false` response means "already synced").
/// - Order is preserved (oldest-updated first).
///
/// Pure: `self_login` is the bridge's GitHub login (`None` when unknown — then nothing is filtered as self).
pub fn plan_comment_ingest(
    comments: &[IssueComment],
    repo: &str,
    board_task_id: i64,
    self_login: Option<&str>,
) -> CommentIngestPlan {
    let mut posts = Vec::new();
    for c in comments {
        if let Some(me) = self_login
            && !c.author.is_empty()
            && c.author == me
        {
            continue; // loop-safety: don't re-ingest our own reflected comment
        }
        posts.push(CommentPost {
            board_task_id,
            body: c.body.clone(),
            external_author: attribution(&c.author),
            comment_ref: comment_ref(repo, c.id),
        });
    }
    CommentIngestPlan { posts }
}

// ── PR → review (BUILD 2a): map a GitHub pull request to a board code-review status ────────────────────

/// The concluding review status for a GitHub pull request, per BUILD 2a of the Review-entity design
/// (board Doc #5): a code review mirrors a PR with the three terminal-ish states only — intermediate
/// states (draft / in-review / changes-requested) are BUILD 2b via the Pulls + Reviews APIs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrReviewStatus {
    /// The PR is still open (no verdict yet).
    Open,
    /// The PR was merged — the code review concluded `approved`.
    Approved,
    /// The PR was closed without merging — the code review concluded `closed`.
    Closed,
}

impl PrReviewStatus {
    /// The board review-status string. This is the status the adapter passes to the board's `create_review`
    /// (co-designed on task #373); a stable spelling of each state so BUILD 1's status enum and the adapter
    /// agree.
    pub fn as_board_status(self) -> &'static str {
        match self {
            PrReviewStatus::Open => "open",
            PrReviewStatus::Approved => "approved",
            PrReviewStatus::Closed => "closed",
        }
    }
}

/// Map a GitHub PR row (an [`Issue`] flagged [`Issue::is_pull_request`]) to its BUILD-2a review status.
///
/// A merged PR is always also `state:"closed"`, so `merged_at` is checked FIRST: a non-empty
/// `pr_merged_at` ⇒ [`PrReviewStatus::Approved`]; otherwise a `state:"closed"` PR is
/// [`PrReviewStatus::Closed`] (closed-unmerged); anything else (open) is [`PrReviewStatus::Open`]. Pure —
/// no extra GitHub call, since `merged_at` rides the issues-list `pull_request` object (2a: no new endpoints).
pub fn pr_review_status(issue: &Issue) -> PrReviewStatus {
    if issue.pr_merged_at.as_deref().is_some_and(|s| !s.is_empty()) {
        PrReviewStatus::Approved
    } else if issue.state == "closed" {
        PrReviewStatus::Closed
    } else {
        PrReviewStatus::Open
    }
}

// ── OUT direction (board → GitHub): reflect an authorized task comment onto its linked issue ──────────

/// A resolved OUT action: post [`body`](OutboundComment::body) as a comment on the GitHub issue identified
/// by [`external_id`](OutboundComment::external_id). Produced from a `task.outbound_reflect` firehose event
/// (board-core #264) that the board already authorized — the bridge posts it, no re-checking.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutboundComment {
    /// The board comment id this came from (dedup / idempotency on the OUT side).
    pub comment_id: i64,
    /// The GitHub issue ref to comment on (`owner/repo#number`; `board::parse_issue_ref` splits it).
    pub external_id: String,
    /// The external parent, when the target is nested (often absent).
    pub external_parent_id: Option<String>,
    /// The rendered GitHub comment body (original comment + a fleet-board attribution line).
    pub body: String,
    /// The firehose event `seq` — the daemon advances its persisted cursor to this after the post lands.
    pub event_seq: i64,
}

/// Render the GitHub comment body for a reflected board comment. The GitHub comment is posted by the
/// bridge's own bot account, so the ORIGINAL board author is attributed inline (else every reflected comment
/// would look like it came from the bot). Prefers the human external-author name when the board comment was
/// itself attributed to one; falls back to the board agent id. Pure.
pub fn render_outbound_github_comment(reflect: &TaskReflect) -> String {
    let who = reflect.external_author.as_deref().unwrap_or(reflect.author.as_str());
    format!("_↩ reflected from the fleet board — {who}_\n\n{}", reflect.body)
}

/// Plan the GitHub comments to post from a batch of firehose events.
///
/// - Only `task.outbound_reflect` events (board-core #264) whose `source` is ours ([`LINK_SOURCE`]) become
///   actions; everything else is skipped. Per #264 the event's existence IS the authorization (the board
///   already applied the per-link `{direction, outbound_authors}` policy), so no re-checking here.
/// - The new cursor is the max `seq` across ALL events in the batch (even skipped ones), never below
///   `cursor`, so a skipped/unrelated event is not reprocessed on the next poll.
///
/// Pure — the daemon posts each action via the GitHub client and records a link so a re-emit is idempotent.
pub fn plan_outbound(events: &[Event], cursor: i64) -> (Vec<OutboundComment>, i64) {
    let mut out = Vec::new();
    let mut new_cursor = cursor;
    for ev in events {
        if ev.seq > new_cursor {
            new_cursor = ev.seq;
        }
        if let Some(r) = ev.as_task_reflect()
            && r.source == LINK_SOURCE
        {
            out.push(OutboundComment {
                comment_id: r.comment_id,
                external_id: r.external_id.clone(),
                external_parent_id: r.external_parent_id.clone(),
                body: render_outbound_github_comment(&r),
                event_seq: ev.seq,
            });
        }
    }
    (out, new_cursor)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn issue(number: i64, title: &str, author: &str) -> Issue {
        Issue {
            number,
            title: title.to_string(),
            body: "body text".to_string(),
            state: "open".to_string(),
            author: author.to_string(),
            updated_at: "2026-09-29T10:00:00Z".to_string(),
            html_url: format!("https://github.com/o/r/issues/{number}"),
            is_pull_request: false,
            pr_merged_at: None,
        }
    }

    fn comment(id: i64, body: &str, author: &str) -> IssueComment {
        IssueComment {
            id,
            body: body.to_string(),
            author: author.to_string(),
            updated_at: "2026-09-29T11:00:00Z".to_string(),
            html_url: format!("https://github.com/o/r/issues/1#c{id}"),
        }
    }

    // ── plan_issue_ingest (dedup is now board-side, #270 — planner emits a create per non-PR issue) ──

    #[test]
    fn ingest_creates_a_task_per_issue_attributed() {
        let issues = [issue(42, "Fix widget", "octocat"), issue(43, "Add gizmo", "hubot")];
        let plan = plan_issue_ingest(&issues, "o/r");
        assert_eq!(plan.creates.len(), 2);
        assert_eq!(plan.creates[0].issue_ref, "o/r#42");
        assert_eq!(plan.creates[0].issue_number, 42);
        assert_eq!(plan.creates[0].title, "Fix widget");
        assert_eq!(plan.creates[0].external_author.as_deref(), Some("github:octocat"));
        assert!(plan.creates[0].description.contains("body text"));
        assert!(plan.creates[0].description.contains("o/r#42"), "back-reference footer present");
        assert_eq!(plan.creates[1].external_author.as_deref(), Some("github:hubot"));
    }

    #[test]
    fn ingest_skips_pull_requests() {
        let mut pr = issue(7, "a PR", "dev");
        pr.is_pull_request = true;
        let plan = plan_issue_ingest(&[pr, issue(8, "real", "dev")], "o/r");
        assert_eq!(plan.creates.len(), 1, "only the real issue is mirrored");
        assert_eq!(plan.creates[0].issue_number, 8);
    }

    #[test]
    fn ingest_ghost_author_is_unattributed() {
        let plan = plan_issue_ingest(&[issue(9, "ghosted", "")], "o/r");
        assert_eq!(plan.creates[0].external_author, None, "no fabricated identity for a ghost");
    }

    #[test]
    fn render_description_handles_empty_body() {
        let mut i = issue(5, "t", "u");
        i.body = String::new();
        let d = render_task_description("o/r", &i);
        assert!(d.contains("_(no description)_"));
        assert!(d.contains("by @u"));
        assert!(d.contains("state: open"));
    }

    #[test]
    fn render_description_marks_unknown_author() {
        let i = issue(5, "t", "");
        assert!(render_task_description("o/r", &i).contains("by @(unknown)"));
    }

    // ── plan_comment_ingest ──────────────────────────────────────────────────────────────────────

    #[test]
    fn comment_ingest_posts_attributed_comments() {
        let comments = [comment(1, "first", "octocat"), comment(2, "second", "hubot")];
        let plan = plan_comment_ingest(&comments, "o/r", 100, Some("fleet-bot"));
        assert_eq!(plan.posts.len(), 2);
        assert_eq!(plan.posts[0].board_task_id, 100);
        assert_eq!(plan.posts[0].body, "first");
        assert_eq!(plan.posts[0].external_author.as_deref(), Some("github:octocat"));
        assert_eq!(plan.posts[0].comment_ref, "o/r#c1");
        assert_eq!(plan.posts[1].comment_ref, "o/r#c2");
    }

    #[test]
    fn comment_ingest_skips_the_bridges_own_comments_loop_safety() {
        // A comment authored by our own GitHub account is a reflected-OUT comment coming back — skip it.
        // (Board link-dedup can't catch this: it's a genuinely new GitHub comment id.)
        let comments = [comment(1, "reflected", "fleet-bot"), comment(2, "human", "octocat")];
        let plan = plan_comment_ingest(&comments, "o/r", 100, Some("fleet-bot"));
        assert_eq!(plan.posts.len(), 1, "own comment filtered");
        assert_eq!(plan.posts[0].external_author.as_deref(), Some("github:octocat"));
    }

    #[test]
    fn comment_ingest_none_self_login_filters_nothing_as_self() {
        let comments = [comment(1, "x", "anyone")];
        let plan = plan_comment_ingest(&comments, "o/r", 100, None);
        assert_eq!(plan.posts.len(), 1);
    }

    #[test]
    fn comment_ingest_ghost_author_is_unattributed_and_not_self_filtered() {
        // An empty-login (ghost) comment must not be mistaken for the bridge and must post unattributed.
        let comments = [comment(1, "ghost note", "")];
        let plan = plan_comment_ingest(&comments, "o/r", 100, Some(""));
        assert_eq!(plan.posts.len(), 1, "empty author != self even when self_login is empty");
        assert_eq!(plan.posts[0].external_author, None);
    }

    // ── pr_review_status (BUILD 2a: PR → code-review status) ───────────────────────────────────────

    fn pr(number: i64, state: &str, merged_at: Option<&str>) -> Issue {
        let mut i = issue(number, "a PR", "dev");
        i.is_pull_request = true;
        i.state = state.to_string();
        i.pr_merged_at = merged_at.map(str::to_string);
        i
    }

    #[test]
    fn pr_status_open_pr_is_open() {
        assert_eq!(pr_review_status(&pr(1, "open", None)), PrReviewStatus::Open);
        assert_eq!(pr_review_status(&pr(1, "open", None)).as_board_status(), "open");
    }

    #[test]
    fn pr_status_merged_pr_is_approved() {
        // A merged PR is state:"closed" AND has merged_at — merged_at wins over the closed state.
        let s = pr_review_status(&pr(2, "closed", Some("2026-09-30T00:00:00Z")));
        assert_eq!(s, PrReviewStatus::Approved);
        assert_eq!(s.as_board_status(), "approved");
    }

    #[test]
    fn pr_status_closed_unmerged_pr_is_closed() {
        let s = pr_review_status(&pr(3, "closed", None));
        assert_eq!(s, PrReviewStatus::Closed);
        assert_eq!(s.as_board_status(), "closed");
    }

    #[test]
    fn pr_status_empty_merged_at_is_not_treated_as_merged() {
        // Defensive: an empty-string merged_at must not read as merged (only a real timestamp does).
        assert_eq!(pr_review_status(&pr(4, "closed", Some(""))), PrReviewStatus::Closed);
        assert_eq!(pr_review_status(&pr(5, "open", Some(""))), PrReviewStatus::Open);
    }

    // ── plan_outbound (board → GitHub) ─────────────────────────────────────────────────────────────

    fn reflect_event(seq: i64, source: &str, external_id: &str, comment_id: i64, author: &str, body: &str) -> Event {
        Event {
            seq,
            kind: crate::board::TASK_OUTBOUND_REFLECT.to_string(),
            actor: Some(author.to_string()),
            task_id: Some(1),
            channel_id: None,
            created_at: None,
            data: serde_json::json!({
                "task_id": 1, "comment_id": comment_id, "author": author, "body": body,
                "source": source, "external_id": external_id,
            }),
        }
    }

    fn plain_event(seq: i64) -> Event {
        Event {
            seq,
            kind: "task.commented".to_string(),
            actor: None,
            task_id: Some(1),
            channel_id: None,
            created_at: None,
            data: serde_json::Value::Null,
        }
    }

    #[test]
    fn outbound_maps_github_reflects_to_comments() {
        let events = [reflect_event(10, "github", "o/r#3", 42, "concierge", "ship it")];
        let (out, cursor) = plan_outbound(&events, 5);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].external_id, "o/r#3");
        assert_eq!(out[0].comment_id, 42);
        assert_eq!(out[0].event_seq, 10);
        assert!(out[0].body.contains("ship it"), "original body carried");
        assert!(out[0].body.contains("concierge"), "board author attributed inline");
        assert_eq!(cursor, 10, "cursor advances to the max seq");
    }

    #[test]
    fn outbound_skips_non_github_sources_but_advances_cursor() {
        // A reflect for a different source (e.g. a slack link) must be ignored — not our adapter.
        let events = [reflect_event(20, "slack", "C7", 1, "concierge", "x")];
        let (out, cursor) = plan_outbound(&events, 0);
        assert!(out.is_empty(), "non-github source skipped");
        assert_eq!(cursor, 20, "cursor still advances past the skipped event");
    }

    #[test]
    fn outbound_skips_non_reflect_events_but_advances_cursor() {
        let events = [plain_event(30), reflect_event(31, "github", "o/r#1", 5, "a", "hi")];
        let (out, cursor) = plan_outbound(&events, 0);
        assert_eq!(out.len(), 1);
        assert_eq!(cursor, 31, "cursor advances past the skipped non-reflect event too");
    }

    #[test]
    fn outbound_cursor_is_monotonic_and_empty_batch_keeps_it() {
        // A stale/lower seq must never regress the cursor; an empty batch keeps it.
        let (_out, c1) = plan_outbound(&[reflect_event(3, "github", "o/r#1", 1, "a", "old")], 100);
        assert_eq!(c1, 100, "lower seq doesn't regress the cursor");
        let (out, c2) = plan_outbound(&[], 42);
        assert!(out.is_empty());
        assert_eq!(c2, 42);
    }

    #[test]
    fn outbound_render_prefers_external_author_name() {
        let mut r = TaskReflect {
            task_id: 1,
            comment_id: 1,
            body: "hello".to_string(),
            author: "concierge".to_string(),
            external_author: Some("github:octocat".to_string()),
            source: "github".to_string(),
            external_id: "o/r#1".to_string(),
            external_parent_id: None,
        };
        assert!(render_outbound_github_comment(&r).contains("github:octocat"));
        r.external_author = None;
        assert!(render_outbound_github_comment(&r).contains("concierge"), "falls back to board author");
    }
}
