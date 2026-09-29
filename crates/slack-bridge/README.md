# slack-bridge

The fleet's **Slack ↔ board bridge adapter** — the transport + sync end of approved design #141.
Agents coordinate through the coordination board; the board auto-mirrors to Slack; Slack (including
operator DMs) syncs back. The board owns the bridge **core** (channel-map, external-identity,
outbound-authz — board tasks #149/#150/#151); this crate is **transport + sync only**.

It's one instance of a generic external-source bridge — Slack today, a GitHub adapter (#136) reuses the
same board core and the pure sync planner with its own resolver.

## Architecture

Pure, unit-tested core (always compiled) + a thin async transport binary (behind the `transport` feature):

| module        | role |
|---------------|------|
| `config`      | Fail-soft config from a **single TOML file** (no env vars — operator mandate #159). |
| `board`       | Token-less localhost board REST client: firehose poll (`GET /events`), attributed post (`POST /channels/:id/posts`), and channel-link read/write (`/external-links`, board-core #149 slice 2). |
| `format`      | board↔Slack message shaping: render an outbound-reflect as Slack mrkdwn (attribution + HTML-escape + length-cap), a degraded plain variant, the relay-plan escalation, and the traversal-safe `@agent` parse. |
| `sync`        | Pure bidirectional planning: firehose events → Slack posts (+ cursor), inbound Slack → attributed board post. Channel map injected as a resolver. |
| `resolver`    | The board↔Slack `ChannelMap` (bidirectional lookups) built from config links and/or the board's `external-links` table. |
| `main`/`runner` | The async Socket Mode daemon (feature `transport`): outbound poll loop (cursor-persisted, relay-resilient) + inbound Socket Mode listener + a 60s channel-map refresh. Not unit-tested (live WebSocket); the gate is the lib's `cargo test`. |

Data flow:

```
board firehose (channel.outbound_reflect)                 Slack Socket Mode (message events)
        │  poll_events                                             │  on_push_event
        ▼                                                          ▼
   sync::plan_outbound ──► format::render ──► Slack post      sync::plan_inbound ──► board::post_raw
   (channel map resolves board_channel → Slack channel)       (Slack channel → board channel; attributed
                                                               external_author = slack:<user>)
```

## Build

```sh
# Pure core + tests (fast — no async tree):
cargo test -p slack-bridge

# The daemon binary (pulls slack-morphism/tokio/hyper — REQUIRED to build the bin):
cargo build -p slack-bridge --features transport --release
# → target/release/slack-bridge
```

The `transport` feature is `required-features` on the `[[bin]]`, so the default `cargo test --workspace`
/ `nix flake check` never compile the heavy async tree.

## Run

```sh
slack-bridge --config /path/to/slack-bridge.toml
```

`--config` is the **only** input — there is no env-var configuration (mandate #159). With no tokens in the
config the daemon stays alive but **idle** (fail-soft), so it is safe to deploy before the Slack app
exists.

## Config (TOML)

```toml
bot_token    = "xoxb-..."     # Slack bot token (needs chat:write etc.)
app_token    = "xapp-..."     # Slack app-level token, scope connections:write (Socket Mode)
# channel    = "C0123ABCD"    # optional default channel
board_api    = "http://127.0.0.1:8880/board/api"  # optional; default shown
default_to   = "concierge"    # optional; default
bridge_agent = "slack-bridge" # optional; the board agent id this bridge posts inbound as
state_dir    = "/var/lib/slack-bridge"  # optional; defaults to the config file's dir. Holds the firehose
                                        # cursor — MUST be writable + durable across restarts.

# Static channel links (0+). Usually EMPTY in prod — links live in the board's external-links table and
# are read/registered at runtime. These are a local/dev override (applied AFTER board links; last wins).
[[channel_map]]
board_channel_id = 7
slack_channel    = "C07ABC"
```

- Both tokens present ⇒ live; either missing ⇒ dormant (valid).
- Unknown keys are rejected (`deny_unknown_fields`) — a typo surfaces as a "malformed config" (fail-soft
  dormant), not a silent drop.
- `Debug` on the config **redacts** the tokens — they never print into logs.

## Deploy (camshaft/dotfiles #153, fleet-tunnel)

- systemd role runs `slack-bridge --config <path>`; `Restart=always` (fail-soft startup makes this safe).
- The config is delivered as the **agenix-decrypted TOML secret** `slack-bridge.toml.age` (mode 0400) —
  **not** an env file / `EnvironmentFile=` (mandate #159).
- `StateDirectory=slack-bridge` (or any persistent writable dir) for the firehose cursor.
- Needs localhost reach to the board front-door (`board_api`).
- No subprocess/watchdog needs — it's a pure network daemon.

## Channel map

- **Board-backed (prod):** at startup and every 60s the daemon reads
  `GET /external-links?source=slack&board_kind=channel` and rebuilds the map. A link registered while the
  daemon runs is picked up **without a restart**.
- **Register a link:** `register_channel_link(board_channel_id, slack_channel)` →
  `POST /external-links {source:"slack", external_id, board_kind:"channel", board_id}` (idempotent).
- **Config `[[channel_map]]`** is a static override, applied after board links (last wins).

## Cutover runbook (#154, design §8 — jointly with concierge)

Invariant: **the operator never loses Slack contact.** Rollback at any phase = concierge falls back to its
file inbox (kept until P4).

- **P0/P1 — wire the operator-DM channel + dual-run.**
  1. Deploy the daemon live with the operator's real Slack tokens (#153).
  2. Ensure an operator-DM **board channel** exists (board #90 DM-as-channel).
  3. Register the link: `register_channel_link(<operator_dm_board_channel_id>, "<operator Slack DM id>")`.
     The running daemon picks it up within 60s — no restart.
  4. Daemon runs while the concierge still also uses its file inbox (dual-run).
- **P2 — concierge dual-writes** asks/answers to the operator-DM board channel **and** its file inbox.
  Prove the round-trip both ways.
- **P3 — concierge board-native** (board DM becomes the operator interface); retire the concierge
  inbox-mirror. **Trigger = operator go after P2 is proven (D5).**
- **P4 — retire** the legacy cadenza `fleet/slack-bridge` deploy + `slack-threads.json`; keep a
  board-heartbeat liveness check.

### E2E acceptance
1. concierge board-DM post → operator sees it in their Slack DM.
2. operator Slack reply → attributed board post (`external_author = slack:<user>`) in the DM channel.
3. a **non-concierge** board post does **NOT** reach Slack (guaranteed board-side by #150's concierge-only
   OUT authz — the bridge only ever sees an authorized `channel.outbound_reflect` event).

## Operational notes

- **Firehose cursor** (`<state_dir>/slack-bridge.cursor`): advanced only past terminally-handled events; a
  restart resumes without gap. First run initializes at the firehose head (skips backlog).
- **Relay resilience:** a Slack post that deterministically fails on content degrades (plain variant) then
  quarantines — it never head-of-line-blocks the outbound loop.
- **No echo loop:** inbound posts use the bridge's own `bridge_agent` as `sender`; it isn't in a channel's
  `outbound_authors`, so its posts don't reflect back OUT.

## Follow-ons (non-blocking)

- Inbound reply **threading** (Slack `thread_ts` ↔ board post seq) — needs board-core #151 slice 2; posts
  top-level meanwhile.
- Board-heartbeat liveness for P4.
