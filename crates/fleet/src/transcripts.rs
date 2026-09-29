//! `transcripts` — a FAITHFUL, LOSSLESS reader/renderer of an agent's harness session transcript.
//!
//! This is NOT a digest, summarizer, or regex-extractor: it renders every conversational record (each
//! user/assistant/system turn, every tool call, tool result, and error) verbatim, with no interpretation, so
//! a downstream reviewer (board task #176 / #187 observers) reads exactly what happened. Non-conversational
//! harness bookkeeping (mode/permission/attachment/ai-title/…) is not rendered, but it is COUNTED and the
//! omitted types are reported in a footer, so nothing is silently dropped.
//!
//! Harness-agnostic by construction: parsing/rendering sits behind the [`Harness`] seam so a second harness
//! (#129, Codex) plugs in as another impl producing the same normalized text. Claude Code JSONL is the first
//! renderer.
//!
//! Secrets: rendered text is passed through [`scrub`], which redacts obvious credential shapes (the same
//! token families the `fleet send` leak scanner flags) so a citation is session-id + line, never raw secret
//! output.

use std::path::{Path, PathBuf};

use serde_json::Value;

/// A harness renderer seam. Each harness parses its own session record shape and renders ONE record to
/// faithful text, or returns `None` for a non-conversational bookkeeping record (which the caller counts).
pub trait Harness {
    /// A stable id for the harness (e.g. `claude-code`), used in the rendering header.
    fn id(&self) -> &'static str;
    /// Render one parsed transcript record (a JSONL line) to faithful text, or `None` to skip (and count) a
    /// non-conversational record. The returned string is NOT yet scrubbed — the caller scrubs the whole
    /// rendering once.
    fn render_record(&self, rec: &Value) -> Option<String>;
}

/// The Claude Code JSONL harness: records are `{"type": "user"|"assistant"|"system"|…, "message": {"role",
/// "content": <string | block[]>}, "timestamp": …}`. Content blocks are `text` / `thinking` / `tool_use` /
/// `tool_result`. Everything conversational is rendered verbatim.
pub struct ClaudeCode;

impl Harness for ClaudeCode {
    fn id(&self) -> &'static str {
        "claude-code"
    }

    fn render_record(&self, rec: &Value) -> Option<String> {
        let ty = rec.get("type").and_then(Value::as_str).unwrap_or("");
        // Only the conversational record types are rendered; everything else is bookkeeping.
        if !matches!(ty, "user" | "assistant" | "system") {
            return None;
        }
        let msg = rec.get("message");
        let role = msg
            .and_then(|m| m.get("role"))
            .and_then(Value::as_str)
            .unwrap_or(ty);
        let ts = rec.get("timestamp").and_then(Value::as_str).unwrap_or("");
        let mut out = String::new();
        if ts.is_empty() {
            out.push_str(&format!("── {role} ──\n"));
        } else {
            out.push_str(&format!("── {role} · {ts} ──\n"));
        }

        // `system` records carry a plain `content` string at the top level; user/assistant carry `message`.
        let content = msg
            .and_then(|m| m.get("content"))
            .or_else(|| rec.get("content"));
        match content {
            Some(Value::String(s)) => {
                out.push_str(s);
                out.push('\n');
            }
            Some(Value::Array(blocks)) => {
                for b in blocks {
                    out.push_str(&render_block(b));
                }
            }
            _ => {
                // No content we recognize — still faithful: note the record was present but empty/odd.
                out.push_str("(no content)\n");
            }
        }
        Some(out)
    }
}

/// Render one Claude Code content block verbatim. Unknown block types are labeled and dumped as JSON rather
/// than dropped, so an unfamiliar shape is never silently lost.
fn render_block(b: &Value) -> String {
    let bt = b.get("type").and_then(Value::as_str).unwrap_or("");
    match bt {
        "text" => {
            let t = b.get("text").and_then(Value::as_str).unwrap_or("");
            format!("{t}\n")
        }
        "thinking" => {
            let t = b.get("thinking").and_then(Value::as_str).unwrap_or("");
            format!("[thinking]\n{t}\n")
        }
        "tool_use" => {
            let name = b.get("name").and_then(Value::as_str).unwrap_or("?");
            let id = b.get("id").and_then(Value::as_str).unwrap_or("?");
            let input = b
                .get("input")
                .map(|v| serde_json::to_string_pretty(v).unwrap_or_else(|_| v.to_string()))
                .unwrap_or_default();
            format!("[tool_use {name} (id {id})]\n{input}\n")
        }
        "tool_result" => {
            let id = b.get("tool_use_id").and_then(Value::as_str).unwrap_or("?");
            let is_err = b
                .get("is_error")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let tag = if is_err { "tool_result ERROR" } else { "tool_result" };
            format!("[{tag} for {id}]\n{}\n", render_tool_result_content(b.get("content")))
        }
        "" => format!("{}\n", b), // no type — dump the raw block
        other => format!("[block {other}]\n{b}\n"),
    }
}

