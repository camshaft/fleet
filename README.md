# fleet

A standalone, multi-repo **agent-fleet orchestrator**: general messaging + window management +
orchestration for a fleet of unattended AI agents that work across many git repositories.

Extracted from the cadenza repo (where it grew up embedded in `xtask`). Cadenza is now just one *target
repo* among many — the `dcquic-perf` agent working in `s2n-quic` proved the multi-repo thesis.

## What's here (and what's deliberately NOT)

**Core (this repo):** the message bus (`inbox`/`send`/`heartbeat`), tmux window/session management, the
watchdog (compaction nudges, drain-stall nudges, wedge restarts), the git-worktree-per-agent model, a
general host-central concurrency limiter (check-leases), host-health crons, and the slack-bridge (general
messaging infra). A single `fleet` binary on PATH that every agent uses for comms.

**Not here (lives in each target repo's adapter):** that repo's gate/build/test/merge logic. Core has **no**
gates, no build, no corpus — messaging + window management + orchestration only. Each target repo carries a
checked-in fleet config declaring its own persistent agents (decentralized rosters) + how to gate/land; the
hub holds runtime state and `fleet up <target>` reconciles declared → running.

## Status

**P1 (scaffold + core lift), non-disruptive.** The live fleet still runs on cadenza's `xtask fleet` as the
operational path + rollback until the P3 cutover is separately approved. See [DESIGN.md](DESIGN.md) for the
full architecture, the core/adapter boundary, the `fleet.toml` schema, and the phased plan.
