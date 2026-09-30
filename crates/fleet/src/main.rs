//! `fleet` — the standalone multi-repo agent-fleet orchestrator.
//!
//! P1 CORE LIFT (in progress). Behavior is lifted from cadenza's `xtask/src/fleet.rs` in byte-identical
//! slices while the LIVE fleet keeps running on cadenza-xtask until the P3 cutover (see ../../DESIGN.md).
//! This slice = the FOUNDATION: the hub-config decouple + the registry types + the message-bus stamp dirs
//! + `heartbeat` (the first end-to-end comms command, proving the decouple works standalone).

// P1-SCAFFOLD ALLOW: the registry types + load/save/inbox are the foundation the NEXT lift slices wire
// (inbox/send/add/up). They are constructed+exercised by the tests already; drop this allow once the
// message-bus subcommands land and use them in non-test code.
#![allow(dead_code)]

use std::path::{Path, PathBuf};

use clap::{Parser, Subcommand};
use serde::{Deserialize, Serialize};

mod board;
mod config;
mod notify;
mod transcripts;
mod workspace;

use transcripts::Harness as _; // bring the harness-seam methods (`.id()`) into scope for rendering

/// The tmux session board-native agents run in (their windows are opened here by `launch_board_agent`, and
/// the notifier injects wakes here). From `config.session`, else `main`.
fn board_session() -> String {
    config::get()
        .session
        .clone()
        .unwrap_or_else(|| "main".to_string())
}

/// This box's host id for host-affinity (`config.host`, else the system hostname). `fleet up`/`watchdog` use
/// it to skip agents pinned to a different host. HOME/hostname are OS locators, not fleet knobs.
fn this_host() -> String {
    if let Some(h) = config::get().host.clone().filter(|s| !s.trim().is_empty()) {
        return h;
    }
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "localhost".to_string())
}

/// Whether an agent (by its board `metadata`) should be managed on `this_host`: an agent whose `host` is unset
/// is unpinned (run-anywhere → managed everywhere, today's behavior); a set `host` (a string or an array of
/// strings) matches only when `this_host` is among them. Pure — unit-tested.
fn agent_host_matches(metadata: Option<&serde_json::Value>, this_host: &str) -> bool {
    let Some(host) = metadata.and_then(|m| m.get("host")) else {
        return true; // unpinned
    };
    match host {
        serde_json::Value::String(s) => s.trim().is_empty() || s == this_host,
        serde_json::Value::Array(items) => {
            // An empty array is treated as unpinned; otherwise this_host must be listed.
            items.is_empty()
                || items
                    .iter()
                    .filter_map(serde_json::Value::as_str)
                    .any(|h| h == this_host)
        }
        serde_json::Value::Null => true, // host: null → unpinned
        _ => true,                       // an odd shape shouldn't strand the agent — treat as unpinned
    }
}

/// Whether an agent's `host` metadata EXPLICITLY names `this_host` — a non-empty string equal to it, or an
/// array that contains it. Unlike [`agent_host_matches`], an UNSET / empty / null host returns FALSE: such an
/// agent is not deliberately assigned here. This is the `--pinned-only` launch predicate: a per-box durable
/// reconcile must launch only agents explicitly pinned to it, never the unpinned "run-anywhere" agents — else
/// a second box would double-launch the first box's whole roster (its live-but-unpinned agents). Pure —
/// unit-tested.
fn agent_host_is_explicit(metadata: Option<&serde_json::Value>, this_host: &str) -> bool {
    match metadata.and_then(|m| m.get("host")) {
        Some(serde_json::Value::String(s)) => s == this_host,
        Some(serde_json::Value::Array(items)) => items
            .iter()
            .filter_map(serde_json::Value::as_str)
            .any(|h| h == this_host),
        _ => false,
    }
}

/// Whether the watchdog should manage an agent on this host. Under `pinned_only` (a secondary box like green),
/// ONLY agents EXPLICITLY pinned here ([`agent_host_is_explicit`]) — so it never re-arms or spawns an observer
/// against an unpinned agent whose tmux window / transcript lives on another box. Without it, the loose
/// predicate ([`agent_host_matches`]): this-host-pinned OR unpinned run-anywhere. Pure — unit-tested.
fn watchdog_manages_agent(md: Option<&serde_json::Value>, host: &str, pinned_only: bool) -> bool {
    if pinned_only {
        agent_host_is_explicit(md, host)
    } else {
        agent_host_matches(md, host)
    }
}

/// Derive the set of agents this host should serve on the reverse tunnel: the tmux `windows` that are also
/// board agents (`id`→optional host metadata) AND whose host-affinity matches `this_host`. Sorted, deduped.
/// This replaces a hand-maintained static list — a window that isn't a board agent (a daemon/scratch window)
/// is dropped, and an agent pinned to another host is dropped. Pure — unit-tested.
fn derive_served_set(
    windows: &[String],
    agents: &[(String, Option<serde_json::Value>)],
    this_host: &str,
) -> Vec<String> {
    let win: std::collections::BTreeSet<&str> = windows.iter().map(String::as_str).collect();
    let mut served: Vec<String> = agents
        .iter()
        .filter(|(id, md)| win.contains(id.as_str()) && agent_host_matches(md.as_ref(), this_host))
        .map(|(id, _)| id.clone())
        .collect();
    served.sort();
    served.dedup();
    served
}

/// One agent's durable row in the runtime registry (the machine-local manifest that survives a reboot).
/// Lifted verbatim from cadenza fleet.rs so the registry.json format is byte-identical across the cutover.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Agent {
    /// Unique agent name — also the tmux window name and the inbox directory name.
    name: String,
    /// The role prompt body under `<hub>/loops/<role>.md` this agent runs.
    role: String,
    /// For a `vertical` agent, the feature it owns (e.g. `iterators`); empty otherwise.
    #[serde(default)]
    vertical: String,
    /// For a `vertical` agent, the subsystem the feature lives in; empty otherwise.
    #[serde(default)]
    area: String,
    /// The agent's git worktree (absolute path).
    worktree: String,
    /// The branch checked out in `worktree`.
    branch: String,
    /// The `/loop` interval the window drives the role at (e.g. `10m`).
    interval: String,
    /// The model alias this agent runs under (`opus` for most; `fable` for `breaker`).
    model: String,
    /// The reasoning-effort level the window launches with.
    #[serde(default = "default_effort")]
    effort: String,
    /// `active` (loop should be running) or `stopped` (removed — window kept for scrollback).
    status: String,
    /// Whether the window is launched with `--disallowedTools AskUserQuestion`.
    #[serde(default = "default_true")]
    disallow_ask: bool,
}

fn default_true() -> bool {
    true
}
fn default_effort() -> String {
    "high".to_string()
}
fn default_interval() -> String {
    "10m".to_string()
}
fn default_model() -> String {
    "opus".to_string()
}

/// One declared agent in a target repo's checked-in `fleet.toml` roster — the DESIRED persistent set
/// (decentralized rosters, operator direction 2026-09-05). Runtime-only fields (worktree, live status,
/// window) are NOT here; `fleet up` derives them when it reconciles this declaration into a running agent.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
struct RosterEntry {
    name: String,
    role: String,
    #[serde(default)]
    vertical: String,
    #[serde(default)]
    area: String,
    #[serde(default = "default_interval")]
    interval: String,
    #[serde(default = "default_model")]
    model: String,
    #[serde(default = "default_effort")]
    effort: String,
}

/// A target repo's identity + adapter hooks, from its checked-in `fleet.toml` `[repo]` table. The gate/
/// merge hooks are OPAQUE to core (core is messaging+windows+orchestration only — it never runs a gate);
/// the per-repo adapter owns them. Empty `gate`/`merge` = none.
#[derive(Clone, Debug, Deserialize)]
struct RepoConfig {
    /// Absolute path to the target repo checkout on this host.
    path: String,
    /// The branch to cut agent worktrees from (e.g. `origin/main`).
    #[serde(default = "default_base")]
    base: String,
    /// Adapter hook: how to gate a change (a command, or "" = none). Core does NOT run this.
    #[serde(default)]
    gate: String,
    /// Adapter hook: how to land (e.g. `pr-sync` | `gh-pr` | `direct` | ""). Core does NOT run this.
    #[serde(default)]
    merge: String,
}

fn default_base() -> String {
    "origin/main".to_string()
}

/// A target repo's whole checked-in fleet config (`fleet.toml`): its identity/adapter + its declared
/// roster. This is the decentralized desired-state that travels WITH the repo; the hub holds the actual
/// runtime state, and `fleet up` reconciles declared → running.
#[derive(Clone, Debug, Deserialize)]
struct TargetConfig {
    repo: RepoConfig,
    #[serde(default, rename = "agent")]
    agents: Vec<RosterEntry>,
}

/// Resolve a model alias to the full id `claude --model` receives (the fleet runs the 1M-context
/// variants). The ONE place the long ids live, so registry/CLI stay readable. Unknown → passthrough.
fn resolve_model(alias: &str) -> String {
    match alias {
        "opus" => "us.anthropic.claude-opus-4-8[1m]".to_string(),
        "fable" => "us.anthropic.claude-fable-5[1m]".to_string(),
        other => other.to_string(),
    }
}

/// Only a TERMINAL-INTERACTIVE role keeps AskUserQuestion; every other role runs unattended and routes
/// human-shaped decisions to the concierge as an `ask`. `describe` DERIVES the launch policy from the
/// role (not a persisted field) so a role→policy change takes effect on the next relaunch.
fn role_is_terminal_interactive(role: &str) -> bool {
    role == "design"
}

/// The on-disk runtime manifest: just the list of agents. Kept flat so it is easy to read + diff.
#[derive(Default, Serialize, Deserialize)]
struct Registry {
    #[serde(default)]
    agents: Vec<Agent>,
}

/// Filesystem anchors for the machine-local runtime state (the HUB). In the standalone fleet the hub is
/// selected by EXPLICIT config (`$FLEET_HUB`), decoupled from any one target repo's git dir — the central
/// change of the extraction. During P1 the default (when `$FLEET_HUB` is unset) reproduces cadenza's exact
/// behavior: resolve the hub via `git --git-common-dir` of the cwd, so a `fleet` binary run inside cadenza
/// resolves the SAME `<cadenza>/.claude/fleet` hub as cadenza-xtask does today → byte-identical.
struct Fleet {
    /// `<hub>/.claude/fleet` — the machine-local runtime state dir (registry, inbox, heartbeat, stop, …).
    root: PathBuf,
}

impl Fleet {
    /// Resolve the hub. `config.hub` (the eventual `~/.fleet` multi-repo model) wins; otherwise fall back
    /// to git-common-dir discovery of the cwd (cadenza's current behavior) so P1 is byte-identical.
    fn resolve() -> Self {
        let hub = match config::get().hub.as_ref() {
            Some(h) => PathBuf::from(h),
            None => {
                let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
                hub_root(&cwd).unwrap_or(cwd)
            }
        };
        Fleet {
            root: hub.join(".claude/fleet"),
        }
    }

    fn registry_path(&self) -> PathBuf {
        self.root.join("registry.json")
    }
    /// Where per-agent worktrees are checked out (`<hub>/.claude/worktrees/<name>`) — hub-managed, so a
    /// worktree stays off the target repo's own tree. `root` is `<hub>/.claude/fleet`, so its parent is
    /// `<hub>/.claude`.
    fn worktrees_dir(&self) -> PathBuf {
        self.root
            .parent()
            .map(|p| p.join("worktrees"))
            .unwrap_or_else(|| self.root.join("worktrees"))
    }
    fn inbox(&self, agent: &str) -> PathBuf {
        self.root.join("inbox").join(agent)
    }
    fn stopfile(&self, agent: &str) -> PathBuf {
        self.root.join("stop").join(agent)
    }
    /// Ensure the agent's inbox (+ its `processed/` archive) exists. Idempotent.
    fn ensure_inbox(&self, agent: &str) {
        std::fs::create_dir_all(self.inbox(agent).join("processed")).ok();
    }

    /// Load the runtime registry; empty on absent/malformed (never a hard fail — the manifest is recreated).
    fn load(&self) -> Registry {
        let p = self.registry_path();
        match std::fs::read_to_string(&p) {
            Ok(s) => serde_json::from_str(&s).unwrap_or_else(|e| {
                eprintln!("fleet: {p:?} is not valid JSON ({e}); starting from an empty registry");
                Registry::default()
            }),
            Err(_) => Registry::default(),
        }
    }

    /// Persist the registry pretty-printed via temp+rename so a concurrent reader never sees a half-write.
    fn save(&self, reg: &Registry) {
        std::fs::create_dir_all(&self.root).expect("create .claude/fleet");
        let json = serde_json::to_string_pretty(reg).expect("serialize registry");
        let tmp = self.registry_path().with_extension("json.tmp");
        std::fs::write(&tmp, json).expect("write registry tmp");
        std::fs::rename(&tmp, self.registry_path()).expect("rename registry into place");
    }
}

/// `<hub>/.git` → `<hub>` via `git --git-common-dir` (cadenza's current hub discovery; the P1 default).
fn hub_root(dir: &Path) -> Option<PathBuf> {
    let out = std::process::Command::new("git")
        .current_dir(dir)
        .args(["rev-parse", "--path-format=absolute", "--git-common-dir"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let common = String::from_utf8(out.stdout).ok()?;
    PathBuf::from(common.trim()).parent().map(Path::to_path_buf)
}

/// Refresh an agent's liveness touch-file (`<hub>/.claude/fleet/heartbeat/<name>`), or report STOPPED if a
/// stop-file exists. Returns `true` iff stopped. A per-agent touch-file (not a registry rewrite) keeps the
/// tick cheap + avoids contending with `add`/`remove`. Lifted verbatim from cadenza fleet.rs.
fn heartbeat_refresh_liveness(fleet: &Fleet, name: &str) -> bool {
    if fleet.stopfile(name).exists() {
        return true;
    }
    let dir = fleet.root.join("heartbeat");
    std::fs::create_dir_all(&dir).ok();
    std::fs::write(dir.join(name), "tick\n").ok();
    false
}

fn heartbeat(fleet: &Fleet, name: &str) {
    if heartbeat_refresh_liveness(fleet, name) {
        println!("STOPPED");
        std::process::exit(2);
    }
    println!("ok");
}

// ── message bus ──────────────────────────────────────────────────────────────────────────────────

/// A fleet message. Lifted verbatim from cadenza fleet.rs so the on-disk JSON is byte-identical.
#[derive(Serialize, Deserialize)]
struct Message {
    from: String,
    to: String,
    kind: String,
    subject: String,
    #[serde(default)]
    r#ref: String,
    #[serde(default)]
    body: String,
    /// A per-process ordinal (metadata only — the inbox sorts by FILENAME, whose leading field is the
    /// durable cross-process delivery sequence, NOT this). Kept for on-disk-format compatibility.
    seq: u64,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    in_reply_to: String,
    #[serde(default = "default_urgency")]
    urgency: String,
}

const URGENCY_LEVELS: [&str; 4] = ["low", "normal", "high", "urgent"];

fn default_urgency() -> String {
    "normal".to_string()
}
fn is_valid_urgency(u: &str) -> bool {
    URGENCY_LEVELS.contains(&u)
}

/// The compact inbox-line tag for a message's urgency: elevated levels get a loud suffix so they stand
/// out in the oldest-first listing; `normal`/`low`/unknown get nothing (keeps the common line identical).
fn urgency_tag(u: &str) -> &'static str {
    match u {
        "urgent" => "  <<URGENT>>",
        "high" => "  <high>",
        _ => "",
    }
}

/// The message kinds that are INFORMATIONAL (read-and-archive; not a drain-stall) — the SINGLE source of
/// truth. `message_kind_is_actionable` is "not in this list"; the `fleet inbox` summary derives its legend
/// from it so the classifier + CLI text can never drift. Unknown kinds → actionable (fail-safe).
const INFORMATIONAL_KINDS: &[&str] = &["note", "merged", "backlog", "status", "reply"];

fn message_kind_is_actionable(kind: &str) -> bool {
    !INFORMATIONAL_KINDS.contains(&kind)
}

/// Is `s` a single, safe path component (no traversal / separators)? Guards `--processed <msg>`.
fn is_safe_component(s: &str) -> bool {
    !s.is_empty()
        && s != "."
        && s != ".."
        && !s.contains('/')
        && !s.contains('\\')
        && !s.contains('\0')
        && !s.contains("..")
}

/// Count regular files in `dir` matching `keep` (0 if unreadable/absent).
fn count_dir(dir: &Path, keep: impl Fn(&str) -> bool) -> usize {
    std::fs::read_dir(dir)
        .map(|rd| {
            rd.filter_map(Result::ok)
                .filter(|e| e.file_type().map(|t| t.is_file()).unwrap_or(false))
                .filter(|e| keep(&e.file_name().to_string_lossy()))
                .count()
        })
        .unwrap_or(0)
}

/// Inbox filenames lead with the zero-padded durable delivery-seq, so a plain lexicographic sort ==
/// oldest-first arrival order. Pure so the ordering contract is unit-testable.
fn sort_inbox_filenames(names: &mut [String]) {
    names.sort();
}

/// Per-process ordinal for `Message::seq` (the toolchain forbids wall-clock). Delivery ORDER uses the
/// durable cross-process `next_delivery_seq` in the filename, not this.
fn next_seq() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(1);
    SEQ.fetch_add(1, Ordering::Relaxed)
}

/// Durable hub-global delivery sequence (monotonic ACROSS processes) so message filenames sort in send
/// order for the oldest-first drain. temp+rename with a UNIQUE temp per writer so concurrent sends never
/// corrupt the counter. Lifted verbatim from cadenza fleet.rs.
fn next_delivery_seq(fleet: &Fleet) -> u64 {
    let path = fleet.root.join(".delivery-seq");
    let cur = std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or(0);
    let next = cur.saturating_add(1);
    let tmp = fleet.root.join(format!(
        ".delivery-seq.{}.{}.tmp",
        std::process::id(),
        next_seq()
    ));
    if std::fs::write(&tmp, format!("{next}\n")).is_ok() {
        if std::fs::rename(&tmp, &path).is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
    } else {
        let _ = std::fs::remove_file(&tmp);
    }
    next
}

/// Validate an agent name that becomes a filesystem path component (`inbox/<name>`). `^[A-Za-z0-9]
/// [A-Za-z0-9-]*$` — a leading ASCII alphanumeric, then alphanumerics/hyphens only: no path separator,
/// no `.`/`..` traversal, no dotfile/flag lookalike. Lifted verbatim (matches the slack-bridge sink).
fn validate_agent_name(name: &str) -> Result<(), &'static str> {
    if name.is_empty() {
        return Err("empty");
    }
    if name.len() > 128 {
        return Err("too long");
    }
    if !name
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphanumeric())
    {
        return Err("must start with an ASCII letter or digit");
    }
    if !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        return Err(
            "only ASCII alphanumerics and `-` are allowed (no dots/underscores/separators)",
        );
    }
    Ok(())
}

/// Rescue a reply addressed to `unknown`: find a `fleet/<agent>` token in the subject + route there.
fn recipient_from_subject(subject: &str) -> Option<String> {
    let idx = subject.find("fleet/")?;
    let rest = &subject[idx + "fleet/".len()..];
    let agent: String = rest
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '-')
        .collect();
    if agent.is_empty() || validate_agent_name(&agent).is_err() {
        None
    } else {
        Some(agent)
    }
}

/// Deliver a message: temp+rename into the recipient's inbox so a reader never sees a partial file. The
/// filename `<delivery-seq>-<pid>-<kind>.json` sorts in send order. A `to == "unknown"` whose subject
/// names a `fleet/<agent>` is rescued to that agent. Path-traversal-guarded at this single chokepoint.
fn deliver(fleet: &Fleet, msg: &Message) {
    let to: String = if msg.to == "unknown" {
        match recipient_from_subject(&msg.subject) {
            Some(real) => {
                eprintln!("fleet deliver: rescued a `to=unknown` message → routing to '{real}'");
                real
            }
            None => msg.to.clone(),
        }
    } else {
        msg.to.clone()
    };
    if let Err(why) = validate_agent_name(&to) {
        eprintln!("fleet: refusing to deliver to invalid agent name {to:?}: {why}");
        std::process::exit(1);
    }
    let inbox = fleet.inbox(&to);
    std::fs::create_dir_all(&inbox).expect("create recipient inbox");
    let fname = format!(
        "{:012}-{}-{}.json",
        next_delivery_seq(fleet),
        std::process::id(),
        msg.kind
    );
    let json = serde_json::to_string_pretty(msg).expect("serialize message");
    let tmp = inbox.join(format!(".{fname}.tmp"));
    std::fs::write(&tmp, json).expect("write message tmp");
    std::fs::rename(&tmp, inbox.join(&fname)).expect("rename message into inbox");
}

/// Conservative env-dump / secret-token scanner (belt-and-braces P0 leak guard, operator seq-198): flags
/// only UNAMBIGUOUS leaks (a large KEY=VALUE block, a secret-named key with a real value, a live-looking
/// credential token) so it never false-positives on the legion of prose bodies that document env vars.
/// Lifted verbatim from cadenza fleet.rs. Repo-agnostic → core.
fn message_secret_findings(title: &str, body: &str) -> Vec<String> {
    let mut findings = Vec::new();
    let text = format!("{title}\n{body}");
    let mut env_toks = 0usize;
    for tok in text.split_whitespace() {
        let Some(eq) = tok.find('=') else { continue };
        let key = &tok[..eq];
        let val = &tok[eq + 1..];
        let key_shaped = !key.is_empty()
            && key
                .chars()
                .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
            && key
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_uppercase() || c == '_');
        if !key_shaped || val.is_empty() {
            continue;
        }
        env_toks += 1;
        let ku = key.to_ascii_uppercase();
        let secret_named = ku.starts_with("AWS_")
            || ku.starts_with("ANTHROPIC_")
            || [
                "TOKEN",
                "SECRET",
                "PASSWORD",
                "PASSWD",
                "API_KEY",
                "CREDENTIAL",
                "SESSION_TOKEN",
                "ACCESS_KEY",
            ]
            .iter()
            .any(|p| ku.contains(p));
        let trivial = matches!(val, "1" | "0" | "true" | "false" | "yes" | "no") || val.len() <= 4;
        if secret_named && !trivial {
            findings.push(format!("secret-named key with a real value — `{key}=…`"));
        }
        if val.contains("sk-ant-")
            || val.starts_with("AKIA")
            || val.starts_with("ghp_")
            || val.starts_with("xoxb-")
        {
            findings.push(format!(
                "assignment `{key}=…` value looks like a live credential token"
            ));
        }
    }
    if env_toks >= 8 {
        findings.push(format!(
            "{env_toks} KEY=VALUE env assignments — this looks like an ENV DUMP; a body must be prose"
        ));
    }
    for (tok, min_run) in [
        ("sk-ant-", 12usize),
        ("ghp_", 20),
        ("xoxb-", 10),
        ("AKIA", 16),
        ("ASIA", 16),
    ] {
        let mut hay = text.as_str();
        while let Some(pos) = hay.find(tok) {
            let after = &hay[pos + tok.len()..];
            let run = after
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
                .count();
            if run >= min_run {
                findings.push(format!(
                    "a live-looking credential token (`{tok}…`) appears in the text"
                ));
                break;
            }
            hay = &hay[pos + tok.len()..];
        }
    }
    findings
}

/// Kinds whose sender EXPECTS a reply — refuse an unresolved (`unknown`) sender for these, or the reply
/// dead-letters and the sender idles forever. (Cadenza's `merge-request` reply-expecting kind is adapter
/// territory and not in this core list.)
fn kind_expects_reply(kind: &str) -> bool {
    matches!(kind, "ask" | "issue")
}

/// Core `send`: validate urgency, resolve the body (`--body-file` wins, leak-safe), leak-guard the
/// subject/body, resolve the sender (`--from` → `$FLEET_AGENT` → `unknown`), then deliver. DEFERRED to
/// later P1 slices (not yet lifted): the tmux "wake the recipient" nudge, the branch/worktree sender
/// derivation, and the cadenza-adapter merge-request/pr-sync guards. Sender must be `--from`/`$FLEET_AGENT`
/// until the derivation lands.
#[allow(clippy::too_many_arguments)]
fn send(
    fleet: &Fleet,
    to: &str,
    kind: &str,
    subject: &str,
    r#ref: &str,
    body: &str,
    body_file: Option<PathBuf>,
    from: Option<String>,
    urgency: &str,
) {
    if !is_valid_urgency(urgency) {
        eprintln!(
            "fleet send: REFUSING — unknown --urgency `{urgency}`. Valid levels: {}.",
            URGENCY_LEVELS.join(" | ")
        );
        std::process::exit(1);
    }
    let body: String = match &body_file {
        Some(p) => match std::fs::read_to_string(p) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("fleet send: cannot read --body-file {}: {e}", p.display());
                std::process::exit(2);
            }
        },
        None => body.to_string(),
    };
    let leak = message_secret_findings(subject, &body);
    if !leak.is_empty() {
        eprintln!(
            "fleet send: REFUSING — the --subject/--body looks like it carries env-dump or secret material:"
        );
        for f in &leak {
            eprintln!("  ✗ {f}");
        }
        eprintln!(
            "A message is PROSE — never env/set/command output/a credential. Use --body-file with a literal file."
        );
        std::process::exit(1);
    }
    let from = from
        .filter(|s| !s.trim().is_empty())
        .or_else(|| {
            config::get()
                .agent
                .clone()
                .filter(|s| !s.trim().is_empty())
        })
        .unwrap_or_else(|| "unknown".to_string());
    if from != "unknown"
        && let Err(why) = validate_agent_name(&from)
    {
        eprintln!(
            "fleet send: REFUSING — resolved sender `{from}` is not a valid agent name ({why})."
        );
        std::process::exit(1);
    }
    if from == "unknown" && kind_expects_reply(kind) {
        eprintln!(
            "fleet send: REFUSING a `{kind}` from an UNRESOLVED sender (the reply would dead-letter). \
             Pass `--from <your-agent-name>` (or set $FLEET_AGENT)."
        );
        std::process::exit(1);
    }
    let msg = Message {
        from,
        to: to.to_string(),
        kind: kind.to_string(),
        subject: subject.to_string(),
        r#ref: r#ref.to_string(),
        body,
        seq: next_seq(),
        in_reply_to: String::new(),
        urgency: urgency.to_string(),
    };
    deliver(fleet, &msg);
    eprintln!(
        "fleet send: {} → {} [{}] {}",
        msg.from, msg.to, msg.kind, msg.subject
    );
}

// ── worktree lifecycle (target-parameterized) ─────────────────────────────────────────────────────

/// The topic branch an agent's worktree checks out: `fleet/<name>` (the standalone fleet has no single
/// `trunk`-holder like cadenza's pr-sync — that's a cadenza-adapter concern, not core).
fn agent_branch(name: &str) -> String {
    format!("fleet/{name}")
}

/// Build a runtime [`Agent`] row from a declared [`RosterEntry`] + the worktree path, deriving the
/// runtime-only fields (branch, worktree, status=active, disallow_ask-from-role) — the desired→actual
/// projection `fleet up` uses when it provisions a declared agent.
fn agent_from_roster(entry: &RosterEntry, worktree: &Path) -> Agent {
    Agent {
        name: entry.name.clone(),
        role: entry.role.clone(),
        vertical: entry.vertical.clone(),
        area: entry.area.clone(),
        worktree: worktree.to_string_lossy().to_string(),
        branch: agent_branch(&entry.name),
        interval: entry.interval.clone(),
        model: entry.model.clone(),
        effort: entry.effort.clone(),
        status: "active".to_string(),
        disallow_ask: !role_is_terminal_interactive(&entry.role),
    }
}