/// A `tool_result`'s `content` is either a plain string or a list of `{type:text,text}` blocks. Render it
/// verbatim either way.
fn render_tool_result_content(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .map(|p| {
                p.get("text")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .unwrap_or_else(|| p.to_string())
            })
            .collect::<Vec<_>>()
            .join("\n"),
        Some(other) => other.to_string(),
        None => String::new(),
    }
}

/// The full rendering of a set of parsed records: every conversational record rendered by `harness`, plus a
/// footer accounting for the non-conversational records that were omitted (by count and type) so the omission
/// is explicit. The result is scrubbed of secrets.
pub fn render(records: &[Value], harness: &dyn Harness) -> String {
    let mut out = String::new();
    let mut omitted: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();
    for rec in records {
        match harness.render_record(rec) {
            Some(text) => {
                out.push_str(&text);
                out.push('\n');
            }
            None => {
                let ty = rec.get("type").and_then(Value::as_str).unwrap_or("?").to_string();
                *omitted.entry(ty).or_insert(0) += 1;
            }
        }
    }
    if !omitted.is_empty() {
        let total: usize = omitted.values().sum();
        let kinds: Vec<String> = omitted.iter().map(|(k, n)| format!("{k}×{n}")).collect();
        out.push_str(&format!(
            "── ({total} non-conversational records omitted: {}) ──\n",
            kinds.join(", ")
        ));
    }
    scrub(&out)
}

/// Redact obvious credential shapes from rendered text so a transcript can be cited without leaking secrets.
/// Mirrors the `fleet send` leak scanner's token families: `sk-ant-…`, `AKIA…`, `ghp_…`, `xoxb-…`, and the
/// VALUE of a `SECRET_NAMED_KEY=value` assignment. Conservative — it redacts the token, not the surrounding
/// prose, so the rendering stays legible.
pub fn scrub(text: &str) -> String {
    const REDACTED: &str = "‹redacted›";
    let mut out = String::with_capacity(text.len());
    for line in text.split_inclusive('\n') {
        for tok in split_keep_ws(line) {
            out.push_str(&scrub_token(tok, REDACTED));
        }
    }
    out
}

/// Split a string into alternating whitespace and non-whitespace runs, preserving every character so a
/// rejoin is byte-for-byte the original (minus redactions).
fn split_keep_ws(s: &str) -> Vec<&str> {
    let mut runs = Vec::new();
    let mut it = s.char_indices().peekable();
    while let Some(&(start, c)) = it.peek() {
        let ws = c.is_whitespace();
        while let Some(&(_, c2)) = it.peek() {
            if c2.is_whitespace() != ws {
                break;
            }
            it.next();
        }
        let end = it.peek().map(|&(j, _)| j).unwrap_or(s.len());
        runs.push(&s[start..end]);
    }
    runs
}

/// Redact one whitespace-delimited token if it looks like a credential; otherwise return it unchanged.
fn scrub_token<'a>(tok: &'a str, redacted: &str) -> std::borrow::Cow<'a, str> {
    if tok.trim().is_empty() {
        return std::borrow::Cow::Borrowed(tok);
    }
    // A KEY=VALUE assignment whose key is secret-named → redact just the value.
    if let Some(eq) = tok.find('=') {
        let key = &tok[..eq];
        let val = &tok[eq + 1..];
        let key_shaped = !key.is_empty()
            && key.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_');
        if key_shaped && !val.is_empty() {
            let ku = key.to_ascii_uppercase();
            let secret_named = ku.starts_with("AWS_")
                || ku.starts_with("ANTHROPIC_")
                || ["TOKEN", "SECRET", "PASSWORD", "PASSWD", "API_KEY", "CREDENTIAL", "ACCESS_KEY"]
                    .iter()
                    .any(|p| ku.contains(p));
            if secret_named {
                return std::borrow::Cow::Owned(format!("{key}={redacted}"));
            }
        }
    }
    // A bare credential token shape → redact the whole token.
    if tok.contains("sk-ant-")
        || tok.starts_with("AKIA")
        || tok.starts_with("ghp_")
        || tok.starts_with("xoxb-")
    {
        return std::borrow::Cow::Owned(redacted.to_string());
    }
    std::borrow::Cow::Borrowed(tok)
}

