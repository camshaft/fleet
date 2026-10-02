#!/usr/bin/env python3
"""memory-sync: bridge the Claude Code memory directory and the board (task_848).

Safety net for the operator directive (task_826 comment_3477): an agent that writes the native
Claude Code memory FILE directly (instead of calling board-memory) still gets its memory onto the
board. Reuses the shared frontmatter parser (memory_convention.parse_memory) and the board-memory CLI
convention wrapper, so a file-written memory and a tool-written one are byte-identical in shape.

Stable entrypoint for v-fleet-tooling to wire from hooks:
  memory_sync.py --direction file-to-board --agent <self> --memory-dir DIR
  memory_sync.py --direction file-to-board --repo  <repo> --memory-dir DIR
  memory_sync.py --direction board-to-file --agent <self> --memory-dir DIR
v-fleet-tooling wires SessionEnd + a periodic timer -> file-to-board, SessionStart -> board-to-file,
and discovers the per-project memory dir (~/.claude/projects/<encoded-cwd>/memory).

file-to-board is idempotent: a per-slug content hash in the state file skips unchanged memories, and
the board-memory deterministic path (agents/<agent>/<slug> or repos/<repo>/<slug>) versions a changed
one in place (no duplicates). An agent syncs only its own scope, so file->board is same-writer.
"""
import argparse
import hashlib
import json
import os
import subprocess
import sys

import yaml  # emit canonical frontmatter on board->file reconstruction

from memory_convention import parse_memory  # shared frontmatter parser (one write-shape convention)

SKIP_NAMES = {"MEMORY.md", "README.md"}


def sha256(text):
    return hashlib.sha256(text.encode("utf-8", "replace")).hexdigest()


def load_state(path):
    try:
        with open(path, encoding="utf-8") as f:
            return json.load(f)
    except (OSError, ValueError):
        return {}


def save_state(path, state):
    os.makedirs(os.path.dirname(os.path.abspath(path)), exist_ok=True)
    with open(path, "w", encoding="utf-8") as f:
        json.dump(state, f, indent=0, sort_keys=True)


def scope_flags(args):
    if args.agent:
        return ["--agent", args.agent]
    return ["--repo", args.repo]


def scope_prefix(args):
    """The document path prefix for this scope -- stripped from a full board path to get the slug."""
    if args.agent:
        return f"agents/{args.agent}/"
    return f"repos/{args.repo}/"


def reconstruct_file(doc):
    """Render a board doc (board-memory get JSON) back into a canonical memory file.

    The file carries the fields the native memory format + parse_memory round-trip: name (title),
    description, and metadata.type, followed by the body -- so re-parsing a reconstructed file yields
    the same core shape file_to_board would send. Board-side tags/provenance stay the board's record
    (file_to_board re-derives them on the next push), so they are intentionally not written here.
    """
    meta = doc.get("metadata") or {}
    mtype = str(meta.get("type") or "project")
    fm = {
        "name": doc.get("title") or "",
        "description": str(meta.get("description") or ""),
        "metadata": {"type": mtype},
    }
    front = yaml.safe_dump(fm, sort_keys=False, allow_unicode=True, default_flow_style=False).strip()
    body = (doc.get("body") or "").rstrip("\n")
    return f"---\n{front}\n---\n\n{body}\n"


def file_to_board(args):
    """Push new/changed memory files to the board via board-memory write (idempotent)."""
    state = load_state(args.state)
    changed = skipped = failed = 0
    for fname in sorted(os.listdir(args.memory_dir)):
        if not fname.endswith(".md") or fname in SKIP_NAMES:
            continue
        path = os.path.join(args.memory_dir, fname)
        with open(path, encoding="utf-8", errors="replace") as f:
            raw = f.read()
        digest = sha256(raw)
        rec, _warn = parse_memory(path)
        slug = rec["slug"]
        if state.get(slug) == digest:
            skipped += 1
            continue
        cmd = [args.board_memory, "write", "--slug", slug, *scope_flags(args),
               "--name", rec["name"], "--desc", rec["description"], "--type", rec["type"]]
        if args.dry_run:
            print(f"WOULD write {slug} ({len(rec['body'])}B body, type={rec['type']})")
            changed += 1
            continue
        res = subprocess.run(cmd, input=rec["body"], text=True, capture_output=True)
        if res.returncode == 0:
            state[slug] = digest
            changed += 1
        else:
            failed += 1
            print(f"FAIL {slug}: {res.stderr.strip()[:200]}", file=sys.stderr)
    if not args.dry_run:
        save_state(args.state, state)
    print(f"file->board: changed {changed}, skipped {skipped}, failed {failed}", file=sys.stderr)
    return 1 if failed else 0