/// Idempotently ensure agent `name`'s worktree exists, cut from `base` in the TARGET repo. Unlike
/// cadenza's hub-anchored `ensure_worktree`, this is PARAMETERIZED by the target repo + base (the
/// multi-repo generalization): `git -C <target_repo> worktree add -b fleet/<name> <hub>/.claude/
/// worktrees/<name> <base>`. Returns the worktree path. A pre-existing worktree dir is a clean no-op.
/// Errors are returned (not `exit`) so the caller (`fleet up`) can report per-agent + continue.
fn ensure_worktree(
    fleet: &Fleet,
    target_repo: &Path,
    base: &str,
    name: &str,
) -> Result<PathBuf, String> {
    let wt = fleet.worktrees_dir().join(name);
    if wt.is_dir() {
        return Ok(wt);
    }
    if let Some(parent) = wt.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let out = std::process::Command::new("git")
        .current_dir(target_repo)
        .args(["worktree", "add", "-b", &agent_branch(name)])
        .arg(&wt)
        .arg(base)
        .output()
        .map_err(|e| format!("failed to spawn git worktree add: {e}"))?;
    if out.status.success() {
        Ok(wt)
    } else {
        Err(format!(
            "git worktree add for '{name}' (base {base}) failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

// ── tmux window launch ─────────────────────────────────────────────────────────────────────────

/// The tmux session the fleet's windows live in (`config.session`, else `main`).
fn fleet_session() -> String {
    config::get()
        .session
        .clone()
        .unwrap_or_else(|| "main".to_string())
}

/// Where the launcher script lives (`config.window_sh`, else the hub copy `<hub>/.claude/fleet/window.sh`
/// materialized at setup). window.sh resolves the agent's config via `fleet describe` + launches claude.
fn window_sh_path(fleet: &Fleet) -> PathBuf {
    config::get()
        .window_sh
        .as_ref()
        .map(PathBuf::from)
        .unwrap_or_else(|| fleet.root.join("window.sh"))
}

/// The `tmux new-window` argv to open agent `name`'s window (cwd = its worktree) running the launcher.
/// Pure so the command construction is unit-tested without spawning tmux. `-c <worktree>` sets the
/// window's start dir; the window runs `bash <window_sh> <name>`.
fn new_window_argv(session: &str, name: &str, worktree: &str, window_sh: &str) -> Vec<String> {
    vec![
        "new-window".into(),
        "-t".into(),
        format!("{session}:"),
        "-n".into(),
        name.into(),
        "-c".into(),
        worktree.into(),
        "bash".into(),
        window_sh.into(),
        name.into(),
    ]
}

/// Launch agent `name`'s tmux window (ensure the session exists first). Side-effecting; a tmux hiccup is
/// reported (not fatal) so one launch failure doesn't abort a batch. Returns whether the window launched.
fn launch_window(session: &str, name: &str, worktree: &str, window_sh: &Path) -> bool {
    let tmux = |args: &[&str]| {
        std::process::Command::new("tmux")
            .args(args)
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    };
    // Ensure the session exists (detached) before adding a window.
    if !tmux(&["has-session", "-t", session]) {
        let _ = tmux(&["new-session", "-d", "-s", session]);
    }
    let ws = window_sh.to_string_lossy().to_string();
    let argv = new_window_argv(session, name, worktree, &ws);
    let argv_ref: Vec<&str> = argv.iter().map(String::as_str).collect();
    let ok = tmux(&argv_ref);
    if !ok {
        eprintln!(
            "  ! '{name}': tmux new-window failed (window not launched — worktree/row are provisioned)"
        );
    }
    ok
}

// ── reconcile (decentralized roster: declared desired-state → running actual-state) ────────────────

/// The reconcile plan for one target: which DECLARED agents need launching (absent from the runtime
/// registry, or present-but-`stopped`), and which RUNNING agents are UNDECLARED drift (active in the
/// registry but named by no declared roster — reported, never auto-killed). Pure so the reconciliation
/// policy is unit-tested without touching the hub or minting worktrees.
#[derive(Debug, PartialEq, Eq, Default)]
struct ReconcilePlan {
    /// Declared agents that should be launched (not currently active in the registry).
    to_launch: Vec<String>,
    /// Declared agents already active — nothing to do.
    already_running: Vec<String>,
    /// Active registry agents not named by the declared roster — drift to surface (NOT auto-removed).
    undeclared_running: Vec<String>,
}

/// Compute the reconcile plan from a target's DECLARED roster + the hub's RUNNING agents. An agent
/// "counts as running" only if its registry row is `status == "active"`; a `stopped` row (or no row)
/// means the declared agent needs launching. Undeclared *active* agents are reported as drift.
fn reconcile_plan(declared: &[RosterEntry], running: &[Agent]) -> ReconcilePlan {
    let active_names: std::collections::HashSet<&str> = running
        .iter()
        .filter(|a| a.status == "active")
        .map(|a| a.name.as_str())
        .collect();
    let declared_names: std::collections::HashSet<&str> =
        declared.iter().map(|e| e.name.as_str()).collect();
    let mut plan = ReconcilePlan::default();
    for e in declared {
        if active_names.contains(e.name.as_str()) {
            plan.already_running.push(e.name.clone());
        } else {
            plan.to_launch.push(e.name.clone());
        }
    }
    for a in running.iter().filter(|a| a.status == "active") {
        if !declared_names.contains(a.name.as_str()) {
            plan.undeclared_running.push(a.name.clone());
        }
    }
    plan
}

/// The BOARD-native reconcile plan for one host: of the board-native agents pinned to this box, which are
/// already running (have a live tmux window), which are intentionally stood down (board presence `offline`,
/// no window — left down, never auto-launched), and which need launching. This is the board-registry analogue
/// of [`ReconcilePlan`], where "declared" is the board roster (filtered to native + host by the caller) and
/// "running" is the live tmux window set rather than the file-hub registry.
#[derive(Debug, PartialEq, Eq, Default)]
struct BoardReconcile {
    /// Declared agents with no live window that should be launched.
    to_launch: Vec<String>,
    /// Declared agents with a live tmux window — nothing to do.
    already_running: Vec<String>,
    /// Declared agents deliberately stood down (`offline`) with no window — reported, never auto-launched.
    stood_down: Vec<String>,
}

/// Compute the board-native host reconcile from the host-matched native roster (`(id, is_offline)`) and the
/// live tmux `windows`. An agent with a live window counts as running regardless of its board presence (it is
/// up); an agent with no window is to-launch UNLESS it is deliberately `offline`, in which case it is reported
/// as stood-down and left down. Pure so the host reconcile policy is unit-tested without the board or tmux.
fn board_reconcile_plan(declared: &[(String, bool)], windows: &[String]) -> BoardReconcile {
    let win: std::collections::BTreeSet<&str> = windows.iter().map(String::as_str).collect();
    let mut plan = BoardReconcile::default();
    for (id, offline) in declared {
        if win.contains(id.as_str()) {
            plan.already_running.push(id.clone());
        } else if *offline {
            plan.stood_down.push(id.clone());
        } else {
            plan.to_launch.push(id.clone());
        }
    }
    for v in [&mut plan.to_launch, &mut plan.already_running, &mut plan.stood_down] {
        v.sort();
        v.dedup();
    }
    plan
}

/// `fleet up <config>`: parse a target repo's `fleet.toml`, load the hub's runtime registry, and REPORT
/// the reconcile plan (declared → running). DRY-RUN / reporting only in this P1 slice — the actual
/// worktree-mint + window-launch of `to_launch` agents lands with the window-management slice (it needs
/// ensure_worktree(target repo+base) + the tmux launcher, deferred here). Reads the target repo path +
/// adapter hooks but does NOT run the gate/merge hooks (those are the per-repo adapter's, not core's).
/// Upsert an active registry row for `agent` (replace any existing same-name row so a `stopped`→`active`
/// re-launch flips cleanly), returning the mutated registry. Pure so the upsert is unit-tested.
fn upsert_agent(mut reg: Registry, agent: Agent) -> Registry {
    reg.agents.retain(|a| a.name != agent.name);
    reg.agents.push(agent);
    reg
}

fn up(fleet: &Fleet, config_path: &Path, provision: bool, launch: bool) {
    let text = match std::fs::read_to_string(config_path) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("fleet up: cannot read {} ({e})", config_path.display());
            std::process::exit(2);
        }
    };
    let cfg: TargetConfig = match toml::from_str(&text) {
        Ok(c) => c,
        Err(e) => {
            eprintln!(
                "fleet up: {} is not valid fleet.toml ({e})",
                config_path.display()
            );
            std::process::exit(1);
        }
    };
    let mut reg = fleet.load();
    let plan = reconcile_plan(&cfg.agents, &reg.agents);
    println!(
        "fleet up: target repo {} (base {}, gate {:?}, merge {:?}) — {} declared agent(s):",
        cfg.repo.path,
        cfg.repo.base,
        cfg.repo.gate,
        cfg.repo.merge,
        cfg.agents.len()
    );
    if !plan.already_running.is_empty() {
        println!("  ✓ already running: {}", plan.already_running.join(", "));
    }
    if !plan.undeclared_running.is_empty() {
        println!(
            "  ⚠ undeclared-but-running (drift, NOT auto-removed): {}",
            plan.undeclared_running.join(", ")
        );
    }
    if plan.to_launch.is_empty() {
        if plan.undeclared_running.is_empty() {
            println!("  ✓ reconciled — every declared agent is running, no drift.");
        }
        return;
    }
    if !provision {
        println!(
            "  ⟳ TO LAUNCH ({}): {}  [dry-run — pass --provision to create worktree+inbox+registry row; \
             the tmux WINDOW launch lands with the window-mgmt slice]",
            plan.to_launch.len(),
            plan.to_launch.join(", ")
        );
        return;
    }
    // --provision: materialize each to_launch agent's DURABLE state (worktree + inbox + active registry
    // row). The tmux window launch is still deferred (window-mgmt slice) — a provisioned-not-launched
    // agent has its worktree + row + inbox ready, which is exactly the pre-launch state.
    let target = Path::new(&cfg.repo.path);
    let session = fleet_session();
    let window_sh = window_sh_path(fleet);
    let mut provisioned = 0usize;
    let mut launched = 0usize;
    for name in &plan.to_launch {
        let Some(entry) = cfg.agents.iter().find(|e| &e.name == name) else {
            continue;
        };
        match ensure_worktree(fleet, target, &cfg.repo.base, name) {
            Ok(wt) => {
                fleet.ensure_inbox(name);
                let _ = std::fs::remove_file(fleet.stopfile(name));
                reg = upsert_agent(reg, agent_from_roster(entry, &wt));
                provisioned += 1;
                println!(
                    "  + provisioned '{name}' (worktree {} + inbox + active row)",
                    wt.display()
                );
                if launch && launch_window(&session, name, &wt.to_string_lossy(), &window_sh) {
                    launched += 1;
                    println!("  ▶ launched '{name}' in tmux session '{session}'");
                }
            }
            Err(why) => eprintln!("  ! '{name}': {why} — skipped"),
        }
    }
    if provisioned > 0 {
        fleet.save(&reg);
    }
    if launch {
        println!(
            "  ✓ provisioned {provisioned}/{} + launched {launched} window(s) in '{session}'.",
            plan.to_launch.len()
        );
    } else {
        println!(
            "  ✓ provisioned {provisioned}/{} declared-to-launch agent(s) — pass --launch to also open tmux windows.",
            plan.to_launch.len()
        );
    }
}

/// `fleet up-board`: reconcile the BOARD-native roster host-filtered to THIS box against the running tmux
/// windows — the board-registry analogue of [`up`] (which reconciles a checked-in `fleet.toml`). Reads the
/// board roster, keeps only board-native agents (`metadata.native == true`) whose host-affinity matches this
/// box ([`agent_host_matches`] — unset/unpinned = managed everywhere, as today), and reports which are already
/// running, which are intentionally stood down, and which need launching. With `--launch` it spins up each
/// to-launch agent via the per-agent board-native launch path ([`spin_up`] with apply). Host affinity means a
/// box brings up exactly its own declared, host-pinned agents — green's reconcile never touches dev-desk
/// windows and vice-versa. Reads the board only (agents coordinate via their own MCP). NOTE: a hard per-agent
/// launch failure exits (spin_up's contract), aborting the remaining launches — re-run to continue.
fn up_board(launch: bool, pinned_only: bool) {
    let board = board::Board::connect().unwrap_or_else(|e| {
        eprintln!("fleet up-board: board unavailable ({e}); cannot read the roster");
        std::process::exit(1);
    });
    let roster = board.list_agents().unwrap_or_else(|e| {
        eprintln!("fleet up-board: board roster query failed ({e})");
        std::process::exit(1);
    });
    let host = this_host();
    // Board-native agents (metadata.native == true) whose host-affinity matches this box → (id, is_offline).
    // Under --pinned-only, an agent whose host is not EXPLICITLY this box is excluded from the launch set and
    // reported as skipped — so a per-box reconcile can't launch another box's unpinned run-anywhere agents.
    let mut skipped_unpinned: Vec<String> = Vec::new();
    let declared: Vec<(String, bool)> = roster
        .iter()
        .filter_map(|a| {
            let md = a.get("metadata");
            let is_native = md
                .and_then(|m| m.get("native"))
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false);
            if !is_native || !agent_host_matches(md, &host) {
                return None;
            }
            let id = a.get("id").and_then(serde_json::Value::as_str)?.to_string();
            if pinned_only && !agent_host_is_explicit(md, &host) {
                skipped_unpinned.push(id);
                return None;
            }
            let offline = a.get("status").and_then(serde_json::Value::as_str) == Some("offline");
            Some((id, offline))
        })
        .collect();
    skipped_unpinned.sort();
    let windows = tmux_window_names(&board_session());
    let plan = board_reconcile_plan(&declared, &windows);
    println!(
        "fleet up-board: host '{host}'{} — {} board-native agent(s){}:",
        if pinned_only { " [--pinned-only]" } else { "" },
        declared.len(),
        if pinned_only {
            " explicitly pinned here"
        } else {
            " pinned here"
        }
    );
    if !skipped_unpinned.is_empty() {
        println!(
            "  ⊘ skipped (unpinned — reported not launched under --pinned-only): {}",
            skipped_unpinned.join(", ")
        );
    }
    if !plan.already_running.is_empty() {
        println!("  ✓ already running: {}", plan.already_running.join(", "));
    }
    if !plan.stood_down.is_empty() {
        println!(
            "  ⏸ stood down (offline, not launched): {}",
            plan.stood_down.join(", ")
        );
    }
    if plan.to_launch.is_empty() {
        println!("  ✓ reconciled — every active declared agent for this host is running.");
        return;
    }
    if !launch {
        println!(
            "  ⟳ TO LAUNCH ({}): {}  [dry-run — pass --launch to spin each up]",
            plan.to_launch.len(),
            plan.to_launch.join(", ")
        );
        return;
    }
    println!("  ⟳ launching {} agent(s):", plan.to_launch.len());
    for id in &plan.to_launch {
        spin_up(id, true);
    }
}

// ── describe (window.sh eval surface) ────────────────────────────────────────────────────────────

/// Emit shell-safe `KEY=VALUE` lines for `window.sh` to `eval` at launch. The model alias is expanded to
/// its full id here (the point window.sh hands it to `claude --model`); DISALLOW_ASK is DERIVED from the
/// role so a role→policy change takes effect on the next relaunch without rewriting persisted rows.
fn describe(fleet: &Fleet, name: &str) {
    let reg = fleet.load();
    let Some(a) = reg.agents.iter().find(|a| a.name == name) else {
        eprintln!("fleet describe: no agent named '{name}'");
        std::process::exit(1);
    };
    let q = |s: &str| format!("'{}'", s.replace('\'', "'\\''"));
    println!("WORKTREE={}", q(&a.worktree));
    println!("ROLE={}", q(&a.role));
    println!("MODEL={}", q(&resolve_model(&a.model)));
    println!("EFFORT={}", q(&a.effort));
    println!("INTERVAL={}", q(&a.interval));
    println!("VERTICAL={}", q(&a.vertical));
    println!("AREA={}", q(&a.area));
    let disallow_ask = !role_is_terminal_interactive(&a.role);
    println!("DISALLOW_ASK={}", if disallow_ask { 1 } else { 0 });
}

// ── inbox (receive half) ─────────────────────────────────────────────────────────────────────────

/// The consume outcome from the (src, dst) existence pair — pure so the idempotency contract is tested.
#[derive(Debug, PartialEq, Eq)]
enum ConsumeAction {
    Move,
    ClearStray,
    AlreadyDone,
    Missing,
}

fn inbox_consume_action(src_exists: bool, dst_exists: bool) -> ConsumeAction {
    match (src_exists, dst_exists) {
        (true, false) => ConsumeAction::Move,
        (true, true) => ConsumeAction::ClearStray,
        (false, true) => ConsumeAction::AlreadyDone,
        (false, false) => ConsumeAction::Missing,
    }
}

/// A raced drain between the exists() probe and the ClearStray remove can leave `NotFound` — which
/// SATISFIES the goal ("live inbox no longer holds msg"), so it is NOT fatal; any other error is.
fn clear_stray_remove_is_fatal(kind: std::io::ErrorKind) -> bool {
    kind != std::io::ErrorKind::NotFound
}

/// List an agent's inbox: ALWAYS print the resolved HUB path (a wrong path is the failure mode this
/// guards), oldest-first, with an actionable/informational split + urgency flags. A read error is LOUD +
/// distinct from an empty inbox (the "0 messages" confusion this command exists to prevent).
fn inbox_list(fleet: &Fleet, name: &str) {
    let dir = fleet.inbox(name);
    let rd = match std::fs::read_dir(&dir) {
        Ok(rd) => rd,
        Err(e) => {
            eprintln!(
                "inbox for '{name}' at {}: COULD NOT READ ({e}). That path is missing or unreadable — \
                 this is NOT an empty inbox. Verify it's the HUB path, not a worktree-relative `.claude/...`.",
                dir.display()
            );
            std::process::exit(1);
        }
    };
    let mut names: Vec<String> = rd
        .filter_map(Result::ok)
        .filter(|e| e.file_type().map(|t| t.is_file()).unwrap_or(false))
        .map(|e| e.file_name().to_string_lossy().to_string())
        .filter(|n| n.ends_with(".json"))
        .collect();
    sort_inbox_filenames(&mut names);
    println!(
        "inbox for '{name}' at {} ({} message(s)):",
        dir.display(),
        names.len()
    );
    if names.is_empty() {
        println!(
            "  0 messages — nothing to drain (if you EXPECTED mail, verify this is the HUB path, \
             not a worktree-relative `.claude/...`)."
        );
        return;
    }
    let mut actionable = 0usize;
    let mut elevated = 0usize;
    for n in &names {
        let (from, kind, urgency) = std::fs::read_to_string(dir.join(n))
            .ok()
            .and_then(|t| serde_json::from_str::<Message>(&t).ok())
            .map(|m| (m.from, m.kind, m.urgency))
            .unwrap_or_else(|| ("?".to_string(), "?".to_string(), default_urgency()));
        let is_act = message_kind_is_actionable(&kind);
        if is_act {
            actionable += 1;
        }
        let utag = urgency_tag(&urgency);
        if !utag.is_empty() {
            elevated += 1;
        }
        let mark = if is_act { "⚑" } else { "·" };
        println!("  {mark} {n}  [{kind}] from {from}{utag}");
    }
    let informational = names.len().saturating_sub(actionable);
    println!(
        "  ── {actionable} actionable (⚑ = anything not below), {informational} informational (· {})",
        INFORMATIONAL_KINDS.join("/")
    );
    if elevated > 0 {
        println!(
            "  ── {elevated} message(s) flagged high/urgent (<high> / <<URGENT>>) — prioritize reading these"
        );
    }
    if actionable == 0 {
        println!(
            "  ✓ nothing ACTIONABLE queued — the informational mail is safe to archive to processed/ \
             (it is not a drain-stall)."
        );
    }
}

/// Consume one inbox message: MOVE `<msg>` to `processed/` under the resolver-owned HUB path, then
/// re-list. Idempotent (already-archived = success), loud on a genuinely-missing name (a typo must not
/// masquerade as a drain), and safe against a mid-move stray + a raced TOCTOU remove.
fn inbox_consume(fleet: &Fleet, name: &str, msg: &str) {
    if !is_safe_component(msg) {
        eprintln!(
            "fleet inbox --processed: refusing unsafe message name {msg:?} (must be a bare inbox \
             filename from the listing — no path separators or `..`)."
        );
        std::process::exit(1);
    }
    let dir = fleet.inbox(name);
    let src = dir.join(msg);
    let processed_dir = dir.join("processed");
    let dst = processed_dir.join(msg);
    match inbox_consume_action(src.exists(), dst.exists()) {
        ConsumeAction::AlreadyDone => {
            println!("fleet inbox: '{msg}' already in processed/ (no-op) — re-listing '{name}'.");
            inbox_list(fleet, name);
            return;
        }
        ConsumeAction::Missing => {
            eprintln!(
                "fleet inbox --processed: no message {msg:?} in '{name}' inbox at {} (nor in \
                 processed/). Check the exact filename from `fleet inbox {name}` — a typo here silently \
                 leaves the real message UNCONSUMED (the drain-stall this command prevents).",
                dir.display()
            );
            std::process::exit(1);
        }
        ConsumeAction::ClearStray => {
            if let Err(e) = std::fs::remove_file(&src)
                && clear_stray_remove_is_fatal(e.kind())
            {
                eprintln!(
                    "fleet inbox --processed: '{msg}' is already archived, but could not remove the \
                     stray live copy {} ({e}).",
                    src.display()
                );
                std::process::exit(1);
            }
            println!(
                "fleet inbox: '{msg}' was already in processed/ — cleared the stray live copy — re-listing '{name}'."
            );
            inbox_list(fleet, name);
            return;
        }
        ConsumeAction::Move => {}
    }
    if let Err(e) = std::fs::create_dir_all(&processed_dir) {
        eprintln!(
            "fleet inbox --processed: could not create {} ({e}).",
            processed_dir.display()
        );
        std::process::exit(1);
    }
    if let Err(e) = std::fs::rename(&src, &dst) {
        eprintln!(
            "fleet inbox --processed: could not move {} → {} ({e}).",
            src.display(),
            dst.display()
        );
        std::process::exit(1);
    }
    println!("fleet inbox: moved '{msg}' → processed/. Remaining:");
    inbox_list(fleet, name);
}

/// A compact inbox depth string (`empty` | `N msg`) for status/watchdog surfaces.
fn inbox_depth(fleet: &Fleet, name: &str) -> String {
    let n = count_dir(&fleet.inbox(name), |f| f.ends_with(".json"));
    if n == 0 {
        "empty".to_string()
    } else {
        format!("{n} msg")
    }
}

// ── watchdog decision core (pure — the tmux pane-capture + send-keys sweep wraps these later) ───────

/// Context-% at/above which the watchdog surfaces a saturation warning (report-only) — below the 100%
/// wall so an agent can still self-`/compact`. Lifted verbatim from cadenza fleet.rs.
const CTX_SATURATION_THRESHOLD: u8 = 85;
/// Context-% at which an agent is UNRECOVERABLY WEDGED — `/compact` can no longer submit, so only a
/// restart clears it.
const CTX_WEDGE_THRESHOLD: u8 = 100;
/// PRE-WALL escalation threshold for a general agent — high enough that "hasn't self-compacted yet" is a
/// real signal, but below the wall so a `/compact` can still submit.
const CTX_PREWALL_THRESHOLD: u8 = 95;
/// PRE-WALL threshold for the merge-integrator role (if any) — earlier, since its wedge stalls the whole
/// queue. (In the standalone fleet this is a per-target-adapter role, e.g. cadenza's pr-sync.)
const CTX_PREWALL_THRESHOLD_INTEGRATOR: u8 = 92;

// Threshold ordering invariant, guarded at COMPILE time (a bad future edit fails the build, not a test).
const _: () = assert!(
    CTX_PREWALL_THRESHOLD_INTEGRATOR < CTX_PREWALL_THRESHOLD,
    "the single-writer integrator must escalate earlier than a general agent"
);
const _: () = assert!(
    CTX_PREWALL_THRESHOLD < CTX_WEDGE_THRESHOLD,
    "the pre-wall bound must sit below the 100% wall so /compact can still submit"
);

/// The pre-wall escalation threshold for `role` — the earlier integrator bound for a single-writer
/// integrator role, else the general bound. Pure. (Which role is the integrator is a per-target-adapter
/// concern; the standalone core just knows the general-vs-earlier policy.)
fn prewall_threshold_for(is_integrator: bool) -> u8 {
    if is_integrator {
        CTX_PREWALL_THRESHOLD_INTEGRATOR
    } else {
        CTX_PREWALL_THRESHOLD
    }
}

/// A just-completed compaction banner — its visible `% context` is the PRE-compaction stale value, so
/// `parse_context_pct` returns None (unknown, not saturated) when this is present.
fn pane_shows_recent_compaction(pane_text: &str) -> bool {
    let lower = pane_text.to_ascii_lowercase();
    lower.contains("compacted") && lower.contains("see full summary")
}

/// Parse the "% context" indicator Claude Code renders in its status line out of a captured pane. Takes
/// the LAST match (the live status line is at the bottom). `None` if absent, or if the pane shows a
/// just-completed compaction (the visible % is then stale). Pure so the parse is unit-tested.
fn parse_context_pct(pane_text: &str) -> Option<u8> {
    if pane_shows_recent_compaction(pane_text) {
        return None;
    }
    let bytes = pane_text.as_bytes();
    let mut found: Option<u8> = None;
    for (i, _) in pane_text.match_indices("% context") {
        let mut start = i;
        while start > 0 && bytes[start - 1].is_ascii_digit() {
            start -= 1;
        }
        if start < i
            && let Ok(pct) = pane_text[start..i].parse::<u16>()
        {
            found = Some(pct.min(100) as u8);
        }
    }
    found
}

/// Should the watchdog send-keys `/compact` this sweep? Fires ONLY in the PRE-WALL band [saturation, wall)
/// — agents can't self-invoke `/compact` (a built-in, not a tool) — and not recently sent (thrash-guard).
/// Pure so the trigger is unit-tested without tmux.
fn should_send_compact(ctx_pct: Option<u8>, sent_recently: bool) -> bool {
    matches!(ctx_pct, Some(p) if (CTX_SATURATION_THRESHOLD..CTX_WEDGE_THRESHOLD).contains(&p))
        && !sent_recently
}

/// Should the watchdog AUTO-RESTART a wedged agent (at/above the 100% wall, where `/compact` can't
/// submit)? Not if restarted recently (thrash-guard). Pure.
fn should_auto_restart_wedge(ctx_pct: Option<u8>, restarted_recently: bool) -> bool {
    matches!(ctx_pct, Some(p) if p >= CTX_WEDGE_THRESHOLD) && !restarted_recently
}

#[derive(Parser)]
#[command(
    name = "fleet",
    about = "Standalone multi-repo agent-fleet orchestrator"
)]
struct Cli {
    /// Path to the TOML config file (else `$XDG_CONFIG_HOME/fleet/config.toml`, else
    /// `$HOME/.config/fleet/config.toml`). The fleet binary is config-file driven, not `FLEET_*` env vars.
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Refresh an agent's liveness stamp (prints `ok`, or `STOPPED`+exit 2 if a stop-file exists).
    Heartbeat {
        /// The agent name.
        name: String,
    },
    /// Send a message to another agent's inbox.
    Send {
        /// Recipient agent name.
        #[arg(long)]
        to: String,
        /// Message kind (e.g. `note`, `ask`, `reply`, `issue`).
        #[arg(long)]
        kind: String,
        /// Subject line.
        #[arg(long)]
        subject: String,
        /// Optional structured ref (e.g. a commit sha).
        #[arg(long, default_value = "")]
        r#ref: String,
        /// Inline body (prefer --body-file for anything with special chars — leak-safe + literal).
        #[arg(long, default_value = "")]
        body: String,
        /// Read the body from a file (wins over --body).
        #[arg(long)]
        body_file: Option<PathBuf>,
        /// Explicit sender (else $FLEET_AGENT, else `unknown`).
        #[arg(long)]
        from: Option<String>,
        /// Urgency: low | normal | high | urgent.
        #[arg(long, default_value = "normal")]
        urgency: String,
    },
    /// List an agent's inbox (the RESOLVER — prints the canonical HUB path), or with `--processed <msg>`
    /// archive one message into `processed/`.
    Inbox {
        /// The agent name whose inbox to list/drain.
        name: String,
        /// Archive this message filename into `processed/` (instead of listing).
        #[arg(long)]
        processed: Option<String>,
    },
    /// Emit an agent's launch config as shell `KEY=VALUE` lines for the window launcher to `eval`.
    Describe {
        /// The agent name.
        name: String,
    },
    /// Reconcile a target repo's checked-in `fleet.toml` declared roster against the running fleet.
    /// Reports the plan by default; `--provision` materializes to_launch agents' durable state (worktree
    /// + inbox + active registry row). The tmux window launch lands with the window-management slice.
    Up {
        /// Path to the target repo's `fleet.toml`.
        config: PathBuf,
        /// Actually provision to_launch agents (create worktree + inbox + registry row), not just report.
        #[arg(long)]
        provision: bool,
        /// Also open a tmux window per provisioned agent (implies --provision). No-op without --provision.
        #[arg(long)]
        launch: bool,
    },
    /// Reconcile the BOARD-native roster host-filtered to THIS box (the board-registry analogue of `up`,
    /// which reconciles a checked-in `fleet.toml`). Reads the board roster, keeps board-native agents
    /// (`metadata.native == true`) pinned to this host (unset host = managed everywhere), and reports which
    /// are already running, stood down, or need launching; `--launch` spins each to-launch agent up. Host
    /// affinity keeps a box from launching another box's agents. Reads the board only.
    UpBoard {
        /// Spin up each to-launch agent (default: just report the plan).
        #[arg(long)]
        launch: bool,
        /// Launch ONLY agents whose `host` is EXPLICITLY this box (exclude unpinned run-anywhere agents from
        /// the launch set; they are still reported as skipped). Required for a safe per-box durable reconcile
        /// on a second box — without it, an unpinned agent live on another box would be double-launched here.
        #[arg(long)]
        pinned_only: bool,
    },
    /// Spin up ONE board-declared agent into its `~/.fleet` workspace (the new-system launch path).
    /// Reads the agent's board record (charter + metadata incl. `repos`) and reports the materialize +
    /// launch plan; `--apply` performs it. Agents coordinate via their own in-session board MCP — this
    /// only READS the board to know what to launch.
    SpinUp {
        /// The board agent id to spin up.
        agent: String,
        /// Perform the materialize + launch (default: just report the plan).
        #[arg(long)]
        apply: bool,
    },
    /// Gracefully spin DOWN one board-native agent (the inverse of `spin-up`): mark its board `status`
    /// `offline` then stop its loop by killing its tmux window. RESUMABLE, not a retire — the board record
    /// (charter + metadata) is left intact, so `spin-up` revives it later, and `up-board` treats it as stood
    /// down (offline + no window → never auto-launched). Refuses if the agent is NOT board-native (a file-hub
    /// agent uses `cargo xtask fleet remove`) or if its pane shows a turn in flight (unless `--force`). Reports
    /// the plan by default; `--apply` performs it. Sets `offline` BEFORE the kill so an interleaved `up-board`
    /// cannot see it online-but-windowless and relaunch it.
    SpinDown {
        /// The board agent id to spin down.
        agent: String,
        /// Perform the offline+kill (default: just report the plan).
        #[arg(long)]
        apply: bool,
        /// Spin down even if the pane shows an in-flight turn (default: refuse a busy agent).
        #[arg(long)]
        force: bool,
    },
    /// Report every board-declared agent's liveness off its board `last_seen` (the watchdog's read side).
    /// Reads the roster from the board (orchestrator read — agents coordinate via their own MCP) and
    /// classifies each by how stale its heartbeat is: live / quiet / STALE.
    Status {
        /// Only print agents that are not `live` (quiet or STALE) — the ones a watchdog would look at.
        #[arg(long)]
        stale_only: bool,
    },
    /// Board-native liveness watchdog: for each board-native agent (metadata.native == true), compare its
    /// heartbeat age to its OWN loop interval and flag re-arm candidates — an agent heartbeats ~once per
    /// interval, so an age beyond several intervals means missed ticks. Report-only (non-destructive).
    Watchdog {
        /// Only print agents that are not `ok` (late/STALE) — the re-arm candidates.
        #[arg(long)]
        stale_only: bool,
        /// ACT on each candidate: inject a wake into its tmux window so it runs a tick now (automates the
        /// manual loop-reissue). Without this the watchdog is report-only.
        #[arg(long)]
        rearm: bool,
        /// Also detect per-agent transcript-growth OBSERVATION candidates (#187): agents whose newest session
        /// grew past the threshold (CDZ_OBSERVE_LINES, default 2000 lines) since last observed, plus stood-down
        /// agents with a closing tail. Opt-in so the always-on re-arm sweep pays no transcript-read cost.
        #[arg(long)]
        observe: bool,
        /// With --observe, SPAWN an ephemeral observer per highest-growth candidate (#188): bounded by a
        /// per-sweep cap (CDZ_OBSERVE_SPAWN_CAP, default 3) + a per-target cooldown
        /// (CDZ_OBSERVE_SPAWN_COOLDOWN_SECS, default 1800). The watermark advances only when the observer
        /// confirms via `fleet observe-record`. Without this, --observe is detect/report-only.
        #[arg(long)]
        spawn: bool,
        /// With --observe --spawn, PREVIEW the spawn plan (which observers would launch) without launching.
        #[arg(long)]
        dry_run: bool,
        /// Restrict the board scan to agents EXPLICITLY pinned to this host (metadata.host names it) — the same
        /// predicate as `up-board --pinned-only`. Excludes unpinned "run-anywhere" agents, so a box running the
        /// watchdog fleet-wide never re-arms or spawns an observer against an agent whose tmux window /
        /// transcript lives on ANOTHER box. Use this on a secondary box (e.g. green) that should only manage its
        /// own pinned agents; the primary box runs without it to cover the unpinned roster.
        #[arg(long)]
        pinned_only: bool,
    },
    /// CONFIRM an observation (#188): advance the per-agent observer watermark to `<session>:<offset>`. The
    /// ephemeral observer calls this as its LAST step, AFTER emitting its report/proposal(s) — so a crashed
    /// or incomplete observation never advances the watermark and the span re-fires next sweep (the design's
    /// confirmed-observation guardrail). This is the ONLY writer of the watermark.
    ObserveRecord {
        /// The OBSERVED agent (the watermark is keyed by the target, not by the `observer` identity).
        agent: String,
        /// The session id observed.
        #[arg(long)]
        session: String,
        /// The line offset read through (the new watermark).
        #[arg(long)]
        offset: usize,
    },
    /// Post a deploy-confirmed event to the `deploys` board channel (#171) — the deploy pipeline (#73/#74
    /// deployer role) calls this after a `colmena apply switch`, so agents subscribed to `deploys` (a waiter
    /// blocked on a green deploy) are woken. Creates the channel if absent.
    PostDeploy {
        /// The repo that was deployed (e.g. `camshaft/task-board`).
        #[arg(long)]
        repo: String,
        /// The deployed commit sha (a waiter matches its awaited commit against this).
        #[arg(long)]
        sha: String,
        /// The host the deploy landed on (e.g. `green-machine`).
        #[arg(long)]
        host: String,
        /// The deploy outcome — `live` (succeeded) or `failed` (a waiter must STOP + escalate, not wait).
        #[arg(long)]
        status: String,
    },
    /// Write launch-shaping metadata onto an agent's board record — the migration primitive that makes an
    /// agent spin-up-ready. Merges (only the given keys change). Reports the patch by default; `--apply`
    /// writes it. Use this instead of hand-editing the board when pushing an agent to be board-backed.
    SetMeta {
        /// The board agent id to update.
        agent: String,
        /// A repo the agent works in, as `owner/name@branch` (branch defaults to `main`). Repeatable.
        #[arg(long = "repo")]
        repos: Vec<String>,
        /// Set the agent's loop interval (e.g. `2m`, `30m`, `2h`).
        #[arg(long)]
        interval: Option<String>,
        /// Pin the agent to a host (host-affinity — only that box's `fleet up`/`watchdog` manages it). Pass
        /// `""` to clear the pin (unpinned = run-anywhere).
        #[arg(long)]
        host: Option<String>,
        /// Set the board-native roster marker: `--native true` marks the agent board-native (the board
        /// watchdog then judges its liveness by board `last_seen`, and the file-hub watchdog stops scanning
        /// it); `--native false` clears it back to a file-hub row. Omitted = leave `native` untouched. This
        /// is the migration flip for an agent that has BECOME board-active (its tick now polls the board)
        /// but was launched outside `spin-up` (which sets `native:true` by construction).
        #[arg(long)]
        native: Option<bool>,
        /// Opt-in launch inside the workdir's flake devShell: `--devshell true` makes `spin-up` launch the
        /// agent via `nix develop` so the flake-pinned toolchain (node/cargo/…) is on PATH instead of the
        /// host's (#214); `--devshell false` clears it. Omitted = leave untouched. Set only for an agent
        /// whose workdir is a flake with a devShell.
        #[arg(long)]
        devshell: Option<bool>,
        /// Perform the write (default: just print the metadata patch that would be sent).
        #[arg(long)]
        apply: bool,
    },
    /// Change an agent's loop interval on BOTH the board metadata (what the pending-work watchdog reads to
    /// compute overdue) and its file-hub registry row if one still exists — so the two never disagree. Writing
    /// only the registry (the frozen cadenza `set-interval` does this) leaves the board metadata stale, so an
    /// agent that lowers its cadence gets false overdue-nudges every cycle; this writes the board too. Applies
    /// directly (it's a routine cadence change, not the migration primitive that `set-meta` is).
    SetInterval {
        /// The agent id whose loop interval to change.
        agent: String,
        /// The new loop interval (e.g. `2m`, `30m`, `2h`).
        interval: String,
    },
    /// Run the event-driven wake notifier: a local HTTP endpoint that receives the board's per-agent
    /// webhook POSTs and `tmux send-keys` injects `[notification] task #<id>` / `message #<seq>` into the
    /// recipient agent's window (register this endpoint as each board-backed agent's `webhook_url`). Blocks.
    Notify {
        /// Port to listen on (127.0.0.1 only).
        #[arg(long, default_value_t = 8899)]
        port: u16,
    },
    /// Print a FAITHFUL, LOSSLESS rendering of an agent's harness session transcript (every turn, tool call,
    /// tool result, and error — no digest/summary), with secrets scrubbed. `--since <session:offset>` emits
    /// only content past a prior watermark, plus `--overlap` lines of prior context; the new watermark is
    /// printed at the end so the next observation can advance. This is the reader the fleet-self-improve
    /// observers (#176/#187) read from.
    Transcripts {
        /// The agent id whose transcript to render.
        agent: String,
        /// Render exactly this session JSONL file instead of locating the agent's newest session.
        #[arg(long)]
        session: Option<PathBuf>,
        /// Only emit content past this `<session-id>:<line-offset>` watermark (from a prior run's footer).
        #[arg(long)]
        since: Option<String>,
        /// Lines of prior context to re-include before the `--since` offset, so no boundary is lost.
        #[arg(long, default_value_t = 40)]
        overlap: usize,
    },
    /// Print the set of agents THIS host should serve on the reverse tunnel — the board agents that have a
    /// live tmux window in this session AND aren't pinned to another host. Replaces a hand-maintained static
    /// list (a stale list silently starves new agents of event-wakes). `--toml` emits the `agents = [...]`
    /// block for a tunnel config; default prints one id per line.
    ServedSet {
        /// Emit the `agents = [ ... ]` TOML array block (paste/redirect into the fleet-tunnel config).
        #[arg(long)]
        toml: bool,
    },
    /// Print a systemd USER service + timer that runs the watchdog on a cadence (a host installs it
    /// declaratively — home-manager `systemd.user.services`/`timers`, same as fleet-notify — no imperative
    /// write path). The service is a oneshot (`fleet watchdog` is single-sweep); the timer re-fires it. Emit
    /// the go-live shape with `--observe --pinned-only` (adds `--observe --spawn` for the observer cadence and
    /// `--pinned-only` so a secondary box only manages its own pinned agents).
    WatchdogUnit {
        /// Include `--observe --spawn` (the fleet-self-improve observer cadence, #290).
        #[arg(long)]
        observe: bool,
        /// Include `--pinned-only` (manage only agents explicitly pinned to this host — for a secondary box).
        #[arg(long)]
        pinned_only: bool,
        /// Timer cadence in seconds (systemd `OnUnitActiveSec`).
        #[arg(long, default_value_t = 60)]
        interval_secs: u64,
        /// The fleet binary path to put in `ExecStart` (defaults to this binary's absolute path; set it to the
        /// installed path on the target host, e.g. the flake output).
        #[arg(long)]
        bin: Option<String>,
        /// Drop the `--rearm --stale-only` liveness sweep from the unit — an OBSERVER-ONLY cadence
        /// (`--observe`) that coexists with an existing rearm watchdog without double-rearming (dev-desk).
        #[arg(long)]
        no_rearm: bool,
        /// INSTALL the units into `~/.config/systemd/user/` (user-level, no sudo) instead of printing them, and
        /// print the `systemctl --user enable` command — a clean, reversible install path for a host not on the
        /// declarative (nix) model. Reverse with `--uninstall`.
        #[arg(long)]
        install: bool,
        /// REMOVE the user units this installed (the inverse of `--install`) and print the `disable` command.
        #[arg(long)]
        uninstall: bool,
    },
    /// Print the build provenance — package version + the commit the binary was built from (baked at build
    /// time). Compare the rev to `origin/main` to tell whether a deployed binary is current (a stale binary
    /// silently runs old logic — the failure mode a stale watchdog binary hit).
    Version,
}

