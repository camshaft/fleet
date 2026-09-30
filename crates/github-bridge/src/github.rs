//! `github` — the GitHub REST API transport: the issue/comment domain model, pure parsers, and a thin
//! authenticated client for polling a repo's issues + comments.
//!
//! This is the OTHER end of the bridge from [`crate::board`]: the board client reads/writes the coordination
//! board; this client reads GitHub. GitHub is plain REST polling (no webhooks/streaming needed for the fleet
//! use), so the whole adapter stays a blocking poll loop with no heavy async tree — unlike Slack's Socket
//! Mode. All PARSING is factored into pure functions ([`parse_issues`], [`parse_issue_comments`]) that are
//! unit-tested against captured GitHub JSON without a network; the HTTP methods are thin ureq wrappers
//! exercised live by the daemon (a later slice).
//!
//! The client is transport only: it does NOT decide what to ingest or how to map issues to tasks — that is
//! the sync planner's job (a later slice), kept separate so the mapping stays pure + testable and the GitHub
//! specifics stay confined to this module.

use serde::Deserialize;
use serde_json::Value;

/// GitHub's max page size for list endpoints. The poll loop pages until a page returns fewer than this.
pub const PER_PAGE: usize = 100;

/// The GitHub REST API version this adapter pins (sent as `X-GitHub-Api-Version`). Pinning avoids silent
/// breakage when GitHub advances the default version.
const API_VERSION: &str = "2022-11-28";

/// The external-identity prefix for a GitHub user: a stable `github:<login>` key the board attributes an
/// ingested author with (board-core #149), so board readers see the GitHub author, not the bridge.
pub fn github_external_author(login: &str) -> String {
    format!("github:{login}")
}

/// A GitHub issue, reduced to the fields ingest needs. Note the GitHub issues list endpoint also returns
/// PULL REQUESTS (a PR is an issue with a `pull_request` object); [`Issue::is_pull_request`] flags them so
/// ingest can skip PRs and mirror only real issues.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Issue {
    /// The issue number within its repo (the `#N` users see; stable, unlike the internal id).
    pub number: i64,
    pub title: String,
    /// The issue body (Markdown). Empty string when GitHub returns `null` (an issue with no description).
    pub body: String,
    /// `"open"` or `"closed"`.
    pub state: String,
    /// The author's GitHub login, or empty when the account is gone ("ghost").
    pub author: String,
    /// RFC3339 last-updated timestamp — the poll cursor (`?since=`) and staleness check.
    pub updated_at: String,
    /// The issue's web URL (for a human-legible back-reference on the mirrored task).
    pub html_url: String,
    /// True when this "issue" is actually a pull request (has a `pull_request` object). Issue ingest skips
    /// these; the PR-review sync (BUILD 2) mirrors them instead.
    pub is_pull_request: bool,
    /// For a PR row, the `pull_request.merged_at` RFC3339 timestamp — `Some` iff the PR was merged, `None`
    /// for an open or closed-unmerged PR (and always `None` for a real issue). The issues-list endpoint
    /// includes this on the nested `pull_request` object, so the merged/closed distinction needs no extra
    /// GitHub call (BUILD 2a: "no new endpoints"). See [`crate::sync::pr_review_status`].
    pub pr_merged_at: Option<String>,
}

/// A comment on a GitHub issue, reduced to the fields the attributed-comment sync needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IssueComment {
    /// GitHub's globally-unique comment id — the stable dedup/link key (an issue's comments share the issue
    /// number, so the comment id is what identifies a comment).
    pub id: i64,
    pub body: String,
    /// The commenter's GitHub login, or empty for a ghost account.
    pub author: String,
    /// RFC3339 last-updated timestamp — the per-issue comment poll cursor.
    pub updated_at: String,
    pub html_url: String,
}

/// The nested `user` object on issues/comments. Login is optional (a deleted account serializes as `null`).
#[derive(Deserialize)]
struct RawUser {
    #[serde(default)]
    login: Option<String>,
}

#[derive(Deserialize)]
struct RawIssue {
    number: i64,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    state: Option<String>,
    #[serde(default)]
    user: Option<RawUser>,
    #[serde(default)]
    updated_at: Option<String>,
    #[serde(default)]
    html_url: Option<String>,
    /// Present iff this row is a pull request. Presence flags a PR; `merged_at` (nested) distinguishes a
    /// merged PR from an open/closed-unmerged one.
    #[serde(default)]
    pull_request: Option<RawPullRequest>,
}

