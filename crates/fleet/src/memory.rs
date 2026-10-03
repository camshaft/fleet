//! `memory` — the Claude Code memory directory <-> board auto-sync (task_848), the Rust port of
//! `memory_sync.py` (task_956: fleet tooling is Rust, not Python). It is the native-tool fallback cameron
//! asked for (task_826 comment_3477): an agent that writes the native Claude Code memory file directly
//! (instead of calling `board-memory`) still gets its memory onto the board.
//!
//! Two directions, both shelling the `board-memory` CLI (the thin REST shim stays, per the operator ruling):
//!   - **file-to-board** (the core ask): scan the memory dir, parse each file's frontmatter, and run
//!     `board-memory write` for each new/changed file. Idempotent via a per-slug content-hash state file —
//!     an unchanged file is skipped; a changed one is versioned in place by the deterministic board path.
//!   - **board-to-file** (the local read-through cache for native recall/offline): `board-memory recall`
//!     the slug index, then `board-memory read` each body into `<slug>.md`.
//!
//! Single-writer stays intact: an agent syncs only its own `--agent <self>` (or a given `--repo`) scope, so
//! file-to-board is same-writer with no cross-agent conflict.
//!
//! Port notes vs the Python: the frontmatter parse is the lenient line-based parser (the Python's own
//! fallback path), not a full YAML load — the fleet crate carries no YAML dependency, and the memory-file
//! frontmatter shape (single-line `name`/`description` + an indented `metadata.type`) parses identically.
//! An exotic multi-line YAML scalar in frontmatter would differ, but memory files do not use them. The
//! per-slug content hash is sha256 hex, byte-identical to the Python state format.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use sha2::{Digest, Sha256};

/// Memory index/readme files that are not per-fact memories and must never be pushed as a board memory.
const SKIP_NAMES: &[&str] = &["MEMORY.md", "README.md"];
/// The board's four memory types; anything else is coerced to `project` (with a warning).
const VALID_TYPES: &[&str] = &["user", "feedback", "project", "reference"];

/// Which half of the sync to run (CLI `--direction`).
#[derive(Clone, Copy, clap::ValueEnum)]
pub enum SyncDirection {
    /// Push new/changed memory files to the board (the native-tool fallback).
    #[value(name = "file-to-board")]
    FileToBoard,
    /// Refresh the local memory dir from the board (read-through cache for native recall/offline).
    #[value(name = "board-to-file")]
    BoardToFile,
}

/// The sync scope: an agent's own memories, or a repo's. Exactly one is set (the CLI enforces it).
enum Scope {
    Agent(String),
    Repo(String),
}

impl Scope {
    /// The `board-memory` scope flags for this scope (`--agent <id>` or `--repo <name>`).
    fn flags(&self) -> [&str; 2] {
        match self {
            Scope::Agent(a) => ["--agent", a],
            Scope::Repo(r) => ["--repo", r],
        }
    }
}

/// A parsed memory file: the fields the board-memory write convention needs plus the body `[[wiki-links]]`.
/// This is the shared frontmatter parser (the dream port reuses it), so it carries the full record even
/// though file-to-board uses only slug/name/description/type/body.
#[derive(Debug, PartialEq, Eq)]
pub struct MemoryRecord {
    pub slug: String,
    pub name: String,
    pub description: String,
    pub mtype: String,
    pub links: Vec<String>,
    pub body: String,
}

/// Split a `---\n…\n---\n` YAML frontmatter header off the top of a memory file, returning
/// `Some((frontmatter_block, body))` when both the opening and closing `---` fences are present, else `None`
/// (treated as "no frontmatter", body = the whole text). Mirrors the Python `FM_RE`:
/// `^---\s*\n(.*?)\n---\s*\n?(.*)$` in dotall mode — the body has its leading whitespace stripped (the greedy
/// `\s*` after the closing fence). Pure.
fn split_frontmatter(text: &str) -> Option<(&str, &str)> {
    // The opener must be the very first line: `---` plus only trailing whitespace.
    let nl0 = text.find('\n')?;
    let first = &text[..nl0];
    if first.trim_end() != "---" {
        return None;
    }
    let rest = &text[nl0 + 1..];
    // The closer is the first line (preceded by a newline, i.e. not rest's offset 0) that is `---` + ws.
    let mut offset = 0usize;
    for line in rest.split_inclusive('\n') {
        let content = line.strip_suffix('\n').unwrap_or(line);
        if offset > 0 && content.trim_end() == "---" {
            // block = everything before the `\n` that precedes this closing line.
            let block = &rest[..offset - 1];
            // body = everything after the `---` on the closing line, leading whitespace stripped.
            let body = rest[offset + 3..].trim_start();
            return Some((block, body));
        }
        offset += line.len();
    }
    None
}

