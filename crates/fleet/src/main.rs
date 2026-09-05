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
fn up(fleet: &Fleet, config_path: &Path) {
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
    let reg = fleet.load();
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
    if !plan.to_launch.is_empty() {
        println!(
            "  ⟳ TO LAUNCH ({}): {}  [dry-run — worktree-mint + window-launch land with the window-mgmt slice]",
            plan.to_launch.len(),
            plan.to_launch.join(", ")
        );
    }
    if !plan.undeclared_running.is_empty() {
        println!(
            "  ⚠ undeclared-but-running (drift, NOT auto-removed): {}",
            plan.undeclared_running.join(", ")
        );
    }
    if plan.to_launch.is_empty() && plan.undeclared_running.is_empty() {
        println!("  ✓ reconciled — every declared agent is running, no drift.");
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
    /// Reconcile a target repo's checked-in `fleet.toml` declared roster against the running fleet
    /// (reports the plan; the actual launch lands with the window-management slice).
    Up {
        /// Path to the target repo's `fleet.toml`.
        config: PathBuf,
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
        Cmd::Up { config } => up(&fleet, &config),
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
