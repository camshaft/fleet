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
| `sync`        | Pure bidirectional planning. IN: issues → idempotent task creates + comments → attributed board comments (loop-safe, dedup'd); **pull requests → board code reviews** (BUILD 2a: status open/merged→approved/closed-unmerged→closed + conversation-comment log entries). OUT: `task.outbound_reflect` (board-core #264) → GitHub issue comments (source-filtered, attribution-rendered). |
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
github_token      = "ghp_..."     # GitHub PAT or App installation token (issues:read/write, across ALL repos below)
# github_token_file = "/run/agenix/github-bridge-token"  # OR: read the bare token from this file (agenix
                                  # secret), keeping the rest of this config non-secret. Overrides inline; a
                                  # relative path resolves against this file's dir. Missing/empty ⇒ dormant.
repos        = ["camshaft/fleet", "camshaft/dotfiles"]  # owner/name list — ingested into project_id
# repo       = "camshaft/fleet"   # singular sugar for a one-repo config (folds into `repos`)
project_id   = 16                  # board project all ingested issues become tasks in
board_api    = "http://127.0.0.1:8079/api"        # optional; default shown (green board loopback)
api_base     = "https://api.github.com"           # optional; override for GitHub Enterprise Server
default_to   = "concierge"        # optional; default
bridge_agent = "github-bridge"    # optional; the board agent id this bridge writes as
state_dir    = "/var/lib/github-bridge"  # optional; defaults to the config file's dir. Holds the cursor
                                         # state.json — MUST be writable + durable across restarts.
```

- Token present ⇒ live; missing ⇒ dormant (valid — deploy before the token is minted).
- `repos` (or the singular `repo`) + `project_id` both present ⇒ ingest active; either missing ⇒ up-but-idle
  (valid). Multiple repos all mirror into the one `project_id`, each scanning independently (per-repo cursor).
  The `github_token` must cover **every** listed repo (a fine-grained PAT enumerating them, a classic
  `repo`-scoped PAT, or an org App installation). The repo set is **explicit** — there is no auto-discovery.
- Unknown keys are rejected (`deny_unknown_fields`) — a typo surfaces as a "malformed config" (fail-soft
  dormant), not a silent drop.
- `Debug` on the config **redacts** the token — it never prints into logs.

## Deploy (camshaft/dotfiles, fleet-tunnel/green-machine-ops)

- systemd role runs `github-bridge --config <path>`; `Restart=always` (fail-soft startup makes this safe).
- The config is delivered as the **agenix-decrypted TOML secret** `github-bridge.toml.age` (mode 0400) —
  **not** an env file / `EnvironmentFile=` (mandate #159).
- `StateDirectory=github-bridge` (or any persistent writable dir) for the firehose cursor.
- Needs localhost reach to the board front-door (`board_api`) + outbound HTTPS to `api_base`.

## Issue ↔ task links (idempotent, board-side — #270)

- `create_task` / `comment_task` carry an `external_link {source:"github", external_id}` (the issue ref
  `owner/repo#<number>` or comment ref `owner/repo#c<id>`). The board **atomically** creates-or-returns-
  existing keyed on `(source, external_id)` and reports `created` in the response (board-core #270).
- So ingest is **exactly-once with no create→link race**: the adapter just posts every polled issue/comment
  and relies on `created:false` (+ the returned id) to know it was already mirrored. No separate
  link-register call and no pre-fetch of existing links.

## GitHub PR → board code review (BUILD 2a, Review entity #372/#373)

The issues poll (`state=all`) already returns pull requests (a PR is an issue with a `pull_request` object),
so mirroring PRs as **code reviews** needs no new GitHub endpoint:

- Each PR becomes a board review via the idempotent `create_review` (`kind=code`, `external_link
  {source:"github_pr", external_id:"owner/repo#<number>"}`) — the board de-dupes + links atomically and
  reports `created`, same as issue ingest (#270).
- **Status** maps the PR's state. 2a (concluding states, from the issues-list row alone): `pull_request.merged_at`
  set → `approved`; closed-unmerged → `closed`; otherwise `open`. **2b** refines an *open* PR via one Pulls-API
  + one Reviews-API call (only for open PRs — closed ones are already terminal): a **draft** stays `open`; a
  ready PR whose latest decisive review requested changes → `changes_requested`; otherwise `in_review`. A PR
  seen open then later merged advances via the idempotent `set_review_status` (same-status re-apply is a
  board-side no-op); a transient Pulls/Reviews fetch error falls back to `open` rather than wedging.
- The PR's **conversation comments** (same `/issues/:n/comments` endpoint) are appended to the review as
  `comment`-type log entries via the idempotent `append_review_log`, loop-safe (skips the bridge's own
  reflected comments) and keyed on the comment ref (`owner/repo#c<id>`).
- The PR's **inline diff-review comments** (`GET /pulls/:n/comments`, 2b-2) are appended as `finding`-type
  review-log entries — the body prefixed with the `file:line` location — keyed on a distinct review-comment
  ref (`owner/repo#rc<id>`) so a finding never collides with a conversation comment (`#c<id>`).
- The `github_pr` link source is distinct from `github` (issue↔task) so PR-review links never collide with
  issue-ingest links. BUILD 2 (2a + 2b) is complete; every mirror is idempotent + loop-safe.

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
- **Delivery.** **IN (ingest) is exactly-once** — the board de-duplicates create/comment (issues) and
  create_review/append_review_log (PRs) on the external link atomically (#270 / #372), so a
  response-read-failed retry returns `created:false` / `appended:false` rather than duplicating.
  **OUT (GitHub comment post) remains at-least-once**: GitHub issue comments have no idempotency key, so a
  write that succeeds while its response fails to read duplicates one comment on retry — rare + non-fatal,
  and inherent to the GitHub API. See the `runner` module doc.
