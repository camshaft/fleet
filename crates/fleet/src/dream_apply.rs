//! `dream_apply` — INC 2 of the dream pass (task_827), the Rust port of `dream_apply.py` (task_956). The
//! GATED apply-workflow: INC 1 (`dream analyze`) proposes, a human/librarian disposes, and THIS executes a
//! dispositioned proposal under strict lane rules. Nothing here is autonomous — every call names the
//! disposition decision and the authenticating principal, and the lane gate refuses an unauthorized apply.
//!
//! Librarian-blessed invariants (task_827):
//!   - PROTECTED lane (operator-directive / tenet / MEMORY.md / index-* sub-index / kb-reconciliation
//!     ledger / canon-pointer, OR any proposal with a protected target, OR any `edit_index` op) is
//!     LIBRARIAN-ONLY: the op is EMITTED for the librarian to run in their own authenticated context, never
//!     script-executed (the board REST layer exposes no caller auth to certify a 'librarian' principal).
//!   - Destructive ops ARCHIVE, never hard-delete (restore-reversible).
//!   - A merge keeps every superseded memory's provenance in the survivor; a supersede preserves the
//!     `superseded_by` relationship rather than archiving.
//!   - Every op is a reviewable, reversible plan; dry-run (the default) applies nothing.
//!
//! Tool boundary: board-side ops are gated/version-pinned/audited here; KB-side ops (promote/supersede) are
//! MCP tools the orchestrating agent runs, so the script EMITS the exact MCP call spec rather than executing
//! it — the plan stays the single authoritative, reviewable artifact. `--execute` performs only board-side
//! ops, and only a STANDARD-lane op that carries a disposition version pin (condition C); absent the pin it
//! fail-closes, which is why a freshly-built plan (no pin yet) refuses to board-execute until a disposition
//! layer injects the pinned version.

use std::path::{Path, PathBuf};

use serde_json::{Map, Value, json};

/// Ops that are librarian-only regardless of the proposal's lane.
const PROTECTED_OPS: &[&str] = &["edit_index"];

/// The op string of a proposal (`proposed_change.op`), or "" if malformed.
fn op_of(proposal: &Value) -> &str {
    proposal["proposed_change"]["op"].as_str().unwrap_or("")
}

/// Condition B (fail closed on lane): anything NOT explicitly `standard` is protected, and any `edit_index`
/// op is protected regardless. An absent/unknown lane is protected. Pure.
fn is_protected(proposal: &Value) -> bool {
    proposal.get("lane").and_then(Value::as_str) != Some("standard")
        || PROTECTED_OPS.contains(&op_of(proposal))
}

/// Conditions A + B + D — the execution decision for one step: `board` (a standard board-side op the script
/// may stage) or `emit` (must be executed by the librarian, not the script). Fail closed on no principal.
fn authorize<'a>(proposal: &Value, step: &Value, principal: &str) -> Result<&'a str, String> {
    if principal.is_empty() {
        return Err("fail-closed: no principal recorded for this disposition".to_string());
    }
    if is_protected(proposal) {
        return Ok("emit"); // A: no board auth -> a protected op is never script-executed
    }
    if step.get("side").and_then(Value::as_str) != Some("board") {
        return Ok("emit"); // D: KB writes stay librarian-executed
    }
    Ok("board")
}