/// The nested `pull_request` object on a PR row of the issues-list response. Only `merged_at` matters here
/// (its presence is the PR flag; the timestamp is the merged/unmerged distinction).
#[derive(Deserialize)]
struct RawPullRequest {
    #[serde(default)]
    merged_at: Option<String>,
}

#[derive(Deserialize)]
struct RawComment {
    id: i64,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    user: Option<RawUser>,
    #[serde(default)]
    updated_at: Option<String>,
    #[serde(default)]
    html_url: Option<String>,
}

fn login_of(user: Option<RawUser>) -> String {
    user.and_then(|u| u.login).unwrap_or_default()
}

/// Pull the JSON array out of a GitHub list response. The list endpoints return a BARE array; the search API
/// wraps it in `{ "items": [...] }`. Accept both (and an `{ "issues"/"comments": [...] }` envelope) so a
/// caller that swaps endpoints later doesn't break. Returns the parse error text on a non-array body.
fn list_array(body: &str, what: &str) -> Result<Vec<Value>, String> {
    let v: Value =
        serde_json::from_str(body).map_err(|e| format!("github {what}: response was not JSON: {e}"))?;
    match v {
        Value::Array(a) => Ok(a),
        Value::Object(ref o) => o
            .get("items")
            .or_else(|| o.get(what))
            .and_then(Value::as_array)
            .cloned()
            .ok_or_else(|| format!("github {what}: object without an array field: {v}")),
        other => Err(format!("github {what}: expected an array, got {other}")),
    }
}

/// Parse a GitHub issues-list response into [`Issue`]s. Null bodies become `""`; a missing/null author
/// becomes `""`; a row with a `pull_request` object is flagged (not filtered — the caller decides). Never
/// panics on a well-formed-but-sparse row.
pub fn parse_issues(body: &str) -> Result<Vec<Issue>, String> {
    let arr = list_array(body, "issues")?;
    arr.into_iter()
        .map(|row| {
            let r: RawIssue =
                serde_json::from_value(row).map_err(|e| format!("github issues: bad row: {e}"))?;
            let pr_merged_at = r.pull_request.as_ref().and_then(|pr| pr.merged_at.clone());
            Ok(Issue {
                number: r.number,
                title: r.title.unwrap_or_default(),
                body: r.body.unwrap_or_default(),
                state: r.state.unwrap_or_default(),
                author: login_of(r.user),
                updated_at: r.updated_at.unwrap_or_default(),
                html_url: r.html_url.unwrap_or_default(),
                is_pull_request: r.pull_request.is_some(),
                pr_merged_at,
            })
        })
        .collect()
}

/// Parse a GitHub issue-comments-list response into [`IssueComment`]s. Same null-tolerance as
/// [`parse_issues`].
pub fn parse_issue_comments(body: &str) -> Result<Vec<IssueComment>, String> {
    let arr = list_array(body, "comments")?;
    arr.into_iter()
        .map(|row| {
            let r: RawComment =
                serde_json::from_value(row).map_err(|e| format!("github comments: bad row: {e}"))?;
            Ok(IssueComment {
                id: r.id,
                body: r.body.unwrap_or_default(),
                author: login_of(r.user),
                updated_at: r.updated_at.unwrap_or_default(),
                html_url: r.html_url.unwrap_or_default(),
            })
        })
        .collect()
}

/// A thin authenticated GitHub REST client (blocking, over ureq). Holds the token; NO `Debug` derive so the
/// credential can't leak via a stray `{:?}`.
pub struct GithubClient {
    api_base: String,
    token: String,
    agent: ureq::Agent,
}

impl GithubClient {
    /// Build a client against `api_base` (public GitHub `https://api.github.com`, or a GHES base) with the
    /// given token. A trailing slash on `api_base` is trimmed so path joins don't double up. No network
    /// round-trip.
    pub fn new(api_base: &str, token: &str) -> Self {
        GithubClient {
            api_base: api_base.trim_end_matches('/').to_string(),
            token: token.to_string(),
            agent: ureq::agent(),
        }
    }

    /// GET a path (already query-formed), returning the raw response body. Sends the auth + versioning +
    /// User-Agent headers GitHub requires (a missing User-Agent is a hard 403).
    fn get(&self, path: &str) -> Result<String, String> {
        self.agent
            .get(&format!("{}{}", self.api_base, path))
            .set("authorization", &format!("Bearer {}", self.token))
            .set("accept", "application/vnd.github+json")
            .set("x-github-api-version", API_VERSION)
            .set("user-agent", "github-bridge")
            .call()
            .map_err(|e| format!("github GET {path} failed: {e}"))?
            .into_string()
            .map_err(|e| format!("github GET {path} read failed: {e}"))
    }

