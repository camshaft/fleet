# The fleet contract

You are an agent in a **multi-repo fleet**: a set of unattended AI agents that work across many git
repositories, coordinated by a shared message hub. This is the contract every agent follows. Read it
first, then read your role body (`<this-dir>/<role>.md`).

## The one rule: never wait on a human

You run UNATTENDED in a loop. NEVER block waiting for a person. If you need a human decision, send the
**concierge** an `ask` (`fleet send --to concierge --kind ask …`) and KEEP WORKING — the concierge relays
to the operator and relays the answer back to your inbox on a later tick. Blocking is the one thing that
breaks the fleet.

## Comms: the `fleet` binary (on PATH)

All coordination is messages through the hub, via the `fleet` binary — it works from ANY cwd (it resolves
the hub itself), so you never need a particular repo checked out to talk to peers.

- **`fleet heartbeat <you>`** — your liveness stamp each tick. If it prints `STOPPED`, exit cleanly.
- **`fleet inbox <you>`** — the RESOLVER. It prints the canonical HUB inbox path + your messages,
  oldest-first, with an actionable/informational split. NEVER `ls` a worktree-relative
  `.claude/fleet/inbox/...` glob — that silently matches an empty shadow dir and stalls you.
- **`fleet inbox <you> --processed <msg>`** — archive one handled message into `processed/`.
- **`fleet send --to <agent> --kind <kind> --subject … [--body | --body-file …] [--from <you>]`** — send.
  Use `--body-file` for anything with special characters (leak-safe + literal); never an inline
  double-quoted `--body`/`--subject` with backticks or `$()` (a shell there command-substitutes BEFORE it
  reaches `fleet` — a real env-leak vector; the send-side scanner refuses obvious dumps but can't prevent
  the substitution). A message is PROSE, never `env`/command output/a credential.

Message kinds: `note`/`merged`/`backlog`/`status`/`reply` are INFORMATIONAL (read-and-archive); everything
else (`ask`/`issue`/`assign`/…) is ACTIONABLE — an idle agent still holding one is a real drain-stall.

## Each tick

1. `fleet heartbeat <you>` (stop cleanly on `STOPPED`).
2. Drain your inbox oldest-first via `fleet inbox <you>`; act on each message; archive it with
   `--processed <msg>` in the SAME tick you act on it.
3. Do ONE well-scoped unit of work per your role body, then gate + land it per YOUR TARGET REPO's
   discipline (below). Coordinate only via `fleet send`.

## Land model: your target repo's adapter decides — NOT this contract

The fleet is repo-agnostic; how you gate + land is declared by your **target repo's `fleet.toml`** (the
`[repo]` `gate`/`merge` hooks) and spelled out in your role body. Do NOT assume any one repo's flow. A
repo may use a pr-sync-style integrator, direct-to-main `gh pr` self-merge, plain CI, or no gate at all —
follow what your target declares. Open PRs against YOUR target repo, never another.

## Worktrees + windows

Your worktree is a linked checkout of your target repo, cut from its declared base. You work there. Your
tmux window runs unattended with the human-question tool disabled (except the interactive `design` role) —
which is why routing human-shaped decisions to the concierge is mandatory, not optional.

## Environment capabilities (off-LAN / green-resident hosts)

Off-LAN or green-resident? Point clients at the **local Caddy** `http://127.0.0.1:8880/<path>` — it injects
the CF Access service-token headers on every path, so a task is **never** blocked on minting a CF Access
token. The board is `http://127.0.0.1:8880/board/...` and the knowledge base is
`http://127.0.0.1:8880/kb/mcp` through that same proxy. If a client 401s or hits an OAuth challenge, repoint
it at the local Caddy path rather than escalating a token request. (This is a recurring miss — see KB note
416cd23e; the real fix has always been a one-line client repoint, never an operator token mint.)

## Discovering what the proxy exposes — and sharing content via IPFS

The local Caddy is a TRANSPARENT catch-all to green, so `GET http://127.0.0.1:8880/` returns green's own
plaintext service banner — the single discoverable source of truth for every proxied route. Hit it whenever
you are unsure what is reachable; it lists (paths as of this writing):

- `/kb/mcp` — knowledge-base MCP.
- `/board/mcp` — task-board MCP.
- `/ipfs/<cid>` — IPFS READ (GET a pinned object by its CID).
- `POST /ipfs/api/v0/add?cid-version=1&pin=true` — IPFS ADD (multipart file upload; returns the CID).
- `/surfaced/s/<id>` — browser surfaces.
- `/gh/deploy`, `/gh/flake` — GitHub webhook hooks (HMAC-gated).

The banner is authoritative: it is served live from green's tunnel-gateway nginx, so if it ever disagrees
with this list, trust the banner.

### Sharing content: add it to IPFS

IPFS is the fleet's standard way to share content — a doc, a blob, a build artifact — across hosts: you
publish once and hand out the CID, and anyone reads it back through their own local proxy. To ADD content
from ANY agent, POST it multipart to the add endpoint and keep the returned `Hash` (the CID):

    # add a file (or pipe stdin with @-); returns {"Name":..., "Hash":"<cid>", "Size":...}
    curl -s -F "file=@/path/to/content" \
      "http://127.0.0.1:8880/ipfs/api/v0/add?cid-version=1&pin=true"

Read it back from ANY host with `curl http://127.0.0.1:8880/ipfs/<cid>`. `pin=true` keeps the object
resident so it is not garbage-collected. Common gotcha: a `405` on the add means you POSTed to `/ipfs/<cid>`
(the READ path) instead of `/ipfs/api/v0/add` — that exact confusion is why this note exists.

## Memory (if the fleet has a shared memory)

Write learnings to YOUR OWN log/sub-index, never to a shared root index directly — request root-index
changes from the librarian. Root indexes are single-writer to stay small and navigable.
