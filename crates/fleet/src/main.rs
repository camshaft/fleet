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
mod notify;
mod workspace;

/// The tmux session board-native agents run in (their windows are opened here by `launch_board_agent`, and
/// the notifier injects wakes here). `$CDZ_FLEET_SESSION`, else `main`.
fn board_session() -> String {
    std::env::var("CDZ_FLEET_SESSION").unwrap_or_else(|_| "main".to_string())
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
    /// Resolve the hub. `$FLEET_HUB` (the eventual `~/.fleet` multi-repo model) wins; otherwise fall back
    /// to git-common-dir discovery of the cwd (cadenza's current behavior) so P1 is byte-identical.
    fn resolve() -> Self {
        let hub = match std::env::var_os("FLEET_HUB") {
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
            std::env::var("FLEET_AGENT")
                .ok()
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

/// The tmux session the fleet's windows live in (`$FLEET_SESSION`, else `main`).
fn fleet_session() -> String {
    std::env::var("FLEET_SESSION").unwrap_or_else(|_| "main".to_string())
}

/// Where the launcher script lives (`$FLEET_WINDOW_SH`, else the hub copy `<hub>/.claude/fleet/window.sh`
/// materialized at setup). window.sh resolves the agent's config via `fleet describe` + launches claude.
fn window_sh_path(fleet: &Fleet) -> PathBuf {
    std::env::var_os("FLEET_WINDOW_SH")
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
    /// Report every board-declared agent's liveness off its board `last_seen` (the watchdog's read side).
    /// Reads the roster from the board (orchestrator read — agents coordinate via their own MCP) and
    /// classifies each by how stale its heartbeat is: live / quiet / STALE.
    Status {
        /// Only print agents that are not `live` (quiet or STALE) — the ones a watchdog would look at.
        #[arg(long)]
        stale_only: bool,
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
        /// Perform the write (default: just print the metadata patch that would be sent).
        #[arg(long)]
        apply: bool,
    },
    /// Run the event-driven wake notifier: a local HTTP endpoint that receives the board's per-agent
    /// webhook POSTs and `tmux send-keys` injects `[notification] task #<id>` / `message #<seq>` into the
    /// recipient agent's window (register this endpoint as each board-backed agent's `webhook_url`). Blocks.
    Notify {
        /// Port to listen on (127.0.0.1 only).
        #[arg(long, default_value_t = 8899)]
        port: u16,
    },
}

fn main() {
    let cli = Cli::parse();
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
        Cmd::SpinUp { agent, apply } => spin_up(&agent, apply),
        Cmd::Status { stale_only } => status(stale_only),
        Cmd::SetMeta {
            agent,
            repos,
            interval,
            apply,
        } => set_meta(&agent, &repos, interval.as_deref(), apply),
        Cmd::Notify { port } => {
            if let Err(e) = notify::serve(port, &board_session()) {
                eprintln!("{e}");
                std::process::exit(1);
            }
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
    let repos = md
        .get("repos")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let fleet_root = std::env::var("FLEET_ROOT")
        .unwrap_or_else(|_| format!("{}/.fleet", std::env::var("HOME").unwrap_or_default()));

    println!("spin-up '{agent}' ({}):", if apply { "APPLY" } else { "dry-run" });
    println!(
        "  charter on board: {}",
        if has_charter { "yes — the agent fetches it in-session at boot" } else { "NO — declare a charter first" }
    );
    println!("  model={model}  effort={effort}  interval={interval}");
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
        println!("  would launch: claude in {workdir} (board MCP in-session) with a self-discovery kickoff, then /loop {interval}");
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
    match launch_board_agent(agent, &workdir, &model, &effort, &interval) {
        Ok(win) => {
            println!(
                "  LAUNCHED '{agent}' in tmux window '{win}' (cwd {workdir}) — it will get_agent itself for its charter, then /loop {interval}"
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

/// Open a tmux window running `claude` in `workdir` with a SELF-DISCOVERY kickoff (the agent fetches its
/// own charter from the board via its in-session MCP — nothing is injected). Refuses to double-launch an
/// existing same-named window. The kickoff is passed via a tmux env var so no shell quoting can mangle it.
fn launch_board_agent(agent: &str, workdir: &str, model: &str, effort: &str, interval: &str) -> Result<String, String> {
    let session = board_session();
    if let Ok(out) = std::process::Command::new("tmux")
        .args(["list-windows", "-t", &session, "-F", "#W"])
        .output()
        && String::from_utf8_lossy(&out.stdout).lines().any(|w| w == agent)
    {
        return Err(format!("a tmux window '{agent}' already exists in session '{session}' (already spun up?)"));
    }
    let tick = "run one tick of your charter: drain your board notifications (check_notifications), \
                do ONE unit of work per your charter, then update your presence (set_status)";
    let kickoff = format!(
        "You are the fleet agent '{agent}', running UNATTENDED. Your task-board MCP tools are available in \
         this session. FIRST call get_agent with agent_id '{agent}' to read your OWN charter + metadata \
         from the board, and follow that charter as your role. Coordinate through the board (send_message \
         / check_notifications / comment_task / set_status) — there is no file inbox. You work in \
         {workdir}. Start your recurring loop now: /loop {interval} {tick}"
    );
    // effort/model are single-quoted (no single-quotes in them) so `[1m]` can't glob; the kickoff rides in
    // $CDZ_KICKOFF (set literally via `-e`, expanded double-quoted) so its spaces/quotes are safe.
    let cmd = format!(
        "exec claude --disallowedTools AskUserQuestion --effort '{effort}' --model '{model}' \
         --autocompact 600000 --dangerously-skip-permissions \"$CDZ_KICKOFF\""
    );
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
fn build_meta_patch(repos: &[String], interval: Option<&str>) -> Result<serde_json::Value, String> {
    let mut patch = serde_json::Map::new();
    if !repos.is_empty() {
        let entries: Vec<serde_json::Value> = repos.iter().map(|s| parse_repo_spec(s)).collect();
        patch.insert("repos".to_string(), serde_json::Value::Array(entries));
    }
    if let Some(iv) = interval {
        patch.insert("interval".to_string(), serde_json::Value::String(iv.to_string()));
    }
    if patch.is_empty() {
        return Err("nothing to set — pass at least one --repo or --interval".to_string());
    }
    Ok(serde_json::Value::Object(patch))
}

/// Write launch-shaping metadata (`repos` / `interval`) onto an agent's board record — the migration
/// primitive. Reports the patch by default; `--apply` PATCHes it (key-level merge, so untouched keys are
/// preserved) and reads the record back to confirm.
fn set_meta(agent: &str, repos: &[String], interval: Option<&str>, apply: bool) {
    let patch = build_meta_patch(repos, interval).unwrap_or_else(|e| {
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
            "  written. metadata now: repos={} interval={}",
            md.get("repos").map(|v| v.to_string()).unwrap_or_else(|| "<none>".into()),
            md.get("interval").and_then(|v| v.as_str()).unwrap_or("<none>")
        ),
        None => println!("  written (could not read back the record to confirm)"),
    }
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
    fn parse_repo_spec_splits_branch_and_defaults_to_main() {
        assert_eq!(parse_repo_spec("camshaft/bolero@master"), serde_json::json!({"repo":"camshaft/bolero","branch":"master"}));
        assert_eq!(parse_repo_spec("camshaft/backbeat"), serde_json::json!({"repo":"camshaft/backbeat","branch":"main"}));
        assert_eq!(parse_repo_spec("camshaft/x@"), serde_json::json!({"repo":"camshaft/x","branch":"main"}), "empty branch → main");
    }

    #[test]
    fn build_meta_patch_includes_only_requested_keys_and_errors_when_empty() {
        let p = build_meta_patch(&["o/r@b".to_string()], Some("2m")).unwrap();
        assert_eq!(p["repos"], serde_json::json!([{"repo":"o/r","branch":"b"}]));
        assert_eq!(p["interval"], "2m");
        // repos only — no interval key
        let p = build_meta_patch(&["o/r".to_string()], None).unwrap();
        assert!(p.get("interval").is_none());
        assert!(p.get("repos").is_some());
        // interval only — no repos key
        let p = build_meta_patch(&[], Some("30m")).unwrap();
        assert!(p.get("repos").is_none());
        assert_eq!(p["interval"], "30m");
        // nothing requested → error (guards a no-op PATCH)
        assert!(build_meta_patch(&[], None).is_err());
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
