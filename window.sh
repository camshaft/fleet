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

# task_347: GUARANTEE a known-good PATH for the agent process and every shell it spawns. A fleet agent's Bash
# tool-calls intermittently spawned with a stripped PATH (coreutils / git / curl / nix all "command not found",
# recoverable only via absolute /usr/bin/... paths) — a per-invocation tax seen across agents + days
# (corroborated). The tmux window can inherit a minimal/empty PATH from the launching daemon, and a tool shell
# that then fails to source a login profile has no usable PATH. APPENDING the standard system + nix-profile bin
# dirs here (before `exec claude`, so claude and all its child shells inherit it) makes the baseline PATH always
# complete while leaving any existing entries FIRST (a repo-/user-preferred tool still wins); the essentials are
# guaranteed present as a fallback, so a bare `git`/`curl`/`nix`/coreutil always resolves. A dir that does not
# exist on this host is harmless (the shell just skips it).
export PATH="${PATH:+$PATH:}/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin:${HOME:-}/.nix-profile/bin:/nix/var/nix/profiles/default/bin"

# task_596: install a `paste_create` helper on PATH so an agent can turn text into an Amazon Paste Tool
# share link (api.paste.tools.amazon.dev) straight from its Bash tool — no MCP, no daemon, no per-instance
# process; the host's Midway session (~/.midway/cookie) is the only credential and the service scales on the
# AWS side (the operator constraint is that a shared capability must scale to thousands of agents, so a
# remote REST service with no local tooling is the right shape — cameron's call: shell helper only, no MCP
# tool). The API is NOT secret-scanned, so the helper defaults to `restricted` visibility; an agent redacts
# and opts into a wider audience explicitly. It is WRITTEN HERE rather than shipped as a separate repo file
# so it travels wherever window.sh is deployed, and placed on PATH BEFORE `exec claude` so claude and every
# Bash-tool shell inherit it (the same inheritance mechanism task_347 relies on above).
FLEET_BIN="${HOME:-/tmp}/.fleet/bin"
if mkdir -p "$FLEET_BIN" 2>/dev/null; then
  cat > "$FLEET_BIN/paste_create" <<'PASTE_CREATE_EOF'
#!/usr/bin/env bash
# paste_create — turn text into an Amazon Paste Tool share link (api.paste.tools.amazon.dev).
# Pipe content in (or pass a FILE). The host's Midway session (~/.midway/cookie) is the credential —
# no MCP, no daemon, no per-instance process. Prints the browserUrl on success.
#
#   some-command 2>&1 | paste_create [-t TITLE] [-v VIS] [-u alias1,alias2] [-k TTLDAYS] [-s SRCURL] [FILE]
#
#   -v VIS    restricted (DEFAULT) | private | public
#   -u LIST   comma-separated aliases allowed to read (only meaningful with restricted)
#   -k TTL    1 | 7 | 30 | 365 | -1 (never)   [default 7]
#   -t TITLE  paste title
#   -s URL    internal https:// provenance link
#
# WARNING: this API is NOT secret-scanned — it stores exactly what you send. Default is `restricted`;
# redact secrets yourself and never use `public` without a named reason.
set -euo pipefail

API="https://api.paste.tools.amazon.dev/api/paste"
COOKIE="${HOME:-}/.midway/cookie"
title="" ; vis="restricted" ; ttl="7" ; users="" ; src="${PASTE_SOURCE:-}"

while getopts "t:v:u:k:s:h" opt; do
  case "$opt" in
    t) title="$OPTARG" ;;
    v) vis="$OPTARG" ;;
    u) users="$OPTARG" ;;
    k) ttl="$OPTARG" ;;
    s) src="$OPTARG" ;;
    h) sed -n '2,16p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "paste_create: bad option (try -h)" >&2; exit 2 ;;
  esac
done
shift $((OPTIND - 1))

infile="/dev/stdin"
[ "${1:-}" != "" ] && { [ -f "$1" ] || { echo "paste_create: no such file: $1" >&2; exit 2; }; infile="$1"; }

[ -f "$COOKIE" ] || { echo "paste_create: no Midway cookie at $COOKIE — run 'mwinit' on the host" >&2; exit 3; }
command -v jq   >/dev/null || { echo "paste_create: jq not found on PATH" >&2; exit 3; }
command -v curl >/dev/null || { echo "paste_create: curl not found on PATH" >&2; exit 3; }

# Build the request body with jq --rawfile (never string-interpolate content — newlines/quotes break JSON).
body=$(jq -n --rawfile content "$infile" \
  --arg vis "$vis" --argjson ttl "$ttl" --arg title "$title" --arg users "$users" --arg src "$src" \
  '{ content: $content, visibility: $vis, ttlDays: $ttl }
    + (if $title != "" then { title: $title }                       else {} end)
    + (if $users != "" then { allowedUsers: ($users | split(",")) } else {} end)
    + (if $src   != "" then { sourceUrl: $src }                     else {} end)')

resp=$(curl -sS -L -b "$COOKIE" -c "$COOKIE" -X POST "$API" -H 'Content-Type: application/json' -d "$body")
url=$(printf '%s' "$resp" | jq -r '.browserUrl // empty' 2>/dev/null || true)

if [ -n "$url" ]; then
  printf '%s\n' "$url"
else
  echo "paste_create: no share link in response (Midway session lapsed? run 'mwinit'). API said:" >&2
  printf '%s' "$resp" | head -c 400 >&2
  echo >&2
  exit 4
fi
PASTE_CREATE_EOF
  chmod +x "$FLEET_BIN/paste_create" 2>/dev/null || true
  export PATH="$FLEET_BIN:$PATH"
fi

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