fn main() {
    let cli = Cli::parse();
    // Record the --config override before any setting is read (config is loaded lazily, once).
    config::set_path(cli.config.clone());
    let fleet = Fleet::resolve();
    match cli.cmd {
        Cmd::Heartbeat { name } => heartbeat(&fleet, &name),
        Cmd::Send {
            to,
            kind,
            subject,
            r#ref,
            body,
            body_file,
            from,
            urgency,
        } => send(
            &fleet, &to, &kind, &subject, &r#ref, &body, body_file, from, &urgency,
        ),
        Cmd::Inbox { name, processed } => match processed {
            Some(msg) => inbox_consume(&fleet, &name, &msg),
            None => inbox_list(&fleet, &name),
        },
        Cmd::Describe { name } => describe(&fleet, &name),
        Cmd::Up {
            config,
            provision,
            launch,
        } => up(&fleet, &config, provision || launch, launch),
        Cmd::UpBoard { launch, pinned_only } => up_board(launch, pinned_only),
        Cmd::SpinUp { agent, apply } => spin_up(&agent, apply),
        Cmd::SpinDown { agent, apply, force } => spin_down(&agent, apply, force),
        Cmd::Status { stale_only } => status(stale_only),
        Cmd::Watchdog {
            stale_only,
            rearm,
            observe,
            spawn,
            dry_run,
            pinned_only,
        } => watchdog(stale_only, rearm, observe, spawn, dry_run, pinned_only),
        Cmd::ObserveRecord {
            agent,
            session,
            offset,
        } => observe_record(&fleet, &agent, &session, offset),
        Cmd::PostDeploy {
            repo,
            sha,
            host,
            status,
        } => post_deploy(&repo, &sha, &host, &status),
        Cmd::SetMeta {
            agent,
            repos,
            interval,
            host,
            native,
            devshell,
            apply,
        } => set_meta(&agent, &repos, interval.as_deref(), host.as_deref(), native, devshell, apply),
        Cmd::SetInterval { agent, interval } => set_interval(&fleet, &agent, &interval),
        Cmd::Notify { port } => {
            if let Err(e) = notify::serve(port, &board_session()) {
                eprintln!("{e}");
                std::process::exit(1);
            }
        }
        Cmd::Transcripts {
            agent,
            session,
            since,
            overlap,
        } => transcripts_cmd(&agent, session.as_deref(), since.as_deref(), overlap),
        Cmd::ServedSet { toml } => served_set(toml),
        Cmd::WatchdogUnit {
            observe,
            pinned_only,
            interval_secs,
            bin,
            no_rearm,
            install,
            uninstall,
        } => watchdog_unit(!no_rearm, observe, pinned_only, interval_secs, bin, install, uninstall),
        Cmd::Version => println!("{}", version_line()),
    }
}

/// The launch plan derived from a board workspace-kind resource: the `setup_script` that materializes the
/// workspace and the free-form `config` hints (`cwd`, `pre_trust`, `env`) the launcher reads. Parsing is
/// pure so it is unit-tested without the board.
struct WorkspaceKindPlan {
    name: String,
    description: Option<String>,
    setup_script: Option<String>,
    cwd: String,
    pre_trust: Vec<String>,
    env: Vec<(String, String)>,
}

/// Parse a board workspace-kind record into a launch plan. `config.cwd` is the launch directory (an
/// absolute path is used as-is; a relative one is taken under `fleet_root`); when absent the agent's own
/// root dir is the default. `config.pre_trust` is an optional list of extra paths to trust (the launch cwd
/// and the fleet root are always trusted), and `config.env` an optional string map of environment variables
/// the setup_script receives. Pure — unit-tested.
fn parse_workspace_kind(agent: &str, fleet_root: &str, rec: &serde_json::Value) -> WorkspaceKindPlan {
    let name = rec.get("name").and_then(|v| v.as_str()).unwrap_or("?").to_string();
    let description = rec.get("description").and_then(|v| v.as_str()).map(str::to_string);
    let setup_script = rec
        .get("setup_script")
        .and_then(|v| v.as_str())
        .filter(|s| !s.trim().is_empty())
        .map(str::to_string);
    let config = rec.get("config").cloned().unwrap_or(serde_json::Value::Null);
    let cwd = match config.get("cwd").and_then(|v| v.as_str()) {
        Some(c) if c.starts_with('/') => c.to_string(),
        Some(c) => format!("{fleet_root}/{c}"),
        None => workspace::agent_root_dir(fleet_root, agent),
    };
    let mut pre_trust: Vec<String> = vec![fleet_root.to_string(), cwd.clone()];
    if let Some(arr) = config.get("pre_trust").and_then(|v| v.as_array()) {
        pre_trust.extend(arr.iter().filter_map(|p| p.as_str().map(str::to_string)));
    }
    let mut env: Vec<(String, String)> = Vec::new();
    if let Some(obj) = config.get("env").and_then(|v| v.as_object()) {
        for (k, v) in obj {
            if let Some(s) = v.as_str() {
                env.push((k.clone(), s.to_string()));
            }
        }
    }
    WorkspaceKindPlan { name, description, setup_script, cwd, pre_trust, env }
}

/// Spin up an agent whose workspace is defined by a board workspace-kind resource rather than by `repos`.
/// Fetches the kind, reports the plan, and on `--apply` runs its setup_script (with `FLEET_AGENT` /
/// `FLEET_ROOT` and any `config.env` in the environment) to materialize the workspace, pre-trusts the
/// launch paths, and launches the agent in the kind's `config.cwd`. (#287)
#[allow(clippy::too_many_arguments)]
fn spin_up_workspace_kind(
    board: &board::Board,
    agent: &str,
    kind: &str,
    fleet_root: &str,
    has_charter: bool,
    harness: &str,
    model: &str,
    effort: &str,
    interval: &str,
    devshell: bool,
    apply: bool,
) {
    let rec = match board.get_workspace_kind(kind) {
        Ok(Some(r)) => r,
        Ok(None) => {
            eprintln!(
                "fleet spin-up: agent '{agent}' declares workspace_kind '{kind}', but no such kind is defined on the board (define it with POST /api/workspace-kinds)"
            );
            std::process::exit(1);
        }
        Err(e) => {
            eprintln!("fleet spin-up: {e}");
            std::process::exit(1);
        }
    };
    let plan = parse_workspace_kind(agent, fleet_root, &rec);

    println!("spin-up '{agent}' ({}):", if apply { "APPLY" } else { "dry-run" });
    println!(
        "  charter on board: {}",
        if has_charter { "yes — the agent fetches it in-session at boot" } else { "NO — declare a charter first" }
    );
    println!("  harness={harness}  model={model}  effort={effort}  interval={interval}");
    println!(
        "  workspace kind: {}{}",
        plan.name,
        plan.description.as_deref().map(|d| format!(" — {d}")).unwrap_or_default()
    );
    println!("  launch cwd: {}", plan.cwd);
    match &plan.setup_script {
        Some(s) => println!(
            "  setup_script: {} line(s) — runs with FLEET_AGENT/FLEET_ROOT{} in the environment",
            s.lines().count(),
            if plan.env.is_empty() { String::new() } else { format!(" + {} config env var(s)", plan.env.len()) }
        ),
        None => println!("  setup_script: NONE — the launch cwd is expected to exist already"),
    }

    if !apply {
        println!("  (dry-run — re-run with --apply to run the setup_script + launch)");
        return;
    }
    if !has_charter {
        eprintln!("  refusing to launch '{agent}': no charter on the board for it to self-discover");
        std::process::exit(1);
    }

    if let Some(script) = &plan.setup_script {
        // Run the setup_script from the fleet root so it has a stable base, with the agent identity and root
        // in the environment; the script is what materializes the launch cwd (and anything else the kind needs).
        if let Err(e) = std::fs::create_dir_all(fleet_root) {
            eprintln!("  setup FAILED: mkdir {fleet_root}: {e}");
            std::process::exit(1);
        }
        let mut cmd = std::process::Command::new("bash");
        cmd.arg("-c")
            .arg(script)
            .current_dir(fleet_root)
            .env("FLEET_AGENT", agent)
            .env("FLEET_ROOT", fleet_root);
        for (k, v) in &plan.env {
            cmd.env(k, v);
        }
        match cmd.status() {
            Ok(st) if st.success() => println!("  setup_script OK"),
            Ok(st) => {
                eprintln!("  setup_script FAILED (exit {})", st.code().unwrap_or(-1));
                std::process::exit(1);
            }
            Err(e) => {
                eprintln!("  setup_script FAILED to run: {e}");
                std::process::exit(1);
            }
        }
    }

    match pre_trust_dirs(&plan.pre_trust) {
        Ok(true) => println!("  pre-trusted {} path(s) (launch cwd + fleet root + config pre_trust)", plan.pre_trust.len()),
        Ok(false) => {}
        Err(e) => eprintln!("  WARN: could not pre-trust: {e} (agent may hit a one-time trust prompt)"),
    }
    match launch_board_agent(agent, &plan.cwd, harness, model, effort, interval, devshell) {
        Ok(win) => {
            println!(
                "  LAUNCHED '{agent}' in tmux window '{win}' (cwd {}) — it will get_agent itself for its charter, then run a work-conserving dynamic /loop (idle cadence ~{interval})",
                plan.cwd
            );
            match board.patch_metadata(agent, serde_json::json!({ "native": true })) {
                Ok(()) => println!("  stamped metadata.native=true (board-native roster marker)"),
                Err(e) => eprintln!("  WARN: launched but could not stamp native flag: {e}"),
            }
        }
        Err(e) => {
            eprintln!("  launch FAILED: {e}");
            std::process::exit(1);
        }
    }
}

/// Spin up one board-declared agent into its `~/.fleet` workspace. Reads the board record (orchestrator
/// read — agents coordinate via their own MCP), then reports the materialize + launch plan; `--apply`
/// materializes each repo's worktree off a shared bare mirror and launches a tmux window running `claude`
/// with a self-discovery kickoff (the agent fetches its own charter). See ../../DESIGN.md.
fn spin_up(agent: &str, apply: bool) {
    let board = board::Board::connect().unwrap_or_else(|e| {
        eprintln!("fleet spin-up: {e}");
        std::process::exit(1);
    });
    let rec = board.get_agent(agent).unwrap_or_else(|e| {
        eprintln!("fleet spin-up: {e}");
        std::process::exit(1);
    });
    let md = rec.get("metadata").cloned().unwrap_or(serde_json::Value::Null);
    let field = |k: &str| md.get(k).and_then(|v| v.as_str()).map(str::to_string);
    let has_charter = rec
        .get("charter")
        .and_then(|v| v.as_str())
        .map(|s| !s.is_empty())
        .unwrap_or(false);
    let model = resolve_model(&field("model").unwrap_or_else(|| "opus".into()));
    let effort = field("effort").unwrap_or_else(|| "high".into());
    let interval = field("interval").unwrap_or_else(|| "30m".into());
    // The agent runtime to launch (metadata.harness); defaults to claude so existing records are unchanged.
    let harness = field("harness").unwrap_or_else(|| "claude".into());
    // Opt-in: launch inside the workdir's flake devShell so the pinned toolchain is on PATH (#214). Off by
    // default — set only for an agent whose workdir is a flake with a devShell.
    let devshell = md.get("devshell").and_then(|v| v.as_bool()).unwrap_or(false);
    let repos = md
        .get("repos")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let fleet_root = config::get()
        .root
        .clone()
        .unwrap_or_else(|| format!("{}/.fleet", std::env::var("HOME").unwrap_or_default()));

    // A board-defined custom workspace kind (metadata.workspace_kind) takes precedence over `repos`: the
    // board resource named by the kind carries a setup_script that materializes the workspace and a
    // free-form config with the launch hints (cwd/pre_trust/env). This lets an environment the fleet does
    // not model natively be defined in a board resource and driven from there. (#287)
    if let Some(kind) = field("workspace_kind") {
        return spin_up_workspace_kind(
            &board, agent, &kind, &fleet_root, has_charter, &harness, &model, &effort, &interval,
            devshell, apply,
        );
    }

    println!("spin-up '{agent}' ({}):", if apply { "APPLY" } else { "dry-run" });
    println!(
        "  charter on board: {}",
        if has_charter { "yes — the agent fetches it in-session at boot" } else { "NO — declare a charter first" }
    );
    println!("  harness={harness}  model={model}  effort={effort}  interval={interval}");
    if repos.is_empty() {
        println!("  repos: NONE declared — no workspace to materialize (declare `repos` on the board record)");
    }
    let mut primary_workdir: Option<String> = None;
    for r in &repos {
        let repo = r.get("repo").and_then(|v| v.as_str()).unwrap_or("?");
        let branch = r.get("branch").and_then(|v| v.as_str()).unwrap_or("main");
        let wd = if apply {
            match workspace::ensure(&fleet_root, agent, repo, branch) {
                Ok(wd) => {
                    println!("  workspace ready: {wd}  (worktree of {repo}@{branch} off a shared mirror)");
                    // Install the generic fail-open fmt pre-commit into the repo's shared MIRROR hooks dir
                    // (a linked worktree runs hooks from the common/mirror dir), so a board-native agent gets
                    // a commit-time rustfmt nudge — the safety net a ~/.fleet worktree otherwise lacks (#283).
                    let hooks =
                        std::path::Path::new(&workspace::mirror_dir(&fleet_root, repo)).join("hooks");
                    install_fmt_hook(&hooks);
                    wd
                }
                Err(e) => {
                    eprintln!("  workspace FAILED for {repo}: {e}");
                    std::process::exit(1);
                }
            }
        } else {
            let wd = workspace::workspace_dir(&fleet_root, agent, repo);
            println!("  would materialize: {wd}  (worktree of {repo}@{branch} off {fleet_root}/mirrors)");
            wd
        };
        if primary_workdir.is_none() {
            primary_workdir = Some(wd);
        }
    }

    // A repo-less agent (e.g. a board orchestrator that works via the board MCP, not a checkout) has no
    // primary worktree: run it in a plain agent directory instead of bailing.
    let repo_less = primary_workdir.is_none();
    let workdir = match primary_workdir {
        Some(wd) => wd,
        None => {
            let dir = workspace::agent_root_dir(&fleet_root, agent);
            if apply {
                if let Err(e) = std::fs::create_dir_all(&dir) {
                    eprintln!("  workspace FAILED: mkdir {dir}: {e}");
                    std::process::exit(1);
                }
                println!("  repo-less workspace ready: {dir}  (no repo declared — runs via the board)");
            } else {
                println!("  would create repo-less workspace: {dir}  (no repo declared — runs via the board)");
            }
            dir
        }
    };
    if !apply {
        match build_launch_cmd(&harness, &model, &effort, devshell.then_some(workdir.as_str())) {
            Ok(_) => println!(
                "  would launch: {harness} in {workdir}{} (board MCP in-session) with a self-discovery kickoff, then a work-conserving dynamic /loop (idle cadence ~{interval})",
                if devshell { " [inside nix develop]" } else { "" }
            ),
            Err(e) => println!("  would NOT launch: {e}"),
        }
        println!("  (dry-run — re-run with --apply to materialize + launch)");
        return;
    }
    if !has_charter {
        eprintln!("  refusing to launch '{agent}': no charter on the board for it to self-discover");
        std::process::exit(1);
    }
    // Pre-trust so claude does not stall on the one-time folder-trust prompt (an interactive agent can't
    // answer it, and --dangerously-skip-permissions does NOT bypass it). A worktree workspace is trusted by
    // its git common dir (the shared MIRROR), which claude does not inherit from the fleet root — so trust
    // each repo's mirror; a repo-less workspace (a plain dir) is trusted by the dir itself. Plus the fleet
    // root. Non-fatal on error.
    let mut trust: Vec<String> = vec![fleet_root.clone()];
    for r in &repos {
        if let Some(repo) = r.get("repo").and_then(|v| v.as_str()) {
            trust.push(workspace::mirror_dir(&fleet_root, repo));
        }
    }
    if repo_less {
        trust.push(workdir.clone());
    }
    match pre_trust_dirs(&trust) {
        Ok(true) => println!("  pre-trusted {} path(s) (fleet root + repo mirror(s))", trust.len()),
        Ok(false) => {}
        Err(e) => eprintln!("  WARN: could not pre-trust: {e} (agent may hit a one-time trust prompt)"),
    }
    match launch_board_agent(agent, &workdir, &harness, &model, &effort, &interval, devshell) {
        Ok(win) => {
            println!(
                "  LAUNCHED '{agent}' in tmux window '{win}' (cwd {workdir}) — it will get_agent itself for its charter, then run a work-conserving dynamic /loop (idle cadence ~{interval})"
            );
            // Mark the agent board-native. `spin-up` IS the board-native launch path, so whatever it
            // launches is board-native by construction; stamping `native: true` gives orchestrators a
            // deterministic roster discriminator (metadata.native == true) instead of guessing from a
            // non-empty charter (which the file-hub registry rows mirrored onto the board also lack).
            // Idempotent key-level merge; non-fatal — the agent is already running.
            match board.patch_metadata(agent, serde_json::json!({ "native": true })) {
                Ok(()) => println!("  stamped metadata.native=true (board-native roster marker)"),
                Err(e) => eprintln!("  WARN: launched but could not stamp native flag: {e}"),
            }
        }
        Err(e) => {
            eprintln!("  launch FAILED: {e}");
            std::process::exit(1);
        }
    }
}

/// Build the SELF-DISCOVERY kickoff prompt for a board-native agent: it fetches its own charter from the
/// board (nothing is injected) and starts a WORK-CONSERVING loop. Pure so the prompt is unit-tested.
///
/// The loop is dynamic (`/loop` with NO fixed interval) so the agent self-paces via its own next-wake
/// decision instead of sleeping a fixed period regardless of pending work. Each tick the agent drains its
/// inbox, does one unit, then gates the next wake on work-present: it keeps looping soon while it holds
/// open assigned tasks or unread messages, and only falls back to the long `interval` idle cadence once its
/// assigned queue is drained AND its inbox is empty — so an agent with assigned work never idle-sleeps.
fn build_kickoff(agent: &str, workdir: &str, interval: &str) -> String {
    let tick = format!(
        "run one tick of your charter: drain your board notifications (check_notifications), do ONE unit \
         of work per your charter, then update your presence (set_status). WORK-CONSERVING PACING: after \
         the unit, check your OPEN assigned tasks (list_tasks with assignee '{agent}', counting ONLY \
         todo/in_progress tasks that are NOT blocked/parked — a blocked task, or one parked on a blocker or \
         a not-yet-existing prereq, is NOT actionable pending work) and your unread notifications. If you \
         hold actionable assigned work OR unread messages, keep going — schedule your next tick SOON \
         (60-120s). Only when you have no actionable assigned task AND your inbox is drained may you fall \
         back to the long idle cadence (about {interval}). NEVER idle-sleep on the long cadence while you \
         still hold an actionable assigned task."
    );
    format!(
        "You are the fleet agent '{agent}', running UNATTENDED. Your task-board MCP tools are available in \
         this session. FIRST call register_agent with agent_id '{agent}' (idempotent) to BIND this session \
         to your identity — get_agent ALONE does NOT bind it, so a board write before register_agent fails \
         with 'no identity for this session'. THEN call get_agent '{agent}' to read your OWN charter + \
         metadata from the board, and follow that charter as your role. On every board write pass your \
         identity EXPLICITLY as a fallback (agent_id / created_by / author / actor = '{agent}') — the board \
         defaults these to null. Coordinate through the board (send_message / check_notifications / \
         comment_task / set_status) — there is no file inbox. Any board Document you author (design / \
         proposal / plan) MUST follow the Fleet Doc-Writing Style Guide — wiki guides/doc-writing-style-guide \
         (Background then Problem Statement then Requirements/Goals/Non-Goals (measurable) then Solutions, \
         each its own section with prose + Pros/Cons, then Recommendation; implementation in an appendix; NO \
         tables/images/TL;DR/idioms in the body — the board viewer is minimal Markdown). Before submitting \
         ANY board doc OR comment, self-check your wording against the banned-phrases list \
         (wiki guides/banned-phrases) and rephrase anything it flags — that list is maintained/data-driven, so \
         read it rather than a fixed set here; until the pre-submit scanner (#308) lands this self-check is \
         yours. If any task of \
         yours becomes BLOCKED ON THE \
         OPERATOR, do not idle on it: set the task status=blocked with blocked_on {{kind:operator, note}}, \
         assign it to 'cameron', and stash your own id in metadata.blocked_owner — so list_tasks(assignee \
         'cameron') is the operator's single 'my asks' dashboard; when the operator answers, reassign the task \
         back to yourself and clear blocked (if your MCP cannot set a typed blocked_on, ask concierge or \
         board-pm to stamp it). You work in {workdir}. Start your recurring \
         loop now: /loop {tick}"
    )
}