    /// Post a comment on an issue (`POST /repos/{repo}/issues/{number}/comments`) — the OUT direction, where
    /// `repo` = `owner/name`. Body is the rendered reflect text. Returns the new comment's id (best-effort;
    /// `0` if the response omits it). Errors on a non-2xx (the daemon then leaves its cursor unadvanced so
    /// the reflect retries).
    pub fn post_issue_comment(&self, repo: &str, number: i64, body: &str) -> Result<i64, String> {
        let url = format!("{}/repos/{repo}/issues/{number}/comments", self.api_base);
        let payload = serde_json::json!({ "body": body }).to_string();
        let raw = self
            .agent
            .post(&url)
            .set("authorization", &format!("Bearer {}", self.token))
            .set("accept", "application/vnd.github+json")
            .set("x-github-api-version", API_VERSION)
            .set("user-agent", "github-bridge")
            .set("content-type", "application/json")
            .send_string(&payload)
            .map_err(|e| format!("github POST issue {repo}#{number} comment failed: {e}"))?
            .into_string()
            .map_err(|e| format!("github POST comment read failed: {e}"))?;
        Ok(serde_json::from_str::<Value>(&raw)
            .ok()
            .and_then(|v| v.get("id").and_then(Value::as_i64))
            .unwrap_or(0))
    }

    /// One page of a repo's issues (`repo` = `owner/name`), oldest-updated first so a cursor advances
    /// monotonically. `state=all` (open + closed). `since` (RFC3339) filters to issues updated at/after it —
    /// the incremental poll cursor. `page` is 1-based. NOTE: the result may include pull requests (flagged
    /// via [`Issue::is_pull_request`]); ingest filters them.
    pub fn list_issues(&self, repo: &str, since: Option<&str>, page: usize) -> Result<Vec<Issue>, String> {
        let mut path = format!(
            "/repos/{repo}/issues?state=all&sort=updated&direction=asc&per_page={PER_PAGE}&page={page}"
        );
        if let Some(s) = since {
            path.push_str("&since=");
            path.push_str(s);
        }
        parse_issues(&self.get(&path)?)
    }