/// Encode a filesystem path into a Claude Code project-dir slug: every `/` and `.` becomes `-` (so
/// `/local/home/u/.fleet/agents/a/repo` → `-local-home-u--fleet-agents-a-repo`). Pure.
pub fn session_slug(cwd: &str) -> String {
    cwd.chars()
        .map(|c| if c == '/' || c == '.' { '-' } else { c })
        .collect()
}

/// True if a project-dir slug plausibly belongs to `agent` under the `.fleet/agents/<agent>/<repo>` workspace
/// layout: the slug contains the `-agents-<agent>` segment, ending there or followed by `-` (the repo).
///
/// LIMITATION (inherent to the flat slug): both `/` and the dashes WITHIN a multi-word agent/repo id collapse
/// to `-`, so a dash-prefix id (`v-fleet`) is indistinguishable from a longer id (`v-fleet-tooling`) by slug
/// alone. This is a best-effort filter; for an exact target pass `--session <file>`, and when several agent
/// ids are dash-prefixes of each other the longest anchoring match should be preferred by the caller. Pure.
pub fn slug_is_for_agent(slug: &str, agent: &str) -> bool {
    let needle = format!("-agents-{agent}");
    match slug.find(&needle) {
        Some(pos) => {
            let after = &slug[pos + needle.len()..];
            after.is_empty() || after.starts_with('-')
        }
        None => false,
    }
}

/// The `~/.claude/projects` dir (HOME is an OS locator, not a fleet knob).
fn projects_root() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .filter(|s| !s.is_empty())
        .map(|h| PathBuf::from(h).join(".claude/projects"))
}

/// Locate an agent's session JSONL files, newest-first (by mtime), across every project dir that belongs to
/// the agent. Best-effort: an exact target is available via `--session <file>` at the CLI. Returns an empty
/// vec when nothing matches.
pub fn locate_sessions(agent: &str) -> Vec<PathBuf> {
    let Some(root) = projects_root() else {
        return Vec::new();
    };
    let mut files: Vec<(std::time::SystemTime, PathBuf)> = Vec::new();
    let Ok(dirs) = std::fs::read_dir(&root) else {
        return Vec::new();
    };
    for d in dirs.flatten() {
        let name = d.file_name();
        let slug = name.to_string_lossy();
        if !slug_is_for_agent(&slug, agent) {
            continue;
        }
        if let Ok(entries) = std::fs::read_dir(d.path()) {
            for f in entries.flatten() {
                let p = f.path();
                if p.extension().and_then(|e| e.to_str()) == Some("jsonl") {
                    let mtime = f
                        .metadata()
                        .and_then(|m| m.modified())
                        .unwrap_or(std::time::UNIX_EPOCH);
                    files.push((mtime, p));
                }
            }
        }
    }
    files.sort_by_key(|(mtime, _)| std::cmp::Reverse(*mtime)); // newest-first
    files.into_iter().map(|(_, p)| p).collect()
}

/// A parsed `--since` watermark: `<session-id>:<line-offset>` (line-offset = how many JSONL lines of that
/// session were already observed). A bare `<session-id>` means offset 0. Pure.
pub fn parse_watermark(s: &str) -> (String, usize) {
    match s.rsplit_once(':') {
        Some((sid, off)) if off.chars().all(|c| c.is_ascii_digit()) && !off.is_empty() => {
            (sid.to_string(), off.parse().unwrap_or(0))
        }
        _ => (s.to_string(), 0),
    }
}

/// Given a session file's total line count and a `--since` offset, the first line index to render — backed up
/// by `overlap` lines so no boundary context is lost between observations. Pure.
pub fn window_start(since_offset: usize, overlap: usize) -> usize {
    since_offset.saturating_sub(overlap)
}

/// The session id (file stem) of a JSONL path.
pub fn session_id_of(path: &Path) -> String {
    path.file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_string()
}

