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
//! Everything here is pure + unit-tested; the daemon feeds it what it read and executes what it returns
//! (`board::create_task` + `register_issue_link`, `board::comment_task` + a `board_kind="comment"` link).

use crate::board::{comment_ref, issue_ref, IssueTaskLink};
use crate::github::{github_external_author, Issue, IssueComment};
use std::collections::HashSet;

/// A mirrored board task to create from a GitHub issue (the daemon calls `board::create_task` then records
/// the issue↔task link via `board::register_issue_link` using [`TaskCreate::issue_ref`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskCreate {
    /// The durable issue↔task link key (`owner/repo#number`).
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
/// - An issue whose [`issue_ref`] is already in `existing_links` is skipped (idempotent — the task exists).
/// - Order is preserved so the daemon creates oldest-updated first (matching the `?sort=updated&asc` poll).
///
/// Pure: `existing_links` is what `board::list_issue_links` returned.
pub fn plan_issue_ingest(issues: &[Issue], repo: &str, existing_links: &[IssueTaskLink]) -> IssueIngestPlan {
    let linked: HashSet<&str> = existing_links.iter().map(|l| l.issue_ref.as_str()).collect();
    let mut creates = Vec::new();
    for issue in issues {
        if issue.is_pull_request {
            continue;
        }
        let iref = issue_ref(repo, issue.number);
        if linked.contains(iref.as_str()) {
            continue;
        }
        creates.push(TaskCreate {
            title: issue.title.clone(),
            description: render_task_description(repo, issue),
            external_author: attribution(&issue.author),
            issue_number: issue.number,
            issue_ref: iref,
        });
    }
    IssueIngestPlan { creates }
}

/// Plan the attributed board comments to post from a batch of an issue's GitHub comments.
///
/// - A comment authored by the bridge's own GitHub account (`self_login`) is skipped — loop-safety, so a
///   comment the bridge reflected OUT to GitHub isn't re-ingested back IN. (The board side is separately
///   loop-safe via the bridge's `author` ∉ `outbound_authors`; this guards the GitHub side.)
/// - A comment whose [`comment_ref`] is already in `already_synced` is skipped (idempotent).
/// - Order is preserved (oldest-updated first).
///
/// Pure: `already_synced` is the set of comment refs the board already has (from its `board_kind="comment"`
/// links); `self_login` is the bridge's GitHub login (`None` when unknown — then nothing is filtered as self).
pub fn plan_comment_ingest(
    comments: &[IssueComment],
    repo: &str,
    board_task_id: i64,
    self_login: Option<&str>,
    already_synced: &HashSet<String>,
) -> CommentIngestPlan {
    let mut posts = Vec::new();
    for c in comments {
        if let Some(me) = self_login
            && !c.author.is_empty()
            && c.author == me
        {
            continue; // loop-safety: don't re-ingest our own reflected comment
        }
        let cref = comment_ref(repo, c.id);
        if already_synced.contains(&cref) {
            continue;
        }
        posts.push(CommentPost {
            board_task_id,
            body: c.body.clone(),
            external_author: attribution(&c.author),
            comment_ref: cref,
        });
    }
    CommentIngestPlan { posts }
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

    fn link(repo: &str, number: i64, task: i64) -> IssueTaskLink {
        IssueTaskLink { board_task_id: task, issue_ref: issue_ref(repo, number) }
    }

    // ── plan_issue_ingest ──────────────────────────────────────────────────────────────────────────

    #[test]
    fn ingest_creates_a_task_per_new_issue_attributed() {
        let issues = [issue(42, "Fix widget", "octocat"), issue(43, "Add gizmo", "hubot")];
        let plan = plan_issue_ingest(&issues, "o/r", &[]);
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
        let plan = plan_issue_ingest(&[pr, issue(8, "real", "dev")], "o/r", &[]);
        assert_eq!(plan.creates.len(), 1, "only the real issue is mirrored");
        assert_eq!(plan.creates[0].issue_number, 8);
    }

    #[test]
    fn ingest_skips_already_linked_issues_idempotent() {
        // #42 already has a task; only #43 is new.
        let existing = [link("o/r", 42, 100)];
        let plan = plan_issue_ingest(&[issue(42, "old", "a"), issue(43, "new", "b")], "o/r", &existing);
        assert_eq!(plan.creates.len(), 1);
        assert_eq!(plan.creates[0].issue_ref, "o/r#43");
    }

    #[test]
    fn ingest_re_poll_of_only_known_issues_is_a_noop() {
        let existing = [link("o/r", 42, 100)];
        let plan = plan_issue_ingest(&[issue(42, "old", "a")], "o/r", &existing);
        assert!(plan.creates.is_empty(), "nothing new to create");
    }

    #[test]
    fn ingest_ghost_author_is_unattributed() {
        let plan = plan_issue_ingest(&[issue(9, "ghosted", "")], "o/r", &[]);
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
    fn comment_ingest_posts_attributed_new_comments() {
        let comments = [comment(1, "first", "octocat"), comment(2, "second", "hubot")];
        let plan = plan_comment_ingest(&comments, "o/r", 100, Some("fleet-bot"), &HashSet::new());
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
        let comments = [comment(1, "reflected", "fleet-bot"), comment(2, "human", "octocat")];
        let plan = plan_comment_ingest(&comments, "o/r", 100, Some("fleet-bot"), &HashSet::new());
        assert_eq!(plan.posts.len(), 1, "own comment filtered");
        assert_eq!(plan.posts[0].external_author.as_deref(), Some("github:octocat"));
    }

    #[test]
    fn comment_ingest_skips_already_synced() {
        let already: HashSet<String> = [comment_ref("o/r", 1)].into_iter().collect();
        let comments = [comment(1, "dup", "octocat"), comment(2, "new", "octocat")];
        let plan = plan_comment_ingest(&comments, "o/r", 100, None, &already);
        assert_eq!(plan.posts.len(), 1);
        assert_eq!(plan.posts[0].comment_ref, "o/r#c2");
    }

    #[test]
    fn comment_ingest_none_self_login_filters_nothing_as_self() {
        // With no known self login, don't drop anything as "self" (only dedup applies).
        let comments = [comment(1, "x", "anyone")];
        let plan = plan_comment_ingest(&comments, "o/r", 100, None, &HashSet::new());
        assert_eq!(plan.posts.len(), 1);
    }

    #[test]
    fn comment_ingest_ghost_author_is_unattributed_and_not_self_filtered() {
        // An empty-login (ghost) comment must not be mistaken for the bridge and must post unattributed.
        let comments = [comment(1, "ghost note", "")];
        let plan = plan_comment_ingest(&comments, "o/r", 100, Some(""), &HashSet::new());
        assert_eq!(plan.posts.len(), 1, "empty author != self even when self_login is empty");
        assert_eq!(plan.posts[0].external_author, None);
    }
}
