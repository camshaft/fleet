#!/usr/bin/env bash
# window.sh — the standalone fleet's tmux window launcher (generalized from cadenza's, multi-repo).
#
# The Rust `fleet` binary spawns a tmux window per agent and runs this inside it. This resolves the
# agent's config via `fleet describe` (KEY=VALUE lines), cd's into the agent's TARGET-REPO worktree, and
# launches `claude` with a role-aware kickoff. KEY DIFFERENCE from cadenza's window.sh: the TICK is NOT
# hardcoded with cadenza's "cargo xtask fleet sync + pr-sync" framing — the role body + the target repo's
# adapter govern how work is gated/landed. Comms are the standalone `fleet` binary on PATH (no repo
# checkout needed to talk to the hub — the hub is $FLEET_HUB / git-common-dir, resolved by the binary).
set -uo pipefail

AGENT="${1:?usage: window.sh <agent-name>}"

# `fleet` on PATH is the comms + config binary. Resolve the agent's launch config (KEY=VALUE for eval).
CONFIG="$(fleet describe "$AGENT")" || {
  echo "window.sh: no such agent '$AGENT' in the registry (or `fleet` not on PATH)" >&2
  exit 1
}
eval "$CONFIG"   # sets WORKTREE, ROLE, MODEL, EFFORT, INTERVAL, VERTICAL, AREA, DISALLOW_ASK

: "${WORKTREE:?registry gave no WORKTREE for $AGENT}"
: "${ROLE:?registry gave no ROLE for $AGENT}"

if [ ! -d "$WORKTREE" ]; then
  echo "window.sh: worktree $WORKTREE missing — run 'fleet up --provision <fleet.toml>' first" >&2
  exit 1
fi
cd "$WORKTREE"   # the agent works in its TARGET-repo worktree

# Role bodies + the contract live in the fleet repo's loops/ (core), materialized to $FLEET_LOOPS (the
# hub copy) at `fleet up`. Default to the checked-in loops/ next to this script if unset.
FLEET_LOOPS="${FLEET_LOOPS:-$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/loops}"

VNOTE=""
[ -n "${VERTICAL:-}" ] && VNOTE=" Your vertical is '$VERTICAL' in subsystem '${AREA:-}'."

# The recurring TICK prompt (passed to /loop). MUST be non-empty (an empty /loop prompt is a no-op that
# schedules nothing). ROLE-AWARE + target-agnostic: heartbeat + drain inbox (via the `fleet` binary), then
# do ONE unit of work per the role body — the role body / target adapter own how work is gated + landed
# (NOT hardcoded here; a cadenza agent's role says pr-sync/gate, a plain-repo agent's says gh-pr, etc.).
TICK="Run one tick of your role ($ROLE)$VNOTE: (1) 'fleet heartbeat $AGENT' (stop cleanly if it prints \
STOPPED); (2) drain your inbox by listing it with 'fleet inbox $AGENT' (the RESOLVER — it prints the \
canonical HUB inbox path; NEVER ls a worktree-relative '.claude/fleet/inbox/...' glob, which silently \
matches an empty shadow dir and stalls you), oldest-first — act on each message, then archive it with \
'fleet inbox $AGENT --processed <msg>'; (3) do ONE well-scoped unit of work per $FLEET_LOOPS/$ROLE.md, \
following THAT role's gate + land discipline (the role body / your target repo's fleet.toml adapter own \
how you gate and land — this launcher does not assume cadenza's pr-sync). Coordinate with peers only via \
'fleet send'; if you need a human decision send the concierge an 'ask' and keep working — never wait."

KICKOFF="You are the fleet agent named '$AGENT' (role: $ROLE), running UNATTENDED.$VNOTE FIRST read \
$FLEET_LOOPS/AGENTS-fleet.md (the fleet contract — inbox protocol, the land model, never wait on a human). \
THEN read $FLEET_LOOPS/$ROLE.md (your role). Your worktree is $WORKTREE. LIST your inbox with 'fleet inbox \
$AGENT'. Then start your recurring loop by running EXACTLY this (the interval AND a non-empty tick prompt): \
/loop $INTERVAL $TICK"

# APPROVALS: a fleet agent loops unattended, so a permission prompt would stall it. The operator runs
# these windows with the approval system OFF (trusted host + repos) — hence --dangerously-skip-permissions.
# DISALLOW_ASK (all roles except the terminal-interactive `design`) denies the human-question tool so no
# unattended agent can pop an interactive prompt. Arg order: the variadic --disallowedTools goes FIRST
# (followed by another flag) so it can't slurp the positional KICKOFF; the prompt lands last.
CLAUDE_ARGS=()
if [ "${DISALLOW_ASK:-1}" = "1" ]; then
  CLAUDE_ARGS+=(--disallowedTools AskUserQuestion)
fi
CLAUDE_ARGS+=(--effort "${EFFORT:-high}" --model "$MODEL" --dangerously-skip-permissions)

echo "window.sh: launching '$AGENT' (role=$ROLE model=$MODEL effort=${EFFORT:-high} interval=$INTERVAL) in $WORKTREE"
exec claude "${CLAUDE_ARGS[@]}" "$KICKOFF"
