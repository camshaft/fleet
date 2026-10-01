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

/// Whether a board agent record is STAGED — a `metadata.staged == true` agent is a pre-registered helper
/// held in reserve (minted ahead of need, deployed later by clearing the flag), so NO auto-launch/manage
/// path may bring it up: reconcile must not launch it and the watchdog must not re-arm or spawn an observer
/// against it. Absent flag → not staged (the common case). Pure — unit-tested.
fn agent_is_staged(md: Option<&serde_json::Value>) -> bool {
    md.and_then(|m| m.get("staged"))
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
}

/// Whether the watchdog should manage an agent on this host. A STAGED agent ([`agent_is_staged`]) is never
/// managed — it is a reserve helper that is not meant to be running, so re-arming or spawning an observer
/// against it would be a spurious wake of an intentionally-down agent. Otherwise, under `pinned_only` (a
/// secondary box like green), ONLY agents EXPLICITLY pinned here ([`agent_host_is_explicit`]) — so it never
/// re-arms or spawns an observer against an unpinned agent whose tmux window / transcript lives on another
/// box. Without it, the loose predicate ([`agent_host_matches`]): this-host-pinned OR unpinned run-anywhere.
/// Pure — unit-tested.
fn watchdog_manages_agent(md: Option<&serde_json::Value>, host: &str, pinned_only: bool) -> bool {
    if agent_is_staged(md) {
        return false;
    }
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
        "sonnet" => "us.anthropic.claude-sonnet-5".to_string(),
        // Self-heal a stale/mis-registered id: the bare Anthropic-API sonnet id (`claude-sonnet-5-5`) is
        // rejected by this fleet's Bedrock endpoint (400 "invalid model identifier"), so an agent registered
        // with it 400s every turn and never ticks (the board-triage / board-follow-up outage). Map the bad
        // literal to the valid Bedrock id so a launch survives the misregistration instead of dying silently.
        "claude-sonnet-5-5" | "sonnet-5-5" => "us.anthropic.claude-sonnet-5".to_string(),
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
    let mut skipped_staged: Vec<String> = Vec::new();
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
            // A staged (reserve) agent is never auto-launched — it is minted ahead of need and deployed
            // later by clearing metadata.staged, so exclude it from the reconcile set entirely.
            if agent_is_staged(md) {
                skipped_staged.push(id);
                return None;
            }
            if pinned_only && !agent_host_is_explicit(md, &host) {
                skipped_unpinned.push(id);
                return None;
            }
            let offline = a.get("status").and_then(serde_json::Value::as_str) == Some("offline");
            Some((id, offline))
        })
        .collect();
    skipped_unpinned.sort();
    skipped_staged.sort();
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
    if !skipped_staged.is_empty() {
        println!(
            "  ⊘ skipped (staged — reserve helper, not auto-launched): {}",
            skipped_staged.join(", ")
        );
    }
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
        /// When THIS binary is stale relative to its checkout (a merged fix not yet rebuilt — #388), ACT on it:
        /// run `fleet redeploy --apply` (fast-forward origin/main, rebuild every daemon binary, restart the
        /// daemon services) instead of only printing the STALE warning. Safe by construction — redeploy declines
        /// a dirty or off-`main` checkout and keeps the old binaries if a build fails — and self-limiting: after
        /// the rebuild the binary matches the checkout, so the trigger does not re-fire. Off by default: a host
        /// opts in via its watchdog unit's `ExecStart` so an auto-restart of the daemons is never a surprise.
        #[arg(long)]
        self_redeploy: bool,
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
        /// Which harness transcript format to render: `claude` (Claude Code JSONL, the default) or `codex`
        /// (Codex CLI rollout JSONL). Codex rollouts are dated files, not per-agent project dirs, so for
        /// `codex` pass the rollout file explicitly with `--session <path>` — agent auto-location is
        /// claude-only for now.
        #[arg(long, default_value = "claude")]
        harness: String,
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
    /// Audit every board agent for a working PUSH-WAKE path (#386, operator: no poll-only agents). Each
    /// EXPECTED-RUNNING agent (not offline / stood-down) must be reachable by a wake: either a non-empty
    /// `webhook_url` (green-resident agents point it at their local fleet-notify) OR a LIVE reverse tunnel
    /// (off-LAN agents, keyed on the board by `GET /tunnels`). An agent with NEITHER is POLL-ONLY — it only
    /// sees work on its (slow) loop interval, the exact regression this guards. Prints one line per agent
    /// with its wake class and exits non-zero when any poll-only agent is found, so a supervisor can gate on
    /// it. Read-only (no board writes).
    WakeAudit {
        /// Also list agents classified `webhook` / `tunnel` (default: print those as a count and name only the
        /// poll-only ones, so the signal is not buried).
        #[arg(long)]
        verbose: bool,
    },
    /// Seam-check a MONITOR vertical (task_579): ff-sync its worktree to origin/main, then report whether any
    /// incoming commit touched the agent's declared SEAM — its `metadata.seam` file globs. A monitor wake is
    /// otherwise 100% deterministic git plumbing, so this lets the kickoff GATE the model wake: exit 0 = GREEN
    /// (no on-seam change — heartbeat and do NOT wake the model), exit 3 = CHANGED (wake the model; the
    /// seam-touching paths are printed so its tick opens with the diff in hand), exit 1 = error / no seam
    /// declared. Report-only — it computes the verdict, it does not itself wake or skip anything.
    SeamCheck {
        /// The agent whose `metadata.seam` globs + worktree to check.
        agent: String,
        /// Skip `git fetch` and check against the already-fetched `origin/main` (for tests / rapid re-runs).
        #[arg(long)]
        no_fetch: bool,
    },
    /// Safeguard-wedge check for an agent (task_582): scan its newest session transcript tail for a run of
    /// consecutive model-turn REFUSALS (stop_reason=refusal) — the signature of a loop stuck getting rejected
    /// by model safeguards. This is a DISTINCT failure from the idle stall the watchdog catches (a refused
    /// agent keeps advancing last_seen, so it reads as healthy while it burns). Exit code gates a supervisor:
    /// 0 = healthy (no trailing refusal run), 3 = WEDGED (recover via spin-down --force + spin-up), 1 = error
    /// / no transcript. Report-only — it classifies, it does not restart.
    SafeguardCheck {
        /// The agent whose newest session transcript to scan.
        agent: String,
        /// How many consecutive trailing refusal turns constitute a wedge.
        #[arg(long, default_value_t = 3)]
        threshold: usize,
        /// How many trailing transcript lines to scan for assistant turns.
        #[arg(long, default_value_t = 80)]
        tail: usize,
    },
    /// Nudge stale in_progress tasks (board task #478, operator: automate what board-follow-up was missing).
    /// A task in `in_progress` whose latest activity (its `updated_at`, or a later comment) is at least
    /// `--threshold-hours` old gets a comment pinging its assignee for a progress update or ETA. Per-task
    /// COOLDOWN: re-nudges the same task no more than once per `--cooldown-hours` while it stays idle, so it
    /// never spams. EXCLUSIONS (enforced unconditionally, not flag-gated): tasks assigned to the configured
    /// operator id (`config.operator_id`, if set — the operator is never nudged) and tasks not in `in_progress` (a `blocked` task is parked on a named
    /// dependency, not silently stalled — it is excluded by construction, since the board query is scoped to
    /// `in_progress`). An unassigned `in_progress` task is skipped too — there is no one to ping. Report-only
    /// by default (prints who it WOULD nudge and why); `--apply` posts the comments for real.
    NudgeStale {
        /// Actually post the nudge comments (default: dry-run — report the candidates, write nothing).
        #[arg(long)]
        apply: bool,
        /// How many hours of no activity before a task is a candidate.
        #[arg(long, default_value_t = 1.0)]
        threshold_hours: f64,
        /// Minimum hours between re-nudges of the SAME still-idle task.
        #[arg(long, default_value_t = 2.0)]
        cooldown_hours: f64,
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
        /// Include `--self-redeploy` (#388): the installed watchdog rebuilds + restarts the daemons when this
        /// binary falls behind its checkout, so a merged fix goes live without a manual rebuild. For a host that
        /// builds its daemons from a local checkout (not a hermetic flake deploy).
        #[arg(long)]
        self_redeploy: bool,
        /// INSTALL the units into `~/.config/systemd/user/` (user-level, no sudo) instead of printing them, and
        /// print the `systemctl --user enable` command — a clean, reversible install path for a host not on the
        /// declarative (nix) model. Reverse with `--uninstall`.
        #[arg(long)]
        install: bool,
        /// REMOVE the user units this installed (the inverse of `--install`) and print the `disable` command.
        #[arg(long)]
        uninstall: bool,
    },
    /// Emit or install a systemd USER service that supervises a long-running fleet-host daemon (Type=simple,
    /// Restart=on-failure), so it survives a tmux-window reap and restarts on crash — the durable replacement
    /// for a bare keep-alive window (#359). The service captures a known-good PATH so the daemon resolves
    /// tmux/git/curl at runtime. `--install` writes it to `~/.config/systemd/user/` (no sudo); `--enable`
    /// writes it AND brings it fully up (`daemon-reload` + `enable --now`) in one shot — the launch default
    /// that makes the supervised service, not a bare tmux window, the way a host daemon comes up; `--uninstall`
    /// removes it; default prints it for a declarative host to translate.
    DaemonUnit {
        /// Daemon name → unit `fleet-<name>.service` (e.g. `notifier`, `tunnel`).
        name: String,
        /// The command `ExecStart` runs. Defaults to `<bin> notify` for name `notifier`; required otherwise.
        #[arg(long)]
        exec: Option<String>,
        /// Restart backoff in seconds (systemd `RestartSec`).
        #[arg(long, default_value_t = 2)]
        restart_sec: u64,
        /// The fleet binary path for the built-in `notifier` default ExecStart (defaults to this binary).
        #[arg(long)]
        bin: Option<String>,
        /// INSTALL into `~/.config/systemd/user/` (user-level, no sudo) instead of printing; prints the enable command.
        #[arg(long)]
        install: bool,
        /// ENABLE in one shot: write the unit, then `systemctl --user daemon-reload` + `enable --now` so the
        /// daemon comes up supervised immediately (the #359 launch default). Idempotent; implies the write.
        #[arg(long)]
        enable: bool,
        /// REMOVE the user unit this installed (the inverse of `--install`) and print the disable command.
        #[arg(long)]
        uninstall: bool,
    },
    /// Print the build provenance — package version + the commit the binary was built from (baked at build
    /// time). Compare the rev to `origin/main` to tell whether a deployed binary is current (a stale binary
    /// silently runs old logic — the failure mode a stale watchdog binary hit).
    Version,
    /// Bring the host's fleet daemons up to `origin/main`: when the built binary's rev is behind the remote,
    /// fast-forward the checkout, rebuild the release binary, and restart the daemon services — the manual
    /// rebuild+restart step a merged fleet PR otherwise needs before its fix goes live (#388). Default: REPORT
    /// only (safe dry-run); `--apply` acts. Refuses to act on a dirty or non-`main` checkout (never clobbers
    /// local work).
    Redeploy {
        /// Actually fast-forward + rebuild + restart. Without it, only report whether a redeploy is needed.
        #[arg(long)]
        apply: bool,
    },
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
            self_redeploy,
        } => watchdog(stale_only, rearm, observe, spawn, dry_run, pinned_only, self_redeploy),
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
            harness,
        } => transcripts_cmd(&agent, session.as_deref(), since.as_deref(), overlap, &harness),
        Cmd::ServedSet { toml } => served_set(toml),
        Cmd::WakeAudit { verbose } => wake_audit(verbose),
        Cmd::SeamCheck { agent, no_fetch } => seam_check(&agent, no_fetch),
        Cmd::SafeguardCheck { agent, threshold, tail } => safeguard_check(&agent, threshold, tail),
        Cmd::NudgeStale {
            apply,
            threshold_hours,
            cooldown_hours,
        } => nudge_stale(apply, threshold_hours, cooldown_hours),
        Cmd::WatchdogUnit {
            observe,
            pinned_only,
            interval_secs,
            bin,
            no_rearm,
            self_redeploy,
            install,
            uninstall,
        } => watchdog_unit(!no_rearm, observe, pinned_only, self_redeploy, interval_secs, bin, install, uninstall),
        Cmd::DaemonUnit { name, exec, restart_sec, bin, install, enable, uninstall } => {
            daemon_unit(&name, exec, restart_sec, bin, install, enable, uninstall)
        }
        Cmd::Version => println!("{}", version_line()),
        Cmd::Redeploy { apply } => redeploy(apply),
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

