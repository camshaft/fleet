//! `brain` — the Claude brain, driven through the `claude` CLI. The Python original used the Claude
//! Agent SDK (a persistent session wired to three MCP servers with a filesystem-scope hook). Rust has no
//! Agent SDK, so we drive the same CLI the SDK itself drives: `claude --print --output-format stream-json`
//! with `--mcp-config` for the three servers, an `--allowedTools` policy, `--permission-mode
//! bypassPermissions`, and `--append-system-prompt`. Auth + MCP wiring are therefore unchanged from the
//! Python path.
//!
//! Split for testability: this module holds the PURE pieces — building the `--mcp-config` JSON, building
//! the argv, and decoding the CLI's newline-delimited `stream-json` into events (which tool ran, what the
//! final spoken text is). The actual `Command` spawn + timeout + `--resume` session bookkeeping lives in
//! the `runtime` module, which only has to feed lines through [`decode_line`] and read off the final text.

use serde_json::Value;

use crate::config::Brain;

/// Build the `--mcp-config` argument value: the JSON object the CLI expects, wiring the three HTTP MCP
/// servers under the names the `mcp__<server>` tool prefixes in [`Brain::allowed_tools`] reference
/// (`knowledge-base`, `task-board`, `surfaced`). Pure.
pub fn mcp_config_json(brain: &Brain) -> String {
    serde_json::json!({
        "mcpServers": {
            "knowledge-base": { "type": "http", "url": brain.kb_mcp_url },
            "task-board": { "type": "http", "url": brain.task_board_mcp_url },
            "surfaced": { "type": "http", "url": brain.surfaced_mcp_url },
        }
    })
    .to_string()
}

/// Build the `--settings` JSON that registers the filesystem-scope guard as a `PreToolUse` hook: run
/// `hook_command` before every Read/Glob/Grep so it can deny out-of-scope reads (ported from the Python
/// `_fs_scope_guard`). The CLI has NO `--hook` flag — hooks are only reachable through `--settings`. Pure.
pub fn settings_json(hook_command: &str) -> String {
    serde_json::json!({
        "hooks": {
            "PreToolUse": [{
                "matcher": "Read|Glob|Grep",
                "hooks": [{ "type": "command", "command": hook_command }]
            }]
        }
    })
    .to_string()
}

/// Build the full `claude` CLI argument vector for one turn. `prompt` is the transcribed user utterance;
/// `resume` is a prior session id to continue (barge-in / follow-up keep one conversation), or `None` for
/// a fresh session. `settings`, when set, is the `--settings` JSON (see [`settings_json`]) that installs
/// the filesystem-scope `PreToolUse` hook. Pure — returns argv, spawns nothing.
///
/// Streaming JSON output requires `--verbose` (the CLI rejects `--output-format stream-json` with `--print`
/// otherwise), so it's always included.
pub fn build_args(
    brain: &Brain,
    system_prompt: &str,
    prompt: &str,
    resume: Option<&str>,
    settings: Option<&str>,
) -> Vec<String> {
    let mut args: Vec<String> = vec![
        "--print".to_string(),
        prompt.to_string(),
        "--output-format".to_string(),
        "stream-json".to_string(),
        "--verbose".to_string(),
        "--permission-mode".to_string(),
        "bypassPermissions".to_string(),
        "--mcp-config".to_string(),
        mcp_config_json(brain),
        "--append-system-prompt".to_string(),
        system_prompt.to_string(),
        "--max-turns".to_string(),
        brain.max_turns.to_string(),
    ];
    if !brain.model.is_empty() {
        args.push("--model".to_string());
        args.push(brain.model.clone());
    }
    if !brain.allowed_tools.is_empty() {
        args.push("--allowedTools".to_string());
        args.push(brain.allowed_tools.join(","));
    }
    if let Some(session) = resume {
        args.push("--resume".to_string());
        args.push(session.to_string());
    }
    if let Some(s) = settings {
        args.push("--settings".to_string());
        args.push(s.to_string());
    }
    args
}