/// Split one `key: value` line into `(key, value)` the way the Python lenient parser does: the key is the
/// leading `[\w-]+` run, the value is everything after the first `:` with surrounding whitespace trimmed and
/// then surrounding `"` stripped (so an unquoted value with an embedded colon is preserved). `leading`
/// pre-trimmed indentation lets the same logic serve top-level and indented (`metadata` child) lines. Pure.
fn parse_kv(line: &str) -> Option<(&str, String)> {
    let key_end = line.find(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '-'))?;
    if key_end == 0 {
        return None; // no `[\w-]+` key
    }
    let key = &line[..key_end];
    let after = line[key_end..].trim_start();
    let val = after.strip_prefix(':')?; // key `\s*:` — a non-colon after the key is not a kv line
    let val = val.trim().trim_matches('"').to_string();
    Some((key, val))
}

/// The lenient line-based frontmatter parse (the Python `lenient_frontmatter`): recover `name`,
/// `description`, and `metadata.<child>` (we read `type`). A top-level non-kv line resets the metadata
/// context; an indented line under `metadata:` is a metadata child. Pure.
fn parse_frontmatter(block: &str) -> (BTreeMap<String, String>, BTreeMap<String, String>) {
    let mut fm = BTreeMap::new();
    let mut meta = BTreeMap::new();
    let mut in_meta = false;
    for line in block.split('\n') {
        if line.trim().is_empty() {
            continue;
        }
        let indented = line.starts_with(' ') || line.starts_with('\t');
        if indented {
            if in_meta && let Some((k, v)) = parse_kv(line.trim_start()) {
                meta.insert(k.to_string(), v);
            }
            continue;
        }
        match parse_kv(line) {
            None => {
                in_meta = false;
            }
            Some(("metadata", _)) => {
                in_meta = true;
            }
            Some((k, v)) => {
                in_meta = false;
                fm.insert(k.to_string(), v);
            }
        }
    }
    (fm, meta)
}

/// Collect the sorted, de-duplicated set of `[[name]]` wiki-link targets in `body` — the Python `LINK_RE`
/// `\[\[([^\]|#]+)` (capture up to the first `]`, `|`, or `#`). Pure.
fn body_links(body: &str) -> Vec<String> {
    let mut set = std::collections::BTreeSet::new();
    let bytes = body.as_bytes();
    let mut i = 0;
    while i + 1 < bytes.len() {
        if bytes[i] == b'[' && bytes[i + 1] == b'[' {
            let start = i + 2;
            let mut j = start;
            while j < bytes.len() && !matches!(bytes[j], b']' | b'|' | b'#') {
                j += 1;
            }
            if j > start {
                set.insert(body[start..j].to_string());
            }
            i = j;
        } else {
            i += 1;
        }
    }
    set.into_iter().collect()
}

/// Parse one memory file into a [`MemoryRecord`] plus an optional parse warning. Never fails for content
/// issues: a file with no frontmatter still yields a record (slug from the filename, empty description,
/// `project` type, full body). Mirrors the Python `parse_memory`.
pub fn parse_memory(path: &Path) -> std::io::Result<(MemoryRecord, Option<String>)> {
    let raw = std::fs::read(path)?;
    let text = String::from_utf8_lossy(&raw).into_owned();
    let slug = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();

    let mut warning: Option<String> = None;
    let (fm, meta, body) = match split_frontmatter(&text) {
        Some((block, body)) => {
            let (fm, meta) = parse_frontmatter(block);
            (fm, meta, body.to_string())
        }
        None => {
            warning = Some("no frontmatter".to_string());
            (BTreeMap::new(), BTreeMap::new(), text.clone())
        }
    };

    let mut mtype = meta
        .get("type")
        .cloned()
        .unwrap_or_else(|| "project".to_string());
    if mtype.is_empty() {
        mtype = "project".to_string();
    }
    if !VALID_TYPES.contains(&mtype.as_str()) {
        let note = format!("type '{mtype}' -> project");
        warning = Some(match warning {
            Some(w) => format!("{w}; {note}"),
            None => note,
        });
        mtype = "project".to_string();
    }

    let name = match fm.get("name") {
        Some(n) if !n.is_empty() => n.clone(),
        _ => slug.clone(),
    };
    let description = fm.get("description").cloned().unwrap_or_default();
    let links = body_links(&body);

    Ok((
        MemoryRecord {
            slug,
            name,
            description,
            mtype,
            links,
            body,
        },
        warning,
    ))
}