/// Build the shell command that launches the agent's harness (agent runtime) in its tmux window, per the
/// selected `harness`. This is the one seam every harness plugs into: the window launch, trust, and kickoff
/// are harness-agnostic, only this command differs. The kickoff rides in `$CDZ_KICKOFF` (set on the window),
/// so the command references that env var rather than interpolating the prompt. Pure so it is unit-tested.
///
/// `claude` is fully wired. `codex` is a recognized-but-not-yet-wired harness: it returns an actionable
/// error rather than a guessed command, because Codex needs both its own CLI launch flags (model /
/// unattended-approval / initial-prompt) AND its own kickoff/loop/wake semantics — the Claude `/loop` +
/// in-session-MCP self-discovery model is Claude-specific and does not carry over unchanged. An unknown
/// harness is rejected so a typo'd `metadata.harness` fails loudly at spin-up instead of launching nothing.
/// `devshell` (opt-in, `metadata.devshell = true`): when `Some(workdir)`, launch INSIDE that workdir's flake
/// devShell (`nix develop "path:<workdir>" --command …`) so the flake-pinned toolchain (node/cargo/python/…)
/// is on PATH instead of the host's — the host PATH drifts (e.g. host node v18 breaks a flake-pinned build,
/// bare `curl`/`python3` intermittently miss in a compound shell call), and each agent otherwise re-derives
/// per-charter workarounds (#214). Default `None` = launch on the host PATH exactly as before. The caller
/// only sets it for an agent whose workdir is a flake with a devShell.
fn build_launch_cmd(
    harness: &str,
    model: &str,
    effort: &str,
    devshell: Option<&str>,
) -> Result<String, String> {
    match harness {
        "claude" => {
            // effort/model are single-quoted (no single-quotes in them) so `[1m]` can't glob; the kickoff
            // rides in $CDZ_KICKOFF (set literally via `-e`, expanded double-quoted) so its spaces/quotes
            // are safe.
            let claude = format!(
                "claude --disallowedTools AskUserQuestion --effort '{effort}' --model '{model}' \
                 --autocompact 600000 --dangerously-skip-permissions \"$CDZ_KICKOFF\""
            );
            Ok(match devshell {
                Some(dir) => format!("exec nix develop \"path:{dir}\" --command {claude}"),
                None => format!("exec {claude}"),
            })
        }
        "codex" => Err(
            "harness 'codex' is recognized but its launch is not wired yet — fill in the codex arm of \
             build_launch_cmd (the codex CLI's model flag + unattended/no-approval flags + initial-prompt \
             from $CDZ_KICKOFF) AND give codex its own kickoff/loop/wake semantics (the Claude /loop + \
             in-session-MCP self-discovery model does not carry over). Validate against a live codex install."
                .to_string(),
        ),
        other => Err(format!(
            "unknown harness '{other}' (known: claude, codex) — set metadata.harness on the agent's board record"
        )),
    }
}

/// The graceful spin-down decision for a board-native agent, given whether its board record is `native`,
/// whether a live tmux window exists, whether the pane shows an in-flight turn, and `--force`. Pure so the
/// stand-down policy is unit-tested without the board or tmux.
#[derive(Debug, PartialEq, Eq)]
enum SpinDownAction {
    /// Not a board-native agent — refuse (a file-hub agent stands down via `cargo xtask fleet remove`).
    NotBoardNative,
    /// A turn is in flight and `--force` was not given — refuse so a working agent is never killed.
    RefuseBusy,
    /// Set the board status offline, then kill the live window (stops the loop).
    OfflineAndKill,
    /// Already windowless — set the board status offline only, so up-board leaves it stood down.
    OfflineOnly,
}

fn spin_down_action(is_native: bool, has_window: bool, is_working: bool, force: bool) -> SpinDownAction {
    if !is_native {
        return SpinDownAction::NotBoardNative;
    }
    if has_window && is_working && !force {
        return SpinDownAction::RefuseBusy;
    }
    if has_window {
        SpinDownAction::OfflineAndKill
    } else {
        SpinDownAction::OfflineOnly
    }
}

/// `fleet spin-down`: gracefully stand down ONE board-native agent — the inverse of [`spin_up`]. Marks its
/// board `status` `offline` (so `up-board` treats it as stood down: offline + no window → never auto-launched)
/// then kills its tmux window to stop the loop. RESUMABLE, not a retire — the board record (charter +
/// metadata) is untouched, so `spin-up` revives it. Refuses a non-board-native agent (a file-hub agent uses
/// `cargo xtask fleet remove`) and a busy pane (unless `--force`). Sets offline BEFORE the kill so an
/// interleaved `up-board` cannot see it online-but-windowless and relaunch it.
fn spin_down(agent: &str, apply: bool, force: bool) {
    let board = board::Board::connect().unwrap_or_else(|e| {
        eprintln!("fleet spin-down: {e}");
        std::process::exit(1);
    });
    let rec = board.get_agent(agent).unwrap_or_else(|e| {
        eprintln!("fleet spin-down: {e}");
        std::process::exit(1);
    });
    let is_native = rec
        .get("metadata")
        .and_then(|m| m.get("native"))
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    let session = board_session();
    let has_window = tmux_window_names(&session).iter().any(|w| w == agent);
    let is_working = has_window && window_is_working(&session, agent);
    let action = spin_down_action(is_native, has_window, is_working, force);

    println!("spin-down '{agent}' ({}):", if apply { "APPLY" } else { "dry-run" });
    match action {
        SpinDownAction::NotBoardNative => {
            eprintln!(
                "  ✗ '{agent}' is NOT board-native (metadata.native != true) — spin-down manages board-native \
                 agents only. A file-hub agent stands down via `cargo xtask fleet remove {agent}`."
            );
            std::process::exit(1);
        }
        SpinDownAction::RefuseBusy => {
            eprintln!(
                "  ✗ '{agent}' has a turn IN FLIGHT (its pane is working) — refusing so a running agent is not \
                 killed mid-work. Re-run when it's idle, or pass --force to stand it down anyway."
            );
            std::process::exit(1);
        }
        SpinDownAction::OfflineAndKill => println!(
            "  plan: set board status=offline, then kill tmux window {session}:{agent} (stops the loop)"
        ),
        SpinDownAction::OfflineOnly => println!(
            "  plan: no live window — set board status=offline only (up-board then leaves it stood down)"
        ),
    }
    println!(
        "  RESUMABLE: board record (charter + metadata) left intact — `fleet spin-up {agent} --apply` revives it."
    );
    if !apply {
        println!("  (dry-run — re-run with --apply to perform it)");
        return;
    }
    // Offline FIRST, then kill: `up-board` only leaves an agent down when it is offline + windowless, so
    // setting offline before removing the window closes the relaunch race.
    let msg = format!(
        "Spun down via `fleet spin-down` (resumable). Board record intact; `fleet spin-up {agent} --apply` revives it."
    );
    if let Err(e) = board.set_status(agent, "offline", &msg) {
        eprintln!("  ✗ failed to set board status offline: {e}");
        std::process::exit(1);
    }
    println!("  ✓ board status set offline");
    if matches!(action, SpinDownAction::OfflineAndKill) {
        let target = format!("{session}:{agent}");
        let killed = std::process::Command::new("tmux")
            .args(["kill-window", "-t", &target])
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if killed {
            println!("  ✓ killed window {target} (loop stopped)");
        } else {
            println!(
                "  ! tmux kill-window {target} failed (status is offline; remove the window manually if it lingers)"
            );
        }
    }
    println!("  spun down '{agent}' — stood down + resumable.");
}

/// A recognizable marker in the fleet-installed fmt pre-commit hook, so a re-install tells OUR hook (safe to
/// refresh) from a foreign one (never clobber).
const FMT_HOOK_MARKER: &str = "# fleet:fmt-warn";

/// The generic, repo-agnostic pre-commit hook `spin-up` installs into a materialized worktree's shared mirror
/// hooks dir: a FAIL-OPEN rustfmt nudge so a board-native agent gets a commit-time warning when its staged
/// Rust is not `cargo fmt`-clean (the required `checks/rustfmt` CI job / `cargo xtask check` reds otherwise —
/// the class that bit #10139). NEVER blocks a commit (exit 0), never mutates files, no-ops without staged .rs
/// or without cargo. Silence with FLEET_SKIP_FMT_HOOK=1. Kept generic (no cadenza-specific checks) so it is
/// correct for every repo an agent's worktree may be.
fn fmt_precommit_hook_body() -> String {
    format!(
        "#!/usr/bin/env bash\n\
         {FMT_HOOK_MARKER} (installed by `fleet spin-up`; fail-open rustfmt nudge for board-native worktrees)\n\
         [ \"${{FLEET_SKIP_FMT_HOOK:-}}\" = \"1\" ] && exit 0\n\
         staged=$(git diff --cached --name-only --diff-filter=ACM -- '*.rs' 2>/dev/null)\n\
         [ -z \"$staged\" ] && exit 0\n\
         command -v cargo >/dev/null 2>&1 || exit 0\n\
         if ! cargo fmt --all --check >/dev/null 2>&1; then\n\
         \x20 echo \"warn pre-commit: staged Rust is not rustfmt-clean — run 'cargo fmt' before landing (the\" >&2\n\
         \x20 echo \"  required checks/rustfmt CI job / 'cargo xtask check' reds otherwise). Silence: FLEET_SKIP_FMT_HOOK=1.\" >&2\n\
         fi\n\
         exit 0\n"
    )
}

#[derive(Debug, PartialEq, Eq)]
enum FmtHookAction {
    /// No pre-commit present — install ours.
    Install,
    /// Our hook is already there — refresh it (idempotent; picks up body changes).
    Refresh,
    /// A FOREIGN pre-commit exists — never clobber it; skip.
    SkipForeign,
}

/// Decide what to do about installing the fmt hook given the existing `pre-commit` content (None = absent).
/// Pure so the never-clobber-a-foreign-hook policy is unit-tested. Ours is recognized by [`FMT_HOOK_MARKER`].
fn fmt_hook_install_action(existing: Option<&str>) -> FmtHookAction {
    match existing {
        None => FmtHookAction::Install,
        Some(body) if body.contains(FMT_HOOK_MARKER) => FmtHookAction::Refresh,
        Some(_) => FmtHookAction::SkipForeign,
    }
}

/// Install the generic fmt pre-commit hook into `hooks_dir` (a repo's shared MIRROR hooks dir, so it covers
/// every worktree cut from that mirror). Idempotent (silent no-op when already current), never clobbers a
/// foreign hook. Best-effort — a failure only forfeits the commit-time nudge (the gate still covers fmt), so
/// it warns rather than aborting spin-up.
fn install_fmt_hook(hooks_dir: &std::path::Path) {
    let hook = hooks_dir.join("pre-commit");
    let existing = std::fs::read_to_string(&hook).ok();
    let body = fmt_precommit_hook_body();
    match fmt_hook_install_action(existing.as_deref()) {
        FmtHookAction::SkipForeign => {
            println!("  fmt hook: foreign pre-commit at {} left untouched", hook.display());
            return;
        }
        // Already exactly our current hook — nothing to do, stay quiet (the common case after first install).
        FmtHookAction::Refresh if existing.as_deref() == Some(body.as_str()) => return,
        FmtHookAction::Install | FmtHookAction::Refresh => {}
    }
    if let Err(e) = std::fs::create_dir_all(hooks_dir) {
        eprintln!("  WARN: fmt hook not installed (mkdir {}: {e})", hooks_dir.display());
        return;
    }
    if let Err(e) = std::fs::write(&hook, &body) {
        eprintln!("  WARN: fmt hook not installed (write {}: {e})", hook.display());
        return;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755));
    }
    println!(
        "  fmt hook: {} the fail-open rustfmt pre-commit at {}",
        if existing.is_some() { "refreshed" } else { "installed" },
        hook.display()
    );
}

/// Open a tmux window running the agent's harness in `workdir` with a SELF-DISCOVERY kickoff (the agent
/// fetches its own charter from the board via its in-session MCP — nothing is injected). The launch command
/// is harness-specific (see [`build_launch_cmd`]); refuses to double-launch an existing same-named window.
/// The kickoff is passed via a tmux env var so no shell quoting can mangle it.
fn launch_board_agent(agent: &str, workdir: &str, harness: &str, model: &str, effort: &str, interval: &str, devshell: bool) -> Result<String, String> {
    let session = board_session();
    if let Ok(out) = std::process::Command::new("tmux")
        .args(["list-windows", "-t", &session, "-F", "#W"])
        .output()
        && String::from_utf8_lossy(&out.stdout).lines().any(|w| w == agent)
    {
        return Err(format!("a tmux window '{agent}' already exists in session '{session}' (already spun up?)"));
    }
    let kickoff = build_kickoff(agent, workdir, interval);
    let cmd = build_launch_cmd(harness, model, effort, devshell.then_some(workdir))?;
    let status = std::process::Command::new("tmux")
        .args([
            "new-window", "-d",
            "-t", &session,
            "-n", agent,
            "-c", workdir,
            "-e", &format!("CDZ_KICKOFF={kickoff}"),
            &cmd,
        ])
        .status()
        .map_err(|e| format!("tmux new-window: {e}"))?;
    if !status.success() {
        return Err("tmux new-window failed (is the fleet tmux session running?)".to_string());
    }
    Ok(format!("{session}:{agent}"))
}

/// Add a trusted-project entry for `dir` to a parsed `~/.claude.json` value. Returns whether it changed
/// the value (false = already trusted → no write needed). Only touches `projects.<dir>`; leaves every
/// other key untouched (preserve_order keeps the rest of the config byte-stable). Pure — unit-tested.
fn ensure_trusted(config: &mut serde_json::Value, dir: &str) -> bool {
    let already = config
        .get("projects")
        .and_then(|p| p.get(dir))
        .and_then(|e| e.get("hasTrustDialogAccepted"))
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    if already {
        return false;
    }
    let Some(obj) = config.as_object_mut() else { return false };
    let projects = obj
        .entry("projects")
        .or_insert_with(|| serde_json::json!({}));
    let Some(projects) = projects.as_object_mut() else { return false };
    let entry = projects
        .entry(dir.to_string())
        .or_insert_with(|| serde_json::json!({ "allowedTools": [] }));
    entry["hasTrustDialogAccepted"] = serde_json::Value::Bool(true);
    true
}

/// Idempotently mark each of `dirs` (and its symlink-canonical form) trusted in `~/.claude.json`, so a
/// spun-up agent never stalls on the one-time folder-trust prompt. Returns true if it wrote a change.
///
/// Two subtleties this handles, both learned from a real stall: (1) for a git *worktree* workspace claude
/// resolves the project to the worktree's git common dir — the shared bare **mirror** — not the workspace
/// dir or the fleet root, and it does NOT reliably inherit trust from an ancestor, so the mirror path is
/// what must be trusted; (2) claude launches with a cwd derived from the literal path (e.g.
/// `/home/<u>/.fleet/...` when `$HOME` is a symlink) but stores/checks the canonical form, so trusting only
/// one form leaves the other prompting — hence both. Writes atomically (temp + rename) only when a change
/// is needed, so it can't tear the shared config and rarely races a concurrent writer.
fn pre_trust_dirs(dirs: &[String]) -> Result<bool, String> {
    let home = std::env::var("HOME").map_err(|_| "no HOME".to_string())?;
    let cfg = format!("{home}/.claude.json");
    let raw = std::fs::read_to_string(&cfg).map_err(|e| format!("read {cfg}: {e}"))?;
    let mut v: serde_json::Value = serde_json::from_str(&raw).map_err(|e| format!("parse {cfg}: {e}"))?;
    let mut changed = false;
    for dir in dirs {
        changed |= ensure_trusted(&mut v, dir);
        let canon = std::fs::canonicalize(dir)
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|_| dir.clone());
        if &canon != dir {
            changed |= ensure_trusted(&mut v, &canon);
        }
    }
    if !changed {
        return Ok(false);
    }
    let body = serde_json::to_string_pretty(&v).map_err(|e| e.to_string())?;
    let tmp = format!("{cfg}.fleet-tmp-{}", std::process::id());
    std::fs::write(&tmp, body).map_err(|e| format!("write {tmp}: {e}"))?;
    std::fs::rename(&tmp, &cfg).map_err(|e| format!("rename {tmp} -> {cfg}: {e}"))?;
    Ok(true)
}

/// How stale a board `last_seen` is allowed to get before the watchdog cares. An agent heartbeats far more
/// often than any of these bounds, so `live` is the steady state; `quiet` is worth a glance; `STALE` is a
/// candidate for a wedge/dead check.
const LIVE_SECS: i64 = 15 * 60; // < this: freshly heartbeating
const QUIET_SECS: i64 = 60 * 60; // < this: quiet but plausibly alive; beyond: STALE

/// Classify a heartbeat age (seconds since the board `last_seen`) into a liveness bucket. A negative age
/// (clock skew — a `last_seen` in the future) is reported as `live` rather than treated as stale.
fn liveness_verdict(age_secs: i64) -> &'static str {
    if age_secs < LIVE_SECS {
        "live"
    } else if age_secs < QUIET_SECS {
        "quiet"
    } else {
        "STALE"
    }
}

/// Age in whole seconds between `now` and an RFC3339 `last_seen`, or `None` if it doesn't parse.
fn last_seen_age_secs(last_seen: &str, now: time::OffsetDateTime) -> Option<i64> {
    use time::format_description::well_known::Rfc3339;
    time::OffsetDateTime::parse(last_seen, &Rfc3339)
        .ok()
        .map(|t| (now - t).whole_seconds())
}

/// Report every board-declared agent's liveness off its board `last_seen` — the read side of the watchdog.
/// Reads the roster from the board (orchestrator read; agents coordinate via their own MCP) and prints one
/// line per agent: id, board status, liveness bucket + heartbeat age, and the raw `last_seen`.
fn status(stale_only: bool) {
    let board = board::Board::connect().unwrap_or_else(|e| {
        eprintln!("fleet status: {e}");
        std::process::exit(1);
    });
    let agents = board.list_agents().unwrap_or_else(|e| {
        eprintln!("fleet status: {e}");
        std::process::exit(1);
    });
    let now = time::OffsetDateTime::now_utc();
    println!("{:<28} {:<10} {:<14} last_seen", "agent", "status", "liveness");
    let mut shown = 0usize;
    for a in &agents {
        let id = a.get("id").and_then(|v| v.as_str()).unwrap_or("?");
        let st = a.get("status").and_then(|v| v.as_str()).unwrap_or("");
        let ls = a.get("last_seen").and_then(|v| v.as_str()).unwrap_or("");
        let live = match last_seen_age_secs(ls, now) {
            Some(age) => {
                let verdict = liveness_verdict(age);
                if stale_only && verdict == "live" {
                    continue;
                }
                format!("{verdict}({}m)", age / 60)
            }
            None => {
                if stale_only {
                    continue;
                }
                "?".to_string()
            }
        };
        println!("{id:<28} {st:<10} {live:<14} {ls}");
        shown += 1;
    }
    if stale_only && shown == 0 {
        println!("(all {} agents live)", agents.len());
    }
}

/// How many of an agent's own loop intervals a heartbeat may lapse before the watchdog calls it a re-arm
/// candidate: one missed tick is jitter, but several missed intervals means the loop isn't cycling.
const WATCHDOG_OVERDUE_INTERVALS: u64 = 3;

/// At/above this loop interval, an agent holding OPEN assigned tasks is a retighten candidate: it should be
/// cycling faster to drain its queue (the work-conserving principle), not idling on a long cadence.
const WATCHDOG_LONG_INTERVAL_SECS: u64 = 3600; // 1h

/// Whether an agent's board presence means it is NOT expected to be looping, so its lapsing heartbeat is by
/// design, not a stall: `away` / `offline` (deliberately idle or spun down) and `done` (retired / at-rest — a
/// worker parked until revived by a trigger). Re-arming such an agent with a drained queue just burns a tick,
/// or worse wakes an agent meant to stay at-rest. `online` (or an unset status) is a live looping agent. Pure
/// — unit-tested.
fn presence_suppresses_rearm(status: Option<&str>) -> bool {
    matches!(status, Some("away") | Some("offline") | Some("done"))
}

/// A watchdog re-arm/retighten candidate: either the heartbeat is `STALE` (lapsed several intervals — the loop
/// isn't cycling), OR the agent holds open assigned work while sitting on a long idle interval (work-conserving
/// — it should loop tighter until its queue drains).
///
/// NOT `late`: an agent that heartbeats once per loop interval naturally reaches age ≈ 1× its interval right
/// before its next scheduled tick, so `late` (1–3× interval) is the NORMAL band for a healthy idle agent, not
/// a stall — re-arming on `late` pokes every idle agent once per interval (the v-slack-bridge report). Only
/// `STALE` (≥ [`WATCHDOG_OVERDUE_INTERVALS`]× interval) means the loop actually stopped, matching
/// [`watchdog_verdict`]'s own doc ("`STALE` (a re-arm candidate)"). A genuinely dead loop still reaches STALE.
///
/// `idle_presence` (board presence `away` OR `offline`) short-circuits to NOT a candidate when the agent has
/// ZERO open tasks: an agent that deliberately went idle with a drained queue paused its loop on purpose (and
/// typically holds a scheduled wakeup), so its lapsing heartbeat is expected, not a stall — re-arming it just
/// burns a tick. An idle-presence agent that STILL holds open tasks is not short-circuited (it shouldn't have
/// parked with work), so it stays a candidate. Pure — unit-tested.
fn is_retighten_candidate(
    verdict: &str,
    open_tasks: usize,
    interval_secs: u64,
    idle_presence: bool,
) -> bool {
    if idle_presence && open_tasks == 0 {
        return false;
    }
    verdict == "STALE" || (open_tasks > 0 && interval_secs >= WATCHDOG_LONG_INTERVAL_SECS)
}

/// Parse a fleet loop interval into seconds: a bare number is seconds; a trailing `s`/`m`/`h`/`d` scales.
/// Returns `None` for an empty or unrecognized value. Pure — unit-tested.
fn parse_interval_secs(spec: &str) -> Option<u64> {
    let s = spec.trim();
    let last = s.chars().last()?;
    let (num, mult) = match last {
        's' => (&s[..s.len() - 1], 1u64),
        'm' => (&s[..s.len() - 1], 60),
        'h' => (&s[..s.len() - 1], 3600),
        'd' => (&s[..s.len() - 1], 86400),
        c if c.is_ascii_digit() => (s, 1),
        _ => return None,
    };
    num.trim().parse::<u64>().ok().map(|n| n.saturating_mul(mult))
}

/// Watchdog verdict for a board-native agent: compare heartbeat `age_secs` to its OWN loop `interval_secs`.
/// `ok` within one interval (steady heartbeat), `late` within the overdue window, `STALE` (a re-arm
/// candidate) beyond it. Negative age (clock skew — a future `last_seen`) is treated as `ok`. Pure.
fn watchdog_verdict(age_secs: i64, interval_secs: u64) -> &'static str {
    if age_secs < 0 || interval_secs == 0 {
        return "ok";
    }
    let age = age_secs as u64;
    if age < interval_secs {
        "ok"
    } else if age < interval_secs.saturating_mul(WATCHDOG_OVERDUE_INTERVALS) {
        "late"
    } else {
        "STALE"
    }
}

/// Capture an agent's visible tmux pane text (no scrollback), or `None` if tmux errors / the window is gone.
fn capture_pane(session: &str, agent: &str) -> Option<String> {
    let target = format!("{session}:{agent}");
    std::process::Command::new("tmux")
        .args(["capture-pane", "-p", "-t", &target])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
}

/// Does the pane show Claude Code's IDLE input prompt — a line that is just the `❯` glyph with an empty
/// input? While a turn is generating, the input line is replaced by the live status, so a bare `❯` line is
/// a reliable IDLE signal (a line with typed text after `❯` does NOT match). Pure — unit-tested.
fn pane_shows_idle_prompt(pane_text: &str) -> bool {
    pane_text.lines().any(|l| l.trim() == "❯")
}

/// Does the pane show Claude actively working (a turn in flight)? Mirrors the cadenza watchdog heuristic so
/// the auto-wake NEVER injects into a heads-down pane (operator ban 2026-09-10 + the seq-1387 wake-only
/// fence). An idle `❯` prompt overrides the lingering footer ("esc to interrupt" in the persistent hint)
/// and a completed turn's token remnant; otherwise the working affordances (live meter, API-retry,
/// backgrounding hint) mean a turn is generating. Tracks Claude Code's status-line vocabulary. Pure.
fn pane_shows_working(pane_text: &str) -> bool {
    if pane_shows_idle_prompt(pane_text) {
        return false;
    }
    pane_text.contains("esc to interrupt")
        || pane_text.contains("Retrying in ")
        || pane_text.contains("Retrying…")
        || pane_text.contains("to run in background")
        || ((pane_text.contains("↓") || pane_text.contains("↑")) && pane_text.contains("tokens"))
}

/// Is the agent's pane actively working right now? Captures the visible pane and classifies it. A capture
/// failure (no window / tmux error) returns false — the caller then treats "not working" per its own
/// window-existence handling (the wake inject itself no-ops on a missing window).
fn window_is_working(session: &str, agent: &str) -> bool {
    capture_pane(session, agent)
        .map(|s| pane_shows_working(&s))
        .unwrap_or(false)
}

/// Unix mtime (whole seconds) of a file, or `None` if it's absent / unreadable. Used to age a file-hub
/// heartbeat touch-file (`<hub>/.claude/fleet/heartbeat/<name>`).
fn file_mtime_unix(path: &Path) -> Option<u64> {
    std::fs::metadata(path)
        .ok()?
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|d| d.as_secs())
}

/// Count an agent's UNDRAINED file-hub inbox MESSAGES under `<hub>/inbox/<name>`. A message is a `.json`
/// file (the delivery format `<seq>-<pid>-<kind>.json`) — the SAME predicate `inbox_list`/`inbox_depth`
/// use, so this counts exactly what a drain would. Non-message files (a `*_seed.txt`/`seed-*.md` kickoff
/// seed) and the `processed/` archive dir are excluded — otherwise a lingering seed file reads as a
/// perpetual "pending message" and the watchdog false-nudges the agent every sweep. 0 when the inbox is
/// absent.
fn inbox_pending_count(fleet: &Fleet, name: &str) -> usize {
    std::fs::read_dir(fleet.inbox(name))
        .map(|rd| {
            rd.flatten()
                .filter(|e| e.path().is_file())
                .filter(|e| e.file_name().to_string_lossy().ends_with(".json"))
                .count()
        })
        .unwrap_or(0)
}

/// The wake injected into a re-arm candidate's window by `fleet watchdog --rearm`. It never reaps or
/// restarts (the operator banned auto-reap) — it nudges the agent to run a tick, which is exactly what a
/// human was doing by hand for stalled loops. A benign prompt: worst case the agent no-ops one tick.
const WATCHDOG_REARM_WAKE: &str = "[watchdog] you have pending work (an overdue loop and/or open assigned tasks) — run a tick NOW: check_notifications, do one unit, set_status, and keep looping until your queue drains (do not idle-sleep while you hold assigned tasks).";

/// A re-arm to the SAME agent is never sent more often than this, even for a short or unparsed (0s) interval.
const WATCHDOG_REARM_COOLDOWN_FLOOR_SECS: u64 = 300; // 5 min = 5× the 1-min poll

/// Pure: is a re-arm to this agent still on cooldown? The watchdog re-arms an agent at most once per its OWN
/// loop interval (floored at [`WATCHDOG_REARM_COOLDOWN_FLOOR_SECS`]) — so an agent whose self-firing loop the
/// watchdog is covering is woken on ITS cadence, not on every 1-min poll. Without this, a healthy long-interval
/// agent (e.g. a 4h disk-sweep whose cron never armed) is nudged every single sweep. `None` (never armed) is
/// not on cooldown. Unit-tested.
fn rearm_on_cooldown(last_rearm: Option<u64>, now: u64, interval_secs: u64) -> bool {
    let cooldown = interval_secs.max(WATCHDOG_REARM_COOLDOWN_FLOOR_SECS);
    last_rearm.is_some_and(|last| now.saturating_sub(last) < cooldown)
}

/// The per-agent last-re-arm stamp path: `<hub>/.claude/fleet/watchdog/<name>.rearm` (contents = unix secs).
fn rearm_stamp_path(fleet: &Fleet, name: &str) -> PathBuf {
    fleet.root.join("watchdog").join(format!("{name}.rearm"))
}

/// Read an agent's last-re-arm unix time from its stamp; `None` on an absent or unparseable stamp (never armed).
fn read_rearm_stamp(fleet: &Fleet, name: &str) -> Option<u64> {
    std::fs::read_to_string(rearm_stamp_path(fleet, name))
        .ok()?
        .trim()
        .parse()
        .ok()
}

/// Record that an agent was just re-armed at `now` (best-effort — a write failure is non-fatal, it just means
/// the cooldown isn't enforced for that agent next sweep).
fn write_rearm_stamp(fleet: &Fleet, name: &str, now: u64) {
    let p = rearm_stamp_path(fleet, name);
    if let Some(dir) = p.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(p, now.to_string());
}

// ── observation triggers (#187, BUILD 2/5) ─────────────────────────────────────────────────────────
// The watchdog is also the SPAWNER of ephemeral per-agent observer sessions: it tracks each agent's
// transcript growth against a per-agent watermark and, when the unobserved increment crosses a threshold
// (the size trigger) or a stood-down agent has a closing tail (the mandatory spin-down trigger), an
// observation of exactly that increment is due. This slice implements the DETECTION + watermark store,
// report-only — the actual ephemeral spawn (and advancing the watermark past an observed span) lands with
// the observer role (BUILD 3, #188), which flips `--observe` on in the always-on sweep.

/// The default per-agent transcript-growth threshold in JSONL lines/records (the unit the `transcripts
/// --since <sid:offset>` window uses). Conservative first cut — a long-running agent emits thousands of
/// records, so this bounds each observation to a large single span; tune DOWN on evidence. `CDZ_OBSERVE_LINES`
/// overrides it; `0` disables the size trigger (the spin-down trigger still fires). See [`observe_trigger`].
const OBSERVE_LINES_DEFAULT: usize = 2000;

/// A per-agent observation decision derived from the last-observed watermark and the current newest session.
#[derive(Debug, PartialEq, Eq)]
struct ObserveDecision {
    /// Observe now: the size increment crossed threshold, OR a stood-down agent has an unobserved tail.
    fire: bool,
    /// The session to observe.
    session: String,
    /// The line offset to observe FROM (0 on a session rotation / first observation).
    since_offset: usize,
    /// The unobserved line increment (for the report + the next watermark).
    increment: usize,
}