/// A decoded line of the CLI's `stream-json` output. The runtime logs [`ToolUse`]/[`Thinking`] for
/// visibility and keeps the last [`Result`] text as the spoken reply.
///
/// [`ToolUse`]: BrainEvent::ToolUse
/// [`Thinking`]: BrainEvent::Thinking
/// [`Result`]: BrainEvent::Result
#[derive(Debug, Clone, PartialEq)]
pub enum BrainEvent {
    /// The session id from the `system`/`init` line — captured so the next turn can `--resume` it.
    Init { session_id: Option<String> },
    /// The assistant emitted text in an intermediate turn (accumulated as a running reply).
    Text(String),
    /// The assistant emitted a thinking block (logged, never spoken).
    Thinking(String),
    /// The assistant invoked a tool (logged so the operator can see what the brain reached for).
    ToolUse { name: String },
    /// The terminal `result` line: `text` is the final spoken answer, `is_error` flags a failed turn.
    Result { text: String, is_error: bool },
}

/// Decode one `stream-json` line into an event, or `None` for lines we don't act on (blank lines, user
/// tool-result echoes, unrecognized shapes). Pure — this is the tested seam. The CLI emits one JSON object
/// per line; the shapes we care about:
///   - `{"type":"system","subtype":"init","session_id":"…"}`
///   - `{"type":"assistant","message":{"content":[{"type":"text"|"thinking"|"tool_use", …}]}}`
///   - `{"type":"result","subtype":"success","result":"…","is_error":false}`
pub fn decode_line(line: &str) -> Option<BrainEvent> {
    let line = line.trim();
    if line.is_empty() {
        return None;
    }
    let v: Value = serde_json::from_str(line).ok()?;
    match v.get("type").and_then(Value::as_str)? {
        "system" => {
            // Only the init line carries the session id; other system subtypes are ignored.
            if v.get("subtype").and_then(Value::as_str) == Some("init") {
                Some(BrainEvent::Init {
                    session_id: v
                        .get("session_id")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                })
            } else {
                None
            }
        }
        "assistant" => {
            // Take the FIRST actionable content block (text/thinking/tool_use). A turn with several
            // blocks emits several assistant lines in practice, but if not, first-block is enough for
            // logging + accumulation.
            let content = v.get("message")?.get("content")?.as_array()?;
            content.iter().find_map(assistant_block_event)
        }
        "result" => {
            // `result` is the final answer; fall back to an empty string so a malformed terminal line
            // still ends the turn rather than hanging.
            let text = v
                .get("result")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            // The CLI's success line is `subtype:"success"`; every other subtype is a failure
            // (`error_max_turns`, `error_during_execution`, …). Treat a present non-success subtype, or
            // an explicit `is_error:true`, as an error.
            let is_error = v.get("is_error").and_then(Value::as_bool).unwrap_or(false)
                || matches!(v.get("subtype").and_then(Value::as_str), Some(s) if s != "success");
            Some(BrainEvent::Result { text, is_error })
        }
        _ => None,
    }
}

