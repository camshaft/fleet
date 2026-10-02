#!/usr/bin/env python3
"""memory_convention: the single home for the agent-memory write-shape convention (task_825 / task_848).

A memory file is YAML frontmatter + a Markdown body. parse_memory turns one file into the canonical
record shape -- {slug, name, description, type, links, body} -- that the board-memory CLI is fed, so a
file-written memory and a tool-written one are byte-identical in shape. Extracted here (not copied) so
every Python consumer imports ONE definition: the migration manifest generator (task_826) and the
file<->board sync (memory_sync, task_848). Pure parsing logic, no host paths or deployment details.
"""
import os
import re

import yaml

VALID_TYPES = {"user", "feedback", "project", "reference"}
LINK_RE = re.compile(r"\[\[([^\]|#]+)")  # [[name]] / [[name|label]] / [[name#region]] -> name
FM_RE = re.compile(r"^---\s*\n(.*?)\n---\s*\n?(.*)$", re.DOTALL)


def lenient_frontmatter(block):
    """Line-based frontmatter parse for blocks strict YAML rejects (e.g. an unquoted
    description value containing a colon). Recovers name / description / metadata.type;
    the value is everything after the first ': ', so embedded colons are preserved."""
    fm = {}
    meta = {}
    in_meta = False
    for line in block.split("\n"):
        if not line.strip():
            continue
        if line[0] in " \t":  # indented -> a metadata child
            if in_meta:
                mm = re.match(r"\s+([\w-]+)\s*:\s*(.*)$", line)
                if mm:
                    meta[mm.group(1)] = mm.group(2).strip().strip('"')
            continue
        m = re.match(r"([\w-]+)\s*:\s*(.*)$", line)
        if not m:
            in_meta = False
            continue
        key, val = m.group(1), m.group(2)
        if key == "metadata":
            in_meta = True
            continue
        in_meta = False
        fm[key] = val.strip().strip('"')
    if meta:
        fm["metadata"] = meta
    return fm


def parse_memory(path):
    """Return (record_fields, warning_or_None). Never raises for content issues."""
    with open(path, encoding="utf-8", errors="replace") as f:
        text = f.read()
    warning = None
    fm = {}
    body = text
    m = FM_RE.match(text)
    if m:
        try:
            fm = yaml.safe_load(m.group(1)) or {}
            if not isinstance(fm, dict):
                fm, warning = {}, "frontmatter not a mapping"
            else:
                body = m.group(2)
        except Exception as e:  # strict YAML rejected it -> recover via line-based parse
            fm = lenient_frontmatter(m.group(1))
            body = m.group(2)
            warning = f"lenient frontmatter (strict YAML failed: {str(e).splitlines()[0]})"
    else:
        warning = "no frontmatter"

    slug = os.path.splitext(os.path.basename(path))[0]
    meta = fm.get("metadata") or {}
    if not isinstance(meta, dict):
        meta = {}
    mtype = str(meta.get("type") or "project")
    if mtype not in VALID_TYPES:
        warning = (warning + "; " if warning else "") + f"type '{mtype}' -> project"
        mtype = "project"
    links = sorted(set(LINK_RE.findall(body)))
    return {
        "slug": slug,
        "name": str(fm.get("name") or slug),
        "description": str(fm.get("description") or ""),
        "type": mtype,
        "links": links,
        "body": body,
    }, warning
