//! `scope` — the filesystem-scope guard, split into a pure, tested core and a thin stdin/stdout shim.
//!
//! The brain runs `claude` with `--permission-mode bypassPermissions` (an autonomous voice assistant),
//! so the ONLY thing confining its filesystem reads is a `PreToolUse` hook that denies Read/Glob/Grep
//! outside the projects tree (ported from the Python `_fs_scope_guard`). The CLI has no `--hook` flag, so
//! the hook is a *command*: the `voice-assistant` binary re-invoked as `voice-assistant scope-guard <dir>`,
//! which reads the hook payload on stdin and prints a deny decision (or nothing) on stdout.
//!
//! [`path_within`] (lexical containment) and [`decision`] (payload → optional deny reason) are pure and
//! unit-tested here; [`run_from_stdin`] is the tiny I/O wrapper the bin dispatches to.

use std::path::{Component, Path, PathBuf};

use serde_json::Value;

/// Lexically normalize a path: resolve `.` and `..` components without touching the filesystem, so the
/// containment check can't be fooled by `projects/../../etc/passwd`. Not symlink-aware (a lexical guard);
/// the runtime canonicalizes real paths before calling in, so this handles the `..` traversal case.
fn normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// True if `target` is `base` or lives inside it, after lexical normalization. Pure — the whole
/// containment policy (ported from the Python `_under`).
pub fn path_within(base: &Path, target: &Path) -> bool {
    let base = normalize(base);
    let target = normalize(target);
    target == base || target.starts_with(&base)
}

/// The scope decision for one `PreToolUse` payload: `Some(reason)` denies the call, `None` allows it.
/// Only Read/Glob/Grep are scoped (the tools that touch the filesystem); every other tool is allowed
/// (the assistant stays autonomous). Pure — this is the tested seam.
pub fn decision(projects_dir: &Path, payload: &Value) -> Option<String> {
    let tool = payload.get("tool_name").and_then(Value::as_str)?;
    if !matches!(tool, "Read" | "Glob" | "Grep") {
        return None;
    }
    let input = payload.get("tool_input")?;
    // Read uses `file_path`; Glob/Grep use `path`.
    let target = input
        .get("file_path")
        .or_else(|| input.get("path"))
        .and_then(Value::as_str)?;
    if path_within(projects_dir, Path::new(target)) {
        None
    } else {
        Some(format!(
            "Filesystem reads are limited to {}.",
            projects_dir.display()
        ))
    }
}

/// Render a [`decision`] result as the `PreToolUse` hook's stdout JSON: a deny object, or `{}` to allow.
/// Pure (string in, string out) so the exact hook schema is unit-tested.
pub fn decision_json(reason: Option<String>) -> String {
    match reason {
        Some(r) => serde_json::json!({
            "hookSpecificOutput": {
                "hookEventName": "PreToolUse",
                "permissionDecision": "deny",
                "permissionDecisionReason": r,
            }
        })
        .to_string(),
        None => "{}".to_string(),
    }
}

/// The `voice-assistant scope-guard <projects_dir>` entrypoint: read the hook payload from stdin, print
/// the decision JSON to stdout, exit 0. A parse failure allows the call (fail-open on a malformed hook
/// payload is the CLI's own default; the guard only ever *tightens* on a well-formed out-of-scope read).
pub fn run_from_stdin(projects_dir: &Path) {
    use std::io::Read;
    let mut buf = String::new();
    let _ = std::io::stdin().read_to_string(&mut buf);
    let reason = serde_json::from_str::<Value>(&buf)
        .ok()
        .and_then(|v| decision(projects_dir, &v));
    println!("{}", decision_json(reason));
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn within_accepts_self_and_children_rejects_siblings_and_traversal() {
        let base = Path::new("/home/u/Projects");
        assert!(path_within(base, Path::new("/home/u/Projects")));
        assert!(path_within(
            base,
            Path::new("/home/u/Projects/cadenza/src/main.rs")
        ));
        assert!(!path_within(base, Path::new("/home/u/Secrets/key")));
        // lexical `..` traversal is normalized away, then rejected
        assert!(!path_within(
            base,
            Path::new("/home/u/Projects/../.ssh/id_rsa")
        ));
        // a sibling that merely shares a name prefix isn't "within"
        assert!(!path_within(base, Path::new("/home/u/Projects-backup/x")));
    }

    #[test]
    fn only_read_glob_grep_are_scoped() {
        let dir = Path::new("/home/u/Projects");
        // an out-of-scope Read is denied
        let deny = decision(
            dir,
            &json!({"tool_name": "Read", "tool_input": {"file_path": "/etc/passwd"}}),
        );
        assert!(deny.is_some());
        // Glob/Grep use `path`
        assert!(
            decision(
                dir,
                &json!({"tool_name": "Grep", "tool_input": {"path": "/etc"}})
            )
            .is_some()
        );
        // an in-scope Read is allowed
        assert!(
            decision(
                dir,
                &json!({"tool_name": "Read", "tool_input": {"file_path": "/home/u/Projects/a"}})
            )
            .is_none()
        );
        // a non-fs tool is never scoped, wherever it points
        assert!(
            decision(
                dir,
                &json!({"tool_name": "WebFetch", "tool_input": {"url": "http://x"}})
            )
            .is_none()
        );
        assert!(
            decision(
                dir,
                &json!({"tool_name": "mcp__task-board__list_tasks", "tool_input": {}})
            )
            .is_none()
        );
    }

    #[test]
    fn decision_json_shapes_the_deny_and_allow_forms() {
        let allow: Value = serde_json::from_str(&decision_json(None)).unwrap();
        assert_eq!(allow, json!({}));
        let deny: Value = serde_json::from_str(&decision_json(Some("nope".to_string()))).unwrap();
        assert_eq!(deny["hookSpecificOutput"]["permissionDecision"], "deny");
        assert_eq!(
            deny["hookSpecificOutput"]["permissionDecisionReason"],
            "nope"
        );
    }
}
