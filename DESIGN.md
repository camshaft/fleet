# DESIGN: extract the fleet into a standalone multi-repo orchestrator

Status: **greenlit to design-doc stage** (operator, 2026-09-05). Repo created. **REARCHITECTED
2026-09-28** — see the board-offload section immediately below; the 2026-09-05 architecture beneath it is
partly SUPERSEDED. Owner: `v-fleet-tooling`. Operator direction (2026-09-28): make `camshaft/fleet` the
real fleet, get it in shape, then **deprecate the cadenza-embedded fleet functionality entirely**; prefer
a real language (Rust) over bash for everything.

---

## 2026-09-28 REARCHITECTURE — the task board offloads the fleet "core"

Since 2026-09-05 a standalone **task board** (an MCP + REST server with a live agent roster) shipped and
became the fleet's coordination substrate. It already provides, as durable server features, most of what
the 2026-09-05 plan was going to REIMPLEMENT in the fleet core:

- **Agent roster / registry** — `register_agent` / `update_agent` / `get_agent` / `list_agents`, each agent
  carrying `charter` + an arbitrary `metadata` bag (role/model/effort/interval/**repos**/…). This REPLACES
  `registry.json` and the decentralized per-repo `fleet.toml` rosters. **The board is the registry.**
- **Messaging bus** — `send_message` / `get_messages` / `check_notifications` / `get_events` /
  `subscribe`. This REPLACES the file inbox (`inbox/`, `.delivery-seq`, `processed/`). **Agents coordinate
  through the board.**
- **Charters** — the `charter` field is the source of an agent's role prompt. This REPLACES
  `loops/<role>.md` as the authored source (a checked-in seed remains a bootstrap fallback).
- **Task tracking** — projects/tasks/comments/status replace the ad-hoc backlog.

**Consequence: the fleet crate does NOT reimplement any of that.** The in-progress "lift `registry.json` +
inbox byte-identical from cadenza `fleet.rs`" in `crates/fleet/src/main.rs` is therefore SUPERSEDED — that
state lives on the board now. What remains for `fleet` to own is exactly the set of things the board can't:

1. **Workspace materialization** — a per-agent workspace directory holding one git **worktree per repo**
   off a shared **bare-mirror store** (`~/.fleet/mirrors/<repo>.git` + `~/.fleet/agents/<agent>/<repo>`),
   so an agent works across many repos with N agents sharing a repo's objects. (Reference logic prototyped
   in cadenza `fleet/agent-workspace.sh` + `fleet/board-reconcile-workspaces.py` — to be ported to Rust
   here; see cadenza `fleet/DESIGN-fleet-per-agent-workspaces.md`.)
2. **Spin-up / launch** — launch an agent process (tmux window) in its workspace with the board MCP
   available and a **board-sourced charter** kickoff, at the agent's declared model/effort/interval.
   (Reference: cadenza `window.sh`; port to Rust, board-driven, no cadenza framing.)
3. **Liveness / orchestration** — the watchdog, keyed off the board's `last_seen`/`status` (not file
   heartbeats): detect wedged/idle/dead agents, re-arm/reissue, escalate. Host-health crons (tmp/disk/cpu)
   stay host-level and board-independent.
