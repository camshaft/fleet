# github-bridge

The fleet's **GitHub ↔ board bridge adapter** — the transport + sync end of approved design #141 (task
#136). A GitHub repo's issues mirror IN to coordination-board tasks; GitHub issue comments sync in
attributed to their GitHub authors; an authorized board task comment reflects OUT onto the GitHub issue
under policy. The board owns the bridge **core** (external-identity, external-links, outbound-reflect authz
— board tasks #149/#150/#151); this crate is **transport + sync only**.

It is the **second adapter** over that shared core — Slack is the first (`crates/slack-bridge`). Concerns
shared by both adapters live once on the board; only GitHub-specifics live here.

## Architecture

Pure, unit-tested core (always compiled) + a thin blocking poll-loop daemon binary (added in a later slice
behind a feature — GitHub is plain REST polling, so there is no heavy async tree like Slack's Socket Mode):

| module        | role |
|---------------|------|
| `config`      | Fail-soft config from a **single TOML file** (no env vars — operator mandate #159): GitHub token + `owner/repo` + board `project_id`, board REST base, GitHub API base. |
| `board`       | Token-less localhost board REST client: firehose poll (`GET /events`, board-core #150/#264), create/comment mirrored tasks with GitHub-author attribution (`POST /tasks`, `POST /tasks/:id/comments`, board-core #149), and durable issue↔task + comment link read/register (`/external-links`, board-core #149 slice 2 / #151). |
| `github`      | GitHub REST transport: `Issue`/`IssueComment` model, null/ghost-tolerant PR-flagging parsers, and a thin no-`Debug` authenticated client (issues + comments poll, `viewer_login`, `post_issue_comment`). |
| `sync`        | Pure bidirectional planning. IN: issues → idempotent task creates + comments → attributed board comments (loop-safe, dedup'd). OUT: `task.outbound_reflect` (board-core #264) → GitHub issue comments (source-filtered, attribution-rendered). |
| `state`       | The daemon's persisted cursors (firehose seq for OUT, GitHub `?since=` for IN), fail-soft load. |
| `main`/`runner` | The daemon (feature `daemon`): a blocking poll loop — IN (GitHub → board) + OUT (board firehose → GitHub) each tick, fail-soft dormant with no token. Not unit-tested (live network); the gate is the lib's `cargo test`. |

Data flow (target):

```
board firehose (authorized reflect, #150)                 GitHub REST (issues + comments)
        │  poll_events                                            │  list_issues / list_comments
        ▼                                                         ▼
   reflect task comment ──► GitHub issue comment           ingest ──► board::create_task / comment_task
   (issue↔task link resolves task → issue)                 (attributed external_author = github:<login>)
```

## Build

```sh
# Pure core + tests (fast — no daemon tree):
cargo test -p github-bridge

# The daemon binary (pulls clap/tracing — REQUIRED to build the bin):
cargo build -p github-bridge --features daemon --release
# → target/release/github-bridge
```

The `daemon` feature is `required-features` on the `[[bin]]`, so the default `cargo test --workspace` /
`nix flake check` never compile the CLI/logging tree.

## Run

```sh
github-bridge --config /path/to/github-bridge.toml
```

`--config` is the **only** input — there is no env-var configuration (mandate #159). With no token in the
config the daemon stays alive but **idle** (fail-soft), so it is safe to deploy before the token is minted.
Each tick runs IN (GitHub → board) then OUT (board firehose → GitHub); logging is `RUST_LOG`-controlled
(default `info`).

## Config (TOML)

```toml
github_token = "ghp_..."          # GitHub PAT or App installation token (issues:read/write, etc.)
repo         = "camshaft/fleet"    # owner/name of the repo whose issues are ingested
project_id   = 16                  # board project ingested issues become tasks in
board_api    = "http://127.0.0.1:8880/board/api"  # optional; default shown (local board front-door)
api_base     = "https://api.github.com"           # optional; override for GitHub Enterprise Server
default_to   = "concierge"        # optional; default
bridge_agent = "github-bridge"    # optional; the board agent id this bridge writes as
state_dir    = "/var/lib/github-bridge"  # optional; defaults to the config file's dir. Holds the firehose
                                         # cursor — MUST be writable + durable across restarts.
```

- Token present ⇒ live; missing ⇒ dormant (valid — deploy before the token is minted).
- `repo` + `project_id` both present ⇒ ingest active; either missing ⇒ up-but-idle (valid).
- Unknown keys are rejected (`deny_unknown_fields`) — a typo surfaces as a "malformed config" (fail-soft
  dormant), not a silent drop.
- `Debug` on the config **redacts** the token — it never prints into logs.

## Deploy (camshaft/dotfiles, fleet-tunnel/green-machine-ops)

- systemd role runs `github-bridge --config <path>`; `Restart=always` (fail-soft startup makes this safe).
- The config is delivered as the **agenix-decrypted TOML secret** `github-bridge.toml.age` (mode 0400) —
  **not** an env file / `EnvironmentFile=` (mandate #159).
- `StateDirectory=github-bridge` (or any persistent writable dir) for the firehose cursor.
- Needs localhost reach to the board front-door (`board_api`) + outbound HTTPS to `api_base`.

## Issue ↔ task links

- The durable map lives in the board's generic `external_link` table (board-core #149 slice 2 / #151):
  `source="github"`, `board_kind="task"`, `external_id="owner/repo#<number>"`, `board_id=<task id>`.
- Ingest reads `GET /external-links?source=github&board_kind=task` to stay **idempotent** — an issue already
  linked to a task is updated, never re-created — and registers the link right after creating the task.

## Operational notes

- **Persisted cursors** (`<state_dir>/github-bridge.state.json`): the board firehose `seq` (OUT) + the
  GitHub `?since=` timestamp (IN). Advanced only past terminally-handled work; a restart resumes without a
  gap. First run initializes the firehose at HEAD (skip the board backlog) but leaves the issue cursor empty
  (ingest the issue backlog).
- **No echo loop:** the bridge writes board tasks/comments as its own `bridge_agent`, which is not an
  authorized OUT reflector, so its own writes never reflect back OUT to GitHub; and IN comment sync skips
  comments authored by the bridge's own GitHub account (`viewer_login`).
- **Attribution:** ingested GitHub authors are attributed via `external_author = github:<login>` +
  `upsert_external_identity` (board-core #149), so board readers see the GitHub author, not the bridge.
- **Delivery is at-least-once.** Steady state is exactly-once (external-link dedup + persisted cursor); the
  one residual window is a remote write that succeeds but whose response we fail to read — a rare network
  blip then duplicates one GitHub comment / board task on retry. Inherent (no GitHub comment idempotency
  key; `create_task` links only after it returns). See the `runner` module doc + the follow-on below.

## Follow-ons (coordinate with v-task-board)

- **Idempotent create-keyed-on-external-link (board core):** to make IN ingest exactly-once, `create_task`
  (and `comment_task`) should accept the intended `external_link` and atomically create-or-return-existing
  keyed on `(source, external_id)`. Removes the duplicate-task/comment risk on a create-succeeded-response-
  -failed retry. Proposed to v-task-board.
