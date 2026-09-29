//! `runtime::brain` — spawn the `claude` CLI for one turn, stream-decode its output, and return the final
//! spoken text, bounded by a timeout. The pure pieces (argv, settings JSON, line decoding) live in
//! [`crate::brain`]; this is the I/O shell: `Command` spawn, a reader thread feeding decoded lines to a
//! collector, a wall-clock timeout that kills a hung turn, and `--resume` session bookkeeping so barge-in
//! and follow-ups continue one conversation (the port of the Python persistent `ClaudeSDKClient`).

use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use crate::brain::{self, BrainEvent};
use crate::config::Brain;

/// A persistent brain: holds the config, the `--settings` JSON that installs the FS-scope hook, and the
/// current session id so turns chain via `--resume`.
pub struct BrainRunner {
    cfg: Brain,
    system_prompt: String,
    settings: String,
    session_id: Option<String>,
}

impl BrainRunner {
    /// Build a runner. `scope_guard_argv0` is how to invoke this binary as the FS-scope hook command
    /// (e.g. `"/usr/bin/voice-assistant scope-guard /home/u/Projects"`).
    pub fn new(cfg: Brain, system_prompt: String, scope_guard_command: String) -> Self {
        let settings = brain::settings_json(&scope_guard_command);
        Self {
            cfg,
            system_prompt,
            settings,
            session_id: None,
        }
    }

    /// Ask one turn. Logs tool calls / intermediate text as they stream, returns only the final assistant
    /// text. A turn exceeding `ask_timeout_secs` is killed and yields a short spoken apology (port of the
    /// Python `asyncio.wait_for` fallback), so a hung tool/API call can't wedge the loop.
    pub fn ask(&mut self, prompt: &str) -> String {
        let args = brain::build_args(
            &self.cfg,
            &self.system_prompt,
            prompt,
            self.session_id.as_deref(),
            Some(&self.settings),
        );
        let mut child = match Command::new(&self.cfg.claude_bin)
            .args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(c) => c,
            Err(e) => {
                eprintln!("[brain] failed to spawn {}: {e}", self.cfg.claude_bin);
                return "Sorry, I couldn't reach my brain just now.".to_string();
            }
        };

        // Read + decode stdout on a thread so the main thread can enforce the wall-clock timeout.
        let stdout = child.stdout.take().expect("piped stdout");
        let (tx, rx) = mpsc::channel::<BrainEvent>();
        std::thread::spawn(move || {
            let reader = BufReader::new(stdout);
            for line in reader.lines().map_while(Result::ok) {
                if let Some(ev) = brain::decode_line(&line)
                    && tx.send(ev).is_err()
                {
                    break; // collector gave up (timeout) — stop decoding
                }
            }
        });

        let timeout = Duration::from_secs_f64(self.cfg.ask_timeout_secs);
        let deadline = std::time::Instant::now() + timeout;
        let mut final_text = String::new();
        let mut prior = String::new();
        loop {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                eprintln!("[brain] ask timed out");
                let _ = child.kill();
                let _ = child.wait();
                return "Sorry, that one hung on me — ask again?".to_string();
            }
            match rx.recv_timeout(remaining) {
                Ok(BrainEvent::Init { session_id }) => {
                    if session_id.is_some() {
                        self.session_id = session_id;
                    }
                }
                Ok(BrainEvent::ToolUse { name }) => eprintln!("[tool] {name}"),
                Ok(BrainEvent::Thinking(t)) => {
                    if !t.trim().is_empty() {
                        eprintln!("[think] {t}");
                    }
                }
                Ok(BrainEvent::Text(t)) => {
                    // Intermediate assistant text is a step, not the answer; log the prior step and keep
                    // the latest as the running reply (mirrors the Python `final`/`[think]` handling).
                    let t = t.trim().to_string();
                    if !t.is_empty() {
                        if !prior.is_empty() {
                            eprintln!("[think] {prior}");
                        }
                        prior = t.clone();
                        final_text = t;
                    }
                }
                Ok(BrainEvent::Result { text, is_error }) => {
                    let _ = child.wait();
                    if is_error && text.trim().is_empty() {
                        return "Sorry, I hit a snag on that one.".to_string();
                    }
                    // The terminal `result` text is authoritative; fall back to the last streamed text.
                    return if text.trim().is_empty() {
                        final_text
                    } else {
                        text.trim().to_string()
                    };
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    eprintln!("[brain] ask timed out");
                    let _ = child.kill();
                    let _ = child.wait();
                    return "Sorry, that one hung on me — ask again?".to_string();
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    // Stream ended without a `result` line — return whatever we accumulated.
                    let _ = child.wait();
                    return final_text;
                }
            }
        }
    }
}