    /// The authenticated account's own login (`GET /user` → `login`). The daemon fetches this once at
    /// startup so `sync::plan_comment_ingest` can skip comments the bridge itself posted (loop-safety: an
    /// OUT-reflected comment must not re-ingest). Best-effort at the call site — a GitHub App installation
    /// token may 403 on `/user`; the daemon then runs with no self-login (dedup still guards re-posts).
    pub fn viewer_login(&self) -> Result<String, String> {
        let raw = self.get("/user")?;
        let v: Value =
            serde_json::from_str(&raw).map_err(|e| format!("github GET /user: not JSON: {e}"))?;
        v.get("login")
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| format!("github GET /user: no login in response {v}"))
    }

    /// One page of an issue's comments, oldest-updated first. `since` filters incrementally. `page` 1-based.
    pub fn list_issue_comments(
        &self,
        repo: &str,
        issue_number: i64,
        since: Option<&str>,
        page: usize,
    ) -> Result<Vec<IssueComment>, String> {
        let mut path =
            format!("/repos/{repo}/issues/{issue_number}/comments?per_page={PER_PAGE}&page={page}");
        if let Some(s) = since {
            path.push_str("&since=");
            path.push_str(s);
        }
        parse_issue_comments(&self.get(&path)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn github_external_author_is_stable_prefix() {
        assert_eq!(github_external_author("octocat"), "github:octocat");
    }

    // ── parse_issues ───────────────────────────────────────────────────────────────────────────────

    #[test]
    fn parse_issues_maps_the_fields() {
        // A trimmed real GitHub issues-list row.
        let body = r#"[
            {"number": 42, "title": "Fix the widget", "body": "it's broken",
             "state": "open", "user": {"login": "octocat"},
             "updated_at": "2026-09-29T10:00:00Z", "html_url": "https://github.com/o/r/issues/42"}
        ]"#;
        let issues = parse_issues(body).unwrap();
        assert_eq!(issues.len(), 1);
        let i = &issues[0];
        assert_eq!(i.number, 42);
        assert_eq!(i.title, "Fix the widget");
        assert_eq!(i.body, "it's broken");
        assert_eq!(i.state, "open");
        assert_eq!(i.author, "octocat");
        assert_eq!(i.updated_at, "2026-09-29T10:00:00Z");
        assert_eq!(i.html_url, "https://github.com/o/r/issues/42");
        assert!(!i.is_pull_request);
    }

    #[test]
    fn parse_issues_flags_pull_requests() {
        // The issues endpoint returns PRs too — they carry a `pull_request` object. Flag, don't drop.
        let body = r#"[
            {"number": 7, "title": "a PR", "state": "open", "user": {"login": "dev"},
             "updated_at": "t", "html_url": "u", "pull_request": {"url": "..."}},
            {"number": 8, "title": "a real issue", "state": "open", "user": {"login": "dev"},
             "updated_at": "t", "html_url": "u"}
        ]"#;
        let issues = parse_issues(body).unwrap();
        assert!(issues[0].is_pull_request, "row with pull_request is flagged");
        assert!(!issues[1].is_pull_request, "row without is a real issue");
        assert_eq!(issues[1].pr_merged_at, None, "a real issue has no merged_at");
    }

    #[test]
    fn parse_issues_reads_pull_request_merged_at() {
        // The issues-list `pull_request` object carries merged_at (null until merged) — 2a needs no extra
        // fetch to tell a merged PR from a closed-unmerged one.
        let body = r#"[
            {"number": 10, "title": "merged PR", "state": "closed", "user": {"login": "dev"},
             "updated_at": "t", "html_url": "u",
             "pull_request": {"url": "...", "merged_at": "2026-09-30T00:00:00Z"}},
            {"number": 11, "title": "open PR", "state": "open", "user": {"login": "dev"},
             "updated_at": "t", "html_url": "u", "pull_request": {"url": "...", "merged_at": null}}
        ]"#;
        let issues = parse_issues(body).unwrap();
        assert!(issues[0].is_pull_request);
        assert_eq!(issues[0].pr_merged_at.as_deref(), Some("2026-09-30T00:00:00Z"));
        assert!(issues[1].is_pull_request);
        assert_eq!(issues[1].pr_merged_at, None, "null merged_at → None (open/unmerged PR)");
    }

    #[test]
    fn parse_issues_tolerates_null_body_and_ghost_author() {
        let body = r#"[
            {"number": 1, "title": "no body", "body": null, "state": "closed",
             "user": null, "updated_at": "t", "html_url": "u"}
        ]"#;
        let i = &parse_issues(body).unwrap()[0];
        assert_eq!(i.body, "", "null body → empty string");
        assert_eq!(i.author, "", "null/ghost user → empty author");
        assert_eq!(i.state, "closed");
    }

    #[test]
    fn parse_issues_accepts_search_items_envelope() {
        // The search API wraps rows in {items:[...]}; accept it so a caller can swap endpoints.
        let body = r#"{"total_count": 1, "items": [
            {"number": 5, "title": "t", "state": "open", "user": {"login": "u"},
             "updated_at": "t", "html_url": "h"}]}"#;
        let issues = parse_issues(body).unwrap();
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].number, 5);
    }

    #[test]
    fn parse_issues_empty_and_errors() {
        assert!(parse_issues("[]").unwrap().is_empty());
        assert!(parse_issues(r#"{"nope": 1}"#).is_err());
        assert!(parse_issues("not json").is_err());
    }

    // ── parse_issue_comments ─────────────────────────────────────────────────────────────────────

    #[test]
    fn parse_comments_maps_the_fields() {
        let body = r#"[
            {"id": 555, "body": "looks good", "user": {"login": "hubot"},
             "updated_at": "2026-09-29T11:00:00Z", "html_url": "https://github.com/o/r/issues/42#c555"}
        ]"#;
        let cs = parse_issue_comments(body).unwrap();
        assert_eq!(cs.len(), 1);
        assert_eq!(cs[0].id, 555);
        assert_eq!(cs[0].body, "looks good");
        assert_eq!(cs[0].author, "hubot");
        assert_eq!(cs[0].updated_at, "2026-09-29T11:00:00Z");
    }

    #[test]
    fn parse_comments_tolerates_null_body_and_ghost() {
        let body = r#"[{"id": 1, "body": null, "user": null, "updated_at": "t", "html_url": "u"}]"#;
        let c = &parse_issue_comments(body).unwrap()[0];
        assert_eq!(c.body, "");
        assert_eq!(c.author, "");
    }

    #[test]
    fn parse_comments_empty_and_errors() {
        assert!(parse_issue_comments("[]").unwrap().is_empty());
        assert!(parse_issue_comments(r#"{"x": 1}"#).is_err());
    }

    #[test]
    fn client_new_trims_trailing_slash() {
        let c = GithubClient::new("https://api.github.com/", "ghp_x");
        assert_eq!(c.api_base, "https://api.github.com");
    }
}