/// Lowercase hex sha256 of `text` — the per-slug content fingerprint, byte-identical to the Python state.
/// `pub(crate)` so the dream pass reuses the same hashing (exact-duplicate body identity).
pub(crate) fn sha256_hex(text: &str) -> String {
    let digest = Sha256::digest(text.as_bytes());
    let mut out = String::with_capacity(64);
    for b in digest {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

/// Load the per-slug content-hash state (`{slug: sha256}`), or an empty map on any read/parse error (the
/// Python's try/except: a missing or corrupt state file simply means "nothing synced yet").
fn load_state(path: &Path) -> BTreeMap<String, String> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

/// Persist the per-slug content-hash state (creating the parent dir), sorted keys.
fn save_state(path: &Path, state: &BTreeMap<String, String>) -> std::io::Result<()> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string_pretty(state).unwrap_or_else(|_| "{}".to_string());
    std::fs::write(path, json)
}

/// Run the `board-memory` CLI with `args` and an optional `stdin` body, returning its exit status, stdout,
/// and stderr. A spawn failure (e.g. the shim not on PATH) surfaces as an `Err`.
fn run_board_memory(
    bin: &str,
    args: &[&str],
    stdin: Option<&str>,
) -> std::io::Result<std::process::Output> {
    let mut cmd = Command::new(bin);
    cmd.args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        });
    let mut child = cmd.spawn()?;
    if let Some(body) = stdin {
        child
            .stdin
            .take()
            .expect("stdin was piped")
            .write_all(body.as_bytes())?;
    }
    child.wait_with_output()
}

/// Push new/changed memory files to the board (file-to-board). Returns a nonzero exit code if any write
/// failed, matching the Python.
fn file_to_board(
    scope: &Scope,
    memory_dir: &Path,
    board_memory: &str,
    state_path: &Path,
    dry_run: bool,
) -> i32 {
    let mut state = load_state(state_path);
    let (mut changed, mut skipped, mut failed) = (0u32, 0u32, 0u32);

    let mut names: Vec<String> = match std::fs::read_dir(memory_dir) {
        Ok(rd) => rd
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect(),
        Err(e) => {
            eprintln!("cannot read memory dir {}: {e}", memory_dir.display());
            return 1;
        }
    };
    names.sort();

    for fname in names {
        if !fname.ends_with(".md") || SKIP_NAMES.contains(&fname.as_str()) {
            continue;
        }
        let path = memory_dir.join(&fname);
        let raw = match std::fs::read(&path) {
            Ok(b) => b,
            Err(e) => {
                eprintln!("FAIL {fname}: {e}");
                failed += 1;
                continue;
            }
        };
        let digest = sha256_hex(&String::from_utf8_lossy(&raw));
        let (rec, _warn) = match parse_memory(&path) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("FAIL {fname}: {e}");
                failed += 1;
                continue;
            }
        };
        if state.get(&rec.slug) == Some(&digest) {
            skipped += 1;
            continue;
        }
        if dry_run {
            // NOTE: the body content pushed is byte-identical to the Python; this count is in bytes
            // (`.len()`), where the Python printed Unicode code points (`len(str)`). For a multibyte body
            // the two counts differ by the extra UTF-8 bytes — a cosmetic dry-run label difference only,
            // not a content divergence (verified against the 2927-memory camshaft-cadenza store).
            println!(
                "WOULD write {} ({}B body, type={})",
                rec.slug,
                rec.body.len(),
                rec.mtype
            );
            changed += 1;
            continue;
        }
        let [s0, s1] = scope.flags();
        let args = [
            "write",
            "--slug",
            &rec.slug,
            s0,
            s1,
            "--name",
            &rec.name,
            "--desc",
            &rec.description,
            "--type",
            &rec.mtype,
        ];
        match run_board_memory(board_memory, &args, Some(&rec.body)) {
            Ok(out) if out.status.success() => {
                state.insert(rec.slug.clone(), digest);
                changed += 1;
            }
            Ok(out) => {
                let err = String::from_utf8_lossy(&out.stderr);
                eprintln!(
                    "FAIL {}: {}",
                    rec.slug,
                    err.trim().chars().take(200).collect::<String>()
                );
                failed += 1;
            }
            Err(e) => {
                eprintln!("FAIL {}: {e}", rec.slug);
                failed += 1;
            }
        }
    }

    if !dry_run && let Err(e) = save_state(state_path, &state) {
        eprintln!("state save failed: {e}");
    }
    eprintln!("file->board: changed {changed}, skipped {skipped}, failed {failed}");
    i32::from(failed > 0)
}