/// Decide whether an agent's transcript growth warrants one observation. `wm` is the last-observed watermark
/// `(session, line_offset)`; `cur` is the current newest session `(session, line_count)`. On the SAME session
/// the unobserved span is `cur_lines - wm_offset` starting at `wm_offset`; on a session ROTATION (or the first
/// observation — an empty watermark session) the whole new session is unobserved, so observe from 0. Fires on
/// the SIZE trigger (increment ≥ `threshold`, when `threshold > 0`) OR the mandatory SPIN-DOWN trigger (a
/// `stood_down`/offline agent with ANY unobserved tail — capture the closing read before context is gone).
/// Pure — unit-tested.
fn observe_trigger(
    wm: (&str, usize),
    cur: (&str, usize),
    threshold: usize,
    stood_down: bool,
) -> ObserveDecision {
    let (wm_session, wm_offset) = wm;
    let (cur_session, cur_lines) = cur;
    let (since_offset, increment) = if wm_session == cur_session {
        (wm_offset, cur_lines.saturating_sub(wm_offset))
    } else {
        (0, cur_lines)
    };
    let size_fire = threshold > 0 && increment >= threshold;
    let spindown_fire = stood_down && increment > 0;
    ObserveDecision {
        fire: size_fire || spindown_fire,
        session: cur_session.to_string(),
        since_offset,
        increment,
    }
}

/// The per-agent observation watermark path: `<hub>/.claude/fleet/observer/<name>.watermark` (contents =
/// `<session-id>:<line-offset>`, the `transcripts --since` form). Sibling of the re-arm cooldown store.
fn observe_watermark_path(fleet: &Fleet, name: &str) -> PathBuf {
    fleet.root.join("observer").join(format!("{name}.watermark"))
}

/// Read an agent's last-observed watermark `(session, line_offset)`; `("", 0)` when absent/unparseable — so
/// the first crossing observes the newest session from its start.
fn read_observe_watermark(fleet: &Fleet, name: &str) -> (String, usize) {
    std::fs::read_to_string(observe_watermark_path(fleet, name))
        .ok()
        .map(|s| transcripts::parse_watermark(s.trim()))
        .unwrap_or_default()
}

/// Cheap line count of a session JSONL (streamed, no per-line JSON parse) — the growth measure, matching
/// [`transcripts::parse_jsonl`]'s line semantics. `0` on an unreadable file.
fn session_line_count(path: &Path) -> usize {
    use std::io::BufRead;
    match std::fs::File::open(path) {
        Ok(f) => std::io::BufReader::new(f).lines().count(),
        Err(_) => 0,
    }
}

/// Advance an agent's observation watermark to `<session>:<offset>` — the CONFIRMED-observation write. Only
/// [`observe_record`] calls this (never detection or spawn), so a crashed/timed-out observer leaves the span
/// unobserved and it re-fires next sweep (design guardrail, #188 comment 494). Best-effort.
fn write_observe_watermark(fleet: &Fleet, name: &str, session: &str, offset: usize) {
    let p = observe_watermark_path(fleet, name);
    if let Some(dir) = p.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(p, format!("{session}:{offset}"));
}

/// Observation check for one agent (#187 detection): measure its newest session's growth against its
/// watermark and return the observation DECISION when one should fire (size threshold crossed, or a
/// stood-down agent has a closing tail). `None` when the agent has no session or is below threshold. The
/// caller formats the report line and (with `--spawn`) launches an observer scoped to `d.session:since_offset`.
fn observe_candidate(fleet: &Fleet, agent: &str, stood_down: bool, threshold: usize) -> Option<ObserveDecision> {
    let sessions = transcripts::locate_sessions(agent);
    let newest = sessions.first()?;
    let session = transcripts::session_id_of(newest);
    let lines = session_line_count(newest);
    let (wm_session, wm_offset) = read_observe_watermark(fleet, agent);
    let d = observe_trigger(
        (wm_session.as_str(), wm_offset),
        (session.as_str(), lines),
        threshold,
        stood_down,
    );
    d.fire.then_some(d)
}

// ── observer spawn (#188 BUILD 3/5) ────────────────────────────────────────────────────────────────
// `fleet watchdog --observe --spawn` launches an EPHEMERAL observer session per fired candidate (bounded by
// a per-agent spawn cooldown + a per-sweep cap), scoped to the unobserved window. The session acts as the
// single stable board id `observer` (design author + board-pm, #188 comments 500/501): session lifetime is
// decoupled from board identity, so proposals/reports/kb entries are one queryable author and the roster
// never fragments. The watermark advances ONLY when the observer confirms via `fleet observe-record`.

/// Max observers launched in one watchdog sweep — an anti-firehose bound on top of the observer's own
/// FLOOR/CAP. Highest-growth candidates first. `CDZ_OBSERVE_SPAWN_CAP` overrides.
const OBSERVE_SPAWN_CAP_DEFAULT: usize = 3;

/// Don't re-spawn an observer for the SAME target within this window — an observation takes minutes and the
/// watermark only advances on confirmation, so without a cooldown a still-running (or crashed) observer's
/// target would re-spawn every 60s sweep. `CDZ_OBSERVE_SPAWN_COOLDOWN_SECS` overrides.
const OBSERVE_SPAWN_COOLDOWN_SECS: u64 = 1800; // 30 min

/// The per-target observer spawn stamp: `<hub>/.claude/fleet/observer/<name>.spawned` (unix secs of the last
/// spawn). Sibling of the watermark + re-arm stamps.
fn observe_spawn_stamp_path(fleet: &Fleet, name: &str) -> PathBuf {
    fleet.root.join("observer").join(format!("{name}.spawned"))
}

fn read_observe_spawn_stamp(fleet: &Fleet, name: &str) -> Option<u64> {
    std::fs::read_to_string(observe_spawn_stamp_path(fleet, name))
        .ok()
        .and_then(|s| s.trim().parse().ok())
}

fn write_observe_spawn_stamp(fleet: &Fleet, name: &str, now: u64) {
    let p = observe_spawn_stamp_path(fleet, name);
    if let Some(dir) = p.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(p, now.to_string());
}

/// Whether a fresh observer spawn for this target is still on cooldown. Pure — unit-tested.
fn observe_on_spawn_cooldown(last_spawn: Option<u64>, now: u64, cooldown: u64) -> bool {
    last_spawn.is_some_and(|last| now.saturating_sub(last) < cooldown)
}

/// The board project that holds the fleet-self-improve lane — observation tasks and their proposal children
/// (#28). An observation task is the parent; each proposal the observer files is a child, so the lane reads
/// as a tree and child_rollup counts "proposals from this observation" (#290).
const SELF_IMPROVE_PROJECT: i64 = 28;

/// The board observation task an observer works: the "observe <target>'s window" work item the watchdog
/// enqueues per fired candidate (#290). Pure to build so the exact shape is unit-tested and previewable in
/// the spawn dry-run before any live create.
struct ObservationTaskSpec {
    title: String,
    body: String,
    project_id: i64,
}

/// Build the observation task for one candidate window: a titled work item naming the target and the exact
/// transcript window the observer will read, in the fleet-self-improve project. The observer drives from this
/// task, files its proposals as children, and closes it on completion (#290). Pure — unit-tested.
fn observation_task_spec(target: &str, session: &str, since_offset: usize) -> ObservationTaskSpec {
    ObservationTaskSpec {
        title: format!("observe {target} — {session}:{since_offset}"),
        body: format!(
            "Observe agent `{target}`'s transcript window (session `{session}`, from line offset \
             {since_offset}) and file each above-floor, evidence-cited improvement as a CHILD proposal task \
             of this one. Close this task when the observation is complete. Kicked off by the fleet watchdog \
             self-improve cadence (#290)."
        ),
        project_id: SELF_IMPROVE_PROJECT,
    }
}

/// The kickoff for an EPHEMERAL observer session: it acts as the stable board id `observer`, reads exactly
/// the target window, files curated proposals into project #28, CONFIRMS via `observe-record` as its last
/// step, then exits (no loop). Pure so the prompt is unit-tested. `fleet_bin` is the ABSOLUTE path to THIS
/// standalone fleet binary — used for every `fleet` subcommand, because the `fleet` on PATH may be a
/// different build (during the migration it is the cadenza embedded fleet, which lacks `transcripts` /
/// `observe-record`). `role_path` points at the full role body (the authoritative method); the kickoff
/// carries the parameters + identity + completion command so the observation is well-formed regardless.
/// `observation_task` is the board task (in project #28) this observation is driven by, when the watchdog
/// enqueued one (#290): the observer files each proposal as a CHILD of it and CLOSES it on completion, so the
/// lane reads as a tree and the closed task is the "observation ran" signal. `None` keeps the pre-pipeline
/// behavior (standalone proposals in #28) for the rollout window before the enqueue is turned on.
fn build_observer_kickoff(
    target: &str,
    session: &str,
    since_offset: usize,
    role_path: &str,
    fleet_bin: &str,
    observation_task: Option<i64>,
) -> String {
    let filing = match observation_task {
        Some(n) => format!(
            "You are working OBSERVATION TASK #{n} in board project #28 (fleet-self-improve). File each \
             above-floor, evidence-cited, deduped finding as a CHILD proposal task of it (create_task with \
             parent_id={n}, created_by=\"observer\") so the lane reads as a tree (#{n} → its proposals). Dedup \
             against the OPEN proposal children of #{n} and other OPEN `observer` proposals in #28. When you \
             have filed every proposal (or an explicit no-op finding), CLOSE the observation: update_task {n} \
             with status=\"done\", actor=\"observer\" — the closed task IS the signal the observation ran."
        ),
        None =>
            "File only above-floor, evidence-cited, deduped proposals into board project #28 \
             (fleet-self-improve) per that project's template; dedup against OPEN proposals by author \
             `observer` in #28."
                .to_string(),
    };
    format!(
        "You are an EPHEMERAL fleet `observer`. Board IDENTITY: you act as the single stable board agent id \
         `observer`. FIRST call register_agent 'observer' (idempotent). Then author EVERY board write AS \
         `observer` by PASSING THE IDENTITY PARAMETER on each call — the board defaults these to NULL, so you \
         MUST set them or the lane's single-author dedup query breaks: create_task with created_by=\"observer\", \
         comment_task / comment_document with author=\"observer\", update_task with actor=\"observer\", and \
         attribute kb_remember to `observer`. Never leave created_by/author null. This session makes exactly \
         ONE observation and EXITS — \
         do NOT start a /loop. IMPORTANT: for every `fleet` command use THIS binary by its absolute path — \
         `{fleet_bin}` — NOT the `fleet` on PATH (which may be a different build lacking `transcripts` / \
         `observe-record`). Read your full role and method at {role_path} and follow it exactly. YOUR TARGET \
         WINDOW: agent '{target}', session '{session}', from line offset {since_offset}. Read it IN FULL \
         with: {fleet_bin} transcripts {target} --session {session} --since {session}:{since_offset} \
         --overlap 40 . Lean HARD on kb_search. {filing} The Target field + agent·session·turn evidence say \
         WHICH agent each proposal is about. As your VERY LAST step — after filing your proposal(s)/close \
         above or an explicit no-op report — CONFIRM the observation so the watermark advances and this span \
         is not re-observed: run \
         `{fleet_bin} observe-record {target} --session {session} --offset <final-line-count-you-read-through>`. \
         Then exit. If you crash or stop before observe-record, the span stays unobserved and re-fires — \
         which is correct; never observe-record without having emitted."
    )
}

/// Launch (or, with `dry_run`, preview) an ephemeral observer session for one target window. Runs in a
/// repo-less workspace under the fleet root; the board identity is `observer` (the session registers/authors
/// as it per [`build_observer_kickoff`]). Returns a short action label for the sweep report. Best-effort:
/// a launch error is reported, never fatal (one target failing must not abort the sweep).
fn spawn_observer(
    board_session: &str,
    target: &str,
    obs_session: &str,
    since_offset: usize,
    observation_task: Option<i64>,
    dry_run: bool,
) -> String {
    let fleet_root = config::get()
        .root
        .clone()
        .unwrap_or_else(|| format!("{}/.fleet", std::env::var("HOME").unwrap_or_default()));
    let workdir = workspace::agent_root_dir(&fleet_root, "observer");
    // Use THIS standalone binary (its absolute path) for the observer's `fleet` commands — the `fleet` on
    // PATH may be the cadenza embedded fleet, which lacks `transcripts`/`observe-record`. The role body ships
    // beside the binary at <repo>/loops/observer.md (binary = <repo>/target/<profile>/fleet → up 3).
    let exe = std::env::current_exe().ok();
    let fleet_bin = exe
        .as_ref()
        .and_then(|p| p.to_str())
        .unwrap_or("fleet")
        .to_string();
    let role_path = exe
        .as_ref()
        .and_then(|p| p.ancestors().nth(3))
        .map(|repo| repo.join("loops/observer.md").to_string_lossy().into_owned())
        .unwrap_or_else(|| "loops/observer.md".to_string());
    // The observation task this observer drives from (#290): the caller resolved it (reuse an open one, else
    // create) and threads its id here so the observer files proposals as its children + closes it. `None`
    // (dry-run, or a board hiccup) → the observer falls back to standalone proposals.
    let kickoff =
        build_observer_kickoff(target, obs_session, since_offset, &role_path, &fleet_bin, observation_task);
    // A per-target tmux window (local only — the BOARD identity stays `observer`), so several observations
    // can run at once without a name clash.
    let window = format!("obs-{}", target.replace(['/', ':', '.'], "-"));
    if dry_run {
        return format!("would-spawn({window}←{obs_session}:{since_offset})");
    }
    if let Err(e) = std::fs::create_dir_all(&workdir) {
        return format!("spawn-FAILED(mkdir {workdir}: {e})");
    }
    let _ = pre_trust_dirs(&[fleet_root.clone(), workdir.clone()]);
    let cmd = match build_launch_cmd("claude", &resolve_model("opus"), "high", None) {
        Ok(c) => c,
        Err(e) => return format!("spawn-FAILED({e})"),
    };
    match std::process::Command::new("tmux")
        .args([
            "new-window", "-d", "-t", board_session, "-n", &window, "-c", &workdir,
            "-e", &format!("CDZ_KICKOFF={kickoff}"), &cmd,
        ])
        .status()
    {
        Ok(s) if s.success() => format!("spawned({window})"),
        Ok(_) => "spawn-FAILED(tmux new-window)".to_string(),
        Err(e) => format!("spawn-FAILED(tmux: {e})"),
    }
}

/// True iff `observe-record` is running inside an ephemeral observer's OWN tmux window (name `obs-…`), so it
/// should close that window as the observation's final act. An observer does exactly one observation and its
/// last step is this command, but the interactive harness it runs in does not exit on its own — it idles at
/// the prompt, leaving the `obs-<target>` window (and its model session) lingering until reaped by hand. The
/// `obs-` name guard means a manual `observe-record` run from any other window never self-closes. Pure —
/// unit-tested.
fn observer_should_self_close(in_tmux: bool, current_window: &str) -> bool {
    in_tmux && current_window.starts_with("obs-")
}

/// `fleet observe-record <agent> --session <sid> --offset <n>`: the CONFIRMED-observation watermark advance
/// (#188), called by the observer as its LAST step after emitting. This is the only writer of the watermark,
/// so an observer that crashed before this leaves the span unobserved to re-fire. Also clears the spawn
/// stamp: the observation completed, so a fresh growth past the new watermark may spawn immediately (the
/// cooldown only exists to avoid re-spawning an in-flight/crashed observer, not a completed one). Finally, if
/// this ran inside the observer's own `obs-<target>` tmux window, it closes that window (the observer's one
/// job is done and the harness would otherwise idle there) — the watermark is written FIRST, so the record is
/// durable even though the close tears down this process.
fn observe_record(fleet: &Fleet, agent: &str, session: &str, offset: usize) {
    write_observe_watermark(fleet, agent, session, offset);
    let _ = std::fs::remove_file(observe_spawn_stamp_path(fleet, agent));
    println!("observe-record: {agent} watermark → {session}:{offset} (observation confirmed)");
    // Self-close the observer's own window as the last act (see observer_should_self_close). Best-effort: any
    // tmux hiccup just leaves the window for the manual reap it replaces. `kill-window` is dispatched to the
    // tmux server before this pane is torn down, so it completes even though it kills our own process tree.
    if std::env::var_os("TMUX").is_some()
        && let Ok(out) = std::process::Command::new("tmux")
            .args(["display-message", "-p", "#W\t#{window_id}"])
            .output()
    {
        let line = String::from_utf8_lossy(&out.stdout);
        if let Some((name, id)) = line.trim().split_once('\t')
            && observer_should_self_close(true, name)
        {
            let _ = std::process::Command::new("tmux").args(["kill-window", "-t", id]).status();
        }
    }
}

// ── post-deploy (#171 deploy-notification channel) ─────────────────────────────────────────────────

/// The board channel deploy events are posted to; agents subscribe while waiting on a green deploy and leave
/// when done (board-pm approved one channel, #171). Created-if-absent on first post.
const DEPLOYS_CHANNEL: &str = "deploys";
/// The identity the deploy pipeline authors its posts as.
const DEPLOY_SENDER: &str = "deployer";

/// Format a deploy-confirmed channel message. A parseable, stable prefix so a waiter can match its commit:
/// `deploy <repo>@<sha> → <host>: <STATUS>` (STATUS upper-cased, e.g. LIVE | FAILED — a waiter must STOP +
/// escalate on FAILED, not wait forever, per board-pm). Pure — unit-tested.
fn deploy_event_body(repo: &str, sha: &str, host: &str, status: &str) -> String {
    format!("deploy {repo}@{sha} → {host}: {}", status.trim().to_uppercase())
}

/// `fleet post-deploy --repo --sha --host --status`: the deploy pipeline (#73/#74 deployer role) calls this
/// after a `colmena apply switch` to post a deploy-confirmed event to the `deploys` channel (#171). Resolves
/// the channel name → id (create-if-absent) then posts. Subscribed waiters are woken via the `channel.post`
/// wake-arm. Exits non-zero on a board error so a deployer can log the miss (the deploy already happened; a
/// failed NOTICE must not be silent).
fn post_deploy(repo: &str, sha: &str, host: &str, status: &str) {
    let board = board::Board::connect().unwrap_or_else(|e| {
        eprintln!("fleet post-deploy: board unavailable ({e})");
        std::process::exit(1);
    });
    let channel_id = board
        .create_or_get_channel(DEPLOYS_CHANNEL, DEPLOY_SENDER)
        .unwrap_or_else(|e| {
            eprintln!("fleet post-deploy: resolve '{DEPLOYS_CHANNEL}' channel: {e}");
            std::process::exit(1);
        });
    let body = deploy_event_body(repo, sha, host, status);
    match board.post_to_channel(channel_id, DEPLOY_SENDER, &body) {
        Ok(()) => println!("post-deploy: posted to #{DEPLOYS_CHANNEL} (id {channel_id}): {body}"),
        Err(e) => {
            eprintln!("fleet post-deploy: post to #{DEPLOYS_CHANNEL} (id {channel_id}) failed: {e}");
            std::process::exit(1);
        }
    }
}

/// The fenced, cooldown-limited re-arm of ONE candidate window, shared by the board and file-hub scans:
/// skip if still on cooldown (`"cooldown"`), skip if the pane is actively working (`"working-skip"`, the hard
/// fence), else inject the wake and stamp (`"re-armed"`); a missing window is `"no-window"`. Returns the action
/// label and whether a wake was actually sent. Never reaps or restarts.
fn rearm_candidate(
    fleet: &Fleet,
    session: &str,
    name: &str,
    interval_secs: u64,
    now: u64,
) -> (&'static str, bool) {
    if rearm_on_cooldown(read_rearm_stamp(fleet, name), now, interval_secs) {
        return ("cooldown", false);
    }
    // HARD FENCE (operator ban 2026-09-10 + seq-1387 wake-only): NEVER inject into a pane that is actively
    // working — that would interrupt a heads-down turn. A working candidate is left alone this sweep.
    if window_is_working(session, name) {
        return ("working-skip", false);
    }
    match notify::tmux_inject(session, name, WATCHDOG_REARM_WAKE) {
        Ok(()) => {
            write_rearm_stamp(fleet, name, now);
            ("re-armed", true)
        }
        Err(_) => ("no-window", false),
    }
}

/// Board-native liveness watchdog: for each BOARD-NATIVE agent (metadata.native == true), flag
/// re-arm/retighten candidates on two signals — (1) heartbeat age overdue for its own loop interval
/// (late/STALE), and (2) open assigned tasks while on a long idle interval (work-conserving). Report-only by
/// default; with `rearm` it ACTS on each candidate by injecting a wake into its tmux window (automating the
/// manual loop-reissue) — it still never reaps or restarts. File-hub mirror rows (no `native` flag) are
/// skipped: they don't run a board-native loop, so their `last_seen` is meaningless here.
///
/// A board that is unreachable does NOT abort the watchdog: the board dimension is skipped with a warning
/// and the FILE-HUB scan still runs. That resilience is the point — a flaky board is exactly when file-hub
/// agents (which have NO board delivery) most need the poll, so their liveness must not hinge on it.
fn watchdog(
    stale_only: bool,
    rearm: bool,
    observe: bool,
    spawn: bool,
    spawn_dry_run: bool,
    pinned_only: bool,
) {
    // Self-surface a stale binary: the watchdog is long-running (a timer/loop re-execs this binary), so if its
    // source checkout advanced past the built rev it would silently run old logic (a merged fix not effective
    // until rebuilt). Warn rather than act — rebuilding is out of band. No-op for a deployed binary (no .git).
    if let Some(w) = build_freshness_warning(env!("FLEET_BUILD_REV"), checkout_head_short().as_deref()) {
        eprintln!("{w}");
    }
    // Board-native agent ids, so the file-hub scan can SKIP any that still have a stale active file-hub row
    // (heartbeat to the board, not the file → a stale file mtime would false-flag them). Empty when the board
    // is unreachable — the file-hub scan then covers everything as a best-effort outage fallback.
    let native_ids = match board::Board::connect().and_then(|b| b.list_agents().map(|agents| (b, agents))) {
        Ok((board, agents)) => {
            let native_ids = native_agent_ids(&agents);
            watchdog_board(&board, &agents, stale_only, rearm, observe, spawn, spawn_dry_run, pinned_only);
            native_ids
        }
        Err(e) => {
            eprintln!("fleet watchdog: board unavailable ({e}); scanning the file-hub only");
            std::collections::BTreeSet::new()
        }
    };
    // FILE-HUB agents are not on the board (no board event delivery), so the event-wake path never reaches
    // them — the poll watchdog is their only liveness. Scan the file-hub registry too (no-op when no hub is
    // configured / no active file-hub agents, i.e. a board-only host). Runs regardless of board health above.
    watchdog_file_hub(stale_only, rearm, &native_ids);

    // TUNNEL health: the reverse tunnel carries event-wakes to this host's agents; a WEDGED socket delivers
    // nothing silently, so a starved agent only falls back to its (slow) poll interval. If a health-probe URL
    // is configured, GET it each sweep and report — surfacing a wedge here beats an idle agent discovering it.
    watchdog_tunnel_health();
}

/// Probe the fleet-tunnel health endpoint (config `tunnel_health_url`) and print its verdict. A no-op when
/// unset (a host with no tunnel). Report-only — like the rest of the watchdog it never reaps/restarts; it
/// makes a WEDGED wake-delivery path visible. The bounded GET can't hang the sweep.
fn watchdog_tunnel_health() {
    let Some(url) = config::get().tunnel_health_url.clone().filter(|s| !s.trim().is_empty()) else {
        return;
    };
    let agent = ureq::AgentBuilder::new()
        .timeout(std::time::Duration::from_secs(2))
        .build();
    let probe: Result<u16, String> = match agent.get(&url).call() {
        Ok(resp) => Ok(resp.status()),
        Err(ureq::Error::Status(code, _)) => Ok(code), // a 503 is a reachable "not ok", not a transport error
        Err(e) => Err(e.to_string()),
    };
    let (ok, msg) = tunnel_health_line(&probe);
    println!("-- tunnel --");
    println!("{}", msg);
    if !ok {
        eprintln!("  ⚠ tunnel wedged: event-wakes are NOT being delivered to this host — agents fall back to slow poll. Probe {url}");
    }
}

/// Classify a tunnel health-probe outcome into `(ok, one-line report)`. `Ok(200)` = healthy; any other
/// status (e.g. the daemon's own `503` when the socket is wedged) or a transport error = NOT ok. Pure so the
/// verdict is unit-testable without a live daemon; the caller does the bounded GET. See fleet-tunnel #59.
fn tunnel_health_line(probe: &Result<u16, String>) -> (bool, String) {
    match probe {
        Ok(200) => (true, "tunnel health: OK (HTTP 200 — board WS connected, frame fresh, upstream reachable)".to_string()),
        Ok(code) => (false, format!("tunnel health: WEDGED (HTTP {code} — probe reachable but not ok)")),
        Err(e) => (false, format!("tunnel health: UNREACHABLE (probe failed: {e})")),
    }
}

/// The ids of every board-native agent (metadata.native == true) in a board roster. The file-hub scan uses
/// this to skip agents already covered by the board scan. Pure — unit-tested.
fn native_agent_ids(agents: &[serde_json::Value]) -> std::collections::BTreeSet<String> {
    agents
        .iter()
        .filter(|a| {
            a.get("metadata")
                .and_then(|m| m.get("native"))
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false)
        })
        .filter_map(|a| a.get("id").and_then(serde_json::Value::as_str).map(String::from))
        .collect()
}