/// Map one assistant content block to an event (text / thinking / tool_use), or `None` for shapes we skip
/// (e.g. `tool_result`, which only appears on `user` lines anyway).
fn assistant_block_event(block: &Value) -> Option<BrainEvent> {
    match block.get("type").and_then(Value::as_str)? {
        "text" => Some(BrainEvent::Text(
            block
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
        )),
        "thinking" => Some(BrainEvent::Thinking(
            block
                .get("thinking")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
        )),
        "tool_use" => Some(BrainEvent::ToolUse {
            name: block
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
        }),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn brain() -> Brain {
        Brain::default()
    }

    #[test]
    fn mcp_config_wires_three_named_servers() {
        let json: Value = serde_json::from_str(&mcp_config_json(&brain())).unwrap();
        let servers = &json["mcpServers"];
        // Names must match the `mcp__<server>` allowed-tool prefixes.
        assert_eq!(servers["knowledge-base"]["type"], "http");
        assert_eq!(
            servers["knowledge-base"]["url"],
            "http://localhost:8077/mcp"
        );
        assert_eq!(servers["task-board"]["url"], "http://localhost:8079/mcp");
        assert_eq!(
            servers["surfaced"]["url"],
            "http://localhost:8787/surfaced/mcp"
        );
    }

    #[test]
    fn build_args_has_streaming_and_permission_flags() {
        let args = build_args(&brain(), "be brief", "what's next?", None, None);
        // stream-json requires --verbose alongside --print.
        assert!(
            args.windows(2)
                .any(|w| w == ["--output-format", "stream-json"])
        );
        assert!(args.iter().any(|a| a == "--verbose"));
        assert!(
            args.windows(2)
                .any(|w| w == ["--permission-mode", "bypassPermissions"])
        );
        assert!(
            args.windows(2)
                .any(|w| w == ["--append-system-prompt", "be brief"])
        );
        // the prompt is the value after --print
        let i = args.iter().position(|a| a == "--print").unwrap();
        assert_eq!(args[i + 1], "what's next?");
        // default model is empty → no --model flag
        assert!(!args.iter().any(|a| a == "--model"));
        // allowed tools are comma-joined
        let i = args.iter().position(|a| a == "--allowedTools").unwrap();
        assert!(args[i + 1].contains("mcp__knowledge-base"));
        assert!(args[i + 1].contains("WebSearch"));
    }

    #[test]
    fn build_args_adds_model_resume_and_settings_when_set() {
        let mut b = brain();
        b.model = "opus".to_string();
        let settings = settings_json("/usr/bin/scope-guard");
        let args = build_args(&b, "sp", "hi", Some("sess-123"), Some(&settings));
        assert!(args.windows(2).any(|w| w == ["--model", "opus"]));
        assert!(args.windows(2).any(|w| w == ["--resume", "sess-123"]));
        let i = args.iter().position(|a| a == "--settings").unwrap();
        assert!(args[i + 1].contains("PreToolUse"));
    }

    #[test]
    fn settings_json_registers_a_pretooluse_command_hook() {
        let s: Value = serde_json::from_str(&settings_json("/bin/guard")).unwrap();
        let matcher = &s["hooks"]["PreToolUse"][0];
        assert_eq!(matcher["matcher"], "Read|Glob|Grep");
        assert_eq!(matcher["hooks"][0]["type"], "command");
        assert_eq!(matcher["hooks"][0]["command"], "/bin/guard");
    }

    #[test]
    fn decode_init_line_captures_session() {
        let ev = decode_line(r#"{"type":"system","subtype":"init","session_id":"abc"}"#).unwrap();
        assert_eq!(
            ev,
            BrainEvent::Init {
                session_id: Some("abc".to_string())
            }
        );
        // a non-init system line is ignored
        assert!(decode_line(r#"{"type":"system","subtype":"other"}"#).is_none());
    }

    #[test]
    fn decode_assistant_text_thinking_and_tool_use() {
        let text = decode_line(
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"hello"}]}}"#,
        )
        .unwrap();
        assert_eq!(text, BrainEvent::Text("hello".to_string()));

        let think = decode_line(
            r#"{"type":"assistant","message":{"content":[{"type":"thinking","thinking":"hmm"}]}}"#,
        )
        .unwrap();
        assert_eq!(think, BrainEvent::Thinking("hmm".to_string()));

        let tool = decode_line(
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"kb_search","input":{}}]}}"#,
        )
        .unwrap();
        assert_eq!(
            tool,
            BrainEvent::ToolUse {
                name: "kb_search".to_string()
            }
        );
    }

    #[test]
    fn decode_result_line_is_the_final_text() {
        let ev = decode_line(
            r#"{"type":"result","subtype":"success","result":"the answer","is_error":false}"#,
        )
        .unwrap();
        assert_eq!(
            ev,
            BrainEvent::Result {
                text: "the answer".to_string(),
                is_error: false
            }
        );
    }

    #[test]
    fn decode_result_error_subtype_flags_error() {
        let ev =
            decode_line(r#"{"type":"result","subtype":"error_max_turns","result":""}"#).unwrap();
        match ev {
            BrainEvent::Result { is_error, .. } => assert!(is_error),
            other => panic!("expected Result, got {other:?}"),
        }
    }

    #[test]
    fn decode_ignores_blank_garbage_and_user_lines() {
        assert!(decode_line("").is_none());
        assert!(decode_line("   ").is_none());
        assert!(decode_line("not json").is_none());
        // user tool-result echoes aren't actionable for us
        assert!(decode_line(r#"{"type":"user","message":{"content":[]}}"#).is_none());
    }
}