/// Parse a board workspace-kind record into a launch plan. The launch directory is resolved with precedence
/// `cwd_override` (the agent's own `metadata.workspace_cwd`) > the kind's `config.cwd` > the agent's own root
/// dir; an absolute path is used as-is, a relative one is taken under `fleet_root`. The per-agent override lets
/// several agents share ONE kind (same setup_script/env) while each launches in its own workspace directory.
/// `config.pre_trust` is an optional list of extra paths to trust (the launch cwd and the fleet root are always
/// trusted), and `config.env` an optional string map of environment variables the setup_script receives. Pure —
/// unit-tested.
fn parse_workspace_kind(
    agent: &str,
    fleet_root: &str,
    rec: &serde_json::Value,
    cwd_override: Option<&str>,
) -> WorkspaceKindPlan {
    let name = rec.get("name").and_then(|v| v.as_str()).unwrap_or("?").to_string();
    let description = rec.get("description").and_then(|v| v.as_str()).map(str::to_string);
    let setup_script = rec
        .get("setup_script")
        .and_then(|v| v.as_str())
        .filter(|s| !s.trim().is_empty())
        .map(str::to_string);
    let config = rec.get("config").cloned().unwrap_or(serde_json::Value::Null);
    let cwd_src = cwd_override
        .filter(|s| !s.trim().is_empty())
        .or_else(|| config.get("cwd").and_then(|v| v.as_str()));
    let cwd = match cwd_src {
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

/// The environment a workspace-kind `setup_script` receives: the agent identity and fleet root, the resolved
/// per-agent launch directory as `FLEET_WORKSPACE_CWD`, then the kind's own `config.env`. The launch cwd is
/// exported because several agents can share ONE kind (same `setup_script`) while each launches in its own
/// workspace directory at an arbitrary host path with no shared convention, so the shared script has no other
/// way to find the agent's own workspace. Config env is appended last so a kind may override a built-in if it
/// deliberately must. Pure — unit-tested.
fn setup_script_env(agent: &str, fleet_root: &str, plan: &WorkspaceKindPlan) -> Vec<(String, String)> {
    let mut env = vec![
        ("FLEET_AGENT".to_string(), agent.to_string()),
        ("FLEET_ROOT".to_string(), fleet_root.to_string()),
        ("FLEET_WORKSPACE_CWD".to_string(), plan.cwd.clone()),
    ];
    env.extend(plan.env.iter().cloned());
    env
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
    reactive: bool,
    apply: bool,
    cwd_override: Option<&str>,
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
    let plan = parse_workspace_kind(agent, fleet_root, &rec, cwd_override);

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
            "  setup_script: {} line(s) — runs with FLEET_AGENT/FLEET_ROOT/FLEET_WORKSPACE_CWD{} in the environment",
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
        cmd.arg("-c").arg(script).current_dir(fleet_root);
        for (k, v) in setup_script_env(agent, fleet_root, &plan) {
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

    match pre_trust_for_harness(harness, &plan.pre_trust) {
        Ok(true) => println!("  pre-trusted {} path(s) (launch cwd + fleet root + config pre_trust) in the {harness} config", plan.pre_trust.len()),
        Ok(false) => {}
        Err(e) => eprintln!("  WARN: could not pre-trust: {e} (agent may hit a one-time trust prompt)"),
    }
    match launch_board_agent(agent, &plan.cwd, harness, model, effort, interval, devshell, reactive) {
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

/// Normalize a board record's `metadata.repos` into the canonical list of `{repo, …}` entries, tolerating
/// the looser shapes a HAND-authored registration writes instead of the structured form `fleet set-meta`
/// produces (the off-tree membrain-cdk was minted with a CSV string, #472 / operator seq-6269):
/// - an array of `{repo: …}` objects → kept as-is (the canonical form);
/// - an array of bare strings `["A","B"]` → each wrapped as `{repo: "A"}`;
/// - a single comma-separated string `"A, B, C"` → split + trimmed into `{repo}` entries.
///
/// Without this, `as_array()` alone silently drops a CSV-string `repos` to "none declared" and the agent's
/// worktrees never materialize. Any other shape yields no entries. Pure — unit-tested.
fn normalize_repos(v: Option<&serde_json::Value>) -> Vec<serde_json::Value> {
    match v {
        Some(serde_json::Value::Array(a)) => a
            .iter()
            .map(|e| match e {
                serde_json::Value::String(s) => serde_json::json!({ "repo": s }),
                other => other.clone(),
            })
            .collect(),
        Some(serde_json::Value::String(s)) => s
            .split(',')
            .map(str::trim)
            .filter(|p| !p.is_empty())
            .map(|p| serde_json::json!({ "repo": p }))
            .collect(),
        _ => Vec::new(),
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
    // Opt-in REACTIVE responder pacing (#438): a mention-only bot (e.g. a Slack-channel participant) gets a
    // kickoff whose only actionable trigger is being explicitly addressed, so it does not self-poll on ambient
    // chatter. Off by default — a normal work-conserving worker is unchanged.
    let reactive = md.get("reactive").and_then(|v| v.as_bool()).unwrap_or(false);
    // Accept the canonical structured list AND the looser hand-authored shapes (bare-string array, CSV
    // string) so a repos written by hand is never silently dropped (#472 / operator seq-6269).
    if md.get("repos").is_some_and(serde_json::Value::is_string) {
        eprintln!("  note: metadata.repos is a comma-separated STRING — normalized to a structured list; prefer writing repos as [{{\"repo\":\"…\"}}] (the form `fleet set-meta --repo` produces)");
    }
    let repos = normalize_repos(md.get("repos"));
    let fleet_root = config::get()
        .root
        .clone()
        .unwrap_or_else(|| format!("{}/.fleet", std::env::var("HOME").unwrap_or_default()));

    // A board-defined custom workspace kind (metadata.workspace_kind) takes precedence over `repos`: the
    // board resource named by the kind carries a setup_script that materializes the workspace and a
    // free-form config with the launch hints (cwd/pre_trust/env). This lets an environment the fleet does
    // not model natively be defined in a board resource and driven from there. (#287)
    if let Some(kind) = field("workspace_kind") {
        // A per-agent `metadata.workspace_cwd` overrides the kind's `config.cwd`, so several agents can share
        // one kind (same setup_script/env) while each launches in its own workspace directory.
        let cwd_override = field("workspace_cwd");
        return spin_up_workspace_kind(
            &board, agent, &kind, &fleet_root, has_charter, &harness, &model, &effort, &interval,
            devshell, reactive, apply, cwd_override.as_deref(),
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
    // Pre-trust so the harness does not stall on the one-time folder-trust prompt (an interactive agent can't
    // answer it, and neither claude's --dangerously-skip-permissions nor codex's bypass flag skips it). The
    // trust TARGET differs by harness: claude resolves a worktree workspace to its git common dir (the shared
    // MIRROR), which it does not inherit from the fleet root — so trust each repo's mirror (a repo-less
    // workspace is trusted by its own dir) plus the fleet root; codex trusts the folder it launches in, i.e.
    // the workdir. Non-fatal on error.
    let trust: Vec<String> = if harness == "codex" {
        vec![workdir.clone()]
    } else {
        let mut t = vec![fleet_root.clone()];
        for r in &repos {
            if let Some(repo) = r.get("repo").and_then(|v| v.as_str()) {
                t.push(workspace::mirror_dir(&fleet_root, repo));
            }
        }
        if repo_less {
            t.push(workdir.clone());
        }
        t
    };
    match pre_trust_for_harness(&harness, &trust) {
        Ok(true) => println!("  pre-trusted {} path(s) in the {harness} config", trust.len()),
        Ok(false) => {}
        Err(e) => eprintln!("  WARN: could not pre-trust: {e} (agent may hit a one-time trust prompt)"),
    }
    match launch_board_agent(agent, &workdir, &harness, &model, &effort, &interval, devshell, reactive) {
        Ok(win) => {
            let loop_kind = if reactive { "reactive (mention-only) /loop" } else { "work-conserving dynamic /loop" };
            println!(
                "  LAUNCHED '{agent}' in tmux window '{win}' (cwd {workdir}) — it will get_agent itself for its charter, then run a {loop_kind} (idle cadence ~{interval})"
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
fn build_kickoff(agent: &str, workdir: &str, interval: &str, operator: Option<&str>, reactive: bool) -> String {
    // The operator-blocked dashboard convention (operator seq-2292) applies only when this deployment names an
    // operator (config.operator_id); a generic fleet with no designated operator omits it. The id is
    // interpolated, never hard-coded, so the public fleet code carries no operator name (task_611).
    let operator_clause = match operator {
        Some(op) => format!(
            " If the block is ON THE OPERATOR specifically, ALSO assign the task to '{op}' and stash your own \
             id in metadata.blocked_owner — so list_tasks(assignee '{op}') is the operator's single 'my asks' \
             dashboard; when the operator answers, reassign the task back to yourself and clear blocked."
        ),
        None => String::new(),
    };
    // REACTIVE responders (mention-only bots like a Slack channel participant) invert the pacing: the generic
    // work-conserving loop treats ANY unread notification as a reason to re-poll SOON, but a silence-default
    // responder must treat ambient channel chatter as NON-work and only act when EXPLICITLY addressed — else
    // it self-schedules short re-polls on coordination noise while (correctly) staying silent (#438). Its
    // actionable triggers are being addressed OR an in-thread follow-up on a conversation it is already part
    // of (a reply under a thread root it is subscribed to — the notifier wakes it on that delivery, and the
    // prompt must count it as addressed so it CONTINUES the exchange rather than ignoring it as ambient);
    // otherwise it goes idle and waits for a live event-wake.
    let tick = if reactive {
        format!(
            "run one tick as a REACTIVE responder. Your ONLY actionable triggers are being EXPLICITLY \
             ADDRESSED — a mention or @mention of you in a channel you belong to, a direct message to you, or \
             a task assigned to you — OR an in-thread FOLLOW-UP on a conversation you are already engaged in: \
             a post whose reply_to is a thread root you are subscribed to (check_notifications surfaces \
             data.reply_to and a per-recipient thread_subscribed marker). A threaded reply to a message you \
             are handling continues that exchange and IS addressed to you, even without a fresh @mention — \
             stay in the thread until it resolves. Ambient channel chatter and unread coordination posts that \
             do NOT address you and are NOT under a thread you are engaged in are NOT work — read them for \
             context if useful, but they NEVER make you act or re-poll. Each wake: check whether anything \
             ADDRESSES you or continues a thread you are in (check_notifications with agent_id '{agent}'); if \
             so, handle it per your charter (respond / act), then re-check. If NOTHING addresses you, you are \
             DONE for this wake — update presence (set_status) if useful, then go idle and WAIT TO BE WOKEN. \
             Do NOT schedule a soon next tick just because coordination chatter is unread — silence is your \
             default and a live event-wake re-tickets you the instant you are addressed, so short-cadence \
             polling on ambient activity buys nothing and risks a wrong ambient interjection. Fall straight to \
             the long idle cadence (about {interval}) whenever nothing addresses you."
        )
    } else {
        format!(
            "run one tick of your charter: drain your board notifications (check_notifications), do ONE unit \
             of work per your charter, then update your presence (set_status). WORK-CONSERVING PACING: after \
             the unit, check your OPEN assigned tasks (list_tasks with assignee '{agent}', counting ONLY \
             todo/in_progress tasks that are NOT blocked/parked — a blocked task, or one parked on a blocker or \
             a not-yet-existing prereq, is NOT actionable pending work) and your unread notifications. If you \
             hold actionable assigned work OR unread messages, keep going — schedule your next tick SOON \
             (60-120s). Only when you have no actionable assigned task AND your inbox is drained may you fall \
             back to the long idle cadence (about {interval}). NEVER idle-sleep on the long cadence while you \
             still hold an actionable assigned task. If your assigned cluster is DONE / at-rest — no actionable \
             work and your only revival triggers are external events (a routed message, a new assignment, a \
             decline) — do NOT keep self-re-arming at your active interval: persist a long cadence by setting \
             your BOARD metadata.interval (update_agent, e.g. 2-3h) — that is the source of truth the watchdog \
             reads, and it works even for a board-only agent with no file-hub registry row (the frozen \
             `cargo xtask fleet set-interval` writes only the registry, so it fails for a board-only agent and \
             leaves the board mirror stale — do NOT rely on it); a raw next-tick reschedule does NOT persist \
             against the watchdog either, so it keeps waking you at the active interval. Then rely on \
             event-wake — a routed message or assignment nudges your window awake immediately regardless of \
             interval, so a long rest cadence never delays revival, it only cuts empty self-directed ticks."
        )
    };
    format!(
        "You are the fleet agent '{agent}', running UNATTENDED. Your task-board MCP tools are available in \
         this session. Call register_agent with agent_id '{agent}' once (idempotent) so your board record \
         exists, then get_agent '{agent}' to read your OWN charter + metadata from the board and follow that \
         charter as your role. IMPORTANT — the board does NOT bind your session: each call may reach a fresh, \
         unbound board, so register_agent does NOT make later id-less calls work. Pass your identity \
         EXPLICITLY on EVERY board call, and note that each tool NAMES the identity field DIFFERENTLY — \
         agent_id on check_notifications / set_status / get_messages / list_tasks, from_agent on send_message, \
         author on comment_task / comment_document, actor on update_task, created_by on create_task (all = \
         '{agent}'). Passing the WRONG field (e.g. agent_id to comment_task, which takes author) is silently \
         accepted but records the actor as null, so the board cannot exclude you from your own notification and \
         you wake on your OWN comment — use each tool's own field. The board defaults identity to null and a \
         call that omits it fails with 'no identity for this session'. Never PRECOMPUTE or guess a task id: \
         reference a task only by the id create_task RETURNS, because concurrent creation on the shared board \
         can hand a guessed next-id to a DIFFERENT agent's task. TYPED REFERENCES (task_584): whenever you \
         WRITE a task or PR/issue reference into any board body (a comment, a message, or a channel post), \
         spell it as a TYPED id — 'task_N' for a board task, or 'owner/repo#N' for a GitHub issue/PR — and \
         NEVER a bare '#N': the board hard-rejects a bare '#N' in posted content, so a bare ref costs you a \
         reword-and-retry every time. \
         Coordinate through the board (send_message / check_notifications / \
         comment_task / set_status) — there is no file inbox. Any board Document you author (design / \
         proposal / plan) MUST follow the Fleet Doc-Writing Style Guide — wiki guides/doc-writing-style-guide \
         (Background then Problem Statement then Requirements/Goals/Non-Goals (measurable) then Solutions, \
         each its own section with prose + Pros/Cons, then Recommendation; implementation in an appendix; NO \
         tables/images/TL;DR/idioms in the body — the board viewer is minimal Markdown). Before submitting \
         ANY board doc OR comment, self-check your wording against the banned-phrases list \
         (wiki guides/banned-phrases) and rephrase anything it flags — that list is maintained/data-driven, so \
         read it rather than a fixed set here; until the pre-submit scanner (#308) lands this self-check is \
         yours. When you WRITE content into an MCP tool argument — a comment, a message, or a doc/version \
         body — pass the ACTUAL content as the argument, NEVER a shell substitution like a $(cat file) token \
         or a backtick command: the MCP call has no shell, so the literal token is stored VERBATIM and \
         silently clobbers the target while the write still returns success, so READ BACK what you wrote \
         (re-get the doc/comment) to confirm the real content landed (task_589). And never put a commit/PR \
         attribution line — a 'Generated with ...' or a 'Co-Authored-By:' line — in a board task/doc body or \
         comment; those belong only on git commits and PR descriptions, not board content. OWNER-CONFIRM gate: before you EXECUTE, or route to the operator, any DESTRUCTIVE or \
         operator-directed action (service restart, deploy, data-touching command) that you SYNTHESIZED from \
         another agent's trace or diagnosis of a service you do NOT own, first confirm the exact command with \
         that service's OWNER (the authority on their live unit); if the owner cannot confirm in time, mark it \
         OWNER-UNCONFIRMED so the operator double-checks — partial visibility can read a stale unit as live. \
         SHARED-TASK COORDINATION (task_565): when an escalation (e.g. an anti-stall judgment-check) flags a \
         dangling spin-off or follow-on on a SHARED task that has a LIVE owner, the OWNER files the spin-off — \
         a coordinator files only if the owner is absent/stalled or has not acted within a beat; never have \
         both owner and coordinator race-create it. And DEDUP IS SINGLE-WRITER: when duplicate tasks exist, \
         the ONE dedup owner picks the survivor, cancels the loser, and FREEZES — declare 'I own the end-state, \
         stop toggling'; never symmetric-cancel your OWN duplicate deferring to the other agent, which \
         deadlocks (both cancel, then both re-toggle); if you spot a dup you do not own the dedup for, flag the \
         dedup owner rather than cancelling your own. \
         If a task of \
         yours becomes BLOCKED ON AN EXTERNAL DEPENDENCY you cannot act on — the operator, another agent, or \
         a pending deploy/CI/cross-agent reply — do NOT sit on the SOON cadence polling for it: set the task \
         status=blocked with a blocked_on note. A blocked task is NOT actionable pending work, so if it is \
         your only open task you drop to the long/event-woken cadence — the live actionable-event wake \
         re-tickets you the moment a reply, an assignment, or the dep landing arrives, so short-cadence \
         polling buys nothing.{operator_clause} (If your MCP cannot set a typed blocked_on, ask concierge or board-pm to stamp it.) STATUS \
         HONESTY (task_506): never set your presence offline or away while you still hold a live in_progress \
         assigned task — an in_progress task means actively-worked, so before you stand down you MUST either \
         progress it or re-state it as blocked (with a blocked_on note) or done; standing down on a live \
         in_progress task is a status-honesty violation the watchdog flags and re-arms. You work \
         in {workdir}. Start your recurring \
         loop now: /loop {tick}"
    )
}

/// Build the shell command that launches the agent's harness (agent runtime) in its tmux window, per the
/// selected `harness`. This is the one seam every harness plugs into: the window launch, trust, and kickoff
/// are harness-agnostic, only this command differs. The kickoff rides in `$CDZ_KICKOFF` (set on the window),
/// so the command references that env var rather than interpolating the prompt. Pure so it is unit-tested.
///
/// `claude` and `codex` are both wired. Each takes a persistent-TUI launch that reads its initial prompt
/// from `$CDZ_KICKOFF`; the kickoff prose itself drives the recurring loop, so the two share one kickoff and
/// wake path and differ only in the CLI's own launch flags. `codex` reaches its model through whatever the
/// agent's `metadata.model` names, which for a codex agent is an OpenAI-wire model name (the codex CLI speaks
/// the OpenAI wire protocol) rather than a Claude model id — this arm passes it through unchanged, so the
/// concrete name lives in board data, not here. An unknown harness is rejected so a typo'd `metadata.harness`
/// fails loudly at spin-up instead of launching nothing.
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
        "codex" => {
            // The bypass flag runs unattended (no per-action approval, no sandbox); the model is
            // single-quoted so nothing in it can glob; the kickoff rides in $CDZ_KICKOFF (set literally via
            // `-e`, expanded double-quoted) so its spaces/quotes are safe. codex still gates a first-run
            // launch on per-workspace folder trust, which the bypass flag does NOT skip — spin-up pre-trusts
            // the workdir out of band, so the launch itself does not carry it.
            let codex = format!(
                "codex --dangerously-bypass-approvals-and-sandbox --model '{model}' \"$CDZ_KICKOFF\""
            );
            Ok(match devshell {
                Some(dir) => format!("exec nix develop \"path:{dir}\" --command {codex}"),
                None => format!("exec {codex}"),
            })
        }
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
#[allow(clippy::too_many_arguments)]
fn launch_board_agent(agent: &str, workdir: &str, harness: &str, model: &str, effort: &str, interval: &str, devshell: bool, reactive: bool) -> Result<String, String> {
    let session = board_session();
    if let Ok(out) = std::process::Command::new("tmux")
        .args(["list-windows", "-t", &session, "-F", "#W"])
        .output()
        && String::from_utf8_lossy(&out.stdout).lines().any(|w| w == agent)
    {
        return Err(format!("a tmux window '{agent}' already exists in session '{session}' (already spun up?)"));
    }
    let kickoff = build_kickoff(agent, workdir, interval, config::get().operator_id.as_deref(), reactive);
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

/// Whether `dir` still needs a codex trust entry — `true` unless `projects.<dir>.trust_level` is already
/// `"trusted"`. codex records first-run folder trust as a `[projects."<dir>"]` table with
/// `trust_level = "trusted"`, and `--dangerously-bypass-approvals-and-sandbox` runs unattended but does NOT
/// skip that gate, so spin-up must pre-trust the workdir the way it does for claude. Pure — unit-tested.
fn codex_trust_missing(config: &toml::Value, dir: &str) -> bool {
    config
        .get("projects")
        .and_then(|p| p.get(dir))
        .and_then(|e| e.get("trust_level"))
        .and_then(toml::Value::as_str)
        != Some("trusted")
}

/// Render the `[projects."<dir>"]` trust table to append to a codex config. `dir` is a TOML basic-string
/// key, so `\` and `"` are escaped (a filesystem path rarely holds either, but the key must encode exactly).
/// Pure — unit-tested.
fn codex_trust_table(dir: &str) -> String {
    let key = dir.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\n[projects.\"{key}\"]\ntrust_level = \"trusted\"\n")
}

/// Idempotently mark each of `dirs` (and its symlink-canonical form) trusted in `~/.codex/config.toml`, so a
/// spun-up codex agent never stalls on the one-time folder-trust prompt. For any dir not already trusted it
/// APPENDS a `[projects."<dir>"]` table: an append preserves the operator's existing config and comments
/// verbatim, whereas a parse-then-reserialize would strip them (`toml` 0.8 does not round-trip comments), and
/// those host-local specifics must not be disturbed. The file must already exist (the operator configures
/// codex's model provider there); a missing file is an error the caller warns on. Returns true if it appended.
fn pre_trust_dirs_codex(dirs: &[String]) -> Result<bool, String> {
    let home = std::env::var("HOME").map_err(|_| "no HOME".to_string())?;
    let cfg = format!("{home}/.codex/config.toml");
    let raw = std::fs::read_to_string(&cfg).map_err(|e| format!("read {cfg}: {e}"))?;
    let v: toml::Value = raw.parse().map_err(|e| format!("parse {cfg}: {e}"))?;
    // The literal and canonical (symlink-resolved) forms that still need an entry, de-duplicated — same
    // both-forms care as the claude path (an agent may launch under a literal path but be checked canonical).
    let mut want: Vec<String> = Vec::new();
    for dir in dirs {
        let mut forms = vec![dir.clone()];
        if let Ok(canon) = std::fs::canonicalize(dir) {
            let c = canon.to_string_lossy().into_owned();
            if &c != dir {
                forms.push(c);
            }
        }
        for form in forms {
            if codex_trust_missing(&v, &form) && !want.contains(&form) {
                want.push(form);
            }
        }
    }
    if want.is_empty() {
        return Ok(false);
    }
    let mut body = raw;
    if !body.ends_with('\n') {
        body.push('\n');
    }
    for form in &want {
        body.push_str(&codex_trust_table(form));
    }
    let tmp = format!("{cfg}.fleet-tmp-{}", std::process::id());
    std::fs::write(&tmp, &body).map_err(|e| format!("write {tmp}: {e}"))?;
    std::fs::rename(&tmp, &cfg).map_err(|e| format!("rename {tmp} -> {cfg}: {e}"))?;
    Ok(true)
}

/// Pre-trust `dirs` in the folder-trust store of the given `harness`, so a spun-up agent never stalls on the
/// one-time folder-trust prompt: `codex` → `~/.codex/config.toml`; every other harness (claude, the default)
/// → `~/.claude.json`. Returns whether it wrote a change.
fn pre_trust_for_harness(harness: &str, dirs: &[String]) -> Result<bool, String> {
    match harness {
        "codex" => pre_trust_dirs_codex(dirs),
        _ => pre_trust_dirs(dirs),
    }
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

/// A watchdog re-arm/retighten candidate: an agent that HOLDS OPEN ASSIGNED WORK and either has a `STALE`
/// heartbeat (its loop lapsed several intervals — it stopped cycling while holding a queue) or is sitting on a
/// long idle interval (work-conserving — it should loop tighter until that queue drains).
///
/// A DRAINED QUEUE (zero open assigned tasks) is NEVER a candidate (#332). An agent that emptied its queue and
/// then lengthened its own poll cadence is doing sanctioned idle work, not stalling — the operator explicitly
/// sanctions a long idle cadence once the queue empties, so reading a lengthened poll as "overdue" and nagging
/// it ("keep looping until your queue drains") is a no-op that traps a mission-complete agent in
/// heartbeat-and-reschedule churn (v-capmeshd / v-nmidid: 0 open tasks yet STALE against a short registered
/// interval → falsely flagged). Any message that arrives for a drained agent wakes it through the event-driven
/// notifier, not this backstop. Requiring `open_tasks > 0` also subsumes the earlier presence short-circuit: an
/// `away` / `offline` / `done` agent that parked with a drained queue is covered by the same guard, while one
/// that parked while STILL holding work stays a candidate (it shouldn't have parked with work), unchanged.
///
/// NOT `late`: an agent that heartbeats once per loop interval naturally reaches age ≈ 1× its interval right
/// before its next scheduled tick, so `late` (1–3× interval) is the NORMAL band for a healthy idle agent, not
/// a stall. Only `STALE` (≥ [`WATCHDOG_OVERDUE_INTERVALS`]× interval) means the loop actually stopped. Pure —
/// unit-tested.
fn is_retighten_candidate(verdict: &str, open_tasks: usize, interval_secs: u64) -> bool {
    if open_tasks == 0 {
        return false;
    }
    verdict == "STALE" || interval_secs >= WATCHDOG_LONG_INTERVAL_SECS
}

/// #535 work-driven tight cadence: an agent that HOLDS actionable work must loop tightly rather than sleep its
/// full registered interval, so — independent of that interval — a native agent with open actionable tasks
/// (`open_tasks > 0`, already unblocked/non-monitor-exempt via [`board::Board::open_task_count`]) that is NOT
/// stood down and has gone quiet longer than [`WATCHDOG_WORK_CADENCE_SECS`] is a wake candidate. This closes
/// the gap [`is_retighten_candidate`] left: a work-holder on a MODERATE interval (< 1h, not yet STALE) was
/// judged "already tight" and slept its whole interval with work pending. The actual wake stays cooldown-
/// limited and pane-fenced by [`rearm_candidate`], so this only changes WHICH agents are caught, never waking
/// a working pane or spamming a stalled one. `None` age (unknown last_seen) is not a candidate. Pure.
fn work_driven_rearm(open_tasks: usize, age_secs: Option<i64>, stood_down: bool) -> bool {
    !stood_down
        && open_tasks > 0
        && matches!(age_secs, Some(a) if a >= WATCHDOG_WORK_CADENCE_SECS)
}

/// #544 drained self-poller: a live, at-rest agent that keeps self-scheduling short ticks with NO actionable
/// work should drop to a long registry cadence + event-wake, but a running `/loop` never picks up a
/// `build_kickoff` edit (it re-passes its spawn-time prompt), so the watchdog injects the instruction
/// ([`WATCHDOG_LENGTHEN_WAKE`]). Candidate = not stood down, not `reactive` (a mention-paced responder is not
/// this), not a `deliberate_monitor` (holds a monitor_exempt task — it is MEANT to poll), not a `patrol` agent
/// (a proactive event-less SWEEP — board-follow-up / board-triage — whose 0-assigned-tasks + short cadence IS
/// its charter, not idle self-polling: lengthening it to event-wake-only would silence the anti-stall /
/// reconciliation / intake layer, since its catches never arrive as events), 0 actionable tasks, a live
/// heartbeat (`verdict != "STALE"` — a stale loop is a re-arm/relaunch case, not an over-eager poller), and a
/// SHORT registered interval (`< WATCHDOG_LONG_INTERVAL_SECS` — a 1h+ agent is already at a long cadence). Once
/// it lengthens past that bound it stops qualifying, so the inject fires about once per agent. Pure —
/// unit-tested.
fn drained_idle_candidate(
    open_tasks: usize,
    interval_secs: u64,
    stood_down: bool,
    reactive: bool,
    deliberate_monitor: bool,
    patrol: bool,
    verdict: &str,
) -> bool {
    !stood_down
        && !reactive
        && !deliberate_monitor
        && !patrol
        && open_tasks == 0
        && verdict != "STALE"
        && interval_secs > 0
        && interval_secs < WATCHDOG_LONG_INTERVAL_SECS
}

/// Grace window before an agent whose heartbeat never advanced past registration is called "never-ticked":
/// below this, an agent legitimately still shows `last_seen == created_at` because its first loop tick has
/// not landed yet (cold boot + charter fetch + first sweep). Comfortably longer than that for any interval.
const WATCHDOG_NEVER_TICKED_GRACE_SECS: i64 = 600; // 10m

/// A board-native agent that LAUNCHED but never completed a single loop tick: its `last_seen` never advanced
/// past its `created_at` (the board stamps them equal at registration and only a real tick moves `last_seen`),
/// yet it registered well over the grace window ago. This is the launch-time-crash signature (#417) — a bad
/// model id 400-looping, a bad charter, a harness crash — the board-triage / board-follow-up outage where both
/// sat `online` but dead for ~3.7h until an operator noticed. It is DISTINCT from a live-but-idle loop (which
/// ticked at least once, so `last_seen` > `created_at`) and from an ordinary `STALE` heartbeat (which ticked,
/// then lapsed while holding work): those older signals require open tasks or bucket on age, so a freshly
/// crash-looping agent with an empty queue slips past both — exactly why the outage went unflagged. A wake
/// cannot recover it (there is no live loop to re-arm — the #412/#420 lesson), so the watchdog surfaces it for
/// investigation + relaunch, not a no-op wake. Timestamps are compared within a 2s epsilon so a precision
/// difference between the two board columns does not mask the equality. Pure — unit-tested.
fn agent_never_ticked(created_at: &str, last_seen: &str, now: time::OffsetDateTime, grace_secs: i64) -> bool {
    use time::format_description::well_known::Rfc3339;
    let (Ok(created), Ok(seen)) = (
        time::OffsetDateTime::parse(created_at, &Rfc3339),
        time::OffsetDateTime::parse(last_seen, &Rfc3339),
    ) else {
        return false;
    };
    // last_seen never advanced past created_at (within a small epsilon for column-precision drift) ...
    if (seen - created).whole_seconds().abs() > 2 {
        return false;
    }
    // ... and old enough that a first tick should have landed by now.
    (now - created).whole_seconds() >= grace_secs
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
// A re-arm candidate ALWAYS holds pending work now ([`is_retighten_candidate`] never flags a drained agent),
// so the wake states that plainly — no "overdue and/or open tasks" hedging that could name a phantom trigger
// (the #332 false-nag). "Pending work" covers both paths: open assigned tasks (board) or unread inbox items.
const WATCHDOG_REARM_WAKE: &str = "[watchdog] you hold pending work and your loop has gone quiet — run a tick NOW: check_notifications, do one unit, set_status, and keep looping until your queue drains (do not idle-sleep while you hold pending work).";

/// The wake injected to a DRAINED self-poller (#544): an at-rest agent with no actionable work that keeps
/// self-scheduling short ticks. It cannot pick this up from a `build_kickoff` edit (a running `/loop` re-passes
/// its spawn-time prompt), so the watchdog injects the instruction directly — the agent then persists a long
/// cadence by setting its BOARD metadata.interval (the cadence this sweep reads) and drops to event-wake. Once
/// its interval is long it is no longer a candidate, so this fires about once per agent, not every sweep. The
/// lever is the board metadata (update_agent), NOT the frozen `cargo xtask fleet set-interval`: that writes
/// only the file-hub registry, so it fails for a board-only agent (no registry row) and leaves the board
/// metadata.interval this sweep reads stale (task_566: concierge + design-multi-operator both hit this).
const WATCHDOG_LENGTHEN_WAKE: &str = "[watchdog] you are at-rest with no actionable assigned work but are self-polling at a short cadence. Persist a long idle cadence: set your board metadata.interval to 3h via update_agent (that is the cadence this watchdog reads, and it works even if you have no file-hub registry row) - a raw next-tick reschedule does NOT persist - then rely on event-wake, since a routed message or new assignment wakes you immediately regardless of interval, so a long rest cadence never delays revival, it only stops the empty self-directed ticks. Do NOT schedule a short next tick.";

/// A re-arm to the SAME agent is never sent more often than this, even for a short or unparsed (0s) interval.
const WATCHDOG_REARM_COOLDOWN_FLOOR_SECS: u64 = 300; // 5 min = 5× the 1-min poll

/// #535 work-driven tight cadence: how long an agent that HOLDS actionable work may be quiet before the
/// watchdog treats it as a wake candidate, INDEPENDENT of its registered interval (which then only sets the
/// idle cadence). Short — a work-holder should be cycling about this often. The actual wake is still
/// cooldown-limited by [`rearm_on_cooldown`] (never shorter than [`WATCHDOG_REARM_COOLDOWN_FLOOR_SECS`]) and
/// pane-fenced by [`rearm_candidate`], so this tightens WHEN a stalled work-holder is caught without waking a
/// working pane or spamming every sweep.
const WATCHDOG_WORK_CADENCE_SECS: i64 = 120; // 2 min

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
         attribute kb_remember to `observer`. Never leave created_by/author null. Keep board bodies CLEAN: \
         never append a commit/PR attribution line — a 'Generated with ...' or a 'Co-Authored-By:' line — to a \
         task body, comment, or proposal; that belongs on git commits and PRs, not board content (board-pm). \
         This session makes exactly \
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

/// The distinct angles an adversarial review is run on (#374, Doc #5 D16) — one ephemeral reviewer per angle,
/// each `(key, focus)`: `key` labels the angle in the reviewer's board writes, `focus` is the lens it reviews
/// through. The clarity angle names the MAINTAINED writing lists (Doc #7 / Doc #8) as run-time truth rather
/// than a hardcoded pattern copy, because those lists are data-driven and grow (board-pm, fleet writing policy).
const REVIEW_ANGLES: &[(&str, &str)] = &[
    (
        "correctness-completeness",
        "Is it CORRECT and COMPLETE? Find factual errors, missing cases, unhandled inputs, gaps between what \
         it claims and what it does, and requirements it does not meet.",
    ),
    (
        "clarity-writing",
        "Is it CLEAR and well WRITTEN? Apply the humanize three-pass — remove AI vocabulary, break AI \
         sentence/section structures, add human texture — judging against the Fleet Doc-Writing Style Guide \
         (Document #7, including the A6 humanize-judgment appendix) and the banned-phrases list (Document #8), \
         which you READ AT REVIEW TIME (they are maintained and growing — never a frozen copy).",
    ),
    (
        "risk-security",
        "What could go WRONG? Find security holes, unsafe assumptions, failure modes, data-loss or \
         irreversibility, and operational risks the author did not call out.",
    ),
    (
        "alternatives",
        "What ALTERNATIVES were not considered? Name simpler or stronger approaches the author did not weigh, \
         and any stated choice that lacks a rationale versus its alternatives.",
    ),
];

/// The kickoff for an EPHEMERAL adversarial reviewer session (#374): it acts as the stable board id
/// `reviewer`, reads ONE review's target on ONE angle, files each finding to the review's append-only log
/// (an actionable finding links a child task), then EXITS — no loop. It NEVER transitions the review status:
/// the changes_requested / approved / vetted transitions are the D17 person-review gate, not the reviewer's
/// (board-pm confirmed). `fleet_bin` is the ABSOLUTE path to THIS standalone fleet binary (the `fleet` on PATH
/// may be a different build); `role_path` points at the full role body (`loops/reviewer.md`), the
/// authoritative method — the kickoff carries the review id + angle + identity so the review is well-formed
/// regardless. Pure so the prompt is unit-tested. Mirrors [`build_observer_kickoff`] on the same ephemeral
/// spawn engine (#187/#188/#290) — one review, one angle, then exit.
fn build_reviewer_kickoff(
    review_id: i64,
    angle_key: &str,
    angle_focus: &str,
    role_path: &str,
    fleet_bin: &str,
) -> String {
    format!(
        "You are an EPHEMERAL fleet `reviewer`. Board IDENTITY: you act as the single stable board agent id \
         `reviewer`. FIRST call register_agent 'reviewer' (idempotent). Then author EVERY board write AS \
         `reviewer` by PASSING THE IDENTITY PARAMETER on each call — the board defaults these to NULL: \
         append_review_log / comment_task with author=\"reviewer\", create_task with created_by=\"reviewer\", \
         update_task with actor=\"reviewer\". Never leave author/created_by null. This session makes exactly \
         ONE adversarial review on ONE angle and EXITS — do NOT start a /loop. IMPORTANT: for every `fleet` \
         command use THIS binary by its absolute path — `{fleet_bin}` — NOT the `fleet` on PATH (which may be \
         a different build). Read your full role and method at {role_path} and follow it exactly. \
         YOUR REVIEW: review #{review_id}, ANGLE `{angle_key}`. Call get_review {review_id} to read its \
         target_ref, kind, and source, then read that target IN FULL before forming any finding. YOUR LENS: \
         {angle_focus} Lean HARD on kb_search for the relevant norms/standards. Record each above-floor, \
         evidence-cited, deduped finding on the review's append-only log — `append_review_log` with \
         review_id={review_id}, author=\"reviewer\", a `finding`-type entry naming your angle `{angle_key}` and \
         quoting the exact spot in the target; an ACTIONABLE finding also links a CHILD task (create_task with \
         created_by=\"reviewer\", then reference its id in the entry). Dedup against the review's existing log \
         entries before appending. If your angle is CLEAN (nothing above-floor), append ONE brief no-op finding \
         (angle `{angle_key}`: no findings, and why) so the log records that your angle ran — do not invent a \
         finding to look busy. DO NOT call set_review_status — you NEVER transition the review \
         (open/in_review/changes_requested/approved/closed); the vetted/approved and changes-requested \
         transitions are the person-review gate (D17), decided on your recorded findings, not by you. When you \
         have appended your finding(s) or the no-op, EXIT — this is a one-shot review session, you do not loop."
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

/// The fenced, cooldown-limited injection of ONE wake into a candidate window, shared by the board and
/// file-hub scans: skip if still on cooldown (`"cooldown"`), skip if the pane is actively working
/// (`"working-skip"`, the hard fence), else inject `wake` and stamp (`"re-armed"`); a missing window is
/// `"no-window"`. `wake` is the message to inject — [`WATCHDOG_REARM_WAKE`] for a work-holder that went quiet,
/// [`WATCHDOG_LENGTHEN_WAKE`] for a drained self-poller (#544). Returns the action label and whether a wake was
/// actually sent. Never reaps or restarts.
fn rearm_candidate(
    fleet: &Fleet,
    session: &str,
    name: &str,
    cooldown_base_secs: u64,
    now: u64,
    wake: &str,
) -> (&'static str, bool) {
    if rearm_on_cooldown(read_rearm_stamp(fleet, name), now, cooldown_base_secs) {
        return ("cooldown", false);
    }
    // HARD FENCE (operator ban 2026-09-10 + seq-1387 wake-only): NEVER inject into a pane that is actively
    // working — that would interrupt a heads-down turn. A working candidate is left alone this sweep.
    if window_is_working(session, name) {
        return ("working-skip", false);
    }
    match notify::tmux_inject(session, name, wake) {
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
    self_redeploy: bool,
) {
    // Self-surface (or self-heal) a stale binary: the watchdog is long-running (a timer/loop re-execs this
    // binary), so if its source checkout advanced past the built rev it would silently run old logic (a merged
    // fix not effective until rebuilt). With `--self-redeploy` (#388) ACT on it — rebuild + restart the daemons
    // so the fix goes live without a manual step; otherwise WARN (rebuilding stays out of band). No-op for a
    // deployed binary (no .git) or when already fresh.
    match watchdog_stale_self_action(env!("FLEET_BUILD_REV"), checkout_head_short().as_deref(), self_redeploy) {
        StaleSelfAction::Fresh => {}
        StaleSelfAction::Warn(w) => eprintln!("{w}"),
        StaleSelfAction::Redeploy(w) => {
            eprintln!("{w}");
            eprintln!("fleet watchdog: --self-redeploy set → redeploying the daemons now…");
            match run_redeploy(true) {
                Ok(msg) => {
                    eprintln!("fleet watchdog: self-redeploy: {msg}");
                    // The daemons (including this watchdog's timer) were just restarted onto the fresh binary;
                    // skip the rest of THIS sweep so we don't run stale liveness logic against a just-restarted
                    // daemon set — the next timer fire runs the current binary and does a clean pass.
                    return;
                }
                // A failed/DECLINED self-redeploy (dirty tree, off main, build error) must NOT abort the sweep or
                // skip the liveness pass — stale-but-running liveness beats none. Log and fall through to the
                // normal sweep on the still-stale binary; the next sweep re-triggers once the blocker clears.
                Err(why) => eprintln!("fleet watchdog: self-redeploy skipped: {why}"),
            }
        }
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

/// True iff the roster is non-empty but EVERY agent row lacks a `metadata` object — the signature of a board
/// `/agents` LIST response that dropped per-agent metadata (the by-id endpoint still carries it). In that
/// state the native filter reads `native == false` for every agent, so the whole board-native watchdog scan
/// (re-arm AND observe) silently sees zero agents and prints a benign "all 0 ok" instead of flagging that it
/// is blind. The watchdog must warn loudly on this shape rather than mistake it for an idle-but-healthy
/// board. Pure — unit-tested.
fn roster_metadata_stripped(agents: &[serde_json::Value]) -> bool {
    !agents.is_empty() && agents.iter().all(|a| a.get("metadata").is_none())
}

/// The set of agent ids that OWN at least one `in_progress` task, from a `list_tasks_by_status("in_progress")`
/// projection. `in_progress` is the status that means "actively being worked" — `blocked` (parked on a named
/// dependency) and `done` are a DIFFERENT status and never appear in this list, so an id in this set holds a
/// non-blocked, unfinished, assigned deliverable. This drives the #506 holding-work-while-at-rest guard: an
/// agent that stood down (board `offline`) while its id is in this set left live work behind instead of
/// progressing it or marking it `blocked`/`done`. Pure — unit-tested.
fn inprogress_task_assignees(tasks: &[serde_json::Value]) -> std::collections::BTreeSet<String> {
    tasks
        .iter()
        // A monitor-exempt in_progress task is a legitimate continuous monitor (#506 Phase B / #167), not an
        // unworked deliverable — its owner is not "holding work at rest", so it never contributes here. An
        // owner who ALSO holds a non-exempt in_progress task is still collected via that task.
        .filter(|t| !task_is_monitor_exempt(t))
        .filter_map(|t| t.get("assignee").and_then(serde_json::Value::as_str))
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect()
}

/// Match a changed-file PATH against one SEAM glob (task_579). Glob grammar, deliberately small:
///   `**` matches any run of characters INCLUDING `/` (recursive — spans path segments);
///   `*`  matches any run of characters EXCEPT `/` (a single path segment);
///   every other byte is literal.
/// So `crates/foo/**` matches `crates/foo/a/b.rs`; `crates/*/mod.rs` matches `crates/foo/mod.rs` but not
/// `crates/foo/bar/mod.rs`; `**/perform_arg_ground.rs` matches that file at any depth; a bare literal path
/// matches only itself. Pure — unit-tested.
fn seam_glob_matches(path: &str, glob: &str) -> bool {
    glob_rec(glob.as_bytes(), path.as_bytes())
}

fn glob_rec(pat: &[u8], text: &[u8]) -> bool {
    if let Some(rest) = pat.strip_prefix(b"**") {
        // `**` matches zero or more chars, `/` included: try consuming 0..=all of text.
        if glob_rec(rest, text) {
            return true;
        }
        // `**/` also matches ZERO directories (gitignore semantics: `**/foo` matches `foo` at the root too).
        if rest.strip_prefix(b"/").is_some_and(|after| glob_rec(after, text)) {
            return true;
        }
        return (0..text.len()).any(|i| glob_rec(rest, &text[i + 1..]));
    }
    match pat.first() {
        None => text.is_empty(),
        Some(b'*') => {
            // Single `*`: zero or more chars, but never crossing a `/`.
            let rest = &pat[1..];
            if glob_rec(rest, text) {
                return true;
            }
            let mut i = 0;
            while i < text.len() && text[i] != b'/' {
                if glob_rec(rest, &text[i + 1..]) {
                    return true;
                }
                i += 1;
            }
            false
        }
        Some(&c) => text.first() == Some(&c) && glob_rec(&pat[1..], &text[1..]),
    }
}

/// The subset of `changed` paths that touch any of the agent's declared `seams` (task_579). EMPTY means
/// GREEN — no incoming commit touched the monitor's seam, so the wake is a deterministic no-op and the
/// caller may heartbeat WITHOUT waking the model. Non-empty means CHANGED — wake the model with exactly
/// these paths in hand. Pure — unit-tested.
fn seam_touched<'a>(changed: &'a [String], seams: &[String]) -> Vec<&'a str> {
    changed
        .iter()
        .filter(|p| seams.iter().any(|g| seam_glob_matches(p, g)))
        .map(String::as_str)
        .collect()
}

/// The `stop_reason` of a Claude Code transcript record IF it is a completed assistant turn (`type` ==
/// `assistant`, `message.stop_reason` present). A model-safeguard REFUSAL surfaces here as stop_reason
/// `refusal` (verified against a live wedge — v-s2n-quic 2026-09-30, whose refused turns also carried
/// `isApiErrorMessage:true`). Returns None for any non-assistant record or a turn with no stop_reason. Pure.
fn assistant_stop_reason(rec: &serde_json::Value) -> Option<String> {
    if rec.get("type").and_then(serde_json::Value::as_str) != Some("assistant") {
        return None;
    }
    rec.get("message")
        .and_then(|m| m.get("stop_reason"))
        .and_then(serde_json::Value::as_str)
        .map(String::from)
}

/// A SAFEGUARD WEDGE (task_582): the agent's loop is stuck submitting turns the model keeps REFUSING.
/// Signature = a run of at least `threshold` consecutive `refusal` stop_reasons at the TAIL of the
/// assistant-turn sequence (its most recent turns). This is a DISTINCT failure from the idle/no-output stall
/// the liveness check catches: a refused agent keeps advancing `last_seen` because it is actively submitting
/// (and getting refused), so the watchdog reads it as healthy while it burns (v-s2n-quic burned wedged until
/// a manual spin-down+spin-up). `reasons` is the ordered assistant stop_reasons, oldest-first. Pure —
/// unit-tested.
fn safeguard_wedge(reasons: &[String], threshold: usize) -> bool {
    threshold > 0
        && reasons.len() >= threshold
        && reasons.iter().rev().take(threshold).all(|r| r == "refusal")
}

/// True if a task carries the board's derived `monitor_exempt` flag (v-task-board #167): a genuinely
/// continuous monitor, marked via `metadata.monitor_exempt = true` and surfaced as a top-level bool on both
/// `list_tasks` and `get_task` (absent/false by default). A monitor-exempt task is meant to stay `in_progress`
/// without per-tick progress, so it is excluded from BOTH the nudge cadence (#478) and the #506
/// holding-work-at-rest violation — read straight from the list projection, no per-task metadata fetch. Pure.
fn task_is_monitor_exempt(task: &serde_json::Value) -> bool {
    task.get("monitor_exempt")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
}

/// True if a task is PARKED on a blocker in the list projection: it carries a non-empty `blocked_on_kind`
/// (e.g. `external` for an infra/no-owner wait — v-task-board seq-7873 / task-board#178 — or `operator` /
/// `task`). A parked task is legitimately waiting, not a stalled deliverable, so the nudge cadence skips it
/// (mirrors [`board::task_is_actionable`], which treats any blocker as not-actionable). Read straight from the
/// list record — no per-task fetch. Pure — unit-tested.
fn task_is_parked_on_blocker(task: &serde_json::Value) -> bool {
    task.get("blocked_on_kind")
        .and_then(serde_json::Value::as_str)
        .is_some_and(|k| !k.is_empty())
}

/// #544: assignees of monitor-exempt `in_progress` tasks — a DELIBERATE continuous monitor. Such an agent can
/// legitimately show 0 "actionable" tasks ([`board::Board::open_task_count`] excludes its exempt task) yet is
/// meant to keep polling at its cadence, so the drained-self-poller lengthen must EXCLUDE it. Pure —
/// unit-tested.
fn monitor_exempt_task_owners(tasks: &[serde_json::Value]) -> std::collections::BTreeSet<String> {
    tasks
        .iter()
        .filter(|t| task_is_monitor_exempt(t))
        .filter_map(|t| t.get("assignee").and_then(serde_json::Value::as_str))
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect()
}

/// How the watchdog reads an agent's `metadata.native`, fail-safe (task_500).
enum NativeVerdict {
    /// `native: true` — a board-native agent, managed/observed as normal.
    Native,
    /// `native: false` — a deliberately-marked file-hub row; skip it (intended).
    NotNative,
    /// `native` absent, `metadata` entirely missing, or `native` present but not a bool — the roster shape is
    /// unknown. NOT the same as `false`: the task_418 roster compaction dropped `metadata` from the list
    /// projection, so a hard `unwrap_or(false)` read `native=false` for every agent and silently disabled the
    /// whole observer cadence. The caller must treat Unknown as fail-safe (process + warn), never as a skip.
    Unknown,
}

/// Read `metadata.native` as a tri-state (task_500 fail-safe). An EXPLICIT `native:false` is distinct from an
/// ABSENT/malformed field: the former is a deliberate file-hub marker to skip, the latter is a roster-shape
/// unknown the watchdog must not silently treat as not-native. Pure — unit-tested.
fn read_native(md: Option<&serde_json::Value>) -> NativeVerdict {
    match md.and_then(|m| m.get("native")) {
        Some(v) => match v.as_bool() {
            Some(true) => NativeVerdict::Native,
            Some(false) => NativeVerdict::NotNative,
            None => NativeVerdict::Unknown,
        },
        None => NativeVerdict::Unknown,
    }
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
    // Resilience: a roster where NO agent carries metadata means the board /agents LIST endpoint dropped
    // per-agent metadata (the by-id endpoint still has it). Every agent then reads native == false and the
    // whole board-native scan (re-arm AND observe) is silently blind — it would otherwise print a benign
    // "all 0 board-native agents ok". Warn loudly so this failure mode can never hide again.
    if roster_metadata_stripped(agents) {
        eprintln!(
            "-- WARNING: board /agents returned {} agent(s) but NONE carry metadata — the board-native scan \
             (re-arm + observe) sees zero native agents and is effectively DISABLED. The by-id endpoint has \
             metadata; the LIST endpoint is omitting it. No observer will spawn and no board agent will be \
             re-armed until the /agents list includes per-agent metadata.",
            agents.len()
        );
    }
    // Observation (#187): per-agent transcript-growth threshold (lines/records). Read once per sweep.
    let observe_threshold = std::env::var("CDZ_OBSERVE_LINES")
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(OBSERVE_LINES_DEFAULT);
    // Collected observation candidates: (target agent, stood_down, decision). Displayed after the table, and
    // — with --spawn (#188) — the highest-growth few are launched as ephemeral observers (cap + cooldown).
    let mut obs: Vec<(String, bool, ObserveDecision)> = Vec::new();
    // #506 holding-work-while-at-rest guard: the set of agents that own an `in_progress` task, read once per
    // sweep. An agent that is `offline` (stood down) while it appears here left a live, non-blocked deliverable
    // behind — the v-bolero case. Best-effort: a query error degrades to an empty set (no false violations)
    // rather than failing the whole watchdog.
    // Both sets are derived from ONE in_progress query: `inprogress_owners` (non-exempt owners, the #506
    // holding-work signal) and `monitor_exempt_owners` (deliberate continuous monitors, excluded from the #544
    // drained-self-poller lengthen). A query error degrades both to empty (no false violations / no false
    // lengthens) rather than failing the whole watchdog.
    let (inprogress_owners, monitor_exempt_owners) = match board.list_tasks_by_status("in_progress") {
        Ok(tasks) => (inprogress_task_assignees(&tasks), monitor_exempt_task_owners(&tasks)),
        Err(_) => (std::collections::BTreeSet::new(), std::collections::BTreeSet::new()),
    };
    println!(
        "{:<28} {:<8} {:<7} {:<5} {:<8} {:<12} last_seen",
        "agent", "interval", "age", "open", "verdict", "action"
    );
    let mut flagged = 0usize;
    let mut rearmed = 0usize;
    let mut never_ticked_count = 0usize;
    let mut holding_work_count = 0usize;
    // task_582 safeguard-wedge (report-only this slice): agents whose newest session is stuck in a trailing
    // run of model-safeguard refusals. Collected across the sweep and surfaced in one WARNING below.
    let mut wedged_ids: Vec<String> = Vec::new();
    let mut native = 0usize;
    // task_500: ids whose metadata.native was ABSENT/malformed (NOT an explicit false) — collected to warn.
    let mut unknown_native_ids: Vec<String> = Vec::new();
    for a in agents {
        let md = a.get("metadata");
        let id = a.get("id").and_then(|v| v.as_str()).unwrap_or("?");
        // task_500 fail-safe native gate. The task_418 roster compaction dropped `metadata` from the list
        // projection, so a hard `unwrap_or(false)` read `native=false` for every agent and this loop skipped
        // ALL of them — silently zeroing the observe-candidate set and disabling the whole observer cadence. So
        // distinguish an EXPLICIT `native:false` (a deliberately-marked file-hub row — skip, as intended) from
        // an ABSENT/malformed native (roster shape unknown): an UNKNOWN must NOT hard-skip — process it (degrade
        // to over-observing) and surface it in a loud WARNING below, never a silent zero.
        match read_native(md) {
            NativeVerdict::NotNative => continue,
            NativeVerdict::Unknown => unknown_native_ids.push(id.to_string()),
            NativeVerdict::Native => {}
        }
        // Host affinity: skip agents this box should not manage — a DIFFERENT-box pin always, and (under
        // --pinned-only) unpinned run-anywhere agents too, so a secondary box never re-arms/observes an agent
        // whose tmux window / transcript lives elsewhere. See [`watchdog_manages_agent`].
        if !watchdog_manages_agent(md, &host, pinned_only) {
            continue;
        }
        native += 1;
        let interval_str = md
            .and_then(|m| m.get("interval"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let ls = a.get("last_seen").and_then(|v| v.as_str()).unwrap_or("");
        let interval_secs = parse_interval_secs(interval_str).unwrap_or(0);
        let age_secs = last_seen_age_secs(ls, now);
        let (verdict, age_str) = match age_secs {
            Some(age) => (watchdog_verdict(age, interval_secs), format!("{}m", age / 60)),
            None => ("?", "?".to_string()),
        };
        // Best-effort open assigned-task count (the second signal); a query error degrades to 0/"?" and
        // simply doesn't flag on the task dimension rather than failing the whole watchdog.
        let (open_tasks, open_str) = match board.open_task_count(id) {
            Ok(n) => (n, n.to_string()),
            Err(_) => (0, "?".to_string()),
        };
        // `stood_down` (board `offline`) = a RETIRED/spun-down agent → drives the observe spin-down trigger.
        // A drained agent's re-arm suppression is handled by [`is_retighten_candidate`] on its open-task count
        // (a zero queue is never a candidate), so no separate presence gate is needed here.
        let status = a.get("status").and_then(serde_json::Value::as_str);
        let stood_down = status == Some("offline");
        // Launch-crash signal (#417): an EXPECTED-RUNNING agent (not staged, not deliberately offline) whose
        // last_seen never advanced past created_at is dead-on-arrival, not idle. It is surfaced below
        // regardless of the open-task / stale gates the older signals apply — a crash-looping agent with an
        // empty queue slips past both, which is exactly how the board-triage/board-follow-up outage went
        // unflagged. Staged (registered-but-unlaunched) and offline agents have legitimately not ticked.
        let created_at = a.get("created_at").and_then(serde_json::Value::as_str).unwrap_or("");
        let never_ticked = !agent_is_staged(md)
            && !stood_down
            && agent_never_ticked(created_at, ls, now, WATCHDOG_NEVER_TICKED_GRACE_SECS);
        // #506 holding-work-while-at-rest violation: the agent stood down (offline) while it still owns a live
        // `in_progress` task — the v-bolero case (created work, then slept). `in_progress` is actively-worked,
        // so standing down on it (instead of progressing it or marking it `blocked`/`done`) is a status-honesty
        // violation, not a legitimate stand-down. Flagged + re-armed below so it re-enters its loop.
        let holding_work_at_rest = stood_down && inprogress_owners.contains(id);
        // task_582 safeguard-wedge scan (report-only). The wedge's signature is that last_seen keeps
        // ADVANCING — the agent LOOKS healthy, so the age/verdict/stale gates never flag it — while its last
        // few assistant turns are all stop_reason=refusal (a model-safeguard reject loop, burning turns). So
        // scan here, BEFORE the stale-only skip, for every running (not stood-down, not never-ticked) managed
        // agent. The read is bounded + fail-safe (a non-local or unreadable transcript yields false). Detected
        // agents are surfaced in one WARNING after the loop; auto spin-down/spin-up is task_582's next slice.
        if !stood_down
            && !never_ticked
            && agent_is_safeguard_wedged(id, SAFEGUARD_WEDGE_THRESHOLD, SAFEGUARD_WEDGE_TAIL)
        {
            wedged_ids.push(id.to_string());
        }
        // Observation (#187): check transcript growth BEFORE the stale-only skip below — a spin-down (offline)
        // agent is not a re-arm candidate, so it would be skipped, yet its closing read is exactly what the
        // mandatory spin-down trigger must catch. Report-only this slice (no spawn / no watermark advance).
        if observe && let Some(d) = observe_candidate(&fleet, id, stood_down, observe_threshold) {
            obs.push((id.to_string(), stood_down, d));
        }
        let retighten = is_retighten_candidate(verdict, open_tasks, interval_secs);
        // #535 work-driven tight cadence: a work-holder quiet beyond the short work cadence is a candidate even
        // on a moderate interval that `is_retighten_candidate` would call "already tight".
        let work_driven = work_driven_rearm(open_tasks, age_secs, stood_down);
        // #544 drained self-poller: an at-rest agent (0 actionable tasks) on a short interval that a
        // build_kickoff edit can't reach — inject the lengthen instruction. Excludes reactive responders and
        // deliberate continuous monitors (a monitor_exempt-task owner is MEANT to poll).
        let reactive = md
            .and_then(|m| m.get("reactive"))
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        // #544: a patrol/sweep agent (board-follow-up / board-triage) runs 0 assigned tasks at a short cadence
        // BY DESIGN — its charter is proactive event-less patrol, so it must never be lengthened to
        // event-wake-only. Opt-out via agent-level metadata.patrol.
        let patrol = md
            .and_then(|m| m.get("patrol"))
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        let drained_idle = drained_idle_candidate(
            open_tasks,
            interval_secs,
            stood_down,
            reactive,
            monitor_exempt_owners.contains(id),
            patrol,
            verdict,
        );
        if stale_only && !retighten && !work_driven && !drained_idle && !never_ticked && !holding_work_at_rest {
            continue;
        }
        // A NEVER-TICKED agent takes priority: a wake cannot recover a loop that never started (no live pane
        // to re-arm — the #412/#420 lesson), so it is flagged for investigation + relaunch, never wake-injected.
        // A HOLDING-WORK-AT-REST violation (#506) is next: the agent stood down with a live in_progress task, so
        // re-arm it back into its loop (with --rearm) — it must progress the work or re-state it (blocked/done).
        // Otherwise, with --rearm, ACT on a retighten candidate: a cooldown-limited, pane-fenced wake so it
        // runs a tick now (never reaps/restarts). See [`rearm_candidate`].
        let action = if never_ticked {
            flagged += 1;
            never_ticked_count += 1;
            "NEVER-TICKED"
        } else if holding_work_at_rest {
            flagged += 1;
            holding_work_count += 1;
            if rearm {
                let (_act, did) = rearm_candidate(&fleet, &session, id, interval_secs, now_unix, WATCHDOG_REARM_WAKE);
                if did {
                    rearmed += 1;
                }
                "HOLDS-WORK@REST→woke"
            } else {
                "HOLDS-WORK@REST"
            }
        } else if retighten || work_driven {
            flagged += 1;
            if rearm {
                // A work-holder is re-kicked on the tight work cadence (cooldown-limited to the 5-min floor),
                // not its long idle interval — that is the #535 fix. A pure retighten candidate keeps its
                // interval-based cooldown. Either way rearm_candidate fences a working pane.
                let cooldown_base = if work_driven {
                    WATCHDOG_WORK_CADENCE_SECS as u64
                } else {
                    interval_secs
                };
                let (act, did) = rearm_candidate(&fleet, &session, id, cooldown_base, now_unix, WATCHDOG_REARM_WAKE);
                if did {
                    rearmed += 1;
                }
                act
            } else if work_driven && !retighten {
                "WORK-CAND"
            } else {
                "candidate"
            }
        } else if drained_idle {
            // #544: a drained self-poller — inject the lengthen instruction ONCE (cooldown base = the long idle
            // cadence, so it is not re-poked before it has had a chance to set-interval itself). Pane-fenced by
            // rearm_candidate. Once it lengthens past WATCHDOG_LONG_INTERVAL_SECS it no longer qualifies.
            flagged += 1;
            if rearm {
                let (act, did) = rearm_candidate(
                    &fleet,
                    &session,
                    id,
                    WATCHDOG_LONG_INTERVAL_SECS,
                    now_unix,
                    WATCHDOG_LENGTHEN_WAKE,
                );
                if did {
                    rearmed += 1;
                    "LENGTHEN"
                } else {
                    act
                }
            } else {
                "DRAINED-CAND"
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
            "-- {native} board-native agent(s); {flagged} re-arm/retighten candidate(s) (overdue heartbeat, open tasks on a long interval, or holding actionable work past the tight work cadence); pass --rearm to wake them"
        );
    }
    if never_ticked_count > 0 {
        // A never-ticked agent is a launch-time crash, not a lapsed loop: a wake cannot fix it. Surface it
        // loudly (even in --stale-only / --rearm runs) so it routes to investigation + relaunch, not a no-op
        // wake — the board-triage/board-follow-up outage (#412/#417) that stayed silent for ~3.7h.
        println!(
            "-- WARNING: {never_ticked_count} agent(s) NEVER-TICKED (launched but last_seen == created_at past the {WATCHDOG_NEVER_TICKED_GRACE_SECS}s grace) — a wake will NOT help; investigate the pane + relaunch (see #417)"
        );
    }
    if holding_work_count > 0 {
        // #506: an agent that stood down (offline) while still owning a live in_progress task. in_progress means
        // actively-worked, so this is a status-honesty violation, not a legitimate rest — surfaced loudly (and
        // re-armed under --rearm) so it re-enters its loop and either progresses the work or re-states it as
        // blocked/done. The prevention companion is the AGENTS-fleet status-honesty contract line (#506 Layer 1).
        println!(
            "-- WARNING: {holding_work_count} agent(s) STOOD DOWN while holding a live in_progress assigned task (#506 violation) — an in_progress task is actively-worked; they must progress it or mark it blocked/done. Re-armed under --rearm."
        );
    }
    if !wedged_ids.is_empty() {
        // task_582: a model-safeguard wedge keeps last_seen advancing, so the liveness/age checks above miss
        // it — surface it loudly. Report-only this slice: recovery is a manual spin-down/spin-up; the next
        // task_582 increment wires an auto spin-down + spin-up (cooldown-fenced) off this same detection.
        println!(
            "-- WARNING: {} agent(s) SAFEGUARD-WEDGED (last {SAFEGUARD_WEDGE_THRESHOLD} assistant turns all stop_reason=refusal; last_seen keeps advancing so the liveness check misses it): {}. Recover: `fleet spin-down <agent> --apply --force` then `fleet spin-up <agent> --apply`.",
            wedged_ids.len(),
            wedged_ids.join(", ")
        );
    }
    if !unknown_native_ids.is_empty() {
        // task_500 fail-safe signal: an absent/malformed metadata.native (not an explicit false) means the
        // roster projection may have dropped the field — the task_418 class of regression. These agents were
        // processed anyway (over-observed, not silently skipped), but surface them loudly so the roster shape
        // gets fixed: stamp each native:true (board-native) or native:false (file-hub) so the gate is explicit.
        println!(
            "-- WARNING: {} agent(s) have an ABSENT/malformed metadata.native and were treated as managed (fail-safe over-observe, task_500) — the roster projection may be dropping the field: {}. Fix: stamp each native:true or native:false so the roster is explicit.",
            unknown_native_ids.len(),
            unknown_native_ids.join(", ")
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
        // from this scan (see `native_ids`). `pending` (unread inbox items) is this path's held-work count —
        // a drained inbox is never a candidate (the same #332 guard as the board path).
        let retighten = is_retighten_candidate(verdict, pending, interval_secs);
        if stale_only && !retighten {
            continue;
        }
        let action = if retighten {
            flagged += 1;
            if rearm {
                let (act, did) = rearm_candidate(&fleet, &session, &a.name, interval_secs, now, WATCHDOG_REARM_WAKE);
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

/// task_579: seam-check a monitor vertical (see [`Cmd::SeamCheck`]). Reads the agent's declared seam globs +
/// worktree from its board record, ff-syncs the worktree to `origin/main`, and reports whether any incoming
/// commit touched the seam — via EXIT CODE so a kickoff wrapper can gate the model wake without parsing text:
/// 0 = GREEN (heartbeat, skip the model), 3 = CHANGED (wake the model; seam paths printed), 1 = error / no
/// seam declared. The verdict itself is [`seam_touched`] (pure, unit-tested); this wrapper is the git + board
/// IO around it.
fn seam_check(agent: &str, no_fetch: bool) {
    let board = board::Board::connect().unwrap_or_else(|e| {
        eprintln!("fleet seam-check: {e}");
        std::process::exit(1);
    });
    let rec = board.get_agent(agent).unwrap_or_else(|e| {
        eprintln!("fleet seam-check: get_agent '{agent}' failed: {e}");
        std::process::exit(1);
    });
    let md = rec.get("metadata");
    let seams: Vec<String> = md
        .and_then(|m| m.get("seam"))
        .and_then(|v| v.as_array())
        .map(|arr| arr.iter().filter_map(|s| s.as_str().map(String::from)).collect())
        .unwrap_or_default();
    if seams.is_empty() {
        eprintln!(
            "fleet seam-check '{agent}': no metadata.seam globs declared — cannot gate this monitor; wake the \
             model. Declare the vertical's seam file globs in its board metadata.seam to enable gating."
        );
        std::process::exit(1);
    }
    let worktree = md.and_then(|m| m.get("worktree")).and_then(|v| v.as_str()).unwrap_or(".");
    let git = |args: &[&str]| -> Result<String, String> {
        let out = std::process::Command::new("git")
            .current_dir(worktree)
            .args(args)
            .output()
            .map_err(|e| format!("git {args:?}: {e}"))?;
        if !out.status.success() {
            return Err(format!("git {args:?}: {}", String::from_utf8_lossy(&out.stderr).trim()));
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    };
    let fetch = if no_fetch { Ok(String::new()) } else { git(&["fetch", "origin", "main"]) };
    if let Err(e) = fetch {
        eprintln!("fleet seam-check '{agent}': {e}");
        std::process::exit(1);
    }
    // Files in commits reachable from origin/main but not HEAD = what a ff-sync would bring in.
    let diff = git(&["diff", "--name-only", "HEAD..origin/main"]).unwrap_or_else(|e| {
        eprintln!("fleet seam-check '{agent}': {e}");
        std::process::exit(1);
    });
    let changed: Vec<String> =
        diff.lines().map(str::trim).filter(|l| !l.is_empty()).map(String::from).collect();
    let matched = seam_touched(&changed, &seams);
    if matched.is_empty() {
        println!(
            "seam-check '{agent}': GREEN — {} incoming file(s), none on seam ({} glob(s)); heartbeat, do NOT wake the model",
            changed.len(),
            seams.len()
        );
        std::process::exit(0);
    }
    println!("seam-check '{agent}': CHANGED — {} seam-touching path(s), wake the model:", matched.len());
    for p in &matched {
        println!("  {p}");
    }
    std::process::exit(3);
}

/// task_582 watchdog-scan tuning. `*_THRESHOLD` / `*_TAIL` MATCH the `Cmd::SafeguardCheck` defaults (3 / 80)
/// so the per-sweep scan and the one-shot command agree on what a wedge is. `*_TAIL_BYTES` bounds the per-agent
/// read: a session transcript can be many MB, so the watchdog reads only the tail (the trailing refusal turns
/// are small JSONL lines, so 256 KiB comfortably covers the last `TAIL` lines) rather than the whole file.
const SAFEGUARD_WEDGE_THRESHOLD: usize = 3;
const SAFEGUARD_WEDGE_TAIL: usize = 80;
const SAFEGUARD_WEDGE_TAIL_BYTES: u64 = 256 * 1024;

/// Extract the assistant-turn stop_reasons from the last `tail` lines of a JSONL transcript `content`
/// (chronological order preserved — newest last). Pure over the string so it is unit-tested without files;
/// task_582 shares it between the one-shot `safeguard_check` command and the per-sweep watchdog wedge scan.
fn tail_stop_reasons(content: &str, tail: usize) -> Vec<String> {
    let mut lines: Vec<&str> = content.lines().rev().take(tail).collect();
    lines.reverse();
    lines
        .into_iter()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter_map(|rec| assistant_stop_reason(&rec))
        .collect()
}

/// Read the last `max_bytes` of a file as text, dropping a leading partial line when the read started
/// mid-file. A session transcript can be many MB, so the per-sweep watchdog wedge scan must NOT read the whole
/// file for each agent each sweep — only the tail carries the recent assistant turns. `None` on an IO error
/// (the caller treats that as "not wedged", never a false flag).
fn read_file_tail(path: &Path, max_bytes: u64) -> Option<String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(path).ok()?;
    let len = f.metadata().ok()?.len();
    let start = len.saturating_sub(max_bytes);
    f.seek(SeekFrom::Start(start)).ok()?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf).ok()?;
    let text = String::from_utf8_lossy(&buf).into_owned();
    // Only when we started mid-file is the first line a (likely partial) fragment to drop.
    if start > 0 {
        Some(text.split_once('\n').map(|(_partial, rest)| rest.to_string()).unwrap_or(text))
    } else {
        Some(text)
    }
}

/// task_582: is the agent's newest session wedged by a model-safeguard refusal run? The non-printing core the
/// watchdog calls per managed agent each sweep. Locates the newest session, reads only its bounded tail
/// ([`read_file_tail`]), and applies [`safeguard_wedge`] to the trailing assistant stop_reasons. FAIL-SAFE: a
/// missing transcript, a different host (no local session), or an IO error returns `false` — a healthy agent
/// is never false-flagged, so this is safe to run on every managed agent each sweep.
fn agent_is_safeguard_wedged(agent: &str, threshold: usize, tail: usize) -> bool {
    let sessions = transcripts::locate_sessions(agent);
    let Some(path) = sessions.first() else {
        return false;
    };
    let Some(content) = read_file_tail(path, SAFEGUARD_WEDGE_TAIL_BYTES) else {
        return false;
    };
    safeguard_wedge(&tail_stop_reasons(&content, tail), threshold)
}

/// task_582: scan an agent's newest session transcript tail for a safeguard wedge (see [`Cmd::SafeguardCheck`]).
/// Locates the agent's sessions via [`transcripts::locate_sessions`], reads the last `tail` lines of the
/// newest one, extracts the assistant-turn stop_reasons, and applies [`safeguard_wedge`]. Reports via EXIT
/// CODE for a supervisor: 0 = healthy, 3 = WEDGED, 1 = error / no transcript. The verdict is pure
/// ([`safeguard_wedge`] / [`assistant_stop_reason`]); this wrapper is the file IO around it.
fn safeguard_check(agent: &str, threshold: usize, tail: usize) {
    let sessions = transcripts::locate_sessions(agent);
    let Some(path) = sessions.first() else {
        eprintln!("fleet safeguard-check '{agent}': no session transcript found (nothing to scan)");
        std::process::exit(1);
    };
    let content = std::fs::read_to_string(path).unwrap_or_else(|e| {
        eprintln!("fleet safeguard-check '{agent}': reading {}: {e}", path.display());
        std::process::exit(1);
    });
    // Last `tail` lines, in chronological order, parsed to assistant stop_reasons only (shared with the
    // per-sweep watchdog scan via `tail_stop_reasons`).
    let reasons = tail_stop_reasons(&content, tail);
    if safeguard_wedge(&reasons, threshold) {
        println!(
            "safeguard-check '{agent}': WEDGED — the last {threshold} assistant turns are all stop_reason=refusal \
             (model-safeguard wedge; last_seen keeps advancing so the liveness check misses it). Recover with \
             `fleet spin-down {agent} --apply --force` then `fleet spin-up {agent} --apply`."
        );
        std::process::exit(3);
    }
    let refusals = reasons.iter().filter(|r| r.as_str() == "refusal").count();
    println!(
        "safeguard-check '{agent}': OK — {} assistant turn(s) in the last {tail} lines, {refusals} refusal(s), no trailing run >= {threshold}",
        reasons.len()
    );
    std::process::exit(0);
}

/// Match a bare SESSION ID against a set of located session files by id (`session_id_of`). Returns the file
/// whose id equals `sid`, or `None`. Pure over `candidates` — unit-tested.
fn match_session_id_in(sid: &str, candidates: &[PathBuf]) -> Option<PathBuf> {
    candidates
        .iter()
        .find(|c| transcripts::session_id_of(c).as_str() == sid)
        .cloned()
}

/// Resolve a `--session` argument to a transcript file path: an existing FILE PATH is used verbatim,
/// otherwise the argument is treated as a BARE SESSION ID and matched by id against the agent's located
/// session files (`<agent-projects-dir>/<sid>.jsonl`). Returns `None` when a bare id matches no known
/// session. This is the #360 fix: `--session <sid>` is the form the observer kickoff and observer.md
/// document, but it used to fail with "No such file or directory" because the argument was used verbatim as
/// a path — recurring friction on the observer self-improve loop. The `exists()` check is the only I/O; the
/// id match is pure ([`match_session_id_in`]).
fn resolve_session_arg(session: &Path, agent: &str) -> Option<PathBuf> {
    if session.exists() {
        return Some(session.to_path_buf());
    }
    match_session_id_in(&session.to_string_lossy(), &transcripts::locate_sessions(agent))
}

/// Render an agent's session transcript faithfully (see [`transcripts`]). Resolves the session file
/// (`--session` as a file path OR a bare session id, else the watermark's session, else the agent's newest),
/// parses it, windows it by the
/// `--since` record offset backed up by `--overlap`, renders + scrubs, and prints the advancing watermark.
/// The watermark offset is a RECORD index (parsed JSONL records already observed), printed as
/// `<session-id>:<record-count>` in the footer.
fn transcripts_cmd(agent: &str, session: Option<&Path>, since: Option<&str>, overlap: usize, harness: &str) {
    // Select the harness renderer up front so an unknown value fails before we touch the filesystem.
    let harness: Box<dyn transcripts::Harness> = match harness {
        "codex" => Box::new(transcripts::Codex),
        "claude" | "claude-code" => Box::new(transcripts::ClaudeCode),
        other => {
            eprintln!("fleet transcripts: unknown --harness '{other}' (expected 'claude' or 'codex')");
            std::process::exit(1);
        }
    };
    let (path, want_offset) = match session {
        Some(p) => {
            let off = since.map(|s| transcripts::parse_watermark(s).1).unwrap_or(0);
            match resolve_session_arg(p, agent) {
                Some(found) => (found, off),
                None => {
                    eprintln!(
                        "fleet transcripts: --session '{}' is neither an existing file nor a known session id for '{agent}'",
                        p.display()
                    );
                    std::process::exit(1);
                }
            }
        }
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
        harness.id()
    );
    print!("{}", transcripts::render(windowed, harness.as_ref()));
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

/// How a board agent is reachable by a push-wake (#386). Decided purely from its `webhook_url` and whether
/// the board has a live reverse tunnel for it. Pure — unit-tested.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum WakePath {
    /// A non-empty `webhook_url` — the board POSTs events straight to it (green-resident agents point it at
    /// their local fleet-notify).
    Webhook,
    /// No webhook, but the board has a LIVE tunnel for the agent — an off-LAN agent woken down the tunnel.
    Tunnel,
    /// Neither — the agent only discovers work on its (slow) loop interval. The regression #386 forbids.
    PollOnly,
}

impl WakePath {
    fn label(self) -> &'static str {
        match self {
            WakePath::Webhook => "webhook",
            WakePath::Tunnel => "tunnel",
            WakePath::PollOnly => "POLL-ONLY",
        }
    }
}

/// Classify an agent's push-wake path: a non-empty `webhook_url` wins (a direct POST target); else a live
/// tunnel covers it; else it is poll-only. A whitespace-only webhook is treated as absent. Pure — unit-tested.
fn classify_wake_path(webhook_url: Option<&str>, has_live_tunnel: bool) -> WakePath {
    let has_webhook = webhook_url.map(|u| !u.trim().is_empty()).unwrap_or(false);
    if has_webhook {
        WakePath::Webhook
    } else if has_live_tunnel {
        WakePath::Tunnel
    } else {
        WakePath::PollOnly
    }
}

/// Whether an agent is a PERSISTENT LOOP agent EXPECTED to be running (and so subject to the no-poll rule).
/// Excludes, because none has an event loop a missing wake path would strand: a not-running status (`offline`
/// stood down, `done` finished, or `cancelled`); a pending/acted stand-down request (winding down before the
/// status flips); a STAGED reserve helper (`metadata.staged == true`, #392 — minted ahead of need, its wake
/// wired at launch, so the watchdog already skips it per PR #122 and the audit mirrors that); and a non-loop
/// `kind` — an `assistant` is an interactive human-driven session and an `observer` is spawned per watchdog
/// sweep and exits, so neither waits on events. Pure — unit-tested.
fn agent_expected_running(agent: &serde_json::Value) -> bool {
    let status = agent.get("status").and_then(serde_json::Value::as_str).unwrap_or("");
    if matches!(status.to_ascii_lowercase().as_str(), "offline" | "done" | "cancelled") {
        return false;
    }
    let standing_down = agent
        .get("stand_down_requested_at")
        .map(|v| !v.is_null())
        .unwrap_or(false);
    if standing_down {
        return false;
    }
    if agent_is_staged(agent.get("metadata")) {
        return false;
    }
    let kind = agent.get("kind").and_then(serde_json::Value::as_str).unwrap_or("");
    !matches!(kind, "assistant" | "observer")
}

/// `fleet wake-audit` (#386): confirm every expected-running board agent has a push-wake path. Cross the agent
/// roster's `webhook_url` (from each agent's detail record) with the board's live tunnel set (`GET /tunnels`)
/// and classify each. Prints a per-agent verdict and exits non-zero if any expected-running agent is poll-only,
/// so a supervisor can gate on "no poll-only agents." Read-only.
fn wake_audit(verbose: bool) {
    let board = board::Board::connect().unwrap_or_else(|e| {
        eprintln!("fleet wake-audit: board unavailable ({e}); cannot audit wake paths");
        std::process::exit(1);
    });
    let roster = board.list_agents().unwrap_or_else(|e| {
        eprintln!("fleet wake-audit: board roster query failed ({e})");
        std::process::exit(1);
    });
    let tunnels = board.tunnel_agent_ids().unwrap_or_else(|e| {
        eprintln!("fleet wake-audit: board /tunnels query failed ({e}); cannot tell tunnel coverage");
        std::process::exit(1);
    });

    let mut poll_only: Vec<String> = Vec::new();
    let (mut n_webhook, mut n_tunnel, mut n_skipped) = (0usize, 0usize, 0usize);
    // Stable order so the report reads the same run-to-run.
    let mut ids: Vec<String> = roster
        .iter()
        .filter_map(|a| a.get("id").and_then(serde_json::Value::as_str).map(str::to_string))
        .collect();
    ids.sort();

    println!("fleet wake-audit: {} agent(s), {} live tunnel(s)", ids.len(), tunnels.len());
    for id in &ids {
        // The list record omits webhook_url; the DETAIL record carries it. Fall back to the (already-fetched)
        // list record only if the detail fetch fails, so a transient error never silently flags an agent.
        let detail = board.get_agent(id).ok();
        let expected = detail
            .as_ref()
            .map(agent_expected_running)
            .unwrap_or(true);
        if !expected {
            n_skipped += 1;
            if verbose {
                println!("  - {id}: offline / standing down (skipped)");
            }
            continue;
        }
        let webhook = detail
            .as_ref()
            .and_then(|d| d.get("webhook_url").and_then(serde_json::Value::as_str))
            .map(str::to_string);
        let path = classify_wake_path(webhook.as_deref(), tunnels.contains(id));
        match path {
            WakePath::Webhook => {
                n_webhook += 1;
                if verbose {
                    println!("  - {id}: webhook ({})", webhook.as_deref().unwrap_or(""));
                }
            }
            WakePath::Tunnel => {
                n_tunnel += 1;
                if verbose {
                    println!("  - {id}: tunnel");
                }
            }
            WakePath::PollOnly => {
                poll_only.push(id.clone());
                println!("  - {id}: {} — no webhook_url and no live tunnel", WakePath::PollOnly.label());
            }
        }
    }

    println!(
        "\nsummary: {n_webhook} webhook, {n_tunnel} tunnel, {} poll-only, {n_skipped} stood-down",
        poll_only.len()
    );
    if poll_only.is_empty() {
        println!("PASS: every expected-running agent has a push-wake path (no poll-only agents).");
    } else {
        eprintln!(
            "FAIL: {} poll-only agent(s) — wire a webhook_url (green-resident) or a tunnel (off-LAN): {}",
            poll_only.len(),
            poll_only.join(", ")
        );
        std::process::exit(1);
    }
}

/// The `author` a nudge comment is posted as — also the marker `nudge_last_secs` searches a task's prior
/// comments for, to find this daemon's own last nudge (the cooldown clock; #478).
const NUDGE_AUTHOR: &str = "fleet-nudge-daemon";
/// #540 inc2: where an unassigned-or-gone-owner stale task is ROUTED — the router that owns assignment. It is
/// event-woken on the reassignment (task.assigned), so this needs no polling on board-pm's side.
const NUDGE_ROUTER: &str = "board-pm";
/// task_540 follow-on (board-pm greenlit): the EXTENDED re-nudge cooldown applied when the assignee has
/// acknowledged a queued todo since the last nudge — a fresh ack/ETA buys this much quiet instead of the
/// normal ~1h, so an acknowledged-queued backlog is not re-nudged hourly. 4h to start (tune toward 4–6h);
/// the anti-parking revert to the normal cadence on a stale ack is in [`stale_task_should_nudge`].
const ACKED_NUDGE_COOLDOWN_HOURS: f64 = 4.0;

/// Whether a task idle for `idle_secs` should be nudged now, given `last_nudge_secs` (the age of this
/// daemon's own most recent nudge comment on it, if any). Pure — unit-tested. First nudge fires once idle
/// reaches `threshold_secs`; a re-nudge additionally needs the PRIOR nudge to be at least the cooldown old,
/// so a still-idle task is pinged at most once per cooldown window, never every sweep.
///
/// task_540 follow-on (board-pm greenlit): the re-nudge cooldown is `acked_cooldown_secs` (longer, e.g. 4h)
/// instead of `cooldown_secs` (the normal ~1h) when `assignee_ack_fresher` — the assignee has posted a
/// comment NEWER than our last nudge, i.e. acknowledged/ETA'd this queued todo since we last pinged. A fresh
/// ack buys the longer quiet; the ANTI-PARKING guard is automatic: once an extended-cooldown nudge fires, our
/// last nudge is newer than the ack, so `assignee_ack_fresher` goes false and the normal cadence resumes — a
/// stale ETA stops buying quiet. (The first nudge, `last_nudge_secs == None`, is unaffected: an ack cannot be
/// "newer than the last nudge" when there is no last nudge, and the extended cooldown is a RE-nudge concept.)
fn stale_task_should_nudge(
    idle_secs: i64,
    last_nudge_secs: Option<i64>,
    assignee_ack_fresher: bool,
    threshold_secs: i64,
    cooldown_secs: i64,
    acked_cooldown_secs: i64,
) -> bool {
    if idle_secs < threshold_secs {
        return false;
    }
    match last_nudge_secs {
        None => true,
        Some(since_last_nudge) => {
            let cooldown = if assignee_ack_fresher { acked_cooldown_secs } else { cooldown_secs };
            since_last_nudge >= cooldown
        }
    }
}

/// A task's latest activity age in seconds: the freshest of its `updated_at` and every comment's
/// `created_at` (a comment does NOT bump `updated_at` on this board, so both must be checked — #478).
/// `None` only if `updated_at` itself fails to parse (a malformed record); an unparseable comment
/// timestamp is skipped rather than failing the whole task.
fn task_latest_activity_age_secs(task: &serde_json::Value, now: time::OffsetDateTime) -> Option<i64> {
    let updated_at = task.get("updated_at").and_then(serde_json::Value::as_str).unwrap_or("");
    let mut best = last_seen_age_secs(updated_at, now)?;
    if let Some(comments) = task.get("comments").and_then(serde_json::Value::as_array) {
        for c in comments {
            if let Some(age) = c
                .get("created_at")
                .and_then(serde_json::Value::as_str)
                .and_then(|ts| last_seen_age_secs(ts, now))
            {
                best = best.min(age);
            }
        }
    }
    Some(best)
}

/// The age in seconds of this daemon's OWN most recent nudge comment on a task (author == `NUDGE_AUTHOR`),
/// or `None` if it has never nudged this task — the cooldown clock `stale_task_should_nudge` reads.
fn task_last_nudge_age_secs(task: &serde_json::Value, now: time::OffsetDateTime) -> Option<i64> {
    task.get("comments")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter(|c| c.get("author").and_then(serde_json::Value::as_str) == Some(NUDGE_AUTHOR))
        .filter_map(|c| c.get("created_at").and_then(serde_json::Value::as_str))
        .filter_map(|ts| last_seen_age_secs(ts, now))
        .min()
}

/// task_540: the age in seconds of the task ASSIGNEE's most-recent comment (an ack/ETA), or `None` if the
/// assignee has never commented on it. Mirrors [`task_last_nudge_age_secs`] but keyed on the assignee rather
/// than the nudge daemon — the two together tell an acknowledged-queued todo (assignee acked AFTER our last
/// nudge) from a neglected one, which drives the extended re-nudge cooldown.
fn newest_assignee_comment_age_secs(
    task: &serde_json::Value,
    assignee: &str,
    now: time::OffsetDateTime,
) -> Option<i64> {
    task.get("comments")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter(|c| c.get("author").and_then(serde_json::Value::as_str) == Some(assignee))
        .filter_map(|c| c.get("created_at").and_then(serde_json::Value::as_str))
        .filter_map(|ts| last_seen_age_secs(ts, now))
        .min()
}

/// task_540: is the assignee's latest ack NEWER than the daemon's last nudge? True only when BOTH exist and
/// the ack is more recent (a smaller age). Pure — unit-tested; this is the signal that extends the re-nudge
/// cooldown for an acknowledged-queued todo. Never true before the first nudge (no nudge to be newer than).
fn assignee_ack_fresher_than_last_nudge(
    ack_age_secs: Option<i64>,
    last_nudge_age_secs: Option<i64>,
) -> bool {
    matches!((ack_age_secs, last_nudge_age_secs), (Some(ack), Some(nudge)) if ack < nudge)
}

/// A compact `<N>h<M>m` rendering of a duration in seconds, for the report line (e.g. `3h12m`).
fn format_hm(secs: i64) -> String {
    let secs = secs.max(0);
    format!("{}h{}m", secs / 3600, (secs % 3600) / 60)
}

/// True iff a task is a tracking/epic PARENT whose progress lives in still-open children — it has children
/// (`total > 0`) and not all are done (`done < total`). Such a parent is not itself stalled: its live work IS
/// the children, so nudging it is a false positive (board-pm's #294 daemon-tuning class). A leaf task
/// (`total == 0`) is not a tracking parent, and a parent whose children are ALL done (`done == total`) is not
/// exempted — an all-children-done parent may itself need a nudge to close. Reads `child_rollup` ({done,total},
/// returned by `get_task`). Pure — unit-tested.
fn is_tracking_parent_with_open_children(child_rollup: Option<&serde_json::Value>) -> bool {
    let Some(cr) = child_rollup else {
        return false;
    };
    let total = cr.get("total").and_then(serde_json::Value::as_i64).unwrap_or(0);
    let done = cr.get("done").and_then(serde_json::Value::as_i64).unwrap_or(0);
    total > 0 && done < total
}

/// #540: whether a task shows WORKER activity — at least one comment from someone OTHER than the nudge daemon
/// itself. A `todo` task counts as a stalled deliverable (vs untouched backlog) only once real work has been
/// recorded on it, so the nudge widens to `todo` only when this holds. The daemon's own prior nudges never
/// bootstrap this (they are `NUDGE_AUTHOR`), so a bare todo is never self-qualified. Pure — unit-tested.
fn task_has_worker_activity(task: &serde_json::Value) -> bool {
    task.get("comments")
        .and_then(serde_json::Value::as_array)
        .is_some_and(|cs| {
            cs.iter().any(|c| {
                c.get("author")
                    .and_then(serde_json::Value::as_str)
                    .is_some_and(|a| a != NUDGE_AUTHOR)
            })
        })
}

/// #540: the ACTIONABLE nudge body. Beyond "post an update", it spells out the concrete choices the operator
/// asked for — reassign if you cannot progress it, or update the status (done / blocked-with-a-note, and say
/// so if you are unsure whether it is blocked) — so a nudge drives a resolution rather than just a ping. Pure
/// — unit-tested.
fn nudge_body(threshold_hours: f64, assignee: &str, idle_secs: i64) -> String {
    format!(
        "fleet nudge: this task has had no activity for over {threshold_hours}h (idle {}). {assignee}, please \
         do ONE of: post a progress update or ETA; if you cannot progress it now, reassign it to an available \
         agent; or update the status - mark it done, or blocked with a blocked_on note if it is waiting on \
         something (if you are unsure whether it is blocked, say that).",
        format_hm(idle_secs)
    )
}

/// #540 inc2: is a task's assigned OWNER GONE — no longer a registered agent in the roster (retired/removed),
/// so its stale task is genuinely orphaned and should be ROUTED to the router for re-placement? This is the
/// FALSE-POSITIVE-FREE reroute signal: an `offline`/`away` owner, or one with a stale heartbeat, is NOT gone —
/// it is a DELIBERATELY spun-down (operator-directed, RESUMABLE) or slow-cadence owner whose tasks are its own
/// correct work parked until it resumes, and rerouting those just bounces (board-pm #540 inc2 review: all 4
/// offline/stale-owner reroutes were spun-down-resumable false positives). Only an owner absent from the live
/// roster cannot come back to its work. `roster_ids` is the set of currently-registered agent ids. Pure —
/// unit-tested.
fn owner_is_gone(owner: &str, roster_ids: &std::collections::BTreeSet<String>) -> bool {
    !roster_ids.contains(owner)
}

/// #540 inc2: the comment posted when a stale task is ROUTED to the router (board-pm) — an audit trail naming
/// WHY it landed in the router's queue (unassigned, or its owner is gone from the roster). Pure — unit-tested.
fn route_body(reason: &str, threshold_hours: f64, idle_secs: i64) -> String {
    format!(
        "fleet nudge: reassigned to {NUDGE_ROUTER} for routing - {reason}, and stale for over {threshold_hours}h \
         (idle {}). {NUDGE_ROUTER}, please assign it to a capable agent or update its status (done / blocked / \
         cancelled if obsolete).",
        format_hm(idle_secs)
    )
}

/// Nudge stale ASSIGNED work + ROUTE stale ownerless work (board #478 + #540). A `todo`/`in_progress` task
/// whose latest activity is at least `threshold_hours` old is acted on, at most once per `cooldown_hours` while
/// it stays idle: a task with a live owner gets a comment pinging that owner ([`nudge_body`]); an UNASSIGNED
/// task, or one whose owner is gone from the roster ([`owner_is_gone`]), is REASSIGNED to the router
/// ([`NUDGE_ROUTER`]) with an audit comment ([`route_body`]) so it lands in the router's queue for placement.
/// Always excludes tasks assigned to the configured operator id (`config.operator_id`, if set), `monitor_exempt` tasks (#167 — a deliberate
/// continuous monitor, and the opt-out for a task the router intentionally leaves unassigned), and tasks PARKED
/// on a blocker (`blocked_on_kind` — e.g. `external` for an infra wait, task-board#178 — which are legitimately
/// waiting, not stalled). A `todo` task
/// must show worker activity ([`task_has_worker_activity`]) to count — a bare backlog item is not a stall.
/// Report-only unless `apply` — a dry run prints exactly what it WOULD do without writing (the #478/#540 review
/// gate).
fn nudge_stale(apply: bool, threshold_hours: f64, cooldown_hours: f64) {
    let board = board::Board::connect().unwrap_or_else(|e| {
        eprintln!("fleet nudge-stale: {e}");
        std::process::exit(1);
    });
    let threshold_secs = (threshold_hours * 3600.0).round() as i64;
    let cooldown_secs = (cooldown_hours * 3600.0).round() as i64;
    // task_540: the longer re-nudge cooldown for a todo whose assignee acked since the last nudge.
    let acked_cooldown_secs = (ACKED_NUDGE_COOLDOWN_HOURS * 3600.0).round() as i64;
    // #540 inc2: the set of currently-registered agent ids, for the gone-owner routing signal (an assignee
    // absent from this set is retired/removed → its stale task is orphaned). Best-effort — a roster query
    // error degrades to an EMPTY set, which would make every owner look "gone"; guard that below by only
    // consulting it when it is non-empty (a failed roster query must not mass-reroute live owners' tasks).
    let roster_ids: std::collections::BTreeSet<String> = board
        .list_agents()
        .unwrap_or_default()
        .into_iter()
        .filter_map(|a| a.get("id").and_then(serde_json::Value::as_str).map(str::to_string))
        .collect();
    // #540: nudge stale ASSIGNED work in in_progress AND todo. A task where work started (a plan comment) but
    // was never flipped to in_progress still stalls, and the operator wants it caught (task_512). A bare
    // untouched todo is NOT nudged — the per-task check below requires worker activity for a todo — so widening
    // to todo stays high-signal (real stalls only, not unstarted backlog).
    let mut candidates = board.list_tasks_by_status("in_progress").unwrap_or_else(|e| {
        eprintln!("fleet nudge-stale: {e}");
        std::process::exit(1);
    });
    match board.list_tasks_by_status("todo") {
        Ok(mut todo) => candidates.append(&mut todo),
        Err(e) => eprintln!("fleet nudge-stale: listing todo tasks failed ({e}); nudging in_progress only"),
    }

    println!(
        "fleet nudge-stale: {} assigned in_progress/todo task(s), threshold {threshold_hours}h, cooldown {cooldown_hours}h{}",
        candidates.len(),
        if apply { "" } else { " (DRY RUN — no comments will be posted)" }
    );

    let now = time::OffsetDateTime::now_utc();
    // The operator's own tasks are their work queue, not a stall, so a task assigned to the operator is never
    // nudged/routed (#478 exclusion). The operator id is a deployment-specific value read from config; absent →
    // NO exemption (the generic case: a fleet with no designated operator). The fleet code holds no operator id.
    let operator_id = config::get().operator_id.as_deref();
    let mut nudged = 0usize;
    let mut routed = 0usize;
    for t in &candidates {
        let id = match t.get("id").and_then(serde_json::Value::as_i64) {
            Some(id) => id,
            None => continue,
        };
        // #540 inc2: an UNASSIGNED task is no longer skipped — it is routed to the router below. Empty-string
        // assignee is normalized to None (unassigned).
        let assignee = t
            .get("assignee")
            .and_then(serde_json::Value::as_str)
            .filter(|s| !s.is_empty());
        // The operator's own tasks are never nudged/routed (#478 exclusion). Only when an operator id is
        // configured — an absent operator_id exempts no one (and must NOT skip an unassigned task here).
        if operator_id.is_some() && assignee == operator_id {
            continue;
        }

        // #167/#506: a monitor-exempt task is a legitimate continuous monitor, not a stalled deliverable —
        // never nudge it. This is ALSO the #540 inc2 opt-out: a task the router DELIBERATELY leaves unassigned
        // is marked monitor_exempt so the daemon does not re-grab it. Read from the list record's derived bool.
        if task_is_monitor_exempt(t) {
            continue;
        }

        // task-board#178: a task PARKED on a blocker is legitimately waiting, not a stalled deliverable, so
        // nudging its owner (or routing it) is noise. The list projection carries the blocker as
        // `blocked_on_kind` (e.g. `external` for an infra/no-owner wait — v-task-board seq-7873 — or `operator`
        // / `task`); an external-blocked task in particular has no board owner who can act, and the
        // operator/board-pm own unblocking the others. Mirror `task_is_actionable`: skip ANY non-empty blocker.
        if task_is_parked_on_blocker(t) {
            continue;
        }

        // Cheap prefilter: if the LIST record's own updated_at is already fresher than the threshold, the
        // full latest-activity (which can only be fresher still, since comments never predate it being
        // created) is fresher too — skip without a per-task fetch. A stale updated_at still needs the full
        // record: a later comment can make the task fresh again without ever bumping updated_at.
        let list_updated_age = t
            .get("updated_at")
            .and_then(serde_json::Value::as_str)
            .and_then(|ts| last_seen_age_secs(ts, now));
        if matches!(list_updated_age, Some(age) if age < threshold_secs) {
            continue;
        }

        let full = match board.get_task(id) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("fleet nudge-stale: #{id}: {e} (skipped)");
                continue;
            }
        };
        // Re-check status: it may have changed between the list query and this fetch. #540: in_progress OR
        // todo now qualify.
        let status = full.get("status").and_then(serde_json::Value::as_str).unwrap_or("");
        if status != "in_progress" && status != "todo" {
            continue;
        }
        // board-pm race-hardening (#540 inc2): re-read the AUTHORITATIVE assignee from the FRESH get_task
        // record, not the older list snapshot — a task reassigned or unassigned between the list query and now
        // must route/nudge on its LIVE owner, closing the read-vs-act race. Re-apply the operator exclusion on
        // the fresh value too (a task just handed to the operator must not be nudged).
        let assignee = full.get("assignee").and_then(serde_json::Value::as_str).filter(|s| !s.is_empty());
        if operator_id.is_some() && assignee == operator_id {
            continue;
        }
        // #540(b): a todo is only a stall once work actually STARTED on it (a real comment) — a bare untouched
        // todo is backlog waiting to be picked up, not a stalled deliverable, so it is never nudged. in_progress
        // needs no such guard (being in_progress IS the work-started signal).
        if status == "todo" && !task_has_worker_activity(&full) {
            continue;
        }
        // #294 false-positive class: a tracking/epic parent whose progress is in its still-open children is
        // not itself stalled — its live work IS the children — so skip it to keep nudges high-signal.
        if is_tracking_parent_with_open_children(full.get("child_rollup")) {
            continue;
        }
        let idle_secs = match task_latest_activity_age_secs(&full, now) {
            Some(a) => a,
            None => continue,
        };
        let last_nudge_secs = task_last_nudge_age_secs(&full, now);
        // task_540: a fresh assignee ack (a comment newer than our last nudge) extends the re-nudge cooldown,
        // so an acknowledged-queued todo is not re-nudged hourly; a stale ack reverts to the normal cadence
        // (the anti-parking guard, in stale_task_should_nudge).
        let ack_age_secs = assignee.and_then(|a| newest_assignee_comment_age_secs(&full, a, now));
        let ack_fresher = assignee_ack_fresher_than_last_nudge(ack_age_secs, last_nudge_secs);
        if !stale_task_should_nudge(
            idle_secs,
            last_nudge_secs,
            ack_fresher,
            threshold_secs,
            cooldown_secs,
            acked_cooldown_secs,
        ) {
            continue;
        }

        let title = t.get("title").and_then(serde_json::Value::as_str).unwrap_or("");
        // #540 inc2: decide ROUTE (reassign to the router) vs NUDGE (ping a live owner). Route an UNASSIGNED
        // task, or one whose owner is GONE from the roster — retired/removed, not merely offline (an offline
        // owner is usually a DELIBERATELY spun-down, resumable agent whose task must stay with it, board-pm
        // seq-7848). Never re-route one already owned by the router itself (self-reassign loop). Otherwise nudge
        // the owner (inc1). Guard: an empty roster means the roster query failed — do NOT treat every owner as
        // gone and mass-reroute; skip the gone-owner route entirely in that case.
        let route_reason: Option<String> = match assignee {
            None => Some("unassigned".to_string()),
            Some(owner) if owner != NUDGE_ROUTER => (!roster_ids.is_empty()
                && owner_is_gone(owner, &roster_ids))
            .then(|| format!("owner {owner} is gone from the roster")),
            Some(_) => None, // already the router → nudge it, don't self-reassign
        };

        match route_reason {
            Some(reason) => {
                if apply {
                    let res = board.reassign_task(id, NUDGE_ROUTER, NUDGE_AUTHOR).and_then(|()| {
                        board.comment_task(id, NUDGE_AUTHOR, &route_body(&reason, threshold_hours, idle_secs))
                    });
                    match res {
                        Ok(()) => {
                            routed += 1;
                            println!("  routed #{id} \"{title}\" → {NUDGE_ROUTER} ({reason}, idle={})", format_hm(idle_secs));
                        }
                        Err(e) => eprintln!("  #{id} \"{title}\": route FAILED: {e}"),
                    }
                } else {
                    routed += 1;
                    println!("  would route #{id} \"{title}\" → {NUDGE_ROUTER} ({reason}, idle={})", format_hm(idle_secs));
                }
            }
            None => {
                let owner = assignee.expect("Some (a live non-router owner) when not routing");
                let kind = if last_nudge_secs.is_some() { "re-nudge" } else { "first nudge" };
                if apply {
                    let body = nudge_body(threshold_hours, owner, idle_secs);
                    match board.comment_task(id, NUDGE_AUTHOR, &body) {
                        Ok(()) => {
                            nudged += 1;
                            println!("  nudged #{id} \"{title}\" ({kind}, assignee={owner}, idle={})", format_hm(idle_secs));
                        }
                        Err(e) => eprintln!("  #{id} \"{title}\": nudge FAILED: {e}"),
                    }
                } else {
                    nudged += 1;
                    println!("  would nudge #{id} \"{title}\" ({kind}, assignee={owner}, idle={})", format_hm(idle_secs));
                }
            }
        }
    }

    println!(
        "\n{} {} nudge(s) + {} route(s) to {NUDGE_ROUTER}{}",
        if apply { "posted" } else { "would post" },
        nudged,
        routed,
        if apply { "" } else { " — re-run with --apply to act" }
    );
}

/// The watchdog invocation a cadence unit runs: the liveness sweep (`--rearm --stale-only`) when `rearm`, plus
/// the observer cadence (`--observe --spawn`) and/or the host filter (`--pinned-only`) when requested. An
/// OBSERVER-ONLY unit (`rearm=false, observe=true` → `watchdog --observe --spawn`) can run alongside an
/// existing rearm watchdog without double-rearming — the dev-desk coexistence case. Pure — unit-tested.
fn watchdog_exec_args(rearm: bool, observe: bool, pinned_only: bool, self_redeploy: bool) -> String {
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
    if self_redeploy {
        args.push_str(" --self-redeploy");
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
#[allow(clippy::too_many_arguments)]
fn watchdog_unit(
    rearm: bool,
    observe: bool,
    pinned_only: bool,
    self_redeploy: bool,
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
    let exec_args = watchdog_exec_args(rearm, observe, pinned_only, self_redeploy);
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

/// A systemd USER service that supervises a long-running fleet-host daemon: `Type=simple` with
/// `Restart=on-failure` so a crash restarts it, ordered after the network, and enabled into `default.target`
/// so it comes up on login/boot. `env_block` seeds a known-good environment (a captured `PATH`) so the daemon
/// resolves tmux/git/curl at runtime regardless of profile sourcing. This is the durable replacement for a bare
/// keep-alive tmux window, which a reap silently kills (#359). Pure — unit-tested.
fn daemon_unit_file(name: &str, exec: &str, restart_sec: u64, env_block: &str) -> String {
    format!(
        "[Unit]\n\
         Description=Fleet {name} daemon\n\
         After=network-online.target\n\
         Wants=network-online.target\n\n\
         [Service]\n\
         Type=simple\n\
         {env_block}\
         ExecStart={exec}\n\
         Restart=on-failure\n\
         RestartSec={restart_sec}\n\n\
         [Install]\n\
         WantedBy=default.target\n"
    )
}

/// A known-good `PATH` for a supervised daemon, captured from THIS process's environment at install time so the
/// service resolves tmux/git/curl/nix on bare name even though a systemd unit starts with a stripped env. Reads
/// the environment (not pure).
fn captured_daemon_env() -> String {
    render_service_env_lines(&[("PATH", std::env::var("PATH").ok())])
}

/// The `systemctl --user …` invocations that bring a freshly-written unit fully up: reload the manager so it
/// sees the new file, then enable-and-start it. Split out so the argv is unit-tested without shelling
/// systemctl. Pure.
fn enable_argv(unit: &str) -> Vec<Vec<String>> {
    vec![
        vec!["--user".into(), "daemon-reload".into()],
        vec!["--user".into(), "enable".into(), "--now".into(), unit.into()],
    ]
}

/// `fleet daemon-unit <name>` — supervise a fleet-host daemon under systemd (see [`daemon_unit_file`]). Default:
/// PRINT the unit (a declarative host translates it). `--install`: WRITE `fleet-<name>.service` into
/// `~/.config/systemd/user/` (no sudo). `--enable`: write it AND bring it up in one shot (`daemon-reload` +
/// `enable --now`) — the #359 launch default, so a host daemon comes up supervised rather than as a bare tmux
/// window. `--uninstall`: remove it. `exec` is the daemon command; the built-in `notifier` defaults to
/// `<bin> notify`, any other name requires `--exec`.
fn daemon_unit(
    name: &str,
    exec: Option<String>,
    restart_sec: u64,
    bin: Option<String>,
    install: bool,
    enable: bool,
    uninstall: bool,
) {
    let unit = format!("fleet-{name}.service");
    if uninstall {
        let Some(dir) = user_unit_dir() else {
            eprintln!("fleet daemon-unit --uninstall: cannot resolve ~/.config/systemd/user");
            std::process::exit(1);
        };
        println!("  disable FIRST: systemctl --user disable --now {unit}");
        let path = dir.join(&unit);
        match std::fs::remove_file(&path) {
            Ok(()) => println!("removed {}", path.display()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => println!("absent (ok): {}", path.display()),
            Err(e) => eprintln!("  WARN: remove {}: {e}", path.display()),
        }
        println!("  then: systemctl --user daemon-reload");
        return;
    }
    let fleet_bin = bin.unwrap_or_else(|| {
        std::env::current_exe()
            .ok()
            .and_then(|p| p.to_str().map(str::to_string))
            .unwrap_or_else(|| "fleet".to_string())
    });
    let exec = match exec {
        Some(e) => e,
        None if name == "notifier" => format!("{fleet_bin} notify"),
        None => {
            eprintln!("fleet daemon-unit {name}: --exec is required (no built-in command for '{name}'; the notifier defaults to `<bin> notify`)");
            std::process::exit(1);
        }
    };
    let body = daemon_unit_file(name, &exec, restart_sec, &captured_daemon_env());
    // `--enable` implies the write (it is the one-shot bring-up), so either flag lands the unit file.
    if install || enable {
        let Some(dir) = user_unit_dir() else {
            eprintln!("fleet daemon-unit: cannot resolve ~/.config/systemd/user (no HOME/XDG_CONFIG_HOME)");
            std::process::exit(1);
        };
        if let Err(e) = std::fs::create_dir_all(&dir) {
            eprintln!("fleet daemon-unit: mkdir {}: {e}", dir.display());
            std::process::exit(1);
        }
        let path = dir.join(&unit);
        if let Err(e) = std::fs::write(&path, &body) {
            eprintln!("fleet daemon-unit: write {}: {e}", path.display());
            std::process::exit(1);
        }
        println!("installed {}", path.display());
        println!("  ExecStart: {exec}");
        if enable {
            // Bring it fully up now: reload so systemd sees the new unit, then enable --now (start + start on
            // login/boot). Idempotent. A non-zero/failed systemctl (e.g. no user manager on this host) is a
            // WARN, not fatal — the unit file is written and can be enabled later, same as plain --install.
            for args in enable_argv(&unit) {
                match std::process::Command::new("systemctl").args(&args).status() {
                    Ok(s) if s.success() => println!("  ran: systemctl {}", args.join(" ")),
                    Ok(_) => eprintln!("  WARN: systemctl {} returned non-zero (enable it later once the user manager is up)", args.join(" ")),
                    Err(e) => eprintln!("  WARN: systemctl {} failed: {e} (enable it later)", args.join(" ")),
                }
            }
            println!("  reverse: fleet daemon-unit {name} --uninstall  (or: systemctl --user disable --now {unit})");
        } else {
            println!("  enable:  systemctl --user daemon-reload && systemctl --user enable --now {unit}");
            println!("  reverse: fleet daemon-unit {name} --uninstall  (or: systemctl --user disable --now {unit})");
        }
        return;
    }
    print!("# ---- {unit} (systemd USER daemon, Restart=on-failure) ----\n{body}");
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

/// What the watchdog should do about its OWN binary freshness at the start of a sweep (#388). Decided purely
/// from the baked build rev, the checkout HEAD, and whether `--self-redeploy` is set. Pure — unit-tested.
#[derive(Debug, PartialEq, Eq)]
enum StaleSelfAction {
    /// The binary matches its checkout (or freshness can't be told: unknown rev / no source tree) — do nothing.
    Fresh,
    /// Stale, but `--self-redeploy` is off — print the STALE warning (the long-standing report-only behavior).
    Warn(String),
    /// Stale and `--self-redeploy` is on — surface the warning AND trigger `fleet redeploy --apply`.
    Redeploy(String),
}

/// Decide the watchdog's binary-freshness action (#388). Reuses [`build_freshness_warning`] as the cheap
/// baked-vs-HEAD staleness signal (no per-sweep network fetch): when it reports staleness, `--self-redeploy`
/// turns the passive warning into a redeploy trigger. Pure — unit-tested.
fn watchdog_stale_self_action(baked_rev: &str, checkout_head: Option<&str>, self_redeploy: bool) -> StaleSelfAction {
    match build_freshness_warning(baked_rev, checkout_head) {
        None => StaleSelfAction::Fresh,
        Some(w) if self_redeploy => StaleSelfAction::Redeploy(w),
        Some(w) => StaleSelfAction::Warn(w),
    }
}

/// The git checkout root the running binary was built from — walk up from the binary's path to a dir
/// containing `.git`. `None` when there is no source tree (a deployed/hermetic binary). Best-effort.
fn checkout_root() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    exe.ancestors().find(|p| p.join(".git").exists()).map(Path::to_path_buf)
}

/// Run `git -C <root> <args...>` and return trimmed stdout on success, else `None`. Best-effort git helper.
fn git_capture(root: &Path, args: &[&str]) -> Option<String> {
    let out = std::process::Command::new("git").arg("-C").arg(root).args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8(out.stdout).ok()?.trim().to_string();
    (!s.is_empty()).then_some(s)
}

/// The short HEAD sha of the git checkout the running binary was built from. `None` when there is no source
/// tree (a deployed binary) or git is unavailable. Best-effort — only used to warn about a stale binary.
fn checkout_head_short() -> Option<String> {
    git_capture(&checkout_root()?, &["rev-parse", "--short", "HEAD"])
}

/// The default systemd USER units `fleet redeploy` restarts (the long-running fleet daemons) when the config
/// does not override `redeploy_services`. A unit not present on this host is skipped, not an error.
const DEFAULT_REDEPLOY_SERVICES: &[&str] =
    &["fleet-watchdog.timer", "fleet-notify.service", "fleet-tunnel.service"];

/// The release builds `fleet redeploy` runs before restarting the daemons — one per binary that BACKS a
/// service in [`DEFAULT_REDEPLOY_SERVICES`]. `fleet-watchdog`/`fleet-notify` are the `fleet` binary
/// (`fleet watchdog` / `fleet notify`); `fleet-tunnel` is its OWN crate + binary, built only with
/// `--features transport`. Each entry is the cargo args AFTER `build --release`. Keep in sync with the
/// restarted services: a daemon binary missing here would be restarted STALE — the #451 gap, where a
/// fleet-tunnel change did not go live via redeploy because only `--bin fleet` was rebuilt.
const REDEPLOY_BUILDS: &[&[&str]] =
    &[&["--bin", "fleet"], &["-p", "fleet-tunnel", "--features", "transport"]];

/// What `fleet redeploy` should do, decided purely from the build/repo state. Pure — unit-tested.
#[derive(Debug, PartialEq, Eq)]
enum RedeployAction {
    /// The built binary already matches `origin/main` — nothing to do.
    UpToDate,
    /// The binary is behind `origin/main` and the checkout is safe to fast-forward + rebuild.
    Rebuild,
    /// Behind, but the checkout is dirty or not on `main` — refuse to act (never clobber local work).
    NeedsManual(String),
}

/// Decide the redeploy action from the baked build rev, the resolved `origin/main` sha, and the checkout
/// state. `UpToDate` when the built base sha equals `origin/main` (a `-dirty` suffix compares by base — a
/// dirty build of the same commit is current). Otherwise a rebuild is needed, but only SAFE when the tree is
/// clean AND on `main` (a fast-forward can't clobber); a dirty or off-`main` checkout returns `NeedsManual`
/// so an automated redeploy never discards a sibling's in-progress work. Pure — unit-tested.
fn redeploy_action(baked_rev: &str, remote_sha: &str, dirty: bool, on_main: bool) -> RedeployAction {
    let base = baked_rev.strip_suffix("-dirty").unwrap_or(baked_rev);
    if base == remote_sha {
        return RedeployAction::UpToDate;
    }
    if dirty {
        return RedeployAction::NeedsManual("the checkout has uncommitted changes".to_string());
    }
    if !on_main {
        return RedeployAction::NeedsManual("the checkout is not on the `main` branch".to_string());
    }
    RedeployAction::Rebuild
}

/// `fleet redeploy [--apply]` (#388): bring the host's fleet daemons up to `origin/main`. A merged fleet PR
/// does not rebuild/restart the running daemons, so a landed fix stays dormant (the watchdog silently runs
/// old logic) until a manual `cargo build --release` + `systemctl --user restart`. This collapses that dance
/// into one command: fetch `origin/main`, and when the built binary is behind it, fast-forward + rebuild +
/// restart the daemon services. SAFE: reports by default (acts only with `--apply`) and refuses a dirty or
/// off-`main` checkout so it never clobbers a sibling's in-progress work.
fn redeploy(apply: bool) {
    // CLI wrapper: run the redeploy, then translate its outcome to a process exit code. The core is
    // `run_redeploy` (no `process::exit`) so the watchdog can invoke it inline without a failure flapping the
    // long-running watchdog service (#388's `--self-redeploy`).
    match run_redeploy(apply) {
        Ok(msg) => println!("{msg}"),
        Err(why) => {
            eprintln!("fleet redeploy: {why}");
            std::process::exit(1);
        }
    }
}

/// The redeploy core, decoupled from `process::exit` so it is callable BOTH from the `redeploy` CLI (which
/// exits on `Err`) and inline from the watchdog's `--self-redeploy` path (which must only LOG on `Err`, never
/// abort the sweep). Emits progress lines as it goes and returns a one-line terminal outcome: `Ok` when there
/// was nothing to do or the rebuild+restart succeeded, `Err(why)` when it declined (dirty / off-main) or a
/// step failed. On any build failure it aborts BEFORE restarting so the daemons keep their old, working
/// binaries. Never restarts on a half-rebuilt tree.
fn run_redeploy(apply: bool) -> Result<String, String> {
    let Some(root) = checkout_root() else {
        return Err("no source checkout (a deployed/hermetic binary tracks its flake input, not git) — nothing to redeploy".to_string());
    };
    // Refresh the remote ref so the comparison is against the current origin/main.
    if git_capture(&root, &["fetch", "--quiet", "origin", "main"]).is_none() {
        // fetch prints nothing on success, so None here can be a clean fetch OR a failure; probe the ref next.
    }
    let Some(remote_sha) = git_capture(&root, &["rev-parse", "--short", "origin/main"]) else {
        return Err(format!("cannot resolve origin/main (fetch failed or no such remote) in {}", root.display()));
    };
    let dirty = git_capture(&root, &["status", "--porcelain"]).is_some();
    let on_main = git_capture(&root, &["symbolic-ref", "--short", "HEAD"]).as_deref() == Some("main");
    let baked = env!("FLEET_BUILD_REV");
    let action = redeploy_action(baked, &remote_sha, dirty, on_main);

    println!("fleet redeploy ({}): built rev {baked}, origin/main {remote_sha} — {}",
        if apply { "APPLY" } else { "report" },
        match &action {
            RedeployAction::UpToDate => "UP TO DATE".to_string(),
            RedeployAction::Rebuild => "REBUILD NEEDED (clean, on main)".to_string(),
            RedeployAction::NeedsManual(why) => format!("BEHIND but MANUAL redeploy needed ({why})"),
        }
    );
    match action {
        RedeployAction::UpToDate => return Ok(format!("up to date at {remote_sha}; nothing to redeploy")),
        RedeployAction::NeedsManual(why) => {
            return Err(format!("not acting — {why}. Resolve it, then re-run (or rebuild by hand)."));
        }
        RedeployAction::Rebuild => {}
    }
    if !apply {
        return Ok("report only — re-run with --apply to fast-forward, rebuild, and restart the daemons".to_string());
    }
    // Fast-forward to origin/main (guaranteed possible: clean + on main + behind).
    println!("  fast-forwarding to origin/main…");
    if git_capture(&root, &["merge", "--ff-only", "origin/main"]).is_none()
        && git_capture(&root, &["rev-parse", "--short", "HEAD"]).as_deref() != Some(remote_sha.as_str())
    {
        return Err("fast-forward to origin/main failed; aborting before rebuild".to_string());
    }
    // Rebuild EVERY daemon binary before restarting anything (#451): the fleet binary AND the separate
    // fleet-tunnel binary. If any build fails, abort before restarting so the daemons keep their old, working
    // binaries rather than being restarted onto a half-rebuilt tree.
    for &extra in REDEPLOY_BUILDS {
        println!("  building release (cargo build --release {})…", extra.join(" "));
        let mut args = vec!["build", "--release"];
        args.extend_from_slice(extra);
        let build = std::process::Command::new("cargo").current_dir(&root).args(&args).status();
        match build {
            Ok(s) if s.success() => {}
            Ok(s) => {
                return Err(format!(
                    "`cargo build --release {}` failed (exit {:?}); daemons NOT restarted (they keep the old, working binaries)",
                    extra.join(" "),
                    s.code()
                ));
            }
            Err(e) => {
                return Err(format!("could not run cargo ({e}); daemons NOT restarted"));
            }
        }
    }
    let services: Vec<String> = config::get()
        .redeploy_services
        .clone()
        .unwrap_or_else(|| DEFAULT_REDEPLOY_SERVICES.iter().map(|s| s.to_string()).collect());
    println!("  restarting {} daemon service(s)…", services.len());
    for svc in &services {
        let st = std::process::Command::new("systemctl")
            .args(["--user", "restart", svc])
            .status();
        match st {
            Ok(s) if s.success() => println!("    restarted {svc}"),
            // A unit not installed on this host is not an error — the default set is a superset across hosts.
            Ok(_) => println!("    skipped {svc} (not present on this host, or restart returned non-zero)"),
            Err(e) => println!("    could not restart {svc} ({e})"),
        }
    }
    Ok(format!("now at {remote_sha}, binary rebuilt, daemons restarted."))
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
        let k = build_kickoff("v-x", "/wt/v-x", "30m", Some("op-x"), false);
        // Identity (#336): the board does NOT bind the session (a fresh unbound board per call), so
        // register_agent can't make later id-less calls work — the kickoff must say pass ids EXPLICITLY on
        // EVERY call, and still register once + self-discover the charter via get_agent.
        assert!(k.contains("register_agent"), "registers the record once (idempotent)");
        assert!(k.contains("get_agent"), "self-discovers its charter");
        assert!(k.contains("does NOT bind your session"), "states the board never binds the session (#336)");
        assert!(k.contains("EVERY board call"), "explicit ids on every call, not a fallback");
        assert!(k.contains("from_agent") && k.contains("created_by") && k.contains("actor"), "names the explicit-id params incl. send_message's from_agent");
        // Per-tool identity map (#531): each tool names identity DIFFERENTLY, and the wrong field records
        // actor=null and self-notifies you on your own comment — the exact footgun task_531 exists to stop.
        assert!(k.contains("author on comment_task"), "names comment_task's author field explicitly (#531)");
        assert!(k.contains("actor on update_task") && k.contains("created_by on create_task"), "per-tool write-identity map");
        assert!(k.contains("wake on your OWN comment"), "warns that the wrong identity field self-notifies");
        // Never precompute a task id (#526): reference only the id create_task returns.
        assert!(k.contains("Never PRECOMPUTE") && k.contains("id create_task RETURNS"), "bans guessing a task id (#526)");
        // Typed references (task_584): a bare `#N` is hard-rejected on board bodies, so the kickoff mandates
        // a TYPED id (`task_N` or `owner/repo#N`) in any comment/message/post — else a reword-retry every time.
        assert!(k.contains("TYPED REFERENCES") && k.contains("owner/repo#N"), "mandates typed task/PR refs, not a bare #N (task_584)");
        assert!(k.contains("hard-rejects a bare"), "states the board hard-rejects a bare #N in posted content");
        assert!(k.contains("'v-x'") && k.contains("/wt/v-x"));
        // OWNER-CONFIRM gate (#352): a trace-derived destructive/operator action against a service you don't
        // own must be owner-confirmed before executing or routing (a near-miss almost restarted a stale unit).
        assert!(k.contains("OWNER-CONFIRM gate") && k.contains("OWNER-UNCONFIRMED"), "binds the owner-confirm gate for trace-derived destructive actions");
        // Shared-task coordination (task_565): R1 give-the-owner-a-beat (owner files a flagged spin-off; a
        // coordinator only if the owner is absent/stalled) + R2 dedup-is-single-writer (one owner picks the
        // survivor and FREEZES; never symmetric-cancel your own dup, which deadlocks).
        assert!(k.contains("SHARED-TASK COORDINATION") && k.contains("the OWNER files the spin-off"), "R1: give the live owner a beat before a coordinator race-creates a flagged spin-off");
        assert!(k.contains("DEDUP IS SINGLE-WRITER") && k.contains("symmetric-cancel"), "R2: dedup is single-writer; never symmetric-cancel your own dup (deadlock)");
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
        // task_589: never pass a shell $(cat file) substitution as an MCP content arg (stored verbatim,
        // silent clobber) + read back after a write; and no commit/PR attribution lines in board bodies.
        assert!(k.contains("$(cat file)") && k.contains("READ BACK"), "warns against a $(cat) MCP arg + mandates read-back-after-write (task_589)");
        assert!(k.contains("Co-Authored-By:") && k.contains("not board content"), "bans commit/PR attribution lines in board bodies");
        // External-dependency blocked → long/event-woken cadence (#349): the block carve-out is generalized
        // beyond the operator to ANY external dep (another agent, a pending deploy/CI), so an agent holding a
        // sole externally-blocked task sets it blocked + drops to the long cadence instead of SOON-polling.
        assert!(k.contains("BLOCKED ON AN EXTERNAL DEPENDENCY"), "generalizes the block carve-out beyond the operator (#349)");
        assert!(k.contains("another agent") && k.contains("deploy/CI"), "names the non-operator external deps");
        assert!(k.contains("long/event-woken cadence"), "a sole blocked task drops to the long/event-woken cadence, not SOON polling");
        // Operator-blocked dashboard convention (operator seq-2292) is preserved as a sub-case, with the
        // operator id INTERPOLATED from config (task_611), not hard-coded: reassign to the configured operator
        // + typed blocked_on + stash the real owner so list_tasks(assignee <operator>) is the one dashboard.
        assert!(k.contains("ON THE OPERATOR specifically") && k.contains("assign the task to 'op-x'"), "interpolates the configured operator id into the operator-blocked convention");
        assert!(k.contains("list_tasks(assignee 'op-x')"), "the operator dashboard clause uses the configured id");
        assert!(k.contains("metadata.blocked_owner"), "stashes the real owner for reassign-back");
        // No designated operator → the operator-blocked clause is omitted, but the surrounding external-dep
        // guidance stays intact (generic fleet with no operator; task_611).
        let k_no_op = build_kickoff("v-x", "/wt/v-x", "30m", None, false);
        assert!(!k_no_op.contains("ON THE OPERATOR specifically"), "omits the operator-blocked clause when no operator is configured");
        assert!(k_no_op.contains("buys nothing. (If your MCP cannot set a typed blocked_on"), "the surrounding external-dep clause reads cleanly with the operator clause omitted");
        // Status honesty (task_506 Layer 1): never stand down (offline/away) holding a live in_progress task —
        // progress it or re-state it blocked/done first; the companion watchdog warning flags the violation.
        assert!(k.contains("STATUS HONESTY") && k.contains("in_progress"), "bans standing down on a live in_progress task (task_506 Layer 1)");
        assert!(k.contains("status-honesty violation the watchdog flags"), "ties the kickoff clause to the watchdog #506 warning");
        // Drained / at-rest → persist a long cadence on the BOARD metadata (#383 + task_566): the lever is the
        // board metadata.interval (update_agent, the cadence the watchdog reads) which works even for a
        // board-only agent — NOT the frozen `cargo xtask fleet set-interval` which only writes the file-hub
        // registry (fails board-only, leaves the mirror stale). A raw reschedule also does not persist.
        assert!(k.contains("DONE / at-rest"), "routes a drained/done cluster to the rest-cadence path (#383)");
        assert!(k.contains("metadata.interval") && k.contains("update_agent"), "board metadata.interval via update_agent is the board-native cadence lever (task_566)");
        assert!(k.contains("board-only agent with no file-hub registry row"), "the board lever works for a board-only agent, unlike the frozen cargo xtask set-interval");
        assert!(k.contains("does NOT persist"), "explains a raw next-tick reschedule does not stick against the watchdog");
    }

    #[test]
    fn redeploy_action_rebuilds_only_when_behind_clean_and_on_main() {
        // Built rev already matches origin/main -> nothing to do (even if dirty / off main).
        assert_eq!(redeploy_action("abc123", "abc123", false, true), RedeployAction::UpToDate);
        assert_eq!(redeploy_action("abc123", "abc123", true, false), RedeployAction::UpToDate);
        // A -dirty build of the SAME commit is current (compares by base sha).
        assert_eq!(redeploy_action("abc123-dirty", "abc123", false, true), RedeployAction::UpToDate);
        // Behind + clean + on main -> safe to fast-forward + rebuild.
        assert_eq!(redeploy_action("old111", "new222", false, true), RedeployAction::Rebuild);
        // Behind but DIRTY -> refuse (never clobber uncommitted work).
        assert!(matches!(redeploy_action("old111", "new222", true, true), RedeployAction::NeedsManual(_)));
        // Behind but OFF main (a feature branch) -> refuse (a ff-only would fail / clobber intent).
        assert!(matches!(redeploy_action("old111", "new222", false, false), RedeployAction::NeedsManual(_)));
    }

    #[test]
    fn redeploy_builds_cover_every_daemon_binary() {
        // Each build is non-empty cargo args, and the set covers BOTH daemon binaries: the `fleet` bin
        // (fleet-watchdog + fleet-notify) and the separate fleet-tunnel bin (its own crate + transport
        // feature). The #451 regression guard: dropping fleet-tunnel here restarts it stale.
        assert!(REDEPLOY_BUILDS.iter().all(|b| !b.is_empty()), "no empty build arg-set");
        let flat: Vec<&str> = REDEPLOY_BUILDS.iter().flat_map(|b| b.iter().copied()).collect();
        assert!(flat.contains(&"fleet") && flat.windows(2).any(|w| w == ["--bin", "fleet"]), "builds --bin fleet");
        assert!(flat.contains(&"fleet-tunnel"), "builds the fleet-tunnel crate");
        assert!(flat.contains(&"transport"), "fleet-tunnel needs its transport feature");
        // Every service backed by a binary that must exist has a build (both counts stay aligned).
        assert!(!REDEPLOY_BUILDS.is_empty() && !DEFAULT_REDEPLOY_SERVICES.is_empty());
    }

    #[test]
    fn build_kickoff_reactive_mode_only_acts_when_addressed() {
        let r = build_kickoff("frank", "/wt/frank", "30m", Some("op-x"), true);
        // Still self-discovering + explicit-identity like every kickoff (the boot contract is shared).
        assert!(r.contains("register_agent") && r.contains("get_agent"), "reactive kickoff still self-discovers");
        assert!(r.contains("'frank'"), "carries the agent id");
        assert!(r.contains("/loop run one tick"), "still a dynamic /loop");
        // The reactive discipline (#438): only being ADDRESSED is actionable; ambient chatter is NOT work.
        assert!(r.contains("REACTIVE responder"), "declares reactive mode");
        assert!(r.contains("EXPLICITLY") && r.contains("ADDRESSED"), "an explicit address is a trigger");
        // #438 thread-engagement: an in-thread follow-up on a conversation it is already in ALSO triggers it —
        // the live thread-subscription delivery wakes it on a reply_to=subscribed-root post, and the prompt
        // must count that as addressed so it continues the exchange instead of treating it as ambient chatter.
        assert!(r.contains("reply_to") && r.contains("thread_subscribed"), "an in-thread follow-up is a trigger");
        assert!(r.contains("FOLLOW-UP"), "names the thread follow-up trigger");
        assert!(r.contains("WAIT TO BE WOKEN"), "goes idle and waits for an event-wake when unaddressed");
        // CRITICAL: it must NOT carry the work-conserving 'unread => keep going SOON' pacing that mis-fires on
        // ambient channel chatter (the exact anti-pattern the observer caught in Frank's boot).
        assert!(!r.contains("WORK-CONSERVING PACING"), "reactive mode drops the work-conserving pacing");
        assert!(!r.contains("keep going — schedule your next tick SOON"), "no SOON re-poll on unread chatter");
        // And the default (non-reactive) kickoff must be UNCHANGED — it keeps the work-conserving pacing.
        let w = build_kickoff("v-x", "/wt/v-x", "30m", Some("op-x"), false);
        assert!(w.contains("WORK-CONSERVING PACING") && !w.contains("REACTIVE responder"), "default worker unchanged");
    }

    #[test]
    fn build_launch_cmd_wires_claude_and_codex_and_rejects_unknown() {
        // claude is fully wired: the exec line carries the model/effort and reads the kickoff from the env.
        let c = build_launch_cmd("claude", "claude-x", "high", None).expect("claude wired");
        assert!(c.starts_with("exec claude "));
        assert!(c.contains("--model 'claude-x'") && c.contains("--effort 'high'"));
        assert!(c.contains("\"$CDZ_KICKOFF\""), "kickoff rides in the env var, not interpolated");
        // devshell (#214): opt-in launch inside the workdir's flake devShell so the pinned toolchain is on PATH.
        let d = build_launch_cmd("claude", "claude-x", "high", Some("/wt/v-x")).expect("claude wired");
        assert!(d.starts_with("exec nix develop \"path:/wt/v-x\" --command claude "), "wrapped in nix develop");
        assert!(d.contains("--model 'claude-x'") && d.contains("\"$CDZ_KICKOFF\""), "same claude args inside the devShell");
        // codex is wired: bypass flag for unattended run, model passed through single-quoted, kickoff from env.
        let x = build_launch_cmd("codex", "codex-m", "high", None).expect("codex wired");
        assert!(x.starts_with("exec codex "));
        assert!(x.contains("--dangerously-bypass-approvals-and-sandbox"), "unattended: no approval/sandbox gate");
        assert!(x.contains("--model 'codex-m'"), "model passed through (board data supplies the concrete name)");
        assert!(x.contains("\"$CDZ_KICKOFF\""), "codex reads the same kickoff env var, not an interpolated prompt");
        assert!(!x.contains("--effort"), "codex takes no --effort flag (claude-only)");
        // codex honors the same devShell wrapping as claude.
        let xd = build_launch_cmd("codex", "codex-m", "high", Some("/wt/v-x")).expect("codex wired");
        assert!(xd.starts_with("exec nix develop \"path:/wt/v-x\" --command codex "), "codex wrapped in nix develop too");
        // an unknown/typo'd harness fails loudly.
        let u = build_launch_cmd("gpt5", "m", "high", None).unwrap_err();
        assert!(u.contains("unknown harness 'gpt5'"));
    }

    #[test]
    fn codex_trust_missing_only_when_not_already_trusted() {
        let cfg: toml::Value = r#"
            model = "codex-bedrock"
            [projects."/wt/v-yes"]
            trust_level = "trusted"
            [projects."/wt/v-partial"]
            trust_level = "untrusted"
        "#
        .parse()
        .expect("valid toml");
        // Already trusted → no entry needed.
        assert!(!codex_trust_missing(&cfg, "/wt/v-yes"));
        // Present but a different trust_level → still needs the trusted entry.
        assert!(codex_trust_missing(&cfg, "/wt/v-partial"));
        // Absent entirely → needs the entry.
        assert!(codex_trust_missing(&cfg, "/wt/v-absent"));
        // A config with no projects table at all → needs the entry.
        let bare: toml::Value = "model = \"x\"".parse().expect("valid toml");
        assert!(codex_trust_missing(&bare, "/wt/v-x"));
    }

    #[test]
    fn codex_trust_table_is_a_valid_appendable_projects_entry() {
        let t = codex_trust_table("/wt/v-x");
        assert_eq!(t, "\n[projects.\"/wt/v-x\"]\ntrust_level = \"trusted\"\n");
        // The appended table must parse, and it must read back as trusted (round-trips through the check).
        let v: toml::Value = t.parse().expect("appended table is valid toml");
        assert!(!codex_trust_missing(&v, "/wt/v-x"), "the rendered table marks the dir trusted");
        // A key with TOML-special chars is escaped so the table still parses and round-trips.
        let weird = codex_trust_table(r#"/wt/a"b\c"#);
        let vw: toml::Value = weird.parse().expect("escaped key is valid toml");
        assert!(!codex_trust_missing(&vw, r#"/wt/a"b\c"#), "escaped key round-trips to the same dir");
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
    fn review_angles_are_the_four_confirmed_adversarial_lenses() {
        // The angle set is board-pm-confirmed against Doc #5 D16 (#374): correctness+completeness, clarity+writing,
        // risk+security, alternatives-not-considered — one ephemeral reviewer per angle.
        let keys: Vec<&str> = REVIEW_ANGLES.iter().map(|(k, _)| *k).collect();
        assert_eq!(
            keys,
            vec!["correctness-completeness", "clarity-writing", "risk-security", "alternatives"]
        );
        // The clarity angle names the MAINTAINED lists as run-time truth (not a hardcoded pattern copy).
        let clarity = REVIEW_ANGLES.iter().find(|(k, _)| *k == "clarity-writing").unwrap().1;
        assert!(clarity.contains("three-pass"), "clarity angle applies the humanize three-pass");
        assert!(clarity.contains("Document #7") && clarity.contains("Document #8"), "names the maintained writing lists");
        assert!(clarity.contains("READ AT REVIEW TIME"), "reads the lists at review time, not a frozen copy");
    }

    #[test]
    fn build_reviewer_kickoff_reviews_one_review_on_one_angle_and_never_transitions() {
        let (key, focus) = REVIEW_ANGLES[0]; // correctness-completeness
        let k = build_reviewer_kickoff(88, key, focus, "/repo/loops/reviewer.md", "/abs/fleet");
        // Identity + ephemeral (one review, no loop) — the reviewer analog of the observer kickoff.
        assert!(k.contains("register_agent 'reviewer'"), "binds the stable reviewer identity");
        assert!(k.contains("author=\"reviewer\""), "authors board writes as reviewer");
        assert!(k.contains("do NOT start a /loop") && k.contains("you do not loop"), "ephemeral one-shot, not a loop");
        // Drives from the review + its angle.
        assert!(k.contains("review #88") && k.contains("get_review 88"), "reads the assigned review");
        assert!(k.contains("ANGLE `correctness-completeness`"), "carries the assigned angle");
        assert!(k.contains(focus), "carries the angle's review lens");
        assert!(k.contains("/repo/loops/reviewer.md") && k.contains("/abs/fleet"), "names the role + this binary");
        // Files findings on the review log (+ actionable child tasks); does NOT transition status (D17 person-gate).
        assert!(k.contains("append_review_log") && k.contains("review_id=88"), "records findings on the review log");
        assert!(k.contains("finding"), "entries are findings");
        assert!(k.contains("CHILD task"), "an actionable finding links a child task");
        assert!(k.contains("DO NOT call set_review_status"), "the reviewer never transitions the review (D17 person-gate)");
        assert!(k.contains("no-op finding"), "a clean angle still records that it ran");
        // The clarity angle carries the maintained-lists-at-review-time contract when built for it.
        let clarity_k = build_reviewer_kickoff(88, REVIEW_ANGLES[1].0, REVIEW_ANGLES[1].1, "/r/loops/reviewer.md", "/abs/fleet");
        assert!(clarity_k.contains("three-pass") && clarity_k.contains("Document #7"), "clarity kickoff points at the writing guide + three-pass");
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
        let p = parse_workspace_kind("v-example", "/home/u/.fleet", &rec, None);
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
        let p = parse_workspace_kind("v-bare", "/home/u/.fleet", &rec, None);
        assert_eq!(p.cwd, workspace::agent_root_dir("/home/u/.fleet", "v-bare"));
        assert_eq!(p.pre_trust, vec!["/home/u/.fleet".to_string(), p.cwd.clone()]);
        assert!(p.setup_script.is_none(), "whitespace-only setup_script is treated as absent");
        // A relative config.cwd is taken under the fleet root.
        let rec2 = serde_json::json!({ "name": "rel", "config": { "cwd": "checkout/here" } });
        let p2 = parse_workspace_kind("v-rel", "/home/u/.fleet", &rec2, None);
        assert_eq!(p2.cwd, "/home/u/.fleet/checkout/here");
    }

    #[test]
    fn parse_workspace_kind_per_agent_cwd_override_beats_config_cwd() {
        // Several agents share ONE kind (same setup_script/env) but each launches in its own workspace dir via
        // metadata.workspace_cwd — the override wins over the kind's config.cwd, absolute used as-is.
        let rec = serde_json::json!({
            "name": "membrain",
            "setup_script": "verify workspace\n",
            "config": { "cwd": "/shared/default", "env": { "K": "v" } }
        });
        let p = parse_workspace_kind("m-a", "/home/u/.fleet", &rec, Some("/work/agent-a"));
        assert_eq!(p.cwd, "/work/agent-a", "per-agent override beats config.cwd");
        assert_eq!(p.env, vec![("K".to_string(), "v".to_string())], "shared env still comes from the kind");
        // A blank/whitespace override is ignored → falls back to config.cwd.
        let p2 = parse_workspace_kind("m-b", "/home/u/.fleet", &rec, Some("   "));
        assert_eq!(p2.cwd, "/shared/default", "blank override falls back to config.cwd");
        // A relative override is taken under the fleet root, same as config.cwd.
        let p3 = parse_workspace_kind("m-c", "/home/u/.fleet", &rec, Some("rel/ws"));
        assert_eq!(p3.cwd, "/home/u/.fleet/rel/ws");
    }

    #[test]
    fn setup_script_env_exports_resolved_launch_cwd_and_kind_env() {
        // A shared kind's setup_script must receive the agent's OWN resolved launch cwd (FLEET_WORKSPACE_CWD)
        // so it can find src/<package> at an arbitrary host path, plus the kind's own config env, after the
        // built-in FLEET_AGENT/FLEET_ROOT.
        let plan = WorkspaceKindPlan {
            name: "membrain".to_string(),
            description: None,
            setup_script: Some("make build\n".to_string()),
            cwd: "/work/agent-a".to_string(),
            pre_trust: vec![],
            env: vec![("K".to_string(), "v".to_string())],
        };
        let env = setup_script_env("m-a", "/home/u/.fleet", &plan);
        assert_eq!(
            env,
            vec![
                ("FLEET_AGENT".to_string(), "m-a".to_string()),
                ("FLEET_ROOT".to_string(), "/home/u/.fleet".to_string()),
                ("FLEET_WORKSPACE_CWD".to_string(), "/work/agent-a".to_string()),
                ("K".to_string(), "v".to_string()),
            ],
            "built-ins first (incl. the resolved launch cwd), then the kind's config env"
        );
    }

    #[test]
    fn read_native_is_tristate_absent_is_unknown_not_false() {
        use serde_json::json;
        // Explicit true/false are decisive.
        assert!(matches!(read_native(Some(&json!({"native": true}))), NativeVerdict::Native));
        assert!(matches!(read_native(Some(&json!({"native": false}))), NativeVerdict::NotNative));
        // task_500: an ABSENT native key, entirely missing metadata, and a non-bool native are ALL Unknown
        // (fail-safe) — the task_418 regression dropped metadata, which must NOT read as a hard not-native.
        assert!(matches!(read_native(Some(&json!({"interval": "3h"}))), NativeVerdict::Unknown));
        assert!(matches!(read_native(None), NativeVerdict::Unknown));
        assert!(matches!(read_native(Some(&json!({"native": "true"}))), NativeVerdict::Unknown));
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
        // A DRAINED queue (0 open tasks) is NEVER a candidate — any verdict, any interval (#332): a
        // mission-complete agent that lengthened its own cadence is doing sanctioned idle, not stalling, so
        // re-arming it ("keep looping until your queue drains") is a phantom nag (v-capmeshd / v-nmidid: 0 open
        // tasks yet STALE against a short registered interval).
        assert!(!is_retighten_candidate("STALE", 0, 600), "drained + STALE → NOT a candidate (was the bug)");
        assert!(!is_retighten_candidate("late", 0, 600), "late is normal idle, and the queue is empty anyway");
        assert!(!is_retighten_candidate("ok", 0, 6 * 3600), "drained on a long interval → sanctioned idle");
        // HOLDS open work → candidate when the loop stalled (STALE) or it idles on a long interval it should
        // be draining tighter.
        assert!(is_retighten_candidate("STALE", 1, 600), "stalled loop holding work → re-arm");
        assert!(is_retighten_candidate("ok", 2, 3600), "1h+ with open tasks");
        assert!(is_retighten_candidate("late", 1, 6 * 3600));
        // Open work on a SHORT interval that is cycling (ok/late) is fine — it's already tight.
        assert!(!is_retighten_candidate("ok", 3, 600), "10m with tasks is already tight");
        assert!(!is_retighten_candidate("late", 3, 600), "short-interval late is the normal cycling band");
    }

    #[test]
    fn work_driven_rearm_catches_a_quiet_work_holder_regardless_of_interval() {
        let wc = WATCHDOG_WORK_CADENCE_SECS;
        // Holds work and has been quiet beyond the short work cadence → candidate, even though (elsewhere) its
        // interval may be moderate and is_retighten_candidate would call it "already tight" (the #535 gap).
        assert!(work_driven_rearm(1, Some(wc), false));
        assert!(work_driven_rearm(3, Some(wc * 100), false), "long quiet with work → still a candidate");
        // Below the work cadence → not yet (it is cycling tightly enough / may be mid-tick).
        assert!(!work_driven_rearm(1, Some(wc - 1), false));
        // No actionable work → never a work-driven candidate (a drained/idle agent rests on its interval).
        assert!(!work_driven_rearm(0, Some(wc * 100), false));
        // Stood down (offline / spun down) → not woken by this path; a deliberate stand-down is not "holding
        // work quietly" (the #506 holding-work-at-rest path handles an offline agent that still owns work).
        assert!(!work_driven_rearm(2, Some(wc * 100), true));
        // Unknown last_seen (None age) → not a candidate (no basis to call it quiet).
        assert!(!work_driven_rearm(2, None, false));
    }

    #[test]
    fn drained_idle_candidate_catches_a_short_interval_at_rest_self_poller() {
        let short = WATCHDOG_LONG_INTERVAL_SECS - 1; // e.g. 30m, under the 1h long bound
        let long = WATCHDOG_LONG_INTERVAL_SECS; // already at a long cadence
        // Args: (open_tasks, interval, stood_down, reactive, deliberate_monitor, patrol, verdict).
        // Live, drained (0 actionable), short interval, none of the exclusions → lengthen it.
        assert!(drained_idle_candidate(0, short, false, false, false, false, "ok"));
        assert!(drained_idle_candidate(0, short, false, false, false, false, "late"), "late still counts (ticking)");
        // Has actionable work → not this path (the work-driven/retighten paths own that).
        assert!(!drained_idle_candidate(1, short, false, false, false, false, "ok"));
        // Already at a long interval → nothing to lengthen (this is what convergence looks like).
        assert!(!drained_idle_candidate(0, long, false, false, false, false, "ok"));
        // Stood down → a spun-down agent is left alone, not lengthened.
        assert!(!drained_idle_candidate(0, short, true, false, false, false, "ok"));
        // Reactive responder paces on mentions, not on an idle interval → excluded.
        assert!(!drained_idle_candidate(0, short, false, true, false, false, "ok"));
        // Deliberate continuous monitor (holds a monitor_exempt task) is MEANT to poll → never lengthened.
        assert!(!drained_idle_candidate(0, short, false, false, true, false, "ok"));
        // #544 fix: a PATROL/sweep agent (board-follow-up / board-triage) runs 0 tasks at a short cadence BY
        // DESIGN — lengthening it would blind the anti-stall layer, so it is excluded even though every other
        // signal says "drained self-poller".
        assert!(!drained_idle_candidate(0, short, false, false, false, true, "ok"), "patrol agent is never lengthened");
        // STALE = a stopped loop (a re-arm/relaunch case), not an over-eager poller → not this path.
        assert!(!drained_idle_candidate(0, short, false, false, false, false, "STALE"));
        // Unknown/zero interval → can't call it a short self-poller.
        assert!(!drained_idle_candidate(0, 0, false, false, false, false, "ok"));
    }

    #[test]
    fn monitor_exempt_task_owners_collects_only_exempt_task_assignees() {
        let tasks = vec![
            serde_json::json!({"assignee":"v-monitor","status":"in_progress","monitor_exempt":true}),
            serde_json::json!({"assignee":"v-worker","status":"in_progress","monitor_exempt":false}),
            serde_json::json!({"assignee":"","status":"in_progress","monitor_exempt":true}), // empty → dropped
        ];
        let owners = monitor_exempt_task_owners(&tasks);
        assert!(owners.contains("v-monitor"));
        assert!(!owners.contains("v-worker"), "a non-exempt task's owner is not a deliberate monitor");
        assert_eq!(owners.len(), 1);
    }

    #[test]
    fn agent_never_ticked_flags_a_launched_but_dead_on_arrival_loop() {
        use time::{format_description::well_known::Rfc3339, Duration};
        let now = time::OffsetDateTime::now_utc();
        let stamp = |d: Duration| (now - d).format(&Rfc3339).unwrap();
        let grace = 600; // 10m

        // last_seen never advanced past created_at, and it registered well before the grace window → the
        // #417 launch-crash signature (board-triage/board-follow-up: online but never ticked).
        let old = stamp(Duration::minutes(20));
        assert!(agent_never_ticked(&old, &old, now, grace), "equal stamps past the grace window → never-ticked");

        // Still inside the grace window: a just-registered agent legitimately has last_seen == created_at
        // until its first tick lands — must NOT flag.
        let fresh = stamp(Duration::minutes(2));
        assert!(!agent_never_ticked(&fresh, &fresh, now, grace), "within grace → still booting, not a crash");

        // Ticked at least once (last_seen advanced past created_at) → live/idle, never a never-ticked flag,
        // however old the heartbeat is.
        let created = stamp(Duration::hours(4));
        let seen = stamp(Duration::minutes(30));
        assert!(!agent_never_ticked(&created, &seen, now, grace), "advanced last_seen → it ticked, not dead-on-arrival");

        // Epsilon: a sub-2s precision difference between the two columns still counts as equal ...
        let created_eps = stamp(Duration::minutes(20));
        let seen_eps = stamp(Duration::minutes(20) - Duration::seconds(1));
        assert!(agent_never_ticked(&created_eps, &seen_eps, now, grace), "1s column drift is within epsilon");
        // ... but a real multi-second advance is a genuine tick.
        let seen_ticked = stamp(Duration::minutes(20) - Duration::seconds(5));
        assert!(!agent_never_ticked(&created_eps, &seen_ticked, now, grace), "5s advance is a real tick");

        // Unparseable stamps never flag (degrade safe).
        assert!(!agent_never_ticked("", "", now, grace));
        assert!(!agent_never_ticked("garbage", "garbage", now, grace));
    }

    #[test]
    fn match_session_id_in_finds_the_file_by_bare_id() {
        use std::path::PathBuf;
        let candidates = vec![
            PathBuf::from("/home/u/.claude/projects/-x/2ac1ff53-859a-4205-8a04-2be2f6f2e1b4.jsonl"),
            PathBuf::from("/home/u/.claude/projects/-x/sess-9.jsonl"),
        ];
        // A bare session id (the observer-kickoff / observer.md form, #360) resolves to its file by file_stem.
        assert_eq!(
            match_session_id_in("2ac1ff53-859a-4205-8a04-2be2f6f2e1b4", &candidates),
            Some(candidates[0].clone())
        );
        assert_eq!(match_session_id_in("sess-9", &candidates), Some(candidates[1].clone()));
        // An unknown id matches nothing → the caller emits a clear error rather than reading a wrong file.
        assert!(match_session_id_in("no-such-session", &candidates).is_none());
        assert!(match_session_id_in("sess-9", &[]).is_none());
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
    fn roster_metadata_stripped_fires_only_when_every_row_lacks_metadata() {
        // The failure shape: a non-empty roster where NO agent carries a metadata object (the /agents LIST
        // endpoint dropped it) → warn.
        let stripped = vec![
            serde_json::json!({"id":"a","status":"online"}),
            serde_json::json!({"id":"b","status":"online"}),
        ];
        assert!(roster_metadata_stripped(&stripped));
        // Any row WITH metadata (even `false`/empty) means the endpoint is serving metadata → not this failure.
        let has_some = vec![
            serde_json::json!({"id":"a"}),
            serde_json::json!({"id":"b","metadata":{"native":false}}),
        ];
        assert!(!roster_metadata_stripped(&has_some));
        // A normal roster with metadata is fine.
        assert!(!roster_metadata_stripped(&[serde_json::json!({"id":"a","metadata":{"native":true}})]));
        // An empty roster is a board outage / no agents, NOT a metadata-stripping bug → do not warn.
        assert!(!roster_metadata_stripped(&[]));
    }

    #[test]
    fn inprogress_task_assignees_collects_nonempty_owners_deduped() {
        let tasks = vec![
            serde_json::json!({"id":1,"assignee":"v-bolero","status":"in_progress"}),
            serde_json::json!({"id":2,"assignee":"v-bolero","status":"in_progress"}), // same owner → deduped
            serde_json::json!({"id":3,"assignee":"librarian","status":"in_progress"}),
            serde_json::json!({"id":4,"assignee":"","status":"in_progress"}),          // unassigned → dropped
            serde_json::json!({"id":5,"status":"in_progress"}),                          // no assignee → dropped
        ];
        let owners = inprogress_task_assignees(&tasks);
        assert!(owners.contains("v-bolero") && owners.contains("librarian"));
        assert_eq!(owners.len(), 2, "deduped, and empty/absent assignees dropped");
        // Empty task list → empty set (a board query error degrades here → no false #506 violations).
        assert!(inprogress_task_assignees(&[]).is_empty());
    }

    #[test]
    fn inprogress_task_assignees_excludes_monitor_exempt_owners() {
        let tasks = vec![
            // A monitor-exempt in_progress task: its owner is a legitimate continuous monitor, not holding
            // work at rest → excluded (#506 Phase B / #167).
            serde_json::json!({"id":1,"assignee":"v-monitor","status":"in_progress","monitor_exempt":true}),
            // A normal in_progress task still contributes its owner.
            serde_json::json!({"id":2,"assignee":"v-worker","status":"in_progress","monitor_exempt":false}),
            // An owner holding BOTH an exempt and a non-exempt task is still flagged via the non-exempt one.
            serde_json::json!({"id":3,"assignee":"v-both","status":"in_progress","monitor_exempt":true}),
            serde_json::json!({"id":4,"assignee":"v-both","status":"in_progress"}),
        ];
        let owners = inprogress_task_assignees(&tasks);
        assert!(owners.contains("v-worker") && owners.contains("v-both"));
        assert!(!owners.contains("v-monitor"), "a purely monitor-exempt owner is not holding work at rest");
        assert_eq!(owners.len(), 2);
    }

    #[test]
    fn task_is_monitor_exempt_reads_the_derived_bool_defaulting_false() {
        assert!(task_is_monitor_exempt(&serde_json::json!({"id":1,"monitor_exempt":true})));
        assert!(!task_is_monitor_exempt(&serde_json::json!({"id":1,"monitor_exempt":false})));
        // Absent (pre-#167 payload or a non-exempt row) → false, so nothing is wrongly skipped.
        assert!(!task_is_monitor_exempt(&serde_json::json!({"id":1})));
    }

    #[test]
    fn task_is_parked_on_blocker_flags_any_non_empty_blocked_on_kind() {
        // task-board#178: an external/infra wait is parked, not stalled → skip its nudge.
        assert!(task_is_parked_on_blocker(&serde_json::json!({"id":1,"blocked_on_kind":"external"})));
        // Any other blocker kind (operator/task) is likewise parked.
        assert!(task_is_parked_on_blocker(&serde_json::json!({"id":1,"blocked_on_kind":"operator"})));
        assert!(task_is_parked_on_blocker(&serde_json::json!({"id":1,"blocked_on_kind":"task"})));
        // Absent or empty blocker → NOT parked, so a genuinely stale unblocked task is still nudged.
        assert!(!task_is_parked_on_blocker(&serde_json::json!({"id":1})));
        assert!(!task_is_parked_on_blocker(&serde_json::json!({"id":1,"blocked_on_kind":""})));
        assert!(!task_is_parked_on_blocker(&serde_json::json!({"id":1,"blocked_on_kind":null})));
    }

    #[test]
    fn seam_glob_matches_handles_star_doublestar_and_literals() {
        // Literal exact path.
        assert!(seam_glob_matches("crates/foo/perform_arg_ground.rs", "crates/foo/perform_arg_ground.rs"));
        assert!(!seam_glob_matches("crates/foo/other.rs", "crates/foo/perform_arg_ground.rs"));
        // `**` spans path segments.
        assert!(seam_glob_matches("crates/foo/a/b.rs", "crates/foo/**"));
        assert!(seam_glob_matches("crates/foo/a/b/c.rs", "crates/foo/**"));
        assert!(!seam_glob_matches("crates/bar/a.rs", "crates/foo/**"));
        // `**/<file>` matches that file at ANY depth.
        assert!(seam_glob_matches("a/b/perform_arg_ground.rs", "**/perform_arg_ground.rs"));
        assert!(seam_glob_matches("perform_arg_ground.rs", "**/perform_arg_ground.rs"));
        // Single `*` is one segment only — does NOT cross `/`.
        assert!(seam_glob_matches("crates/foo/mod.rs", "crates/*/mod.rs"));
        assert!(!seam_glob_matches("crates/foo/bar/mod.rs", "crates/*/mod.rs"), "* must not cross a slash");
        // `*.rs` is top-level only.
        assert!(seam_glob_matches("x.rs", "*.rs"));
        assert!(!seam_glob_matches("a/x.rs", "*.rs"), "*.rs is one segment, not recursive");
        // `**.rs` (or **/*.rs) matches nested.
        assert!(seam_glob_matches("a/x.rs", "**/*.rs"));
    }

    #[test]
    fn seam_touched_returns_only_on_seam_paths_and_is_green_when_empty() {
        let seams = vec!["crates/effects/**".to_string(), "**/const_fold.rs".to_string()];
        let changed = vec![
            "crates/effects/perform_arg_ground.rs".to_string(), // on seam (** dir)
            "crates/opt/const_fold.rs".to_string(),             // on seam (** file)
            "docs/readme.md".to_string(),                       // off seam
            "crates/syntax/lexer.rs".to_string(),               // off seam
        ];
        let hit = seam_touched(&changed, &seams);
        assert_eq!(hit, vec!["crates/effects/perform_arg_ground.rs", "crates/opt/const_fold.rs"]);
        // No seam-touching change -> GREEN (empty) -> caller need not wake the model.
        let clean = vec!["docs/x.md".to_string(), "crates/syntax/lexer.rs".to_string()];
        assert!(seam_touched(&clean, &seams).is_empty(), "no on-seam change is GREEN");
        // No declared seam -> nothing can match -> GREEN (the command treats 'no seam' separately).
        assert!(seam_touched(&changed, &[]).is_empty());
    }

    #[test]
    fn assistant_stop_reason_reads_only_completed_assistant_turns() {
        assert_eq!(
            assistant_stop_reason(&serde_json::json!({"type":"assistant","message":{"stop_reason":"refusal"}})),
            Some("refusal".to_string())
        );
        assert_eq!(
            assistant_stop_reason(&serde_json::json!({"type":"assistant","message":{"stop_reason":"end_turn"}})),
            Some("end_turn".to_string())
        );
        // Non-assistant records / missing stop_reason -> None (not counted as a turn).
        assert_eq!(assistant_stop_reason(&serde_json::json!({"type":"user","message":{"stop_reason":"refusal"}})), None);
        assert_eq!(assistant_stop_reason(&serde_json::json!({"type":"assistant","message":{"role":"assistant"}})), None);
        assert_eq!(assistant_stop_reason(&serde_json::json!({"type":"system"})), None);
    }

    #[test]
    fn safeguard_wedge_flags_only_a_trailing_run_of_refusals() {
        let r = |s: &[&str]| s.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        // A trailing run of >= threshold refusals -> WEDGED.
        assert!(safeguard_wedge(&r(&["end_turn", "refusal", "refusal", "refusal"]), 3));
        assert!(safeguard_wedge(&r(&["refusal", "refusal"]), 2));
        // Fewer than threshold trailing refusals -> not wedged.
        assert!(!safeguard_wedge(&r(&["refusal", "refusal"]), 3));
        // A refusal run that is NOT at the tail (recovered after) -> not wedged (the agent is working again).
        assert!(!safeguard_wedge(&r(&["refusal", "refusal", "refusal", "end_turn"]), 3));
        // Mixed tail -> not wedged.
        assert!(!safeguard_wedge(&r(&["refusal", "tool_use", "refusal"]), 3));
        // Empty / threshold 0 -> never wedged (no false positive on a fresh or empty transcript).
        assert!(!safeguard_wedge(&[], 3));
        assert!(!safeguard_wedge(&r(&["refusal", "refusal", "refusal"]), 0));
    }

    #[test]
    fn tail_stop_reasons_parses_only_trailing_assistant_turns() {
        let line = |t: &str, sr: &str| format!(r#"{{"type":"{t}","message":{{"stop_reason":"{sr}"}}}}"#);
        // Mixed transcript: a user line (no stop_reason) and a non-JSON line are both dropped; assistant
        // stop_reasons are returned in chronological order (newest last).
        let content = [
            line("assistant", "end_turn"),
            r#"{"type":"user"}"#.to_string(),
            "not json".to_string(),
            line("assistant", "refusal"),
            line("assistant", "refusal"),
        ]
        .join("\n");
        assert_eq!(tail_stop_reasons(&content, 80), vec!["end_turn", "refusal", "refusal"]);
        // A small tail window keeps only the LAST N lines (here the two trailing refusals).
        assert_eq!(tail_stop_reasons(&content, 2), vec!["refusal", "refusal"]);
    }

    #[test]
    fn read_file_tail_bounds_the_read_and_drops_a_partial_leading_line() {
        let path = std::env::temp_dir().join(format!("fleet-tail-{}-{}.txt", std::process::id(), line!()));
        std::fs::write(&path, "aaaa\nbbbb\ncccc\ndddd\n").unwrap();
        // A budget covering the whole 20-byte file starts at offset 0 -> returned verbatim (no partial drop).
        assert_eq!(read_file_tail(&path, 10_000).as_deref(), Some("aaaa\nbbbb\ncccc\ndddd\n"));
        // A 7-byte budget reads bytes [13..20] = "c\ndddd\n"; starting mid-file drops the partial first line.
        assert_eq!(read_file_tail(&path, 7).as_deref(), Some("dddd\n"));
        let _ = std::fs::remove_file(&path);
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
    fn normalize_repos_accepts_structured_bare_string_and_csv_shapes() {
        // Canonical structured form (what `fleet set-meta --repo` writes) → kept as-is.
        let structured = serde_json::json!([{"repo":"camshaft/fleet"},{"repo":"camshaft/cadenza","branch":"main"}]);
        assert_eq!(normalize_repos(Some(&structured)), vec![
            serde_json::json!({"repo":"camshaft/fleet"}),
            serde_json::json!({"repo":"camshaft/cadenza","branch":"main"}),
        ]);
        // A CSV STRING (the hand-mint shape, #472) → split + trimmed into {repo} entries, NOT silently dropped.
        let csv = serde_json::json!("Membrain, MembrainCDK, ElasticShuffleCDK");
        assert_eq!(normalize_repos(Some(&csv)), vec![
            serde_json::json!({"repo":"Membrain"}),
            serde_json::json!({"repo":"MembrainCDK"}),
            serde_json::json!({"repo":"ElasticShuffleCDK"}),
        ]);
        // A bare-string array → each wrapped as {repo}.
        let bare = serde_json::json!(["Membrain","MembrainCDK"]);
        assert_eq!(normalize_repos(Some(&bare)), vec![
            serde_json::json!({"repo":"Membrain"}),
            serde_json::json!({"repo":"MembrainCDK"}),
        ]);
        // Absent / empty / other shapes → no entries (the workspace_kind path handles off-tree agents).
        assert!(normalize_repos(None).is_empty());
        assert!(normalize_repos(Some(&serde_json::json!(""))).is_empty(), "empty string → no entries");
        assert!(normalize_repos(Some(&serde_json::json!("  ,  , "))).is_empty(), "blank CSV parts filtered out");
        assert!(normalize_repos(Some(&serde_json::json!(42))).is_empty());
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
        assert_eq!(watchdog_exec_args(true, false, false, false), "watchdog --rearm --stale-only");
        assert_eq!(watchdog_exec_args(true, true, false, false), "watchdog --rearm --stale-only --observe --spawn");
        assert_eq!(watchdog_exec_args(true, false, true, false), "watchdog --rearm --stale-only --pinned-only");
        // The green go-live shape: liveness + observer cadence + host filter.
        assert_eq!(
            watchdog_exec_args(true, true, true, false),
            "watchdog --rearm --stale-only --observe --spawn --pinned-only"
        );
        // OBSERVER-ONLY (rearm=false): coexists with an existing rearm watchdog without double-rearming (dev-desk).
        assert_eq!(watchdog_exec_args(false, true, false, false), "watchdog --observe --spawn");
        // --self-redeploy (#388) appends last: a local-checkout host installs the self-healing watchdog.
        assert_eq!(
            watchdog_exec_args(true, false, false, true),
            "watchdog --rearm --stale-only --self-redeploy"
        );
    }

    #[test]
    fn classify_wake_path_prefers_webhook_then_tunnel_then_poll_only() {
        // A non-empty webhook is a direct POST target — it wins even if a tunnel also covers the agent.
        assert_eq!(classify_wake_path(Some("http://127.0.0.1:8899/wake"), true), WakePath::Webhook);
        assert_eq!(classify_wake_path(Some("http://127.0.0.1:8899/wake"), false), WakePath::Webhook);
        // No webhook (None, empty, or whitespace) but a live tunnel → tunnel-woken.
        assert_eq!(classify_wake_path(None, true), WakePath::Tunnel);
        assert_eq!(classify_wake_path(Some(""), true), WakePath::Tunnel);
        assert_eq!(classify_wake_path(Some("   "), true), WakePath::Tunnel);
        // Neither → poll-only, the #386 regression. (v-knowledge-base's empty webhook + no tunnel was the
        // operator's original flagged instance.)
        assert_eq!(classify_wake_path(None, false), WakePath::PollOnly);
        assert_eq!(classify_wake_path(Some(""), false), WakePath::PollOnly);
    }

    #[test]
    fn agent_expected_running_excludes_non_running_statuses_kinds_and_standdown() {
        use serde_json::json;
        // A live loop agent (vertical/named/worker/legacy-None kind) in a running status is in scope.
        assert!(agent_expected_running(&json!({"status": "online", "kind": "vertical"})));
        assert!(agent_expected_running(&json!({"status": "away", "kind": "named-agent"})));
        assert!(agent_expected_running(&json!({"status": "busy"}))); // kind absent → legacy loop agent
        // Not-running statuses (any case) are excluded — a stood-down/finished/cancelled agent needs no wake.
        assert!(!agent_expected_running(&json!({"status": "offline"})));
        assert!(!agent_expected_running(&json!({"status": "OFFLINE"})));
        assert!(!agent_expected_running(&json!({"status": "done", "kind": "worker"})));
        assert!(!agent_expected_running(&json!({"status": "cancelled"})));
        // A pending stand-down request winds the agent down even before status flips.
        assert!(!agent_expected_running(&json!({"status": "online", "stand_down_requested_at": "2026-09-30T00:00:00Z"})));
        // A null stand-down field is NOT a stand-down.
        assert!(agent_expected_running(&json!({"status": "online", "stand_down_requested_at": null})));
        // Non-loop kinds have no event loop to strand: an interactive assistant session, an ephemeral observer.
        assert!(!agent_expected_running(&json!({"status": "online", "kind": "assistant"})));
        assert!(!agent_expected_running(&json!({"status": "online", "kind": "observer"})));
        // A STAGED reserve helper (#392) is expected-dormant — wake wired at launch — so not a poll-only gap,
        // mirroring the watchdog's own staged skip (PR #122).
        assert!(!agent_expected_running(&json!({"status": "online", "kind": "vertical", "metadata": {"staged": true}})));
        assert!(agent_expected_running(&json!({"status": "online", "kind": "vertical", "metadata": {"staged": false}})));
    }

    #[test]
    fn watchdog_stale_self_action_gates_redeploy_on_the_flag() {
        // Fresh binary (baked == HEAD) → nothing, regardless of the flag.
        assert_eq!(watchdog_stale_self_action("abc123", Some("abc123"), false), StaleSelfAction::Fresh);
        assert_eq!(watchdog_stale_self_action("abc123", Some("abc123"), true), StaleSelfAction::Fresh);
        // Can't tell (unknown rev / no checkout) → Fresh, never a spurious redeploy.
        assert_eq!(watchdog_stale_self_action("unknown", Some("def456"), true), StaleSelfAction::Fresh);
        assert_eq!(watchdog_stale_self_action("abc123", None, true), StaleSelfAction::Fresh);
        // Stale + flag OFF → warn only (the long-standing report-only behavior).
        assert!(matches!(
            watchdog_stale_self_action("abc123", Some("def456"), false),
            StaleSelfAction::Warn(_)
        ));
        // Stale + flag ON → redeploy trigger, carrying the same warning text.
        assert!(matches!(
            watchdog_stale_self_action("abc123", Some("def456"), true),
            StaleSelfAction::Redeploy(_)
        ));
    }

    #[test]
    fn render_watchdog_units_is_a_oneshot_service_plus_timer() {
        let u = render_watchdog_units("/run/fleet/bin/fleet", &watchdog_exec_args(true, true, true, false), 60, "");
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
        let (service, timer) = watchdog_unit_files("/bin/fleet", &watchdog_exec_args(false, true, false, false), 90, "");
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
        let (service, _timer) = watchdog_unit_files("/bin/fleet", &watchdog_exec_args(true, true, false, false), 60, &env);
        let env_at = service.find("Environment=\"PATH=").expect("env line present");
        let exec_at = service.find("ExecStart=").expect("ExecStart present");
        assert!(env_at < exec_at, "Environment= must precede ExecStart in the unit");
        // A rearm-only unit (no observe) is emitted with an empty env block — no launch environment needed.
        let (rearm_only, _) = watchdog_unit_files("/bin/fleet", &watchdog_exec_args(true, false, false, false), 60, "");
        assert!(!rearm_only.contains("Environment="), "rearm-only watchdog spawns nothing → no env block");
    }

    #[test]
    fn daemon_unit_file_is_a_restarting_simple_service_with_a_known_good_path() {
        let env = render_service_env_lines(&[("PATH", Some("/usr/bin:/bin".into()))]);
        let u = daemon_unit_file("notifier", "/run/fleet/bin/fleet notify", 2, &env);
        // A long-running daemon that survives a crash — NOT a oneshot; this is the durable replacement for the
        // bare keep-alive tmux window a reap silently kills (#359).
        assert!(u.contains("Type=simple"), "long-running daemon, not oneshot");
        assert!(u.contains("Restart=on-failure") && u.contains("RestartSec=2"), "restarts on crash");
        assert!(u.contains("ExecStart=/run/fleet/bin/fleet notify"));
        assert!(u.contains("WantedBy=default.target"), "enabled comes up on login/boot");
        // The captured PATH is seeded ahead of ExecStart so the daemon resolves tmux/git/curl at runtime (#347/#359).
        let env_at = u.find("Environment=\"PATH=/usr/bin:/bin\"").expect("PATH env line present");
        assert!(env_at < u.find("ExecStart=").expect("ExecStart present"), "env precedes ExecStart");
    }

    #[test]
    fn enable_argv_reloads_then_enables_now_the_named_unit() {
        let steps = enable_argv("fleet-notifier.service");
        // Two invocations, IN ORDER: reload so systemd sees the freshly-written unit, THEN enable --now.
        assert_eq!(steps.len(), 2);
        assert_eq!(steps[0], vec!["--user", "daemon-reload"]);
        assert_eq!(steps[1], vec!["--user", "enable", "--now", "fleet-notifier.service"]);
        // Every invocation is user-level (no sudo) — the same no-privilege install path as --install.
        assert!(steps.iter().all(|a| a.first().map(String::as_str) == Some("--user")), "all user-level");
        // The unit name is carried verbatim into the enable step (a tunnel unit enables the same way).
        assert_eq!(enable_argv("fleet-tunnel.service")[1].last().unwrap(), "fleet-tunnel.service");
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
    fn agent_is_staged_reads_the_reserve_flag() {
        assert!(agent_is_staged(Some(&serde_json::json!({ "staged": true }))));
        assert!(!agent_is_staged(Some(&serde_json::json!({ "staged": false }))));
        assert!(!agent_is_staged(Some(&serde_json::json!({}))), "absent flag → not staged");
        assert!(!agent_is_staged(Some(&serde_json::json!({ "staged": "true" }))), "non-bool → not staged");
        assert!(!agent_is_staged(None));
    }

    #[test]
    fn watchdog_never_manages_a_staged_agent() {
        // A staged reserve helper is not meant to be running, so the watchdog must never re-arm or observe it —
        // even when it is native + pinned to this exact box (which would otherwise be managed).
        let staged_here = serde_json::json!({ "host": "green-machine", "staged": true });
        assert!(!watchdog_manages_agent(Some(&staged_here), "green-machine", false));
        assert!(!watchdog_manages_agent(Some(&staged_here), "green-machine", true));
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
        assert_eq!(resolve_model("sonnet"), "us.anthropic.claude-sonnet-5");
        // The bare Anthropic-API id is remapped to the valid Bedrock id (the board-triage/-follow-up outage:
        // `claude-sonnet-5-5` 400s on this fleet's endpoint), so a mis-registered agent self-heals on launch.
        assert_eq!(resolve_model("claude-sonnet-5-5"), "us.anthropic.claude-sonnet-5");
        assert_eq!(resolve_model("sonnet-5-5"), "us.anthropic.claude-sonnet-5");
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
        // No commit/PR attribution lines in the observer's board task bodies (board-pm; the observer was
        // leaking a "Generated with ..." line into task bodies).
        assert!(k.contains("Co-Authored-By:") && k.contains("not board content"), "observer kickoff bans attribution lines in board bodies");
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

    #[test]
    fn stale_task_should_nudge_gates_first_nudge_on_threshold_and_renudge_on_cooldown() {
        let threshold = 3600; // 1h
        let cooldown = 7200; // 2h
        let acked = 14400; // 4h extended cooldown
        // Helper: the no-ack (normal cadence) case, so the existing assertions read unchanged.
        let n = |idle, last| stale_task_should_nudge(idle, last, false, threshold, cooldown, acked);
        // Fresh (under threshold) → never nudge, nudged before or not.
        assert!(!n(threshold - 1, None));
        assert!(!n(0, None));
        // At/over threshold with no prior nudge → first nudge fires.
        assert!(n(threshold, None));
        assert!(n(threshold * 10, None));
        // Still idle, but the prior nudge is younger than the cooldown → no re-nudge (no spam).
        assert!(!n(threshold * 5, Some(cooldown - 1)));
        // Prior nudge at/past the cooldown → re-nudge.
        assert!(n(threshold * 5, Some(cooldown)));
        assert!(n(threshold * 5, Some(cooldown * 3)));

        // task_540 acked-cooldown: a fresh assignee ack extends the re-nudge cooldown to `acked`.
        // Prior nudge past the NORMAL cooldown but within the ACKED cooldown → suppressed WHEN acked, fired
        // when not (so acking a queued todo buys the longer quiet).
        assert!(!stale_task_should_nudge(threshold * 5, Some(cooldown + 1), true, threshold, cooldown, acked));
        assert!(stale_task_should_nudge(threshold * 5, Some(cooldown + 1), false, threshold, cooldown, acked));
        // Anti-parking: even with a fresh ack, once the prior nudge is past the ACKED cooldown it re-nudges.
        assert!(stale_task_should_nudge(threshold * 5, Some(acked), true, threshold, cooldown, acked));
        // The ack flag never overrides the threshold gate, and never fabricates a first nudge early.
        assert!(!stale_task_should_nudge(threshold - 1, None, true, threshold, cooldown, acked));
        assert!(stale_task_should_nudge(threshold, None, true, threshold, cooldown, acked));
    }

    #[test]
    fn assignee_ack_fresher_than_last_nudge_requires_both_and_a_newer_ack() {
        // Ack newer than the nudge (smaller age) → fresher.
        assert!(assignee_ack_fresher_than_last_nudge(Some(10), Some(100)));
        // Ack older than, or equal to, the nudge → not fresher (a stale ETA stops buying quiet; equal is not newer).
        assert!(!assignee_ack_fresher_than_last_nudge(Some(100), Some(10)));
        assert!(!assignee_ack_fresher_than_last_nudge(Some(50), Some(50)));
        // Missing either side → not fresher (incl. before the first nudge: no nudge to be newer than).
        assert!(!assignee_ack_fresher_than_last_nudge(None, Some(100)));
        assert!(!assignee_ack_fresher_than_last_nudge(Some(10), None));
        assert!(!assignee_ack_fresher_than_last_nudge(None, None));
    }

    #[test]
    fn newest_assignee_comment_age_secs_picks_the_assignees_most_recent_comment() {
        use time::{format_description::well_known::Rfc3339, Duration};
        let now = time::OffsetDateTime::now_utc();
        let stamp = |d: Duration| (now - d).format(&Rfc3339).unwrap();
        let task = serde_json::json!({
            "comments": [
                { "author": "fleet-nudge-daemon", "created_at": stamp(Duration::minutes(1)) },
                { "author": "v-x", "created_at": stamp(Duration::hours(3)) },
                { "author": "someone-else", "created_at": stamp(Duration::minutes(2)) },
                { "author": "v-x", "created_at": stamp(Duration::hours(1)) },
            ],
        });
        // Picks v-x's MOST RECENT comment (1h), ignoring the daemon's and others' newer comments.
        let age = newest_assignee_comment_age_secs(&task, "v-x", now).unwrap();
        assert!((age - 3600).abs() < 2, "newest assignee comment is 1h old, got {age}");
        // An assignee who never commented → None.
        assert_eq!(newest_assignee_comment_age_secs(&task, "v-never", now), None);
    }

    #[test]
    fn task_latest_activity_age_secs_is_the_freshest_of_updated_at_and_any_comment() {
        use time::{format_description::well_known::Rfc3339, Duration};
        let now = time::OffsetDateTime::now_utc();
        let stamp = |d: Duration| (now - d).format(&Rfc3339).unwrap();

        // No comments: falls back to updated_at alone.
        let no_comments = serde_json::json!({ "updated_at": stamp(Duration::hours(3)) });
        let age = task_latest_activity_age_secs(&no_comments, now).unwrap();
        assert!((age - 3 * 3600).abs() < 2, "age ~= 3h, got {age}");

        // A comment newer than updated_at (comments do NOT bump updated_at on this board) makes the task
        // fresh even though updated_at itself is old — the #478 trap this function exists to avoid.
        let fresh_comment = serde_json::json!({
            "updated_at": stamp(Duration::hours(10)),
            "comments": [
                { "author": "someone", "created_at": stamp(Duration::hours(9)) },
                { "author": "someone", "created_at": stamp(Duration::minutes(20)) },
            ],
        });
        let age = task_latest_activity_age_secs(&fresh_comment, now).unwrap();
        assert!(age < 3600, "the 20m-old comment wins over the 10h-old updated_at, got {age}");

        // Every comment older than updated_at: updated_at (the most recent real event) wins.
        let stale_comments = serde_json::json!({
            "updated_at": stamp(Duration::minutes(5)),
            "comments": [{ "author": "someone", "created_at": stamp(Duration::hours(4)) }],
        });
        let age = task_latest_activity_age_secs(&stale_comments, now).unwrap();
        assert!(age < 600, "updated_at (5m old) beats an older comment, got {age}");
    }

    #[test]
    fn task_last_nudge_age_secs_only_counts_this_daemons_own_comments() {
        use time::{format_description::well_known::Rfc3339, Duration};
        let now = time::OffsetDateTime::now_utc();
        let stamp = |d: Duration| (now - d).format(&Rfc3339).unwrap();

        let never_nudged = serde_json::json!({
            "comments": [{ "author": "someone-else", "created_at": stamp(Duration::hours(1)) }],
        });
        assert_eq!(task_last_nudge_age_secs(&never_nudged, now), None);

        let nudged_twice = serde_json::json!({
            "comments": [
                { "author": "someone-else", "created_at": stamp(Duration::minutes(1)) },
                { "author": NUDGE_AUTHOR, "created_at": stamp(Duration::hours(3)) },
                { "author": NUDGE_AUTHOR, "created_at": stamp(Duration::hours(1)) },
            ],
        });
        let age = task_last_nudge_age_secs(&nudged_twice, now).unwrap();
        assert!((age - 3600).abs() < 2, "picks the MOST RECENT own nudge (1h), not the older one, got {age}");
    }

    #[test]
    fn format_hm_renders_hours_and_minutes() {
        assert_eq!(format_hm(0), "0h0m");
        assert_eq!(format_hm(59), "0h0m");
        assert_eq!(format_hm(60), "0h1m");
        assert_eq!(format_hm(3600), "1h0m");
        assert_eq!(format_hm(3600 * 3 + 60 * 12), "3h12m");
        assert_eq!(format_hm(-5), "0h0m", "never goes negative");
    }

    #[test]
    fn is_tracking_parent_with_open_children_skips_only_parents_with_open_kids() {
        let cr = |body: serde_json::Value| body;
        // A parent with open children (done < total) → tracking parent, skip the nudge.
        let open = cr(serde_json::json!({"child_rollup":{"done":1,"total":3}}));
        assert!(is_tracking_parent_with_open_children(open.get("child_rollup")));
        // All children done (done == total) → NOT exempted; the parent itself may need a nudge to close.
        let all_done = cr(serde_json::json!({"child_rollup":{"done":3,"total":3}}));
        assert!(!is_tracking_parent_with_open_children(all_done.get("child_rollup")));
        // A leaf task (no children, total == 0) → not a tracking parent, nudge as normal.
        let leaf = cr(serde_json::json!({"child_rollup":{"done":0,"total":0}}));
        assert!(!is_tracking_parent_with_open_children(leaf.get("child_rollup")));
        // Missing child_rollup → not exempted.
        assert!(!is_tracking_parent_with_open_children(None));
    }

    #[test]
    fn task_has_worker_activity_needs_a_non_nudge_comment() {
        // A real (non-daemon) comment = work started → a todo with this qualifies for a nudge.
        let planned = serde_json::json!({"comments":[{"author":"board-pm","body":"plan: ..."}]});
        assert!(task_has_worker_activity(&planned));
        // Only the nudge daemon's own comments do NOT count — a bare todo the daemon has never legitimately
        // nudged can't self-qualify (and this avoids a self-sustaining nudge loop).
        let only_nudges = serde_json::json!({"comments":[{"author":NUDGE_AUTHOR,"body":"fleet nudge: ..."}]});
        assert!(!task_has_worker_activity(&only_nudges));
        // No comments at all → untouched backlog, not a stall.
        assert!(!task_has_worker_activity(&serde_json::json!({"comments":[]})));
        assert!(!task_has_worker_activity(&serde_json::json!({})));
        // A mix (worker + nudge) still counts — the worker comment is present.
        let mixed = serde_json::json!({"comments":[{"author":NUDGE_AUTHOR},{"author":"v-runtime"}]});
        assert!(task_has_worker_activity(&mixed));
    }

    #[test]
    fn owner_is_gone_flags_only_an_owner_absent_from_the_roster() {
        let roster: std::collections::BTreeSet<String> =
            ["v-alpha", "v-beta"].iter().map(|s| s.to_string()).collect();
        // A registered owner is present regardless of its status (offline is deliberate/resumable, board-pm
        // seq-7848) → NOT gone, so its stale task stays with it.
        assert!(!owner_is_gone("v-alpha", &roster));
        assert!(!owner_is_gone("v-beta", &roster));
        // An owner no longer in the roster (retired/removed) → gone → its stale task is orphaned and routes.
        assert!(owner_is_gone("v-retired", &roster));
    }

    #[test]
    fn route_body_names_the_router_reason_and_actions() {
        let b = route_body("unassigned", 1.0, 7200);
        assert!(b.contains(NUDGE_ROUTER), "names the router (board-pm)");
        assert!(b.contains("unassigned") && b.contains("idle 2h"), "carries the reason + idle age");
        assert!(b.contains("assign it to a capable agent") && b.contains("update its status"), "actionable for the router");
        // The idle-owner reason is carried verbatim too.
        assert!(route_body("owner v-x is idle/dead", 1.0, 3600).contains("owner v-x is idle/dead"));
    }

    #[test]
    fn nudge_body_is_actionable_reassign_or_status() {
        let b = nudge_body(1.0, "v-runtime", 7200);
        // Names the assignee and the idle duration.
        assert!(b.contains("v-runtime") && b.contains("idle 2h"));
        // Spells out the actionable choices the operator asked for (#540), not just "post an update".
        assert!(b.contains("progress update or ETA"), "keeps the update/ETA option");
        assert!(b.contains("reassign"), "offers reassignment when the owner can't progress it");
        assert!(b.contains("done") && b.contains("blocked with a blocked_on note"), "offers the status transitions");
        assert!(b.contains("unsure whether it is blocked"), "covers the maybe-blocked case");
    }
}