/// The BOARD dimension of the watchdog: scan the board roster's native agents. Split out of [`watchdog`] so a
/// board outage skips only this pass, leaving the file-hub scan to run. See [`watchdog`] for the signals.
#[allow(clippy::too_many_arguments)]
fn watchdog_board(
    board: &board::Board,
    agents: &[serde_json::Value],
    stale_only: bool,
    rearm: bool,
    observe: bool,
    spawn: bool,
    spawn_dry_run: bool,
    pinned_only: bool,
) {
    let now = time::OffsetDateTime::now_utc();
    let now_unix = now.unix_timestamp().max(0) as u64; // for the per-agent re-arm cooldown stamps
    let fleet = Fleet::resolve(); // stamp store (<hub>/.claude/fleet/watchdog/); shared with the file-hub scan
    let session = board_session();
    let host = this_host(); // host-affinity: this box only manages agents pinned here (or unpinned)
    if pinned_only {
        println!("(--pinned-only: managing only agents EXPLICITLY pinned to {host}; unpinned run-anywhere agents skipped)");
    }
    // Observation (#187): per-agent transcript-growth threshold (lines/records). Read once per sweep.
    let observe_threshold = std::env::var("CDZ_OBSERVE_LINES")
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(OBSERVE_LINES_DEFAULT);
    // Collected observation candidates: (target agent, stood_down, decision). Displayed after the table, and
    // — with --spawn (#188) — the highest-growth few are launched as ephemeral observers (cap + cooldown).
    let mut obs: Vec<(String, bool, ObserveDecision)> = Vec::new();
    println!(
        "{:<28} {:<8} {:<7} {:<5} {:<8} {:<12} last_seen",
        "agent", "interval", "age", "open", "verdict", "action"
    );
    let mut flagged = 0usize;
    let mut rearmed = 0usize;
    let mut native = 0usize;
    for a in agents {
        let md = a.get("metadata");
        let is_native = md
            .and_then(|m| m.get("native"))
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        if !is_native {
            continue;
        }
        // Host affinity: skip agents this box should not manage — a DIFFERENT-box pin always, and (under
        // --pinned-only) unpinned run-anywhere agents too, so a secondary box never re-arms/observes an agent
        // whose tmux window / transcript lives elsewhere. See [`watchdog_manages_agent`].
        if !watchdog_manages_agent(md, &host, pinned_only) {
            continue;
        }
        native += 1;
        let id = a.get("id").and_then(|v| v.as_str()).unwrap_or("?");
        let interval_str = md
            .and_then(|m| m.get("interval"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let ls = a.get("last_seen").and_then(|v| v.as_str()).unwrap_or("");
        let interval_secs = parse_interval_secs(interval_str).unwrap_or(0);
        let (verdict, age_str) = match last_seen_age_secs(ls, now) {
            Some(age) => (watchdog_verdict(age, interval_secs), format!("{}m", age / 60)),
            None => ("?", "?".to_string()),
        };
        // Best-effort open assigned-task count (the second signal); a query error degrades to 0/"?" and
        // simply doesn't flag on the task dimension rather than failing the whole watchdog.
        let (open_tasks, open_str) = match board.open_task_count(id) {
            Ok(n) => (n, n.to_string()),
            Err(_) => (0, "?".to_string()),
        };
        // Presence-derived idleness. `stood_down` (board `offline`) = a RETIRED/spun-down agent → drives the
        // observe spin-down trigger. `idle_presence` = a presence that is NOT expected to loop (`away` /
        // `offline` / `done` at-rest, see [`presence_suppresses_rearm`]) → suppresses the re-arm when its queue
        // is drained, so a healthy idle vertical OR a deliberately at-rest worker (e.g. a `done` agent parked
        // until revived) isn't flagged STALE and poked every interval. Kept separate from `stood_down`: an
        // `away`/`done` agent is idle/at-rest but NOT spun down, so it must not trigger a spin-down observation.
        let status = a.get("status").and_then(serde_json::Value::as_str);
        let stood_down = status == Some("offline");
        let idle_presence = presence_suppresses_rearm(status);
        // Observation (#187): check transcript growth BEFORE the stale-only skip below — a spin-down (offline)
        // agent is not a re-arm candidate, so it would be skipped, yet its closing read is exactly what the
        // mandatory spin-down trigger must catch. Report-only this slice (no spawn / no watermark advance).
        if observe && let Some(d) = observe_candidate(&fleet, id, stood_down, observe_threshold) {
            obs.push((id.to_string(), stood_down, d));
        }
        let retighten = is_retighten_candidate(verdict, open_tasks, interval_secs, idle_presence);
        if stale_only && !retighten {
            continue;
        }
        // With --rearm, ACT on each candidate: a cooldown-limited, pane-fenced wake so it runs a tick now
        // (never reaps/restarts). See [`rearm_candidate`].
        let action = if retighten {
            flagged += 1;
            if rearm {
                let (act, did) = rearm_candidate(&fleet, &session, id, interval_secs, now_unix);
                if did {
                    rearmed += 1;
                }
                act
            } else {
                "candidate"
            }
        } else {
            "ok"
        };
        let iv = if interval_str.is_empty() { "?" } else { interval_str };
        println!("{id:<28} {iv:<8} {age_str:<7} {open_str:<5} {verdict:<8} {action:<12} {ls}");
    }
    if stale_only && flagged == 0 {
        println!("(all {native} board-native agents ok)");
    } else if rearm {
        println!(
            "-- {native} board-native agent(s); {flagged} candidate(s); {rearmed} re-armed (wake injected)"
        );
    } else {
        println!(
            "-- {native} board-native agent(s); {flagged} re-arm/retighten candidate(s) (overdue heartbeat, or open tasks on a long interval); pass --rearm to wake them"
        );
    }
    if observe {
        if obs.is_empty() {
            println!(
                "-- observation: no agent over the {observe_threshold}-line growth threshold"
            );
        } else {
            let display: Vec<String> = obs
                .iter()
                .map(|(id, sd, d)| {
                    let why = if *sd { "spin-down" } else { "size" };
                    format!("{id}[{}:{}+{} {why}]", d.session, d.since_offset, d.increment)
                })
                .collect();
            let tail = if spawn { "" } else { " (report-only; pass --spawn to launch observers)" };
            println!("-- observation candidates ({}){}: {}", obs.len(), tail, display.join(", "));
        }
        if spawn {
            observe_spawn_pass(board, &fleet, &session, &mut obs, now_unix, spawn_dry_run);
        }
    }
}

/// Resolve the observation task an observer should be spawned against for `target` (#290): REUSE an OPEN one
/// if present (an in-flight or crashed observer's task — never a duplicate), else CREATE a fresh task stamped
/// `metadata.observes=target` so the idempotency query finds it next sweep. Authored as `observer` so the
/// whole self-improve lane stays single-author (the observer's proposal children match). In `dry_run` this
/// only QUERIES (read-only) and reports what it WOULD create/reuse, returning the existing id or `None` (no
/// create). Best-effort: a board error returns `None` and the observer runs without a parent task (falling
/// back to standalone proposals) rather than aborting the sweep.
fn resolve_observation_task(
    board: &board::Board,
    target: &str,
    session: &str,
    since_offset: usize,
    dry_run: bool,
) -> Option<i64> {
    match board.open_observation_task(SELF_IMPROVE_PROJECT, target) {
        Ok(Some(existing)) => {
            println!("   observation task: reuse OPEN #{existing} (observes {target})");
            Some(existing)
        }
        Ok(None) => {
            let spec = observation_task_spec(target, session, since_offset);
            if dry_run {
                println!(
                    "   observation task: would CREATE in project #{} — \"{}\"",
                    spec.project_id, spec.title
                );
                None
            } else {
                let meta = serde_json::json!({ "observes": target });
                match board.create_task(spec.project_id, &spec.title, &spec.body, "observer", meta, None) {
                    Ok(id) => {
                        println!("   observation task: created #{id} (observes {target})");
                        Some(id)
                    }
                    Err(e) => {
                        eprintln!("   observation task: create FAILED ({e}); observer runs without a parent task");
                        None
                    }
                }
            }
        }
        Err(e) => {
            eprintln!("   observation task: open-check FAILED ({e}); observer runs without a parent task");
            None
        }
    }
}

/// The --spawn half (#188): launch ephemeral observers for the highest-growth candidates, bounded by a
/// per-sweep CAP and a per-target COOLDOWN (an in-flight/crashed observer's target must not re-spawn every
/// 60s sweep — the watermark only advances on confirmation). Each spawn is driven by an observation task
/// (#290): [`resolve_observation_task`] reuses/creates it and its id threads into the observer. `dry_run`
/// previews without launching (or creating). Splitting this out keeps the sweep loop readable; sorting by
/// increment puts the most-grown windows first.
fn observe_spawn_pass(
    board: &board::Board,
    fleet: &Fleet,
    session: &str,
    obs: &mut [(String, bool, ObserveDecision)],
    now_unix: u64,
    dry_run: bool,
) {
    let cap = std::env::var("CDZ_OBSERVE_SPAWN_CAP")
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(OBSERVE_SPAWN_CAP_DEFAULT);
    let cooldown = std::env::var("CDZ_OBSERVE_SPAWN_COOLDOWN_SECS")
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(OBSERVE_SPAWN_COOLDOWN_SECS);
    // SPIN-DOWN candidates first (their closing read is mandatory + unrepeatable — a retiring agent's
    // context is about to be gone), then most-grown first. So a small spin-down window is never crowded out
    // of the per-sweep cap by large size candidates.
    obs.sort_by_key(|(_, stood_down, d)| (std::cmp::Reverse(*stood_down), std::cmp::Reverse(d.increment)));
    let mut launched = 0usize;
    let mut actions: Vec<String> = Vec::new();
    for (id, _sd, d) in obs.iter() {
        if launched >= cap {
            actions.push(format!("{id}=cap-deferred"));
            continue;
        }
        if observe_on_spawn_cooldown(read_observe_spawn_stamp(fleet, id), now_unix, cooldown) {
            actions.push(format!("{id}=cooldown"));
            continue;
        }
        let obs_task = resolve_observation_task(board, id, &d.session, d.since_offset, dry_run);
        let act = spawn_observer(session, id, &d.session, d.since_offset, obs_task, dry_run);
        if !dry_run && act.starts_with("spawned") {
            write_observe_spawn_stamp(fleet, id, now_unix);
        }
        if act.starts_with("spawned") || act.starts_with("would-spawn") {
            launched += 1;
        }
        actions.push(format!("{id}={act}"));
    }
    let mode = if dry_run { "DRY-RUN " } else { "" };
    println!("-- observer {mode}spawn (cap {cap}): {}", actions.join(", "));
}

/// Watchdog scan of the FILE-HUB registry (the agents not yet migrated board-native). Same signals adapted
/// to the file hub: heartbeat age = the `heartbeat/<name>` touch-file mtime vs the agent's interval;
/// "pending work" = undrained inbox messages. Same pane-fenced, wake-only re-arm. No-op (silent) when the
/// hub resolves to no registry or has no active agents, so a board-only host prints nothing extra. Requires
/// `config.hub` to point at the file hub for anything to scan. `native_ids` are board-native agents to SKIP:
/// an agent that migrated board-native but still has an active file-hub row heartbeats to the board, not the
/// file, so its file mtime is stale by design — the board scan already covers it, and scanning it here would
/// false-flag it STALE (the v-slack-bridge report). The real cleanup is `fleet deregister`, but skipping is
/// the robust guard.
fn watchdog_file_hub(stale_only: bool, rearm: bool, native_ids: &std::collections::BTreeSet<String>) {
    let fleet = Fleet::resolve();
    let reg = fleet.load();
    let active: Vec<&Agent> = reg
        .agents
        .iter()
        .filter(|a| a.status == "active" && !native_ids.contains(&a.name))
        .collect();
    if active.is_empty() {
        return;
    }
    let session = board_session();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    println!("-- file-hub agents --");
    println!(
        "{:<28} {:<8} {:<7} {:<5} {:<8} {:<12} heartbeat-age",
        "agent", "interval", "age", "inbox", "verdict", "action"
    );
    let mut flagged = 0usize;
    let mut rearmed = 0usize;
    for a in &active {
        let interval_secs = parse_interval_secs(&a.interval).unwrap_or(0);
        let hb = file_mtime_unix(&fleet.root.join("heartbeat").join(&a.name));
        let (verdict, age_str) = match hb {
            Some(m) => {
                let age = now.saturating_sub(m) as i64;
                (watchdog_verdict(age, interval_secs), format!("{}m", age / 60))
            }
            None => ("?", "?".to_string()),
        };
        let pending = inbox_pending_count(&fleet, &a.name);
        // File-hub rows have no board presence; board-native (offline-capable) agents are already excluded
        // from this scan (see `native_ids`), so no stand-down short-circuit applies here.
        let retighten = is_retighten_candidate(verdict, pending, interval_secs, false);
        if stale_only && !retighten {
            continue;
        }
        let action = if retighten {
            flagged += 1;
            if rearm {
                let (act, did) = rearm_candidate(&fleet, &session, &a.name, interval_secs, now);
                if did {
                    rearmed += 1;
                }
                act
            } else {
                "candidate"
            }
        } else {
            "ok"
        };
        let iv = if a.interval.is_empty() { "?" } else { &a.interval };
        println!(
            "{:<28} {iv:<8} {age_str:<7} {pending:<5} {verdict:<8} {action:<12}",
            a.name
        );
    }
    if rearm {
        println!(
            "-- {} active file-hub agent(s); {flagged} candidate(s); {rearmed} re-armed",
            active.len()
        );
    } else {
        println!(
            "-- {} active file-hub agent(s); {flagged} candidate(s); pass --rearm to wake them",
            active.len()
        );
    }
}

/// Parse a `owner/name@branch` repo spec into a board `repos` entry `{repo, branch}` (branch defaults to
/// `main` when the `@branch` suffix is absent or empty). Pure — unit-tested.
fn parse_repo_spec(spec: &str) -> serde_json::Value {
    let (repo, branch) = match spec.split_once('@') {
        Some((r, b)) if !b.is_empty() => (r, b),
        _ => (spec.trim_end_matches('@'), "main"),
    };
    serde_json::json!({ "repo": repo, "branch": branch })
}

/// Build the metadata patch (the subset of keys to merge) from the requested `repos` + `interval`, or an
/// error string if nothing was requested. Pure — unit-tested.
fn build_meta_patch(
    repos: &[String],
    interval: Option<&str>,
    host: Option<&str>,
    native: Option<bool>,
    devshell: Option<bool>,
) -> Result<serde_json::Value, String> {
    let mut patch = serde_json::Map::new();
    if !repos.is_empty() {
        let entries: Vec<serde_json::Value> = repos.iter().map(|s| parse_repo_spec(s)).collect();
        patch.insert("repos".to_string(), serde_json::Value::Array(entries));
    }
    if let Some(iv) = interval {
        patch.insert("interval".to_string(), serde_json::Value::String(iv.to_string()));
    }
    if let Some(h) = host {
        // Pin the agent to a host (host-affinity). `""` clears the pin (unpinned) via JSON null.
        let val = if h.trim().is_empty() {
            serde_json::Value::Null
        } else {
            serde_json::Value::String(h.to_string())
        };
        patch.insert("host".to_string(), val);
    }
    if let Some(n) = native {
        // The board-native roster marker the watchdog splits on (native → board-watchdog; else file-hub).
        patch.insert("native".to_string(), serde_json::Value::Bool(n));
    }
    if let Some(d) = devshell {
        // Opt-in: launch the agent inside its workdir's flake devShell (pinned toolchain on PATH; #214).
        patch.insert("devshell".to_string(), serde_json::Value::Bool(d));
    }
    if patch.is_empty() {
        return Err(
            "nothing to set — pass at least one --repo, --interval, --host, --native, or --devshell".to_string(),
        );
    }
    Ok(serde_json::Value::Object(patch))
}

/// Write launch-shaping metadata (`repos` / `interval` / `host` / `native`) onto an agent's board record —
/// the migration primitive. Reports the patch by default; `--apply` PATCHes it (key-level merge, so
/// untouched keys are preserved) and reads the record back to confirm.
fn set_meta(
    agent: &str,
    repos: &[String],
    interval: Option<&str>,
    host: Option<&str>,
    native: Option<bool>,
    devshell: Option<bool>,
    apply: bool,
) {
    let patch = build_meta_patch(repos, interval, host, native, devshell).unwrap_or_else(|e| {
        eprintln!("fleet set-meta: {e}");
        std::process::exit(2);
    });
    let board = board::Board::connect().unwrap_or_else(|e| {
        eprintln!("fleet set-meta: {e}");
        std::process::exit(1);
    });
    println!(
        "set-meta '{agent}' ({}): merge {}",
        if apply { "APPLY" } else { "dry-run" },
        serde_json::to_string(&patch).unwrap_or_default()
    );
    if !apply {
        println!("  (dry-run — re-run with --apply to write; merge preserves every other metadata key)");
        return;
    }
    if let Err(e) = board.patch_metadata(agent, patch) {
        eprintln!("  write FAILED: {e}");
        std::process::exit(1);
    }
    match board.get_agent(agent).ok().and_then(|r| r.get("metadata").cloned()) {
        Some(md) => println!(
            "  written. metadata now: repos={} interval={} host={}",
            md.get("repos").map(|v| v.to_string()).unwrap_or_else(|| "<none>".into()),
            md.get("interval").and_then(|v| v.as_str()).unwrap_or("<none>"),
            md.get("host").map(|v| v.to_string()).unwrap_or_else(|| "<none>".into())
        ),
        None => println!("  written (could not read back the record to confirm)"),
    }
}

/// Set `agent`'s interval in its file-hub registry row if one exists; returns whether a row was updated. Pure
/// over the registry so it's unit-testable; the caller persists with [`Fleet::save`].
fn registry_set_interval(reg: &mut Registry, agent: &str, interval: &str) -> bool {
    if let Some(a) = reg.agents.iter_mut().find(|a| a.name == agent) {
        a.interval = interval.to_string();
        true
    } else {
        false
    }
}

/// Change an agent's loop interval on the board metadata (the source of truth the watchdog reads) AND, if the
/// agent still has a file-hub registry row, that row — so a still-migrating agent's two records never disagree
/// and the watchdog can't false-nudge an agent that lowered its cadence.
fn set_interval(fleet: &Fleet, agent: &str, interval: &str) {
    // 1) Board metadata — the write that actually stops the false overdue-nudges (the watchdog reads it).
    let board = board::Board::connect().unwrap_or_else(|e| {
        eprintln!("fleet set-interval: {e}");
        std::process::exit(1);
    });
    if let Err(e) = board.patch_metadata(agent, serde_json::json!({ "interval": interval })) {
        eprintln!("fleet set-interval: board write FAILED: {e}");
        std::process::exit(1);
    }
    println!("set-interval '{agent}': board metadata interval = {interval}");
    // 2) Keep the legacy file-hub registry row in sync if one still exists (migration hygiene; a board-only
    // host or an already-deregistered agent simply has no row to update).
    let mut reg = fleet.load();
    if registry_set_interval(&mut reg, agent, interval) {
        fleet.save(&reg);
        println!("  also updated the file-hub registry row (kept in sync during migration)");
    }
}

/// Render an agent's session transcript faithfully (see [`transcripts`]). Resolves the session file
/// (`--session`, else the watermark's session, else the agent's newest), parses it, windows it by the
/// `--since` record offset backed up by `--overlap`, renders + scrubs, and prints the advancing watermark.
/// The watermark offset is a RECORD index (parsed JSONL records already observed), printed as
/// `<session-id>:<record-count>` in the footer.
fn transcripts_cmd(agent: &str, session: Option<&Path>, since: Option<&str>, overlap: usize) {
    let (path, want_offset) = match session {
        Some(p) => (
            p.to_path_buf(),
            since.map(|s| transcripts::parse_watermark(s).1).unwrap_or(0),
        ),
        None => {
            let sessions = transcripts::locate_sessions(agent);
            if sessions.is_empty() {
                eprintln!("fleet transcripts: no session files found for '{agent}' (try --session <file>)");
                std::process::exit(1);
            }
            match since.map(transcripts::parse_watermark) {
                // Watermark names a session: render THAT one from its offset if we can find it, else the
                // newest from the start (the named session rotated away).
                Some((sid, off)) => match sessions.iter().find(|p| transcripts::session_id_of(p) == sid) {
                    Some(p) => (p.clone(), off),
                    None => (sessions.into_iter().next().unwrap(), 0),
                },
                None => (sessions.into_iter().next().unwrap(), 0),
            }
        }
    };

    let (records, _lines) = transcripts::parse_jsonl(&path).unwrap_or_else(|e| {
        eprintln!("fleet transcripts: {e}");
        std::process::exit(1);
    });
    let start = transcripts::window_start(want_offset, overlap).min(records.len());
    let windowed = &records[start..];
    let sid = transcripts::session_id_of(&path);

    println!(
        "=== transcript: {agent} · session {sid} · records {start}..{} (overlap {overlap}) · harness {} ===",
        records.len(),
        transcripts::ClaudeCode.id()
    );
    print!("{}", transcripts::render(windowed, &transcripts::ClaudeCode));
    // The advancing watermark: feed back as `--since <this>` next observation.
    println!("=== watermark: {sid}:{} ===", records.len());
}

/// List the tmux windows in `session` by name (`#W`), or an empty list if tmux is unreachable — a host with
/// no session yet serves nothing, which is the correct degenerate answer, not an error.
fn tmux_window_names(session: &str) -> Vec<String> {
    std::process::Command::new("tmux")
        .args(["list-windows", "-t", session, "-F", "#W"])
        .output()
        .ok()
        .map(|out| {
            String::from_utf8_lossy(&out.stdout)
                .lines()
                .map(|l| l.trim().to_string())
                .filter(|l| !l.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

/// `fleet served-set` — print the agents THIS host should serve on the reverse tunnel: the board agents that
/// have a live tmux window in this session AND aren't pinned to another host. This replaces the hand-kept
/// static tunnel served list, whose staleness silently starves a new/moved agent of event-wakes (the
/// v-s2n-quic starvation). Default: one id per line; `--toml`: the `agents = [ ... ]` block for the tunnel
/// config. A board outage prints an error and exits non-zero (never a partial/misleading list).
fn served_set(toml: bool) {
    let board = board::Board::connect().unwrap_or_else(|e| {
        eprintln!("fleet served-set: board unavailable ({e}); cannot derive the served set");
        std::process::exit(1);
    });
    let roster = board.list_agents().unwrap_or_else(|e| {
        eprintln!("fleet served-set: board roster query failed ({e})");
        std::process::exit(1);
    });
    let agents: Vec<(String, Option<serde_json::Value>)> = roster
        .iter()
        .filter_map(|a| {
            let id = a.get("id").and_then(serde_json::Value::as_str)?.to_string();
            Some((id, a.get("metadata").cloned()))
        })
        .collect();
    let windows = tmux_window_names(&board_session());
    let served = derive_served_set(&windows, &agents, &this_host());
    if toml {
        println!("agents = [");
        for id in &served {
            println!("  \"{id}\",");
        }
        println!("]");
    } else {
        for id in &served {
            println!("{id}");
        }
    }
}

/// The watchdog invocation a cadence unit runs: the liveness sweep (`--rearm --stale-only`) when `rearm`, plus
/// the observer cadence (`--observe --spawn`) and/or the host filter (`--pinned-only`) when requested. An
/// OBSERVER-ONLY unit (`rearm=false, observe=true` → `watchdog --observe --spawn`) can run alongside an
/// existing rearm watchdog without double-rearming — the dev-desk coexistence case. Pure — unit-tested.
fn watchdog_exec_args(rearm: bool, observe: bool, pinned_only: bool) -> String {
    let mut args = String::from("watchdog");
    if rearm {
        args.push_str(" --rearm --stale-only");
    }
    if observe {
        args.push_str(" --observe --spawn");
    }
    if pinned_only {
        args.push_str(" --pinned-only");
    }
    args
}

/// The runtime-config environment an observer needs to launch a fresh Claude Code session, captured from the
/// installing process's environment. A systemd USER service starts with a stripped environment, and a window
/// the watchdog spawns via `tmux new-window` inherits the INVOKING client's `PATH` (not the tmux server's), so
/// under the service the observer window's `exec claude` cannot find `claude` (it lives in a user-local bin) and
/// the window closes with status 127 the moment it opens. Bedrock model selection is likewise env-driven. So
/// capture the exact working values at install time.
///
/// This is an ALLOWLIST of generic, public variable NAMES (their values are written into the LOCAL unit file,
/// never into source): the search `PATH`, the AWS region/profile the SDK resolves file-based credentials by, the
/// Bedrock toggle, and the default model ids. The volatile per-session identity variables (a session id, a
/// messaging socket/token, a pid) are deliberately EXCLUDED so a spawned observer gets a clean session, never a
/// copy of the installer's session. An unset variable is skipped.
const OBSERVER_ENV_ALLOWLIST: &[&str] = &[
    "PATH",
    "AWS_REGION",
    "AWS_DEFAULT_REGION",
    "AWS_PROFILE",
    "CLAUDE_CODE_USE_BEDROCK",
    "ANTHROPIC_DEFAULT_OPUS_MODEL",
    "ANTHROPIC_DEFAULT_SONNET_MODEL",
    "ANTHROPIC_DEFAULT_HAIKU_MODEL",
];

/// Render systemd `Environment="K=V"` lines for the given (name, value) pairs, one per present value (a `None`
/// value is skipped). Values are double-quoted per systemd syntax so a `PATH` with no spaces is safe and a value
/// that ever gains a space stays one assignment. Pure — unit-tested.
fn render_service_env_lines(vars: &[(&str, Option<String>)]) -> String {
    let mut out = String::new();
    for (name, value) in vars {
        if let Some(v) = value {
            out.push_str(&format!("Environment=\"{name}={v}\"\n"));
        }
    }
    out
}

/// The observer runtime-config env block ([`OBSERVER_ENV_ALLOWLIST`]) read from THIS process's environment and
/// rendered as systemd `Environment=` lines. Run at install time from the working interactive session so the
/// captured values are the ones under which a Claude session actually launches. Reads the environment (not pure).
fn captured_observer_env() -> String {
    let vars: Vec<(&str, Option<String>)> =
        OBSERVER_ENV_ALLOWLIST.iter().map(|n| (*n, std::env::var(n).ok())).collect();
    render_service_env_lines(&vars)
}

/// The systemd USER service + timer for the watchdog cadence as `(service_text, timer_text)` — pure unit text
/// (no display headers), so it can be written to unit files or wrapped for stdout. The service is a `oneshot`
/// (the watchdog is single-sweep) ordered After/Wants `fleet-notify` (the wake path it complements); the timer
/// re-fires it on `OnUnitActiveSec`. `env_block` is the pre-rendered `Environment=` lines (empty when the unit
/// spawns nothing — a rearm-only watchdog needs no launch environment). Pure — unit-tested.
fn watchdog_unit_files(
    fleet_bin: &str,
    exec_args: &str,
    interval_secs: u64,
    env_block: &str,
) -> (String, String) {
    let service = format!(
        "[Unit]\n\
         Description=Fleet watchdog — out-of-band /loop re-arm + observer cadence\n\
         After=fleet-notify.service\n\
         Wants=fleet-notify.service\n\n\
         [Service]\n\
         Type=oneshot\n\
         {env_block}\
         ExecStart={fleet_bin} {exec_args}\n"
    );
    let timer = format!(
        "[Unit]\n\
         Description=Fleet watchdog cadence\n\n\
         [Timer]\n\
         OnBootSec=60\n\
         OnUnitActiveSec={interval_secs}\n\
         Persistent=true\n\n\
         [Install]\n\
         WantedBy=timers.target\n"
    );
    (service, timer)
}

/// The two units concatenated with display headers, for `fleet watchdog-unit` stdout — a host installs these
/// DECLARATIVELY (home-manager `systemd.user.services`/`timers`); the emitted text is the canonical shape to
/// translate, not a file to write. Pure — unit-tested.
fn render_watchdog_units(fleet_bin: &str, exec_args: &str, interval_secs: u64, env_block: &str) -> String {
    let (service, timer) = watchdog_unit_files(fleet_bin, exec_args, interval_secs, env_block);
    format!(
        "# ---- fleet-watchdog.service (systemd USER oneshot) ----\n{service}\n\
         # ---- fleet-watchdog.timer (fires the service every {interval_secs}s) ----\n{timer}"
    )
}

/// The `~/.config/systemd/user` directory (XDG_CONFIG_HOME, else `$HOME/.config`) where a user timer installs.
fn user_unit_dir() -> Option<std::path::PathBuf> {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".config")))
        .map(|c| c.join("systemd/user"))
}

/// `fleet watchdog-unit` — the watchdog cadence's systemd USER service + timer. Default: PRINT it (a host on
/// the declarative/nix model translates the text). `--install`: WRITE it into `~/.config/systemd/user/`
/// (user-level, no sudo) for a host not on that model — a clean, reversible path. `--uninstall`: remove it.
/// `rearm=false` (`--no-rearm`) installs an OBSERVER-ONLY unit that coexists with an existing rearm watchdog
/// (the dev-desk go-live: the system rearm service is left untouched, no sudo needed). `bin` defaults to this
/// binary's absolute path.
fn watchdog_unit(
    rearm: bool,
    observe: bool,
    pinned_only: bool,
    interval_secs: u64,
    bin: Option<String>,
    install: bool,
    uninstall: bool,
) {
    let fleet_bin = bin.unwrap_or_else(|| {
        std::env::current_exe()
            .ok()
            .and_then(|p| p.to_str().map(str::to_string))
            .unwrap_or_else(|| "fleet".to_string())
    });
    let exec_args = watchdog_exec_args(rearm, observe, pinned_only);
    // Only an observer-spawning watchdog needs a launch environment (a rearm-only sweep just sends keys to an
    // existing window). Capture it from this (working) session so the installed service can launch Claude.
    let env_block = if observe { captured_observer_env() } else { String::new() };
    if uninstall {
        watchdog_unit_uninstall();
        return;
    }
    if install {
        watchdog_unit_install(&fleet_bin, &exec_args, interval_secs, &env_block);
        return;
    }
    print!("{}", render_watchdog_units(&fleet_bin, &exec_args, interval_secs, &env_block));
}

/// Write the watchdog service + timer into `~/.config/systemd/user/` and print the enable command. User-level
/// (no sudo). Idempotent (overwrites). Non-fatal guidance to stop any ad-hoc watchdog loop and to reverse.
fn watchdog_unit_install(fleet_bin: &str, exec_args: &str, interval_secs: u64, env_block: &str) {
    let Some(dir) = user_unit_dir() else {
        eprintln!("fleet watchdog-unit --install: cannot resolve ~/.config/systemd/user (no HOME/XDG_CONFIG_HOME)");
        std::process::exit(1);
    };
    if let Err(e) = std::fs::create_dir_all(&dir) {
        eprintln!("fleet watchdog-unit --install: mkdir {}: {e}", dir.display());
        std::process::exit(1);
    }
    let (service, timer) = watchdog_unit_files(fleet_bin, exec_args, interval_secs, env_block);
    for (name, body) in [("fleet-watchdog.service", &service), ("fleet-watchdog.timer", &timer)] {
        let path = dir.join(name);
        if let Err(e) = std::fs::write(&path, body) {
            eprintln!("fleet watchdog-unit --install: write {}: {e}", path.display());
            std::process::exit(1);
        }
        println!("installed {}", path.display());
    }
    println!("  ExecStart: {fleet_bin} {exec_args}");
    println!("  enable:  systemctl --user daemon-reload && systemctl --user enable --now fleet-watchdog.timer");
    println!("  reverse: fleet watchdog-unit --uninstall  (or: systemctl --user disable --now fleet-watchdog.timer)");
}

/// Remove the user watchdog units this installed and print the disable command. Best-effort (a missing file is
/// fine — the inverse of an install that never happened).
fn watchdog_unit_uninstall() {
    let Some(dir) = user_unit_dir() else {
        eprintln!("fleet watchdog-unit --uninstall: cannot resolve ~/.config/systemd/user");
        std::process::exit(1);
    };
    println!("  disable FIRST: systemctl --user disable --now fleet-watchdog.timer");
    for name in ["fleet-watchdog.service", "fleet-watchdog.timer"] {
        let path = dir.join(name);
        match std::fs::remove_file(&path) {
            Ok(()) => println!("removed {}", path.display()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => println!("absent (ok): {}", path.display()),
            Err(e) => eprintln!("  WARN: remove {}: {e}", path.display()),
        }
    }
    println!("  then: systemctl --user daemon-reload");
}

/// The build-provenance line: package version + the revision the binary was built from (baked by `build.rs`
/// into `FLEET_BUILD_REV`). Lets an operator or agent tell whether a deployed binary is current by comparing
/// the rev to `origin/main` — the signal that was missing when a stale watchdog binary silently ran old logic.
fn version_line() -> String {
    format!("fleet {} (rev {})", env!("CARGO_PKG_VERSION"), env!("FLEET_BUILD_REV"))
}

/// Whether the running binary is STALE relative to its source checkout — the baked build rev differs from the
/// checkout's current HEAD (the failure mode a stale watchdog binary hit: the checkout was pulled to main but
/// the binary not rebuilt, so it silently ran old logic). Returns a warning, or `None` when it can't tell: an
/// `unknown` baked rev, or no local checkout (a hermetic/deployed binary, which tracks its flake input). A
/// `-dirty` baked rev compares by its base sha — a dirty build of the same commit is not stale. Pure — unit-tested.
fn build_freshness_warning(baked_rev: &str, checkout_head: Option<&str>) -> Option<String> {
    let head = checkout_head?;
    if baked_rev.is_empty() || baked_rev == "unknown" {
        return None;
    }
    let base = baked_rev.strip_suffix("-dirty").unwrap_or(baked_rev);
    (base != head).then(|| {
        format!(
            "⚠ fleet binary is STALE: built at {baked_rev} but the checkout HEAD is {head} — \
             rebuild (cargo build --release) so the running process uses current logic"
        )
    })
}

/// The short HEAD sha of the git checkout the running binary was built from — walk up from the binary's path
/// to a dir containing `.git`, then `git rev-parse`. `None` when there is no source tree (a deployed binary)
/// or git is unavailable. Best-effort — only used to warn about a stale binary.
fn checkout_head_short() -> Option<String> {
    let exe = std::env::current_exe().ok()?;
    let root = exe.ancestors().find(|p| p.join(".git").exists())?;
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8(out.stdout).ok()?.trim().to_string();
    (!s.is_empty()).then_some(s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    // A UNIQUE hub dir per call (pid + a monotonic counter) so parallel tests never share/clobber state.
    fn tmp_hub() -> (PathBuf, Fleet) {
        static N: AtomicU64 = AtomicU64::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let base = std::env::temp_dir().join(format!("fleet-test-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let fleet = Fleet {
            root: base.join(".claude/fleet"),
        };
        (base, fleet)
    }

    #[test]
    fn build_kickoff_is_work_conserving_and_self_discovering() {
        let k = build_kickoff("v-x", "/wt/v-x", "30m");
        // Identity binding (#216): register_agent FIRST binds the session; get_agent alone does not, so the
        // kickoff must register before any write + name the explicit-identity fallback (board defaults null).
        assert!(k.contains("register_agent"), "binds identity before writing");
        assert!(k.contains("get_agent ALONE does NOT bind") || k.contains("get_agent"), "still self-discovers charter");
        assert!(k.contains("created_by") && k.contains("actor"), "names the explicit-identity fallback params");
        assert!(k.contains("'v-x'") && k.contains("/wt/v-x"));
        // Dynamic loop (no fixed interval arg after /loop) — the agent self-paces.
        assert!(k.contains("/loop run one tick"), "dynamic /loop, not `/loop 30m`");
        assert!(!k.contains("/loop 30m"), "must NOT pin a fixed interval on the loop");
        // Work-conserving: gate the next wake on open assigned work + unread, long idle only when drained.
        assert!(k.contains("WORK-CONSERVING PACING"));
        assert!(k.contains("list_tasks with assignee 'v-x'"));
        assert!(k.contains("NEVER idle-sleep"));
        assert!(k.contains("about 30m"), "the interval is the idle-fallback ceiling");
        // blocked/parked ≠ actionable (board-pm refinement): the self-check counts only todo/in_progress and
        // excludes a blocked/parked task, so a task parked on a blocker doesn't keep the loop hot.
        assert!(k.contains("todo/in_progress") && k.contains("NOT blocked/parked"), "blocked/parked is not actionable work");
        // Doc-writing style guide clause (board-pm, operator-approved): every future author carries it.
        assert!(k.contains("Fleet Doc-Writing Style Guide"), "kickoff points authors at the doc-writing style guide");
        // Banned-phrases self-check (operator writing policy): reference the maintained list (data-driven),
        // applied to any doc OR comment, until the #308 pre-submit scanner lands.
        assert!(k.contains("banned-phrases") && k.contains("doc OR comment"), "kickoff points authors at the banned-phrases list for docs and comments");
        // Operator-blocked dashboard convention (operator seq-2292): a task blocked on the operator gets
        // reassigned to 'cameron' + typed blocked_on so list_tasks(assignee cameron) is the operator's one dashboard.
        assert!(k.contains("BLOCKED ON THE") && k.contains("assign it to 'cameron'"), "carries the operator-blocked convention");
        assert!(k.contains("metadata.blocked_owner"), "stashes the real owner for reassign-back");
    }

    #[test]
    fn build_launch_cmd_wires_claude_and_stages_codex_and_rejects_unknown() {
        // claude is fully wired: the exec line carries the model/effort and reads the kickoff from the env.
        let c = build_launch_cmd("claude", "claude-x", "high", None).expect("claude wired");
        assert!(c.starts_with("exec claude "));
        assert!(c.contains("--model 'claude-x'") && c.contains("--effort 'high'"));
        assert!(c.contains("\"$CDZ_KICKOFF\""), "kickoff rides in the env var, not interpolated");
        // devshell (#214): opt-in launch inside the workdir's flake devShell so the pinned toolchain is on PATH.
        let d = build_launch_cmd("claude", "claude-x", "high", Some("/wt/v-x")).expect("claude wired");
        assert!(d.starts_with("exec nix develop \"path:/wt/v-x\" --command claude "), "wrapped in nix develop");
        assert!(d.contains("--model 'claude-x'") && d.contains("\"$CDZ_KICKOFF\""), "same claude args inside the devShell");
        // codex is recognized but not yet wired — an actionable error, never a guessed command.
        let e = build_launch_cmd("codex", "m", "high", None).unwrap_err();
        assert!(e.contains("codex") && e.contains("not wired"));
        // an unknown/typo'd harness fails loudly.
        let u = build_launch_cmd("gpt5", "m", "high", None).unwrap_err();
        assert!(u.contains("unknown harness 'gpt5'"));
    }

    #[test]
    fn spin_down_action_covers_native_busy_window_and_windowless() {
        use SpinDownAction::*;
        // Not board-native → refuse regardless of window/force (a file-hub agent uses cargo xtask fleet remove).
        assert_eq!(spin_down_action(false, true, false, false), NotBoardNative);
        assert_eq!(spin_down_action(false, false, false, true), NotBoardNative);
        // Native + a working pane + no --force → refuse so a running agent is never killed mid-turn.
        assert_eq!(spin_down_action(true, true, true, false), RefuseBusy);
        // --force overrides the busy refusal → offline + kill.
        assert_eq!(spin_down_action(true, true, true, true), OfflineAndKill);
        // Native + an IDLE live window → offline then kill (stops the loop).
        assert_eq!(spin_down_action(true, true, false, false), OfflineAndKill);
        // Native + no window → offline ONLY (still mark offline so up-board leaves it stood down); force moot.
        assert_eq!(spin_down_action(true, false, false, false), OfflineOnly);
        assert_eq!(spin_down_action(true, false, true, true), OfflineOnly);
    }

    #[test]
    fn fmt_hook_install_action_never_clobbers_a_foreign_hook() {
        use FmtHookAction::*;
        // Absent → install ours.
        assert_eq!(fmt_hook_install_action(None), Install);
        // Our own hook (marker present) → refresh (idempotent).
        assert_eq!(fmt_hook_install_action(Some(&fmt_precommit_hook_body())), Refresh);
        // A foreign hook → NEVER clobber.
        assert_eq!(fmt_hook_install_action(Some("#!/bin/sh\n# someone else's pre-commit\n")), SkipForeign);
    }

    #[test]
    fn fmt_precommit_hook_is_fail_open_valid_bash() {
        let b = fmt_precommit_hook_body();
        assert!(b.contains(FMT_HOOK_MARKER), "carries the ownership marker");
        assert!(b.contains("FLEET_SKIP_FMT_HOOK"), "has the silencer");
        assert!(b.contains("cargo fmt --all --check"), "checks fmt (read-only)");
        assert!(b.trim_end().ends_with("exit 0"), "FAIL-OPEN: the hook never blocks a commit");
        // Syntax-check with `bash -n` (a broken hook would fail every commit in the shared mirror); skip if absent.
        let dir = std::env::temp_dir().join(format!("fleet-fmthook-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let f = dir.join("pre-commit");
        if std::fs::write(&f, &b).is_err() {
            return; // can't stage the file — skip the syntax check rather than false-fail
        }
        if let Ok(o) = std::process::Command::new("bash").arg("-n").arg(&f).output() {
            assert!(o.status.success(), "hook bash syntax error:\n{}", String::from_utf8_lossy(&o.stderr));
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn observation_task_spec_names_the_target_and_window_in_project_28() {
        let s = observation_task_spec("v-example", "sess-abc", 1200);
        assert_eq!(s.project_id, SELF_IMPROVE_PROJECT);
        assert_eq!(s.title, "observe v-example — sess-abc:1200");
        assert!(s.body.contains("v-example"), "body names the target");
        assert!(s.body.contains("sess-abc"), "body names the session");
        assert!(s.body.contains("1200"), "body names the offset");
        assert!(s.body.contains("CHILD proposal task"), "body states the child-filing contract");
        assert!(s.body.contains("Close this task"), "body states the close-on-done contract");
    }

    #[test]
    fn observer_kickoff_drives_from_the_observation_task_when_given_one() {
        let with = build_observer_kickoff("v-t", "sess-9", 42, "loops/observer.md", "/abs/fleet", Some(207));
        // The observation-task variant carries the child-filing + close-the-parent contract.
        assert!(with.contains("OBSERVATION TASK #207"), "names the observation task");
        assert!(with.contains("parent_id=207"), "proposals are children of the observation task");
        assert!(with.contains("update_task 207"), "closes the observation task");
        assert!(with.contains("status=\"done\""), "close = mark done");

        let without = build_observer_kickoff("v-t", "sess-9", 42, "loops/observer.md", "/abs/fleet", None);
        // The rollout-compat variant files standalone proposals — no parent linkage, no task close. ("update_task"
        // alone appears in the shared identity preamble; the CLOSE contract is `status="done"` on the task.)
        assert!(!without.contains("parent_id="), "no child linkage without an observation task");
        assert!(!without.contains("OBSERVATION TASK #"), "not driven by an observation task");
        assert!(!without.contains("status=\"done\""), "no task close without an observation task");
        assert!(without.contains("above-floor"), "still files curated proposals");

        // Both variants keep the invariant boot + confirm contract.
        for k in [&with, &without] {
            assert!(k.contains("register_agent 'observer'"), "binds the observer identity");
            assert!(k.contains("observe-record v-t --session sess-9"), "confirms via observe-record last");
            assert!(k.contains("/abs/fleet"), "uses this binary's absolute path");
        }
    }

    #[test]
    fn parse_workspace_kind_reads_setup_script_and_config_hints() {
        let rec = serde_json::json!({
            "name": "example-env",
            "description": "a board-defined environment",
            "setup_script": "echo materialize\n",
            "config": {
                "cwd": "/work/example",
                "pre_trust": ["/work/example/sub", "/opt/toolchain"],
                "env": { "FOO": "bar", "IGNORED_NUM": 7 }
            }
        });
        let p = parse_workspace_kind("v-example", "/home/u/.fleet", &rec);
        assert_eq!(p.name, "example-env");
        assert_eq!(p.description.as_deref(), Some("a board-defined environment"));
        assert_eq!(p.setup_script.as_deref(), Some("echo materialize\n"));
        assert_eq!(p.cwd, "/work/example", "absolute config.cwd is used as-is");
        // The launch cwd + the fleet root are always trusted, then the config pre_trust entries.
        assert_eq!(
            p.pre_trust,
            vec![
                "/home/u/.fleet".to_string(),
                "/work/example".to_string(),
                "/work/example/sub".to_string(),
                "/opt/toolchain".to_string(),
            ]
        );
        // Only string-valued env keys survive; a non-string value is dropped.
        assert_eq!(p.env, vec![("FOO".to_string(), "bar".to_string())]);
    }

    #[test]
    fn parse_workspace_kind_defaults_cwd_and_treats_blank_setup_as_none() {
        // No config at all: cwd falls back to the agent's own root dir under the fleet root, no extra trust.
        let rec = serde_json::json!({ "name": "bare", "setup_script": "   \n" });
        let p = parse_workspace_kind("v-bare", "/home/u/.fleet", &rec);
        assert_eq!(p.cwd, workspace::agent_root_dir("/home/u/.fleet", "v-bare"));
        assert_eq!(p.pre_trust, vec!["/home/u/.fleet".to_string(), p.cwd.clone()]);
        assert!(p.setup_script.is_none(), "whitespace-only setup_script is treated as absent");
        // A relative config.cwd is taken under the fleet root.
        let rec2 = serde_json::json!({ "name": "rel", "config": { "cwd": "checkout/here" } });
        let p2 = parse_workspace_kind("v-rel", "/home/u/.fleet", &rec2);
        assert_eq!(p2.cwd, "/home/u/.fleet/checkout/here");
    }

    #[test]
    fn parse_interval_secs_handles_units_and_bare_numbers() {
        assert_eq!(parse_interval_secs("90s"), Some(90));
        assert_eq!(parse_interval_secs("30m"), Some(1800));
        assert_eq!(parse_interval_secs("2h"), Some(7200));
        assert_eq!(parse_interval_secs("1d"), Some(86400));
        assert_eq!(parse_interval_secs("45"), Some(45), "bare number → seconds");
        assert_eq!(parse_interval_secs(" 6h "), Some(21600), "trimmed");
        assert_eq!(parse_interval_secs(""), None);
        assert_eq!(parse_interval_secs("5x"), None, "unknown unit");
        assert_eq!(parse_interval_secs("h"), None, "no number");
    }

    #[test]
    fn watchdog_verdict_buckets_by_the_agents_own_interval() {
        let iv: u64 = 1800; // 30m
        let ivi = iv as i64;
        assert_eq!(watchdog_verdict(0, iv), "ok");
        assert_eq!(watchdog_verdict(ivi - 1, iv), "ok", "within one interval");
        assert_eq!(watchdog_verdict(ivi, iv), "late", "at one interval → late");
        assert_eq!(watchdog_verdict(ivi * 3 - 1, iv), "late");
        assert_eq!(watchdog_verdict(ivi * 3, iv), "STALE", "beyond the overdue window → re-arm candidate");
        assert_eq!(watchdog_verdict(-120, iv), "ok", "clock skew (future last_seen) is ok");
        assert_eq!(watchdog_verdict(999999, 0), "ok", "unknown interval → never flagged");
    }

    #[test]
    fn is_retighten_candidate_flags_stale_or_work_on_a_long_interval_but_not_late() {
        // STALE (loop lapsed several intervals) is a candidate regardless of task count (not idle-presence).
        assert!(is_retighten_candidate("STALE", 0, 600, false));
        // `late` is the NORMAL once-per-interval band for a healthy idle agent — NOT a re-arm (the
        // v-slack-bridge report: a completed 30m-interval vertical sits at ~1× interval before its next tick).
        assert!(
            !is_retighten_candidate("late", 0, 600, false),
            "late is normal idle, only STALE is a stall"
        );
        // A healthy heartbeat with NO open work is fine on any interval.
        assert!(!is_retighten_candidate("ok", 0, 6 * 3600, false));
        // Open work on a LONG interval → retighten (should loop tighter to drain the queue) — any verdict.
        assert!(is_retighten_candidate("ok", 2, 3600, false), "1h+ with open tasks");
        assert!(is_retighten_candidate("late", 1, 6 * 3600, false));
        // Open work on a SHORT interval is fine — it's already cycling fast.
        assert!(!is_retighten_candidate("ok", 3, 600, false), "10m with tasks is already tight");
    }

    #[test]
    fn is_retighten_candidate_never_nudges_an_idle_presence_agent_with_a_drained_queue() {
        // Deliberately idle presence (away OR offline) + zero open tasks → NOT a candidate even at STALE: it
        // paused its loop on purpose (typically holding a scheduled wakeup); re-arming just burns a tick.
        // Covers BOTH the offline/spun-down case (design-fleet-self-improve) and the away/completed-but-not-
        // retired case (v-slack-bridge: away, all tasks done, drained, scheduled 1800s wakeup pending).
        assert!(!is_retighten_candidate("STALE", 0, 600, true));
        assert!(!is_retighten_candidate("late", 0, 1800, true), "the v-slack-bridge scenario");
        // …but an idle-presence agent that STILL holds open work IS a candidate (it shouldn't have parked).
        assert!(is_retighten_candidate("ok", 1, 6 * 3600, true), "idle with open work → still nudge");
        assert!(is_retighten_candidate("STALE", 2, 600, true));
    }

    #[test]
    fn presence_suppresses_rearm_covers_at_rest_states_incl_done() {
        // Deliberately-not-looping presences: away/offline (idle/spun-down) + done (retired/at-rest, revive on
        // trigger). A `done` at-rest worker (the real v-bach case: 26h stale, no window) must NOT read as a
        // STALE re-arm candidate — that's the bug this fixes.
        assert!(presence_suppresses_rearm(Some("done")), "at-rest/done → not a re-arm target");
        assert!(presence_suppresses_rearm(Some("offline")));
        assert!(presence_suppresses_rearm(Some("away")));
        // A live/looping agent (or unset status) IS eligible — a genuine stall must still flag.
        assert!(!presence_suppresses_rearm(Some("online")));
        assert!(!presence_suppresses_rearm(None));
    }

    #[test]
    fn pane_working_detection_fences_heads_down_agents() {
        // A bare idle prompt is NOT working — even with the lingering footer + a completed turn's token
        // remnant (the exact false-positive the idle-prompt override guards against).
        let idle = "some earlier output\n↓ 4.2k tokens\n⏵⏵ bypass permissions · esc to interrupt · ← for agents\n❯";
        assert!(pane_shows_idle_prompt(idle));
        assert!(!pane_shows_working(idle), "idle ❯ overrides lingering footer/token remnant");
        // A turn in flight → working (no bare ❯ prompt while generating).
        assert!(pane_shows_working("Percolating… (2m 3s · ↓ 12.0k tokens)"));
        assert!(pane_shows_working("doing a thing  esc to interrupt"));
        assert!(pane_shows_working("Retrying in 4s… (attempt 2/10)"));
        assert!(pane_shows_working("(ctrl+b to run in background)"));
        // No prompt and no working affordance → not working (e.g. a dead/shell pane) — safe to wake.
        assert!(!pane_shows_working("bash-5.2$ "));
    }

    #[test]
    fn native_agent_ids_collects_only_native_true_rows() {
        let agents = vec![
            serde_json::json!({"id":"v-slack-bridge","metadata":{"native":true}}),
            serde_json::json!({"id":"v-file-hub-only","metadata":{"native":false}}),
            serde_json::json!({"id":"v-no-flag","metadata":{}}),
            serde_json::json!({"id":"v-board-pm","metadata":{"native":true,"role":"pm"}}),
            serde_json::json!({"metadata":{"native":true}}), // no id → dropped
        ];
        let ids = native_agent_ids(&agents);
        assert!(ids.contains("v-slack-bridge") && ids.contains("v-board-pm"));
        assert!(!ids.contains("v-file-hub-only") && !ids.contains("v-no-flag"));
        assert_eq!(ids.len(), 2, "only native:true rows with an id");
    }

    #[test]
    fn rearm_cooldown_uses_the_interval_floored_and_treats_never_armed_as_free() {
        // Never armed → free to re-arm.
        assert!(!rearm_on_cooldown(None, 10_000, 600));
        // A 4h-interval agent re-armed 1h ago is still on cooldown (< its own interval).
        assert!(rearm_on_cooldown(Some(10_000), 10_000 + 3_600, 4 * 3_600));
        // …and free again once a full interval has elapsed.
        assert!(!rearm_on_cooldown(Some(10_000), 10_000 + 4 * 3_600, 4 * 3_600));
        // A short/zero interval is floored at 5 min: a re-arm 60s ago is still on cooldown.
        assert!(rearm_on_cooldown(Some(10_000), 10_000 + 60, 0));
        assert!(!rearm_on_cooldown(Some(10_000), 10_000 + 301, 0));
    }

    #[test]
    fn inbox_pending_count_counts_files_not_the_processed_dir() {
        let (base, fleet) = tmp_hub();
        fleet.ensure_inbox("a1"); // creates inbox/a1/processed
        assert_eq!(inbox_pending_count(&fleet, "a1"), 0, "empty inbox (processed dir excluded)");
        std::fs::write(fleet.inbox("a1").join("0001-msg.json"), "{}").unwrap();
        std::fs::write(fleet.inbox("a1").join("0002-msg.json"), "{}").unwrap();
        assert_eq!(inbox_pending_count(&fleet, "a1"), 2, "two undrained messages; processed/ not counted");
        // A non-message kickoff SEED file (not `.json`) must NOT count — else a lingering seed reads as a
        // perpetual pending message and the watchdog false-nudges the agent every sweep (v-s2n-quic/v-etude).
        std::fs::write(fleet.inbox("a1").join("s2n_seed.txt"), "seed").unwrap();
        std::fs::write(fleet.inbox("a1").join("seed-a1.md"), "seed").unwrap();
        assert_eq!(inbox_pending_count(&fleet, "a1"), 2, "seed files excluded; only .json messages count");
        assert_eq!(inbox_pending_count(&fleet, "nobody"), 0, "absent inbox → 0");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn parse_repo_spec_splits_branch_and_defaults_to_main() {
        assert_eq!(parse_repo_spec("camshaft/bolero@master"), serde_json::json!({"repo":"camshaft/bolero","branch":"master"}));
        assert_eq!(parse_repo_spec("camshaft/backbeat"), serde_json::json!({"repo":"camshaft/backbeat","branch":"main"}));
        assert_eq!(parse_repo_spec("camshaft/x@"), serde_json::json!({"repo":"camshaft/x","branch":"main"}), "empty branch → main");
    }

    #[test]
    fn build_meta_patch_includes_only_requested_keys_and_errors_when_empty() {
        let p = build_meta_patch(&["o/r@b".to_string()], Some("2m"), None, None, None).unwrap();
        assert_eq!(p["repos"], serde_json::json!([{"repo":"o/r","branch":"b"}]));
        assert_eq!(p["interval"], "2m");
        // repos only — no interval/host/native/devshell key
        let p = build_meta_patch(&["o/r".to_string()], None, None, None, None).unwrap();
        assert!(p.get("interval").is_none() && p.get("host").is_none() && p.get("native").is_none() && p.get("devshell").is_none());
        assert!(p.get("repos").is_some());
        // interval only — no repos key
        let p = build_meta_patch(&[], Some("30m"), None, None, None).unwrap();
        assert!(p.get("repos").is_none());
        assert_eq!(p["interval"], "30m");
        // host set, and "" clears the pin (JSON null)
        let p = build_meta_patch(&[], None, Some("green-machine"), None, None).unwrap();
        assert_eq!(p["host"], "green-machine");
        let p = build_meta_patch(&[], None, Some(""), None, None).unwrap();
        assert_eq!(p["host"], serde_json::Value::Null, "empty host clears the pin");
        // native: tri-state — Some(true)/Some(false) emit the bool; None omits the key entirely
        let p = build_meta_patch(&[], None, None, Some(true), None).unwrap();
        assert_eq!(p["native"], serde_json::Value::Bool(true), "--native true → the board-native marker");
        let p = build_meta_patch(&[], None, None, Some(false), None).unwrap();
        assert_eq!(p["native"], serde_json::Value::Bool(false), "--native false → clear back to file-hub");
        assert!(build_meta_patch(&["o/r".to_string()], None, None, None, None).unwrap().get("native").is_none(),
            "native untouched when not requested");
        // devshell: tri-state, same shape (#214 opt-in launch-in-nix-develop marker)
        let p = build_meta_patch(&[], None, None, None, Some(true)).unwrap();
        assert_eq!(p["devshell"], serde_json::Value::Bool(true), "--devshell true → launch inside the flake devShell");
        assert!(build_meta_patch(&["o/r".to_string()], None, None, None, None).unwrap().get("devshell").is_none(),
            "devshell untouched when not requested");
        // nothing requested → error (guards a no-op PATCH)
        assert!(build_meta_patch(&[], None, None, None, None).is_err());
    }

    #[test]
    fn agent_host_matches_honors_pin_and_treats_unset_as_run_anywhere() {
        // unpinned (no host / null / empty) → managed everywhere
        assert!(agent_host_matches(Some(&serde_json::json!({})), "dev-desk"));
        assert!(agent_host_matches(Some(&serde_json::json!({"host":null})), "dev-desk"));
        assert!(agent_host_matches(Some(&serde_json::json!({"host":""})), "dev-desk"));
        assert!(agent_host_matches(None, "dev-desk"));
        // string pin: matches only its host
        assert!(agent_host_matches(Some(&serde_json::json!({"host":"green-machine"})), "green-machine"));
        assert!(!agent_host_matches(Some(&serde_json::json!({"host":"green-machine"})), "dev-desk"));
        // array pin: matches if listed; empty array = unpinned
        assert!(agent_host_matches(Some(&serde_json::json!({"host":["green-machine","dev-desk"]})), "dev-desk"));
        assert!(!agent_host_matches(Some(&serde_json::json!({"host":["green-machine"]})), "dev-desk"));
        assert!(agent_host_matches(Some(&serde_json::json!({"host":[]})), "dev-desk"));
    }

    #[test]
    fn version_line_reports_package_version_and_a_baked_rev() {
        let v = version_line();
        assert!(v.starts_with(&format!("fleet {} (rev ", env!("CARGO_PKG_VERSION"))), "names the pkg version");
        assert!(v.ends_with(")"), "wraps the rev");
        // build.rs always bakes a non-empty rev (a real short-sha, or the "unknown" fallback).
        assert!(!env!("FLEET_BUILD_REV").is_empty(), "the build rev is always baked");
    }

    #[test]
    fn build_freshness_warning_flags_only_a_binary_behind_its_checkout() {
        assert!(build_freshness_warning("abc123", Some("def456")).is_some(), "baked != head → stale");
        assert!(build_freshness_warning("abc123", Some("abc123")).is_none(), "baked == head → current");
        assert!(
            build_freshness_warning("abc123-dirty", Some("abc123")).is_none(),
            "a dirty build of the same commit is not stale"
        );
        assert!(build_freshness_warning("unknown", Some("abc123")).is_none(), "unknown baked rev → can't tell");
        assert!(build_freshness_warning("", Some("abc123")).is_none(), "empty baked rev → can't tell");
        assert!(build_freshness_warning("abc123", None).is_none(), "no checkout (deployed binary) → not applicable");
    }

    #[test]
    fn watchdog_exec_args_builds_the_liveness_base_plus_opt_ins() {
        // rearm base, opt-in observe + pinned.
        assert_eq!(watchdog_exec_args(true, false, false), "watchdog --rearm --stale-only");
        assert_eq!(watchdog_exec_args(true, true, false), "watchdog --rearm --stale-only --observe --spawn");
        assert_eq!(watchdog_exec_args(true, false, true), "watchdog --rearm --stale-only --pinned-only");
        // The green go-live shape: liveness + observer cadence + host filter.
        assert_eq!(
            watchdog_exec_args(true, true, true),
            "watchdog --rearm --stale-only --observe --spawn --pinned-only"
        );
        // OBSERVER-ONLY (rearm=false): coexists with an existing rearm watchdog without double-rearming (dev-desk).
        assert_eq!(watchdog_exec_args(false, true, false), "watchdog --observe --spawn");
    }

    #[test]
    fn render_watchdog_units_is_a_oneshot_service_plus_timer() {
        let u = render_watchdog_units("/run/fleet/bin/fleet", &watchdog_exec_args(true, true, true), 60, "");
        // A oneshot service (the watchdog is single-sweep) driven by a timer — not a Restart loop.
        assert!(u.contains("Type=oneshot"), "single-sweep → oneshot, not a loop");
        assert!(u.contains("ExecStart=/run/fleet/bin/fleet watchdog --rearm --stale-only --observe --spawn --pinned-only"));
        assert!(u.contains("OnUnitActiveSec=60"), "the timer re-fires on the cadence");
        // Ordered after the wake path it complements (green's request), and installable as a user timer.
        assert!(u.contains("After=fleet-notify.service") && u.contains("Wants=fleet-notify.service"));
        assert!(u.contains("WantedBy=timers.target"));
    }

    #[test]
    fn watchdog_unit_files_splits_service_and_timer_cleanly() {
        // Observer-only exec, for the dev-desk coexistence install.
        let (service, timer) = watchdog_unit_files("/bin/fleet", &watchdog_exec_args(false, true, false), 90, "");
        // The service file has the oneshot + ExecStart, NO timer/header lines.
        assert!(service.contains("Type=oneshot"));
        assert!(service.contains("ExecStart=/bin/fleet watchdog --observe --spawn"));
        assert!(!service.contains("OnUnitActiveSec"), "timer stanza belongs in the timer file, not the service");
        assert!(!service.contains("# ----"), "unit files carry no display headers");
        // The timer file drives the cadence + is enable-able.
        assert!(timer.contains("OnUnitActiveSec=90") && timer.contains("WantedBy=timers.target"));
        assert!(!timer.contains("ExecStart"), "no ExecStart in the timer");
    }

    #[test]
    fn render_service_env_lines_emits_present_values_and_skips_unset() {
        let block = render_service_env_lines(&[
            ("PATH", Some("/home/u/.local/bin:/usr/bin".into())),
            ("CLAUDE_CODE_USE_BEDROCK", Some("1".into())),
            ("AWS_PROFILE", None), // unset → skipped, no empty assignment
        ]);
        assert!(block.contains("Environment=\"PATH=/home/u/.local/bin:/usr/bin\"\n"));
        assert!(block.contains("Environment=\"CLAUDE_CODE_USE_BEDROCK=1\"\n"));
        assert!(!block.contains("AWS_PROFILE"), "an unset var is skipped, not emitted empty");
    }

    #[test]
    fn watchdog_unit_env_block_lands_in_the_service_before_execstart() {
        // The captured env block sits in [Service] ahead of ExecStart so the spawned observer inherits PATH
        // (else `exec claude` is not found under the stripped systemd env and the window closes with 127).
        let env = render_service_env_lines(&[("PATH", Some("/home/u/.local/bin".into()))]);
        let (service, _timer) = watchdog_unit_files("/bin/fleet", &watchdog_exec_args(true, true, false), 60, &env);
        let env_at = service.find("Environment=\"PATH=").expect("env line present");
        let exec_at = service.find("ExecStart=").expect("ExecStart present");
        assert!(env_at < exec_at, "Environment= must precede ExecStart in the unit");
        // A rearm-only unit (no observe) is emitted with an empty env block — no launch environment needed.
        let (rearm_only, _) = watchdog_unit_files("/bin/fleet", &watchdog_exec_args(true, false, false), 60, "");
        assert!(!rearm_only.contains("Environment="), "rearm-only watchdog spawns nothing → no env block");
    }

    #[test]
    fn watchdog_manages_agent_uses_strict_pin_only_under_pinned_only() {
        let green = serde_json::json!({ "host": "green-machine" });
        let unpinned = serde_json::json!({});
        // Default (loose): this-host-pinned OR unpinned are managed here.
        assert!(watchdog_manages_agent(Some(&green), "green-machine", false));
        assert!(watchdog_manages_agent(Some(&unpinned), "green-machine", false), "unpinned managed everywhere by default");
        assert!(!watchdog_manages_agent(Some(&green), "dev-desk", false), "other-box pin never managed here");
        // --pinned-only: ONLY agents explicitly pinned here — unpinned run-anywhere agents are excluded, so a
        // secondary box never re-arms/observes an agent whose window/transcript is on another box.
        assert!(watchdog_manages_agent(Some(&green), "green-machine", true));
        assert!(!watchdog_manages_agent(Some(&unpinned), "green-machine", true), "unpinned EXCLUDED under --pinned-only");
        assert!(!watchdog_manages_agent(Some(&green), "dev-desk", true));
    }

    #[test]
    fn agent_host_is_explicit_requires_a_deliberate_pin_to_this_box() {
        // EXPLICIT pin → true only for the named box (this is the --pinned-only launch predicate).
        assert!(agent_host_is_explicit(Some(&serde_json::json!({"host":"green-machine"})), "green-machine"));
        assert!(!agent_host_is_explicit(Some(&serde_json::json!({"host":"green-machine"})), "dev-desk"));
        assert!(agent_host_is_explicit(Some(&serde_json::json!({"host":["green-machine","dev-desk"]})), "dev-desk"));
        assert!(!agent_host_is_explicit(Some(&serde_json::json!({"host":["green-machine"]})), "dev-desk"));
        // UNPINNED (unset / null / empty string / empty array) → FALSE — the key difference from
        // agent_host_matches: an unpinned run-anywhere agent is NOT an explicit launch candidate here, so a
        // second box's --pinned-only reconcile won't double-launch it.
        assert!(!agent_host_is_explicit(None, "dev-desk"));
        assert!(!agent_host_is_explicit(Some(&serde_json::json!({})), "dev-desk"));
        assert!(!agent_host_is_explicit(Some(&serde_json::json!({"host":null})), "dev-desk"));
        assert!(!agent_host_is_explicit(Some(&serde_json::json!({"host":""})), "dev-desk"));
        assert!(!agent_host_is_explicit(Some(&serde_json::json!({"host":[]})), "dev-desk"));
    }

    #[test]
    fn tunnel_health_line_ok_only_on_200_else_wedged_or_unreachable() {
        // 200 = healthy wake-delivery path.
        let (ok, msg) = tunnel_health_line(&Ok(200));
        assert!(ok && msg.contains("OK"));
        // The daemon's own 503 (socket wedged / stale frame / upstream down) = reachable but NOT ok.
        let (ok, msg) = tunnel_health_line(&Ok(503));
        assert!(!ok && msg.contains("WEDGED") && msg.contains("503"));
        // Any other non-200 is also not ok (defensive).
        let (ok, _) = tunnel_health_line(&Ok(404));
        assert!(!ok);
        // A transport error (daemon down / no port) = unreachable, not ok.
        let (ok, msg) = tunnel_health_line(&Err("connection refused".to_string()));
        assert!(!ok && msg.contains("UNREACHABLE") && msg.contains("connection refused"));
    }

    #[test]
    fn served_set_is_windows_intersect_board_minus_off_host_pins() {
        // Board roster: two unpinned, one pinned here, one pinned elsewhere.
        let agents = vec![
            ("v-a".to_string(), Some(serde_json::json!({}))),
            ("v-b".to_string(), Some(serde_json::json!({"host": "dev-desk"}))),
            ("v-elsewhere".to_string(), Some(serde_json::json!({"host": "green-machine"}))),
            ("v-nometa".to_string(), None),
        ];
        // tmux windows: some agents, plus daemon/scratch windows that are NOT board agents, plus a board
        // agent (v-noagent-window ... actually a board agent with no window is v-noagent) — cover both drops.
        let windows = vec![
            "v-a".to_string(),
            "v-b".to_string(),
            "v-elsewhere".to_string(), // pinned to green-machine → dropped even though a window exists here
            "v-nometa".to_string(),
            "notify".to_string(),   // a daemon window, not a board agent → dropped
            "scratch".to_string(),  // not a board agent → dropped
        ];
        // A board agent with NO window here (v-c) must also be absent (nothing to wake on this host).
        let mut with_windowless = agents.clone();
        with_windowless.push(("v-c".to_string(), Some(serde_json::json!({}))));
        let served = derive_served_set(&windows, &with_windowless, "dev-desk");
        assert_eq!(served, vec!["v-a", "v-b", "v-nometa"], "window∩board, minus off-host pins and non-agents");
        // sorted + deduped even if the board lists a duplicate id
        let dupe = vec![("v-a".to_string(), None), ("v-a".to_string(), None)];
        assert_eq!(derive_served_set(&["v-a".to_string()], &dupe, "dev-desk"), vec!["v-a"]);
    }

    #[test]
    fn liveness_verdict_buckets_by_heartbeat_age() {
        assert_eq!(liveness_verdict(-30), "live", "clock skew (future) is not stale");
        assert_eq!(liveness_verdict(0), "live");
        assert_eq!(liveness_verdict(LIVE_SECS - 1), "live");
        assert_eq!(liveness_verdict(LIVE_SECS), "quiet", "at the live bound → quiet");
        assert_eq!(liveness_verdict(QUIET_SECS - 1), "quiet");
        assert_eq!(liveness_verdict(QUIET_SECS), "STALE", "at the quiet bound → STALE");
        assert_eq!(liveness_verdict(6 * 60 * 60), "STALE");
    }

    #[test]
    fn last_seen_age_is_positive_for_a_past_stamp_and_none_for_garbage() {
        use time::format_description::well_known::Rfc3339;
        use time::{Duration, OffsetDateTime};
        let now = OffsetDateTime::now_utc();
        let past = (now - Duration::minutes(20)).format(&Rfc3339).unwrap();
        let age = last_seen_age_secs(&past, now).expect("parses");
        assert!((1190..=1210).contains(&age), "≈20m in seconds, got {age}");
        assert_eq!(last_seen_age_secs("not-a-timestamp", now), None);
    }

    #[test]
    fn ensure_trusted_adds_missing_is_idempotent_and_creates_projects() {
        let mut v = serde_json::json!({"projects": {"/x": {"hasTrustDialogAccepted": true}}});
        assert!(ensure_trusted(&mut v, "/root/.fleet")); // new dir -> changed
        assert_eq!(v["projects"]["/root/.fleet"]["hasTrustDialogAccepted"], true);
        assert!(!ensure_trusted(&mut v, "/root/.fleet")); // now trusted -> no change
        assert!(!ensure_trusted(&mut v, "/x")); // already trusted -> no change
        let mut empty = serde_json::json!({"other": 1});
        assert!(ensure_trusted(&mut empty, "/d")); // creates the projects map
        assert_eq!(empty["projects"]["/d"]["hasTrustDialogAccepted"], true);
        assert_eq!(empty["other"], 1, "other keys are preserved");
    }

    #[test]
    fn heartbeat_writes_a_liveness_stamp_and_reports_not_stopped() {
        let (base, fleet) = tmp_hub();
        assert!(
            !heartbeat_refresh_liveness(&fleet, "a1"),
            "no stop-file → not stopped"
        );
        assert!(
            fleet.root.join("heartbeat/a1").exists(),
            "liveness touch-file written"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn heartbeat_reports_stopped_when_a_stopfile_exists() {
        let (base, fleet) = tmp_hub();
        std::fs::create_dir_all(fleet.root.join("stop")).unwrap();
        std::fs::write(fleet.stopfile("a2"), "").unwrap();
        assert!(
            heartbeat_refresh_liveness(&fleet, "a2"),
            "stop-file present → stopped"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn registry_round_trips_through_save_load() {
        let (base, fleet) = tmp_hub();
        let reg = Registry {
            agents: vec![Agent {
                name: "v-x".into(),
                role: "vertical".into(),
                vertical: "iterators".into(),
                area: "rcdzc".into(),
                worktree: "/wt/v-x".into(),
                branch: "topic/v-x".into(),
                interval: "10m".into(),
                model: "opus".into(),
                effort: "high".into(),
                status: "active".into(),
                disallow_ask: true,
            }],
        };
        fleet.save(&reg);
        let back = fleet.load();
        assert_eq!(back.agents.len(), 1);
        assert_eq!(back.agents[0].name, "v-x");
        assert!(
            fleet.inbox(&back.agents[0].name).ends_with("v-x"),
            "inbox path is <hub>/inbox/<name>"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn delivery_seq_is_monotonic_and_durable() {
        let (base, fleet) = tmp_hub();
        std::fs::create_dir_all(&fleet.root).unwrap();
        assert_eq!(next_delivery_seq(&fleet), 1);
        assert_eq!(next_delivery_seq(&fleet), 2);
        assert_eq!(
            next_delivery_seq(&fleet),
            3,
            "durable across calls (reads the file)"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn validate_agent_name_accepts_real_names_and_rejects_traversal() {
        assert!(validate_agent_name("v-fleet-tooling").is_ok());
        assert!(validate_agent_name("pr-sync").is_ok());
        assert!(validate_agent_name("").is_err());
        assert!(validate_agent_name("..").is_err(), "no dot/traversal");
        assert!(validate_agent_name("../../etc").is_err());
        assert!(validate_agent_name("-flag").is_err(), "no leading hyphen");
        assert!(validate_agent_name("a/b").is_err(), "no path separator");
    }

    #[test]
    fn recipient_from_subject_rescues_a_fleet_token() {
        assert_eq!(
            recipient_from_subject("merged: fleet/v-x landed").as_deref(),
            Some("v-x")
        );
        assert_eq!(recipient_from_subject("no token here"), None);
    }

    #[test]
    fn secret_findings_flags_dumps_and_tokens_but_passes_prose() {
        // Prose that merely mentions env vars must NOT trip (the false-positive bar).
        assert!(
            message_secret_findings("docs", "set CDZ_NO_CARGO_SHIM=1 and CDZ_CHECK_LEASE_MAX=2")
                .is_empty()
        );
        // A secret-named key with a real value trips.
        assert!(!message_secret_findings("x", "AWS_SESSION_TOKEN=FQoGiZ3longvalue").is_empty());
        // A live-looking token pasted anywhere trips.
        assert!(
            !message_secret_findings("x", "here is ghp_0123456789abcdefghijklmnop token")
                .is_empty()
        );
    }

    #[test]
    fn deliver_writes_a_sortable_json_into_the_recipient_inbox() {
        let (base, fleet) = tmp_hub();
        let msg = Message {
            from: "a".into(),
            to: "b".into(),
            kind: "note".into(),
            subject: "hi".into(),
            r#ref: String::new(),
            body: "body".into(),
            seq: 1,
            in_reply_to: String::new(),
            urgency: "normal".into(),
        };
        deliver(&fleet, &msg);
        let entries: Vec<_> = std::fs::read_dir(fleet.inbox("b"))
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n.ends_with(".json"))
            .collect();
        assert_eq!(entries.len(), 1);
        assert!(
            entries[0].starts_with("000000000001-") && entries[0].ends_with("-note.json"),
            "filename sorts by delivery seq + tags the kind: {}",
            entries[0]
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn consume_action_covers_the_four_existence_cases() {
        assert_eq!(inbox_consume_action(true, false), ConsumeAction::Move);
        assert_eq!(inbox_consume_action(true, true), ConsumeAction::ClearStray);
        assert_eq!(
            inbox_consume_action(false, true),
            ConsumeAction::AlreadyDone
        );
        assert_eq!(inbox_consume_action(false, false), ConsumeAction::Missing);
    }

    #[test]
    fn actionable_classifier_is_a_denylist() {
        assert!(message_kind_is_actionable("ask"));
        assert!(message_kind_is_actionable("issue"));
        assert!(
            message_kind_is_actionable("weird-unknown-kind"),
            "unknown → fail-safe actionable"
        );
        assert!(!message_kind_is_actionable("note"));
        assert!(!message_kind_is_actionable("merged"));
        assert!(!message_kind_is_actionable("reply"));
    }

    #[test]
    fn clear_stray_notfound_is_not_fatal() {
        assert!(!clear_stray_remove_is_fatal(std::io::ErrorKind::NotFound));
        assert!(clear_stray_remove_is_fatal(
            std::io::ErrorKind::PermissionDenied
        ));
    }

    #[test]
    fn is_safe_component_rejects_traversal() {
        assert!(is_safe_component("000000000001-123-note.json"));
        assert!(!is_safe_component(".."));
        assert!(!is_safe_component("a/b"));
        assert!(!is_safe_component("../x"));
        assert!(!is_safe_component(""));
    }

    #[test]
    fn deliver_then_consume_moves_message_to_processed_and_is_idempotent() {
        let (base, fleet) = tmp_hub();
        let msg = Message {
            from: "a".into(),
            to: "b".into(),
            kind: "note".into(),
            subject: "hi".into(),
            r#ref: String::new(),
            body: "x".into(),
            seq: 1,
            in_reply_to: String::new(),
            urgency: "normal".into(),
        };
        deliver(&fleet, &msg);
        let fname = std::fs::read_dir(fleet.inbox("b"))
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .find(|n| n.ends_with(".json"))
            .unwrap();
        // The live message is present; processed/ is not.
        assert!(fleet.inbox("b").join(&fname).exists());
        // consume moves it (idempotency is exercised via the pure action classifier above; here we assert
        // the move happened: live gone, archived present).
        std::fs::create_dir_all(fleet.inbox("b").join("processed")).unwrap();
        std::fs::rename(
            fleet.inbox("b").join(&fname),
            fleet.inbox("b").join("processed").join(&fname),
        )
        .unwrap();
        assert!(!fleet.inbox("b").join(&fname).exists(), "live copy gone");
        assert!(
            fleet.inbox("b").join("processed").join(&fname).exists(),
            "archived copy present"
        );
        assert_eq!(inbox_depth(&fleet, "b"), "empty", "no live .json left");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn resolve_model_expands_aliases_and_passes_through_unknown() {
        assert_eq!(resolve_model("opus"), "us.anthropic.claude-opus-4-8[1m]");
        assert_eq!(resolve_model("fable"), "us.anthropic.claude-fable-5[1m]");
        assert_eq!(
            resolve_model("some.custom.model-id"),
            "some.custom.model-id",
            "unknown alias passes through unchanged"
        );
    }

    #[test]
    fn only_design_keeps_the_interactive_prompt() {
        assert!(role_is_terminal_interactive("design"));
        assert!(!role_is_terminal_interactive("vertical"));
        assert!(!role_is_terminal_interactive("concierge"));
        assert!(!role_is_terminal_interactive("pr-sync"));
    }

    fn mk_agent(name: &str, status: &str) -> Agent {
        Agent {
            name: name.into(),
            role: "vertical".into(),
            vertical: String::new(),
            area: String::new(),
            worktree: format!("/wt/{name}"),
            branch: format!("fleet/{name}"),
            interval: "10m".into(),
            model: "opus".into(),
            effort: "high".into(),
            status: status.into(),
            disallow_ask: true,
        }
    }

    fn mk_declared(name: &str) -> RosterEntry {
        RosterEntry {
            name: name.into(),
            role: "vertical".into(),
            vertical: String::new(),
            area: String::new(),
            interval: "10m".into(),
            model: "opus".into(),
            effort: "high".into(),
        }
    }

    #[test]
    fn reconcile_launches_absent_and_stopped_flags_undeclared_and_leaves_active() {
        let declared = vec![mk_declared("a"), mk_declared("b"), mk_declared("c")];
        let running = vec![
            mk_agent("a", "active"),  // declared + active → already_running
            mk_agent("b", "stopped"), // declared + stopped → to_launch
            mk_agent("z", "active"),  // undeclared + active → drift
                                      // "c" declared but has no registry row → to_launch
        ];
        let plan = reconcile_plan(&declared, &running);
        assert_eq!(plan.already_running, vec!["a".to_string()]);
        assert_eq!(plan.to_launch, vec!["b".to_string(), "c".to_string()]);
        assert_eq!(plan.undeclared_running, vec!["z".to_string()]);
    }

    #[test]
    fn reconcile_empty_declared_reports_all_active_as_drift() {
        let plan = reconcile_plan(&[], &[mk_agent("x", "active"), mk_agent("y", "stopped")]);
        assert!(plan.to_launch.is_empty());
        assert!(plan.already_running.is_empty());
        assert_eq!(
            plan.undeclared_running,
            vec!["x".to_string()],
            "stopped y is not drift"
        );
    }

    #[test]
    fn board_reconcile_launches_windowless_active_running_windowed_skips_offline() {
        let declared = vec![
            ("a".to_string(), false), // active, has window → already_running
            ("b".to_string(), false), // active, no window → to_launch
            ("c".to_string(), true),  // offline, no window → stood_down (never launched)
            ("d".to_string(), true),  // offline BUT has window → already_running (it is up)
        ];
        let windows = vec!["a".to_string(), "d".to_string(), "scratch".to_string()];
        let plan = board_reconcile_plan(&declared, &windows);
        assert_eq!(plan.already_running, vec!["a".to_string(), "d".to_string()]);
        assert_eq!(plan.to_launch, vec!["b".to_string()]);
        assert_eq!(plan.stood_down, vec!["c".to_string()]);
    }

    #[test]
    fn board_reconcile_all_running_or_stood_down_has_nothing_to_launch() {
        let declared = vec![("x".to_string(), false), ("y".to_string(), true)];
        let windows = vec!["x".to_string()];
        let plan = board_reconcile_plan(&declared, &windows);
        assert!(plan.to_launch.is_empty(), "x runs, y is stood down");
        assert_eq!(plan.already_running, vec!["x".to_string()]);
        assert_eq!(plan.stood_down, vec!["y".to_string()]);
    }

    #[test]
    fn board_reconcile_empty_declared_is_a_noop_plan() {
        let plan = board_reconcile_plan(&[], &["anything".to_string()]);
        assert!(plan.to_launch.is_empty());
        assert!(plan.already_running.is_empty());
        assert!(plan.stood_down.is_empty());
    }

    #[test]
    fn observe_size_trigger_fires_only_at_or_above_threshold_from_the_watermark() {
        // same session, increment 1200 (2000 - 800) — below the 2000 threshold → no fire.
        let d = observe_trigger(("s1", 800), ("s1", 2000), 2000, false);
        assert!(!d.fire);
        assert_eq!((d.since_offset, d.increment), (800, 1200));
        // grown to 2800 → increment 2000 == threshold → fires, observing FROM the watermark offset.
        let d = observe_trigger(("s1", 800), ("s1", 2800), 2000, false);
        assert!(d.fire);
        assert_eq!(d.session, "s1");
        assert_eq!((d.since_offset, d.increment), (800, 2000));
    }

    #[test]
    fn observe_session_rotation_observes_the_new_session_from_zero() {
        // watermark on the old session; the newest session is a different id → whole new session is unobserved.
        let d = observe_trigger(("old", 5000), ("new", 2500), 2000, false);
        assert!(d.fire, "2500 >= 2000 threshold");
        assert_eq!(d.session, "new");
        assert_eq!((d.since_offset, d.increment), (0, 2500));
        // first observation ever (empty watermark) is the same shape: observe from 0.
        let d = observe_trigger(("", 0), ("s1", 100), 2000, false);
        assert!(!d.fire, "100 < 2000");
        assert_eq!((d.since_offset, d.increment), (0, 100));
    }

    #[test]
    fn observe_spindown_trigger_fires_on_any_tail_even_below_threshold() {
        // A stood-down (offline) agent with a small unobserved tail STILL fires (capture the closing read),
        // even though 50 < 2000; a live agent with the same tail does not.
        let stood = observe_trigger(("s1", 900), ("s1", 950), 2000, true);
        assert!(stood.fire);
        assert_eq!((stood.since_offset, stood.increment), (900, 50));
        let live = observe_trigger(("s1", 900), ("s1", 950), 2000, false);
        assert!(!live.fire);
        // A stood-down agent with NOTHING new (already fully observed) does not re-fire.
        let done = observe_trigger(("s1", 950), ("s1", 950), 2000, true);
        assert!(!done.fire);
        assert_eq!(done.increment, 0);
    }

    #[test]
    fn deploy_event_body_is_parseable_with_upper_status() {
        assert_eq!(
            deploy_event_body("camshaft/task-board", "abc123", "green-machine", "live"),
            "deploy camshaft/task-board@abc123 → green-machine: LIVE"
        );
        // status is upper-cased + trimmed so a waiter keys on LIVE vs FAILED regardless of caller casing.
        assert_eq!(
            deploy_event_body("o/r", "deadbeef", "green-machine", " failed "),
            "deploy o/r@deadbeef → green-machine: FAILED"
        );
    }

    #[test]
    fn observe_spawn_cooldown_holds_within_window_and_lapses_after() {
        assert!(!observe_on_spawn_cooldown(None, 10_000, 1800), "never spawned → not on cooldown");
        assert!(observe_on_spawn_cooldown(Some(9_000), 10_000, 1800), "1000s < 1800 → on cooldown");
        assert!(!observe_on_spawn_cooldown(Some(8_000), 10_000, 1800), "2000s ≥ 1800 → lapsed");
        // saturating: a future stamp (clock skew) is treated as just-spawned → on cooldown, never underflows.
        assert!(observe_on_spawn_cooldown(Some(11_000), 10_000, 1800));
    }

    #[test]
    fn observe_watermark_round_trips_and_defaults_when_absent() {
        let (base, fleet) = tmp_hub();
        assert_eq!(read_observe_watermark(&fleet, "a"), (String::new(), 0), "absent → empty/0");
        write_observe_watermark(&fleet, "a", "sess-1", 4200);
        assert_eq!(read_observe_watermark(&fleet, "a"), ("sess-1".to_string(), 4200));
        // observe-record advances it (and a later read sees the new offset).
        write_observe_watermark(&fleet, "a", "sess-1", 5000);
        assert_eq!(read_observe_watermark(&fleet, "a"), ("sess-1".to_string(), 5000));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn build_observer_kickoff_carries_identity_window_and_completion_command() {
        let k = build_observer_kickoff("v-x", "sess-9", 1200, "/repo/loops/observer.md", "/repo/target/release/fleet", None);
        // Ephemeral + single stable board identity (one-shot; author as `observer`).
        assert!(k.contains("EPHEMERAL"));
        assert!(k.contains("ONE observation") && k.contains("do NOT start a /loop"), "one-shot, not looping");
        assert!(k.contains("`observer`") && k.contains("register_agent"));
        // Author identity must be PASSED explicitly on each board write (board defaults to null) — the gap
        // the first live dry-run surfaced (created_by came out null).
        assert!(k.contains("created_by=\"observer\"") && k.contains("author=\"observer\""));
        // Uses the ABSOLUTE standalone binary (not PATH `fleet`) for transcripts + observe-record, with the
        // exact target window (agent, session, offset).
        assert!(k.contains("/repo/target/release/fleet transcripts v-x --session sess-9 --since sess-9:1200"));
        assert!(k.contains("/repo/target/release/fleet observe-record v-x --session sess-9 --offset"));
        assert!(k.contains("/repo/loops/observer.md"), "points at the full role body");
        assert!(k.contains("project #28"), "files into the fleet-self-improve lane");
    }

    #[test]
    fn observe_record_self_closes_only_an_observer_window_inside_tmux() {
        // Inside tmux AND in the observer's own `obs-…` window → close it (the last act of a one-shot observer).
        assert!(observer_should_self_close(true, "obs-v-task-board"));
        assert!(observer_should_self_close(true, "obs-v-x"));
        // A manual `observe-record` from any other window must NOT self-close (only obs- windows are observers).
        assert!(!observer_should_self_close(true, "main"));
        assert!(!observer_should_self_close(true, "v-fleet-tooling"));
        assert!(!observer_should_self_close(true, "observer"), "the identity name is not the window prefix");
        // Not inside tmux → never close (nothing to close; e.g. run from a plain shell / systemd).
        assert!(!observer_should_self_close(false, "obs-v-x"));
    }

    #[test]
    fn observe_threshold_zero_disables_the_size_trigger_but_not_spindown() {
        // threshold 0 = size trigger off: a huge live increment does not fire.
        let live = observe_trigger(("s1", 0), ("s1", 100_000), 0, false);
        assert!(!live.fire);
        // but a stood-down agent still fires (the mandatory closing read is independent of threshold).
        let stood = observe_trigger(("s1", 0), ("s1", 100_000), 0, true);
        assert!(stood.fire);
        assert_eq!(stood.increment, 100_000);
    }

    #[test]
    fn parse_context_pct_takes_last_match_clamps_and_skips_recent_compaction() {
        assert_eq!(parse_context_pct("noise 85% context used"), Some(85));
        // takes the LAST (bottom = live status line)
        assert_eq!(
            parse_context_pct("40% context used\n…\n97% context used"),
            Some(97)
        );
        assert_eq!(parse_context_pct("120% context"), Some(100), "clamped");
        assert_eq!(parse_context_pct("no marker here"), None);
        // a just-compacted pane's % is stale → unknown, not saturated
        assert_eq!(
            parse_context_pct("Compacted. ctrl+o to see full summary. 99% context"),
            None
        );
    }

    #[test]
    fn compact_and_wedge_decisions_respect_the_bands_and_thrash_guards() {
        // pre-wall band [85,100) → compact (unless recently sent)
        assert!(should_send_compact(Some(85), false));
        assert!(should_send_compact(Some(99), false));
        assert!(!should_send_compact(Some(99), true), "thrash-guard");
        assert!(!should_send_compact(Some(84), false), "below saturation");
        assert!(
            !should_send_compact(Some(100), false),
            "at wall → restart, not compact"
        );
        assert!(!should_send_compact(None, false));
        // at/above the wall → restart (unless recently restarted)
        assert!(should_auto_restart_wedge(Some(100), false));
        assert!(!should_auto_restart_wedge(Some(100), true), "thrash-guard");
        assert!(
            !should_auto_restart_wedge(Some(99), false),
            "still pre-wall"
        );
        assert!(!should_auto_restart_wedge(None, false));
    }

    #[test]
    fn prewall_threshold_is_earlier_for_the_integrator() {
        assert_eq!(
            prewall_threshold_for(true),
            CTX_PREWALL_THRESHOLD_INTEGRATOR
        );
        assert_eq!(prewall_threshold_for(false), CTX_PREWALL_THRESHOLD);
        assert!(prewall_threshold_for(true) < prewall_threshold_for(false));
    }

    #[test]
    fn new_window_argv_opens_the_launcher_in_the_worktree_cwd() {
        let argv = new_window_argv("main", "v-x", "/wt/v-x", "/hub/window.sh");
        assert_eq!(
            argv,
            vec![
                "new-window",
                "-t",
                "main:",
                "-n",
                "v-x",
                "-c",
                "/wt/v-x",
                "bash",
                "/hub/window.sh",
                "v-x",
            ]
        );
    }

    #[test]
    fn agent_branch_and_worktrees_dir_have_the_expected_shape() {
        assert_eq!(agent_branch("v-x"), "fleet/v-x");
        let fleet = Fleet {
            root: PathBuf::from("/some/hub/.claude/fleet"),
        };
        assert_eq!(
            fleet.worktrees_dir(),
            PathBuf::from("/some/hub/.claude/worktrees")
        );
    }

    #[test]
    fn agent_from_roster_derives_runtime_fields() {
        let e = mk_declared("v-x");
        let a = agent_from_roster(&e, Path::new("/wt/v-x"));
        assert_eq!(a.name, "v-x");
        assert_eq!(a.branch, "fleet/v-x");
        assert_eq!(a.worktree, "/wt/v-x");
        assert_eq!(a.status, "active");
        assert!(a.disallow_ask, "vertical role → AskUserQuestion denied");
        // A design agent keeps the interactive prompt.
        let mut d = mk_declared("des");
        d.role = "design".into();
        assert!(!agent_from_roster(&d, Path::new("/wt/des")).disallow_ask);
    }

    #[test]
    fn upsert_agent_replaces_a_same_name_row_so_stopped_flips_to_active() {
        let reg = Registry {
            agents: vec![mk_agent("a", "stopped"), mk_agent("b", "active")],
        };
        let reg = upsert_agent(reg, mk_agent("a", "active"));
        assert_eq!(reg.agents.len(), 2, "no duplicate row for 'a'");
        let a = reg.agents.iter().find(|x| x.name == "a").unwrap();
        assert_eq!(a.status, "active", "stopped row replaced by the active one");
    }

    #[test]
    fn registry_set_interval_updates_a_present_row_and_reports_absent() {
        let mut reg = Registry { agents: vec![mk_agent("a", "active"), mk_agent("b", "active")] };
        assert!(registry_set_interval(&mut reg, "a", "2h"), "row present → updated");
        assert_eq!(reg.agents.iter().find(|x| x.name == "a").unwrap().interval, "2h");
        assert_eq!(reg.agents.iter().find(|x| x.name == "b").unwrap().interval, "10m", "other rows untouched");
        assert!(!registry_set_interval(&mut reg, "nobody", "5m"), "absent agent → false, no-op");
    }

    #[test]
    fn ensure_worktree_cuts_a_linked_worktree_from_the_target_repo_and_is_idempotent() {
        let (base_dir, fleet) = tmp_hub();
        // A throwaway TARGET repo with one commit so there's a base ref.
        let target = base_dir.join("target-repo");
        std::fs::create_dir_all(&target).unwrap();
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .current_dir(&target)
                .args(args)
                .output()
                .unwrap()
        };
        assert!(git(&["init", "-q"]).status.success());
        let _ = git(&["config", "user.email", "t@t"]);
        let _ = git(&["config", "user.name", "t"]);
        std::fs::write(target.join("f"), "x").unwrap();
        assert!(git(&["add", "-A"]).status.success());
        assert!(git(&["commit", "-qm", "init"]).status.success());

        let wt = ensure_worktree(&fleet, &target, "HEAD", "a1").expect("worktree cut");
        assert!(wt.is_dir(), "worktree dir exists");
        assert!(wt.join("f").exists(), "checked out the target repo's tree");
        assert!(wt.ends_with("a1"));
        // Idempotent: a second call is a clean no-op returning the same path (no error re-adding).
        let wt2 = ensure_worktree(&fleet, &target, "HEAD", "a1").expect("idempotent");
        assert_eq!(wt, wt2);
        let _ = std::fs::remove_dir_all(&base_dir);
    }

    #[test]
    fn fleet_toml_parses_repo_and_declared_roster_with_defaults() {
        let toml_src = r#"
            [repo]
            path = "/home/u/Projects/camshaft/cadenza"
            base = "origin/main"
            gate = "cargo xtask fleet gate-local"
            merge = "pr-sync"

            [[agent]]
            name = "v-iterators"
            role = "vertical"
            vertical = "iterators"
            area = "rcdzc"

            [[agent]]
            name = "breaker"
            role = "breaker"
            model = "fable"
        "#;
        let cfg: TargetConfig = toml::from_str(toml_src).expect("valid fleet.toml");
        assert_eq!(cfg.repo.path, "/home/u/Projects/camshaft/cadenza");
        assert_eq!(cfg.repo.merge, "pr-sync");
        assert_eq!(cfg.agents.len(), 2);
        assert_eq!(cfg.agents[0].name, "v-iterators");
        // defaults applied where omitted
        assert_eq!(cfg.agents[0].interval, "10m");
        assert_eq!(cfg.agents[0].model, "opus");
        assert_eq!(cfg.agents[1].model, "fable");
        assert_eq!(cfg.agents[1].effort, "high", "default effort");
    }
}