def board_to_file(args):
    """Refresh the local memory dir from the board (local read-through cache for native recall/offline).

    Full-fidelity via board-memory get, which returns {path,title,metadata,body}: recall lists the scope's
    document paths, then get fetches each doc's title + description + metadata.type + body, reconstructed
    into a canonical memory file (reconstruct_file). The earlier recall+read path lost name/description/type
    (read returns only the body); the get verb (task_825) closed that gap, so a reconstructed file now
    re-parses to the same core shape file_to_board sends. Nested slugs (e.g. dream-reports/<date>) are
    preserved -- the slug is the full board path minus the scope prefix, written to a nested file path.
    """
    os.makedirs(args.memory_dir, exist_ok=True)
    recall = subprocess.run([args.board_memory, "recall", *scope_flags(args)],
                            text=True, capture_output=True)
    if recall.returncode != 0:
        print(f"recall failed: {recall.stderr.strip()[:200]}", file=sys.stderr)
        return 1
    prefix = scope_prefix(args)
    written = failed = 0
    for line in recall.stdout.splitlines():
        line = line.strip()
        if not line.startswith("- "):
            continue
        # format: "- <name> - <description>  (<path>)" -- take the parenthesized full board path
        path = line.rsplit("(", 1)[-1].rstrip(")").strip()
        if not path.startswith(prefix):
            continue
        slug = path[len(prefix):]
        if not slug:
            continue
        if args.dry_run:
            print(f"WOULD refresh {slug} from board")
            written += 1
            continue
        got = subprocess.run([args.board_memory, "get", "--slug", slug, *scope_flags(args)],
                             text=True, capture_output=True)
        if got.returncode != 0:
            print(f"get failed for {slug}: {got.stderr.strip()[:120]}", file=sys.stderr)
            failed += 1
            continue
        try:
            doc = json.loads(got.stdout)
        except ValueError as e:
            print(f"get returned non-JSON for {slug}: {str(e)[:120]}", file=sys.stderr)
            failed += 1
            continue
        dest = os.path.join(args.memory_dir, f"{slug}.md")
        os.makedirs(os.path.dirname(os.path.abspath(dest)), exist_ok=True)
        with open(dest, "w", encoding="utf-8") as f:
            f.write(reconstruct_file(doc))
        written += 1
    print(f"board->file: refreshed {written}, failed {failed}", file=sys.stderr)
    return 1 if failed else 0


def main():
    ap = argparse.ArgumentParser(description="Sync the Claude Code memory directory with the board (task_848).")
    ap.add_argument("--direction", required=True, choices=["file-to-board", "board-to-file"])
    scope = ap.add_mutually_exclusive_group(required=True)
    scope.add_argument("--agent")
    scope.add_argument("--repo")
    ap.add_argument("--memory-dir", required=True, help="the Claude Code memory directory for this scope")
    ap.add_argument("--board-memory", default="board-memory", help="path to the board-memory CLI")
    ap.add_argument("--state", default=os.path.expanduser("~/.config/fleet/memory-sync-state.json"),
                    help="per-slug content-hash state (file-to-board skip-unchanged)")
    ap.add_argument("--dry-run", action="store_true")
    args = ap.parse_args()
    if args.direction == "file-to-board":
        sys.exit(file_to_board(args))
    sys.exit(board_to_file(args))


if __name__ == "__main__":
    main()