/// Translate a proposal into an ordered list of reversible plan steps. Pure — no side effects. Mirrors the
/// Python `build_plan`; an unknown op is refused.
fn build_plan(proposal: &Value) -> Result<Vec<Value>, String> {
    let op = op_of(proposal);
    let diff = &proposal["proposed_change"]["diff"];
    let empty = Vec::new();
    let superseded = diff.get("superseded_paths").and_then(Value::as_array).unwrap_or(&empty);
    let mut plan = Vec::new();
    match op {
        "merge" => {
            for p in superseded {
                plan.push(json!({
                    "action": "archive_document", "target_path": p,
                    "side": "board", "reversible": "restore_document",
                }));
            }
            plan.push(json!({
                "action": "set_props",
                "target_path": diff.get("survivor_path").cloned().unwrap_or(Value::Null),
                "props": {
                    "merged_from": Value::Array(superseded.clone()),
                    "retained_provenance": diff.get("retained_provenance").cloned().unwrap_or(json!([])),
                },
                "side": "board", "reversible": "props-revert (version-history)",
            }));
        }
        "archive" => {
            plan.push(json!({
                "action": "archive_document",
                "target_path": diff.get("doc_path").cloned().unwrap_or(Value::Null),
                "side": "board", "reversible": "restore_document",
            }));
        }
        "add_links" | "edit_index" => {
            let target = diff
                .get("orphan_path")
                .or_else(|| diff.get("index_path"))
                .cloned()
                .unwrap_or(Value::Null);
            plan.push(json!({
                "action": "version_body", "target_path": target,
                "edit": {
                    "add_links": diff.get("add_backlink_from").cloned().unwrap_or(Value::Null),
                    "pointer_diff": diff.get("pointer_diff").cloned().unwrap_or(Value::Null),
                },
                "side": "board", "reversible": "version-history",
            }));
        }
        "annotate" => {
            let first_path = diff
                .get("paths")
                .and_then(Value::as_array)
                .and_then(|a| a.first())
                .cloned()
                .unwrap_or(Value::Null);
            let note = diff
                .get("default_disposition")
                .or_else(|| diff.get("note"))
                .cloned()
                .unwrap_or(Value::Null);
            plan.push(json!({
                "action": "set_props", "target_path": first_path,
                "props": { "dream_annotation": diff.get("flag").cloned().unwrap_or(Value::Null), "note": note },
                "side": "board", "reversible": "props-revert",
            }));
        }
        "promote" => {
            plan.push(json!({
                "action": "kb_promote", "side": "kb", "reversible": "kb_supersede / kb_mark_outdated",
                "mcp_call": {
                    "tool": "kb_remember/kb_promote",
                    "args": {
                        "source_path": diff.get("source_path").cloned().unwrap_or(Value::Null),
                        "text": "<survivor body>",
                        "kind": diff.get("kind").cloned().unwrap_or(json!("promoted")),
                        "title": diff.get("title").cloned().unwrap_or(Value::Null),
                        "tags": diff.get("tags").cloned().unwrap_or(Value::Null),
                        "collection": diff.get("collection").cloned().unwrap_or(Value::Null),
                    },
                },
            }));
        }
        "supersede" => {
            plan.push(json!({
                "action": "kb_supersede", "side": "kb", "reversible": "relationship-level",
                "mcp_call": {
                    "tool": "kb_supersede",
                    "args": {
                        "old": diff.get("superseded_path").cloned().unwrap_or(Value::Null),
                        "new": diff.get("survivor_path").cloned().unwrap_or(Value::Null),
                        "preserve": "superseded_by",
                    },
                },
            }));
        }
        other => {
            let pid = proposal.get("proposal_id").and_then(Value::as_str).unwrap_or("?");
            return Err(format!("unknown op '{other}' in proposal {pid}"));
        }
    }
    Ok(plan)
}

/// Append one durable audit record (condition E), prepending a UTC timestamp, and return the written record.
fn audit(log_path: &Path, mut record: Map<String, Value>) -> Result<Value, String> {
    let ts = time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_else(|_| "0000-00-00T00:00:00Z".to_string());
    let mut out = Map::new();
    out.insert("ts".to_string(), json!(ts));
    out.append(&mut record);
    let line = serde_json::to_string(&Value::Object(out.clone())).map_err(|e| e.to_string())?;
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)
        .map_err(|e| format!("cannot open audit log {}: {e}", log_path.display()))?;
    writeln!(f, "{line}").map_err(|e| format!("audit write failed: {e}"))?;
    Ok(Value::Object(out))
}

