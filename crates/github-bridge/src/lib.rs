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
//!
//! Later slices add: the GitHub REST transport (issue + comment polling), issue→task ingest, attributed
//! comment sync, and OUT-reflect-under-policy — plus the daemon binary that wires the poll loop together.
//!
//! Kept generic on purpose: GitHub-specifics live in this adapter; the Slack adapter (#152) drops in over the
//! same board core, so any concern shared by both belongs on the board, not duplicated here.

pub mod board;
pub mod config;

pub use board::{
    build_comment_body, build_identity_body, build_task_body, issue_ref, parse_events, parse_issue_links,
    BoardClient, Event, IssueTaskLink, LINK_KIND_TASK, LINK_SOURCE, OUTBOUND_REFLECT,
};
pub use config::{Config, DEFAULT_CONFIG_FILENAME};