/// Parse a `board-memory recall` index line into its slug. The line format is
/// `- <name> - <description>  (<path>)`; the slug is the last path segment inside the trailing parens. Pure.
fn recall_line_slug(line: &str) -> Option<String> {
    let line = line.trim();
    if !line.starts_with("- ") {
        return None;
    }
    let after_paren = line.rsplit_once('(')?.1;
    let path = after_paren.trim_end_matches(')').trim();
    let slug = path.rsplit('/').next().unwrap_or(path).trim();
    if slug.is_empty() {
        None
    } else {
        Some(slug.to_string())
    }
}

/// Refresh the local memory dir from the board (board-to-file).
fn board_to_file(scope: &Scope, memory_dir: &Path, board_memory: &str, dry_run: bool) -> i32 {
    if let Err(e) = std::fs::create_dir_all(memory_dir) {
        eprintln!("cannot create memory dir {}: {e}", memory_dir.display());
        return 1;
    }
    let [s0, s1] = scope.flags();
    let recall = match run_board_memory(board_memory, &["recall", s0, s1], None) {
        Ok(out) if out.status.success() => out,
        Ok(out) => {
            let err = String::from_utf8_lossy(&out.stderr);
            eprintln!(
                "recall failed: {}",
                err.trim().chars().take(200).collect::<String>()
            );
            return 1;
        }
        Err(e) => {
            eprintln!("recall failed: {e}");
            return 1;
        }
    };
    let stdout = String::from_utf8_lossy(&recall.stdout);
    let mut written = 0u32;
    for line in stdout.lines() {
        let Some(slug) = recall_line_slug(line) else {
            continue;
        };
        if dry_run {
            println!("WOULD refresh {slug} from board");
            written += 1;
            continue;
        }
        match run_board_memory(board_memory, &["read", "--slug", &slug, s0, s1], None) {
            Ok(out) if out.status.success() => {
                let dest = memory_dir.join(format!("{slug}.md"));
                if let Err(e) = std::fs::write(&dest, &out.stdout) {
                    eprintln!("write failed for {slug}: {e}");
                    continue;
                }
                written += 1;
            }
            Ok(out) => {
                let err = String::from_utf8_lossy(&out.stderr);
                eprintln!(
                    "read failed for {slug}: {}",
                    err.trim().chars().take(120).collect::<String>()
                );
            }
            Err(e) => eprintln!("read failed for {slug}: {e}"),
        }
    }
    eprintln!("board->file: refreshed {written}");
    0
}

/// The default per-slug content-hash state path (`$XDG_CONFIG_HOME/fleet/…`, else `~/.config/fleet/…`).
fn default_state_path() -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .unwrap_or_else(|| PathBuf::from(".config"));
    base.join("fleet").join("memory-sync-state.json")
}