4. **Per-repo ADAPTER** — how to gate/build/merge in a given repo (the one thing the board doesn't know).
   Stays per-repo; cadenza plugs in its nix gate; a plain repo declares none.

**Revised, board-offloaded scope for the `fleet` crate (Rust — not bash):**
- a small **board client** (roster read, charter fetch, messaging, presence) over the board MCP/REST;
- the **workspace materializer** (mirrors + worktrees);
- **spin-up** (launch a board-declared agent into its workspace with a board charter);
- **liveness/watchdog** off the board;
- host-health;
- the per-repo adapter interface.

**Revised plan:**
- **P1 — board client + workspace materializer + spin-up — DONE + PROVEN LIVE.** PR #1 (this
  rearchitecture) · #2 (read-only REST board client + `fleet spin-up` dry-run) · #3 (`spin-up --apply`:
  materialize a worktree off a shared bare mirror + launch a board-native window with a self-discovery
  kickoff) · #4 (pre-trust the fleet root so launch is fully unattended). `fleet spin-up v-task-board
  --apply` spun the pilot up end-to-end: it materialized `~/.fleet/agents/v-task-board/task-board` off the
  shared mirror, read its OWN charter via `get_agent`, and now coordinates entirely via the board — it has
  autonomously shipped real PRs in its target repo since. Cadenza-nonspecific, board-native, unattended.
- **P2 (in progress)** — liveness/watchdog off the board `last_seen` + host-health + the per-repo adapter
  interface; migrate more agents onto the `fleet spin-up` path. **Read side landed** (PR #6): `fleet status`
  reads the board roster and classifies each agent by heartbeat age — `live` (<15m) / `quiet` (<1h) /
  `STALE`, `--stale-only` for the watchdog's candidate set. It confirms the board `last_seen` is fresh only
  for **board-native** agents (the `v-task-board` pilot reads `live`; legacy file-hub agents read `STALE`
  because they heartbeat to the file hub, not the board) — so the acting side of the watchdog is meaningful
  as agents migrate onto the `fleet spin-up` path, and until then it must not treat a legacy agent's stale
  board stamp as dead. Still to land: the acting side (re-arm/reissue/escalate), host-health, the adapter.
- **P3** — retire `registry.json` and deprecate the cadenza-embedded fleet once parity is proven.

The 2026-09-05 architecture below is retained for history; treat the board-offloaded scope above as the
current plan wherever the two conflict (registry/inbox/messaging/roster → the board, not the fleet crate).

---

## Motivation

The fleet is embedded in cadenza but has organically become general-purpose orchestration (messaging,
window management, watchdog, worktree lifecycle). The `dcquic-perf` agent (works in `camshaft/s2n-quic`,
uses the fleet only for comms) proved a fleet agent can drive a NON-cadenza repo end-to-end. The operator
wants to extract the fleet into its OWN repo so it can run parallel work across MANY repos, with cadenza
becoming just one target among many.

## Confirmed operator rulings (2026-09-05)

- **Hub location: `~/.fleet`** — host-global, outside any target repo.
- **A standalone `fleet` binary on PATH** — used by every agent for all comms; no repo checkout needed to
  talk to the hub.
- **No gates/build/corpus in fleet core.** Fleet core = **general messaging + window management +
  orchestration ONLY.** Per-repo gate/build/merge logic lives in that repo's adapter, never in core.
- **The slack-bridge + its tooling MOVE into the fleet repo** (it's general messaging infra).
- **Decentralized per-repo rosters**: each target repo carries its OWN checked-in fleet config declaring
  that repo's persistent agents; the hub holds runtime state; `fleet up` reconciles declared → running.

## Architecture

### Fleet CORE (the standalone repo) — messaging + windows + orchestration only

- Message bus: registry of live agents, `inbox`/`send`/`heartbeat`, delivery-seq, `processed/` archive.
- Window/session management: `window.sh` launcher, tmux session lifecycle, the loop kickoff, DISALLOW_ASK.
- Orchestration: the watchdog (compact-nudge / drain-nudge / reissue-loop / wedge-restart / update-banner),
  `check-leases` (a general concurrency limiter — NOT gate-specific), the git-worktree-per-agent model.
- Host-health crons: cpu-monitor, prune-tmp-inodes, prune-stale-targets, warm-keep, reap-wedged-nix-clients,
  drain-nudge, compact-nudge (all guard host/disk/tmux, not cadenza).
- **Slack-bridge**: the inbound/outbound Slack↔fleet-message daemon + its config (general messaging infra).
- The generic role bodies + the fleet contract (`AGENTS-fleet.md`), with `perf-agent` as the generic
  foreign-repo template.
- `rebase-freshness` advisory, the send/leak guards — already repo-agnostic, stay in core.

### The HUB decouple (the central change)

Today `Fleet::new` resolves the hub via `git --git-common-dir` of the cadenza worktree →
`<cadenza>/.claude/fleet`. Standalone: the hub is `~/.fleet` (or `$FLEET_HUB`), selected by **explicit
config, not git-common-dir**. Every agent's `fleet` binary resolves the SAME hub regardless of which target
repo it works in. The binary already self-locates independent of cwd (proved by the foreign-repo pattern);
we swap the derivation from git-common-dir to explicit hub config. Hub layout unchanged
(`registry.json`, `inbox/`, `queue/`, `check-leases/`, `.delivery-seq`, cron `.last-run` stamps).

### Per-repo ADAPTER + decentralized roster (checked into each target repo)

Each target repo carries a checked-in `fleet.toml` (or `.fleet/config.toml`) declaring:
- **Identity/location**: repo path on the host, remote, base branch to cut agent worktrees from.
- **Gate/build/merge model** (the per-repo adapter, NOT in core): how to gate a change (a command, or
  "none"), how to land (direct push / `gh pr create` + admin-merge / a pr-sync-style integrator), any
  pre-merge hooks. Cadenza plugs in its nix gate + pr-sync + `.gate-baseline` merge driver + corpus guards
  here; a plain repo declares a trivial gate or none.
- **Declared roster (desired-state)**: this repo's PERSISTENT agents — name, role, model, effort, interval.

**Declared-vs-runtime reconciliation** (the design nuance the operator flagged): the per-repo config is
DECLARATIVE desired-state (checked in, decentralized). The hub holds ACTUAL runtime state (live windows,
heartbeats, inboxes, leases — inherently host-central). `fleet up <target>` reconciles: read the target's
declared roster → for each declared agent not running, mint its worktree (off that repo's base) + launch;
report drift (running-but-undeclared, declared-but-dead). The central `registry.json` becomes pure runtime
state; the DECLARED set is the union of all targets' checked-in rosters. One host runs one hub serving many
targets; a target's roster travels with the repo (clone the repo elsewhere → its fleet config comes too).

### What STAYS in cadenza (as its adapter)

Cadenza's build/gate/corpus/codegen/bench `xtask` subcommands stay entirely in cadenza (they are the
compiler's build tool, never fleet). Cadenza's `fleet.toml` adapter declares: base = trunk/origin-main,
gate = the nix local-gate, merge = pr-sync/self-merge, plus its `.gate-baseline` merge driver +
corpus-vanished pre-commit guard + baseline-drift cron (all cadenza-corpus concepts). The cadenza-shaped
`vertical` role becomes a cadenza-adapter role; `perf-agent` is the generic template in core.

### window.sh role-aware tick (the deferred generalization, now in-scope)

The kickoff/watchdog TICK is currently hardcoded cadenza framing ("cargo xtask fleet sync + pr-sync").
Generalize it to per-role/per-target: the tick recipe comes from the role + the target adapter, so a
foreign-repo agent gets a correct tick natively instead of overriding it in prose (as `perf-agent` does now).

## Phased plan (live-fleet-safe: ~33 agents + dcquic-perf must not break)

- **P1 — Scaffold (non-disruptive):** create the fleet repo; lift `fleet.rs` into a standalone `fleet`
  crate/binary; make the hub explicit config DEFAULTING to the current `<cadenza>/.claude/fleet`, so
  behavior is byte-identical and the live fleet is untouched. Build + unit-test in isolation. Audit + cut
  the small shared surface with cadenza-xtask (e.g. the check-lease pool shared with `gate`).
- **P2 — Adapter + generalize (non-disruptive):** define the `fleet.toml` schema (identity + gate/merge +
  declared roster); write cadenza's adapter reproducing today's exact behavior; land the role-aware tick +
  `ensure_worktree(target)` + the reconciliation `fleet up <target>`. Migrate the slack-bridge into the
  repo (still pointed at the current hub). Verify cadenza adapter is behavior-identical.
- **P3 — CUTOVER (the one risky step):** flip the live fleet to the standalone `fleet` binary + `~/.fleet`
  hub, cadenza as a target. Quiet window; migrate the hub in place (or symlink) so registry/inbox/queue/
  leases survive; re-home crons (system crontab → new paths) + hooks + the slack-bridge to the new hub.
  Cadenza-xtask fleet stays working as ROLLBACK until the cutover is proven.
- **P4 — Prove multi-repo:** stand up a 2nd target repo (s2n-quic) through the generalized native path,
  retiring the comms-shim special-case.

## Effort + risk

Medium-large; strongly favors INCREMENTAL. P1+P2 are low-risk and independently valuable (a cleaner,
testable, repo-agnostic core + adapter model) EVEN IF we never cut over — which is why I recommend doing
them first and reassessing before the P3 cutover. Big risks + mitigations:
- Breaking the live fleet mid-migration → cadenza-xtask fleet stays fully working until P3; rollback = point
  crons/launcher back at cadenza-xtask.
- Hub relocation losing runtime state → in-place/symlink migration + a backup snapshot; atomic.
- Cron/hook/slack-bridge re-homing → careful re-pointing (owned by v-fleet-tooling, low-surprise).
- xtask coupling → audit + cut the shared surface in P1.

## Repo name (for operator approval)

Recommend **`fleet`** (`github.com/camshaft/fleet`) — matches the binary, obvious, minimal. Alternatives if
a distinct name is preferred: `armada`, `flotilla`, `conductor`. (Binary stays `fleet` regardless.)

## Recommended sequencing

Do **P1+P2 (non-disruptive) first, then reassess** before committing to the P3 cutover — NOT a big-bang.
This lets the operator SEE the standalone binary + cadenza adapter + a 2nd-repo dry-run working with the
live fleet unchanged, and gates the concentrated risk behind that evidence.

## Open items gating execution

1. Operator approval of this doc.
2. Repo name (recommend `fleet`).
3. Confirm P1+P2-first-then-reassess (vs full-commit).

On approval → turn the phased plan into a concrete task breakdown and begin P1. Until then, nothing is
created or moved. See `AGENTS-fleet.md` (the contract that moves to core) and the fleet memory
`foreign-repo-fleet-agent-pattern` (the mechanics this builds on).