/// Parse a JSONL file into records, skipping blank/unparseable lines (a truncated final line never aborts the
/// read). Returns the records and the total line count (for the next watermark).
pub fn parse_jsonl(path: &Path) -> Result<(Vec<Value>, usize), String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    let mut recs = Vec::new();
    let mut lines = 0usize;
    for line in text.lines() {
        lines += 1;
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Ok(v) = serde_json::from_str::<Value>(line) {
            recs.push(v);
        }
    }
    Ok((recs, lines))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Vec<Value> {
        vec![
            serde_json::json!({"type":"user","timestamp":"t1","message":{"role":"user","content":"hello there"}}),
            serde_json::json!({"type":"assistant","message":{"role":"assistant","content":[
                {"type":"thinking","thinking":"let me think"},
                {"type":"text","text":"answering now"},
                {"type":"tool_use","name":"Bash","id":"tu1","input":{"cmd":"ls"}}
            ]}}),
            serde_json::json!({"type":"user","message":{"role":"user","content":[
                {"type":"tool_result","tool_use_id":"tu1","content":"file1\nfile2"}
            ]}}),
            serde_json::json!({"type":"ai-title","aiTitle":"x"}),
            serde_json::json!({"type":"mode","mode":"default"}),
        ]
    }

    #[test]
    fn render_is_faithful_and_reports_omissions() {
        let out = render(&sample(), &ClaudeCode);
        // every conversational element is present, verbatim
        assert!(out.contains("── user · t1 ──") && out.contains("hello there"));
        assert!(out.contains("[thinking]") && out.contains("let me think"));
        assert!(out.contains("answering now"));
        assert!(out.contains("[tool_use Bash (id tu1)]") && out.contains("\"cmd\": \"ls\""));
        assert!(out.contains("[tool_result for tu1]") && out.contains("file1\nfile2"));
        // the two bookkeeping records are omitted but ACCOUNTED for (not silently dropped)
        assert!(out.contains("2 non-conversational records omitted"));
        assert!(out.contains("ai-title×1") && out.contains("mode×1"));
    }

    #[test]
    fn tool_result_error_is_labeled() {
        let recs = vec![serde_json::json!({"type":"user","message":{"role":"user","content":[
            {"type":"tool_result","tool_use_id":"z","is_error":true,"content":"boom"}
        ]}})];
        let out = render(&recs, &ClaudeCode);
        assert!(out.contains("[tool_result ERROR for z]") && out.contains("boom"));
    }

    #[test]
    fn scrub_redacts_credential_shapes_but_keeps_prose() {
        let t = "here is sk-ant-abc123 and AWS_SECRET_ACCESS_KEY=wJalrXUtnFEMI and a token ghp_deadbeef ok";
        let s = scrub(t);
        assert!(!s.contains("sk-ant-abc123") && !s.contains("ghp_deadbeef"));
        assert!(!s.contains("wJalrXUtnFEMI"));
        assert!(s.contains("AWS_SECRET_ACCESS_KEY=‹redacted›"));
        assert!(s.contains("here is") && s.contains("and a token") && s.contains("ok"), "prose preserved");
    }

    #[test]
    fn scrub_is_identity_for_clean_text() {
        let t = "just normal prose with an = sign and key=value pairs like PORT=8899\n";
        assert_eq!(scrub(t), t, "non-secret KEY=value and prose are untouched");
    }

    #[test]
    fn session_slug_encodes_slashes_and_dots() {
        assert_eq!(
            session_slug("/local/home/bythewc/.fleet/agents/v-fleet-tooling/cadenza"),
            "-local-home-bythewc--fleet-agents-v-fleet-tooling-cadenza"
        );
    }

    #[test]
    fn slug_is_for_agent_matches_the_agent_segment() {
        let slug = "-local-home-bythewc--fleet-agents-v-fleet-tooling-cadenza";
        assert!(slug_is_for_agent(slug, "v-fleet-tooling"));
        // an unrelated agent id does not appear in the -agents- segment → no match
        assert!(!slug_is_for_agent(slug, "v-cdz-smith"));
        assert!(!slug_is_for_agent(slug, "board-pm"));
        // no-repo layout (slug ends at the agent)
        assert!(slug_is_for_agent("-x--fleet-agents-board-pm", "board-pm"));
        // KNOWN LIMITATION: a dash-prefix id also matches (repo separator == intra-name dash). Documented;
        // callers disambiguate with the longest match or `--session`.
        assert!(slug_is_for_agent(slug, "v-fleet"), "dash-prefix collision is a documented limitation");
    }

    #[test]
    fn parse_watermark_splits_session_and_offset() {
        assert_eq!(parse_watermark("abc-123:450"), ("abc-123".to_string(), 450));
        assert_eq!(parse_watermark("abc-123"), ("abc-123".to_string(), 0));
        // a session id itself contains no trailing :digits, so a UUID with dashes is intact
        assert_eq!(parse_watermark("a951bc3e-bfbc"), ("a951bc3e-bfbc".to_string(), 0));
    }

    #[test]
    fn window_start_backs_up_by_overlap_and_saturates() {
        assert_eq!(window_start(100, 20), 80);
        assert_eq!(window_start(10, 20), 0, "never negative");
    }
}