/// Entry point for the `fleet memory-sync` subcommand. Validates that exactly one scope is set, then runs
/// the requested direction. Returns the process exit code.
#[allow(clippy::too_many_arguments)]
pub fn sync_cmd(
    direction: SyncDirection,
    agent: Option<String>,
    repo: Option<String>,
    memory_dir: &Path,
    board_memory: &str,
    state: Option<PathBuf>,
    dry_run: bool,
) -> i32 {
    let scope = match (agent, repo) {
        (Some(a), None) => Scope::Agent(a),
        (None, Some(r)) => Scope::Repo(r),
        (Some(_), Some(_)) => {
            eprintln!("memory-sync: pass exactly one of --agent or --repo, not both");
            return 2;
        }
        (None, None) => {
            eprintln!("memory-sync: a scope is required (--agent <self> or --repo <repo>)");
            return 2;
        }
    };
    match direction {
        SyncDirection::FileToBoard => {
            let state_path = state.unwrap_or_else(default_state_path);
            file_to_board(&scope, memory_dir, board_memory, &state_path, dry_run)
        }
        SyncDirection::BoardToFile => board_to_file(&scope, memory_dir, board_memory, dry_run),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_frontmatter_extracts_block_and_strips_body_leading_ws() {
        let text = "---\nname: x\ndescription: y\n---\n\n# Body\nmore\n";
        let (block, body) = split_frontmatter(text).expect("has frontmatter");
        assert_eq!(block, "name: x\ndescription: y");
        // The greedy `\s*` after the closing fence strips the blank line before the body.
        assert_eq!(body, "# Body\nmore\n");
    }

    #[test]
    fn split_frontmatter_none_without_both_fences() {
        assert!(split_frontmatter("no fence here\nbody\n").is_none());
        // An opener with no closer is not frontmatter (the whole text is the body).
        assert!(split_frontmatter("---\nname: x\nbody with no closing fence\n").is_none());
        // The closer cannot be the opener's own line (needs a preceding newline).
        assert!(split_frontmatter("---\n").is_none());
    }

    #[test]
    fn parse_kv_takes_value_after_first_colon_and_strips_quotes() {
        assert_eq!(parse_kv("name: hello"), Some(("name", "hello".to_string())));
        // An embedded colon in the value is preserved (value = everything after the first `: `).
        assert_eq!(
            parse_kv("description: uses a: colon"),
            Some(("description", "uses a: colon".to_string()))
        );
        // Surrounding double quotes are stripped.
        assert_eq!(
            parse_kv("name: \"quoted\""),
            Some(("name", "quoted".to_string()))
        );
        // A bare key with no value.
        assert_eq!(parse_kv("metadata:"), Some(("metadata", String::new())));
        // A non-kv line (no colon after the key run) does not parse.
        assert_eq!(parse_kv("- a bullet"), None);
    }

    #[test]
    fn parse_frontmatter_recovers_top_level_and_metadata_children() {
        let block = "name: my-slug\ndescription: a desc\nmetadata:\n  type: feedback\n  area: x";
        let (fm, meta) = parse_frontmatter(block);
        assert_eq!(fm.get("name").map(String::as_str), Some("my-slug"));
        assert_eq!(fm.get("description").map(String::as_str), Some("a desc"));
        assert_eq!(meta.get("type").map(String::as_str), Some("feedback"));
        assert_eq!(meta.get("area").map(String::as_str), Some("x"));
        // `metadata` is a context marker, not stored as a top-level key.
        assert!(!fm.contains_key("metadata"));
    }

    #[test]
    fn parse_frontmatter_top_level_line_resets_metadata_context() {
        // After `metadata:`, a non-indented key ends the metadata block (its child is not captured).
        let block = "metadata:\n  type: user\nname: back-at-top";
        let (fm, meta) = parse_frontmatter(block);
        assert_eq!(meta.get("type").map(String::as_str), Some("user"));
        assert_eq!(fm.get("name").map(String::as_str), Some("back-at-top"));
    }

    #[test]
    fn body_links_are_sorted_deduped_and_cut_at_delimiters() {
        let body = "see [[alpha]] and [[beta|label]] and [[alpha#region]] and [[gamma]]";
        assert_eq!(body_links(body), vec!["alpha", "beta", "gamma"]);
        assert!(body_links("no links here").is_empty());
    }

    #[test]
    fn parse_memory_defaults_type_and_names_from_slug(/* via a temp file */) {
        let dir = std::env::temp_dir().join(format!("mem-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // No frontmatter -> slug name, empty desc, project type, "no frontmatter" warning.
        let p = dir.join("plain-note.md");
        std::fs::write(&p, "just a body\n").unwrap();
        let (rec, warn) = parse_memory(&p).unwrap();
        assert_eq!(rec.slug, "plain-note");
        assert_eq!(rec.name, "plain-note");
        assert_eq!(rec.description, "");
        assert_eq!(rec.mtype, "project");
        assert_eq!(warn.as_deref(), Some("no frontmatter"));

        // An invalid type is coerced to project with a warning.
        let p2 = dir.join("typed.md");
        std::fs::write(
            &p2,
            "---\nname: n\ndescription: d\nmetadata:\n  type: bogus\n---\nbody\n",
        )
        .unwrap();
        let (rec2, warn2) = parse_memory(&p2).unwrap();
        assert_eq!(rec2.mtype, "project");
        assert_eq!(rec2.name, "n");
        assert_eq!(rec2.description, "d");
        assert_eq!(warn2.as_deref(), Some("type 'bogus' -> project"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sha256_hex_is_stable_and_lowercase() {
        // Known vector: sha256("") = e3b0c442...
        assert_eq!(
            sha256_hex(""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn recall_line_slug_pulls_the_trailing_path_leaf() {
        assert_eq!(
            recall_line_slug("- my-name - a description  (agents/v-x/my-slug)").as_deref(),
            Some("my-slug")
        );
        // A non-index line is ignored.
        assert_eq!(recall_line_slug("header text"), None);
        // No parens -> no slug.
        assert_eq!(recall_line_slug("- name - desc"), None);
    }

    #[test]
    fn scope_flags_map_to_the_board_memory_switches() {
        assert_eq!(Scope::Agent("v-x".into()).flags(), ["--agent", "v-x"]);
        assert_eq!(Scope::Repo("r".into()).flags(), ["--repo", "r"]);
    }
}