/// Fetch the current version id of a document by path (`GET {api}/documents/by-path/{path}` ->
/// `current_version_id`) — the condition-C drift check. Latent in a freshly-built plan (no pin to compare),
/// reached only once a disposition layer injects a `dispositioned_version_id`.
fn current_version_id(board_api: &str, path: &str) -> Option<Value> {
    let url = format!("{}/documents/by-path/{}", board_api.trim_end_matches('/'), path);
    let resp = ureq::get(&url).call().ok()?;
    let raw = resp.into_string().ok()?;
    let body: Value = serde_json::from_str(&raw).ok()?;
    body.get("current_version_id").cloned()
}

/// The base audit/receipt fields shared by every step outcome.
fn step_base(proposal: &Value, step: &Value, principal: &str) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("proposal_id".into(), proposal.get("proposal_id").cloned().unwrap_or(Value::Null));
    m.insert("action".into(), step.get("action").cloned().unwrap_or(Value::Null));
    m.insert("target_path".into(), step.get("target_path").cloned().unwrap_or(Value::Null));
    m.insert("principal".into(), json!(principal));
    m.insert("reverse_handle".into(), step.get("reversible").cloned().unwrap_or(Value::Null));
    m.insert("lane".into(), proposal.get("lane").cloned().unwrap_or(Value::Null));
    m
}

/// Apply one plan step under conditions A-E. In dry-run it only PLANS (no audit, no mutation). On execute it
/// gates (A/B/D), version-pins (C), and writes the audit trace (E), emitting an authorized op spec that the
/// agent (standard board-side) or the librarian (protected/KB) runs via the proper tool. Mirrors the Python
/// `run_step`.
fn run_step(
    board_api: &str,
    audit_log: &Path,
    proposal: &Value,
    step: &Value,
    principal: &str,
    dry_run: bool,
) -> Result<Value, String> {
    let decision = authorize(proposal, step, principal)?;
    let base = step_base(proposal, step, principal);

    if dry_run {
        let mut m = base;
        m.insert("status".into(), json!("planned"));
        m.insert("executor".into(), json!(if decision == "emit" { "librarian" } else { "agent" }));
        return Ok(Value::Object(m));
    }

    if decision == "emit" {
        // A/D: protected lane (no board auth to certify a 'librarian' principal) or a KB write -> the
        // librarian executes off this spec; the script never runs it. Audited either way.
        let reason = if is_protected(proposal) {
            "protected lane; board has no caller auth so a protected principal cannot be certified in-script"
        } else {
            "KB write stays librarian-executed (D)"
        };
        let mut m = base;
        m.insert("status".into(), json!("authorized: librarian-execute"));
        m.insert("reason".into(), json!(reason));
        m.insert("version_pin".into(), step.get("dispositioned_version_id").cloned().unwrap_or(Value::Null));
        m.insert(
            "pre_apply_check".into(),
            json!("before applying, confirm the target is still at version_pin; on drift re-route to a fresh disposition, do not apply the stale spec"),
        );
        m.insert("mcp_call".into(), step.get("mcp_call").cloned().unwrap_or(Value::Null));
        return audit(audit_log, m);
    }

    // standard, board-side -> condition C: the disposition MUST pin the target version; refuse on absence
    // (fail closed) or drift (stale-diff clobber guard), then authorize the agent to execute.
    let expected = step.get("dispositioned_version_id").filter(|v| !v.is_null());
    let Some(expected) = expected else {
        let tp = step.get("target_path").and_then(Value::as_str).unwrap_or("?");
        return Err(format!(
            "fail-closed (C): step for {tp} carries no dispositioned_version_id to pin -- disposition must capture the version"
        ));
    };
    let tp = step.get("target_path").and_then(Value::as_str).unwrap_or("");
    let current = current_version_id(board_api, tp).unwrap_or(Value::Null);
    if &current != expected {
        return Err(format!(
            "stale disposition (C): {tp} drifted (dispositioned v{expected} -> current v{current}); re-route to a fresh disposition"
        ));
    }
    let mut m = base;
    m.insert("status".into(), json!("authorized: agent-execute"));
    m.insert("version_before".into(), current);
    m.insert("version_pinned".into(), expected.clone());
    audit(audit_log, m)
}

