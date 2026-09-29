//! `github-bridge` — the fleet's GitHub↔board bridge adapter (library crate).
//!
//! This crate is the transport + sync end of approved design #141 (task #136): a GitHub repo's issues
//! mirror IN to coordination-board tasks, GitHub issue comments sync in attributed to their GitHub authors,
//! and an authorized board task comment reflects OUT onto the GitHub issue under policy. It is the SECOND
//! adapter over the shared external-bridge core (Slack is the first, `crates/slack-bridge`): the board owns
//! the bridge CORE (external-identity, external-links, outbound-reflect authz — board tasks #149/#150/#151)
//! and this crate CONSUMES those primitives, never reimplements them. GitHub-specifics live here so shared
//! concerns stay once on the board.
//!
//! The pure, transport-agnostic core (unit-tested here; wired to the live GitHub REST poll loop by the
//! daemon binary in a later slice):
//! - [`config`] — fail-soft config from a single **TOML file** (operator mandate #159: no env vars; only the
//!   file path is a `--config` CLI flag), including the localhost board REST base the firehose reads and the
//!   GitHub API base + token + `owner/repo` + board `project_id` to ingest into.
//! - [`board`] — the token-less localhost board REST client: subscribe to the event firehose
//!   (`GET /events`, board-core #150), create/comment mirrored tasks with GitHub-author attribution
//!   (board-core #149), and read/register the durable GitHub-issue↔board-task links (board-core #149 slice 2
//!   / #151). Pure parsers + body builders unit-tested without a network.
//! - [`github`] — the GitHub REST transport: the issue/comment domain model, pure parsers (unit-tested
//!   against captured GitHub JSON), and a thin authenticated client for polling a repo's issues + comments.
//! - [`sync`] — the pure bidirectional PLANNING. IN: GitHub issues → mirrored board tasks to create
//!   (idempotent, PR-skipping) and an issue's GitHub comments → attributed board comments to post
//!   (loop-safe, dedup'd). OUT: authorized `task.outbound_reflect` firehose events (board-core #264) →
//!   GitHub issue comments to post (source-filtered, attribution-rendered). The daemon feeds it what it read
//!   and executes what it returns.
//!
//! Later slice adds: the daemon binary that wires the poll loop (GitHub poll + board firehose) together.
//!
//! Kept generic on purpose: GitHub-specifics live in this adapter; the Slack adapter (#152) drops in over the
//! same board core, so any concern shared by both belongs on the board, not duplicated here.

pub mod board;
pub mod config;
pub mod github;
pub mod sync;

pub use board::{
    build_comment_body, build_identity_body, build_task_body, comment_ref, issue_ref, parse_events,
    parse_issue_links, parse_issue_ref, BoardClient, Event, IssueTaskLink, TaskReflect, LINK_KIND_COMMENT,
    LINK_KIND_TASK, LINK_SOURCE, TASK_OUTBOUND_REFLECT,
};
pub use config::{Config, DEFAULT_CONFIG_FILENAME};
pub use github::{
    github_external_author, parse_issue_comments, parse_issues, GithubClient, Issue, IssueComment, PER_PAGE,
};
pub use sync::{
    plan_comment_ingest, plan_issue_ingest, plan_outbound, render_outbound_github_comment,
    render_task_description, CommentIngestPlan, CommentPost, IssueIngestPlan, OutboundComment, TaskCreate,
};