/// The default audit-log path (`$XDG_CONFIG_HOME/fleet/dream-apply-audit.jsonl`, else `~/.config/...`).
fn default_audit_log() -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .unwrap_or_else(|| PathBuf::from(".config"));
    base.join("fleet").join("dream-apply-audit.jsonl")
}

/// Entry point for `fleet dream-apply`. Loads the dream-report, finds the proposal, and either declines it
/// or builds + runs its plan under the lane gate. Returns the process exit code (2 = no such proposal,
/// 3 = apply refused).
#[allow(clippy::too_many_arguments)]
pub fn apply_cmd(
    report: &Path,
    proposal_id: &str,
    disposition: &str,
    principal: &str,
    execute: bool,
    audit_log: Option<PathBuf>,
    board_api: Option<String>,
) -> i32 {
    let text = match std::fs::read_to_string(report) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("cannot read report {}: {e}", report.display());
            return 1;
        }
    };
    let report_json: Value = match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("report is not valid JSON: {e}");
            return 1;
        }
    };
    let proposal = report_json
        .get("proposals")
        .and_then(Value::as_array)
        .and_then(|ps| {
            ps.iter()
                .find(|p| p.get("proposal_id").and_then(Value::as_str) == Some(proposal_id))
        });
    let Some(proposal) = proposal else {
        eprintln!("no such proposal: {proposal_id}");
        return 2;
    };

    if disposition == "decline" {
        let out = json!({
            "proposal": proposal_id,
            "disposition": "decline",
            "principal": principal,
            "action": "none (declined)",
        });
        println!("{}", serde_json::to_string_pretty(&out).unwrap_or_default());
        return 0;
    }

    let plan = match build_plan(proposal) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("APPLY REFUSED: {e}");
            return 3;
        }
    };
    let audit_log = audit_log.unwrap_or_else(default_audit_log);
    let board_api = board_api.unwrap_or_else(crate::board::Board::base_url);

    let mut receipts = Vec::new();
    for step in &plan {
        match run_step(&board_api, &audit_log, proposal, step, principal, !execute) {
            Ok(r) => receipts.push(r),
            Err(e) => {
                eprintln!("APPLY REFUSED: {e}");
                return 3;
            }
        }
    }

    let out = json!({
        "proposal": proposal_id,
        "op": op_of(proposal),
        "lane": proposal.get("lane").cloned().unwrap_or(Value::Null),
        "protected": is_protected(proposal),
        "principal": principal,
        "mode": if execute { "execute" } else { "dry-run" },
        "plan": plan,
        "receipts": receipts,
    });
    println!("{}", serde_json::to_string_pretty(&out).unwrap_or_default());
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn proposal(lane: &str, op: &str, diff: Value) -> Value {
        json!({
            "proposal_id": "dp-test-000000000000",
            "lane": lane,
            "proposed_change": { "op": op, "diff": diff },
        })
    }

    #[test]
    fn is_protected_fails_closed_on_non_standard_and_edit_index() {
        assert!(!is_protected(&proposal("standard", "annotate", json!({}))));
        assert!(is_protected(&proposal("protected", "annotate", json!({}))));
        // edit_index is protected even on a standard lane.
        assert!(is_protected(&proposal("standard", "edit_index", json!({}))));
        // absent lane -> protected.
        assert!(is_protected(&json!({"proposed_change": {"op": "annotate", "diff": {}}})));
    }

    #[test]
    fn authorize_gates_principal_lane_and_side() {
        let std_board = proposal("standard", "annotate", json!({}));
        let step_board = json!({"side": "board"});
        let step_kb = json!({"side": "kb"});
        // no principal -> fail closed.
        assert!(authorize(&std_board, &step_board, "").is_err());
        // standard + board-side -> agent executes.
        assert_eq!(authorize(&std_board, &step_board, "agent-x").unwrap(), "board");
        // standard + KB-side -> emit (librarian).
        assert_eq!(authorize(&std_board, &step_kb, "agent-x").unwrap(), "emit");
        // protected -> emit regardless of side.
        let prot = proposal("protected", "annotate", json!({}));
        assert_eq!(authorize(&prot, &step_board, "librarian").unwrap(), "emit");
    }

    #[test]
    fn build_plan_merge_archives_superseded_then_sets_props() {
        let p = proposal(
            "standard",
            "merge",
            json!({"survivor_path": "repos/r/keep", "superseded_paths": ["repos/r/old1", "repos/r/old2"]}),
        );
        let plan = build_plan(&p).unwrap();
        assert_eq!(plan.len(), 3); // two archives + one set_props
        assert_eq!(plan[0]["action"], "archive_document");
        assert_eq!(plan[0]["target_path"], "repos/r/old1");
        assert_eq!(plan[2]["action"], "set_props");
        assert_eq!(plan[2]["target_path"], "repos/r/keep");
        assert_eq!(plan[2]["props"]["merged_from"], json!(["repos/r/old1", "repos/r/old2"]));
    }

    #[test]
    fn build_plan_annotate_and_kb_ops_and_unknown() {
        let ann = build_plan(&proposal(
            "standard",
            "annotate",
            json!({"paths": ["repos/r/a", "repos/r/b"], "flag": "cross-repo-twin", "default_disposition": "keep-both"}),
        ))
        .unwrap();
        assert_eq!(ann[0]["action"], "set_props");
        assert_eq!(ann[0]["target_path"], "repos/r/a");
        assert_eq!(ann[0]["props"]["dream_annotation"], "cross-repo-twin");

        let kb = build_plan(&proposal("standard", "promote", json!({"source_path": "repos/r/x", "title": "T"}))).unwrap();
        assert_eq!(kb[0]["action"], "kb_promote");
        assert_eq!(kb[0]["side"], "kb");

        assert!(build_plan(&proposal("standard", "frobnicate", json!({}))).is_err());
    }

    #[test]
    fn run_step_dry_run_plans_without_audit() {
        let p = proposal("standard", "annotate", json!({"paths": ["repos/r/a"], "flag": "f"}));
        let plan = build_plan(&p).unwrap();
        let log = std::env::temp_dir().join(format!("dream-audit-noexist-{}.jsonl", std::process::id()));
        let _ = std::fs::remove_file(&log);
        let r = run_step("http://x", &log, &p, &plan[0], "agent-x", true).unwrap();
        assert_eq!(r["status"], "planned");
        assert_eq!(r["executor"], "agent");
        // dry-run writes no audit log.
        assert!(!log.exists());
    }

    #[test]
    fn run_step_protected_dry_run_marks_librarian_executor() {
        let p = proposal("protected", "annotate", json!({"paths": ["repos/r/a"], "flag": "f"}));
        let plan = build_plan(&p).unwrap();
        let r = run_step("http://x", Path::new("/dev/null"), &p, &plan[0], "librarian", true).unwrap();
        assert_eq!(r["status"], "planned");
        assert_eq!(r["executor"], "librarian");
    }

    #[test]
    fn run_step_execute_standard_board_fails_closed_without_version_pin() {
        // A freshly-built plan carries no dispositioned_version_id -> condition C fail-closed on execute.
        let p = proposal("standard", "annotate", json!({"paths": ["repos/r/a"], "flag": "f"}));
        let plan = build_plan(&p).unwrap();
        let log = std::env::temp_dir().join(format!("dream-audit-c-{}.jsonl", std::process::id()));
        let err = run_step("http://127.0.0.1:1", &log, &p, &plan[0], "agent-x", false).unwrap_err();
        assert!(err.contains("fail-closed (C)"), "got: {err}");
    }
}
