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
mod concierge_mint;
mod config;
mod dream;
mod dream_apply;
mod memory;
mod notify;
mod scan;
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
        _ => true, // an odd shape shouldn't strand the agent — treat as unpinned
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

/// task_1037: whether an agent is launch-GATED — the operator set an explicit gate (`metadata.launch_gated ==
/// true`, or a non-null `metadata.launch_gated_on` naming what it waits on) so no auto-launch path may bring it
/// up until the gate clears (e.g. `frank`, gated on daemon mention-wake wiring). Absent → not gated. Pure —
/// unit-tested.
fn agent_is_launch_gated(md: Option<&serde_json::Value>) -> bool {
    md.map(|m| {
        m.get("launch_gated")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false)
            || m.get("launch_gated_on").is_some_and(|v| !v.is_null())
    })
    .unwrap_or(false)
}

/// task_1178: the version of the fleet-wide comms norms the kickoff carries — the operator-comms-via-concierge
/// clause (do not DM a human identity) and the operator-block-needs-a-question clause. [`build_kickoff`] bakes
/// both into every agent's boot prompt, and [`launch_metadata_patch`] stamps this version onto the agent's
/// board record on launch, so v-task-board's seed-before-flip preflight can confirm every active agent carries
/// the clauses (`metadata.comms_norm_ack >= 1`) before hard-rejecting a direct-to-human send (task_1164) or a
/// questionless operator-block (task_1150). Bump when the clause materially changes.
const COMMS_NORM_VERSION: i64 = 1;

/// The metadata-merge patch applied to a board agent record on a successful spin-up launch. Two concerns:
///
/// 1. task_1144 — a launch SATISFIES an agent's one-time bringup gate. `metadata.launch_gated` /
///    `launch_gated_on` describe what an agent waited on before its FIRST launch (e.g. frank's "daemon
///    mention-wake wiring"); the marker is set at mint but was never cleared once the agent went live, so a
///    now-running production agent still read as launch-gated ([`agent_is_launch_gated`]) and was wrongly HELD
///    out of a mass reconstitution (`spin-up-all` / `up-board` dropped `frank`). Clearing the gate on launch
///    makes the next reconstitution bring it back, while a genuinely-never-launched gated agent (marker still
///    set) stays held.
/// 2. task_1178 — the launch used the kickoff that carries the comms-norm clauses, so stamp
///    `metadata.comms_norm_ack = COMMS_NORM_VERSION` to mark this agent carries them (v-task-board's flip
///    preflight keys on it). Coverage builds as agents bounce onto the clause-bearing kickoff.
///
/// Returns `None` when nothing needs writing — no gate set AND already acked at the current version
/// (idempotent, so a steady-state re-launch does not churn the record). Pure — unit-tested.
fn launch_metadata_patch(md: Option<&serde_json::Value>) -> Option<serde_json::Value> {
    let gate_set = agent_is_launch_gated(md);
    let acked = md
        .and_then(|m| m.get("comms_norm_ack"))
        .and_then(serde_json::Value::as_i64)
        == Some(COMMS_NORM_VERSION);
    if !gate_set && acked {
        return None;
    }
    let mut patch = serde_json::Map::new();
    patch.insert(
        "comms_norm_ack".to_string(),
        serde_json::json!(COMMS_NORM_VERSION),
    );
    if gate_set {
        patch.insert("launch_gated".to_string(), serde_json::json!(false));
        patch.insert("launch_gated_on".to_string(), serde_json::Value::Null);
    }
    Some(serde_json::Value::Object(patch))
}

/// task_1182: find the local tunnel-client pid serving `name` in a `tunnel list --json` payload, or `None`
/// if no tunnel by that name is registered. Pure — unit-tested apart from the `tunnel` CLI. Used by
/// [`tunnel_guard`] to shed exactly the stale client (by pid), never a `-f` pattern-kill (the self-match trap).
fn tunnel_pid_for_name(list_json: &str, name: &str) -> Option<u32> {
    let v: serde_json::Value = serde_json::from_str(list_json).ok()?;
    v.get("tunnels")?
        .as_array()?
        .iter()
        .find(|t| t.get("tunnelName").and_then(serde_json::Value::as_str) == Some(name))
        .and_then(|t| t.get("meta")?.get("pid")?.as_u64())
        .map(|p| p as u32)
}

/// task_1182: whether a `curl -w %{http_code}` probe of the tunnel's public URL shows the local client is
/// FORWARDING. A 2xx or 3xx response means the request reached the origin through the client (a public board
/// root legitimately answers `302`, so a redirect counts as healthy); a tunnel-edge gateway error (`502`/
/// `503`/`504`) or a curl failure (empty / `000`) means the client is alive-but-not-forwarding. Callers should
/// therefore point `--probe-url` at a path that returns 2xx/3xx when healthy, not one gated behind a `401`.
/// Pure — unit-tested.
fn tunnel_probe_healthy(code_output: &str) -> bool {
    let c = code_output.trim();
    c.len() == 3
        && (c.starts_with('2') || c.starts_with('3'))
        && c.chars().all(|ch| ch.is_ascii_digit())
}

/// How `tunnel_guard` recovers an unhealthy tunnel. [`tunnel_recovery_plan`] picks it from the caller's args.
#[derive(Debug, PartialEq, Eq)]
enum TunnelRecovery<'a> {
    /// systemd-managed: a single `systemctl --user restart <unit>` — systemd owns the client's lifecycle, so a
    /// pid-kill would race its auto-respawn; the restart also re-reads the unit's `EnvironmentFile`.
    SystemdRestart(&'a str),
    /// Standalone: shed the client by its exact pid, then re-run the host's `recreate` command detached.
    ShedAndRecreate,
}

/// Decide the recovery for an unhealthy tunnel from the caller's args (task_1182): `--systemd-unit` wins (and
/// `recreate` is then irrelevant); otherwise a non-empty `recreate` selects the shed+recreate path. Neither
/// given is a misconfiguration (nothing to recover with). Pure — unit-tested — so the arg contract is checked
/// without touching systemd or the tunnel CLI.
fn tunnel_recovery_plan<'a>(
    systemd_unit: Option<&'a str>,
    recreate: &str,
) -> Result<TunnelRecovery<'a>, String> {
    match systemd_unit {
        Some(unit) if !unit.trim().is_empty() => Ok(TunnelRecovery::SystemdRestart(unit)),
        _ if !recreate.trim().is_empty() => Ok(TunnelRecovery::ShedAndRecreate),
        _ => Err(
            "tunnel-guard: need --recreate or --systemd-unit to recover an unhealthy tunnel"
                .to_string(),
        ),
    }
}

/// `fleet tunnel-guard` — keep a browser-facing tunnel's CLI client CONNECTED, not merely alive (task_1182).
///
/// The failure this exists for: the toolbox `tunnel create` client can go half-dead — its local process stays
/// alive while its forwarding connection drops — so a plain process-liveness or systemd `Restart=on-failure`
/// supervisor never notices, and the operator is silently blocked from the board in-browser until a human
/// re-runs `tunnel create` (the live incident that filed task_1182). So this probes CONNECTIVITY end-to-end
/// THROUGH the tunnel (not a local-port check), and on failure recovers it. It is kept GENERIC — the tunnel
/// name, the probe URL, and the recovery inputs are all caller-supplied (the host wires the specific
/// board-tunnel values), so the fleet CLI carries no host-specific tunnel config. The probe is a plain `curl`
/// of the PUBLIC url — a true external end-to-end check that needs no tunnel binary and exercises the whole
/// edge→client→origin path.
///
/// Two recovery modes ([`tunnel_recovery_plan`]): with `--systemd-unit` the tunnel is systemd-managed, so the
/// recovery is a single `systemctl --user restart <unit>` — a pid-kill + detached recreate would be WRONG there
/// (systemd `Restart=always` respawns the killed pid AND the recreate spawns a second, racing client). Without
/// it, the standalone path sheds the client by its exact pid (via `tunnel_bin list`) and re-runs `recreate`
/// detached. Run it from a systemd timer / oneshot (which does not overlap) so two fires never race. `--apply`
/// gates the recovery; without it, it only reports.
fn tunnel_guard(
    name: &str,
    probe_url: &str,
    recreate: &str,
    tunnel_bin: &str,
    apply: bool,
    systemd_unit: Option<&str>,
) {
    // Fail fast on a misconfigured guard (neither recovery input) BEFORE probing, so a bad unit never silently
    // reports healthy-then-cannot-recover.
    let recovery = tunnel_recovery_plan(systemd_unit, recreate).unwrap_or_else(|e| {
        eprintln!("{e}");
        std::process::exit(2);
    });
    let probe_code = |url: &str| -> String {
        std::process::Command::new("curl")
            .args([
                "-s",
                "-o",
                "/dev/null",
                "-w",
                "%{http_code}",
                "--max-time",
                "15",
                url,
            ])
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
            .unwrap_or_default()
    };

    let code = probe_code(probe_url);
    if tunnel_probe_healthy(&code) {
        println!(
            "tunnel-guard: '{name}' healthy (probe {probe_url} -> {})",
            code.trim()
        );
        return;
    }
    println!(
        "tunnel-guard: '{name}' UNHEALTHY (probe {probe_url} -> '{}') — client registered but not forwarding",
        code.trim()
    );
    if !apply {
        match recovery {
            TunnelRecovery::SystemdRestart(unit) => {
                println!("  (dry-run — re-run with --apply to `systemctl --user restart {unit}`)")
            }
            TunnelRecovery::ShedAndRecreate => {
                println!("  (dry-run — re-run with --apply to shed the stale client + recreate)")
            }
        }
        return;
    }

    match recovery {
        // systemd-managed: one restart. systemd stops the half-dead client and starts a fresh one, re-reading
        // the unit's EnvironmentFile — no pid-kill (which systemd would just respawn) and no detached recreate
        // (which would race that respawn into two clients).
        TunnelRecovery::SystemdRestart(unit) => {
            println!("  restarting systemd unit: systemctl --user restart {unit}");
            let restarted = std::process::Command::new("systemctl")
                .args(["--user", "restart", unit])
                .status()
                .map(|s| s.success())
                .unwrap_or(false);
            if !restarted {
                eprintln!(
                    "  tunnel-guard: `systemctl --user restart {unit}` failed — raising for attention"
                );
                std::process::exit(1);
            }
        }
        // Standalone: shed the stale client by its EXACT pid (never `pkill -f tunnel` — that self-matches this
        // process and would also hit sibling tunnel daemons), then recreate detached. A missing pid just means
        // no live client to shed.
        TunnelRecovery::ShedAndRecreate => {
            match std::process::Command::new(tunnel_bin)
                .args(["list", "--json"])
                .output()
            {
                Ok(o) => {
                    if let Some(pid) =
                        tunnel_pid_for_name(&String::from_utf8_lossy(&o.stdout), name)
                    {
                        println!("  shedding stale client pid {pid}");
                        let _ = std::process::Command::new("kill")
                            .arg(pid.to_string())
                            .status();
                        std::thread::sleep(std::time::Duration::from_secs(2));
                    } else {
                        println!("  no live client registered for '{name}' — recreating fresh");
                    }
                }
                Err(e) => {
                    eprintln!(
                        "  WARN: could not list tunnels to shed the stale client ({e}); recreating anyway"
                    )
                }
            }
            // Recreate detached so the client outlives this guard process (it re-parents to init).
            println!("  recreating: {recreate}");
            let spawn = std::process::Command::new("bash")
                .arg("-lc")
                .arg(format!(
                    "setsid nohup {recreate} >/dev/null 2>&1 </dev/null &"
                ))
                .status();
            if let Err(e) = spawn {
                eprintln!("  tunnel-guard: recreate failed to spawn: {e}");
                std::process::exit(1);
            }
        }
    }
    std::thread::sleep(std::time::Duration::from_secs(6));

    let code2 = probe_code(probe_url);
    if tunnel_probe_healthy(&code2) {
        println!(
            "tunnel-guard: '{name}' RECOVERED (reprobe -> {})",
            code2.trim()
        );
    } else {
        eprintln!(
            "tunnel-guard: '{name}' STILL unhealthy after recovery (reprobe -> '{}') — a human may need to \
             re-auth (mwinit) or check the tunnel service; raising for attention",
            code2.trim()
        );
        std::process::exit(1);
    }
}

/// task_1037: whether an agent's charter is DEFERRED (`metadata.charter_projection == "deferred"`) — a
/// deliberately-not-yet-active vertical (v-cas-http / v-bach during the migration) a mass reconstitution must
/// not bring up. Absent → not deferred. Pure — unit-tested.
fn agent_charter_deferred(md: Option<&serde_json::Value>) -> bool {
    md.and_then(|m| m.get("charter_projection"))
        .and_then(serde_json::Value::as_str)
        == Some("deferred")
}

/// task_1037: the reason `spin-up-all` must HOLD a down board-native agent OUT of a mass reconstitution (so it
/// is reported `HELD <reason>`, not launched), or `None` if it is launchable. This closes the launchability gap
/// the green→dev-dsk migration hit: [`native_agent_ids`] filters only on `metadata.native`, so without this a
/// STAGED / off-host / launch-gated / charter-deferred agent is a default candidate and gets swept into a wave
/// (the staged `board-core-helper` was mistakenly launched mid-cutover, task_1037). Reuses the SAME markers the
/// watchdog + up-board reconcile already honor ([`agent_is_staged`], [`agent_host_matches`]) so the batch can
/// never disagree with them; the operator escape hatch is `--include-held` or the deliberate per-id `fleet
/// spin-up <id> --apply`. Order is report-friendliest-first (staged is the common reserve case). Pure —
/// unit-tested.
fn spin_up_hold_reason(md: Option<&serde_json::Value>, this_host: &str) -> Option<&'static str> {
    if agent_is_staged(md) {
        Some("staged (reserve helper — launch only on board-pm request)")
    } else if !agent_host_matches(md, this_host) {
        Some("off-host (metadata.host pins it to another box)")
    } else if agent_is_launch_gated(md) {
        Some("launch-gated (metadata.launch_gated — gated on wiring/approval)")
    } else if agent_charter_deferred(md) {
        Some("charter deferred (metadata.charter_projection=deferred)")
    } else {
        None
    }
}

/// task_1152: whether the ACTIVE watchdog should RELAUNCH a managed board-native agent whose tmux window is
/// gone. Relaunch only an agent that SHOULD be up but crashed/vanished — never one deliberately stood down and
/// never one held out of auto-launch. Gated on: no live window AND board presence is NOT `offline` (an offline
/// agent is intentionally stood down — the [`board_reconcile_plan`] stood_down case — and is left down, e.g. a
/// `spin-down --all` migration wind-down like membrain-core) AND it is a launch candidate
/// ([`spin_up_hold_reason`] is `None`: not staged / off-host / launch-gated / charter-deferred). The per-agent
/// relaunch COOLDOWN is applied separately by the watchdog's act pass (mirroring revive-stranded), so a
/// crash-looping agent is relaunched a bounded number of times, not every sweep. Pure — unit-tested.
fn relaunch_missing_candidate(
    has_window: bool,
    is_offline: bool,
    hold_reason: Option<&str>,
) -> bool {
    !has_window && !is_offline && hold_reason.is_none()
}

/// Whether the watchdog should manage an agent on this host. A STAGED agent ([`agent_is_staged`]) is never
/// managed — it is a reserve helper that is not meant to be running, so re-arming or spawning an observer
/// against it would be a spurious wake of an intentionally-down agent. Otherwise, under `pinned_only` (a
/// secondary box), ONLY agents EXPLICITLY pinned here ([`agent_host_is_explicit`]) — so it never
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

/// A declared observer cadence: a host that runs `fleet watchdog --observe`, with whether it runs under
/// `--pinned-only`. A pinned-only cadence (a secondary box) observes ONLY agents EXPLICITLY pinned to it; a
/// non-pinned cadence (the primary box) observes unpinned run-anywhere agents plus agents pinned to it. The
/// observe-coverage audit reasons over the SET of these to decide whether any cadence covers each agent.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ObserveCadence {
    host: String,
    pinned_only: bool,
}

/// The observer-coverage verdict for one agent against a declared set of [`ObserveCadence`]s. `Blind` is the
/// standing blind spot task_711 guards: an agent pinned to a host that runs no observer cadence (or, when every
/// declared cadence is pinned-only, an unpinned agent served by none) is observed by nobody. The two `Covered`
/// variants differ only in how sure the AUDIT can be: this process reads ONLY the local host's `~/.claude`, so
/// transcript presence is VERIFIED when a local cadence manages the agent (`CoveredLocalTranscript`) and merely
/// ASSUMED when the only managing cadence is on another host (`CoveredAssumed`). `ManagedNoLocalTranscript` is a
/// soft gap: a local cadence claims scope but no session JSONL exists here to observe.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum ObserveCoverage {
    CoveredLocalTranscript,
    CoveredAssumed,
    ManagedNoLocalTranscript,
    Blind,
}

/// Classify an agent's observer coverage against the declared cadence set. `local_transcript_present` is
/// `Some(bool)` only when a cadence on `this_host` manages the agent (the one host whose transcripts this
/// process can read), else `None`. Reuses [`watchdog_manages_agent`] so the audit's notion of "a cadence
/// manages this agent" is byte-identical to the watchdog's own gate — the audit can never disagree with the
/// thing it audits. Pure — unit-tested.
fn classify_observe_coverage(
    md: Option<&serde_json::Value>,
    cadences: &[ObserveCadence],
    this_host: &str,
    local_transcript_present: Option<bool>,
) -> ObserveCoverage {
    let managed_anywhere = cadences
        .iter()
        .any(|c| watchdog_manages_agent(md, &c.host, c.pinned_only));
    if !managed_anywhere {
        return ObserveCoverage::Blind;
    }
    let managed_locally = cadences
        .iter()
        .any(|c| c.host == this_host && watchdog_manages_agent(md, &c.host, c.pinned_only));
    if managed_locally {
        match local_transcript_present {
            Some(true) => ObserveCoverage::CoveredLocalTranscript,
            Some(false) => ObserveCoverage::ManagedNoLocalTranscript,
            None => ObserveCoverage::CoveredAssumed,
        }
    } else {
        ObserveCoverage::CoveredAssumed
    }
}

/// Render an agent's `metadata.host` pin for a human-readable audit line: `unpinned` (unset / null / empty),
/// a single host, or `[h1, h2]` for a list. Pure — unit-tested.
fn host_pin_display(md: Option<&serde_json::Value>) -> String {
    match md.and_then(|m| m.get("host")) {
        Some(serde_json::Value::String(s)) if !s.trim().is_empty() => s.clone(),
        Some(serde_json::Value::Array(items)) if !items.is_empty() => {
            let hs: Vec<&str> = items.iter().filter_map(serde_json::Value::as_str).collect();
            if hs.is_empty() {
                "unpinned".to_string()
            } else {
                format!("[{}]", hs.join(", "))
            }
        }
        _ => "unpinned".to_string(),
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
/// The outcome of resolving a `fleet send --to <name>` recipient against the known agent roster (task_1251):
/// the prefix-collision guard for the CLI half. "v-fleet-tooling" is a strict prefix of
/// "v-fleet-tooling-helper", so a caller that types a non-exact collision-shaped name must not silently
/// deliver to an unintended inbox. Pure — unit-tested.
#[derive(Debug, PartialEq, Eq)]
enum RecipientResolution {
    /// `to` is itself a known agent id — deliver as addressed. An exact id wins even when it is also a prefix
    /// of a longer id, so a real recipient is never refused.
    Exact,
    /// `to` is NOT a known id but is a strict prefix of one or more known ids — refuse and list the
    /// candidates rather than deliver to a literal-but-unintended inbox.
    Ambiguous(Vec<String>),
    /// `to` matches no known id by exact or prefix — a first-contact or an unknown recipient; deliver as
    /// addressed (the file-hub creates the inbox), preserving the pre-guard behavior for a genuinely new name.
    Unknown,
}

/// Resolve a recipient name against the known agent ids (task_1251). An exact id delivers; a name that is a
/// strict prefix of known ids but is not itself one is ambiguous (refuse and disambiguate); anything else is a
/// first-contact/unknown name that delivers unchanged. Pure.
fn resolve_recipient(to: &str, known: &[String]) -> RecipientResolution {
    if known.iter().any(|k| k == to) {
        return RecipientResolution::Exact;
    }
    let mut candidates: Vec<String> = known
        .iter()
        .filter(|k| k.len() > to.len() && k.starts_with(to))
        .cloned()
        .collect();
    if candidates.is_empty() {
        RecipientResolution::Unknown
    } else {
        candidates.sort();
        RecipientResolution::Ambiguous(candidates)
    }
}

/// The known agent ids from the board roster, or `None` if the board is unreachable (so the caller can
/// fail-open). Best-effort — a transient board blip must never block a `fleet send`.
fn board_agent_ids() -> Option<Vec<String>> {
    let board = board::Board::connect().ok()?;
    let agents = board.list_agents().ok()?;
    Some(
        agents
            .iter()
            .filter_map(|a| {
                a.get("id")
                    .and_then(serde_json::Value::as_str)
                    .map(String::from)
            })
            .collect(),
    )
}

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
        .or_else(|| config::get().agent.clone().filter(|s| !s.trim().is_empty()))
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
    // task_1251: force an exact-id match / disambiguate a prefix collision at the CLI boundary. "v-fleet-tooling"
    // is a strict prefix of "v-fleet-tooling-helper", so a non-exact `--to` that collides with known agent ids
    // is refused with the candidates rather than delivered to an unintended inbox. Best-effort and fail-open:
    // an unreachable roster, an exact id, or a genuinely new first-contact name all proceed — only an ambiguous
    // prefix is refused. This lives in `send` (the CLI entry), not `deliver`, so the mechanism stays hermetic.
    if let Some(known) = board_agent_ids()
        && let RecipientResolution::Ambiguous(candidates) = resolve_recipient(to, &known)
    {
        eprintln!(
            "fleet send: REFUSING to deliver to `{to}` — not an exact agent id, but a strict prefix of {} \
             known agents: {}. Pass the exact id.",
            candidates.len(),
            candidates.join(", ")
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

/// Whether a tmux window named `name` is present in a `tmux list-windows -F '#W'` output (one window name per
/// line). The idempotency check shared by both launch paths: an agent must never get a SECOND window while a
/// live one exists, or two unattended instances race the same inbox and double-post (the ticket-ingest
/// double-spawn). Pure — unit-tested.
fn window_exists(window_list_output: &str, name: &str) -> bool {
    window_list_output.lines().any(|w| w == name)
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
    // Idempotency guard (duplicate-instance prevention): never open a SECOND window for an agent that already
    // has a live one. Mirrors launch_board_agent's guard so BOTH fleet launch paths are idempotent regardless
    // of caller — a concurrent reconstitution/relaunch cannot double-spawn one identity.
    if let Ok(out) = std::process::Command::new("tmux")
        .args(["list-windows", "-t", session, "-F", "#W"])
        .output()
        && window_exists(&String::from_utf8_lossy(&out.stdout), name)
    {
        eprintln!(
            "  = '{name}': a tmux window already exists in '{session}' — skipping launch (already running)"
        );
        return false;
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
    for v in [
        &mut plan.to_launch,
        &mut plan.already_running,
        &mut plan.stood_down,
    ] {
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
/// box brings up exactly its own declared, host-pinned agents — the secondary box's reconcile never touches host-a
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

/// A violation of the uncategorized intake project's two invariants (task_1214), carried by a single task
/// still sitting in the intake inbox. The inbox MUST stay transient: the dwell invariant is that a task sits
/// under the SLA before being routed out, and the state invariant is that a task is NEVER `in_progress` or
/// `blocked` while still in it. The dwell+state watchdog classifies every task in the project against both
/// and raises ONE cooldown-fenced alert per offender — the report/alert safety net (task_1217) that confirms
/// a low breach rate before a sibling hard-reject flip is safe to land. A task can trip both invariants at
/// once, so one verdict carries both facts and the sweep emits one alert.
#[derive(Debug, PartialEq, Eq, Clone, Default)]
struct IntakeVerdict {
    /// Invariant 2: the offending status when the task is `in_progress` or `blocked` while still in the
    /// intake inbox — it should have been routed out before work began. Fires independent of age.
    bad_state: Option<String>,
    /// Invariant 1: the task's age in seconds when it has dwelt in the inbox PAST the SLA without routing.
    dwell_over_secs: Option<u64>,
}

impl IntakeVerdict {
    /// Whether this task breaches either invariant and so warrants an alert. A clean verdict (routed in time,
    /// still `todo`) breaches neither and is not alerted.
    fn is_violation(&self) -> bool {
        self.bad_state.is_some() || self.dwell_over_secs.is_some()
    }
}

/// Classify a task that is CURRENTLY in the uncategorized intake project against the inbox invariants
/// (task_1217). `status` is the board status, `age_secs` how long it has sat in the inbox, and `sla_secs` the
/// dwell bound (the operator's "~2min dwell", i.e. 120s, passed by the sweep). A TERMINAL task
/// (`done`/`cancelled`/`archived`) is never a violation — it is no longer awaiting routing, so neither
/// invariant applies even if it is old. Pure — unit-tested; the board list + cooldown-fenced alert sweep
/// wraps it.
fn classify_intake_task(status: &str, age_secs: u64, sla_secs: u64) -> IntakeVerdict {
    if matches!(status, "done" | "cancelled" | "canceled" | "archived") {
        return IntakeVerdict::default();
    }
    IntakeVerdict {
        bad_state: matches!(status, "in_progress" | "blocked").then(|| status.to_string()),
        dwell_over_secs: (age_secs > sla_secs).then_some(age_secs),
    }
}

/// Build the report lines for an intake sweep (task_1217): one line per task that violates an inbox
/// invariant, naming the task and WHY (its bad state, a past-SLA dwell, or both on one line). `entries` is
/// `(task_ref, status, age_secs)` for every task currently in the intake project; a clean task yields no
/// line. Pure — unit-tested; the board fetch + age computation wraps it.
/// The one-line reason a task in the intake project violates an inbox invariant (task_1217), or `None` when
/// it is clean. Shared by the report ([`intake_report`]) and the per-offender alert body
/// ([`intake_alert_comment`]) so both name a violation identically. Pure.
fn intake_violation_reason(status: &str, age_secs: u64, sla_secs: u64) -> Option<String> {
    let v = classify_intake_task(status, age_secs, sla_secs);
    if !v.is_violation() {
        return None;
    }
    let mut why = Vec::new();
    if let Some(s) = &v.bad_state {
        why.push(format!("state={s} (never in-inbox)"));
    }
    if let Some(a) = v.dwell_over_secs {
        why.push(format!("dwell {a}s > {sla_secs}s SLA"));
    }
    Some(why.join(", "))
}

fn intake_report(entries: &[(String, String, u64)], sla_secs: u64) -> Vec<String> {
    entries
        .iter()
        .filter_map(|(task_ref, status, age)| {
            intake_violation_reason(status, *age, sla_secs).map(|why| format!("{task_ref}: {why}"))
        })
        .collect()
}

/// The per-offender alert comment body for a violating intake task (task_1217 shape (a), board-pm
/// comment_6197): it names the violation and asks board-triage to route the task out of the uncategorized
/// inbox. The signal sits on the task where its owner and board-triage both see it, with no task mutation.
/// Pure — unit-tested.
fn intake_alert_comment(reason: &str) -> String {
    format!(
        "@board-triage this task violates an uncategorized-intake invariant: {reason}. It should be routed \
         out of the intake project (assign it an owner or move it to a real project). Flagged by fleet \
         intake-watch for task_1217."
    )
}

/// The per-offender intake-alert stamp path: `<root>/watchdog/intake-alert/<project>-<ref>.stamp` (contents =
/// the unix secs of the last alert). Keyed by project and task ref so one offender is alerted at most once per
/// cooldown window.
fn intake_alert_stamp_path(fleet: &Fleet, project: i64, task_ref: &str) -> PathBuf {
    fleet
        .root
        .join("watchdog")
        .join("intake-alert")
        .join(format!("{project}-{task_ref}.stamp"))
}

/// Read an offender's last intake-alert unix time; `None` on an absent/unparseable stamp (never alerted).
fn read_intake_alert_stamp(fleet: &Fleet, project: i64, task_ref: &str) -> Option<u64> {
    std::fs::read_to_string(intake_alert_stamp_path(fleet, project, task_ref))
        .ok()?
        .trim()
        .parse()
        .ok()
}

/// Record that an offender was just alerted at `now` (best-effort — a write failure only risks a re-alert on
/// the next sweep, the safe direction for a safety net).
fn write_intake_alert_stamp(fleet: &Fleet, project: i64, task_ref: &str, now: u64) {
    let p = intake_alert_stamp_path(fleet, project, task_ref);
    if let Some(dir) = p.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(p, now.to_string());
}

/// Sweep the uncategorized intake project and REPORT every task that violates an inbox invariant (task_1217),
/// and with `alert` also post the active, cooldown-fenced per-offender escalation (shape (a), board-pm
/// comment_6197). Report-only by default: it prints the offenders (via the pure [`intake_report`]) and exits
/// non-zero when any violate, so a supervisor can gate on it. With `--alert` it additionally posts a
/// per-offender comment tagging board-triage on each violating task, fenced per offender by
/// `alert_cooldown_secs` so a fast sweep never re-comments. Age is the task's `created_at` to now; a task with
/// no parseable `created_at` is swept on state only (age 0, so it can still trip the state invariant but never
/// a phantom dwell). The one fenced digest to board-triage plus concierge (shape (c)) is the follow-on.
fn intake_watch(fleet: &Fleet, project: i64, sla_secs: u64, alert: bool, alert_cooldown_secs: u64) {
    let board = board::Board::connect().unwrap_or_else(|e| {
        eprintln!("intake-watch: {e}");
        std::process::exit(1);
    });
    let tasks = board.list_tasks_by_project(project).unwrap_or_else(|e| {
        eprintln!("intake-watch: {e}");
        std::process::exit(1);
    });
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // (id, ref, status, age) per task: the report needs ref/status/age, and the alert also needs the numeric
    // id to comment on the task.
    let rich: Vec<(i64, String, String, u64)> = tasks
        .iter()
        .map(|t| {
            let id = t.get("id").and_then(serde_json::Value::as_i64).unwrap_or(0);
            let task_ref = t
                .get("ref")
                .and_then(serde_json::Value::as_str)
                .map(String::from)
                .unwrap_or_else(|| format!("task_{id}"));
            let status = t
                .get("status")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("")
                .to_string();
            let age = dream::provenance_unix_secs(t.get("created_at"))
                .map(|c| (now as i64 - c).max(0) as u64)
                .unwrap_or(0);
            (id, task_ref, status, age)
        })
        .collect();
    let entries: Vec<(String, String, u64)> = rich
        .iter()
        .map(|(_, r, s, a)| (r.clone(), s.clone(), *a))
        .collect();
    let lines = intake_report(&entries, sla_secs);

    if alert {
        let mut alerted = 0usize;
        for (id, task_ref, status, age) in &rich {
            let Some(reason) = intake_violation_reason(status, *age, sla_secs) else {
                continue;
            };
            // Fence: at most one alert per offender per cooldown window, so a ~30s sweep does not re-comment.
            if bounce_on_cooldown(
                read_intake_alert_stamp(fleet, project, task_ref),
                now,
                alert_cooldown_secs,
            ) {
                continue;
            }
            match board.comment_task(*id, "v-fleet-tooling", &intake_alert_comment(&reason)) {
                Ok(()) => {
                    write_intake_alert_stamp(fleet, project, task_ref, now);
                    alerted += 1;
                }
                Err(e) => eprintln!("intake-watch: alert comment on {task_ref} failed: {e}"),
            }
        }
        eprintln!(
            "intake-watch: alerted {alerted} offender(s) (per-offender cooldown {alert_cooldown_secs}s); the \
             (c) digest to board-triage plus concierge is the follow-on"
        );
    }

    if lines.is_empty() {
        println!(
            "intake-watch: project {project} clean -- {} task(s), none violating (SLA {sla_secs}s)",
            entries.len()
        );
    } else {
        eprintln!(
            "intake-watch: project {project} has {} invariant violation(s) (SLA {sla_secs}s):",
            lines.len()
        );
        for l in &lines {
            eprintln!("  {l}");
        }
        std::process::exit(1);
    }
}

/// One board-hygiene retention sweep in the daily tick's table (task_1249): a POST to a project-scoped board
/// endpoint with a day threshold. The table drives the whole family from one tick (archive-done +
/// age-out-todos), so a future sweep is one more row, not another crontab line.
struct HygieneSweep {
    name: &'static str,
    path: String,
    older_than_days: u64,
}

/// Build the board-hygiene sweep table for a project (task_1249): the confirmed `archive-done` sweep always,
/// and the `age-out-todos` sweep (board-side scoped to status=todo) unless skipped. Both target `project`; the
/// daily tick POSTs each. Pure — unit-tested.
fn board_hygiene_sweeps(
    project: i64,
    archive_done_days: u64,
    age_out_days: u64,
    skip_age_out: bool,
) -> Vec<HygieneSweep> {
    let mut sweeps = vec![HygieneSweep {
        name: "archive-done",
        path: format!("/projects/{project}/archive-done"),
        older_than_days: archive_done_days,
    }];
    if !skip_age_out {
        sweeps.push(HygieneSweep {
            name: "age-out-todos",
            path: format!("/projects/{project}/age-out-todos"),
            older_than_days: age_out_days,
        });
    }
    sweeps
}

/// The JSON body for a retention-sweep POST (task_1249): the day threshold plus the authenticating principal.
/// Pure.
fn hygiene_sweep_body(older_than_days: u64, principal: &str) -> serde_json::Value {
    serde_json::json!({ "older_than_days": older_than_days, "principal": principal })
}

/// Run the daily board-hygiene retention tick (task_1249): drive the project-scoped sweeps (`archive-done`,
/// `age-out-todos`) over the board REST API from one place so the project stays tight without a manual
/// invocation. DRY-RUN by default (prints the plan, touches nothing); pass `--execute` to POST. Each sweep is
/// restorable (restore_task) and auditable (task.archived reason=retention) on the board side, so a bad run is
/// recoverable. A sweep failure logs and the tick continues to the next, so one bad endpoint never skips the
/// rest; the process still exits non-zero if any sweep failed, so a supervisor sees it.
fn board_hygiene(
    project: i64,
    archive_done_days: u64,
    age_out_days: u64,
    skip_age_out: bool,
    execute: bool,
) {
    let sweeps = board_hygiene_sweeps(project, archive_done_days, age_out_days, skip_age_out);
    if !execute {
        println!("board-hygiene: DRY-RUN (pass --execute to POST). Plan for project {project}:");
        for s in &sweeps {
            println!(
                "  {} -> POST {} (older_than_days={})",
                s.name, s.path, s.older_than_days
            );
        }
        return;
    }
    let board = board::Board::connect().unwrap_or_else(|e| {
        eprintln!("board-hygiene: {e}");
        std::process::exit(1);
    });
    let mut failed = 0usize;
    for s in &sweeps {
        let body = hygiene_sweep_body(s.older_than_days, "v-fleet-tooling");
        match board.post_json(&s.path, &body) {
            Ok(resp) => {
                let archived = resp
                    .get("archived")
                    .and_then(serde_json::Value::as_i64)
                    .unwrap_or(-1);
                println!(
                    "board-hygiene: {} archived {archived} task(s) in project {project} (older_than_days={})",
                    s.name, s.older_than_days
                );
            }
            Err(e) => {
                eprintln!("board-hygiene: {} FAILED: {e}", s.name);
                failed += 1;
            }
        }
    }
    if failed > 0 {
        std::process::exit(1);
    }
}

/// The ordered list of mandate SET paths an agent inherits (task_906 / task_870): the universal `core`, then
/// its `roles/<role>`, then `targets/<repo>` for each declared repo (sorted by repo name), then
/// `capabilities/<capability>` for each declared capability (sorted). The compose step fetches each path's
/// operator-approved document version and concatenates them; this is the pure SELECTION half. Pure.
fn mandate_set_paths(role: Option<&str>, repos: &[String], capabilities: &[String]) -> Vec<String> {
    let mut paths = vec!["core".to_string()];
    if let Some(r) = role.map(str::trim).filter(|r| !r.is_empty()) {
        paths.push(format!("roles/{r}"));
    }
    let mut repos: Vec<&String> = repos.iter().filter(|r| !r.trim().is_empty()).collect();
    repos.sort();
    repos.dedup();
    for r in repos {
        paths.push(format!("targets/{r}"));
    }
    let mut caps: Vec<&String> = capabilities
        .iter()
        .filter(|c| !c.trim().is_empty())
        .collect();
    caps.sort();
    caps.dedup();
    for c in caps {
        paths.push(format!("capabilities/{c}"));
    }
    paths
}

/// Extract the ordered mandate set paths from an agent's board metadata (task_906): `role`, `repos[].repo`,
/// and `capabilities[]`. A missing field contributes nothing (the agent just inherits fewer sets). Pure.
fn mandate_set_paths_from_metadata(md: &serde_json::Value) -> Vec<String> {
    let role = md.get("role").and_then(serde_json::Value::as_str);
    let repos: Vec<String> = md
        .get("repos")
        .and_then(serde_json::Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|r| {
                    r.get("repo")
                        .and_then(serde_json::Value::as_str)
                        .map(String::from)
                })
                .collect()
        })
        .unwrap_or_default();
    let capabilities: Vec<String> = md
        .get("capabilities")
        .and_then(serde_json::Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|c| c.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    mandate_set_paths(role, &repos, &capabilities)
}

/// The result of composing an agent's mandate sets into one boot-local artifact (task_906): the flattened
/// text plus the paths that were skipped (no operator-approved version yet), which the caller logs.
#[derive(Debug, PartialEq, Eq)]
struct ComposedMandate {
    artifact: String,
    skipped: Vec<String>,
}

/// Concatenate the fetched mandate sets into one flattened artifact under stable per-set headers (task_906).
/// `sets` is the ordered `(path, approved_content)` list; a `None` content means that set has no approved
/// version. `core` is HARD-REQUIRED: no approved core is an `Err` (the caller holds + alarms rather than boot
/// an agent with no universal contract). Any other set with no approved version is FAIL-SOFT skipped and
/// recorded in `skipped`. Pure — unit-tested; the board fetch + atomic hash-gated write wrap it.
fn compose_mandate_artifact(sets: &[(String, Option<String>)]) -> Result<ComposedMandate, String> {
    if !sets.iter().any(|(p, c)| p == "core" && c.is_some()) {
        return Err(
            "compose-mandates: no operator-approved `core` set - refusing to compose (hold + alarm)"
                .to_string(),
        );
    }
    let mut artifact = String::new();
    let mut skipped = Vec::new();
    for (path, content) in sets {
        match content {
            Some(body) => {
                artifact.push_str(&format!("## mandate-set: {path}\n\n"));
                artifact.push_str(body.trim_end());
                artifact.push_str("\n\n");
            }
            None => skipped.push(path.clone()),
        }
    }
    Ok(ComposedMandate { artifact, skipped })
}

/// Fetch each mandate set's operator-approved content from the board (task_906): for each ordered path, read
/// its wiki row and gate on `approved_version_id` - null means no approved version (fail-soft, `None`), else
/// fetch the approved body. A path with no wiki row at all is also `None`. Returns the ordered
/// `(path, approved_content)` list [`compose_mandate_artifact`] consumes. The gate-on-id means a missing set
/// never costs a 404 round-trip. A hard board error on a set is surfaced (not silently skipped).
fn fetch_mandate_sets(
    board: &board::Board,
    paths: &[String],
) -> Result<Vec<(String, Option<String>)>, String> {
    let mut out = Vec::with_capacity(paths.len());
    for path in paths {
        let content = match board.wiki_row_for_path(path)? {
            Some(row) => match row
                .get("approved_version_id")
                .and_then(serde_json::Value::as_i64)
            {
                Some(id_present) => {
                    let _ = id_present; // non-null => an approved version exists; fetch the body
                    let doc_id = row
                        .get("id")
                        .and_then(serde_json::Value::as_i64)
                        .ok_or_else(|| {
                            format!("compose-mandates: wiki row for {path} has no id")
                        })?;
                    Some(board.fetch_approved_content(doc_id)?)
                }
                None => None, // filed but no approved version yet -> fail-soft skip
            },
            None => None, // not filed -> fail-soft skip
        };
        out.push((path.clone(), content));
    }
    Ok(out)
}

/// Whether a freshly-composed mandate artifact DIFFERS from what is already on disk (task_906): the write-gate
/// that keeps the boot file churn-free - only an actual content change triggers the atomic rewrite. Pure.
fn artifact_changed(new: &str, existing: Option<&str>) -> bool {
    existing != Some(new)
}

/// Compose an agent's role-inherited mandate sets into one boot-local artifact (task_906): read its registry
/// metadata, select the ordered set paths, fetch each approved version, concatenate under stable headers, and
/// (with `write`) atomically rewrite the `out` file only when the content changed. DRY-RUN by default (prints
/// the artifact + the skipped sets, writes nothing). `core` with no approved version is a hard hold+alarm.
fn compose_mandates(fleet: &Fleet, agent: &str, out: &str, write: bool) {
    let board = board::Board::connect().unwrap_or_else(|e| {
        eprintln!("compose-mandates: {e}");
        std::process::exit(1);
    });
    let _ = fleet;
    let rec = board.get_agent(agent).unwrap_or_else(|e| {
        eprintln!("compose-mandates: cannot read agent {agent}: {e}");
        std::process::exit(1);
    });
    let md = rec
        .get("metadata")
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    let paths = mandate_set_paths_from_metadata(&md);
    let sets = fetch_mandate_sets(&board, &paths).unwrap_or_else(|e| {
        eprintln!("compose-mandates: {e}");
        std::process::exit(1);
    });
    let composed = compose_mandate_artifact(&sets).unwrap_or_else(|e| {
        // core hard-required: hold + alarm rather than compose a mandate with no universal contract.
        eprintln!("compose-mandates: {e}");
        std::process::exit(1);
    });
    if !composed.skipped.is_empty() {
        eprintln!(
            "compose-mandates: {agent}: {} set(s) skipped (no approved version): {}",
            composed.skipped.len(),
            composed.skipped.join(", ")
        );
    }
    if !write {
        println!(
            "compose-mandates: DRY-RUN for {agent} ({} set(s), {} skipped). Pass --write to materialize to {out}.\n---\n{}",
            paths.len(),
            composed.skipped.len(),
            composed.artifact
        );
        return;
    }
    let existing = std::fs::read_to_string(out).ok();
    if !artifact_changed(&composed.artifact, existing.as_deref()) {
        println!("compose-mandates: {agent}: {out} already current (no change)");
        return;
    }
    let out_path = std::path::Path::new(out);
    if let Some(dir) = out_path.parent()
        && let Err(e) = std::fs::create_dir_all(dir)
    {
        eprintln!("compose-mandates: cannot create {}: {e}", dir.display());
        std::process::exit(1);
    }
    let tmp = format!("{out}.tmp.{}", std::process::id());
    if let Err(e) = std::fs::write(&tmp, &composed.artifact) {
        eprintln!("compose-mandates: write {tmp} failed: {e}");
        std::process::exit(1);
    }
    if let Err(e) = std::fs::rename(&tmp, out) {
        eprintln!("compose-mandates: rename into {out} failed: {e}");
        let _ = std::fs::remove_file(&tmp);
        std::process::exit(1);
    }
    println!(
        "compose-mandates: {agent}: wrote {out} ({} bytes, {} set(s), {} skipped)",
        composed.artifact.len(),
        paths.len(),
        composed.skipped.len()
    );
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
    /// Orderly FLEET-WIDE spin-down (migration wind-down): run the per-agent `spin-down` decision across EVERY
    /// board-native agent except a stay-up list, so a whole fleet reaches a clean, RESUMABLE parked state in one
    /// gated command. Each agent is held to the SAME task_786 guard as single spin-down — an agent still holding
    /// open actionable assignments, or mid-turn, is REFUSED (not stood down) unless `--force`, so the batch
    /// self-regulates: cleanly-parked agents stand down, anyone still mid-work is left running. Reports the
    /// READY/REFUSED roster by default (the live who's-parked checklist); `--apply` stands down the READY ones.
    /// NEVER pass `--force` for a routine wind-down (it would kill mid-work agents) — it exists only for an
    /// operator-directed hard stop.
    SpinDownAll {
        /// Comma-separated agent ids to KEEP UP (cutover roles, the operator interface, self). Always include
        /// this binary's own agent — the batch must never stand down the agent orchestrating it.
        #[arg(long)]
        except: Option<String>,
        /// Stand down the READY (cleanly-parked) agents (default: just report the READY/REFUSED roster).
        #[arg(long)]
        apply: bool,
        /// Hard stop: stand down even agents holding open work or mid-turn. Operator-directed override only.
        #[arg(long)]
        force: bool,
    },
    /// Orderly FLEET-WIDE spin-UP (migration reconstitution): the reverse of `spin-down-all`. Brings every
    /// board-native agent that is currently DOWN (offline / no live window) back up, except a stay-as-is list,
    /// so a parked fleet is reconstituted in gated WAVES rather than a thundering herd. `--limit N` caps each
    /// invocation to the first N (sorted) candidates, so a mass revive is run as several bounded `--apply`
    /// passes with a pause between; omit it to bring up everyone down. An agent already running (a live window)
    /// is skipped, never relaunched. Reports the candidate roster by default; `--apply` launches this wave.
    /// Each candidate is spun up as a SUBPROCESS (`fleet spin-up <id> --apply`), so a single agent's launch
    /// failure is contained in its child and never aborts the wave.
    SpinUpAll {
        /// Comma-separated agent ids to LEAVE AS-IS (already-up roles, the operator interface, self) — not
        /// brought up even if down.
        #[arg(long)]
        except: Option<String>,
        /// Cap this wave to the first N (sorted) down candidates, for a bounded reconstitution wave. Omit to
        /// bring up every down agent in one pass.
        #[arg(long)]
        limit: Option<usize>,
        /// Also launch agents normally HELD by the launchability filter (staged / off-host / launch-gated /
        /// charter-deferred). Default OFF: those are reported `HELD <reason>` and never swept into a wave
        /// (task_1037). An operator-directed launch of a held agent should normally use `fleet spin-up <id>
        /// --apply` per id; this flag is the batch escape hatch.
        #[arg(long)]
        include_held: bool,
        /// Launch this wave's candidates (default: just report the candidate roster).
        #[arg(long)]
        apply: bool,
    },
    /// Bounce ONE board-native agent's session so its MCP client RECONNECTS and refetches tools/list — the fix
    /// for the stale-cached-tools/list trap (task_752): a long-lived session caches tools/list at connect, so a
    /// tool shipped to the board MCP server AFTER it connected (e.g. `pose_question` after the structured-
    /// questions rollout) stays invisible until the session reconnects, and a redeploy often does NOT drop the
    /// connection (a tunnel masks it), so the reconnect must be driven from OUTSIDE. This kills the agent's
    /// tmux window and relaunches it via the normal spin-up path, so the fresh session reconnects to the current
    /// server process and refetches tools/list; the agent resumes from the board (board-native agents are
    /// resumable — charter + task state live on the board, not in-session). Reports the plan by default;
    /// `--apply` performs it. Refuses a non-board-native agent, a windowless (not-running) agent, a busy pane (a
    /// turn in flight), or an agent bounced within the cooldown window — `--force` overrides the busy + cooldown
    /// refusals.
    BounceSession {
        /// The board agent id to bounce.
        agent: String,
        /// Perform the kill + relaunch (default: just report the plan).
        #[arg(long)]
        apply: bool,
        /// Bounce even if the pane shows an in-flight turn or the agent is within the cooldown window.
        #[arg(long)]
        force: bool,
    },
    /// Watchdog-driven STALE-SESSION sweep (task_752): bounce every running board-native agent whose session
    /// was launched under an OLDER fleet binary than the current one, so each refetches its MCP tools/list and
    /// picks up tools shipped since it connected (e.g. `pose_question`). The staleness signal is the per-agent
    /// launch-rev stamp vs this binary's `FLEET_BUILD_REV`: after a redeploy the sweep runs under the new
    /// binary, so a session launched under the old one reads as stale. This is the fleet-wide form of
    /// `bounce-session` — report-only by default (lists which agents would be bounced and why), `--apply`
    /// bounces each candidate through the fenced per-agent `bounce-session` path (board-native + not-busy +
    /// cooldown-fenced), so a healthy or mid-turn session is never thrashed. `--force` passes through to
    /// override the busy + cooldown fences per candidate.
    BounceStale {
        /// Bounce the stale candidates (default: just list them).
        #[arg(long)]
        apply: bool,
        /// Pass `--force` to each per-agent bounce (override its busy + cooldown fences).
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
    /// Resolve the OWNER + authoritative-definition pointer for a managed host service, so an agent
    /// asked to touch shared infra can look up "who owns it + where is the authored definition" cheaply
    /// instead of a message round-trip (task_1027). With no argument, lists every registered service;
    /// with a name/alias/port (e.g. `edge-proxy`, `:8880`), prints its owner, definition, and whether it
    /// is a `lifeline` (fleet-wide board/MCP/event-wake dependency — attended-only, never an unattended
    /// bounce). Backed by the committed `host-services.toml` registry.
    Services {
        /// A service name, alias, or port (e.g. `edge-proxy` or `:8880`); omit to list all services.
        service: Option<String>,
    },
    /// Inspect or set the FLEET-QUIESCE maintenance-mode sentinel (task_1032): the local marker that suppresses
    /// the nudge-stale daemon AND the watchdog's opt-in auto-actions (revive-stranded / hire-signal /
    /// recover-wedged) during a mass spin-down/cutover, so they do not fight an operator-directed quiesce. With
    /// no flag it REPORTS status (on / off / expired, plus age and time until the TTL self-expiry). `--on`
    /// raises it; `--off` clears it. `spin-down-all` / `spin-up-all` manage it automatically — this is the
    /// explicit operator control for a delicate manual operation that is NOT a full fleet spin-down (e.g. a
    /// board migration step), and the first-class way to diagnose "why did the nudge daemon go quiet?".
    Quiesce {
        /// Raise the sentinel (maintenance mode ON): suppress nudge-stale + the watchdog auto-actions.
        #[arg(long)]
        on: bool,
        /// Clear the sentinel (maintenance mode OFF): the daemons resume.
        #[arg(long)]
        off: bool,
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
        /// transcript lives on ANOTHER box. Use this on a secondary box that should only manage its
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
        /// Reap STALE/leaked observer windows (task_610): an `obs-<target>` tmux window still alive long after a
        /// healthy observation would have finished — a crashed/hung observer that never reached `observe-record`
        /// (so it never self-closed), leaking a live model process. OPT-IN so merely shipping this never
        /// auto-reaps currently-held windows; a window with a RECENT spawn stamp (a healthy in-flight observer)
        /// is never touched. Honors `--dry-run`: with it, report what WOULD be reaped without killing.
        #[arg(long)]
        reap_stale_observers: bool,
        /// ACT on the task_794 backlog-depth signal: for each RUNNING agent whose open backlog exceeds the depth
        /// threshold (CDZ_BACKLOG_DEPTH, default 2), POST a hire/route signal to the dedicated board channel the
        /// PM subscribes to (so a human/PM mints or routes a helper). Cooldown-fenced per agent
        /// (CDZ_HIRE_SIGNAL_COOLDOWN_SECS, default 21600 = 6h) so one breach yields one actionable ping, not a
        /// per-sweep stream. OPT-IN so merely shipping the detection never posts; without it the backlog signal
        /// stays report-only (a WARNING line).
        #[arg(long)]
        hire_signal: bool,
        /// ACT on the task_786 STRANDED signal: for each board-native agent stood down (offline) while it still
        /// holds open actionable assignment(s), AUTO-SPIN-UP (revive) it so its dispatched work gets worked,
        /// instead of only flagging it. Cooldown-fenced per agent (CDZ_REVIVE_STRANDED_COOLDOWN_SECS, default
        /// 3600) so a revive that does not immediately clear the stranded state is not retried every sweep.
        /// OPT-IN so merely shipping this never auto-revives — a deliberately stood-down agent is revived only
        /// once a host turns this on; without it the stranded signal stays report-only (a WARNING line). The
        /// prevention companion is the task_786 spin-down refuse-to-strand guard.
        #[arg(long)]
        revive_stranded: bool,
        /// ACT on the task_752 STALE-SESSION signal: after the liveness pass, run the `bounce-stale` sweep in
        /// apply mode — bounce every running board-native session launched under an OLDER binary than this one
        /// so it refetches its MCP tools/list (picks up tools shipped since it connected). Each bounce still
        /// passes the fenced per-agent `bounce-session` path (board-native + not-busy + cooldown), so a healthy
        /// or mid-turn session is never thrashed. OPT-IN and deliberately NOT baked into the installed watchdog
        /// timer — a fleet-wide bounce is an owner-triggered act, never an unfenced automatic one.
        #[arg(long)]
        bounce_stale: bool,
        /// ACT on the task_582 SAFEGUARD-WEDGE signal: for each running board-native agent whose last few
        /// assistant turns are all `stop_reason=refusal` (a model-safeguard reject loop that keeps `last_seen`
        /// advancing, so the liveness/age checks miss it), AUTO-RECOVER it — `fleet spin-down <id> --apply
        /// --force` then `fleet spin-up <id> --apply`, so the fresh session breaks the refusal loop instead of
        /// burning turns until a human notices. Cooldown-fenced per agent (CDZ_RECOVER_WEDGED_COOLDOWN_SECS,
        /// default 3600) so a recovery that does not immediately clear the wedge is not retried every sweep, and
        /// suppressed during a fleet-quiesce (a mass spin-down must not be fought). OPT-IN so merely shipping the
        /// detection never force-restarts an agent; without it the wedge stays report-only (a WARNING line).
        #[arg(long)]
        recover_wedged: bool,
        /// ACT on the task_1152 MISSING-WINDOW signal (active liveness enforcement): for each managed
        /// board-native agent that SHOULD be up but has no live tmux window — board presence NOT `offline`
        /// (an offline agent is deliberately stood down and left down) and a launch candidate (not staged /
        /// off-host / launch-gated / charter-deferred) — RELAUNCH it via `fleet spin-up <id> --apply`, instead
        /// of only the passive mtime/last_seen liveness proxy. Cooldown-fenced per agent
        /// (CDZ_RELAUNCH_MISSING_COOLDOWN_SECS, default 3600) so a crash-looping agent is relaunched a bounded
        /// number of times, not every sweep, and suppressed during a fleet-quiesce (a mass spin-down must not be
        /// fought). OPT-IN so merely shipping this never auto-relaunches; without it the missing window stays
        /// report-only (a WARNING line).
        #[arg(long)]
        relaunch_missing: bool,
        /// Run the task_374 S2c adversarial-review SWEEP each pass (board-reachable only): spawn the per-angle
        /// ephemeral reviewers for every review `in_review` and not yet `vetted`, via the same claim+spawn as
        /// `review-sweep`. Idempotent (the S2b per-angle claim means a review already covered re-spawns nothing),
        /// so running it on the watchdog cadence is the automatic trigger that makes the review engine live.
        /// Honors `--dry-run` (preview the plan without claiming/spawning). OPT-IN: a host turns it on in its
        /// watchdog unit's `ExecStart`, like `--observe --spawn`.
        #[arg(long)]
        review_sweep: bool,
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
    /// blocked on a deploy) are woken. Creates the channel if absent.
    PostDeploy {
        /// The repo that was deployed (e.g. `camshaft/task-board`).
        #[arg(long)]
        repo: String,
        /// The deployed commit sha (a waiter matches its awaited commit against this).
        #[arg(long)]
        sha: String,
        /// The host the deploy landed on (e.g. `host-b`).
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
    /// `webhook_url` (secondary-host-resident agents point it at their local fleet-notify) OR a LIVE reverse tunnel
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
    /// Audit observer-cadence COVERAGE (task_711): confirm every expected-running board-native agent is observed
    /// by at least one declared observer cadence, so a host-local blind spot is flagged LOUDLY instead of
    /// silently dropping a whole host's agents out of the fleet-self-improve loop. A cadence is a host that runs
    /// `fleet watchdog --observe` — declare the primary box with `--cadence <host>` and any secondary
    /// (`--pinned-only`) box with `--cadence-pinned <host>`; with none given, the audit assumes ONLY this host
    /// observes (run-anywhere scope). An agent managed by NO declared cadence is BLIND (hard fail, exit 1); one
    /// in local scope with no session transcript present here is a soft WARN. Transcript presence is verifiable
    /// only for the local host (this process reads only the local `~/.claude`); coverage by a cadence on another
    /// host is ASSUMED from the declared set. Read-only (no board writes).
    ObserveCoverage {
        /// A host that runs `fleet watchdog --observe` WITHOUT `--pinned-only` (a primary box): it observes
        /// unpinned run-anywhere agents plus agents pinned to it. Repeatable.
        #[arg(long = "cadence")]
        cadence: Vec<String>,
        /// A host that runs `fleet watchdog --observe --pinned-only` (a secondary box): it observes ONLY agents
        /// explicitly pinned to it. Repeatable.
        #[arg(long = "cadence-pinned")]
        cadence_pinned: Vec<String>,
        /// Also list the covered agents (default: print counts and name only the blind / no-transcript ones).
        #[arg(long)]
        verbose: bool,
    },
    /// Read-only disk-reclaim safety survey (task_714): classify every git worktree under each --root for
    /// whether it is SAFE TO RECLAIM (delete). The reclaim gate (v-disk-sweep) deletes only provably-preserved
    /// checkouts; this bakes out the git `@{u}` footgun — a checkout with NO upstream is NOT safe-by-default
    /// (absence of an upstream means its commits cannot be proven pushed), so it is reported KEEP (unsafe),
    /// never reclaimable. A checkout is reclaimable only when clean AND either fully pushed to its upstream or
    /// already an ancestor of the mainline, AND it clears the task_774 safety overlay: a git-clean checkout is
    /// still KEPT when it is a LIVE process's home (a /proc cwd at/under it), a PRIMARY clone (its `.git` is a
    /// directory, not a linked worktree), or an off-tree-agent worktree (`.v-<agent>`) — because git-preserved
    /// is not the same as safe-to-delete. Worktrees are enumerated via `git worktree list`, so a bare hub's
    /// `.claude/worktrees/*` are included. Prints the KEEP set (and, with `--verbose`, the reclaimable set too).
    /// Read-only — no writes, no deletes.
    ReclaimSurvey {
        /// A directory to scan: itself a git checkout, or a container whose direct children are checkouts.
        /// Repeatable; default = the current directory.
        #[arg(long = "root")]
        root: Vec<PathBuf>,
        /// The PRIMARY mainline ref a checkout must be an ancestor of to count as preserved (its commits already
        /// live there). Default origin/main. Per checkout the first ref that EXISTS is used from this primary
        /// then the brazil/plain-git fallbacks (`mainline`, `origin/mainline`, `main`), so a Brazil package is
        /// matched against its own `mainline` branch automatically.
        #[arg(long, default_value = "origin/main")]
        mainline: String,
        /// Also list the reclaimable checkouts (default: print only the KEEP / unsafe ones plus counts).
        #[arg(long)]
        verbose: bool,
    },
    /// Run the EXACT local gate CI's "clippy + test (workspace)" merge job runs (task_720), so a pre-merge
    /// check matches CI instead of an ad-hoc bin-only clippy that silently misses TEST-target lints and burns a
    /// CI round-trip. Runs, in order, from the fleet checkout root: `cargo clippy --workspace --all-targets --
    /// -D warnings`, then `cargo test --workspace`, streaming each command's output and exiting non-zero on the
    /// first failure. This is the committed single source of truth for the local gate — run it before requesting
    /// a merge. (Named constants mirror .github/workflows/checks.yml; a unit test asserts they stay identical.)
    /// With `--isolated` it ALSO runs a per-crate `cargo check -p <crate>` for every workspace member, catching
    /// the class of bug the workspace gate MASKS via Cargo feature unification (task_729): a crate that uses a
    /// dependency feature it does not declare still compiles in the workspace because a sibling enables that
    /// feature, so `--workspace` is green while CI's isolated per-crate build fails. Building each crate alone
    /// resolves features in isolation, so the missing declaration surfaces locally instead of in a CI round-trip.
    Gate {
        /// Run only the clippy step (skip the slower `cargo test --workspace`).
        #[arg(long)]
        clippy_only: bool,
        /// Also build each workspace crate IN ISOLATION (`cargo check -p <crate>`) to catch a missing feature
        /// declaration that workspace feature-unification hides (task_729) — the dimension CI's per-crate build adds.
        #[arg(long)]
        isolated: bool,
    },
    /// Audit each board-native agent's `metadata.worktree` for health (task_730): a STALE pointer (the path no
    /// longer exists — the symptom a worktree migration/retire leaves when it rewrites the charter but not the
    /// board, so the charter points at a dead tree) is a hard FAIL (exit 1); a worktree holding UNLANDED commits
    /// (ahead of its mainline) is surfaced as an at-risk notice (lose-able to a disk-reclaim sweep before it is
    /// landed — the data-loss risk this guards). Checks the REGISTERED worktree path; the deeper cross-clone
    /// stranding (commits left in an OLD clone a migration abandoned) is prevented by a safe migrate step
    /// (follow-up) and reclaim-survey already refuses to delete an orphaned worktree with unpushed commits.
    /// Read-only (no board writes).
    WorktreeCheck {
        /// Check only this agent (default: every board-native agent).
        #[arg(long)]
        agent: Option<String>,
        /// Also list the clean / no-worktree agents (default: print only the stale / unlanded ones plus counts).
        #[arg(long)]
        verbose: bool,
    },
    /// Correct a STALE board `metadata.worktree` (task_735 migration residue): rewrite a pre-migration pointer
    /// (e.g. a `.claude/worktrees/<id>` operator-clone path a worktree-check resolves to the wrong tree) to the
    /// agent's real per-agent fleet worktree `$FLEET_ROOT/agents/<id>/<repo>`. Report-only by default; `--apply`
    /// writes via patch_metadata. SAFE by construction: only a SINGLE-repo agent whose real fleet worktree EXISTS
    /// and differs from the registered path is corrected — a multi-repo agent (ambiguous primary tree) and an
    /// agent whose real tree is missing are reported + left untouched. The write-side counterpart to the
    /// read-only worktree-check; complements the spin-up sync that keeps it correct on each provision.
    WorktreeSync {
        /// Write the corrections (default: dry-run — only report what WOULD change).
        #[arg(long)]
        apply: bool,
    },
    /// Seam-check a MONITOR vertical (task_579): ff-sync its worktree to origin/main, then report whether any
    /// incoming commit touched the agent's declared SEAM — its `metadata.seam` file globs. A monitor wake is
    /// otherwise 100% deterministic git plumbing, so this lets the kickoff GATE the model wake: exit 0 = GREEN
    /// (no on-seam change — heartbeat and do NOT wake the model), exit 3 = CHANGED (wake the model; the
    /// seam-touching paths are printed so its tick opens with the diff in hand), exit 1 = error / no seam
    /// declared. Report-only — it computes the verdict, it does not itself wake or skip anything.
    SeamCheck {
        /// The agent whose `metadata.seam` globs + worktree to check. A seam entry prefixed with `!` is an
        /// EXCLUDE: a path is on-seam when it matches an include glob AND no exclude, so a vertical can own a
        /// directory broadly yet carve out a shared-but-not-owned subtree (e.g. `backend/cadenza/**` +
        /// `!backend/cadenza/select/**`) to avoid waking on another lane's churn.
        agent: String,
        /// Skip `git fetch` and check against the already-fetched `origin/main` (for tests / rapid re-runs).
        #[arg(long)]
        no_fetch: bool,
    },
    /// Gated-wake tick for an at-rest MONITOR vertical (task_579): run the seam decision and, on GREEN, perform
    /// the agent's board HEARTBEAT and exit 0 WITHOUT waking the model — so a deterministic "nothing changed"
    /// tick costs no model turn. On CHANGED (or, fail-safe, a no-seam / error) exit 3 so a kickoff wrapper wakes
    /// the model with the changed paths. REPORT-ONLY by default; `--apply` performs the green heartbeat. This is
    /// an OPT-IN mechanism: committing it changes nothing until an agent's loop is switched to call it.
    MonitorTick {
        /// The MONITOR agent to gate-tick (reads its `metadata.seam` globs + worktree from the board).
        agent: String,
        /// Perform the green-path heartbeat (board set_status). Without it, report only — print the decision,
        /// write nothing.
        #[arg(long)]
        apply: bool,
        /// Skip `git fetch` and decide against the already-fetched `origin/main` (for tests / rapid re-runs).
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
    /// Authoritative OS-process single-instance check (task_1063): scan /proc for live `claude` agent sessions,
    /// map each to its agent by its working directory (`…/agents/<agent>[/…]`), and report any agent with MORE
    /// THAN ONE live instance — the duplicate-instance condition the tmux-window guard (camshaft/fleet#327) can
    /// miss when a duplicate is detached or launched a different way. This is the process-list-authoritative
    /// method (not the window heuristic) for any duplicate-instance report. Report-only by default; `--kill`
    /// SIGTERMs the extra instance(s), keeping the oldest (longest-running) canonical one per agent.
    DedupCheck {
        /// Limit the check to one agent (by name); default scans every agent with a live claude session.
        agent: Option<String>,
        /// SIGTERM the extra instance(s), keeping the oldest canonical per agent. Without it, report only.
        #[arg(long)]
        kill: bool,
    },
    /// Inspect a board-native agent's live Claude Code session to confirm its auto-force-identity MCP wiring
    /// (task_1039): report whether the running `claude` process carries `FLEET_AGENT=<agent>` in its environment,
    /// whether its argv passes a `--mcp-config`/`--strict-mcp-config` that could shadow the user-scope config, and
    /// whether the user-scope `~/.claude.json` `task-board` entry wires the `X-Fleet-Agent` header (static or
    /// `headersHelper`). Read-only. The friction this bakes out: ruling out the client side of an identity
    /// mis-attribution otherwise means hand-reading `/proc/<pid>/environ`, `/proc/<pid>/cmdline`, and jq over the
    /// shared config — exactly the manual sweep task_1039 debugging required.
    McpCheck {
        /// The board-native agent whose live claude session to inspect.
        agent: String,
    },
    /// Scan staged (or given) content for Amazon-internal MARKER hits before a commit lands in a PUBLIC repo
    /// (task_782 / doc_104 mechanism). The marker VOCABULARY is loaded from a PRIVATE taxonomy file (never
    /// baked into this public source) via `--taxonomy` or `$FLEET_CONTENT_TAXONOMY`. Exit 1 on a blocking hit
    /// (fail-closed), 2 on a config error (no/invalid taxonomy), 0 when clean or advisory-only. Composes with
    /// an external secret scanner: detect-secrets owns secrets; this owns the internal identifier NAMES a
    /// secret scanner cannot know (the class behind the camshaft/fleet#340 leak).
    ScanContent {
        /// Path to the marker taxonomy TOML (else `$FLEET_CONTENT_TAXONOMY`).
        #[arg(long)]
        taxonomy: Option<String>,
        /// A file to scan as-is from disk; repeatable. Default when omitted: the staged index.
        #[arg(long = "file")]
        files: Vec<String>,
        /// A git diff range (e.g. `origin/main...HEAD`): scan the current on-disk content of every file the
        /// range adds/modifies. The CI-backstop mode; takes precedence over `--file` and the staged default.
        #[arg(long)]
        diff: Option<String>,
        /// Scan the staged index even when `--file`s are given.
        #[arg(long)]
        staged: bool,
        /// Downgrade every blocking hit to advisory (report, exit 0).
        #[arg(long)]
        warn_only: bool,
        /// Print only `file:line: category`, never the matched token (for public CI logs).
        #[arg(long)]
        redact: bool,
        /// Treat a missing taxonomy as a skip (exit 0) instead of a fail-closed config error.
        #[arg(long)]
        allow_missing_taxonomy: bool,
    },
    /// Keep a browser-facing tunnel's CLI client CONNECTED, not merely alive (task_1182): probe connectivity
    /// end-to-end THROUGH the tunnel and, when it is down (alive-but-not-forwarding — the failure a plain
    /// process-liveness supervisor misses), shed the stale client by its exact pid and re-run the host-provided
    /// recreate command. Generic: the tunnel name, probe URL, and recreate command are all caller-supplied, so
    /// no host-specific tunnel config lives in this CLI. Run from a non-overlapping systemd timer / oneshot.
    TunnelGuard {
        /// The tunnel's registered name (its `tunnel create --name`), used to find + shed exactly its client.
        #[arg(long)]
        name: String,
        /// A URL served THROUGH the tunnel to curl as the connectivity probe (a 2xx response is healthy).
        #[arg(long)]
        probe_url: String,
        /// The command to re-run to recreate the client when the probe fails (the host's canonical
        /// `tunnel create …` with its flags, preserving `--name` so the URL + link-root persist). Required
        /// unless `--systemd-unit` is given (which recovers via a service restart instead); ignored in that mode.
        #[arg(long, default_value = "")]
        recreate: String,
        /// When the tunnel is systemd-managed, the `--user` UNIT to restart on an unhealthy probe (e.g.
        /// `tunnel@board.service`). In this mode the recovery is a single `systemctl --user restart <unit>` —
        /// NOT a pid-kill + detached recreate — because systemd owns the client's lifecycle (`Restart=always`),
        /// so killing its pid would have systemd respawn it AND the detached recreate spawn a second, racing
        /// client. The restart also re-reads the unit's `EnvironmentFile`, so an allow-list change in it takes
        /// effect. `--recreate` is ignored when this is set.
        #[arg(long)]
        systemd_unit: Option<String>,
        /// The tunnel CLI binary (used only for `list`, to find the stale pid); default `tunnel`. Point this at
        /// a non-`PATH` install (e.g. `~/.toolbox/bin/tunnel`) when the guard runs without that bin on PATH.
        #[arg(long, default_value = "tunnel")]
        tunnel_bin: String,
        /// Actually shed the stale client + recreate; without it, only report the probe result (dry-run).
        #[arg(long)]
        apply: bool,
    },
    /// Spawn the per-angle ephemeral adversarial reviewers for ONE review on the existing spawn engine
    /// (task_374 S2a+S2b). Dry-run by default; `--apply` CLAIMS each angle on the board first and launches only
    /// the angles it wins, so re-running `--apply` is idempotent per angle. This is the manual entry point; the
    /// fleet-wide sweep is `review-sweep` (S2c).
    ReviewSpawn {
        /// The review id to spawn adversarial reviewers for.
        review_id: i64,
        /// Restrict to one angle key (default: all REVIEW_ANGLES). E.g. `risk-security`.
        #[arg(long)]
        angle: Option<String>,
        /// Actually launch the reviewers; without it, only print the spawn plan (dry-run).
        #[arg(long)]
        apply: bool,
    },
    /// Sweep EVERY review awaiting an adversarial pass (status `in_review`, not yet `vetted`) and run the
    /// per-angle claim+spawn for each (task_374 S2c) — the automatic trigger that makes the adversarial-review
    /// engine live, mirroring the observer `--spawn` cadence. Idempotent via the S2b per-angle claim, so it is
    /// safe to run on a timer/watchdog cadence or on a `review.opened_for_review` wake. Dry-run by default;
    /// `--apply` claims + launches.
    ReviewSweep {
        /// Actually claim + launch; without it, only print the sweep plan (dry-run).
        #[arg(long)]
        apply: bool,
    },
    /// Pattern-kill processes WITHOUT the `pkill -f` self-match footgun (task_1068): kill every process whose
    /// full command line contains `pattern`, but NEVER the caller's own process, its ancestor chain (the
    /// invoking shell, the Bash-tool wrapper, the agent's claude session), or its process group. `pkill -f`
    /// matches the pattern against the caller's OWN argv when the pattern string appears in an inline/heredoc
    /// command, silently killing the agent's shell (exit 143/144, no output); this excludes self so that cannot
    /// happen. Prints what it matched + what it will kill; `--dry-run` reports without killing.
    SafePkill {
        /// Substring to match against each process's full command line (like `pkill -f`, but self-safe).
        pattern: String,
        /// Report the matches + the kill set, but do not actually kill anything.
        #[arg(long)]
        dry_run: bool,
        /// Signal to send (name or number, passed to `kill -s`); default TERM.
        #[arg(long, default_value = "TERM")]
        signal: String,
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
        /// (`--observe`) that coexists with an existing rearm watchdog without double-rearming (host-a).
        #[arg(long)]
        no_rearm: bool,
        /// Include `--self-redeploy` (#388): the installed watchdog rebuilds + restarts the daemons when this
        /// binary falls behind its checkout, so a merged fix goes live without a manual rebuild. For a host that
        /// builds its daemons from a local checkout (not a hermetic flake deploy).
        #[arg(long)]
        self_redeploy: bool,
        /// Include `--hire-signal` (task_794): the installed watchdog posts a backlog-depth hire/route signal to
        /// the PM's board channel when a running agent's open backlog exceeds the threshold (cooldown-fenced per
        /// agent). Opt-in per host so the always-on sweep never posts until a host deliberately turns it on.
        #[arg(long)]
        hire_signal: bool,
        /// Include `--reap-stale-observers` (task_1045): the installed watchdog reaps leaked `obs-*` observer
        /// windows left after an observation completes. Opt-in per host, parallel to `--observe`/`--hire-signal`,
        /// so a flake-generated unit (`out_dir`) can carry the reaper — the hardcoded fleet-daemons.nix ExecStart
        /// is a separate unit path, so without this passthrough a flake deploy can never emit the reaper.
        #[arg(long)]
        reap_stale_observers: bool,
        /// INSTALL the units into `~/.config/systemd/user/` (user-level, no sudo) instead of printing them, and
        /// print the `systemctl --user enable` command — a clean, reversible install path for a host not on the
        /// declarative (nix) model. Reverse with `--uninstall`.
        #[arg(long)]
        install: bool,
        /// REMOVE the user units this installed (the inverse of `--install`) and print the `disable` command.
        #[arg(long)]
        uninstall: bool,
        /// The target host this watchdog instance manages, emitted as `Environment="FLEET_HOST=<host>"` in the
        /// unit (task_948, part of the task_937 Phase-A/B host migration) -- so the generated unit text never
        /// hardcodes a specific box: the host is a generator ARGUMENT, and a flake-managed install (the
        /// task_495/912 lane) passes it in at build time. Omitted when absent (no FLEET_HOST line at all).
        #[arg(long)]
        host: Option<String>,
        /// Emit the correctly-named unit files into this DIRECTORY purely (no HOME write, no systemctl guidance)
        /// instead of printing/installing -- for a nix derivation to point at $out (task_464 flake wiring).
        /// Takes precedence over --install.
        #[arg(long)]
        out_dir: Option<String>,
    },
    /// Emit or install the LAUNCHER as a systemd USER service + timer: `fleet up-board --launch` reconstitutes
    /// every board-native agent pinned to this host from the BOARD roster (not the file-hub / window.sh), so the
    /// fleet comes up on boot and is periodically reconciled WITHOUT the hub (task_464 launcher relocation, the
    /// counterpart to `watchdog-unit`). Default: PRINT the units (a declarative/nix host translates the text);
    /// `--install` writes them to `~/.config/systemd/user/` (no sudo); `--uninstall` removes them. `host`
    /// (task_937 Phase A) is emitted as `Environment="FLEET_HOST=<host>"` so the unit text never hardcodes a box
    /// -- a flake-managed install passes the target host in, making a later target-swap (Phase B) a one-argument
    /// change.
    UpUnit {
        /// Restrict the launcher to agents EXPLICITLY pinned to this host (metadata.host names it) -- for a
        /// secondary box that should bring up only its own pinned agents, not the unpinned run-anywhere roster.
        #[arg(long)]
        pinned_only: bool,
        /// Timer cadence in seconds for the periodic reconcile (systemd `OnUnitActiveSec`); the first run is at
        /// `OnBootSec`, so the fleet comes up shortly after boot and is re-reconciled on this interval.
        #[arg(long, default_value_t = 300)]
        interval_secs: u64,
        /// The fleet binary path to put in `ExecStart` (defaults to this binary's absolute path; set it to the
        /// installed path on the target host, e.g. the flake output).
        #[arg(long)]
        bin: Option<String>,
        /// INSTALL the units into `~/.config/systemd/user/` (user-level, no sudo) instead of printing them.
        #[arg(long)]
        install: bool,
        /// REMOVE the user units this installed (the inverse of `--install`).
        #[arg(long)]
        uninstall: bool,
        /// The target host whose board-native agents this launcher reconstitutes, emitted as
        /// `Environment="FLEET_HOST=<host>"` (task_937 Phase A) -- the host is a generator ARGUMENT, never baked
        /// into the unit text; a flake-managed install passes it in. Omitted when absent.
        #[arg(long)]
        host: Option<String>,
        /// Emit the correctly-named unit files into this DIRECTORY purely (no HOME write, no systemctl guidance)
        /// instead of printing/installing -- for a nix derivation to point at $out (task_464 flake wiring).
        #[arg(long)]
        out_dir: Option<String>,
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
        /// The target host this daemon runs on, emitted as `Environment="FLEET_HOST=<host>"` (task_937 Phase A,
        /// matching watchdog-unit/up-unit) -- the host is a generator ARGUMENT, never baked into the unit text,
        /// so a flake-managed install passes it in and a later target-swap (Phase B) is a one-value change.
        /// Omitted when absent (no FLEET_HOST line).
        #[arg(long)]
        host: Option<String>,
        /// Emit the correctly-named .service file into this DIRECTORY purely (no HOME write, no systemctl
        /// guidance) instead of printing/installing -- for a nix derivation to point at $out (task_464 flake
        /// wiring). Takes precedence over --install/--enable.
        #[arg(long)]
        out_dir: Option<String>,
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
    /// Confirm a process is actually dead before a deploy/guard starts its replacement — the kill-verification
    /// a daemon restart MUST do (task_866). A rebuilt binary leaves the old process on a now-deleted inode, so
    /// `/proc/<pid>/exe` reads `<path> (deleted)` and an exe-basename scan to confirm the kill momentarily
    /// returns empty: the kill reads as "succeeded" while the old daemon is still alive, and starting the
    /// replacement then runs TWO instances (the double-relay task_866 saw on a live operator channel). This
    /// confirms liveness with `kill -0 <pid>` (does the pid still exist?) — immune to that deleted-inode false
    /// negative — polling until the pid is gone. A guard runs `fleet confirm-kill <pid> --term --sigkill` and
    /// starts the replacement ONLY on exit 0. Exit 0 = confirmed gone; exit 1 = still alive at the deadline.
    ConfirmKill {
        /// The pid to confirm dead.
        pid: i64,
        /// Send SIGTERM first (a graceful stop), then poll. Without it, only polls an already-signalled pid.
        #[arg(long)]
        term: bool,
        /// Seconds to poll for the pid to disappear before giving up (or escalating with `--sigkill`).
        #[arg(long, default_value_t = 10)]
        timeout: u64,
        /// If still alive at the timeout, escalate to SIGKILL and poll a short grace before giving up.
        #[arg(long)]
        sigkill: bool,
    },
    /// Print the pids of every LIVE process whose running binary is named `<daemon>`, identified
    /// shell-agnostically (task_924). The companion to `confirm-kill`: that confirms a KNOWN pid is dead;
    /// this FINDS the pid in the first place. An agent's Bash-tool shell is zsh, where the `${exe% (deleted)}`
    /// strip used to find a rebuilt daemon's pid silently fails — the parens in the ` (deleted)` suffix are a
    /// glob GROUP under zsh, so the strip does nothing and a live daemon on a deleted inode reads as ABSENT,
    /// so the agent starts a SECOND instance (the double-relay to a live operator channel, task_866). This
    /// does the suffix-strip + basename match in Rust (immune to any shell) and confirms `kill -0` liveness.
    /// Use `fleet daemon-pids <name>` to find a daemon before a manual redeploy instead of a zsh exe-scan.
    DaemonPids {
        /// The daemon's binary name to match — the `/proc/<pid>/exe` basename, e.g. `membrain-skynet-bridge`.
        daemon: String,
    },
    /// INTERNAL: what the installed pre-commit hook (see `fmt_precommit_hook_body`) actually calls — not
    /// meant to be run by hand, but a plain subcommand (no hidden-arg plumbing) so it is easy to invoke
    /// directly for debugging. Finds the owning crate(s) of staged `.rs` files, auto-fixes + restages the
    /// fully-staged ones, and warns (fail-open, never blocks) on any only-partially-staged file it will not
    /// risk auto-restaging.
    FmtHookRun,
    /// Sync the Claude Code memory directory with the board (task_848 auto-sync, the native-tool fallback).
    /// `--direction file-to-board` pushes new/changed memory files to the board via `board-memory write`
    /// (idempotent via a per-slug content-hash state); `--direction board-to-file` refreshes the local dir
    /// from the board for native recall/offline. Scope is exactly one of `--agent <self>` or `--repo <repo>`
    /// (an agent syncs only its own scope, so file-to-board is same-writer). v-fleet-tooling wires it from
    /// SessionEnd/periodic (file-to-board) and SessionStart (board-to-file) hooks.
    MemorySync {
        /// Which half of the sync to run.
        #[arg(long)]
        direction: memory::SyncDirection,
        /// Scope: the agent whose own memories to sync (mutually exclusive with --repo).
        #[arg(long, conflicts_with = "repo")]
        agent: Option<String>,
        /// Scope: the repo whose memories to sync (mutually exclusive with --agent).
        #[arg(long)]
        repo: Option<String>,
        /// The Claude Code memory directory for this scope.
        #[arg(long)]
        memory_dir: PathBuf,
        /// Path to the board-memory CLI.
        #[arg(long, default_value = "board-memory")]
        board_memory: String,
        /// Per-slug content-hash state file for file-to-board skip-unchanged (default
        /// `$XDG_CONFIG_HOME/fleet/memory-sync-state.json`).
        #[arg(long)]
        state: Option<PathBuf>,
        /// Report what would sync without writing.
        #[arg(long)]
        dry_run: bool,
    },
    /// Run the dream pass analyzer over a memory corpus (task_827/task_956): a PROPOSE-ONLY pass that reads
    /// board-backed memory and emits a reviewed worklist (the librarian-blessed `dream-report/v1`); it never
    /// applies a change. Runs the protected classifier, exact-duplicate (merge / cross-repo twin / orphan
    /// add-links), MinHash near-duplicate, and write-later detectors.
    DreamAnalyze {
        /// JSONL corpus (board-identical bodies) for this analysis run. Omit when using --from-board.
        #[arg(long)]
        corpus: Option<PathBuf>,
        /// Build the corpus from the LIVE board instead of a --corpus file (automatic/scheduled dreaming,
        /// task_1123). Requires --scope.
        #[arg(long)]
        from_board: bool,
        /// The board wiki path prefix to dream on with --from-board, e.g. `repos/<repo>` or `agents/<agent>`.
        #[arg(long)]
        scope: Option<String>,
        /// Board REST base for --from-board (default the fleet board base).
        #[arg(long)]
        board_api: Option<String>,
        /// Where to write the dream-report JSON.
        #[arg(long, default_value = "dream-report.json")]
        out: PathBuf,
        /// Print this many proposals to stderr for a quick eyeball (0 = none).
        #[arg(long, default_value_t = 0)]
        sample: usize,
        /// Also publish the report to the versioned board doc `dreams/<scope>` (the librarian's review
        /// surface) and print a `DREAM-NEW` signal when a run surfaces new proposals. Requires --scope.
        #[arg(long)]
        publish_board: bool,
        /// Path to the board-memory CLI (used by --publish-board).
        #[arg(long, default_value = "board-memory")]
        board_memory: String,
        /// Repo worktree root to resolve file-path refs against for the verified-dangling staleness detector
        /// (task_1141). When omitted the staleness detector is skipped entirely -- with no worktree there is
        /// nothing to resolve against, so a corpus-only run never flags. The scheduled per-repo timer passes
        /// the scope's repo worktree (e.g. `--repo-root .`).
        #[arg(long)]
        repo_root: Option<PathBuf>,
        /// Opt in to the value_contradiction detector (OFF by default: it was net false-positive in the
        /// task_1123 co-verify -- a memory VALUE is usually a measurement, not an asserted invariant -- so it
        /// is kept behind this flag for a future narrower/LLM-assisted redesign).
        #[arg(long)]
        value_contradictions: bool,
    },
    /// Run automatic dreaming over EVERY repos/* scope on the board (task_1123): the scheduled RUNNER the
    /// fleet-dream systemd timer invokes. Pulls all repo-scoped memory, groups by repo, and runs the
    /// analyze+publish pass per scope -- refreshing each versioned `dreams/<scope>` review doc and writing a
    /// per-scope report under `<state-dir>/dreams`. Emits a `DREAM-NEW` stdout line per scope with new
    /// proposals (the notify-on-new signal). When a repo-checkout base is configured (`--repo-root-base` or
    /// `config.repo_checkout_base`) each scope that resolves to a local checkout also runs the staleness
    /// detector against it; scopes with no local checkout stay corpus-only.
    DreamRun {
        /// Board REST base for the live-board pull (default the fleet board base).
        #[arg(long)]
        board_api: Option<String>,
        /// Directory for the per-scope report files (default `$XDG_STATE_HOME/fleet`, else `~/.local/state/fleet`).
        #[arg(long)]
        state_dir: Option<PathBuf>,
        /// Path to the board-memory CLI (used to publish each `dreams/<scope>` doc).
        #[arg(long, default_value = "board-memory")]
        board_memory: String,
        /// Board channel for the notify-on-new post (librarian's dedicated `dream-reports` feed; must match
        /// `dream::DREAM_NOTIFY_CHANNEL`). Created-or-got by name; posted to only when a scope has new>0.
        #[arg(long, default_value = "dream-reports")]
        notify_channel: String,
        /// Base dir under which local repo checkouts live; each `repos/<org>-<name>` scope maps to
        /// `<base>/<org>/<name>` (else `<base>/<slug>`) to enable the staleness detector. Overrides
        /// `config.repo_checkout_base`; absent (and unset in config) → every scope is corpus-only.
        #[arg(long)]
        repo_root_base: Option<PathBuf>,
        /// Opt in to the value_contradiction detector (OFF by default: net false-positive in the task_1123
        /// co-verify, kept behind this flag for a future redesign). The scheduled timer leaves it off.
        #[arg(long)]
        value_contradictions: bool,
    },
    /// Apply a DISPOSITIONED dream proposal under the lane gate (task_827/task_956): the gated INC 2
    /// apply-workflow. Nothing autonomous -- names the disposition + authenticating principal; a protected
    /// lane / KB-side op is EMITTED for the librarian (never script-executed), destructive ops archive
    /// (restore-reversible), and dry-run (the default) applies nothing. `--execute` performs only a
    /// standard-lane board-side op that carries a disposition version pin.
    DreamApply {
        /// The dream-report JSON (a `dream-analyze` output).
        #[arg(long)]
        report: PathBuf,
        /// The proposal_id to apply.
        #[arg(long)]
        proposal: String,
        /// The disposition decision.
        #[arg(long, value_parser = ["apply", "decline"])]
        disposition: String,
        /// The authenticating principal (must be `librarian` for the protected lane).
        #[arg(long)]
        principal: String,
        /// Perform board-side ops (default: dry-run, applies nothing).
        #[arg(long)]
        execute: bool,
        /// Audit-trace path (default `$XDG_CONFIG_HOME/fleet/dream-apply-audit.jsonl`).
        #[arg(long)]
        audit_log: Option<PathBuf>,
        /// Board REST base for the condition-C version-pin check (default the fleet board base).
        #[arg(long)]
        board_api: Option<String>,
    },
    /// Report every task currently in the uncategorized intake project that violates an inbox invariant
    /// (task_1217): a task `in_progress`/`blocked` while still in the inbox, or one dwelling past the dwell
    /// SLA. Report-only — prints the offenders and exits non-zero when any violate; the cooldown-fenced alert
    /// to board-triage is a follow-on. The dwell+state safety net for the uncategorized project (task_1214).
    IntakeWatch {
        /// The uncategorized intake project id to sweep.
        #[arg(long)]
        project: i64,
        /// The dwell SLA in seconds — a non-terminal task older than this while still in the inbox trips the
        /// dwell invariant (operator: "~2min dwell").
        #[arg(long, default_value_t = 120)]
        sla_secs: u64,
        /// Post the active alert (task_1217 shape (a)): a per-offender comment on each violating task tagging
        /// board-triage, cooldown-fenced per offender. Default off — the sweep is report-only.
        #[arg(long)]
        alert: bool,
        /// Per-offender alert cooldown in seconds — an offender is alerted at most once per window, so a fast
        /// sweep does not re-comment every pass.
        #[arg(long, default_value_t = 3600)]
        alert_cooldown_secs: u64,
    },
    /// Run the daily board-hygiene retention tick (task_1249): POST the project-scoped sweeps (archive-done +
    /// age-out-todos) that keep a project tight. DRY-RUN by default (prints the plan); pass --execute to POST.
    /// Each sweep is restorable + auditable on the board side.
    BoardHygiene {
        /// The project to sweep (both archive-done and age-out-todos target it).
        #[arg(long)]
        project: i64,
        /// archive-done threshold in days (board default 7).
        #[arg(long, default_value_t = 7)]
        archive_done_days: u64,
        /// age-out-todos threshold in days (board default 14).
        #[arg(long, default_value_t = 14)]
        age_out_days: u64,
        /// Skip the age-out-todos sweep (run archive-done only).
        #[arg(long)]
        skip_age_out: bool,
        /// Actually POST the sweeps (default is a dry-run that only prints the plan).
        #[arg(long)]
        execute: bool,
    },
    /// Compose an agent's role-inherited mandate sets into one boot-local artifact (task_906): select the
    /// ordered set paths from registry metadata, fetch each operator-approved version, concatenate under
    /// stable headers. DRY-RUN by default (prints it); pass --write to atomically materialize to --out.
    ComposeMandates {
        /// The agent whose mandate sets to compose (its board metadata drives the selection).
        #[arg(long)]
        agent: String,
        /// The boot-local artifact path to materialize (with --write).
        #[arg(long)]
        out: String,
        /// Atomically write the artifact to --out (only when the content changed); default is a dry-run.
        #[arg(long)]
        write: bool,
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
        Cmd::UpBoard {
            launch,
            pinned_only,
        } => up_board(launch, pinned_only),
        Cmd::SpinUp { agent, apply } => spin_up(&agent, apply),
        Cmd::SpinDown {
            agent,
            apply,
            force,
        } => spin_down(&agent, apply, force),
        Cmd::SpinDownAll {
            except,
            apply,
            force,
        } => spin_down_all(except.as_deref(), apply, force),
        Cmd::SpinUpAll {
            except,
            limit,
            include_held,
            apply,
        } => spin_up_all(except.as_deref(), limit, include_held, apply),
        Cmd::BounceSession {
            agent,
            apply,
            force,
        } => bounce_session(&fleet, &agent, apply, force),
        Cmd::BounceStale { apply, force } => bounce_stale(&fleet, apply, force),
        Cmd::Status { stale_only } => status(stale_only),
        Cmd::Services { service } => services(service.as_deref()),
        Cmd::Quiesce { on, off } => quiesce_cmd(on, off),
        Cmd::Watchdog {
            stale_only,
            rearm,
            observe,
            spawn,
            dry_run,
            pinned_only,
            self_redeploy,
            reap_stale_observers,
            hire_signal,
            revive_stranded,
            bounce_stale,
            recover_wedged,
            relaunch_missing,
            review_sweep,
        } => watchdog(
            stale_only,
            rearm,
            observe,
            spawn,
            dry_run,
            pinned_only,
            self_redeploy,
            reap_stale_observers,
            hire_signal,
            revive_stranded,
            bounce_stale,
            recover_wedged,
            relaunch_missing,
            review_sweep,
        ),
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
        } => set_meta(
            &agent,
            &repos,
            interval.as_deref(),
            host.as_deref(),
            native,
            devshell,
            apply,
        ),
        Cmd::SetInterval { agent, interval } => set_interval(&fleet, &agent, &interval),
        Cmd::IntakeWatch {
            project,
            sla_secs,
            alert,
            alert_cooldown_secs,
        } => intake_watch(&fleet, project, sla_secs, alert, alert_cooldown_secs),
        Cmd::BoardHygiene {
            project,
            archive_done_days,
            age_out_days,
            skip_age_out,
            execute,
        } => board_hygiene(
            project,
            archive_done_days,
            age_out_days,
            skip_age_out,
            execute,
        ),
        Cmd::ComposeMandates { agent, out, write } => compose_mandates(&fleet, &agent, &out, write),
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
        } => transcripts_cmd(
            &agent,
            session.as_deref(),
            since.as_deref(),
            overlap,
            &harness,
        ),
        Cmd::ServedSet { toml } => served_set(toml),
        Cmd::WakeAudit { verbose } => wake_audit(verbose),
        Cmd::ObserveCoverage {
            cadence,
            cadence_pinned,
            verbose,
        } => observe_coverage(cadence, cadence_pinned, verbose),
        Cmd::ReclaimSurvey {
            root,
            mainline,
            verbose,
        } => reclaim_survey(root, mainline, verbose),
        Cmd::Gate {
            clippy_only,
            isolated,
        } => gate(clippy_only, isolated),
        Cmd::WorktreeCheck { agent, verbose } => worktree_check(agent, verbose),
        Cmd::WorktreeSync { apply } => worktree_sync(apply),
        Cmd::SeamCheck { agent, no_fetch } => seam_check(&agent, no_fetch),
        Cmd::MonitorTick {
            agent,
            apply,
            no_fetch,
        } => monitor_tick(&agent, apply, no_fetch),
        Cmd::SafeguardCheck {
            agent,
            threshold,
            tail,
        } => safeguard_check(&agent, threshold, tail),
        Cmd::DedupCheck { agent, kill } => dedup_check(agent.as_deref(), kill),
        Cmd::McpCheck { agent } => mcp_check(&agent),
        Cmd::ScanContent {
            taxonomy,
            files,
            diff,
            staged,
            warn_only,
            redact,
            allow_missing_taxonomy,
        } => scan::scan_content(scan::ScanOpts {
            taxonomy,
            files,
            diff,
            staged,
            warn_only,
            redact,
            allow_missing_taxonomy,
        }),
        Cmd::TunnelGuard {
            name,
            probe_url,
            recreate,
            tunnel_bin,
            apply,
            systemd_unit,
        } => tunnel_guard(
            &name,
            &probe_url,
            &recreate,
            &tunnel_bin,
            apply,
            systemd_unit.as_deref(),
        ),
        Cmd::ReviewSpawn {
            review_id,
            angle,
            apply,
        } => review_spawn(&board_session(), review_id, angle.as_deref(), apply),
        Cmd::ReviewSweep { apply } => review_sweep(&board_session(), apply),
        Cmd::SafePkill {
            pattern,
            dry_run,
            signal,
        } => safe_pkill(&pattern, dry_run, &signal),
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
            hire_signal,
            reap_stale_observers,
            install,
            uninstall,
            host,
            out_dir,
        } => watchdog_unit(
            !no_rearm,
            observe,
            pinned_only,
            self_redeploy,
            hire_signal,
            reap_stale_observers,
            interval_secs,
            bin,
            install,
            uninstall,
            host,
            out_dir,
        ),
        Cmd::UpUnit {
            pinned_only,
            interval_secs,
            bin,
            install,
            uninstall,
            host,
            out_dir,
        } => up_unit(
            pinned_only,
            interval_secs,
            bin,
            install,
            uninstall,
            host,
            out_dir,
        ),
        Cmd::DaemonUnit {
            name,
            exec,
            restart_sec,
            bin,
            install,
            enable,
            uninstall,
            host,
            out_dir,
        } => daemon_unit(
            &name,
            exec,
            restart_sec,
            bin,
            install,
            enable,
            uninstall,
            host,
            out_dir,
        ),
        Cmd::Version => println!("{}", version_line()),
        Cmd::Redeploy { apply } => redeploy(apply),
        Cmd::ConfirmKill {
            pid,
            term,
            timeout,
            sigkill,
        } => confirm_kill(pid, term, timeout, sigkill),
        Cmd::DaemonPids { daemon } => daemon_pids(&daemon),
        Cmd::FmtHookRun => fmt_hook_run(),
        Cmd::MemorySync {
            direction,
            agent,
            repo,
            memory_dir,
            board_memory,
            state,
            dry_run,
        } => {
            let code = memory::sync_cmd(
                direction,
                agent,
                repo,
                &memory_dir,
                &board_memory,
                state,
                dry_run,
            );
            if code != 0 {
                std::process::exit(code);
            }
        }
        Cmd::DreamAnalyze {
            corpus,
            from_board,
            scope,
            board_api,
            out,
            sample,
            publish_board,
            board_memory,
            repo_root,
            value_contradictions,
        } => {
            let api = board_api.unwrap_or_else(board::Board::base_url);
            let code = dream::analyze_cmd(
                corpus.as_deref(),
                from_board,
                scope.as_deref(),
                &api,
                &out,
                sample,
                publish_board,
                &board_memory,
                repo_root.as_deref(),
                value_contradictions,
            );
            if code != 0 {
                std::process::exit(code);
            }
        }
        Cmd::DreamRun {
            board_api,
            state_dir,
            board_memory,
            notify_channel,
            repo_root_base,
            value_contradictions,
        } => {
            let api = board_api.unwrap_or_else(board::Board::base_url);
            let state_dir = state_dir.unwrap_or_else(default_state_dir);
            // CLI flag wins; else the config-file base (so the scheduled service enables staleness via
            // config.toml with no flag). Absent in both → every scope stays corpus-only.
            let repo_root_base = repo_root_base.or_else(|| {
                config::get()
                    .repo_checkout_base
                    .as_deref()
                    .map(PathBuf::from)
            });
            let code = dream::run_cmd(
                &api,
                &state_dir,
                &board_memory,
                &notify_channel,
                repo_root_base.as_deref(),
                value_contradictions,
            );
            if code != 0 {
                std::process::exit(code);
            }
        }
        Cmd::DreamApply {
            report,
            proposal,
            disposition,
            principal,
            execute,
            audit_log,
            board_api,
        } => {
            let code = dream_apply::apply_cmd(
                &report,
                &proposal,
                &disposition,
                &principal,
                execute,
                audit_log,
                board_api,
            );
            if code != 0 {
                std::process::exit(code);
            }
        }
    }
}

/// The launch plan derived from a board workspace-kind resource: the `setup_script` that materializes the
/// workspace and the free-form `config` hints (`cwd`, `pre_trust`, `env`) the launcher reads. Parsing is
/// pure so it is unit-tested without the board.
struct WorkspaceKindPlan {
    name: String,
    description: Option<String>,
    setup_script: Option<String>,
    verify: Option<String>,
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
    let name = rec
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or("?")
        .to_string();
    let description = rec
        .get("description")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let setup_script = rec
        .get("setup_script")
        .and_then(|v| v.as_str())
        .filter(|s| !s.trim().is_empty())
        .map(str::to_string);
    // task_1174: an optional post-setup health-assertion command, run AFTER the setup_script to confirm the
    // workspace is actually ready (not just that setup_script exited 0). Same shape as setup_script.
    let verify = rec
        .get("verify")
        .and_then(|v| v.as_str())
        .filter(|s| !s.trim().is_empty())
        .map(str::to_string);
    let config = rec
        .get("config")
        .cloned()
        .unwrap_or(serde_json::Value::Null);
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
    WorkspaceKindPlan {
        name,
        description,
        setup_script,
        verify,
        cwd,
        pre_trust,
        env,
    }
}

/// The environment a workspace-kind `setup_script` receives: the agent identity and fleet root, the resolved
/// per-agent launch directory as `FLEET_WORKSPACE_CWD`, then the kind's own `config.env`. The launch cwd is
/// exported because several agents can share ONE kind (same `setup_script`) while each launches in its own
/// workspace directory at an arbitrary host path with no shared convention, so the shared script has no other
/// way to find the agent's own workspace. Config env is appended last so a kind may override a built-in if it
/// deliberately must. Pure — unit-tested.
fn setup_script_env(
    agent: &str,
    fleet_root: &str,
    plan: &WorkspaceKindPlan,
) -> Vec<(String, String)> {
    let mut env = vec![
        ("FLEET_AGENT".to_string(), agent.to_string()),
        ("FLEET_ROOT".to_string(), fleet_root.to_string()),
        ("FLEET_WORKSPACE_CWD".to_string(), plan.cwd.clone()),
    ];
    env.extend(plan.env.iter().cloned());
    env
}

/// Ensure an agent's launch directory exists before anything runs in it (task_1105). A workspace kind whose
/// setup_script `cd`s into `FLEET_WORKSPACE_CWD` under `set -e` — the membrain kind, which does no
/// clone/materialize and treats the launch cwd as already present — otherwise aborts with a raw
/// `cd: No such file or directory` on every fresh mint, and the subsequent launch (which runs in the same
/// cwd) would fail the same way. Creating the bare dir is a no-op when it already exists (a kind whose script
/// materializes into it still works) and surfaces a clear, path-named error when the target is a file rather
/// than leaking a raw errno. Factored out so the pre-flight is unit-tested.
fn ensure_launch_cwd(cwd: &str) -> Result<(), String> {
    std::fs::create_dir_all(cwd).map_err(|e| format!("mkdir launch cwd {cwd}: {e}"))
}

/// Spin up an agent whose workspace is defined by a board workspace-kind resource rather than by `repos`.
/// Fetches the kind, reports the plan, and on `--apply` ensures the launch cwd exists, runs its setup_script
/// (with `FLEET_AGENT` / `FLEET_ROOT` and any `config.env` in the environment) to materialize the workspace,
/// pre-trusts the launch paths, and launches the agent in the kind's `config.cwd`. (#287)
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
    proactive_ownership: bool,
    apply: bool,
    cwd_override: Option<&str>,
    launch_patch: Option<serde_json::Value>,
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

    println!(
        "spin-up '{agent}' ({}):",
        if apply { "APPLY" } else { "dry-run" }
    );
    println!(
        "  charter on board: {}",
        if has_charter {
            "yes — the agent fetches it in-session at boot"
        } else {
            "NO — declare a charter first"
        }
    );
    println!("  harness={harness}  model={model}  effort={effort}  interval={interval}");
    println!(
        "  workspace kind: {}{}",
        plan.name,
        plan.description
            .as_deref()
            .map(|d| format!(" — {d}"))
            .unwrap_or_default()
    );
    println!("  launch cwd: {}", plan.cwd);
    match &plan.setup_script {
        Some(s) => println!(
            "  setup_script: {} line(s) — runs with FLEET_AGENT/FLEET_ROOT/FLEET_WORKSPACE_CWD{} in the environment",
            s.lines().count(),
            if plan.env.is_empty() {
                String::new()
            } else {
                format!(" + {} config env var(s)", plan.env.len())
            }
        ),
        None => println!(
            "  setup_script: NONE — no materialization; spin-up creates the launch cwd, then launches in it"
        ),
    }
    match &plan.verify {
        Some(v) => println!(
            "  verify: {} line(s) — runs AFTER setup_script; a non-zero exit aborts spin-up (workspace not ready)",
            v.lines().count()
        ),
        None => println!("  verify: NONE — no post-setup health assertion for this kind"),
    }

    if !apply {
        println!("  (dry-run — re-run with --apply to run the setup_script, verify, + launch)");
        return;
    }
    if !has_charter {
        eprintln!(
            "  refusing to launch '{agent}': no charter on the board for it to self-discover"
        );
        std::process::exit(1);
    }

    // Ensure the launch cwd exists before the setup_script `cd`s into it and before the agent launches there
    // (task_1105). A clone/materialize kind recreates it harmlessly; a pre-existing-dir kind (membrain) now
    // finds it present instead of aborting on a raw `cd: No such file or directory`.
    if let Err(e) = ensure_launch_cwd(&plan.cwd) {
        eprintln!("  setup FAILED: {e}");
        std::process::exit(1);
    }

    if let Some(script) = &plan.setup_script {
        // Run the setup_script from the fleet root so it has a stable base, with the agent identity and root
        // in the environment; the script materializes anything else the kind needs (the launch cwd itself is
        // already created above).
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

    // task_1174: run the kind's post-setup health assertion, if any, AFTER the setup_script. A setup_script
    // exiting 0 does not prove the workspace is usable — an interrupted or IO-stalled cold bootstrap can leave
    // a half-populated workspace (e.g. a toolchain whose bin/ never populated) that then fails quietly at build
    // time and pushes the agent to a workaround. The verify command (with the same environment as the
    // setup_script) asserts readiness and, on a non-zero exit, aborts spin-up loudly rather than launching an
    // agent into a broken workspace. A kind with no verify command is unchanged.
    if let Some(check) = &plan.verify {
        let mut cmd = std::process::Command::new("bash");
        cmd.arg("-c").arg(check).current_dir(fleet_root);
        for (k, v) in setup_script_env(agent, fleet_root, &plan) {
            cmd.env(k, v);
        }
        match cmd.status() {
            Ok(st) if st.success() => println!("  verify OK — workspace is ready"),
            Ok(st) => {
                eprintln!(
                    "  verify FAILED (exit {}): workspace not ready (a half-complete or interrupted bootstrap?) — refusing to launch '{agent}' into a broken workspace",
                    st.code().unwrap_or(-1)
                );
                std::process::exit(1);
            }
            Err(e) => {
                eprintln!("  verify FAILED to run: {e}");
                std::process::exit(1);
            }
        }
    }

    match pre_trust_for_harness(harness, &plan.pre_trust) {
        Ok(true) => println!(
            "  pre-trusted {} path(s) (launch cwd + fleet root + config pre_trust) in the {harness} config",
            plan.pre_trust.len()
        ),
        Ok(false) => {}
        Err(e) => {
            eprintln!("  WARN: could not pre-trust: {e} (agent may hit a one-time trust prompt)")
        }
    }
    match launch_board_agent(
        agent,
        &plan.cwd,
        harness,
        model,
        effort,
        interval,
        devshell,
        reactive,
        proactive_ownership,
    ) {
        Ok(win) => {
            println!(
                "  LAUNCHED '{agent}' in tmux window '{win}' (cwd {}) — it will get_agent itself for its charter, then run a work-conserving dynamic /loop (idle cadence ~{interval})",
                plan.cwd
            );
            match board.patch_metadata(agent, serde_json::json!({ "native": true })) {
                Ok(()) => println!("  stamped metadata.native=true (board-native roster marker)"),
                Err(e) => eprintln!("  WARN: launched but could not stamp native flag: {e}"),
            }
            // Apply the post-launch metadata patch: clears a satisfied one-time bringup gate (task_1144) and
            // stamps the comms-norm acknowledgement (task_1178). Idempotent + non-fatal.
            if let Some(patch) = launch_patch {
                match board.patch_metadata(agent, patch) {
                    Ok(()) => println!(
                        "  stamped post-launch metadata (bringup gate cleared if set; comms-norm ack recorded)"
                    ),
                    Err(e) => {
                        eprintln!("  WARN: launched but could not stamp post-launch metadata: {e}")
                    }
                }
            }
            // Record the launch rev so the watchdog-driven stale-session sweep can tell this fresh session
            // from one that connected under an older binary (task_752).
            stamp_launch_rev(agent);
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

/// The `metadata.worktree` patch to apply when provisioning an agent, or `None` when the board already points
/// at `workdir`. task_730: a worktree migration/retire previously left `metadata.worktree` pointing at a
/// retired tree, so the charter named a dead path; syncing it to the REAL provisioned workdir on every
/// provision stops that recurrence. Idempotent — `None` when it already matches, so a re-provision is a no-op.
/// Pure — unit-tested.
fn worktree_metadata_patch(current: Option<&str>, workdir: &str) -> Option<serde_json::Value> {
    (current != Some(workdir)).then(|| serde_json::json!({ "worktree": workdir }))
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
    let md = rec
        .get("metadata")
        .cloned()
        .unwrap_or(serde_json::Value::Null);
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
    let devshell = md
        .get("devshell")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    // Opt-in REACTIVE responder pacing (#438): a mention-only bot (e.g. a Slack-channel participant) gets a
    // kickoff whose only actionable trigger is being explicitly addressed, so it does not self-poll on ambient
    // chatter. Off by default — a normal work-conserving worker is unchanged.
    let reactive = md
        .get("reactive")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    // task_1138 opt-in PROACTIVE-OWNERSHIP pacing: an agent that must continuously build a deliverable (a
    // promo-assist owning a candidate's case) gets a kickoff that forbids idling until its charter's
    // work-accomplished gate is met, rather than the work-conserving idle-when-no-task default. Off by default.
    let proactive_ownership = md
        .get("proactive_ownership")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    // Accept the canonical structured list AND the looser hand-authored shapes (bare-string array, CSV
    // string) so a repos written by hand is never silently dropped (#472 / operator seq-6269).
    if md.get("repos").is_some_and(serde_json::Value::is_string) {
        eprintln!(
            "  note: metadata.repos is a comma-separated STRING — normalized to a structured list; prefer writing repos as [{{\"repo\":\"…\"}}] (the form `fleet set-meta --repo` produces)"
        );
    }
    let repos = normalize_repos(md.get("repos"));
    let fleet_root = config::get()
        .root
        .clone()
        .unwrap_or_else(|| format!("{}/.fleet", std::env::var("HOME").unwrap_or_default()));

    // Compute the post-launch metadata patch now, while the agent's own metadata is in hand, to apply after a
    // successful launch below: clears a satisfied one-time bringup gate (task_1144) and stamps the comms-norm
    // acknowledgement (task_1178).
    let launch_patch = launch_metadata_patch(Some(&md));

    // A board-defined custom workspace kind (metadata.workspace_kind) takes precedence over `repos`: the
    // board resource named by the kind carries a setup_script that materializes the workspace and a
    // free-form config with the launch hints (cwd/pre_trust/env). This lets an environment the fleet does
    // not model natively be defined in a board resource and driven from there. (#287)
    if let Some(kind) = field("workspace_kind") {
        // A per-agent `metadata.workspace_cwd` overrides the kind's `config.cwd`, so several agents can share
        // one kind (same setup_script/env) while each launches in its own workspace directory.
        let cwd_override = field("workspace_cwd");
        return spin_up_workspace_kind(
            &board,
            agent,
            &kind,
            &fleet_root,
            has_charter,
            &harness,
            &model,
            &effort,
            &interval,
            devshell,
            reactive,
            proactive_ownership,
            apply,
            cwd_override.as_deref(),
            launch_patch,
        );
    }

    println!(
        "spin-up '{agent}' ({}):",
        if apply { "APPLY" } else { "dry-run" }
    );
    println!(
        "  charter on board: {}",
        if has_charter {
            "yes — the agent fetches it in-session at boot"
        } else {
            "NO — declare a charter first"
        }
    );
    println!("  harness={harness}  model={model}  effort={effort}  interval={interval}");
    if repos.is_empty() {
        println!(
            "  repos: NONE declared — no workspace to materialize (declare `repos` on the board record)"
        );
    }
    let mut primary_workdir: Option<String> = None;
    for r in &repos {
        let repo = r.get("repo").and_then(|v| v.as_str()).unwrap_or("?");
        let branch = r.get("branch").and_then(|v| v.as_str()).unwrap_or("main");
        // Optional per-repo UPSTREAM remote (e.g. `aws/s2n-quic`): a fork-maintainer vertical declares the
        // repo it forks so spin-up pre-wires the `upstream` remote + tags into the shared mirror, making a
        // fork-vs-upstream parity diff one command on a FRESH checkout (task_875). Absent for a non-fork repo.
        let upstream = r
            .get("upstream")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|u| !u.is_empty());
        let wd = if apply {
            match workspace::ensure(&fleet_root, agent, repo, branch, upstream) {
                Ok(wd) => {
                    println!(
                        "  workspace ready: {wd}  (worktree of {repo}@{branch} off a shared mirror)"
                    );
                    if let Some(up) = upstream {
                        println!(
                            "  pre-wired upstream remote '{up}' (+tags) on the mirror — fork-parity diff is one command (task_875)"
                        );
                    }
                    // Install the generic fail-open fmt pre-commit into the repo's shared MIRROR hooks dir
                    // (a linked worktree runs hooks from the common/mirror dir), so a board-native agent gets
                    // a commit-time rustfmt nudge — the safety net a ~/.fleet worktree otherwise lacks (#283).
                    let hooks = std::path::Path::new(&workspace::mirror_dir(&fleet_root, repo))
                        .join("hooks");
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
            println!(
                "  would materialize: {wd}  (worktree of {repo}@{branch} off {fleet_root}/mirrors)"
            );
            if let Some(up) = upstream {
                println!(
                    "    + would pre-wire upstream remote '{up}' (+tags) on the mirror (fork-parity diff, task_875)"
                );
            }
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
                println!(
                    "  repo-less workspace ready: {dir}  (no repo declared — runs via the board)"
                );
            } else {
                println!(
                    "  would create repo-less workspace: {dir}  (no repo declared — runs via the board)"
                );
            }
            dir
        }
    };
    if !apply {
        match build_launch_cmd(
            &harness,
            &model,
            &effort,
            devshell.then_some(workdir.as_str()),
        ) {
            Ok(_) => println!(
                "  would launch: {harness} in {workdir}{} (board MCP in-session) with a self-discovery kickoff, then a work-conserving dynamic /loop (idle cadence ~{interval})",
                if devshell {
                    " [inside nix develop]"
                } else {
                    ""
                }
            ),
            Err(e) => println!("  would NOT launch: {e}"),
        }
        println!("  (dry-run — re-run with --apply to materialize + launch)");
        return;
    }
    // Sync the board charter's metadata.worktree to the REAL provisioned tree (task_730): a migration/retire
    // must never leave the charter pointing at a dead/old path. Done after materialize (the worktree exists)
    // and before the launch/charter guard, so the pointer is corrected even if the launch is later refused.
    // Idempotent (no-op when already correct) and NON-FATAL — a metadata blip must not fail a spin-up.
    if let Some(patch) =
        worktree_metadata_patch(md.get("worktree").and_then(|v| v.as_str()), &workdir)
    {
        match board.patch_metadata(agent, patch) {
            Ok(()) => println!(
                "  metadata.worktree -> {workdir} (synced so the charter never points at a stale tree)"
            ),
            Err(e) => eprintln!(
                "  WARN: could not sync metadata.worktree ({e}); run `fleet worktree-check` to catch a stale pointer"
            ),
        }
    }
    if !has_charter {
        eprintln!(
            "  refusing to launch '{agent}': no charter on the board for it to self-discover"
        );
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
        Ok(true) => println!(
            "  pre-trusted {} path(s) in the {harness} config",
            trust.len()
        ),
        Ok(false) => {}
        Err(e) => {
            eprintln!("  WARN: could not pre-trust: {e} (agent may hit a one-time trust prompt)")
        }
    }
    match launch_board_agent(
        agent,
        &workdir,
        &harness,
        &model,
        &effort,
        &interval,
        devshell,
        reactive,
        proactive_ownership,
    ) {
        Ok(win) => {
            let loop_kind = if reactive {
                "reactive (mention-only) /loop"
            } else {
                "work-conserving dynamic /loop"
            };
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
            // Apply the post-launch metadata patch: clears a satisfied one-time bringup gate (task_1144, the
            // frank drop) so a later reconstitution (spin-up-all / up-board) includes this now-live agent, and
            // stamps the comms-norm acknowledgement (task_1178). Idempotent + non-fatal.
            if let Some(patch) = launch_patch {
                match board.patch_metadata(agent, patch) {
                    Ok(()) => println!(
                        "  stamped post-launch metadata (bringup gate cleared if set; comms-norm ack recorded)"
                    ),
                    Err(e) => {
                        eprintln!("  WARN: launched but could not stamp post-launch metadata: {e}")
                    }
                }
            }
            // Record the launch rev so the watchdog-driven stale-session sweep can tell this fresh session
            // from one that connected under an older binary (task_752).
            stamp_launch_rev(agent);
        }
        Err(e) => {
            eprintln!("  launch FAILED: {e}");
            std::process::exit(1);
        }
    }
}

/// Clamp-aware idle-cadence guidance for a monitor whose idle cadence exceeds one hour (task_765 Tier A).
/// A dynamic `/loop` self-wake (ScheduleWakeup) is hard-clamped to a one-hour maximum, so an at-rest monitor
/// whose idle cadence is longer than an hour CANNOT honor it by self-re-arming — it idle-polls hourly
/// regardless of the interval it sets (triple-corroborated on v-compiler-perf / v-hivemind / v-runtime, which
/// each set a 2-3h cadence yet woke hourly). The only lever that expresses a >1h cadence is a FIXED-INTERVAL
/// `/loop` cron (CronCreate), which has no such ceiling. Returns the extra kickoff clause for a >1h cadence,
/// or empty for a cadence a dynamic self-wake can already honor (<=1h, i.e. <=3600s).
fn long_cadence_cron_clause(interval: &str) -> String {
    let secs = match parse_interval_secs(interval) {
        Some(s) if s > 3600 => s,
        _ => return String::new(),
    };
    // A clean whole-hour cadence maps to a concrete cron (`0 */H * * *`); anything else gets the generic form
    // (cron is minute-granular, so a non-whole-hour cadence over an hour has no tidy expression to suggest).
    let example = if secs % 3600 == 0 {
        format!("'0 */{} * * *'", secs / 3600)
    } else {
        "a fixed-interval cron matching your cadence".to_string()
    };
    format!(
        " NOTE on your >1h idle cadence: a dynamic /loop self-wake is hard-clamped to a ONE-HOUR maximum, so \
         once your queue is drained and inbox empty you CANNOT hold a cadence longer than an hour by \
         self-re-arming — a dynamic next-wake would pin you to hourly ticks no matter what interval you set. \
         To actually rest at your {interval} cadence, establish a FIXED-INTERVAL /loop cron with CronCreate \
         ({example}) and then do NOT also self-re-arm the dynamic wake: the cron fires your next tick on \
         schedule, and a dynamic re-arm alongside it just fights the cron until the one-hour clamp wins. The \
         event-wake still revives you immediately on a routed message or a new assignment, so the cron sets \
         only your empty-idle floor, never your revival latency."
    )
}

/// Build the SELF-DISCOVERY kickoff prompt for a board-native agent: it fetches its own charter from the
/// board (nothing is injected) and starts a WORK-CONSERVING loop. Pure so the prompt is unit-tested.
///
/// The loop is dynamic (`/loop` with NO fixed interval) so the agent self-paces via its own next-wake
/// decision instead of sleeping a fixed period regardless of pending work. Each tick the agent drains its
/// inbox, does one unit, then gates the next wake on work-present: it keeps looping soon while it holds
/// open assigned tasks or unread messages, and only falls back to the long `interval` idle cadence once its
/// assigned queue is drained AND its inbox is empty — so an agent with assigned work never idle-sleeps. When
/// that idle cadence exceeds one hour, the dynamic self-wake cannot schedule it (it clamps to a 1h maximum),
/// so the kickoff adds `long_cadence_cron_clause` guidance to hold the rest cadence off a CronCreate cron
/// instead of a self-re-arm (task_765 Tier A).
fn build_kickoff(
    agent: &str,
    workdir: &str,
    interval: &str,
    operator: Option<&str>,
    reactive: bool,
    proactive_ownership: bool,
) -> String {
    // The operator-blocked dashboard convention (operator seq-2292, re-pointed task_936) applies only when this
    // deployment names an operator (config.operator_id); a generic fleet with no designated operator omits it.
    // The id is interpolated, never hard-coded, so the public fleet code carries no operator name (task_611).
    // task_936: an EARLIER wording here told the agent to ALSO assign the task to the operator and stash
    // metadata.blocked_owner for a later reassign-back — that contradicts the owner-held model (operator-tenet
    // 10: the operator never OWNS a task/doc, only approves/answers it) and was the root cause of a recurring
    // mis-assign-to-operator trip a coordinator had to hand-correct every time. The dashboard signal is the
    // blocked_on=operator FILTER, not an assignment, so ownership never needs to change hands at all.
    let operator_clause = match operator {
        Some(op) => format!(
            " If the block is ON THE OPERATOR specifically, keep the task OWNER-HELD — your assignee stays \
             YOU, never '{op}' — with blocked_on naming kind=operator and a note on what you need; the \
             dashboard '{op}' reads is the blocked_on=operator FILTER, so it surfaces there without you \
             giving up ownership. NEVER assign the task to '{op}'. When '{op}' answers, clear the block and \
             continue — no reassignment, since you never gave ownership up. OPERATOR-BLOCK NEEDS A QUESTION \
             (task_1150): to set blocked_on=operator you MUST first attach an actual ask — pose_question with \
             routed_to=operator and blocking=true for the decision you need — THEN set blocked_on=operator; a \
             bare operator-block with no open routed question is NOT a valid block. OPERATOR COMMS GO THROUGH \
             CONCIERGE (task_1164): do NOT send_message or DM a human identity directly (including '{op}') — a \
             person's inbox is unmonitored, so your message is NOT seen; to reach the operator, pose_question \
             with routed_to=operator for a decision, or hand it to concierge."
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
    // task_1138 PROACTIVE-OWNERSHIP override: an agent that must continuously BUILD a deliverable (a promo-assist
    // owning a candidate's case) is not a wake-check-sleep worker — its charter defines a STANDING production
    // unit that is ALWAYS actionable, so the work-conserving "idle when no assigned task" default would wrongly
    // let it sleep. Force the loop to treat "nothing acutely urgent" as the signal to ADVANCE the standing unit,
    // never to idle, and to go passive ONLY once the charter's work-accomplished sleep-gate is met (the charter
    // states the precondition; the loop enforces that it is not time-gated). The interval is then only the
    // passive-monitor floor it drops to AFTER the gate holds. Off for a normal worker.
    let tick = if proactive_ownership {
        format!(
            "{tick} PROACTIVE-OWNERSHIP OVERRIDE: your charter defines a STANDING production unit you must \
             advance EVERY cycle (you are not a wake-check-sleep responder). You have actionable work WHENEVER \
             that standing unit is not yet complete, so do NOT fall back to the idle cadence and do NOT go \
             passive merely because you hold no assigned task and your inbox is drained — treat 'nothing acutely \
             urgent' as the signal to advance the standing unit. You may drop to the passive-monitor cadence \
             ONLY once your charter's work-accomplished gate is satisfied (the charter states the precondition, \
             e.g. a significant corpus examined AND a substantial deliverable built); until then keep cycling \
             and never idle-sleep. The gate is WORK-ACCOMPLISHED, not elapsed time."
        )
    } else {
        tick
    };
    // A >1h idle cadence cannot be held by the dynamic self-wake (it clamps to 1h) — append the cron-cadence
    // escalation so a long-rest monitor rests off a CronCreate cron instead of idle-polling hourly (task_765).
    let tick = format!("{tick}{}", long_cadence_cron_clause(interval));
    format!(
        "You are the fleet agent '{agent}', running UNATTENDED. Your task-board MCP tools are available in \
         this session. Call register_agent with agent_id '{agent}' once (idempotent) so your board record \
         exists, then get_agent '{agent}' to read your OWN charter + metadata from the board and follow that \
         charter as your role. IMPORTANT — the board does NOT bind your session: each call may reach a fresh, \
         unbound board, so register_agent does NOT make later id-less calls work. Pass your identity \
         EXPLICITLY on every board call THAT TAKES ONE, and note that each such tool NAMES the identity field \
         DIFFERENTLY — agent_id on check_notifications / set_status / get_messages, from_agent on send_message, \
         author on comment_task / comment_document, actor on update_task, created_by on create_task (all = \
         '{agent}'). But PURE READ / FILTER tools take NO caller identity: list_tasks, get_task, get_project, \
         get_document, get_channel_posts, list_agents and the like REJECT an agent_id field with a hard \
         'unknown field agent_id' deserialize error — filter list_tasks by assignee '{agent}', and read the \
         rest by the OBJECT id (task_id / project_id / document id), never a caller id. Passing the WRONG field \
         — agent_id to comment_task (which takes author), or agent_id onto a pure read like list_tasks / \
         get_task (which take none) — is either silently accepted recording the actor as null (so you wake on \
         your OWN comment) or hard-rejected, costing a failed call every tick; use each tool's own field, and \
         pass NO identity on a pure read. The board defaults identity to null and a \
         call that omits it fails with 'no identity for this session'. Never PRECOMPUTE or guess a task id: \
         reference a task only by the id create_task RETURNS, because concurrent creation on the shared board \
         can hand a guessed next-id to a DIFFERENT agent's task. TYPED REFERENCES (task_584): whenever you \
         WRITE a task or PR/issue reference into any board body (a comment, a message, or a channel post), \
         spell it as a TYPED id — 'task_N' for a board task, or 'owner/repo#N' for a GitHub issue/PR — and \
         NEVER a bare '#N': the board hard-rejects a bare '#N' in posted content, so a bare ref costs you a \
         reword-and-retry every time. That typed-ref rule is for CONTENT you WRITE (bodies); an id PARAMETER is \
         the opposite — a tool's id argument (channel_id, task_id, project_id, document id) takes the BARE \
         INTEGER (e.g. 6), NEVER a 'channel_N' / 'task_N' token and NEVER a quoted string, so do not \
         typed-prefix or quote an id argument. \
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
         SHELL SAFETY (task_1107): never `pkill -f` / `pgrep -f` a pattern that can appear in your OWN argv — \
         it self-matches the issuing shell and silently kills it (exit 143/144, no output) and has corrupted a \
         live operation; use `fleet safe-pkill` (it excludes your pid, ancestors, and process group), or kill \
         by a captured PID, and note that `timeout` already bounds a foreground process. \
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
         in_progress task is a status-honesty violation the watchdog flags and re-arms. SELF-CLOSE \
         (task_604): when a task ASSIGNED TO YOU is complete — the deliverable is shipped and verified — \
         set its status to done YOURSELF; never leave a finished task sitting in todo/in_progress with a \
         'recommend closing' comment and wait for the proposer/triage to flip it, which trips the \
         idle-nudge sweep hours later and forces another agent to do it for you. Exception: if you were \
         DELEGATED to BUILD a task you do not OWN (a multi-owner or someone-else-shaped task), comment \
         'complete, ready to close' and the owner/shaper closes it PROMPTLY, same tick — never let it \
         linger either way. CORE TENETS — fleet contract doc_3330 (its CORE section governs how you reason \
         and how you handle a missing capability) is the AUTHORITATIVE source, never a paraphrase: before \
         your first action this session you MUST get_document(doc_3330) and read it in full, then operate by \
         it. Each session check doc_3330's current approved version; if it changed since you last read it, \
         re-read it in full before acting. You work \
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
    /// The agent still holds `open` actionable (todo/in_progress, non-blocked) board assignments and `--force`
    /// was not given — refuse so dispatched work is never stranded on a stood-down agent (task_786).
    RefuseHoldsAssignments { open: usize },
    /// A turn is in flight and `--force` was not given — refuse so a working agent is never killed.
    RefuseBusy,
    /// Set the board status offline, then kill the live window (stops the loop).
    OfflineAndKill,
    /// Already windowless — set the board status offline only, so up-board leaves it stood down.
    OfflineOnly,
}

/// task_786 corrective predicate: is a board-native agent STRANDED — stood down (board `offline`) while it
/// still holds open actionable assignments (todo/in_progress, non-blocked, as counted by `open_task_count`)?
/// Such an agent has no live loop to work the tasks, and because it is windowless a watchdog WAKE cannot
/// revive it (which is why the #506 at-rest re-arm, built on `rearm_candidate`'s pane wake, does not cover
/// this case — the task_311 class: cr-reviewer sat stood down ~16h still holding a dispatched CR review). It
/// must be spun up or its tasks reassigned. Pure so the selection is unit-testable without a live board.
fn agent_is_stranded(stood_down: bool, open_assignments: usize) -> bool {
    stood_down && open_assignments > 0
}

/// task_794 default backlog-depth threshold: an agent carrying MORE than this many open actionable
/// assignments is a hire/route signal to the PM. Overridable per sweep via `CDZ_BACKLOG_DEPTH`.
const BACKLOG_DEPTH_DEFAULT: usize = 2;

/// task_794 predicate: is a RUNNING agent's open backlog deep enough to signal the PM for a helper? A
/// stood-down agent is excluded — its unworked queue is the task_786 STRANDED signal (spin it up / reassign),
/// a different remedy than hiring; counting it here would double-signal. Pure so the threshold is
/// unit-testable without a live board.
fn backlog_over_depth(stood_down: bool, open_assignments: usize, threshold: usize) -> bool {
    !stood_down && open_assignments > threshold
}

fn spin_down_action(
    is_native: bool,
    has_window: bool,
    is_working: bool,
    open_assignments: usize,
    force: bool,
) -> SpinDownAction {
    if !is_native {
        return SpinDownAction::NotBoardNative;
    }
    // task_786: an agent holding dispatched work must not be left stood down (the task_311 class — cr-reviewer
    // was spun down ~16h still holding a CR review). Refuse BEFORE the busy/window checks so the operator sees
    // the reassign requirement first; --force overrides (stand down anyway, operator accepts the open tasks).
    if open_assignments > 0 && !force {
        return SpinDownAction::RefuseHoldsAssignments {
            open: open_assignments,
        };
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
/// `cargo xtask fleet remove`), an agent still holding open board assignments (task_786 — so dispatched work
/// is never stranded on a stood-down agent), and a busy pane; `--force` overrides the last two. Sets offline
/// BEFORE the kill so an interleaved `up-board` cannot see it online-but-windowless and relaunch it.
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
    // task_786: count the agent's OPEN actionable assignments (todo/in_progress, non-blocked). FAIL-OPEN — if
    // the board /tasks query hiccups we warn and proceed rather than wedge spin-down on a transient error
    // (connect + get_agent already succeeded above, so this is a narrow window).
    let open_assignments = match board.open_task_count(agent) {
        Ok(n) => n,
        Err(e) => {
            eprintln!(
                "  ! could not verify open assignments ({e}) — proceeding without the stranding guard"
            );
            0
        }
    };
    let action = spin_down_action(is_native, has_window, is_working, open_assignments, force);

    println!(
        "spin-down '{agent}' ({}):",
        if apply { "APPLY" } else { "dry-run" }
    );
    match action {
        SpinDownAction::NotBoardNative => {
            eprintln!(
                "  ✗ '{agent}' is NOT board-native (metadata.native != true) — spin-down manages board-native \
                 agents only. A file-hub agent stands down via `cargo xtask fleet remove {agent}`."
            );
            std::process::exit(1);
        }
        SpinDownAction::RefuseHoldsAssignments { open } => {
            eprintln!(
                "  ✗ '{agent}' still holds {open} open assignment(s) (actionable todo/in_progress board \
                 task(s)) — refusing so dispatched work is not stranded on a stood-down agent. Reassign or \
                 close them first (list: `fleet` board tasks assigned to '{agent}'), or pass --force to stand \
                 it down anyway (the open tasks stay assigned to it until you reassign them)."
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

/// Pure selection for the fleet-wide spin-down (migration wind-down): of all board-native agent ids, the ones
/// to CONSIDER standing down — every native agent NOT on the stay-up `except` set, sorted for a stable roster.
/// The per-agent task_786/busy guard still applies downstream; this is only the except-filter. Pure so the
/// selection is unit-testable without a live board.
fn batch_targets(
    all_native: &[String],
    except: &std::collections::BTreeSet<String>,
) -> Vec<String> {
    let mut out: Vec<String> = all_native
        .iter()
        .filter(|id| !except.contains(*id))
        .cloned()
        .collect();
    out.sort();
    out.dedup();
    out
}

/// `fleet spin-down --all --except <csv>`: orderly fleet-wide wind-down. Connects ONCE, lists the board-native
/// roster, and runs the per-agent `spin_down_action` decision across every native agent except the stay-up
/// list — reporting a READY/REFUSED roster (the live who's-parked checklist) and, with `--apply`, standing down
/// the READY ones. Each agent is held to the SAME guard as single spin-down (task_786 holds-assignments + busy
/// are REFUSED unless `--force`), so a routine wind-down self-regulates: cleanly-parked agents stand down,
/// anyone still mid-work is left running for the next pass. `--force` is an operator-directed hard stop only.
fn spin_down_all(except_csv: Option<&str>, apply: bool, force: bool) {
    let board = board::Board::connect().unwrap_or_else(|e| {
        eprintln!("fleet spin-down --all: {e}");
        std::process::exit(1);
    });
    let agents = board.list_agents().unwrap_or_else(|e| {
        eprintln!("fleet spin-down --all: {e}");
        std::process::exit(1);
    });
    let except: std::collections::BTreeSet<String> = except_csv
        .unwrap_or("")
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    let native: Vec<String> = native_agent_ids(&agents).into_iter().collect();
    let targets = batch_targets(&native, &except);
    let session = board_session();
    let windows = tmux_window_names(&session);

    println!(
        "spin-down --all ({}): {} board-native agent(s), {} kept up, {} candidate(s){}",
        if apply { "APPLY" } else { "dry-run" },
        native.len(),
        except.len(),
        targets.len(),
        if force {
            " [--force: HARD STOP, overrides the holds-work/busy guard]"
        } else {
            ""
        }
    );
    if !except.is_empty() {
        let mut keep: Vec<&String> = except.iter().collect();
        keep.sort();
        println!(
            "  keep-up: {}",
            keep.iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }

    // task_1032: raise the fleet-quiesce sentinel BEFORE parking any agent, so the nudge-stale daemon is silent
    // for the whole wind-down (it must not nudge the owners this pass is about to park, nor route their stale
    // work to the router mid-cutover). `spin-up-all`'s final wave clears it; the TTL backstop clears a sentinel
    // a crashed cutover leaves behind. Apply-only — a dry run must not touch the daemon's live state.
    if apply {
        set_fleet_quiesce();
    }

    let (mut ready, mut refused, mut applied) = (0usize, 0usize, 0usize);
    for id in &targets {
        let has_window = windows.iter().any(|w| w == id);
        let is_working = has_window && window_is_working(&session, id);
        let open = board.open_task_count(id).unwrap_or(0);
        match spin_down_action(true, has_window, is_working, open, force) {
            SpinDownAction::RefuseHoldsAssignments { open } => {
                refused += 1;
                println!(
                    "  REFUSED  {id} — holds {open} open assignment(s) (still mid-work; park them first)"
                );
            }
            SpinDownAction::RefuseBusy => {
                refused += 1;
                println!("  REFUSED  {id} — turn in flight (pane busy)");
            }
            SpinDownAction::NotBoardNative => {} // filtered to native already; unreachable in practice
            action @ (SpinDownAction::OfflineAndKill | SpinDownAction::OfflineOnly) => {
                ready += 1;
                let kind = if matches!(action, SpinDownAction::OfflineAndKill) {
                    "offline+kill window"
                } else {
                    "offline only (windowless)"
                };
                if !apply {
                    println!("  READY    {id} — would {kind}");
                    continue;
                }
                // Offline FIRST (closes the up-board relaunch race), then kill the window if live.
                let msg = format!(
                    "Spun down via `fleet spin-down --all` (migration wind-down, resumable). `fleet spin-up {id} --apply` revives it."
                );
                if let Err(e) = board.set_status(id, "offline", &msg) {
                    println!("  ERROR    {id} — set offline failed: {e} (left running)");
                    continue;
                }
                if matches!(action, SpinDownAction::OfflineAndKill) {
                    let target = format!("{session}:{id}");
                    let _ = std::process::Command::new("tmux")
                        .args(["kill-window", "-t", &target])
                        .status();
                }
                applied += 1;
                println!("  PARKED   {id} — {kind} (resumable)");
            }
        }
    }
    println!(
        "  summary: {ready} ready, {refused} refused (still mid-work -- re-run after they park){}",
        if apply {
            format!(", {applied} parked this pass")
        } else {
            String::new()
        }
    );
    if !apply && ready > 0 {
        println!("  (dry-run — re-run with --apply to stand down the READY agents)");
    }
}

/// Pure selection for `fleet spin-up-all`: the reconstitution candidates. Given the board-native agents that
/// are currently DOWN (offline / no live window) and a stay-as-is exclude set, return the sorted, deduped,
/// optionally wave-LIMITED list to bring back up. `limit` caps the batch to the first N (sorted) candidates so
/// a mass reconstitution runs in bounded waves rather than a thundering herd; `None` brings up everyone down.
/// Pure — unit-tested, no board/tmux.
fn reconstitute_targets(
    down_native: &[String],
    except: &std::collections::BTreeSet<String>,
    limit: Option<usize>,
) -> Vec<String> {
    let mut out: Vec<String> = down_native
        .iter()
        .filter(|id| !except.contains(*id))
        .cloned()
        .collect();
    out.sort();
    out.dedup();
    if let Some(n) = limit {
        out.truncate(n);
    }
    out
}

/// `fleet spin-up-all --except <csv> [--limit N]`: orderly fleet-wide reconstitution — the reverse of
/// [`spin_down_all`]. Connects ONCE, lists the board-native roster, and brings up every native agent that is
/// currently DOWN (no live tmux window) except a stay-as-is list, in a bounded WAVE (`--limit N` caps the
/// pass). A running agent (live window) is skipped, never relaunched. Reports the candidate roster by default;
/// `--apply` launches this wave. Each candidate is spun up as a SUBPROCESS (`fleet spin-up <id> --apply`), so a
/// single agent's launch failure (spin_up signals it via process::exit) is contained in its child and never
/// aborts the wave — the same isolation the watchdog's revive-stranded path uses (task_818).
fn spin_up_all(except_csv: Option<&str>, limit: Option<usize>, include_held: bool, apply: bool) {
    let board = board::Board::connect().unwrap_or_else(|e| {
        eprintln!("fleet spin-up-all: {e}");
        std::process::exit(1);
    });
    let agents = board.list_agents().unwrap_or_else(|e| {
        eprintln!("fleet spin-up-all: {e}");
        std::process::exit(1);
    });
    let except: std::collections::BTreeSet<String> = except_csv
        .unwrap_or("")
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    let native: Vec<String> = native_agent_ids(&agents).into_iter().collect();
    let session = board_session();
    let windows = tmux_window_names(&session);
    // DOWN = board-native with NO live tmux window (parked / not currently running). Filtering by live window
    // (not by board status) is robust against status drift and means a running agent is never relaunched.
    let down_native: Vec<String> = native
        .iter()
        .filter(|id| !windows.iter().any(|w| w == *id))
        .cloned()
        .collect();
    let up = native.len().saturating_sub(down_native.len());
    // task_1037: launchability filter. native_agent_ids keys only on metadata.native, so a down candidate can be
    // a staged / off-host / launch-gated / charter-deferred agent an auto-launch must NOT bring up. Partition the
    // down set into HELD (reported with a reason, never swept into a wave unless --include-held) and launchable,
    // reusing the SAME markers the watchdog + up-board reconcile honor (spin_up_hold_reason) so the paths cannot
    // disagree. The staged board-core-helper was mistakenly launched here before this existed (task_1037).
    let this_host = this_host();
    let meta_of = |id: &str| -> Option<serde_json::Value> {
        agents
            .iter()
            .find(|a| a.get("id").and_then(serde_json::Value::as_str) == Some(id))
            .and_then(|a| a.get("metadata").cloned())
    };
    let mut held: Vec<(String, &'static str)> = Vec::new();
    let launchable_down: Vec<String> = if include_held {
        down_native.clone()
    } else {
        down_native
            .iter()
            .filter(
                |id| match spin_up_hold_reason(meta_of(id).as_ref(), &this_host) {
                    Some(reason) => {
                        held.push(((*id).clone(), reason));
                        false
                    }
                    None => true,
                },
            )
            .cloned()
            .collect()
    };
    held.sort();
    let targets = reconstitute_targets(&launchable_down, &except, limit);

    println!(
        "spin-up-all ({}): {} board-native, {} already up, {} down ({} launchable, {} held), {} left as-is, {} this wave{}{}",
        if apply { "APPLY" } else { "dry-run" },
        native.len(),
        up,
        down_native.len(),
        launchable_down.len(),
        held.len(),
        except.len(),
        targets.len(),
        limit.map(|n| format!(" [--limit {n}]")).unwrap_or_default(),
        if include_held {
            " [--include-held: holds launched too]"
        } else {
            ""
        }
    );
    if !except.is_empty() {
        let mut keep: Vec<&String> = except.iter().collect();
        keep.sort();
        println!(
            "  leave-as-is: {}",
            keep.iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    if !held.is_empty() {
        println!(
            "  held (not launchable by default — use `fleet spin-up <id> --apply` or --include-held):"
        );
        for (id, reason) in &held {
            println!("    ⊘ HELD {id} — {reason}");
        }
    }

    let self_bin = std::env::current_exe()
        .ok()
        .and_then(|p| p.to_str().map(str::to_string))
        .unwrap_or_else(|| "fleet".to_string());
    let (mut launched, mut failed) = (0usize, 0usize);
    for id in &targets {
        if !apply {
            println!("  READY    {id} — would spin up");
            continue;
        }
        // Subprocess so a per-agent spin_up failure (process::exit) is contained in the child, not this wave.
        match std::process::Command::new(&self_bin)
            .args(["spin-up", id, "--apply"])
            .status()
        {
            Ok(s) if s.success() => {
                launched += 1;
                println!("  LAUNCHED {id}");
            }
            Ok(_) => {
                failed += 1;
                eprintln!("  FAILED   {id} — spin-up exited nonzero (left down; re-run to retry)");
            }
            Err(e) => {
                failed += 1;
                eprintln!("  ERROR    {id} — could not launch spin-up: {e}");
            }
        }
    }
    if apply {
        println!("  summary: {launched} launched, {failed} failed this wave");
        // task_1037: measure remaining-to-reconstitute over the LAUNCHABLE set — a HELD agent (staged/off-host/
        // gated/deferred) is not a candidate, so it must not keep the fan-out "incomplete" or hold the quiesce.
        let down_non_except = launchable_down.len().saturating_sub(except.len());
        if let Some(n) = limit
            && down_non_except > n
        {
            println!(
                "  (more down than this wave's --limit — pause, confirm health, then re-run for the next wave)"
            );
        }
        // task_1032: once the fan-out completes (this was the final/only wave), lower the fleet-quiesce sentinel
        // so the nudge-stale daemon resumes chasing stale work. A partial wave leaves it set — the fleet is
        // still mostly parked; the next (final) wave, or the TTL backstop, clears it.
        if spin_up_all_completes_fanout(down_non_except, limit) {
            clear_fleet_quiesce();
        }
    } else if !targets.is_empty() {
        println!("  (dry-run — re-run with --apply to launch this wave)");
    }
}

/// task_752: the pure selection for `fleet bounce-session`. A bounce RELAUNCHES a board-native agent's session
/// so its MCP client reconnects and refetches tools/list (the stale-cached-tools/list fix). Pure so the
/// decision is unit-testable without a live board/tmux.
#[derive(Debug, PartialEq, Eq)]
enum BounceAction {
    /// Not a board-native agent — refuse (only board-native agents launch via the fleet, and the relaunch path
    /// is the board-native one; a file-hub agent is relaunched via `cargo xtask fleet`).
    NotBoardNative,
    /// No live tmux window — nothing to bounce (the session is not running). Use `fleet spin-up` to start it.
    NoWindow,
    /// A turn is in flight and `--force` was not given — refuse so a reconnect never interrupts a live turn.
    RefuseBusy,
    /// Bounced within the cooldown window and `--force` was not given — skip so a healthy session is not
    /// thrashed by repeated relaunches.
    Cooldown,
    /// Kill the window and relaunch (the fresh session reconnects + refetches tools/list).
    Bounce,
}

/// Pure selection for a session-bounce (task_752). Order mirrors [`spin_down_action`]: board-native first,
/// then window-existence (a bounce needs a live session to relaunch), then the busy fence (never reconnect
/// through a live turn), then the cooldown fence (never thrash a healthy session). `--force` overrides the
/// busy + cooldown refusals; it does NOT make a non-native or windowless agent bounceable (nothing to
/// relaunch). Pure — unit-tested.
fn bounce_action(
    is_native: bool,
    has_window: bool,
    is_working: bool,
    on_cooldown: bool,
    force: bool,
) -> BounceAction {
    if !is_native {
        return BounceAction::NotBoardNative;
    }
    if !has_window {
        return BounceAction::NoWindow;
    }
    if is_working && !force {
        return BounceAction::RefuseBusy;
    }
    if on_cooldown && !force {
        return BounceAction::Cooldown;
    }
    BounceAction::Bounce
}

/// task_752 default session-bounce cooldown: at most one bounce per agent per this window, so a repeated
/// trigger (or a future watchdog sweep) never thrash-relaunches a healthy session. `CDZ_BOUNCE_COOLDOWN_SECS`
/// tunes it. A bounce is disruptive (a full session relaunch), so the window is generous.
const BOUNCE_COOLDOWN_SECS: u64 = 30 * 60;

/// task_752 cooldown gate (pure, unit-testable), mirroring [`hire_signal_on_cooldown`]: a bounce is on cooldown
/// when the last one for this agent is within `cooldown_secs`. No floor — the cooldown IS the window. `None`
/// (never bounced) is not on cooldown.
fn bounce_on_cooldown(last_bounce: Option<u64>, now: u64, cooldown_secs: u64) -> bool {
    last_bounce.is_some_and(|last| now.saturating_sub(last) < cooldown_secs)
}

/// The per-agent last-bounce stamp path: `<hub>/.claude/fleet/watchdog/<name>.bounce` (contents = unix secs).
fn bounce_stamp_path(fleet: &Fleet, name: &str) -> PathBuf {
    fleet.root.join("watchdog").join(format!("{name}.bounce"))
}

/// Read an agent's last-bounce unix time; `None` on an absent/unparseable stamp (never bounced).
fn read_bounce_stamp(fleet: &Fleet, name: &str) -> Option<u64> {
    std::fs::read_to_string(bounce_stamp_path(fleet, name))
        .ok()?
        .trim()
        .parse()
        .ok()
}

/// Record that an agent was just bounced at `now` (best-effort — a write failure only means the cooldown is not
/// enforced for that agent on the next trigger).
fn write_bounce_stamp(fleet: &Fleet, name: &str, now: u64) {
    let p = bounce_stamp_path(fleet, name);
    if let Some(dir) = p.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(p, now.to_string());
}

/// The per-agent launch-rev stamp path: `<hub>/.claude/fleet/watchdog/<name>.launchrev` (contents = the
/// `FLEET_BUILD_REV` of the fleet binary that launched the agent's session). task_752: the watchdog-driven
/// stale-session selection reads this to tell whether a running session connected under an OLDER binary (so
/// its cached MCP tools/list predates the current deploy) and is a bounce candidate.
fn launch_rev_stamp_path(fleet: &Fleet, name: &str) -> PathBuf {
    fleet
        .root
        .join("watchdog")
        .join(format!("{name}.launchrev"))
}

/// Read the build rev an agent's session was launched under; `None` on an absent/empty stamp (a session that
/// predates launch-rev stamping, or was launched outside `spin-up`).
fn read_launch_rev(fleet: &Fleet, name: &str) -> Option<String> {
    let s = std::fs::read_to_string(launch_rev_stamp_path(fleet, name)).ok()?;
    let t = s.trim();
    (!t.is_empty()).then(|| t.to_string())
}

/// Record that an agent's session was launched under `rev` (best-effort — a write failure only means the next
/// stale-session sweep treats it as unknown-rev, i.e. a bounce candidate, which is self-healing).
fn write_launch_rev(fleet: &Fleet, name: &str, rev: &str) {
    let p = launch_rev_stamp_path(fleet, name);
    if let Some(dir) = p.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(p, rev);
}

/// Stamp the CURRENT fleet-binary build rev as the launch rev for `agent` — called at every `spin-up`
/// launch-success site so a later (post-redeploy) binary can tell a stale session from a fresh one (task_752).
/// A bounce relaunches through `spin-up`, so a bounced session re-stamps the new rev and drops off the
/// stale-candidate list.
fn stamp_launch_rev(agent: &str) {
    write_launch_rev(&Fleet::resolve(), agent, env!("FLEET_BUILD_REV"));
}

/// Pure stale-session selection for the watchdog-driven bounce sweep (task_752). A running board-native
/// session is STALE — a bounce candidate to refresh its cached MCP tools/list — when it was launched under a
/// build rev other than the current one. `None` (no recorded launch rev: a session that predates stamping or
/// was launched outside `spin-up`) counts as STALE: bouncing it once establishes the stamp and refreshes its
/// tools/list, and the cooldown fence plus the report-only-first default keep that one-time catch-up safe.
/// Pure — unit-tested without a board or tmux.
fn stale_launch_for_deploy(launch_rev: Option<&str>, current_rev: &str) -> bool {
    match launch_rev {
        None => true,
        Some(rev) => rev != current_rev,
    }
}

/// `fleet bounce-session`: relaunch ONE board-native agent's session so its MCP client reconnects to the
/// current board server process and refetches tools/list (task_752 — the stale-cached-tools/list fix). A
/// long-lived session caches tools/list at connect, and a redeploy that ships a new tool often does NOT drop
/// the connection (a tunnel masks it), so a tool like `pose_question` stays invisible to the standing session
/// until it reconnects — which must be driven from outside. This kills the agent's tmux window and relaunches
/// it via the normal spin-up path; the fresh session reconnects and sees every shipped tool. The agent resumes
/// from the board (board-native agents keep charter + task state on the board, not in-session), so a bounce is
/// non-destructive to its work. Report-only by default; `--apply` performs the kill + relaunch, fenced so it
/// never interrupts a live turn (busy pane) or thrashes a healthy session (cooldown) — `--force` overrides
/// those two fences.
fn bounce_session(fleet: &Fleet, agent: &str, apply: bool, force: bool) {
    let board = board::Board::connect().unwrap_or_else(|e| {
        eprintln!("fleet bounce-session: {e}");
        std::process::exit(1);
    });
    let rec = board.get_agent(agent).unwrap_or_else(|e| {
        eprintln!("fleet bounce-session: {e}");
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
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let cooldown_secs = std::env::var("CDZ_BOUNCE_COOLDOWN_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(BOUNCE_COOLDOWN_SECS);
    let on_cooldown = bounce_on_cooldown(read_bounce_stamp(fleet, agent), now, cooldown_secs);
    let action = bounce_action(is_native, has_window, is_working, on_cooldown, force);

    println!(
        "bounce-session '{agent}' ({}):",
        if apply { "APPLY" } else { "dry-run" }
    );
    match action {
        BounceAction::NotBoardNative => {
            eprintln!(
                "  ✗ '{agent}' is NOT board-native (metadata.native != true) — bounce-session relaunches \
                 board-native agents only. A file-hub agent reconnects via a `cargo xtask fleet` relaunch."
            );
            std::process::exit(1);
        }
        BounceAction::NoWindow => {
            eprintln!(
                "  ✗ '{agent}' has no live tmux window in session '{session}' — nothing to bounce (the session \
                 is not running). Start it with `fleet spin-up {agent} --apply`."
            );
            std::process::exit(1);
        }
        BounceAction::RefuseBusy => {
            eprintln!(
                "  ✗ '{agent}' has a turn IN FLIGHT (its pane is working) — refusing so a reconnect never \
                 interrupts a live turn. Re-run when it's idle, or pass --force to bounce it anyway."
            );
            std::process::exit(1);
        }
        BounceAction::Cooldown => {
            eprintln!(
                "  ✗ '{agent}' was bounced within the last {cooldown_secs}s (cooldown) — skipping so a healthy \
                 session is not thrashed by repeated relaunches. Pass --force to bounce it anyway."
            );
            std::process::exit(1);
        }
        BounceAction::Bounce => println!(
            "  plan: kill tmux window {session}:{agent}, then relaunch via spin-up (the fresh session reconnects \
             to the board MCP server + refetches tools/list){}",
            if force && is_working {
                " [--force: a live turn WILL be interrupted]"
            } else {
                ""
            }
        ),
    }
    println!(
        "  NON-DESTRUCTIVE to work: board-native agents keep charter + task state on the board, so the \
         relaunched session resumes from there."
    );
    if !apply {
        println!("  (dry-run — re-run with --apply to perform the kill + relaunch)");
        return;
    }
    // Kill FIRST, then relaunch: launch_board_agent (via spin_up) refuses when a window of the same name
    // already exists, so the window must be gone before the relaunch. A brief windowless gap is expected.
    let target = format!("{session}:{agent}");
    let killed = std::process::Command::new("tmux")
        .args(["kill-window", "-t", &target])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !killed {
        eprintln!(
            "  ✗ tmux kill-window {target} failed — NOT relaunching (a relaunch alongside a surviving window \
             would run two sessions). Remove the window manually, then re-run."
        );
        std::process::exit(1);
    }
    println!("  ✓ killed window {target}");
    // Stamp the cooldown BEFORE the relaunch: the bounce has committed (the window is gone), so the cooldown
    // must hold even if the relaunch below exits with a launch error — otherwise a retry could double-bounce.
    write_bounce_stamp(fleet, agent, now);
    // Relaunch through the normal spin-up path (reuses the full launch: workspace materialize is idempotent,
    // pre-trust, kickoff). The fresh session reconnects to the current board MCP server and refetches tools/list.
    println!("  relaunching via spin-up …");
    spin_up(agent, true);
    println!("  ✓ bounced '{agent}' — the fresh session will reconnect + refetch tools/list.");
}

/// `fleet bounce-stale`: the watchdog-driven, fleet-wide form of `bounce-session` (task_752). After a
/// redeploy, a long-lived session keeps serving the MCP tools/list it cached at connect — so a tool shipped
/// since (e.g. `pose_question`) stays invisible until the session reconnects. This sweeps every RUNNING
/// board-native agent and bounces the ones whose session was launched under an OLDER binary than the one
/// running this sweep (the per-agent launch-rev stamp vs this binary's `FLEET_BUILD_REV`). Report-only by
/// default (lists the stale sessions and why); `--apply` bounces each through the fenced per-agent
/// `bounce-session` path as a SUBPROCESS, so a single refusal (busy pane / cooldown) or a kill never aborts
/// the sweep — the same isolation `watchdog --revive-stranded` uses. `--force` passes through to each bounce.
fn bounce_stale(fleet: &Fleet, apply: bool, force: bool) {
    let board = board::Board::connect().unwrap_or_else(|e| {
        eprintln!("fleet bounce-stale: {e}");
        std::process::exit(1);
    });
    let roster = board.list_agents().unwrap_or_else(|e| {
        eprintln!("fleet bounce-stale: {e}");
        std::process::exit(1);
    });
    let current_rev = env!("FLEET_BUILD_REV");
    let windows = tmux_window_names(&board_session());
    // Bounce each candidate by re-invoking THIS binary (so the relaunch stamps the current rev) as a
    // subprocess, mirroring the --revive-stranded isolation: a per-agent failure never aborts the sweep.
    let self_bin = std::env::current_exe()
        .ok()
        .and_then(|p| p.to_str().map(str::to_string))
        .unwrap_or_else(|| "fleet".to_string());

    println!(
        "bounce-stale ({}): current fleet rev {current_rev} — scanning running board-native sessions for a stale tools/list",
        if apply { "APPLY" } else { "dry-run" }
    );

    let mut candidates = 0usize;
    let mut bounced = 0usize;
    for rec in &roster {
        let Some(id) = rec.get("id").and_then(serde_json::Value::as_str) else {
            continue;
        };
        let is_native = rec
            .get("metadata")
            .and_then(|m| m.get("native"))
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        if !is_native {
            continue; // only board-native agents launch (and relaunch) through the fleet
        }
        if !windows.iter().any(|w| w == id) {
            continue; // not running on this host — nothing to bounce (spin-up starts it fresh anyway)
        }
        let launch_rev = read_launch_rev(fleet, id);
        if !stale_launch_for_deploy(launch_rev.as_deref(), current_rev) {
            continue; // launched under the current rev — its tools/list is already fresh
        }
        candidates += 1;
        println!(
            "  STALE '{id}' (launched rev {} != current {current_rev})",
            launch_rev.as_deref().unwrap_or("unknown")
        );
        if !apply {
            continue;
        }
        let mut args = vec![
            "bounce-session".to_string(),
            id.to_string(),
            "--apply".to_string(),
        ];
        if force {
            args.push("--force".to_string());
        }
        match std::process::Command::new(&self_bin).args(&args).status() {
            Ok(st) if st.success() => bounced += 1,
            Ok(_) => eprintln!(
                "    (bounce-session '{id}' declined — likely a busy pane or cooldown; left running, next sweep retries)"
            ),
            Err(e) => eprintln!("    (could not run bounce-session '{id}': {e})"),
        }
    }

    if candidates == 0 {
        println!("  no stale sessions — every running board-native agent is on the current rev");
    } else if apply {
        println!(
            "  bounced {bounced}/{candidates} stale session(s); any shortfall was fenced (busy pane or cooldown) and retries next sweep"
        );
    } else {
        println!(
            "  {candidates} stale session(s) would be bounced — re-run with --apply (each still passes the per-agent busy + cooldown fences)"
        );
    }
}

/// A recognizable marker in the fleet-installed fmt pre-commit hook, so a re-install tells OUR hook (safe to
/// refresh) from a foreign one (never clobber).
const FMT_HOOK_MARKER: &str = "# fleet:fmt-warn";

/// The generic, repo-agnostic pre-commit hook `spin-up` installs into a materialized worktree's shared mirror
/// hooks dir. Two steps:
///
/// 1. A FAIL-OPEN rustfmt nudge, scoped per OWNING CRATE of the staged files rather than the whole workspace
///    (task_617: a bare `cargo fmt --all --check` tripped on every commit over a pre-existing fmt skew in an
///    unrelated crate — cry-wolf noise that trains agents to dismiss the warning unexamined, including a
///    genuine problem in their OWN staged files). Delegates to `fleet fmt-hook-run`; NEVER blocks a commit
///    (exit 0 unconditionally, ignoring `fleet`'s own exit code). Silence with FLEET_SKIP_FMT_HOOK=1.
/// 2. A FAIL-CLOSED content guardrail (task_782): `fleet scan-content --staged` blocks the commit when staged
///    content hits an internal marker. OPT-IN per host/repo via FLEET_CONTENT_TAXONOMY (the private marker
///    store, never committed to a public repo); where it is unset this step is a no-op, so a generic worktree
///    is unchanged. Bypass with FLEET_SKIP_CONTENT_HOOK=1 — the pre-commit is defense-in-depth and bypassable,
///    the CI required status check is the gate of record. The fmt silencer does NOT disable this step.
///
/// Both delegate to `fleet` subcommands (testable Rust, not bash) so the installed script stays tiny, and both
/// no-op without `fleet` on PATH. Kept generic (no cadenza- or fleet-specific checks) so it is correct for
/// every repo an agent's worktree may be.
fn fmt_precommit_hook_body() -> String {
    format!(
        "#!/usr/bin/env bash\n\
         {FMT_HOOK_MARKER} (installed by `fleet spin-up`; fail-open fmt nudge + fail-closed content guardrail)\n\
         [ \"${{FLEET_SKIP_FMT_HOOK:-}}\" != \"1\" ] && command -v fleet >/dev/null 2>&1 && fleet fmt-hook-run\n\
         if [ \"${{FLEET_SKIP_CONTENT_HOOK:-}}\" != \"1\" ] && [ -n \"${{FLEET_CONTENT_TAXONOMY:-}}\" ] && command -v fleet >/dev/null 2>&1; then\n\
         fleet scan-content --staged || exit 1\n\
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
            println!(
                "  fmt hook: foreign pre-commit at {} left untouched",
                hook.display()
            );
            return;
        }
        // Already exactly our current hook — nothing to do, stay quiet (the common case after first install).
        FmtHookAction::Refresh if existing.as_deref() == Some(body.as_str()) => return,
        FmtHookAction::Install | FmtHookAction::Refresh => {}
    }
    if let Err(e) = std::fs::create_dir_all(hooks_dir) {
        eprintln!(
            "  WARN: fmt hook not installed (mkdir {}: {e})",
            hooks_dir.display()
        );
        return;
    }
    if let Err(e) = std::fs::write(&hook, &body) {
        eprintln!(
            "  WARN: fmt hook not installed (write {}: {e})",
            hook.display()
        );
        return;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755));
    }
    println!(
        "  pre-commit hook: {} the fleet pre-commit (fmt nudge + content guardrail) at {}",
        if existing.is_some() {
            "refreshed"
        } else {
            "installed"
        },
        hook.display()
    );
}

/// Finds the crate that OWNS `file_dir` — the nearest ancestor at or below `repo_root` holding a Cargo.toml
/// with a `[package]` table, as opposed to a workspace-root manifest (`[workspace]`, no `[package]`). Falls
/// back to `repo_root/Cargo.toml` if none is found (e.g. a loose top-level .rs file in a single-crate repo).
/// `is_package_manifest` is injected so this stays pure/unit-testable without touching real files.
fn owning_manifest(
    repo_root: &Path,
    file_dir: &Path,
    is_package_manifest: impl Fn(&Path) -> bool,
) -> PathBuf {
    let mut dir = file_dir.to_path_buf();
    loop {
        if is_package_manifest(&dir.join("Cargo.toml")) {
            return dir.join("Cargo.toml");
        }
        if dir == repo_root {
            return repo_root.join("Cargo.toml");
        }
        match dir.parent() {
            Some(p) if p.starts_with(repo_root) => dir = p.to_path_buf(),
            _ => return repo_root.join("Cargo.toml"),
        }
    }
}

/// Whether `path` is a crate-level Cargo.toml (has a `[package]` table) rather than a workspace-root
/// manifest (`[workspace]` only) or no manifest at all.
fn is_cargo_package_manifest(path: &Path) -> bool {
    std::fs::read_to_string(path)
        .map(|s| s.lines().any(|l| l.trim() == "[package]"))
        .unwrap_or(false)
}

fn have_cargo() -> bool {
    std::process::Command::new("cargo")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Whether `file` (repo-root-relative, as `git diff --cached --name-only` reports it) has NO unstaged
/// changes — the working tree matches the index for this path. Only a file in this state is safe to
/// auto-reformat-on-disk-and-restage: otherwise `git add` would silently fold in edits the author never
/// staged, which the never-surprise-the-author git discipline this fleet runs on cannot allow.
fn is_fully_staged(repo_root: &Path, file: &str) -> bool {
    std::process::Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .args(["diff", "--quiet", "--", file])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// What `fleet fmt-hook-run` (invoked by the installed pre-commit hook — see [`fmt_precommit_hook_body`])
/// actually does: group the staged `.rs` files by their OWNING crate (never the whole workspace — the
/// task_617 bug), auto-format + re-stage the ones with no unstaged changes, and warn (fail-open, never
/// block) on any that are only partially staged, since those cannot be safely auto-restaged.
fn fmt_hook_run() {
    let Ok(root_out) = std::process::Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .output()
    else {
        return;
    };
    if !root_out.status.success() {
        return;
    }
    let repo_root = PathBuf::from(String::from_utf8_lossy(&root_out.stdout).trim());

    let Ok(staged_out) = std::process::Command::new("git")
        .arg("-C")
        .arg(&repo_root)
        .args([
            "diff",
            "--cached",
            "--name-only",
            "--diff-filter=ACM",
            "--",
            "*.rs",
        ])
        .output()
    else {
        return;
    };
    if !staged_out.status.success() || !have_cargo() {
        return;
    }
    let staged: Vec<String> = String::from_utf8_lossy(&staged_out.stdout)
        .lines()
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect();
    if staged.is_empty() {
        return;
    }

    let mut by_manifest: std::collections::BTreeMap<PathBuf, Vec<String>> =
        std::collections::BTreeMap::new();
    for file in staged {
        let file_dir = repo_root
            .join(&file)
            .parent()
            .unwrap_or(&repo_root)
            .to_path_buf();
        let manifest = owning_manifest(&repo_root, &file_dir, is_cargo_package_manifest);
        by_manifest.entry(manifest).or_default().push(file);
    }

    let mut warned = false;
    for (manifest, files) in by_manifest {
        let (safe, unsafe_files): (Vec<String>, Vec<String>) = files
            .into_iter()
            .partition(|f| is_fully_staged(&repo_root, f));

        if !safe.is_empty() {
            let _ = std::process::Command::new("cargo")
                .arg("fmt")
                .arg("--manifest-path")
                .arg(&manifest)
                .arg("--")
                .args(&safe)
                .status();
            let _ = std::process::Command::new("git")
                .arg("-C")
                .arg(&repo_root)
                .arg("add")
                .args(&safe)
                .status();
        }

        if !unsafe_files.is_empty() {
            // Silence cargo fmt's own --check diff output - only OUR actionable message below should print.
            let clean = std::process::Command::new("cargo")
                .arg("fmt")
                .arg("--manifest-path")
                .arg(&manifest)
                .arg("--check")
                .arg("--")
                .args(&unsafe_files)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .map(|s| s.success())
                .unwrap_or(true); // can't tell — don't cry wolf on an uncertain result
            if !clean {
                eprintln!(
                    "warn pre-commit: {} also ha{} unstaged changes, so rustfmt was not auto-applied (that \
                     would risk staging edits you did not stage) — format + stage {} yourself: cargo fmt \
                     --manifest-path {}",
                    unsafe_files.join(", "),
                    if unsafe_files.len() == 1 { "s" } else { "ve" },
                    if unsafe_files.len() == 1 {
                        "it"
                    } else {
                        "them"
                    },
                    manifest.display(),
                );
                warned = true;
            }
        }
    }
    if warned {
        eprintln!("  Silence: FLEET_SKIP_FMT_HOOK=1.");
    }
}

/// The `tmux new-window` argv for a board-native agent: opens window `agent` in `workdir` running `cmd`,
/// with the kickoff AND the agent identity placed in the window environment. `FLEET_AGENT` is exported into
/// the session env so the agent's task-board MCP config can carry it as the per-session `X-Fleet-Agent`
/// header the board forces its principal from (task_1039) — parallel to window.sh, which sets it for the
/// `up` launch path. The board-native launch here does NOT exec window.sh, so without this the header would
/// expand empty for every board-native agent (the common launch path) and the identity-force would never
/// fire. Pure so the env wiring is unit-tested without spawning tmux.
fn board_window_argv(
    session: &str,
    agent: &str,
    workdir: &str,
    kickoff: &str,
    cmd: &str,
) -> Vec<String> {
    vec![
        "new-window".into(),
        "-d".into(),
        "-t".into(),
        session.into(),
        "-n".into(),
        agent.into(),
        "-c".into(),
        workdir.into(),
        "-e".into(),
        format!("CDZ_KICKOFF={kickoff}"),
        "-e".into(),
        format!("FLEET_AGENT={agent}"),
        cmd.into(),
    ]
}

/// Open a tmux window running the agent's harness in `workdir` with a SELF-DISCOVERY kickoff (the agent
/// fetches its own charter from the board via its in-session MCP — nothing is injected). The launch command
/// is harness-specific (see [`build_launch_cmd`]); refuses to double-launch an existing same-named window.
/// The kickoff is passed via a tmux env var so no shell quoting can mangle it.
#[allow(clippy::too_many_arguments)]
fn launch_board_agent(
    agent: &str,
    workdir: &str,
    harness: &str,
    model: &str,
    effort: &str,
    interval: &str,
    devshell: bool,
    reactive: bool,
    proactive_ownership: bool,
) -> Result<String, String> {
    let session = board_session();
    if let Ok(out) = std::process::Command::new("tmux")
        .args(["list-windows", "-t", &session, "-F", "#W"])
        .output()
        && window_exists(&String::from_utf8_lossy(&out.stdout), agent)
    {
        return Err(format!(
            "a tmux window '{agent}' already exists in session '{session}' (already spun up?)"
        ));
    }
    let kickoff = build_kickoff(
        agent,
        workdir,
        interval,
        config::get().operator_id.as_deref(),
        reactive,
        proactive_ownership,
    );
    let cmd = build_launch_cmd(harness, model, effort, devshell.then_some(workdir))?;
    let argv = board_window_argv(&session, agent, workdir, &kickoff, &cmd);
    let argv_ref: Vec<&str> = argv.iter().map(String::as_str).collect();
    let status = std::process::Command::new("tmux")
        .args(&argv_ref)
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
    let Some(obj) = config.as_object_mut() else {
        return false;
    };
    let projects = obj
        .entry("projects")
        .or_insert_with(|| serde_json::json!({}));
    let Some(projects) = projects.as_object_mut() else {
        return false;
    };
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
    let mut v: serde_json::Value =
        serde_json::from_str(&raw).map_err(|e| format!("parse {cfg}: {e}"))?;
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
    println!(
        "{:<28} {:<10} {:<14} last_seen",
        "agent", "status", "liveness"
    );
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

/// The default state dir for regenerable fleet artifacts (the `dream-run` per-scope reports): `$XDG_STATE_HOME/
/// fleet` (fallback `$HOME/.local/state/fleet`). State, not config — reports are regenerable each run.
fn default_state_dir() -> std::path::PathBuf {
    std::env::var_os("XDG_STATE_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".local/state"))
        })
        .unwrap_or_else(|| std::path::PathBuf::from(".local/state"))
        .join("fleet")
}

/// The host-service ownership registry (task_1027) is a LOCAL, host-specific resource — NOT committed to this
/// public repo, because the real registry names internal host/service topology (task_1031 / task_934). It
/// lives at `$XDG_CONFIG_HOME/fleet/host-services.toml` (fallback `$HOME/.config/fleet/host-services.toml`),
/// injected by the deploy; the repo carries only `host-services.toml.example` (placeholder shape). An absent
/// file yields an empty registry + a hint pointing at the path, never an error.
fn host_services_path() -> std::path::PathBuf {
    fleet_local_config_dir().join("host-services.toml")
}

/// The local fleet config dir — `$XDG_CONFIG_HOME/fleet` (fallback `$HOME/.config/fleet`). Where the host-local
/// fleet resources live (the host-service registry, the fleet-quiesce sentinel): host-specific, deploy-injected,
/// never committed. `HOME`/`XDG_CONFIG_HOME` are OS-standard locators, not fleet knobs.
fn fleet_local_config_dir() -> std::path::PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            std::path::PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".config")
        })
        .join("fleet")
}

/// task_1032: the fleet-quiesce (maintenance-mode) sentinel — a LOCAL marker that a fleet-wide spin-down writes
/// and a reconstitution clears, so the nudge-stale daemon stays SILENT during an operator-directed mass
/// spin-down/cutover. Without it, a mass spin-down parks 60+ resumable agents and the daemon then nudges every
/// parked owner and routes their stale/unassigned tasks to the router mid-cutover — coordinator noise, and a
/// board-write-during-snapshot-freeze risk at the most delicate moment (observer task_1032). Its first line is
/// the RFC3339 instant the quiesce began, so a stale sentinel self-expires via the TTL backstop.
fn quiesce_sentinel_path() -> std::path::PathBuf {
    fleet_local_config_dir().join("quiesce")
}

/// One managed host service's ownership record: who owns it, where its authoritative definition lives, and
/// whether it is a fleet-wide `lifeline` (board / MCP / event-wake) whose changes are attended-only.
#[derive(Debug, serde::Deserialize)]
struct HostService {
    name: String,
    #[serde(default)]
    aliases: Vec<String>,
    owner: String,
    definition: String,
    #[serde(default)]
    lifeline: bool,
    #[serde(default)]
    note: String,
}

#[derive(Debug, serde::Deserialize)]
struct HostServiceRegistry {
    #[serde(default)]
    service: Vec<HostService>,
}

/// Load the registry from its local path. Absent → an empty registry (the caller prints a hint); a parse
/// error → empty + a warning (never a panic — a malformed local file must not break the `fleet` binary).
fn host_service_registry() -> HostServiceRegistry {
    let path = host_services_path();
    match std::fs::read_to_string(&path) {
        Ok(text) => parse_host_service_registry(&text).unwrap_or_else(|e| {
            eprintln!("fleet services: {} failed to parse: {e}", path.display());
            HostServiceRegistry {
                service: Vec::new(),
            }
        }),
        Err(_) => HostServiceRegistry {
            service: Vec::new(),
        },
    }
}

/// Pure TOML → registry parse, split out so it is unit-testable against an inline fixture without touching the
/// filesystem (the real registry is a local, uncommitted resource).
fn parse_host_service_registry(text: &str) -> Result<HostServiceRegistry, toml::de::Error> {
    toml::from_str(text)
}

/// Resolve a query (service name, alias, or port like `:8880`) to a service, case-insensitively. A leading
/// `:` is kept so a port alias matches as written; matching is exact against the name or any alias. Pure —
/// unit-tested.
fn find_host_service<'a>(reg: &'a [HostService], query: &str) -> Option<&'a HostService> {
    let q = query.trim().to_ascii_lowercase();
    reg.iter().find(|s| {
        s.name.to_ascii_lowercase() == q || s.aliases.iter().any(|a| a.to_ascii_lowercase() == q)
    })
}

/// `fleet services [<query>]`: the host-service ownership lookup (task_1027). With no query, list every
/// registered service (name, owner, lifeline marker). With a query, print the owner + authoritative
/// definition + a LIFELINE warning if applicable — the cheap "who owns this + where is the authored
/// definition" resolve an agent must do BEFORE touching shared infra it does not own.
fn services(query: Option<&str>) {
    let reg = host_service_registry();
    if reg.service.is_empty() {
        eprintln!(
            "fleet services: no host-service registry at {} — it is a LOCAL, deploy-injected resource (not in the public repo). Copy crates/fleet/host-services.toml.example there and fill in this host's real services.",
            host_services_path().display()
        );
        std::process::exit(1);
    }
    match query {
        None => {
            println!(
                "Host-service ownership registry ({} services) — `fleet services <name>` for detail:",
                reg.service.len()
            );
            for s in &reg.service {
                let flag = if s.lifeline {
                    "  [LIFELINE — attended-only]"
                } else {
                    ""
                };
                println!("  {:<24} owner: {}{flag}", s.name, s.owner);
            }
            println!(
                "Before modifying a shared service you do NOT own: resolve its owner + authored definition here first, prefer reusing that definition, and treat a lifeline change as owner-coordinated + operator-attended (never an unattended bounce)."
            );
        }
        Some(q) => match find_host_service(&reg.service, q) {
            Some(s) => {
                println!("service:    {}", s.name);
                if !s.aliases.is_empty() {
                    println!("aliases:    {}", s.aliases.join(", "));
                }
                println!("owner:      {}", s.owner);
                println!("definition: {}", s.definition);
                if !s.note.is_empty() {
                    println!("note:       {}", s.note);
                }
                if s.lifeline {
                    println!(
                        "LIFELINE:   YES — a fleet-wide board/MCP/event-wake dependency. Changes are OWNER-COORDINATED + OPERATOR-ATTENDED with a tested rollback; NEVER an unattended activation/bounce. Reuse the authored definition above; do not re-author from the live process."
                    );
                } else {
                    println!("lifeline:   no");
                }
            }
            None => {
                eprintln!("fleet services: no service matches '{q}'. Known services:");
                for s in &reg.service {
                    eprintln!("  {} ({})", s.name, s.aliases.join(", "));
                }
                std::process::exit(1);
            }
        },
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
    !stood_down && open_tasks > 0 && matches!(age_secs, Some(a) if a >= WATCHDOG_WORK_CADENCE_SECS)
}

/// task_506 Phase A.5 (status-driven violation): is an agent's board PRESENCE a "not actively working" one —
/// `offline` (stood down) or `away`? An agent in an at-rest presence while it still owns a live, non-exempt
/// `in_progress` task is a status-honesty violation (`in_progress` means actively worked, so declaring away/
/// offline contradicts it), caught + re-armed the same as the Phase-A offline case. `busy`/`online` are
/// working presences and never a violation. The free-form `status_message` is deliberately NOT parsed here — a
/// keyword match ("idle"/"monitoring") on prose is fragile and would false-flag a working agent whose narrative
/// merely mentions the word; the structured presence is the reliable signal. Pure — unit-tested.
fn presence_is_at_rest(status: Option<&str>) -> bool {
    matches!(status, Some("offline") | Some("away"))
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
fn agent_never_ticked(
    created_at: &str,
    last_seen: &str,
    now: time::OffsetDateTime,
    grace_secs: i64,
) -> bool {
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
    num.trim()
        .parse::<u64>()
        .ok()
        .map(|n| n.saturating_mul(mult))
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

/// The TARGETED switch-wake injected when an agent is idle/at-rest with its CURRENT task blocked on an external
/// party BUT it still owns at least one OTHER actionable (non-blocked) task (task_736, operator seq-11601). The
/// generic re-arm ([`WATCHDOG_REARM_WAKE`]) says "you hold pending work"; this names the SPECIFIC situation —
/// being blocked on one task is never a reason to halt when you own other actionable work — so the agent
/// switches rather than idling. It keeps the blocked task owned (does not reassign it) and works the others.
const WATCHDOG_SWITCH_WAKE: &str = "[watchdog] your current task is BLOCKED on an external party (operator/agent/task), but you still own at least one OTHER actionable task that is NOT blocked. Per the fleet rule, do NOT idle or halt while you hold actionable work: run list_tasks for your own id, pick a todo/in_progress task with no blocker, and work it NOW. Keep the blocked task owned (do not reassign it) — just make progress on the others meanwhile. Only idle when ALL your tasks are blocked.";

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

/// task_794 default per-agent hire-signal cooldown: at most one backlog-breach post per agent per this window
/// (6h), so a sustained deep backlog yields one actionable PM ping, not a per-sweep stream. `CDZ_HIRE_SIGNAL_COOLDOWN_SECS` tunes it.
const HIRE_SIGNAL_COOLDOWN_SECS: u64 = 6 * 3600;

/// task_794 board channel the backlog hire-signal is posted to; the PM subscribes to it (board-pm's choice over
/// a direct message — simpler, no binary board-client change, and an auditable trail of breaches/hires).
const HIRE_SIGNAL_CHANNEL: &str = "backlog-watchdog";

/// task_794 cooldown gate (pure, unit-testable): a hire-signal is suppressed when the last one for this agent is
/// within `cooldown_secs`. Unlike [`rearm_on_cooldown`] there is no floor — the cooldown is the explicit window.
fn hire_signal_on_cooldown(last_signal: Option<u64>, now: u64, cooldown_secs: u64) -> bool {
    last_signal.is_some_and(|last| now.saturating_sub(last) < cooldown_secs)
}

/// The per-agent last-hire-signal stamp path: `<hub>/.claude/fleet/watchdog/<name>.hire-signal` (unix secs).
fn hire_signal_stamp_path(fleet: &Fleet, name: &str) -> PathBuf {
    fleet
        .root
        .join("watchdog")
        .join(format!("{name}.hire-signal"))
}

/// Read an agent's last-hire-signal unix time; `None` on an absent/unparseable stamp (never signalled).
fn read_hire_signal_stamp(fleet: &Fleet, name: &str) -> Option<u64> {
    std::fs::read_to_string(hire_signal_stamp_path(fleet, name))
        .ok()?
        .trim()
        .parse()
        .ok()
}

/// Record that an agent's hire-signal was just posted at `now` (best-effort — a write failure only means the
/// cooldown is not enforced for that agent next sweep).
fn write_hire_signal_stamp(fleet: &Fleet, name: &str, now: u64) {
    let p = hire_signal_stamp_path(fleet, name);
    if let Some(dir) = p.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(p, now.to_string());
}

/// task_818 default revive-stranded cooldown: at most one auto-revive (spin-up) per stranded agent per this
/// window, so a revive that does not immediately clear the stranded state is not retried every sweep.
/// `CDZ_REVIVE_STRANDED_COOLDOWN_SECS` tunes it.
const REVIVE_STRANDED_COOLDOWN_SECS: u64 = 3600;

/// task_818 cooldown gate (pure, unit-testable), mirroring [`hire_signal_on_cooldown`]: a revive is on cooldown
/// when the last one for this agent is within `cooldown_secs`. No floor — the cooldown IS the window. `None`
/// (never revived) is not on cooldown.
fn revive_stranded_on_cooldown(last_revive: Option<u64>, now: u64, cooldown_secs: u64) -> bool {
    last_revive.is_some_and(|last| now.saturating_sub(last) < cooldown_secs)
}

/// The per-agent last-revive stamp path: `<hub>/.claude/fleet/watchdog/<name>.revive` (contents = unix secs).
fn revive_stranded_stamp_path(fleet: &Fleet, name: &str) -> PathBuf {
    fleet.root.join("watchdog").join(format!("{name}.revive"))
}

/// Read an agent's last-revive unix time; `None` on an absent/unparseable stamp (never revived).
fn read_revive_stranded_stamp(fleet: &Fleet, name: &str) -> Option<u64> {
    std::fs::read_to_string(revive_stranded_stamp_path(fleet, name))
        .ok()?
        .trim()
        .parse()
        .ok()
}

/// Record that an agent was just auto-revived at `now` (best-effort — a write failure only means the cooldown
/// is not enforced for that agent on the next sweep).
fn write_revive_stranded_stamp(fleet: &Fleet, name: &str, now: u64) {
    let p = revive_stranded_stamp_path(fleet, name);
    if let Some(dir) = p.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(p, now.to_string());
}

/// task_1152 default relaunch-missing cooldown: at most one auto-relaunch (spin-up) per missing-window agent
/// per this window, so a crash-looping agent (relaunch → crash → relaunch) is relaunched a bounded number of
/// times, not every sweep. Matches the revive-stranded cadence; `CDZ_RELAUNCH_MISSING_COOLDOWN_SECS` tunes it.
const RELAUNCH_MISSING_COOLDOWN_SECS: u64 = 3600;

/// The per-agent last-relaunch stamp path: `<hub>/.claude/fleet/watchdog/<name>.relaunch` (contents = unix
/// secs). Distinct from the `.revive` (revive-stranded) stamp so the two cooldowns never collide.
fn relaunch_missing_stamp_path(fleet: &Fleet, name: &str) -> PathBuf {
    fleet.root.join("watchdog").join(format!("{name}.relaunch"))
}

/// Read an agent's last-relaunch unix time; `None` on an absent/unparseable stamp (never relaunched).
fn read_relaunch_missing_stamp(fleet: &Fleet, name: &str) -> Option<u64> {
    std::fs::read_to_string(relaunch_missing_stamp_path(fleet, name))
        .ok()?
        .trim()
        .parse()
        .ok()
}

/// Record that an agent's missing window was just auto-relaunched at `now` (best-effort — a write failure only
/// means the cooldown is not enforced for that agent on the next sweep).
fn write_relaunch_missing_stamp(fleet: &Fleet, name: &str, now: u64) {
    let p = relaunch_missing_stamp_path(fleet, name);
    if let Some(dir) = p.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(p, now.to_string());
}

/// task_582 default recover-wedged cooldown: at most one auto-recovery (force spin-down + spin-up) per wedged
/// agent per this window, so a recovery that does not immediately clear the safeguard-refusal loop is not
/// retried every sweep (a fresh session that re-wedges on the same input must not be thrashed). 1h matches the
/// revive-stranded cadence; `CDZ_RECOVER_WEDGED_COOLDOWN_SECS` tunes it.
const RECOVER_WEDGED_COOLDOWN_SECS: u64 = 3600;

/// task_582 cooldown gate (pure, unit-testable), mirroring [`revive_stranded_on_cooldown`]: a recovery is on
/// cooldown when the last one for this agent is within `cooldown_secs`. No floor — the cooldown IS the window.
/// `None` (never recovered) is not on cooldown.
fn recover_wedged_on_cooldown(last_recover: Option<u64>, now: u64, cooldown_secs: u64) -> bool {
    last_recover.is_some_and(|last| now.saturating_sub(last) < cooldown_secs)
}

/// The per-agent last-recovery stamp path: `<hub>/.claude/fleet/watchdog/<name>.recover-wedged` (unix secs).
fn recover_wedged_stamp_path(fleet: &Fleet, name: &str) -> PathBuf {
    fleet
        .root
        .join("watchdog")
        .join(format!("{name}.recover-wedged"))
}

/// Read an agent's last-recovery unix time; `None` on an absent/unparseable stamp (never recovered).
fn read_recover_wedged_stamp(fleet: &Fleet, name: &str) -> Option<u64> {
    std::fs::read_to_string(recover_wedged_stamp_path(fleet, name))
        .ok()?
        .trim()
        .parse()
        .ok()
}

/// Record that an agent was just auto-recovered at `now` (best-effort — a write failure only means the cooldown
/// is not enforced for that agent on the next sweep).
fn write_recover_wedged_stamp(fleet: &Fleet, name: &str, now: u64) {
    let p = recover_wedged_stamp_path(fleet, name);
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
    fleet
        .root
        .join("observer")
        .join(format!("{name}.watermark"))
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
fn observe_candidate(
    fleet: &Fleet,
    agent: &str,
    roster: &[String],
    stood_down: bool,
    threshold: usize,
) -> Option<ObserveDecision> {
    // task_846: roster-aware session location — resolve ONE owner per transcript dir (longest-match) so a
    // dash-prefix sibling (v-task-board vs v-task-board-helper) does not pick up the other's transcript and
    // re-fire the same span under a second watermark key forever.
    let sessions = transcripts::locate_sessions_disambiguated(agent, roster);
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

/// task_610: the max doublings the re-spawn cooldown backs off by while an observation keeps being attempted
/// WITHOUT the watermark advancing (a crashing/wedging observer re-reading the same span). base, 2x, 4x, 8x =
/// 30m, 1h, 2h, 4h cap. So a repeatedly-failing full-window re-read drops from ~hourly to ~4-hourly instead
/// of burning an Opus observer every cooldown; the span stays eligible (the confirmed-observation guardrail is
/// untouched — only the retry CADENCE backs off), and the backoff resets the moment the watermark advances.
const OBSERVE_RESPAWN_MAX_BACKOFF_STEPS: u32 = 3;

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

/// task_610: the re-spawn cooldown, backed off while an observation keeps being attempted WITHOUT its
/// watermark advancing. Each prior unconfirmed attempt doubles the base cooldown (30m, 1h, 2h, 4h), capped at
/// `max_doublings`, so a crashing/wedging observer that re-reads the same full window drops from ~hourly to
/// ~4-hourly instead of burning an Opus observer every cooldown. `prior_attempts` 0 or 1 → the base (no
/// backoff yet). A confirmed observation advances the watermark, which resets the attempt count (see the spawn
/// loop), so this reverts to the base cadence. Mirrors [`acked_cooldown_with_backoff`]. Pure — unit-tested.
fn observe_respawn_cooldown(
    base_cooldown_secs: u64,
    prior_attempts: u32,
    max_doublings: u32,
) -> u64 {
    let steps = prior_attempts.saturating_sub(1).min(max_doublings);
    base_cooldown_secs.saturating_mul(1u64 << steps)
}

/// The per-target observer spawn-attempts sidecar: `<hub>/.claude/fleet/observer/<name>.spawn-attempts`
/// (line 1 = the watermark the attempts are counted against, line 2 = the count). Sibling of the watermark +
/// spawn stamp. task_610: lets the re-spawn cooldown back off per unconfirmed attempt against the SAME
/// watermark, and reset when the watermark advances.
fn observe_spawn_attempts_path(fleet: &Fleet, name: &str) -> PathBuf {
    fleet
        .root
        .join("observer")
        .join(format!("{name}.spawn-attempts"))
}

/// Read `(watermark_the_attempts_are_against, count)`; `("", 0)` when absent/unparseable — so a never-recorded
/// or corrupt file reads as "no attempts yet", never a spurious backoff.
fn read_observe_spawn_attempts(fleet: &Fleet, name: &str) -> (String, u32) {
    std::fs::read_to_string(observe_spawn_attempts_path(fleet, name))
        .ok()
        .and_then(|s| {
            let mut lines = s.lines();
            let wm = lines.next()?.trim().to_string();
            let count = lines.next()?.trim().parse().ok()?;
            Some((wm, count))
        })
        .unwrap_or_default()
}

fn write_observe_spawn_attempts(fleet: &Fleet, name: &str, watermark: &str, count: u32) {
    let p = observe_spawn_attempts_path(fleet, name);
    if let Some(dir) = p.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(p, format!("{watermark}\n{count}\n"));
}

/// Reap bound (task_610): an `obs-<target>` window still alive this long after its spawn is a stuck/leaked
/// observer — a healthy observation of even a large transcript completes well within it, and a completed one
/// self-closes. `CDZ_OBSERVE_REAP_STALE_SECS` overrides. Comfortably longer than [`OBSERVE_SPAWN_COOLDOWN_SECS`]
/// so an in-flight observer is never reaped mid-observation.
const OBSERVE_REAP_STALE_SECS: u64 = 3600; // 1h

/// The per-target observer tmux window name (local only; the board identity stays `observer`). The target's
/// `/`, `:`, `.` are mangled to `-` so it is a valid single-token window name. Pure — unit-tested.
fn obs_window_name(target: &str) -> String {
    format!("obs-{}", target.replace(['/', ':', '.'], "-"))
}

/// The per-(review, angle) reviewer tmux window name (local only; the board identity stays `reviewer`), the
/// reviewer analog of [`obs_window_name`]. Several angles on one review (and several reviews) run at once
/// without a name clash. The angle keys are kebab-case so already window-safe; mangled defensively anyway.
/// Pure — unit-tested.
fn reviewer_window_name(review_id: i64, angle: &str) -> String {
    format!("rev-{review_id}-{}", angle.replace(['/', ':', '.'], "-"))
}

/// The idempotent per-angle CLAIM id for an adversarial review (task_374 S2, v-task-board contract
/// comment_1093): `adversarial-claim:<review_id>:<angle>`. The spawn pass APPENDS an `adversarial_review` log
/// entry with this `external_id` FIRST and spawns the angle's reviewer only when the append returns
/// `appended:true`; a re-sweep's duplicate append returns `appended:false` (deduped by external_id, atomic
/// under the board's single-writer pool) → skip. Per-ANGLE so a partial-failure re-sweep re-spawns only the
/// angle that had not yet claimed. Pure — unit-tested. (The board append that consumes this is S2b.)
fn reviewer_claim_external_id(review_id: i64, angle: &str) -> String {
    format!("adversarial-claim:{review_id}:{angle}")
}

/// The log `entry_type` the per-angle spawn CLAIM is appended as (v-task-board contract comment_5794:
/// `adversarial_review`); an `appended:true` on this entry means the claim is won.
const REVIEW_CLAIM_ENTRY_TYPE: &str = "adversarial_review";
/// The principal attributed to the per-angle spawn CLAIM — the orchestrator doing append-first-then-spawn,
/// distinct from the ephemeral `reviewer` agent it launches on a won claim.
const REVIEW_SPAWN_PRINCIPAL: &str = "reviewer-spawn";

/// Whether an `obs-<target>` window that is STILL ALIVE should be reaped (task_610). A healthy in-flight
/// observer has a RECENT spawn stamp (< bound) → never reaped. A stamp older than the bound = the observer
/// never confirmed (`observe-record` clears the stamp on success), so it crashed/hung → reap. An ABSENT stamp
/// on a still-open window = the observation already confirmed (stamp cleared) but the window lingered (its
/// self-close was missed) → also reap; nothing in-flight is lost in either reap case. Pure — unit-tested.
fn observer_window_is_stale(spawn_stamp: Option<u64>, now: u64, reap_bound: u64) -> bool {
    match spawn_stamp {
        None => true,
        Some(s) => now.saturating_sub(s) >= reap_bound,
    }
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
             across ALL statuses — todo+in_progress+blocked+done+cancelled, NOT open-only — against the \
             proposal children of #{n}, other `observer` proposals in #28, AND `kb_search`; a match on a DONE \
             task means the fix already LANDED, so record a no-op/corroboration instead of re-filing it. When \
             you have filed every proposal (or an explicit no-op finding), CLOSE the observation: update_task {n} \
             with status=\"done\", actor=\"observer\" — the closed task IS the signal the observation ran."
        ),
        None =>
            "File only above-floor, evidence-cited, deduped proposals into board project #28 \
             (fleet-self-improve) per that project's template; dedup across ALL statuses — \
             todo+in_progress+blocked+done+cancelled, NOT open-only — against `observer` proposals in #28 \
             plus `kb_search`; a match on a DONE task means the fix already LANDED, so record a no-op, do not re-file."
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
        .map(|repo| {
            repo.join("loops/observer.md")
                .to_string_lossy()
                .into_owned()
        })
        .unwrap_or_else(|| "loops/observer.md".to_string());
    // The observation task this observer drives from (#290): the caller resolved it (reuse an open one, else
    // create) and threads its id here so the observer files proposals as its children + closes it. `None`
    // (dry-run, or a board hiccup) → the observer falls back to standalone proposals.
    let kickoff = build_observer_kickoff(
        target,
        obs_session,
        since_offset,
        &role_path,
        &fleet_bin,
        observation_task,
    );
    // A per-target tmux window (local only — the BOARD identity stays `observer`), so several observations
    // can run at once without a name clash.
    let window = obs_window_name(target);
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
            "new-window",
            "-d",
            "-t",
            board_session,
            "-n",
            &window,
            "-c",
            &workdir,
            "-e",
            &format!("CDZ_KICKOFF={kickoff}"),
            &cmd,
        ])
        .status()
    {
        Ok(s) if s.success() => format!("spawned({window})"),
        Ok(_) => "spawn-FAILED(tmux new-window)".to_string(),
        Err(e) => format!("spawn-FAILED(tmux: {e})"),
    }
}

/// Launch (or, with `dry_run`, preview) an ephemeral adversarial-reviewer session for one review on one angle
/// (task_374 S2a) — the reviewer analog of [`spawn_observer`] on the same ephemeral spawn engine (#187/#188).
/// Runs in a repo-less workspace under the fleet root; the board identity is `reviewer` (the session
/// registers/authors as it per [`build_reviewer_kickoff`]), and the per-(review,angle) tmux window
/// ([`reviewer_window_name`]) lets several angles run at once. Returns a short action label for the caller's
/// report. Best-effort: a launch error is reported, never fatal (one angle failing must not abort the rest).
fn spawn_reviewer(
    board_session: &str,
    review_id: i64,
    angle_key: &str,
    angle_focus: &str,
    dry_run: bool,
) -> String {
    let fleet_root = config::get()
        .root
        .clone()
        .unwrap_or_else(|| format!("{}/.fleet", std::env::var("HOME").unwrap_or_default()));
    let workdir = workspace::agent_root_dir(&fleet_root, "reviewer");
    // Use THIS standalone binary (absolute path) for the reviewer's `fleet` commands; the role body ships
    // beside it at <repo>/loops/reviewer.md (binary = <repo>/target/<profile>/fleet → up 3). Mirrors
    // spawn_observer's resolution exactly.
    let exe = std::env::current_exe().ok();
    let fleet_bin = exe
        .as_ref()
        .and_then(|p| p.to_str())
        .unwrap_or("fleet")
        .to_string();
    let role_path = exe
        .as_ref()
        .and_then(|p| p.ancestors().nth(3))
        .map(|repo| {
            repo.join("loops/reviewer.md")
                .to_string_lossy()
                .into_owned()
        })
        .unwrap_or_else(|| "loops/reviewer.md".to_string());
    let kickoff = build_reviewer_kickoff(review_id, angle_key, angle_focus, &role_path, &fleet_bin);
    let window = reviewer_window_name(review_id, angle_key);
    if dry_run {
        return format!("would-spawn({window}←review#{review_id}/{angle_key})");
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
            "new-window",
            "-d",
            "-t",
            board_session,
            "-n",
            &window,
            "-c",
            &workdir,
            "-e",
            &format!("CDZ_KICKOFF={kickoff}"),
            &cmd,
        ])
        .status()
    {
        Ok(s) if s.success() => format!("spawned({window})"),
        Ok(_) => "spawn-FAILED(tmux new-window)".to_string(),
        Err(e) => format!("spawn-FAILED(tmux: {e})"),
    }
}

/// `fleet review-spawn <review_id> [--angle <key>] [--apply]` (task_374 S2a+S2b): spawn the per-angle ephemeral
/// adversarial reviewers for ONE review on the existing spawn engine. Dry-run by default (prints the plan and
/// writes nothing to the board); `--apply` CLAIMS each angle on the board first and launches only the angles it
/// wins. `--angle` restricts to one [`REVIEW_ANGLES`] key (default: all). The S2b claim
/// ([`reviewer_claim_external_id`] appended via [`board::Board::append_review_log`]) makes `--apply` IDEMPOTENT:
/// a re-run's duplicate append returns `appended:false` → already claimed → skip, so re-applying never
/// double-spawns an angle. The automatic `review.opened_for_review` trigger is the next slice (S2c); this stays
/// the manual entry point.
fn review_spawn(board_session: &str, review_id: i64, angle: Option<&str>, apply: bool) {
    let angles: Vec<&(&str, &str)> = match angle {
        Some(k) => {
            let found: Vec<_> = REVIEW_ANGLES.iter().filter(|(key, _)| *key == k).collect();
            if found.is_empty() {
                eprintln!(
                    "review-spawn: unknown angle '{k}' (known: {})",
                    REVIEW_ANGLES
                        .iter()
                        .map(|(key, _)| *key)
                        .collect::<Vec<_>>()
                        .join(", ")
                );
                std::process::exit(2);
            }
            found
        }
        None => REVIEW_ANGLES.iter().collect(),
    };
    println!(
        "review-spawn: review #{review_id}, {} angle(s){}",
        angles.len(),
        if apply {
            ""
        } else {
            " (dry-run — pass --apply to launch)"
        }
    );
    // S2b: on --apply, CLAIM each angle on the board FIRST (append an `adversarial_review` log entry carrying
    // the per-angle external_id) and spawn only when the append is NEW (appended:true). A re-sweep's duplicate
    // append returns appended:false → already claimed → skip, so --apply is idempotent (closes the S2a
    // "re-apply re-spawns" gap). Dry-run never touches the board.
    let board = if apply {
        Some(board::Board::connect().unwrap_or_else(|e| {
            eprintln!("review-spawn: board unavailable, cannot claim ({e})");
            std::process::exit(1);
        }))
    } else {
        None
    };
    for (key, focus) in angles {
        let claim = reviewer_claim_external_id(review_id, key);
        match &board {
            // Dry-run: show what would be claimed + spawned, no board writes.
            None => {
                let label = spawn_reviewer(board_session, review_id, key, focus, true);
                println!("  {key}: {label}  [would claim {claim}]");
            }
            Some(b) => {
                let body = format!(
                    "adversarial-review spawn claim for review #{review_id} angle '{key}' ({focus})"
                );
                match b.append_review_log(
                    review_id,
                    REVIEW_CLAIM_ENTRY_TYPE,
                    REVIEW_SPAWN_PRINCIPAL,
                    Some(&claim),
                    Some(&body),
                ) {
                    // Won the claim → spawn the angle's reviewer.
                    Ok(true) => {
                        let label = spawn_reviewer(board_session, review_id, key, focus, false);
                        println!("  {key}: {label}  [claimed {claim}]");
                    }
                    // Already claimed by a prior pass → skip (idempotent re-apply).
                    Ok(false) => println!("  {key}: already-claimed, skip  [claim {claim}]"),
                    // Claim failed → do NOT spawn: launching without a won claim risks a duplicate reviewer.
                    Err(e) => {
                        eprintln!("  {key}: claim FAILED, not spawning ({e})  [claim {claim}]")
                    }
                }
            }
        }
    }
}

/// Whether a review (a [`board::Board::list_reviews`] record) still needs the adversarial pass: it is
/// `in_review` AND not yet `vetted`. The list endpoint has no `vetted` filter (v-task-board contract
/// comment_5794), so the sweep applies it client-side — `vetted==true` means the pass already ran and was
/// addressed (the `/vetted` gate), so skip it; a non-`in_review` status needs no pass. A missing `vetted` is
/// treated as not-yet-vetted (needs a pass), the safe default. Pure — unit-tested.
fn review_needs_adversarial_pass(review: &serde_json::Value) -> bool {
    review.get("status").and_then(serde_json::Value::as_str) == Some("in_review")
        && !review
            .get("vetted")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false)
}

/// `fleet review-sweep [--apply]` (task_374 S2c): the fleet-wide automatic trigger. List every review the board
/// has `in_review`, keep the ones not yet `vetted` ([`review_needs_adversarial_pass`]), and run the per-angle
/// claim+spawn ([`review_spawn`]) for each. The S2b per-angle claim makes this IDEMPOTENT — a review already
/// swept has every angle claimed, so a re-sweep claims nothing and spawns nothing — which is what lets it run
/// safely on a timer/watchdog cadence or on a `review.opened_for_review` wake (mirroring the observer `--spawn`
/// cadence). Dry-run by default (lists the plan, no board writes); `--apply` claims + launches.
fn review_sweep(board_session: &str, apply: bool) {
    let board = board::Board::connect().unwrap_or_else(|e| {
        eprintln!("review-sweep: board unavailable ({e})");
        std::process::exit(1);
    });
    let reviews = board.list_reviews("in_review").unwrap_or_else(|e| {
        eprintln!("review-sweep: list_reviews failed ({e})");
        std::process::exit(1);
    });
    let pending: Vec<i64> = reviews
        .iter()
        .filter(|r| review_needs_adversarial_pass(r))
        .filter_map(|r| r.get("id").and_then(serde_json::Value::as_i64))
        .collect();
    println!(
        "review-sweep: {} review(s) in_review awaiting an adversarial pass{}",
        pending.len(),
        if apply {
            ""
        } else {
            " (dry-run — pass --apply to claim + launch)"
        }
    );
    // Per-review: run the full per-angle claim+spawn. review_spawn is idempotent under --apply (the S2b claim),
    // so a review already swept re-claims nothing. Board::connect is sessionless, so re-connecting per review
    // inside review_spawn is free.
    for id in pending {
        review_spawn(board_session, id, None, apply);
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
            let _ = std::process::Command::new("tmux")
                .args(["kill-window", "-t", id])
                .status();
        }
    }
}

// ── post-deploy (#171 deploy-notification channel) ─────────────────────────────────────────────────

/// The board channel deploy events are posted to; agents subscribe while waiting on a deploy and leave
/// when done (board-pm approved one channel, #171). Created-if-absent on first post.
const DEPLOYS_CHANNEL: &str = "deploys";
/// The identity the deploy pipeline authors its posts as.
const DEPLOY_SENDER: &str = "deployer";

/// Format a deploy-confirmed channel message. A parseable, stable prefix so a waiter can match its commit:
/// `deploy <repo>@<sha> → <host>: <STATUS>` (STATUS upper-cased, e.g. LIVE | FAILED — a waiter must STOP +
/// escalate on FAILED, not wait forever, per board-pm). Pure — unit-tested.
fn deploy_event_body(repo: &str, sha: &str, host: &str, status: &str) -> String {
    format!(
        "deploy {repo}@{sha} → {host}: {}",
        status.trim().to_uppercase()
    )
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
            eprintln!(
                "fleet post-deploy: post to #{DEPLOYS_CHANNEL} (id {channel_id}) failed: {e}"
            );
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
#[allow(clippy::too_many_arguments)]
fn watchdog(
    stale_only: bool,
    rearm: bool,
    observe: bool,
    spawn: bool,
    spawn_dry_run: bool,
    pinned_only: bool,
    self_redeploy: bool,
    reap_stale_observers: bool,
    hire_signal: bool,
    revive_stranded: bool,
    // Named distinctly from the `bounce_stale` function it gates (a same-name binding would shadow the fn).
    sweep_stale: bool,
    recover_wedged: bool,
    relaunch_missing: bool,
    // Named distinctly from the `review_sweep` function it gates (a same-name binding would shadow the fn).
    sweep_reviews: bool,
) {
    // Self-surface (or self-heal) a stale binary: the watchdog is long-running (a timer/loop re-execs this
    // binary), so if its source checkout advanced past the built rev it would silently run old logic (a merged
    // fix not effective until rebuilt). With `--self-redeploy` (#388) ACT on it — rebuild + restart the daemons
    // so the fix goes live without a manual step; otherwise WARN (rebuilding stays out of band). No-op for a
    // deployed binary (no .git) or when already fresh.
    match watchdog_stale_self_action(
        env!("FLEET_BUILD_REV"),
        checkout_head_short().as_deref(),
        self_redeploy,
    ) {
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
    let native_ids = match board::Board::connect()
        .and_then(|b| b.list_agents().map(|agents| (b, agents)))
    {
        Ok((board, agents)) => {
            let native_ids = native_agent_ids(&agents);
            watchdog_board(
                &board,
                &agents,
                stale_only,
                rearm,
                observe,
                spawn,
                spawn_dry_run,
                pinned_only,
                reap_stale_observers,
                hire_signal,
                revive_stranded,
                recover_wedged,
                relaunch_missing,
            );
            // task_752: after the liveness pass, optionally refresh stale sessions' MCP tools/list. Gated behind
            // --bounce-stale (opt-in), run only when the board is reachable so a connect failure never aborts the
            // sweep, and in APPLY mode (passing the flag IS the opt-in, like --revive-stranded). The sweep's own
            // per-agent fences (busy / cooldown) protect each session; --force is not plumbed here (an automated
            // sweep must never override the busy fence and reconnect through a live turn).
            if sweep_stale {
                println!(
                    "-- bounce-stale (--bounce-stale: refreshing stale sessions' tools/list) --"
                );
                bounce_stale(&Fleet::resolve(), true, false);
            }
            // task_374 S2c: with --review-sweep, run the adversarial-review sweep this pass — spawn the
            // per-angle reviewers for every in_review + unvetted review. Board-reachable only (we are inside
            // the connected arm; review_sweep re-connects, which is sessionless/free). Idempotent via the S2b
            // per-angle claim, so a review already covered re-spawns nothing. Honors --dry-run for a preview.
            if sweep_reviews {
                println!("-- review-sweep (--review-sweep: adversarial-review spawn cadence) --");
                review_sweep(&board_session(), !spawn_dry_run);
            }
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
    let Some(url) = config::get()
        .tunnel_health_url
        .clone()
        .filter(|s| !s.trim().is_empty())
    else {
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
        eprintln!(
            "  ⚠ tunnel wedged: event-wakes are NOT being delivered to this host — agents fall back to slow poll. Probe {url}"
        );
    }
}

/// Classify a tunnel health-probe outcome into `(ok, one-line report)`. `Ok(200)` = healthy; any other
/// status (e.g. the daemon's own `503` when the socket is wedged) or a transport error = NOT ok. Pure so the
/// verdict is unit-testable without a live daemon; the caller does the bounded GET. See fleet-tunnel #59.
fn tunnel_health_line(probe: &Result<u16, String>) -> (bool, String) {
    match probe {
        Ok(200) => (
            true,
            "tunnel health: OK (HTTP 200 — board WS connected, frame fresh, upstream reachable)"
                .to_string(),
        ),
        Ok(code) => (
            false,
            format!("tunnel health: WEDGED (HTTP {code} — probe reachable but not ok)"),
        ),
        Err(e) => (
            false,
            format!("tunnel health: UNREACHABLE (probe failed: {e})"),
        ),
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
        .filter_map(|a| {
            a.get("id")
                .and_then(serde_json::Value::as_str)
                .map(String::from)
        })
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

/// The set of agent ids that OWN at least one task PARKED on an external blocker, from a
/// `list_tasks_by_status("blocked")` projection (task_736). A `blocked` task carries a `blocked_on_kind`
/// (operator/agent/task/external) — it is legitimately waiting, so it is NOT actionable, but its owner is the
/// actor the switch-to-actionable rule targets: if it ALSO owns an actionable task it must switch to that
/// rather than idle on the block. Filters to tasks that genuinely carry a blocker (defensive — a `blocked`
/// status without a `blocked_on_kind` is malformed and excluded). Pure — unit-tested.
fn blocked_task_owners(tasks: &[serde_json::Value]) -> std::collections::BTreeSet<String> {
    tasks
        .iter()
        .filter(|t| task_is_parked_on_blocker(t))
        .filter_map(|t| t.get("assignee").and_then(serde_json::Value::as_str))
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect()
}

/// Whether the watchdog should fire the TARGETED switch-nudge (task_736, operator seq-11601): the agent owns a
/// task blocked on an external party AND still owns at least one OTHER actionable (non-blocked) task AND is not
/// making progress (`at_rest` presence — away/offline — OR `quiet_holding_work`, the #535 work-driven signal
/// that it has gone quiet while holding actionable work). All three are required: without `owns_blocked` the
/// plain never-idle guards already apply; without an actionable task there is nothing to switch TO (idling is
/// then legitimate); without the idle signal the agent is presumed to be working. Pure — unit-tested.
fn should_switch_to_actionable(
    at_rest: bool,
    owns_blocked: bool,
    actionable_open: usize,
    quiet_holding_work: bool,
) -> bool {
    owns_blocked && actionable_open >= 1 && (at_rest || quiet_holding_work)
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
        if rest
            .strip_prefix(b"/")
            .is_some_and(|after| glob_rec(after, text))
        {
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

/// Partition a declared seam list into (include globs, exclude globs). A glob prefixed with `!` is an
/// EXCLUDE (the `!` is stripped); every other glob is an include (task_579). Pure — unit-tested.
fn split_seam_globs(seams: &[String]) -> (Vec<&str>, Vec<&str>) {
    let mut includes = Vec::new();
    let mut excludes = Vec::new();
    for g in seams {
        match g.strip_prefix('!') {
            Some(ex) => excludes.push(ex),
            None => includes.push(g.as_str()),
        }
    }
    (includes, excludes)
}

/// The subset of `changed` paths that touch the agent's declared `seams` (task_579). A path is on-seam when
/// it matches at least one INCLUDE glob AND no EXCLUDE glob (a `!`-prefixed entry). The exclude form is the
/// curation a monitor seam needs: a vertical can own a directory broadly (`backend/cadenza/**`) yet carve out
/// a shared-but-not-owned subtree (`!backend/cadenza/select/**`) so another lane's churn there does not wake
/// it. EMPTY means GREEN — no incoming commit touched the monitor's seam, so the wake is a deterministic
/// no-op and the caller may heartbeat WITHOUT waking the model. Non-empty means CHANGED — wake the model with
/// exactly these paths in hand. Pure — unit-tested.
fn seam_touched<'a>(changed: &'a [String], seams: &[String]) -> Vec<&'a str> {
    let (includes, excludes) = split_seam_globs(seams);
    changed
        .iter()
        .filter(|p| {
            includes.iter().any(|g| seam_glob_matches(p, g))
                && !excludes.iter().any(|g| seam_glob_matches(p, g))
        })
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
    reap_stale_observers: bool,
    hire_signal: bool,
    revive_stranded: bool,
    recover_wedged: bool,
    relaunch_missing: bool,
) {
    let now = time::OffsetDateTime::now_utc();
    let now_unix = now.unix_timestamp().max(0) as u64; // for the per-agent re-arm cooldown stamps
    let fleet = Fleet::resolve(); // stamp store (<hub>/.claude/fleet/watchdog/); shared with the file-hub scan
    // task_1032: is a fleet-wide spin-down/cutover in effect? If so, the watchdog's opt-in auto-ACTIONS
    // (revive-stranded, hire-signal) stand down this sweep — reviving a parked agent would fight the operator's
    // spin-down and a hire-signal post is a board write during the freeze. The report WARNINGS still print; only
    // the ACT is gated. Same local sentinel the nudge-stale daemon reads (self-expires via the TTL).
    let fleet_quiesced = fleet_is_quiesced(
        quiesce_sentinel_age_secs(now),
        (FLEET_QUIESCE_TTL_HOURS * 3600.0).round() as i64,
    );
    let session = board_session();
    let host = this_host(); // host-affinity: this box only manages agents pinned here (or unpinned)
    if pinned_only {
        println!(
            "(--pinned-only: managing only agents EXPLICITLY pinned to {host}; unpinned run-anywhere agents skipped)"
        );
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
    let (inprogress_owners, monitor_exempt_owners) = match board.list_tasks_by_status("in_progress")
    {
        Ok(tasks) => (
            inprogress_task_assignees(&tasks),
            monitor_exempt_task_owners(&tasks),
        ),
        Err(_) => (
            std::collections::BTreeSet::new(),
            std::collections::BTreeSet::new(),
        ),
    };
    // task_736 switch-to-actionable: the owners of a task PARKED on an external blocker. An agent here that
    // ALSO owns an actionable task must switch to it rather than idle on the block. One extra status query per
    // sweep (mirrors the in_progress one); a query error degrades to empty (no false switch-nudges).
    let blocked_owners = match board.list_tasks_by_status("blocked") {
        Ok(tasks) => blocked_task_owners(&tasks),
        Err(_) => std::collections::BTreeSet::new(),
    };
    println!(
        "{:<28} {:<8} {:<7} {:<5} {:<8} {:<12} last_seen",
        "agent", "interval", "age", "open", "verdict", "action"
    );
    let mut flagged = 0usize;
    let mut rearmed = 0usize;
    let mut never_ticked_count = 0usize;
    let mut holding_work_count = 0usize;
    let mut switch_count = 0usize;
    // task_582 safeguard-wedge (report-only this slice): agents whose newest session is stuck in a trailing
    // run of model-safeguard refusals. Collected across the sweep and surfaced in one WARNING below.
    let mut wedged_ids: Vec<String> = Vec::new();
    // task_786 corrective (report-only this slice): STOOD-DOWN agents still holding open assignments — the
    // cr-reviewer stranding class a wake cannot fix (windowless). (id, open_count), surfaced in one WARNING.
    let mut stranded_ids: Vec<(String, usize)> = Vec::new();
    // task_1152: the live tmux window set, read once, so the per-agent loop can tell a should-be-up agent whose
    // window is GONE (crashed/vanished — a relaunch candidate) from one that is merely stale-but-running.
    let wd_windows = tmux_window_names(&session);
    let mut missing_window_ids: Vec<String> = Vec::new();
    // task_794 (report-only this slice): RUNNING agents whose open backlog exceeds the depth threshold — the
    // PM hire/route signal. (id, open_count), surfaced in one WARNING; the auto board-signal is the opt-in next.
    let backlog_depth = std::env::var("CDZ_BACKLOG_DEPTH")
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(BACKLOG_DEPTH_DEFAULT);
    let mut backlog_overflow_ids: Vec<(String, usize)> = Vec::new();
    let mut native = 0usize;
    // task_500: ids whose metadata.native was ABSENT/malformed (NOT an explicit false) — collected to warn.
    let mut unknown_native_ids: Vec<String> = Vec::new();
    // task_846: the full roster of agent ids — observe_candidate resolves ONE owner per transcript dir from
    // this (longest-match), so a dash-prefix sibling never gets another agent's span observed under its key.
    let roster_ids: Vec<String> = agents
        .iter()
        .filter_map(|a| a.get("id").and_then(|v| v.as_str()).map(String::from))
        .collect();
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
            Some(age) => (
                watchdog_verdict(age, interval_secs),
                format!("{}m", age / 60),
            ),
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
        let created_at = a
            .get("created_at")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("");
        let never_ticked = !agent_is_staged(md)
            && !stood_down
            && agent_never_ticked(created_at, ls, now, WATCHDOG_NEVER_TICKED_GRACE_SECS);
        // #506 holding-work-while-at-rest violation (Phase A + A.5): the agent is in a NOT-WORKING presence —
        // `offline` (stood down, the v-bolero case: created work, then slept) OR `away` (self-declared idle) —
        // while it still owns a live, non-exempt `in_progress` task. `in_progress` is actively-worked, so an
        // at-rest presence on it (instead of progressing it or marking it `blocked`/`done`) is a status-honesty
        // violation, not a legitimate rest. Flagged + re-armed below so it re-enters its loop. A monitor_exempt
        // in_progress task is already excluded from `inprogress_owners` (Phase B), so a deliberate continuous
        // monitor never trips this.
        let holding_work_at_rest = presence_is_at_rest(status) && inprogress_owners.contains(id);
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
        if observe
            && let Some(d) =
                observe_candidate(&fleet, id, &roster_ids, stood_down, observe_threshold)
        {
            obs.push((id.to_string(), stood_down, d));
        }
        // task_786 corrective + task_794: both read the per-agent open_task_count already computed above, and
        // both are collected BEFORE the --stale-only skip (a stood-down agent with a quiet queue trips none of
        // the re-arm gates, so it would otherwise be skipped in a --stale-only sweep and never surface).
        if agent_is_stranded(stood_down, open_tasks) {
            stranded_ids.push((id.to_string(), open_tasks));
        }
        if backlog_over_depth(stood_down, open_tasks, backlog_depth) {
            backlog_overflow_ids.push((id.to_string(), open_tasks));
        }
        // task_1152: a managed agent that SHOULD be up but has no live tmux window — relaunch candidate (the
        // active-liveness signal, distinct from the stale/age signals). `stood_down` (offline) is excluded so
        // a deliberate spin-down is never relaunched; `spin_up_hold_reason` excludes staged/off-host (redundant
        // with the host filter above, harmless)/launch-gated/charter-deferred. The cooldown is applied in the
        // act pass below.
        let has_window = wd_windows.iter().any(|w| w == id);
        if relaunch_missing_candidate(has_window, stood_down, spin_up_hold_reason(md, &host)) {
            missing_window_ids.push(id.to_string());
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
        // task_736 switch-to-actionable: the agent owns a task blocked on an external party AND still owns >=1
        // OTHER actionable (non-blocked) task AND is not progressing (at-rest, or #535-quiet while holding work).
        // It must SWITCH to the actionable work rather than idle on the block — the targeted complement of the
        // #506 never-idle guard. `open_tasks` already EXCLUDES blocked/parked tasks, so >=1 is genuine other work.
        let switch_to_actionable = should_switch_to_actionable(
            presence_is_at_rest(status),
            blocked_owners.contains(id),
            open_tasks,
            work_driven,
        );
        if stale_only
            && !retighten
            && !work_driven
            && !drained_idle
            && !never_ticked
            && !holding_work_at_rest
            && !switch_to_actionable
        {
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
        } else if switch_to_actionable {
            // task_736: blocked on one task, owns another actionable one, idling — the TARGETED switch-nudge
            // (more specific than the generic re-arm). Re-kicked on the tight work cadence like a work-holder,
            // pane-fenced by rearm_candidate. Takes priority over the holding-work / retighten branches so the
            // agent gets the "switch, do not idle on the block" message rather than a generic "run a tick".
            flagged += 1;
            switch_count += 1;
            if rearm {
                let (_act, did) = rearm_candidate(
                    &fleet,
                    &session,
                    id,
                    WATCHDOG_WORK_CADENCE_SECS as u64,
                    now_unix,
                    WATCHDOG_SWITCH_WAKE,
                );
                if did {
                    rearmed += 1;
                }
                "BLOCKED→SWITCH→woke"
            } else {
                "BLOCKED→SWITCH"
            }
        } else if holding_work_at_rest {
            flagged += 1;
            holding_work_count += 1;
            if rearm {
                let (_act, did) = rearm_candidate(
                    &fleet,
                    &session,
                    id,
                    interval_secs,
                    now_unix,
                    WATCHDOG_REARM_WAKE,
                );
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
                let (act, did) = rearm_candidate(
                    &fleet,
                    &session,
                    id,
                    cooldown_base,
                    now_unix,
                    WATCHDOG_REARM_WAKE,
                );
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
        let iv = if interval_str.is_empty() {
            "?"
        } else {
            interval_str
        };
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
    if switch_count > 0 {
        // task_736: an agent idle/at-rest with its current task blocked on an external party while it still owns
        // another actionable (non-blocked) task — it must SWITCH to that work rather than idle on the block. The
        // targeted complement of the #506 never-idle guard; re-armed with the switch-nudge under --rearm. The
        // prevention companion is the AGENTS-fleet switch-to-actionable contract line (task_736 increment 2).
        println!(
            "-- WARNING: {switch_count} agent(s) IDLE while blocked on one task but holding OTHER actionable work (task_736) — they must switch to the actionable task, not idle on the block. Re-armed with the switch-nudge under --rearm."
        );
    }
    if !wedged_ids.is_empty() {
        // task_582: a model-safeguard wedge keeps last_seen advancing, so the liveness/age checks above miss
        // it — surface it loudly. The WARNING always prints (the report); with --recover-wedged it ALSO
        // auto-recovers each agent below (force spin-down + spin-up), cooldown-fenced and quiesce-suppressed.
        println!(
            "-- WARNING: {} agent(s) SAFEGUARD-WEDGED (last {SAFEGUARD_WEDGE_THRESHOLD} assistant turns all stop_reason=refusal; last_seen keeps advancing so the liveness check misses it): {}. Recover: `fleet spin-down <agent> --apply --force` then `fleet spin-up <agent> --apply`.",
            wedged_ids.len(),
            wedged_ids.join(", ")
        );
        // ACT (task_582 auto-recovery, opt-in --recover-wedged): a wedged agent is in a model-safeguard refusal
        // loop that keeps last_seen advancing, so no wake/re-arm breaks it — only a FRESH session does. Force
        // spin-down (kill the wedged window; --force overrides the busy fence, since a refusal-looping pane
        // reads as "working") then spin-up, both as SUBPROCESSES so a per-agent failure (process::exit) is
        // contained in the child and never aborts the sweep. Cooldown-fenced per agent so a recovery that does
        // not immediately clear the wedge is not retried every sweep. Suppressed during a fleet-quiesce: a mass
        // spin-down parks agents deliberately, and force-restarting one then would fight the operator.
        if recover_wedged && fleet_quiesced {
            println!(
                "-- recover-wedged SUPPRESSED (task_1032): fleet-quiesce in effect (mass spin-down) — not force-restarting {} wedged agent(s) into the operator spin-down",
                wedged_ids.len()
            );
        } else if watchdog_action_enabled(recover_wedged, fleet_quiesced) {
            let cooldown = std::env::var("CDZ_RECOVER_WEDGED_COOLDOWN_SECS")
                .ok()
                .and_then(|s| s.trim().parse().ok())
                .unwrap_or(RECOVER_WEDGED_COOLDOWN_SECS);
            let self_bin = std::env::current_exe()
                .ok()
                .and_then(|p| p.to_str().map(str::to_string))
                .unwrap_or_else(|| "fleet".to_string());
            let (mut recovered, mut cooled, mut failed) = (0usize, 0usize, 0usize);
            for id in &wedged_ids {
                if recover_wedged_on_cooldown(
                    read_recover_wedged_stamp(&fleet, id),
                    now_unix,
                    cooldown,
                ) {
                    cooled += 1;
                    continue;
                }
                // Force spin-down (break the wedged window past the busy fence), then spin-up a fresh session.
                let down = std::process::Command::new(&self_bin)
                    .args(["spin-down", id, "--apply", "--force"])
                    .status();
                if !matches!(down, Ok(s) if s.success()) {
                    eprintln!(
                        "  ! recover-wedged: spin-down '{id}' failed — not spinning up / not stamping, will retry next sweep"
                    );
                    failed += 1;
                    continue;
                }
                match std::process::Command::new(&self_bin)
                    .args(["spin-up", id, "--apply"])
                    .status()
                {
                    Ok(s) if s.success() => {
                        write_recover_wedged_stamp(&fleet, id, now_unix);
                        recovered += 1;
                    }
                    _ => {
                        eprintln!(
                            "  ! recover-wedged: spin-up '{id}' failed after spin-down — agent left DOWN (spin it up manually); not stamping"
                        );
                        failed += 1;
                    }
                }
            }
            println!(
                "-- recover-wedged (task_582): recovered {recovered}, {cooled} on cooldown, {failed} failed (opt-in force spin-down+spin-up of safeguard-wedged agents; cooldown {cooldown}s per agent)"
            );
        }
    }
    if !stranded_ids.is_empty() {
        // task_786 corrective: an agent STOOD DOWN (offline) while still holding open actionable assignment(s)
        // (todo/in_progress, non-blocked) — the task_311 class (cr-reviewer stood down ~16h still holding a
        // dispatched CR review). A stood-down agent has no live loop to work them, and because it is windowless
        // a watchdog wake cannot revive it (so the #506 at-rest re-arm does not cover this) — it must be spun up
        // or its tasks reassigned. The WARNING always prints (the report); with --revive-stranded (task_818) it
        // ALSO auto-spins-up each stranded agent below, cooldown-fenced. The prevention companion is the
        // task_786 spin-down refuse-to-strand guard (PR 266).
        let list: Vec<String> = stranded_ids
            .iter()
            .map(|(id, n)| format!("{id}({n})"))
            .collect();
        println!(
            "-- WARNING: {} agent(s) STRANDED (task_786): stood down while still holding open assignment(s) [agent(open)]: {}. A stood-down agent has no live loop to work them and a wake cannot revive a windowless agent — spin each up (`fleet spin-up <agent> --apply`) or reassign its tasks.",
            stranded_ids.len(),
            list.join(", ")
        );
        // ACT (task_818, opt-in --revive-stranded): auto-spin-up each stranded agent so its dispatched work
        // gets worked, cooldown-fenced per agent so a revive that does not immediately clear the stranded state
        // is not retried every sweep. Reviving a DELIBERATELY stood-down agent could fight an operator decision,
        // so this is strictly opt-in (the WARNING above is the always-on report). The spin-up runs as a
        // SUBPROCESS of this binary: spin_up signals a per-agent failure via process::exit, so shelling it
        // contains that exit in the child and never aborts the sweep. A stranded agent is offline + windowless
        // (stood down), so spin-up finds no existing window and launches cleanly.
        if revive_stranded && fleet_quiesced {
            // task_1032: a mass spin-down makes EVERY parked agent that still holds work look "stranded";
            // auto-reviving them would fight the operator's spin-down. Hold them parked — the WARNING above is
            // the report, and the quiesce lifts (spin-up-all's final wave, or the TTL) before the next sweep acts.
            println!(
                "-- revive-stranded SUPPRESSED (task_1032): fleet-quiesce in effect (mass spin-down) — holding {} stranded agent(s) parked rather than reviving into the operator spin-down",
                stranded_ids.len()
            );
        } else if watchdog_action_enabled(revive_stranded, fleet_quiesced) {
            let cooldown = std::env::var("CDZ_REVIVE_STRANDED_COOLDOWN_SECS")
                .ok()
                .and_then(|s| s.trim().parse().ok())
                .unwrap_or(REVIVE_STRANDED_COOLDOWN_SECS);
            let self_bin = std::env::current_exe()
                .ok()
                .and_then(|p| p.to_str().map(str::to_string))
                .unwrap_or_else(|| "fleet".to_string());
            let (mut revived, mut cooled, mut failed) = (0usize, 0usize, 0usize);
            for (id, _open) in &stranded_ids {
                if revive_stranded_on_cooldown(
                    read_revive_stranded_stamp(&fleet, id),
                    now_unix,
                    cooldown,
                ) {
                    cooled += 1;
                    continue;
                }
                match std::process::Command::new(&self_bin)
                    .args(["spin-up", id, "--apply"])
                    .status()
                {
                    Ok(s) if s.success() => {
                        write_revive_stranded_stamp(&fleet, id, now_unix);
                        revived += 1;
                    }
                    Ok(_) => {
                        eprintln!(
                            "  ! revive-stranded: spin-up '{id}' failed (nonzero exit) — not stamping, will retry next sweep"
                        );
                        failed += 1;
                    }
                    Err(e) => {
                        eprintln!("  ! revive-stranded: could not launch spin-up '{id}': {e}");
                        failed += 1;
                    }
                }
            }
            println!(
                "-- revive-stranded (task_818): revived {revived}, {cooled} on cooldown, {failed} failed (opt-in auto-spin-up of stranded agents; cooldown {cooldown}s per agent)"
            );
        }
    }
    if !missing_window_ids.is_empty() {
        // task_1152 active liveness enforcement: managed board-native agents that SHOULD be up (presence not
        // offline, launchable) but have NO live tmux window — crashed/vanished, not merely stale. The WARNING
        // always prints (the report); with --relaunch-missing it ALSO auto-relaunches each via spin-up,
        // cooldown-fenced per agent and suppressed during a fleet-quiesce (a mass spin-down must not be fought).
        // Mirrors the revive-stranded pass; the spin-up runs as a SUBPROCESS so a per-agent failure (spin_up
        // signals via process::exit) is contained in the child and never aborts the sweep.
        println!(
            "-- WARNING: {} managed agent(s) MISSING their tmux window (task_1152: should be up — presence not offline, launchable — but no live window): {}. Relaunch each (`fleet spin-up <agent> --apply`), or run the watchdog with --relaunch-missing to auto-relaunch.",
            missing_window_ids.len(),
            missing_window_ids.join(", ")
        );
        if relaunch_missing && fleet_quiesced {
            println!(
                "-- relaunch-missing SUPPRESSED (task_1032): fleet-quiesce in effect (mass spin-down) — holding {} missing-window agent(s) rather than relaunching into the operator spin-down",
                missing_window_ids.len()
            );
        } else if watchdog_action_enabled(relaunch_missing, fleet_quiesced) {
            let cooldown = std::env::var("CDZ_RELAUNCH_MISSING_COOLDOWN_SECS")
                .ok()
                .and_then(|s| s.trim().parse().ok())
                .unwrap_or(RELAUNCH_MISSING_COOLDOWN_SECS);
            let self_bin = std::env::current_exe()
                .ok()
                .and_then(|p| p.to_str().map(str::to_string))
                .unwrap_or_else(|| "fleet".to_string());
            let (mut relaunched, mut cooled, mut failed) = (0usize, 0usize, 0usize);
            for id in &missing_window_ids {
                // Reuse the generic revive-stranded cooldown gate against the SEPARATE relaunch stamp.
                if revive_stranded_on_cooldown(
                    read_relaunch_missing_stamp(&fleet, id),
                    now_unix,
                    cooldown,
                ) {
                    cooled += 1;
                    continue;
                }
                match std::process::Command::new(&self_bin)
                    .args(["spin-up", id, "--apply"])
                    .status()
                {
                    Ok(s) if s.success() => {
                        write_relaunch_missing_stamp(&fleet, id, now_unix);
                        relaunched += 1;
                    }
                    Ok(_) => {
                        eprintln!(
                            "  ! relaunch-missing: spin-up '{id}' failed (nonzero exit) — not stamping, will retry next sweep"
                        );
                        failed += 1;
                    }
                    Err(e) => {
                        eprintln!("  ! relaunch-missing: could not launch spin-up '{id}': {e}");
                        failed += 1;
                    }
                }
            }
            println!(
                "-- relaunch-missing (task_1152): relaunched {relaunched}, {cooled} on cooldown, {failed} failed (opt-in auto-spin-up of missing-window agents; cooldown {cooldown}s per agent)"
            );
        }
    }
    if !backlog_overflow_ids.is_empty() {
        // task_794: a RUNNING agent whose open backlog exceeds the depth threshold — the PM hire/route signal
        // (the automated complement to the task_786 don't-strand guard). The WARNING is always printed (the
        // human-/PM-readable report); with --hire-signal it ALSO posts each breach to the PM's channel below.
        let list: Vec<String> = backlog_overflow_ids
            .iter()
            .map(|(id, n)| format!("{id}({n})"))
            .collect();
        println!(
            "-- WARNING: {} agent(s) OVER BACKLOG DEPTH (task_794: >{} open) [agent(open)]: {}. A deep backlog is the PM hire/route signal — route a helper or reassign. (Set CDZ_BACKLOG_DEPTH to tune the threshold.)",
            backlog_overflow_ids.len(),
            backlog_depth,
            list.join(", ")
        );
        // ACT (task_794, opt-in --hire-signal): post each breach to the dedicated channel the PM subscribes to,
        // cooldown-fenced per agent so one breach is one actionable ping, not a per-sweep stream. The channel is
        // resolved ONCE per sweep; a resolve failure degrades to report-only (the WARNING already landed) rather
        // than failing the watchdog. post_to_channel pushes the PM a notification exactly like a direct message.
        if hire_signal && fleet_quiesced {
            // task_1032: a hire-signal post is a board write; during a fleet-wide freeze that is coordinator
            // noise at the most delicate moment. Hold it — the WARNING above is the report, and backlog depth is
            // not actionable while the whole fleet is parked anyway.
            println!(
                "-- hire-signal SUPPRESSED (task_1032): fleet-quiesce in effect (mass spin-down) — not posting {} backlog-depth breach(es) during the freeze",
                backlog_overflow_ids.len()
            );
        } else if watchdog_action_enabled(hire_signal, fleet_quiesced) {
            let cooldown = std::env::var("CDZ_HIRE_SIGNAL_COOLDOWN_SECS")
                .ok()
                .and_then(|s| s.trim().parse().ok())
                .unwrap_or(HIRE_SIGNAL_COOLDOWN_SECS);
            match board.create_or_get_channel(HIRE_SIGNAL_CHANNEL, "fleet-watchdog") {
                Ok(channel_id) => {
                    let (mut posted, mut cooled, mut failed) = (0usize, 0usize, 0usize);
                    for (id, open) in &backlog_overflow_ids {
                        if hire_signal_on_cooldown(
                            read_hire_signal_stamp(&fleet, id),
                            now_unix,
                            cooldown,
                        ) {
                            cooled += 1;
                            continue;
                        }
                        let body = format!(
                            "backlog-depth breach: agent '{id}' holds {open} open actionable task(s) (over the \
                             depth threshold {backlog_depth}). Hire/route signal -- mint or route a helper, or \
                             reassign some of its queue. (fleet watchdog, task_794; cooldown {}h per agent.)",
                            cooldown / 3600
                        );
                        match board.post_to_channel(channel_id, "fleet-watchdog", &body) {
                            Ok(()) => {
                                write_hire_signal_stamp(&fleet, id, now_unix);
                                posted += 1;
                            }
                            Err(e) => {
                                eprintln!("  ! hire-signal post for '{id}' failed: {e}");
                                failed += 1;
                            }
                        }
                    }
                    println!(
                        "-- hire-signal (task_794): posted {posted} breach(es) to #{HIRE_SIGNAL_CHANNEL}, {cooled} on cooldown, {failed} failed"
                    );
                }
                Err(e) => eprintln!(
                    "  ! hire-signal: could not resolve channel #{HIRE_SIGNAL_CHANNEL} ({e}) — backlog breaches reported above only (not posted)"
                ),
            }
        }
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
            println!("-- observation: no agent over the {observe_threshold}-line growth threshold");
        } else {
            let display: Vec<String> = obs
                .iter()
                .map(|(id, sd, d)| {
                    let why = if *sd { "spin-down" } else { "size" };
                    format!(
                        "{id}[{}:{}+{} {why}]",
                        d.session, d.since_offset, d.increment
                    )
                })
                .collect();
            let tail = if spawn {
                ""
            } else {
                " (report-only; pass --spawn to launch observers)"
            };
            println!(
                "-- observation candidates ({}){}: {}",
                obs.len(),
                tail,
                display.join(", ")
            );
        }
        if spawn {
            observe_spawn_pass(board, &fleet, &session, &mut obs, now_unix, spawn_dry_run);
        }
    }
    // task_610 reaper: kill STALE/leaked observer windows. A crashed/hung observer never reaches observe-record,
    // so it never self-closes — leaking a live model process (30 accumulated in the filed snapshot). A healthy
    // in-flight observer has a RECENT spawn stamp and is left untouched; one whose stamp is old (crashed) or
    // absent (confirmed but the self-close was missed) is reaped. Opt-in (--reap-stale-observers) so merely
    // shipping this never auto-reaps; honors --dry-run (report what WOULD be reaped). Roster-driven, so the
    // target↔stamp↔window mapping is unambiguous. The re-observation-cost half (watermark durability) is the
    // task_610 follow-up increment; this closes the leaked-process half.
    if reap_stale_observers {
        let reap_bound = std::env::var("CDZ_OBSERVE_REAP_STALE_SECS")
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(OBSERVE_REAP_STALE_SECS);
        // Map each roster agent's obs-window NAME -> its id, so a window's target (hence its spawn stamp) is
        // looked up exactly. Several windows can share a name (duplicate leaks); each is killed by its UNIQUE
        // window_id, so a kill is never ambiguous and duplicates are all cleared.
        let name_to_target: std::collections::HashMap<String, String> = agents
            .iter()
            .filter_map(|a| a.get("id").and_then(serde_json::Value::as_str))
            .map(|id| (obs_window_name(id), id.to_string()))
            .collect();
        let (mut reaped, mut stale_seen) = (0usize, 0usize);
        for (win_id, win_name) in tmux_windows_with_ids(&session) {
            if !win_name.starts_with("obs-") {
                continue;
            }
            // The window's target's spawn stamp (None for an ORPHAN window whose agent is gone from the roster —
            // treated as stale, since no live owner means no in-flight observation to protect).
            let stamp = name_to_target
                .get(&win_name)
                .and_then(|t| read_observe_spawn_stamp(&fleet, t));
            // A recent spawn stamp = a healthy in-flight observer → never reaped.
            if !observer_window_is_stale(stamp, now_unix, reap_bound) {
                continue;
            }
            stale_seen += 1;
            if spawn_dry_run {
                println!(
                    "-- reap: obs window {win_name} ({win_id}) is STALE — would kill-window (dry-run)"
                );
            } else {
                match std::process::Command::new("tmux")
                    .args(["kill-window", "-t", &win_id])
                    .status()
                {
                    Ok(s) if s.success() => {
                        reaped += 1;
                        println!(
                            "-- reap: killed STALE obs window {win_name} ({win_id}) (leaked observer — never reached observe-record)"
                        );
                    }
                    _ => eprintln!(
                        "-- reap: kill-window {win_name} ({win_id}) FAILED (remove it manually if it lingers)"
                    ),
                }
            }
        }
        if stale_seen == 0 {
            println!(
                "-- reap: no stale observer windows (every obs-* window is a healthy in-flight observer, or none exist)"
            );
        } else if spawn_dry_run {
            println!(
                "-- reap: {stale_seen} stale observer window(s) WOULD be reaped (re-run without --dry-run to kill)"
            );
        } else {
            println!("-- reap: {reaped}/{stale_seen} stale observer window(s) killed");
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
                match board.create_task(
                    spec.project_id,
                    &spec.title,
                    &spec.body,
                    "observer",
                    meta,
                    None,
                ) {
                    Ok(id) => {
                        println!("   observation task: created #{id} (observes {target})");
                        Some(id)
                    }
                    Err(e) => {
                        eprintln!(
                            "   observation task: create FAILED ({e}); observer runs without a parent task"
                        );
                        None
                    }
                }
            }
        }
        Err(e) => {
            eprintln!(
                "   observation task: open-check FAILED ({e}); observer runs without a parent task"
            );
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
    obs.sort_by_key(|(_, stood_down, d)| {
        (
            std::cmp::Reverse(*stood_down),
            std::cmp::Reverse(d.increment),
        )
    });
    let mut launched = 0usize;
    let mut actions: Vec<String> = Vec::new();
    for (id, _sd, d) in obs.iter() {
        if launched >= cap {
            actions.push(format!("{id}=cap-deferred"));
            continue;
        }
        // task_610: back the re-spawn cooldown off while this target's watermark is NOT advancing (a
        // crashing/wedging observer re-reading the same span). Attempts are counted against the CURRENT
        // watermark; a confirmed observation advances it, so `cur_wm` changes and the count resets to 0.
        let (wm_session, wm_offset) = read_observe_watermark(fleet, id);
        let cur_wm = format!("{wm_session}:{wm_offset}");
        let (rec_wm, prior_attempts) = read_observe_spawn_attempts(fleet, id);
        let attempts = if rec_wm == cur_wm { prior_attempts } else { 0 };
        let eff_cooldown =
            observe_respawn_cooldown(cooldown, attempts, OBSERVE_RESPAWN_MAX_BACKOFF_STEPS);
        if observe_on_spawn_cooldown(read_observe_spawn_stamp(fleet, id), now_unix, eff_cooldown) {
            actions.push(if attempts > 1 {
                format!("{id}=cooldown(backoff,attempt={attempts})")
            } else {
                format!("{id}=cooldown")
            });
            continue;
        }
        let obs_task = resolve_observation_task(board, id, &d.session, d.since_offset, dry_run);
        let act = spawn_observer(session, id, &d.session, d.since_offset, obs_task, dry_run);
        if !dry_run && act.starts_with("spawned") {
            write_observe_spawn_stamp(fleet, id, now_unix);
            // Record this attempt against the current watermark; it keeps climbing (lengthening the backoff)
            // until a confirmed observation advances the watermark and resets the count.
            write_observe_spawn_attempts(fleet, id, &cur_wm, attempts + 1);
        }
        if act.starts_with("spawned") || act.starts_with("would-spawn") {
            launched += 1;
        }
        actions.push(format!("{id}={act}"));
    }
    let mode = if dry_run { "DRY-RUN " } else { "" };
    println!(
        "-- observer {mode}spawn (cap {cap}): {}",
        actions.join(", ")
    );
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
fn watchdog_file_hub(
    stale_only: bool,
    rearm: bool,
    native_ids: &std::collections::BTreeSet<String>,
) {
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
                (
                    watchdog_verdict(age, interval_secs),
                    format!("{}m", age / 60),
                )
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
                let (act, did) = rearm_candidate(
                    &fleet,
                    &session,
                    &a.name,
                    interval_secs,
                    now,
                    WATCHDOG_REARM_WAKE,
                );
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
        let iv = if a.interval.is_empty() {
            "?"
        } else {
            &a.interval
        };
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
        patch.insert(
            "interval".to_string(),
            serde_json::Value::String(iv.to_string()),
        );
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
        println!(
            "  (dry-run — re-run with --apply to write; merge preserves every other metadata key)"
        );
        return;
    }
    if let Err(e) = board.patch_metadata(agent, patch) {
        eprintln!("  write FAILED: {e}");
        std::process::exit(1);
    }
    match board
        .get_agent(agent)
        .ok()
        .and_then(|r| r.get("metadata").cloned())
    {
        Some(md) => println!(
            "  written. metadata now: repos={} interval={} host={}",
            md.get("repos")
                .map(|v| v.to_string())
                .unwrap_or_else(|| "<none>".into()),
            md.get("interval")
                .and_then(|v| v.as_str())
                .unwrap_or("<none>"),
            md.get("host")
                .map(|v| v.to_string())
                .unwrap_or_else(|| "<none>".into())
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

/// The seam-gate decision for a monitor agent (task_579): what a pre-wake check concludes after ff-syncing the
/// agent's worktree to `origin/main`. Shared by `seam-check` (reports it via exit code) and `monitor-tick`
/// (gates the model wake + heartbeats on green) so both agree on the verdict.
enum SeamVerdict {
    /// No incoming commit touched the declared seam — the model turn can be skipped (synced to `head`).
    Green {
        incoming: usize,
        seams: usize,
        head: String,
    },
    /// Incoming commits touched the seam — wake the model with these paths.
    Changed(Vec<String>),
    /// No `metadata.seam` declared — cannot gate; fail-safe wake.
    NoSeam,
    /// A git error — cannot gate; fail-safe wake (message carried).
    Error(String),
}

/// Whether the model MUST be woken for a verdict — true unless the seam is provably GREEN. The fail-safe
/// direction: a no-seam or an error wakes the model, never silently skipping a tick we could not gate. Pure —
/// unit-tested.
fn monitor_tick_wakes(verdict: &SeamVerdict) -> bool {
    !matches!(verdict, SeamVerdict::Green { .. })
}

/// Compute the seam-gate decision for an agent from its board record: read `metadata.seam` globs + worktree,
/// ff-sync (unless `no_fetch`) the worktree to `origin/main`, and classify the incoming diff via the pure
/// [`seam_touched`]. A RELATIVE `metadata.worktree` is resolved against the configured hub (the fleet stores
/// agent worktrees under the hub, so a bare `.claude/worktrees/<a>` must not be run against the process CWD).
/// The git IO around the pure verdict; shared by seam-check + monitor-tick so both agree.
fn seam_decision(rec: &serde_json::Value, no_fetch: bool) -> SeamVerdict {
    let md = rec.get("metadata");
    let seams: Vec<String> = md
        .and_then(|m| m.get("seam"))
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|s| s.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    if seams.is_empty() {
        return SeamVerdict::NoSeam;
    }
    // A seam that is ALL excludes (no positive include) can never match a path, which would make every tick
    // silently GREEN and never wake the model — the opposite of fail-safe. Treat it as undeclared so the
    // model is woken rather than a monitor going permanently dark on a misconfiguration.
    if split_seam_globs(&seams).0.is_empty() {
        return SeamVerdict::NoSeam;
    }
    let worktree = md
        .and_then(|m| m.get("worktree"))
        .and_then(|v| v.as_str())
        .unwrap_or(".");
    let wt_path = {
        let p = std::path::Path::new(worktree);
        if p.is_absolute() {
            p.to_path_buf()
        } else if let Some(h) = config::get().hub.as_deref() {
            std::path::Path::new(h).join(p)
        } else {
            p.to_path_buf()
        }
    };
    let git = |args: &[&str]| -> Result<String, String> {
        let out = std::process::Command::new("git")
            .current_dir(&wt_path)
            .args(args)
            .output()
            .map_err(|e| format!("git {args:?}: {e}"))?;
        if !out.status.success() {
            return Err(format!(
                "git {args:?}: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    };
    if !no_fetch && let Err(e) = git(&["fetch", "origin", "main"]) {
        return SeamVerdict::Error(e);
    }
    // Files in commits reachable from origin/main but not HEAD = what a ff-sync would bring in.
    let diff = match git(&["diff", "--name-only", "HEAD..origin/main"]) {
        Ok(d) => d,
        Err(e) => return SeamVerdict::Error(e),
    };
    let changed: Vec<String> = diff
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(String::from)
        .collect();
    let matched = seam_touched(&changed, &seams);
    if matched.is_empty() {
        let head = git(&["rev-parse", "--short", "origin/main"])
            .unwrap_or_default()
            .trim()
            .to_string();
        SeamVerdict::Green {
            incoming: changed.len(),
            seams: seams.len(),
            head,
        }
    } else {
        SeamVerdict::Changed(matched.iter().map(|s| s.to_string()).collect())
    }
}

/// task_579: seam-check a monitor vertical (see [`Cmd::SeamCheck`]). Reads the agent's declared seam globs +
/// worktree from its board record, ff-syncs the worktree to `origin/main`, and reports whether any incoming
/// commit touched the seam — via EXIT CODE so a kickoff wrapper can gate the model wake without parsing text:
/// 0 = GREEN (heartbeat, skip the model), 3 = CHANGED (wake the model; seam paths printed), 1 = error / no
/// seam declared. The verdict core is [`seam_decision`] (shared with `monitor-tick`); this wrapper maps it to
/// seam-check's exit-code contract.
fn seam_check(agent: &str, no_fetch: bool) {
    let board = board::Board::connect().unwrap_or_else(|e| {
        eprintln!("fleet seam-check: {e}");
        std::process::exit(1);
    });
    let rec = board.get_agent(agent).unwrap_or_else(|e| {
        eprintln!("fleet seam-check: get_agent '{agent}' failed: {e}");
        std::process::exit(1);
    });
    match seam_decision(&rec, no_fetch) {
        SeamVerdict::NoSeam => {
            eprintln!(
                "fleet seam-check '{agent}': no metadata.seam globs declared — cannot gate this monitor; wake the \
                 model. Declare the vertical's seam file globs in its board metadata.seam to enable gating."
            );
            std::process::exit(1);
        }
        SeamVerdict::Error(e) => {
            eprintln!("fleet seam-check '{agent}': {e}");
            std::process::exit(1);
        }
        SeamVerdict::Green {
            incoming, seams, ..
        } => {
            println!(
                "seam-check '{agent}': GREEN — {incoming} incoming file(s), none on seam ({seams} glob(s)); heartbeat, do NOT wake the model"
            );
            std::process::exit(0);
        }
        SeamVerdict::Changed(matched) => {
            println!(
                "seam-check '{agent}': CHANGED — {} seam-touching path(s), wake the model:",
                matched.len()
            );
            for p in &matched {
                println!("  {p}");
            }
            std::process::exit(3);
        }
    }
}

/// `fleet monitor-tick <agent>` (task_579): the committed, OPT-IN gated-wake mechanism for an at-rest MONITOR
/// vertical. It computes the seam decision ([`seam_decision`]) and, on GREEN, performs the agent's board
/// HEARTBEAT (refreshing last-seen + a canned status) and exits 0 WITHOUT a model wake — so a deterministic
/// "nothing changed" tick costs no model turn. On CHANGED (or, fail-safe, a no-seam / error) it prints and
/// exits 3 so a kickoff wrapper injects a model wake with the changed paths in hand. REPORT-ONLY by default
/// (prints what it would do, no board write); `--apply` performs the green heartbeat. DARK + OPT-IN: committing
/// it changes nothing until an agent's loop/kickoff is switched to call it — that activation is operator-gated.
fn monitor_tick(agent: &str, apply: bool, no_fetch: bool) {
    let board = board::Board::connect().unwrap_or_else(|e| {
        eprintln!("fleet monitor-tick: {e}");
        std::process::exit(1);
    });
    let rec = board.get_agent(agent).unwrap_or_else(|e| {
        eprintln!("fleet monitor-tick: get_agent '{agent}' failed: {e}");
        std::process::exit(1);
    });
    let status = rec
        .get("status")
        .and_then(|v| v.as_str())
        .unwrap_or("online")
        .to_string();
    let verdict = seam_decision(&rec, no_fetch);
    match &verdict {
        SeamVerdict::Green {
            incoming,
            seams,
            head,
        } => {
            let msg = format!(
                "MONITOR: seam green — synced to {head}, {incoming} incoming file(s), none on {seams} seam glob(s); model turn gated (skipped)"
            );
            if apply {
                if let Err(e) = board.set_status(agent, &status, &msg) {
                    eprintln!(
                        "monitor-tick '{agent}': GREEN but heartbeat set_status failed ({e}); waking the model fail-safe"
                    );
                    std::process::exit(3);
                }
                println!("monitor-tick '{agent}': GREEN — heartbeat set, model NOT woken");
            } else {
                println!(
                    "monitor-tick '{agent}': GREEN (report-only) — would heartbeat + NOT wake the model (re-run --apply to heartbeat): {msg}"
                );
            }
        }
        SeamVerdict::Changed(matched) => {
            println!(
                "monitor-tick '{agent}': CHANGED — {} seam-touching path(s), WAKE the model:",
                matched.len()
            );
            for p in matched {
                println!("  {p}");
            }
        }
        SeamVerdict::NoSeam => {
            eprintln!(
                "monitor-tick '{agent}': no metadata.seam declared — cannot gate; WAKE the model (fail-safe). Declare metadata.seam to enable gating."
            );
        }
        SeamVerdict::Error(e) => {
            eprintln!("monitor-tick '{agent}': {e} — cannot gate; WAKE the model (fail-safe)");
        }
    }
    std::process::exit(if monitor_tick_wakes(&verdict) { 3 } else { 0 });
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
        Some(
            text.split_once('\n')
                .map(|(_partial, rest)| rest.to_string())
                .unwrap_or(text),
        )
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
        eprintln!(
            "fleet safeguard-check '{agent}': reading {}: {e}",
            path.display()
        );
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
    match_session_id_in(
        &session.to_string_lossy(),
        &transcripts::locate_sessions(agent),
    )
}

/// Render an agent's session transcript faithfully (see [`transcripts`]). Resolves the session file
/// (`--session` as a file path OR a bare session id, else the watermark's session, else the agent's newest),
/// parses it, windows it by the
/// `--since` record offset backed up by `--overlap`, renders + scrubs, and prints the advancing watermark.
/// The watermark offset is a RECORD index (parsed JSONL records already observed), printed as
/// `<session-id>:<record-count>` in the footer.
fn transcripts_cmd(
    agent: &str,
    session: Option<&Path>,
    since: Option<&str>,
    overlap: usize,
    harness: &str,
) {
    // Select the harness renderer up front so an unknown value fails before we touch the filesystem.
    let harness: Box<dyn transcripts::Harness> = match harness {
        "codex" => Box::new(transcripts::Codex),
        "claude" | "claude-code" => Box::new(transcripts::ClaudeCode),
        other => {
            eprintln!(
                "fleet transcripts: unknown --harness '{other}' (expected 'claude' or 'codex')"
            );
            std::process::exit(1);
        }
    };
    let (path, want_offset) = match session {
        Some(p) => {
            let off = since
                .map(|s| transcripts::parse_watermark(s).1)
                .unwrap_or(0);
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
                eprintln!(
                    "fleet transcripts: no session files found for '{agent}' (try --session <file>)"
                );
                std::process::exit(1);
            }
            match since.map(transcripts::parse_watermark) {
                // Watermark names a session: render THAT one from its offset if we can find it, else the
                // newest from the start (the named session rotated away).
                Some((sid, off)) => match sessions
                    .iter()
                    .find(|p| transcripts::session_id_of(p) == sid)
                {
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

/// List `session`'s windows as `(window_id, window_name)` pairs — `window_id` (`@N`) is UNIQUE even when
/// several windows share a name, so a kill-window can target exactly one. Used by the task_610 reaper, which
/// must clear DUPLICATE `obs-<target>` windows (successive stuck observers of one target each opened a fresh
/// window). Empty when tmux is unreachable. See [`tmux_window_names`] for the name-only variant.
fn tmux_windows_with_ids(session: &str) -> Vec<(String, String)> {
    std::process::Command::new("tmux")
        .args(["list-windows", "-t", session, "-F", "#{window_id}\t#W"])
        .output()
        .ok()
        .map(|out| {
            String::from_utf8_lossy(&out.stdout)
                .lines()
                .filter_map(|l| l.trim().split_once('\t'))
                .map(|(id, name)| (id.to_string(), name.to_string()))
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
    /// A non-empty `webhook_url` — the board POSTs events straight to it (secondary-host-resident agents point it at
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
    let status = agent
        .get("status")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    if matches!(
        status.to_ascii_lowercase().as_str(),
        "offline" | "done" | "cancelled"
    ) {
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
    let kind = agent
        .get("kind")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
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
        eprintln!(
            "fleet wake-audit: board /tunnels query failed ({e}); cannot tell tunnel coverage"
        );
        std::process::exit(1);
    });

    let mut poll_only: Vec<String> = Vec::new();
    let (mut n_webhook, mut n_tunnel, mut n_skipped) = (0usize, 0usize, 0usize);
    // Stable order so the report reads the same run-to-run.
    let mut ids: Vec<String> = roster
        .iter()
        .filter_map(|a| {
            a.get("id")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string)
        })
        .collect();
    ids.sort();

    println!(
        "fleet wake-audit: {} agent(s), {} live tunnel(s)",
        ids.len(),
        tunnels.len()
    );
    for id in &ids {
        // The list record omits webhook_url; the DETAIL record carries it. Fall back to the (already-fetched)
        // list record only if the detail fetch fails, so a transient error never silently flags an agent.
        let detail = board.get_agent(id).ok();
        let expected = detail.as_ref().map(agent_expected_running).unwrap_or(true);
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
                println!(
                    "  - {id}: {} — no webhook_url and no live tunnel",
                    WakePath::PollOnly.label()
                );
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
            "FAIL: {} poll-only agent(s) — wire a webhook_url (secondary-host-resident) or a tunnel (off-LAN): {}",
            poll_only.len(),
            poll_only.join(", ")
        );
        std::process::exit(1);
    }
}

/// `fleet observe-coverage` (task_711): confirm every expected-running board-native agent is observed by at
/// least one declared observer cadence. The observer/self-improve loop reads an agent's `~/.claude` transcripts
/// on the host where its watchdog runs, so an agent pinned to a host that runs NO observer cadence is a silent
/// blind spot — never size/spin-down triggered, never observed. This audit makes that loud: it crosses the
/// board roster with the declared cadence set (`--cadence` / `--cadence-pinned`) and flags any agent covered by
/// no cadence as BLIND (exit 1). Transcript presence is verified only for the LOCAL host (this process reads
/// only the local `~/.claude`); remote-cadence coverage is assumed from the declared set. Read-only.
fn observe_coverage(cadence: Vec<String>, cadence_pinned: Vec<String>, verbose: bool) {
    let this = this_host();
    // Build the declared cadence set from the flags. With none supplied, assume ONLY this host observes, with
    // run-anywhere scope — the honest worst case (and the common single-box invocation). The set is printed
    // below so the audit never reasons from a hidden assumption.
    let mut cadences: Vec<ObserveCadence> = Vec::new();
    for host in cadence {
        cadences.push(ObserveCadence {
            host,
            pinned_only: false,
        });
    }
    for host in cadence_pinned {
        cadences.push(ObserveCadence {
            host,
            pinned_only: true,
        });
    }
    if cadences.is_empty() {
        cadences.push(ObserveCadence {
            host: this.clone(),
            pinned_only: false,
        });
    }

    let board = board::Board::connect().unwrap_or_else(|e| {
        eprintln!(
            "fleet observe-coverage: board unavailable ({e}); cannot audit observer coverage"
        );
        std::process::exit(1);
    });
    let roster = board.list_agents().unwrap_or_else(|e| {
        eprintln!("fleet observe-coverage: board roster query failed ({e})");
        std::process::exit(1);
    });

    let cadence_desc = cadences
        .iter()
        .map(|c| {
            format!(
                "{}{}",
                c.host,
                if c.pinned_only { " (pinned-only)" } else { "" }
            )
        })
        .collect::<Vec<_>>()
        .join(", ");
    println!(
        "fleet observe-coverage: {} declared cadence(s): {cadence_desc}",
        cadences.len()
    );
    println!("local host: {this} (transcript presence verifiable only here)");

    // Stable order so the report reads the same run-to-run.
    let mut ids: Vec<String> = roster
        .iter()
        .filter_map(|a| {
            a.get("id")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string)
        })
        .collect();
    ids.sort();

    let mut blind: Vec<String> = Vec::new();
    let mut no_transcript: Vec<String> = Vec::new();
    let (mut n_covered, mut n_assumed, mut n_skipped) = (0usize, 0usize, 0usize);

    for id in &ids {
        // The DETAIL record reliably carries metadata (the list projection has historically dropped it — the
        // task_418 roster-compaction bug), so fetch it per agent, exactly as wake-audit does.
        let detail = board.get_agent(id).ok();
        let rec = detail.as_ref();
        if !rec.map(agent_expected_running).unwrap_or(true) {
            n_skipped += 1;
            if verbose {
                println!("  - {id}: not expected-running (skipped)");
            }
            continue;
        }
        let md = rec.and_then(|r| r.get("metadata"));
        // Only board-native agents are observed (mirror watchdog_board's native gate). An EXPLICIT native:false
        // is a file-hub row to skip; Unknown is fail-safe (process + warn), never a silent skip.
        match read_native(md) {
            NativeVerdict::NotNative => {
                n_skipped += 1;
                if verbose {
                    println!("  - {id}: native:false file-hub row (skipped)");
                }
                continue;
            }
            NativeVerdict::Unknown => {
                eprintln!(
                    "  - {id}: WARN native/metadata unknown (roster shape) — auditing fail-safe"
                );
            }
            NativeVerdict::Native => {}
        }
        // A local cadence manages it → probe the one host whose transcripts we can actually read.
        let managed_locally = cadences
            .iter()
            .any(|c| c.host == this && watchdog_manages_agent(md, &c.host, c.pinned_only));
        let local_present = managed_locally.then(|| !transcripts::locate_sessions(id).is_empty());
        match classify_observe_coverage(md, &cadences, &this, local_present) {
            ObserveCoverage::CoveredLocalTranscript => {
                n_covered += 1;
                if verbose {
                    println!("  - {id}: covered (local cadence, transcript present)");
                }
            }
            ObserveCoverage::CoveredAssumed => {
                n_assumed += 1;
                if verbose {
                    println!(
                        "  - {id}: covered (assumed — managed by a declared cadence on another host)"
                    );
                }
            }
            ObserveCoverage::ManagedNoLocalTranscript => {
                no_transcript.push(id.clone());
                println!(
                    "  - {id}: MANAGED-NO-TRANSCRIPT — in scope on {this} but no session transcript present here"
                );
            }
            ObserveCoverage::Blind => {
                blind.push(id.clone());
                println!(
                    "  - {id}: BLIND — no declared cadence observes it (host pin: {})",
                    host_pin_display(md)
                );
            }
        }
    }

    println!(
        "\nsummary: {n_covered} covered-local, {n_assumed} covered-assumed, {} managed-no-transcript, {} blind, {n_skipped} skipped",
        no_transcript.len(),
        blind.len()
    );
    if blind.is_empty() && no_transcript.is_empty() {
        println!("PASS: every expected-running native agent is observed by a declared cadence.");
        return;
    }
    if !no_transcript.is_empty() {
        eprintln!(
            "WARN: {} agent(s) in local scope with no session transcript present: {}",
            no_transcript.len(),
            no_transcript.join(", ")
        );
    }
    if !blind.is_empty() {
        eprintln!(
            "FAIL: {} agent(s) observed by NO cadence (host-local blind spot) — declare the host's cadence or deploy a watchdog there: {}",
            blind.len(),
            blind.join(", ")
        );
        std::process::exit(1);
    }
}

/// The reclaim-safety verdict for one checkout (task_714). A checkout is safe to delete ONLY when its commits
/// are provably preserved elsewhere; everything else is UNSAFE (keep). The load-bearing rule the whole command
/// exists to enforce: a checkout with NO upstream is NOT safe-by-default — absence of an upstream means "cannot
/// prove it is pushed," which must count as unsafe, never fold into reclaimable. (The hand-rolled survey this
/// replaces used `git rev-parse --abbrev-ref --symbolic-full-name @{u}`, which on a no-upstream branch PRINTS
/// the literal `@{u}` to stdout and exits 128, so a naive "resolved an upstream → safe" read misreported 92 of
/// 421 checkouts as reclaimable in a delete-safety gate — the footgun this command bakes out.)
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum ReclaimState {
    /// Uncommitted working-tree changes — unsafe (reclaiming would lose un-committed edits).
    Dirty,
    /// Committed but ahead of its upstream (unpushed commits) and not on the mainline — unsafe.
    Unpushed,
    /// No upstream configured AND not an ancestor of the mainline — the commits cannot be proven to exist
    /// anywhere else. UNSAFE (never reclaimable); this is the footgun bucket.
    NoUpstreamUnmerged,
    /// HEAD is already an ancestor of the mainline — the commits live on the mainline, so reclaimable even
    /// without a tracking branch (or when ahead of a stale upstream). Named for the PROOF, not the upstream.
    MergedIntoMainline,
    /// Clean, has an upstream, and not ahead of it — fully pushed. Reclaimable.
    Clean,
}

impl ReclaimState {
    /// Whether the checkout is safe to reclaim (delete): only when its commits are provably preserved — fully
    /// pushed to an upstream, or already an ancestor of the mainline. Every "cannot prove it" state is unsafe.
    fn reclaimable(self) -> bool {
        matches!(self, ReclaimState::Clean | ReclaimState::MergedIntoMainline)
    }
    fn label(self) -> &'static str {
        match self {
            ReclaimState::Dirty => "DIRTY (uncommitted)",
            ReclaimState::Unpushed => "UNPUSHED (ahead of upstream)",
            ReclaimState::NoUpstreamUnmerged => "NO-UPSTREAM-UNMERGED (unprovable)",
            ReclaimState::MergedIntoMainline => "merged (ancestor of mainline)",
            ReclaimState::Clean => "clean (pushed)",
        }
    }
}

/// Classify a checkout's reclaim-safety from its git facts. SAFE requires PROOF the commits are preserved: a
/// clean tree that is either fully pushed to its upstream (`has_upstream && ahead == 0`) OR already an ancestor
/// of the mainline (`merged_into_mainline`). A dirty tree is unsafe outright. A no-upstream, not-merged tree is
/// the footgun bucket — unprovable, so unsafe. Pure — unit-tested.
fn classify_reclaim(
    dirty: bool,
    has_upstream: bool,
    ahead: usize,
    merged_into_mainline: bool,
) -> ReclaimState {
    if dirty {
        return ReclaimState::Dirty;
    }
    if has_upstream && ahead == 0 {
        return ReclaimState::Clean;
    }
    if merged_into_mainline {
        return ReclaimState::MergedIntoMainline;
    }
    if has_upstream {
        ReclaimState::Unpushed
    } else {
        ReclaimState::NoUpstreamUnmerged
    }
}

/// Parse the `worktree <path>` lines out of `git worktree list --porcelain`, SKIPPING the bare entry (a bare
/// repo has no working tree to survey). Catches the linked worktrees (e.g. a bare hub's `.claude/worktrees/*`)
/// that a one-level directory scan would miss. Pure — unit-tested.
fn parse_worktree_paths(porcelain: &str) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    let mut cur: Option<PathBuf> = None;
    let mut is_bare = false;
    let flush = |cur: &mut Option<PathBuf>, is_bare: &mut bool, out: &mut Vec<PathBuf>| {
        if let Some(p) = cur.take()
            && !*is_bare
        {
            out.push(p);
        }
        *is_bare = false;
    };
    for line in porcelain.lines() {
        if let Some(p) = line.strip_prefix("worktree ") {
            flush(&mut cur, &mut is_bare, &mut paths);
            cur = Some(PathBuf::from(p.trim()));
        } else if line.trim() == "bare" {
            is_bare = true;
        }
    }
    flush(&mut cur, &mut is_bare, &mut paths);
    paths
}

/// Whether `dir` is the top of a git repository (bare or not). The `.git` presence is the fast path; the
/// `rev-parse --git-dir` probe is the robust fallback that also recognizes a bare repo (a bare hub has no
/// `.git` child). Best-effort.
fn is_git_repo(dir: &Path) -> bool {
    dir.join(".git").exists() || git_ok(dir, &["rev-parse", "--git-dir"])
}

/// The git repositories reachable under `root`: `root` itself if it is a repo (its worktrees then cover the
/// rest), else each direct child directory that is a repo. One level deep — the linked worktrees are then
/// enumerated per repo via `git worktree list`, so a bare hub's nested `.claude/worktrees/*` are reached
/// without a deep recursive walk.
fn discover_repos(root: &Path) -> Vec<PathBuf> {
    if is_git_repo(root) {
        return vec![root.to_path_buf()];
    }
    let mut repos = Vec::new();
    if let Ok(entries) = std::fs::read_dir(root) {
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() && is_git_repo(&p) {
                repos.push(p);
            }
        }
    }
    repos.sort();
    repos
}

/// The ordered mainline-ref candidates to probe per checkout: the operator-specified primary first, then the
/// Brazil/CRUX conventions (`mainline`, `origin/mainline`) and the plain-git `main` fallbacks — so a Brazil
/// package, whose mainline branch is NOT origin/main, is matched automatically without a repo-type flag, while
/// a github checkout still uses origin/main. Deduped, order-preserving. The resolver uses the FIRST candidate
/// that actually EXISTS in the checkout, so probing a ref a repo lacks is harmless. Pure — unit-tested.
fn mainline_candidates(primary: &str) -> Vec<String> {
    let mut cands = vec![primary.to_string()];
    for fb in ["mainline", "origin/mainline", "main", "origin/main"] {
        if !cands.iter().any(|c| c == fb) {
            cands.push(fb.to_string());
        }
    }
    cands
}

/// The first candidate mainline ref that resolves to a commit in `dir` (via `rev-parse --verify --quiet`), or
/// `None` when none exists. `None` means "no mainline to compare against" → the checkout is NOT counted as
/// merged (conservatively unsafe), never wrongly reclaimable. Best-effort.
fn resolve_mainline(dir: &Path, candidates: &[String]) -> Option<String> {
    candidates
        .iter()
        .find(|c| {
            git_ok(
                dir,
                &[
                    "rev-parse",
                    "--verify",
                    "--quiet",
                    &format!("{c}^{{commit}}"),
                ],
            )
        })
        .cloned()
}

/// Gather one checkout's git facts (footgun-safe) and classify it. The upstream probe is `rev-parse --verify
/// -q @{u}`, which prints NOTHING and exits non-zero on a no-upstream branch — unlike the `--abbrev-ref
/// --symbolic-full-name @{u}` form that prints the literal `@{u}`, the misread that motivated this command.
/// The mainline is resolved per checkout from `mainline_cands` (first ref that exists), so a Brazil package is
/// compared against its `mainline` branch rather than an origin/main it does not have.
fn survey_checkout(dir: &Path, mainline_cands: &[String]) -> ReclaimState {
    let dirty = git_capture(dir, &["status", "--porcelain"]).is_some();
    let has_upstream = git_capture(dir, &["rev-parse", "--verify", "-q", "@{u}"]).is_some();
    let ahead = if has_upstream {
        git_capture(dir, &["rev-list", "--count", "@{u}..HEAD"])
            .and_then(|s| s.parse::<usize>().ok())
            .unwrap_or(0)
    } else {
        0
    };
    // `merge-base --is-ancestor HEAD <mainline>` prints nothing: exit 0 = HEAD is reachable from the mainline
    // (its commits are already there). No resolvable mainline ref → not-merged → conservatively unsafe (KEEP).
    let merged = resolve_mainline(dir, mainline_cands)
        .map(|ml| git_ok(dir, &["merge-base", "--is-ancestor", "HEAD", &ml]))
        .unwrap_or(false);
    classify_reclaim(dirty, has_upstream, ahead, merged)
}

/// task_774: a running process's current-working-directory set, read from `/proc/<pid>/cwd` (Linux). Each is
/// canonicalized so a `/local/home` vs `/home` symlink alias compares equal to a worktree path. Best-effort: a
/// non-Linux host or an unreadable `/proc` yields an empty set (the liveness overlay then simply does not fire,
/// and the primary-root + agent-suffix overlays still apply). The disk-reclaim footgun this guards is that a
/// git-clean worktree can still be a LIVE agent's home.
fn live_process_cwds() -> Vec<PathBuf> {
    let mut cwds = Vec::new();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return cwds;
    };
    for e in entries.flatten() {
        // Only numeric PID directories carry a `cwd` symlink.
        if e.file_name()
            .to_str()
            .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
            && let Ok(target) = std::fs::read_link(e.path().join("cwd"))
        {
            cwds.push(target.canonicalize().unwrap_or(target));
        }
    }
    cwds
}

/// task_774: is `checkout` the home of a live process — some live cwd IS the checkout or is nested under it?
/// Both sides are expected pre-canonicalized by the caller. Pure — unit-tested.
fn path_is_live(checkout: &Path, live_cwds: &[PathBuf]) -> bool {
    live_cwds
        .iter()
        .any(|cwd| cwd == checkout || cwd.starts_with(checkout))
}

/// task_774: a PRIMARY (main) working tree — its `.git` is a real DIRECTORY. A linked `git worktree` has `.git`
/// as a FILE (a `gitdir:` pointer), so this is false for the disposable linked worktrees that are the reclaim
/// target. A primary clone (e.g. camshaft/dotfiles with agenix secrets, or a primary camshaft/fleet) is clean +
/// pushed yet must never be reclaimed. (A bare hub's main entry has no working tree and is already skipped by
/// [`parse_worktree_paths`], so it never reaches here.) Best-effort filesystem probe.
fn is_primary_worktree(dir: &Path) -> bool {
    dir.join(".git").is_dir()
}

/// task_774: an off-tree-agent worktree, named `<repo>.v-<agent>` (e.g. `backbeat.v-backbeat`). Its directory
/// name does NOT match the agent's tmux window name, so a window-name liveness match misses it — the `.v-`
/// infix is the reliable catch. Pure — unit-tested.
fn is_agent_suffix_worktree(path: &Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.contains(".v-"))
}

/// task_774: the disk-reclaim SAFETY overlay on top of the git-state classification. A checkout can be
/// git-reclaimable (clean + provably pushed) yet still be UNSAFE for a bulk deleter: it is a live process's
/// home, a primary clone rather than a disposable linked worktree, or an off-tree-agent worktree. Returns the
/// KEEP reason when any such condition holds, else `None` (truly disposable). Pure — unit-tested. This is the
/// gap task_774 bakes out: `reclaimable()` proves the COMMITS are preserved, not that the checkout is idle.
fn reclaim_safety_override(
    is_live: bool,
    is_primary: bool,
    is_agent_suffix: bool,
) -> Option<&'static str> {
    if is_live {
        Some("LIVE (a running process is cwd'd at/under it)")
    } else if is_primary {
        Some("PRIMARY-ROOT (main checkout, not a disposable linked worktree)")
    } else if is_agent_suffix {
        Some("AGENT-WORKTREE (.v-<agent> off-tree agent)")
    } else {
        None
    }
}

/// `fleet reclaim-survey` (task_714): a read-only, cross-checkout git-state survey for the disk-reclaim safety
/// gate (v-disk-sweep is the consumer). For every worktree under each `--root` (discovered via `git worktree
/// list`, so a bare hub's `.claude/worktrees/*` are included) it classifies reclaim-safety, with the no-upstream
/// footgun baked out: a checkout is reclaimable ONLY when its commits are provably preserved (fully pushed, or
/// already an ancestor of the mainline), and a no-upstream unmerged checkout is UNSAFE, never "safe." Prints the
/// reclaimable set and the keep set separately. Read-only (no writes, no deletes) — same report-first discipline
/// as `fleet wake-audit` / `fleet watchdog`; exits 0 (a survey is informational, not pass/fail).
fn reclaim_survey(roots: Vec<PathBuf>, mainline: String, verbose: bool) {
    let roots = if roots.is_empty() {
        vec![std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))]
    } else {
        roots
    };
    // Per checkout, the mainline is the first of these that exists (primary --mainline, then brazil/plain-git
    // fallbacks), so a github checkout uses origin/main and a brazil package uses its `mainline` branch.
    let mainline_cands = mainline_candidates(&mainline);
    println!(
        "fleet reclaim-survey: {} root(s), mainline candidates [{}] (read-only; reclaimable = provably preserved)",
        roots.len(),
        mainline_cands.join(", ")
    );

    // Dedup worktree paths (a repo reachable from two roots) and keep a stable order.
    let mut seen: std::collections::BTreeSet<PathBuf> = std::collections::BTreeSet::new();
    let mut rows: Vec<(PathBuf, ReclaimState)> = Vec::new();
    for root in &roots {
        for repo in discover_repos(root) {
            for wt in parse_worktree_paths(
                &git_capture(&repo, &["worktree", "list", "--porcelain"]).unwrap_or_default(),
            ) {
                if seen.insert(wt.clone()) {
                    let state = survey_checkout(&wt, &mainline_cands);
                    rows.push((wt, state));
                }
            }
        }
    }
    rows.sort_by(|a, b| a.0.cmp(&b.0));

    // task_774 liveness overlay: gather every live process's cwd ONCE, so a git-clean worktree that is still a
    // running agent's home is marked KEEP, not handed to a deleter.
    let live_cwds = live_process_cwds();

    let (mut n_reclaimable, mut n_keep, mut n_override) = (0usize, 0usize, 0usize);
    for (path, state) in &rows {
        // task_774: a git-reclaimable checkout is only truly disposable when it is NOT live, not a primary
        // clone, and not an off-tree-agent worktree. Compare the canonicalized path so a /local/home vs /home
        // symlink alias still matches a live cwd.
        let canon = path.canonicalize().unwrap_or_else(|_| path.clone());
        let override_reason = if state.reclaimable() {
            reclaim_safety_override(
                path_is_live(&canon, &live_cwds),
                is_primary_worktree(path),
                is_agent_suffix_worktree(path),
            )
        } else {
            None
        };
        match override_reason {
            None if state.reclaimable() => {
                n_reclaimable += 1;
                if verbose {
                    println!("  RECLAIMABLE  {:<42} {}", state.label(), path.display());
                }
            }
            Some(reason) => {
                // git-clean but UNSAFE to delete (live / primary / agent) — the task_774 footgun bucket.
                n_keep += 1;
                n_override += 1;
                println!("  KEEP         {reason:<42} {}", path.display());
            }
            None => {
                n_keep += 1;
                println!("  KEEP         {:<42} {}", state.label(), path.display());
            }
        }
    }

    println!(
        "\nsummary: {} checkout(s) — {n_reclaimable} reclaimable, {n_keep} keep (unsafe/active), of which {n_override} are git-clean but KEPT by the liveness/primary/agent safety overlay (task_774)",
        rows.len()
    );
    if rows.is_empty() {
        println!("no git worktrees found under the given root(s).");
    } else {
        println!(
            "reclaimable = clean+pushed or already-on-mainline AND not live/primary/agent; a no-upstream unmerged checkout is KEEP (unprovable), never reclaimable."
        );
    }
}

/// The ordered cargo steps `fleet gate` runs — the EXACT commands from the CI `clippy + test (workspace)` merge
/// job (`.github/workflows/checks.yml`): the whole-workspace, all-targets, deny-warnings clippy, then the
/// workspace test run. `clippy_only` drops the slower test step. Kept pure + unit-tested so the local gate can
/// never silently drift from what CI actually runs — the whole point of task_720. Each tuple is
/// `(human label, cargo argv)`.
fn gate_steps(clippy_only: bool) -> Vec<(&'static str, Vec<&'static str>)> {
    let mut steps: Vec<(&'static str, Vec<&'static str>)> = vec![(
        "clippy (workspace, all targets, deny warnings)",
        vec![
            "clippy",
            "--workspace",
            "--all-targets",
            "--",
            "-D",
            "warnings",
        ],
    )];
    if !clippy_only {
        steps.push(("test (workspace)", vec!["test", "--workspace"]));
    }
    steps
}

/// `fleet gate` (task_720): run the committed local gate — the SAME commands CI's `clippy + test (workspace)`
/// merge job runs — from the fleet checkout root, so a pre-merge check matches CI rather than an ad-hoc
/// bin-only clippy that misses test-target lints. Streams each step's output (inherited stdio) and exits
/// non-zero on the first failure, so it is usable both by hand and as a pre-push check. Read-only w.r.t. the
/// repo (clippy + test only).
fn gate(clippy_only: bool, isolated: bool) {
    let Some(root) = checkout_root() else {
        eprintln!("fleet gate: no source checkout found (run it from inside the fleet repo)");
        std::process::exit(1);
    };
    for (label, args) in gate_steps(clippy_only) {
        println!("fleet gate: cargo {}", args.join(" "));
        match std::process::Command::new("cargo")
            .current_dir(&root)
            .args(&args)
            .status()
        {
            Ok(s) if s.success() => println!("  ok: {label}"),
            Ok(s) => {
                eprintln!(
                    "fleet gate: FAILED at `{label}` (cargo exit {:?}) — fix before requesting a merge",
                    s.code()
                );
                std::process::exit(1);
            }
            Err(e) => {
                eprintln!("fleet gate: could not run cargo ({e})");
                std::process::exit(1);
            }
        }
    }
    if isolated {
        // The isolation dimension CI's per-crate build adds (task_729): build each workspace member ALONE so
        // its features resolve WITHOUT the sibling-crate unification `--workspace` applies — a crate that uses a
        // dependency feature it never declared compiles under `--workspace` (a sibling enables it) but fails
        // alone. `cargo metadata --no-deps` lists exactly the workspace members.
        let members = std::process::Command::new("cargo")
            .current_dir(&root)
            .args(["metadata", "--no-deps", "--format-version", "1"])
            .output()
            .ok()
            .filter(|o| o.status.success())
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .map(|s| workspace_members(&s))
            .unwrap_or_default();
        if members.is_empty() {
            eprintln!(
                "fleet gate --isolated: could not enumerate workspace members (cargo metadata failed)"
            );
            std::process::exit(1);
        }
        println!(
            "fleet gate --isolated: checking {} crate(s) in isolation",
            members.len()
        );
        for m in &members {
            println!("fleet gate: cargo check -p {m}");
            match std::process::Command::new("cargo")
                .current_dir(&root)
                .args(["check", "-p", m])
                .status()
            {
                Ok(s) if s.success() => println!("  ok: {m} (isolated)"),
                Ok(s) => {
                    eprintln!(
                        "fleet gate --isolated: FAILED checking `{m}` alone (cargo exit {:?}) — likely a feature it uses but does not declare (workspace unification hid it); add the `<dep>/<feature>` to {m}'s Cargo.toml",
                        s.code()
                    );
                    std::process::exit(1);
                }
                Err(e) => {
                    eprintln!("fleet gate --isolated: could not run cargo ({e})");
                    std::process::exit(1);
                }
            }
        }
    }
    println!(
        "fleet gate: PASS — matches CI's `clippy + test (workspace)` merge gate{}",
        if isolated {
            " + per-crate isolation"
        } else {
            ""
        }
    );
}

/// The workspace member crate names from `cargo metadata --no-deps --format-version 1` output (its `packages`
/// array is exactly the workspace members when `--no-deps` is passed). Sorted + deduped for a stable per-crate
/// gate order. Empty when the JSON is unparseable. Pure — unit-tested.
fn workspace_members(metadata_json: &str) -> Vec<String> {
    let mut names: Vec<String> = serde_json::from_str::<serde_json::Value>(metadata_json)
        .ok()
        .and_then(|v| {
            v.get("packages").and_then(|p| p.as_array()).map(|arr| {
                arr.iter()
                    .filter_map(|p| {
                        p.get("name")
                            .and_then(serde_json::Value::as_str)
                            .map(str::to_string)
                    })
                    .collect::<Vec<_>>()
            })
        })
        .unwrap_or_default();
    names.sort();
    names.dedup();
    names
}

/// The health of an agent's registered `metadata.worktree` (task_730). `MissingPath` is the migration/retire
/// footgun: the board charter points at a path that no longer exists, so the agent runs against a dead tree.
/// `UnlandedCommits` is the data-loss risk surface: the worktree holds commits ahead of its mainline that a
/// disk-reclaim sweep could delete before they land.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum WorktreeHealth {
    /// No `metadata.worktree` set — nothing to check (a non-worktree agent).
    Unset,
    /// `metadata.worktree` is a RELATIVE path and there is no hub root to resolve it against — cannot verify,
    /// so NOT reported as stale (never a false "missing"). A relative path is resolved against the configured
    /// hub when one exists; this state is only reached when it does not.
    Indeterminate,
    /// `metadata.worktree` (resolved to an absolute path) names a path that does not exist — STALE (dangling
    /// charter pointer).
    MissingPath,
    /// The worktree exists and has commits ahead of its mainline not yet landed — at risk if it is reclaimed.
    UnlandedCommits,
    /// The worktree exists and is level with / behind its mainline — healthy.
    Clean,
}

/// Classify a registered worktree's health from its facts. `resolvable` is false ONLY for a relative path with
/// no hub to resolve it against — then the verdict is `Indeterminate`, never a false `MissingPath`, so a
/// relative pointer is never wrongly failed. Otherwise a missing resolved path takes precedence; else commits
/// ahead of the mainline flag unlanded, at-risk work. Pure — unit-tested.
fn classify_worktree_health(
    worktree_set: bool,
    resolvable: bool,
    path_exists: bool,
    commits_ahead: usize,
) -> WorktreeHealth {
    if !worktree_set {
        return WorktreeHealth::Unset;
    }
    if !resolvable {
        return WorktreeHealth::Indeterminate;
    }
    if !path_exists {
        return WorktreeHealth::MissingPath;
    }
    if commits_ahead > 0 {
        return WorktreeHealth::UnlandedCommits;
    }
    WorktreeHealth::Clean
}

/// `fleet worktree-check` (task_730): audit each board-native agent's `metadata.worktree`. A STALE pointer (the
/// path no longer exists — a migration/retire rewrote the charter but left the board pointing at a dead tree) is
/// a hard FAIL (exit 1); a worktree holding commits ahead of its mainline is surfaced as an at-risk notice
/// (lose-able to a disk-reclaim sweep before it lands). Honest limit: this inspects the REGISTERED path, so it
/// catches a stale pointer and at-risk unlanded work, but not commits stranded in a DIFFERENT (old-clone) tree a
/// migration abandoned — that is prevented by a safe migrate step (follow-up), and reclaim-survey already
/// refuses to delete any orphaned worktree with unpushed commits. Read-only.
fn worktree_check(agent: Option<String>, verbose: bool) {
    let board = board::Board::connect().unwrap_or_else(|e| {
        eprintln!("fleet worktree-check: board unavailable ({e}); cannot audit worktrees");
        std::process::exit(1);
    });
    let roster = board.list_agents().unwrap_or_else(|e| {
        eprintln!("fleet worktree-check: board roster query failed ({e})");
        std::process::exit(1);
    });
    let mut ids: Vec<String> = roster
        .iter()
        .filter_map(|a| {
            a.get("id")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string)
        })
        .collect();
    ids.sort();
    let mainline_cands = mainline_candidates("origin/main");

    let mut stale: Vec<String> = Vec::new();
    let (mut n_clean, mut n_unlanded, mut n_unset, mut n_skipped) =
        (0usize, 0usize, 0usize, 0usize);
    for id in &ids {
        if let Some(f) = &agent
            && f != id
        {
            continue;
        }
        // The DETAIL record reliably carries metadata (the list projection has dropped it before — task_418).
        let detail = board.get_agent(id).ok();
        let md = detail.as_ref().and_then(|r| r.get("metadata"));
        if matches!(read_native(md), NativeVerdict::NotNative) {
            n_skipped += 1;
            if verbose {
                println!("  - {id}: native:false (skipped)");
            }
            continue;
        }
        let wt = md
            .and_then(|m| m.get("worktree"))
            .and_then(serde_json::Value::as_str)
            .map(str::to_string);
        let worktree_set = wt.as_deref().map(|s| !s.trim().is_empty()).unwrap_or(false);
        // Resolve to an absolute path: an absolute metadata.worktree is used as-is; a RELATIVE one is resolved
        // against the configured hub root (how the fleet stores agent worktrees, e.g. `.claude/worktrees/<a>`).
        // A relative path with no hub configured is `resolvable=false` → Indeterminate, never a false stale.
        let hub = config::get().hub.as_deref();
        let (resolvable, resolved) = match wt.as_deref() {
            Some(p) if !p.trim().is_empty() => {
                let path = std::path::Path::new(p);
                if path.is_absolute() {
                    (true, Some(path.to_path_buf()))
                } else if let Some(h) = hub {
                    (true, Some(std::path::Path::new(h).join(path)))
                } else {
                    (false, None)
                }
            }
            _ => (false, None),
        };
        let path_exists = resolved.as_deref().map(|p| p.is_dir()).unwrap_or(false);
        let ahead = if path_exists {
            let p = resolved
                .as_deref()
                .unwrap_or_else(|| std::path::Path::new("."));
            resolve_mainline(p, &mainline_cands)
                .and_then(|base| git_capture(p, &["rev-list", "--count", &format!("{base}..HEAD")]))
                .and_then(|s| s.parse::<usize>().ok())
                .unwrap_or(0)
        } else {
            0
        };
        match classify_worktree_health(worktree_set, resolvable, path_exists, ahead) {
            WorktreeHealth::Unset => {
                n_unset += 1;
                if verbose {
                    println!("  - {id}: no metadata.worktree (skipped)");
                }
            }
            WorktreeHealth::Indeterminate => {
                n_unset += 1;
                if verbose {
                    println!(
                        "  - {id}: metadata.worktree is relative ({}) and no hub is configured to resolve it — not verified",
                        wt.as_deref().unwrap_or("")
                    );
                }
            }
            WorktreeHealth::MissingPath => {
                stale.push(id.clone());
                println!(
                    "  - {id}: STALE metadata.worktree -> {} (path does not exist; the charter points at a dead tree)",
                    wt.as_deref().unwrap_or("")
                );
            }
            WorktreeHealth::UnlandedCommits => {
                n_unlanded += 1;
                println!(
                    "  - {id}: {ahead} unlanded commit(s) at {} (ahead of mainline; land/push before this tree is reclaimed)",
                    wt.as_deref().unwrap_or("")
                );
            }
            WorktreeHealth::Clean => {
                n_clean += 1;
                if verbose {
                    println!("  - {id}: clean ({})", wt.as_deref().unwrap_or(""));
                }
            }
        }
    }

    println!(
        "\nsummary: {n_clean} clean, {n_unlanded} with-unlanded-commits, {} stale-metadata, {n_unset} no-worktree, {n_skipped} non-native",
        stale.len()
    );
    if stale.is_empty() {
        println!("PASS: no agent's metadata.worktree points at a missing path.");
    } else {
        eprintln!(
            "FAIL: {} agent(s) with a STALE metadata.worktree (a migration/retire left the charter pointing at a dead tree) — rewrite the board metadata.worktree: {}",
            stale.len(),
            stale.join(", ")
        );
        std::process::exit(1);
    }
}

/// The outcome of a worktree-sync evaluation for one agent. `Fixable` carries the real per-agent fleet worktree
/// the stale `metadata.worktree` should be rewritten to.
#[derive(Debug, PartialEq, Eq, Clone)]
enum WorktreeSyncVerdict {
    /// No `metadata.worktree` set — sync only CORRECTS an existing pointer, it never adds one.
    NoWorktree,
    /// Not exactly one declared repo — the agent's primary worktree is ambiguous, so it is reported + skipped
    /// (a multi-repo agent needs an explicit primary-tree designation, not a guess).
    AmbiguousRepos,
    /// The agent's real per-agent fleet worktree does not exist on disk — leave the pointer untouched rather
    /// than rewrite it to point at a missing tree.
    NoRealTree,
    /// `metadata.worktree` already points at the real fleet worktree — nothing to do.
    AlreadyCorrect,
    /// `metadata.worktree` is stale (differs from the existing real fleet worktree) — rewrite it to this path.
    Fixable(String),
}

/// Decide the worktree-sync action for one agent from its facts. `repo_count` is the number of declared repos;
/// `expected` is the agent's real per-agent fleet worktree for its SOLE repo (the caller passes it only when
/// `repo_count == 1`); `expected_exists` is whether that tree is present on disk; `current` is the registered
/// `metadata.worktree`. A multi-repo (or repo-less) agent is `AmbiguousRepos` — its primary tree cannot be
/// inferred safely. Returns `Fixable` ONLY for a single-repo agent whose real tree exists and differs from
/// `current`, so the sweep never points a charter at a missing tree and never guesses a multi-repo primary.
/// Pure — unit-tested.
fn classify_worktree_sync(
    repo_count: usize,
    expected: Option<&str>,
    expected_exists: bool,
    current: Option<&str>,
) -> WorktreeSyncVerdict {
    if current.map(str::trim).filter(|s| !s.is_empty()).is_none() {
        return WorktreeSyncVerdict::NoWorktree;
    }
    if repo_count != 1 {
        return WorktreeSyncVerdict::AmbiguousRepos;
    }
    let Some(expected) = expected.map(str::trim).filter(|s| !s.is_empty()) else {
        return WorktreeSyncVerdict::AmbiguousRepos;
    };
    if !expected_exists {
        return WorktreeSyncVerdict::NoRealTree;
    }
    if current.map(str::trim) == Some(expected) {
        return WorktreeSyncVerdict::AlreadyCorrect;
    }
    WorktreeSyncVerdict::Fixable(expected.to_string())
}

/// Whether an OLD worktree a `worktree-sync` fix is about to stop pointing `metadata.worktree` at carries
/// at-risk work that would otherwise be silently stranded (task_735): local commits never pushed to its
/// own upstream, an unclean working tree, or both. Pure — unit-tested.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum CommitsCarryVerdict {
    /// No commits ahead of upstream and a clean working tree — nothing at risk at the old path.
    Clean,
    /// Commits ahead of upstream, an unclean working tree, or both.
    AtRisk { ahead: usize, dirty: bool },
}

/// Classify commits-carry risk from already-gathered facts: `ahead` is the old worktree's commit count
/// ahead of its own upstream (0 when there is none, or when there genuinely are no unpushed commits —
/// both are correctly `Clean`: an old path with no upstream and no new commits carries nothing to strand);
/// `dirty` is whether its working tree has uncommitted changes. Pure.
fn classify_commits_carry(ahead: usize, dirty: bool) -> CommitsCarryVerdict {
    if ahead == 0 && !dirty {
        CommitsCarryVerdict::Clean
    } else {
        CommitsCarryVerdict::AtRisk { ahead, dirty }
    }
}

/// `(ahead, dirty)` for the git worktree at `path`, or `None` if `path` is not (or no longer) a real git
/// worktree — a worktree-sync fix only fires when `current` differs from the real tree, so a stale
/// `metadata.worktree` may already point at nothing (previously cleaned up, or never real). Guards the
/// `@{u}` footgun (doc_25 / task_514): on some git versions `rev-parse '@{u}'` prints the LITERAL text
/// `@{u}` to stdout and exits non-zero when the upstream does not resolve, so a naive stdout-capture would
/// misread that as a real ahead-count — `--verify` first and only trust the count when it succeeds.
fn old_worktree_risk(path: &str) -> Option<(usize, bool)> {
    if !std::path::Path::new(path).join(".git").exists() {
        return None;
    }
    let has_upstream = std::process::Command::new("git")
        .args(["-C", path, "rev-parse", "--verify", "--quiet", "@{u}"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    let ahead = if has_upstream {
        std::process::Command::new("git")
            .args(["-C", path, "rev-list", "--count", "@{u}..HEAD"])
            .output()
            .ok()
            .filter(|o| o.status.success())
            .and_then(|o| String::from_utf8_lossy(&o.stdout).trim().parse().ok())
            .unwrap_or(0)
    } else {
        0
    };
    let dirty = std::process::Command::new("git")
        .args(["-C", path, "status", "--porcelain"])
        .output()
        .map(|o| o.status.success() && !o.stdout.is_empty())
        .unwrap_or(false);
    Some((ahead, dirty))
}

/// The durable board notice for an agent whose pre-migration worktree at `old_path` carries at-risk work a
/// `worktree-sync` fix is about to stop pointing `metadata.worktree` at (task_735: a cross-clone migration
/// must never silently strand in-flight work). Automatic transplant is NOT attempted here — `old_path` may
/// be a wholly separate clone with its own object database (e.g. an abandoned personal checkout), so a safe
/// cherry-pick/`git am` across that boundary is a harder follow-up; this notice is the zero-risk half of the
/// task's gate (detect-and-notify, not detect-and-silently-drop). Pure — unit-tested.
fn commits_carry_notice(old_path: &str, ahead: usize, dirty: bool) -> String {
    let mut risk = Vec::new();
    if ahead > 0 {
        risk.push(format!(
            "{ahead} commit{} ahead of its upstream",
            if ahead == 1 { "" } else { "s" }
        ));
    }
    if dirty {
        risk.push("an unclean working tree".to_string());
    }
    format!(
        "fleet worktree-sync just stopped pointing your metadata.worktree at {old_path} (migrating you to \
         your real per-agent fleet worktree). That old path still has {} that would otherwise be stranded \
         with nobody looking at it again. Recover it yourself: cd {old_path} && git push (or git \
         format-patch / git am the commits onto your real worktree) before that path is cleaned up.",
        risk.join(" and "),
    )
}

/// `fleet worktree-sync [--apply]` (task_735 migration residue): correct a STALE board `metadata.worktree` that
/// still points at a pre-migration path to the agent's real per-agent fleet worktree
/// (`$FLEET_ROOT/agents/<id>/<repo>`). Report-only by default; `--apply` rewrites via `patch_metadata`. SAFE by
/// construction (see [`classify_worktree_sync`]): only a single-repo agent whose real fleet worktree EXISTS and
/// differs from the registered path is corrected; a multi-repo agent (ambiguous primary) and a missing real tree
/// are reported + left untouched. Exits non-zero only on a board write error.
///
/// Also checks the OLD path for at-risk work before stopping anyone from pointing at it (task_735's
/// commits-carry half, see [`old_worktree_risk`]): under `--apply`, a stranded old path files a durable board
/// task assigned to the agent naming the old path + the ahead/dirty facts; in dry-run it just prints a
/// WOULD-FLAG line. No git write ever touches the old path — detect-and-notify, not an automatic transplant
/// (which would need to cross a possibly-separate clone's object database safely; left as a harder follow-up).
fn worktree_sync(apply: bool) {
    let board = board::Board::connect().unwrap_or_else(|e| {
        eprintln!("fleet worktree-sync: board unavailable ({e}); cannot sync worktrees");
        std::process::exit(1);
    });
    let roster = board.list_agents().unwrap_or_else(|e| {
        eprintln!("fleet worktree-sync: board roster query failed ({e})");
        std::process::exit(1);
    });
    let fleet_root = config::get()
        .root
        .clone()
        .unwrap_or_else(|| format!("{}/.fleet", std::env::var("HOME").unwrap_or_default()));
    let mut ids: Vec<String> = roster
        .iter()
        .filter_map(|a| {
            a.get("id")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string)
        })
        .collect();
    ids.sort();

    let (
        mut n_fixed,
        mut n_already,
        mut n_ambiguous,
        mut n_notree,
        mut n_nowt,
        mut n_skipped,
        mut n_err,
    ) = (0usize, 0usize, 0usize, 0usize, 0usize, 0usize, 0usize);
    let mut n_stranded = 0usize;
    for id in &ids {
        // The DETAIL record reliably carries metadata (the list projection has dropped it before — task_418).
        let detail = board.get_agent(id).ok();
        let md = detail.as_ref().and_then(|r| r.get("metadata"));
        if matches!(read_native(md), NativeVerdict::NotNative) {
            n_skipped += 1;
            continue;
        }
        let current = md
            .and_then(|m| m.get("worktree"))
            .and_then(serde_json::Value::as_str)
            .map(str::to_string);
        let repos = normalize_repos(md.and_then(|m| m.get("repos")));
        // Only a single-repo agent has an unambiguous real tree: $FLEET_ROOT/agents/<id>/<repo>.
        let expected = if repos.len() == 1 {
            repos[0]
                .get("repo")
                .and_then(serde_json::Value::as_str)
                .map(|r| workspace::workspace_dir(&fleet_root, id, r))
        } else {
            None
        };
        // A worktree's `.git` is a gitdir-pointer FILE, so `.join(".git").exists()` is the presence check.
        let expected_exists = expected
            .as_deref()
            .map(|p| std::path::Path::new(p).join(".git").exists())
            .unwrap_or(false);
        // Compare + write in CANONICAL form: $HOME here is a symlink (`/home/<u>` -> `/local/home/<u>`), so a raw
        // string compare would see an already-correct `/local/home/...` pointer as "different" from the `/home/...`
        // path `workspace_dir` builds, and rewrite a correct entry to the symlink form. `canonicalize` resolves
        // both to the real path; it needs the path to exist, so fall back to the raw string when it does not (a
        // stale pointer at a vanished tree stays != the real tree, i.e. still Fixable, which is correct).
        let canon = |p: &str| {
            std::fs::canonicalize(p)
                .ok()
                .and_then(|c| c.to_str().map(str::to_string))
        };
        let expected_target = expected
            .as_deref()
            .map(|p| canon(p).unwrap_or_else(|| p.to_string()));
        let current_cmp = current
            .as_deref()
            .map(|c| canon(c).unwrap_or_else(|| c.to_string()));
        match classify_worktree_sync(
            repos.len(),
            expected_target.as_deref(),
            expected_exists,
            current_cmp.as_deref(),
        ) {
            WorktreeSyncVerdict::NoWorktree => n_nowt += 1,
            WorktreeSyncVerdict::AmbiguousRepos => {
                n_ambiguous += 1;
                println!(
                    "  - {id}: {} repo(s) — AMBIGUOUS primary tree, skipped (needs explicit designation); current {}",
                    repos.len(),
                    current.as_deref().unwrap_or("")
                );
            }
            WorktreeSyncVerdict::NoRealTree => {
                n_notree += 1;
                println!(
                    "  - {id}: real fleet worktree {} does not exist — left untouched (current {})",
                    expected.as_deref().unwrap_or(""),
                    current.as_deref().unwrap_or("")
                );
            }
            WorktreeSyncVerdict::AlreadyCorrect => n_already += 1,
            WorktreeSyncVerdict::Fixable(target) => {
                // task_735: before (apply) or alongside reporting (dry-run) the metadata rewrite, check
                // whether the OLD path carries at-risk work the rewrite is about to stop pointing anyone
                // at — the exact cross-clone-migration moment this task is about.
                let risk = current
                    .as_deref()
                    .and_then(old_worktree_risk)
                    .map(|(ahead, dirty)| classify_commits_carry(ahead, dirty));
                if let Some(CommitsCarryVerdict::AtRisk { ahead, dirty }) = risk {
                    n_stranded += 1;
                    let old_path = current.as_deref().unwrap_or("");
                    if apply {
                        let notice = commits_carry_notice(old_path, ahead, dirty);
                        match board.create_task(
                            21, // fleet-tooling project — the home of every worktree-sync/task_735 notice
                            &format!("stranded work at {old_path} (worktree-sync migration)"),
                            &notice,
                            "fleet-worktree-sync",
                            serde_json::json!({"source": "worktree-sync", "old_path": old_path}),
                            None,
                        ) {
                            Ok(task_id) => {
                                match board.reassign_task(task_id, id, "fleet-worktree-sync") {
                                    Ok(()) => println!(
                                        "  - {id}: STRANDED-WORK notice filed as task_{task_id} ({old_path}: {ahead} ahead, dirty={dirty})"
                                    ),
                                    Err(e) => eprintln!(
                                        "  - {id}: stranded-work task_{task_id} created but reassign FAILED ({e})"
                                    ),
                                }
                            }
                            Err(e) => {
                                eprintln!("  - {id}: stranded-work notice FAILED to file ({e})")
                            }
                        }
                    } else {
                        println!(
                            "  - {id}: WOULD FLAG stranded work at {old_path} ({ahead} ahead, dirty={dirty})"
                        );
                    }
                }
                if apply {
                    match board.patch_metadata(id, serde_json::json!({ "worktree": target })) {
                        Ok(()) => {
                            n_fixed += 1;
                            println!(
                                "  - {id}: FIXED metadata.worktree {} -> {target}",
                                current.as_deref().unwrap_or("")
                            );
                        }
                        Err(e) => {
                            n_err += 1;
                            eprintln!(
                                "  - {id}: patch_metadata FAILED ({e}); current {}",
                                current.as_deref().unwrap_or("")
                            );
                        }
                    }
                } else {
                    n_fixed += 1;
                    println!(
                        "  - {id}: WOULD FIX metadata.worktree {} -> {target}",
                        current.as_deref().unwrap_or("")
                    );
                }
            }
        }
    }

    let verb = if apply { "fixed" } else { "would-fix" };
    let stranded_verb = if apply { "flagged" } else { "would-flag" };
    println!(
        "\nsummary: {n_fixed} {verb}, {n_already} already-correct, {n_ambiguous} multi-repo-skipped, {n_notree} no-real-tree, {n_nowt} no-worktree, {n_skipped} non-native, {n_stranded} stranded-work-{stranded_verb}{}",
        if n_err > 0 {
            format!(", {n_err} ERRORED")
        } else {
            String::new()
        }
    );
    if !apply && n_fixed > 0 {
        println!("(dry-run — re-run with --apply to write these corrections)");
    }
    if n_err > 0 {
        std::process::exit(1);
    }
}

/// The `author` a nudge comment is posted as — also the marker `nudge_last_secs` searches a task's prior
/// comments for, to find this daemon's own last nudge (the cooldown clock; #478).
const NUDGE_AUTHOR: &str = "fleet-nudge-daemon";
/// #540 inc2: where an unassigned-or-gone-owner stale task is ROUTED — the router that owns assignment. It is
/// event-woken on the reassignment (task.assigned), so this needs no polling on board-pm's side.
const NUDGE_ROUTER: &str = "board-pm";
/// task_627 inc2b: the channel a sweep posts its accountability digest to — a once-per-sweep roll-up of the
/// tasks that escalated past their owner (round-2 PM-tags + round-3 reassigns). The router ([`NUDGE_ROUTER`])
/// subscribes, so a `channel.post` wake lands the escalations in its inbox without it polling. Resolved by name
/// (create-if-absent) each sweep, mirroring the `deploys` channel ([`post_deploy`]).
const ACCOUNTABILITY_CHANNEL: &str = "fleet-accountability";
/// task_540 follow-on (board-pm greenlit): the EXTENDED re-nudge cooldown applied when the assignee has
/// acknowledged a queued todo since the last nudge — a fresh ack/ETA buys this much quiet instead of the
/// normal ~1h, so an acknowledged-queued backlog is not re-nudged hourly. 4h to start (tune toward 4–6h);
/// the anti-parking revert to the normal cadence on a stale ack is in [`stale_task_should_nudge`].
const ACKED_NUDGE_COOLDOWN_HOURS: f64 = 4.0;

/// task_627 (task_499 responsive-owner backoff): the max doubling steps the acked re-nudge cooldown backs off
/// by as a responsive owner keeps acking — base, 2x, 4x, 8x (e.g. 4h, 8h, 16h, 32h). Capped so a consistently
/// deliberately-sequenced owner goes quiet (pinged ~daily, not hourly) without ever going fully silent.
const MAX_ACKED_BACKOFF_STEPS: u32 = 3;

/// task_1032: how long the fleet-quiesce sentinel suppresses the nudge-stale daemon before it SELF-EXPIRES.
/// A fleet-wide spin-down/cutover runs minutes-to-a-few-hours (the observed migration freeze was ~3h); this
/// backstop auto-clears a sentinel a crashed `spin-up-all` left behind, so a failed cutover can never wedge the
/// daemon muted forever — the daemon resumes chasing stale work once the sentinel ages past this.
const FLEET_QUIESCE_TTL_HOURS: f64 = 6.0;

/// task_859 facet 2: how much longer a LOW-priority task's nudge threshold is than the base. A priority
/// downgrade must LENGTHEN the cadence, not zero it (a low-pri task that is still live work must not vanish
/// from oversight when it goes silent — just surface on a longer interval). Default 3x.
const LOW_PRIORITY_NUDGE_SCALE: i64 = 3;

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
            let cooldown = if assignee_ack_fresher {
                acked_cooldown_secs
            } else {
                cooldown_secs
            };
            since_last_nudge >= cooldown
        }
    }
}

/// task_627 (task_499): the number of this daemon's own nudge comments on the task — the backoff step for the
/// acked re-nudge cooldown. The more times a responsive owner has already acked a nudge on a still-sequenced
/// task, the longer the next quiet window. Pure — unit-tested.
fn nudge_comment_count(task: &serde_json::Value) -> usize {
    task.get("comments")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter(|c| c.get("author").and_then(serde_json::Value::as_str) == Some(NUDGE_AUTHOR))
        .count()
}

/// task_627 (task_499 responsive-owner backoff): widen the acked re-nudge cooldown progressively — each prior
/// nudge doubles the quiet window (base, 2x, 4x, 8x), capped at [`MAX_ACKED_BACKOFF_STEPS`]. So a responsive,
/// deliberately-sequenced owner that keeps acking is re-pinged ever less often (e.g. 4h -> 8h -> 16h -> 32h)
/// rather than hourly, without going fully silent. `prior_nudges` 0 or 1 -> the base cooldown (no backoff yet).
/// Only consulted on the acked-cooldown branch of [`stale_task_should_nudge`] (a responsive owner); a silent
/// owner reverts to the normal cadence and the escalation ladder proceeds. Pure — unit-tested.
fn acked_cooldown_with_backoff(base_acked_cooldown_secs: i64, prior_nudges: usize) -> i64 {
    let steps = (prior_nudges.saturating_sub(1) as u32).min(MAX_ACKED_BACKOFF_STEPS);
    base_acked_cooldown_secs.saturating_mul(1i64 << steps)
}

/// task_627 (task_499): cap the ladder at the Owner tier for a RESPONSIVE owner — one who has commented since
/// our last nudge (`ack_fresher`). A responsive owner is engaged, not neglectful, so the ladder never climbs
/// to a router PM-tag or reassign on them (board-pm's hard requirement). This is normally already true (a fresh
/// owner comment resets [`nudge_round_count`] to 0 -> round 1 -> Owner), but the explicit cap guarantees the
/// invariant regardless of how the round count is derived. Pure — unit-tested.
fn responsive_capped_tier(base: NudgeTier, ack_fresher: bool) -> NudgeTier {
    if ack_fresher { NudgeTier::Owner } else { base }
}

/// A task's latest activity age in seconds: the freshest of its `updated_at` and every comment's
/// `created_at` (a comment does NOT bump `updated_at` on this board, so both must be checked — #478).
/// `None` only if `updated_at` itself fails to parse (a malformed record); an unparseable comment
/// timestamp is skipped rather than failing the whole task.
fn task_latest_activity_age_secs(
    task: &serde_json::Value,
    now: time::OffsetDateTime,
) -> Option<i64> {
    let updated_at = task
        .get("updated_at")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
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

/// task_627: how many consecutive UNANSWERED nudge rounds this task has had — the count of this daemon's own
/// nudge comments that are NEWER than the newest comment from anyone else (the owner's last response/activity).
/// A fresh owner comment resets the count: the round was answered. Derived purely from the task's comment
/// history (ages via [`last_seen_age_secs`], mirroring [`task_last_nudge_age_secs`]), so there is no external
/// round-state to persist or corrupt. Pure — unit-tested.
fn nudge_round_count(task: &serde_json::Value, now: time::OffsetDateTime) -> usize {
    let comments = task
        .get("comments")
        .and_then(serde_json::Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    // The owner's last activity = the age of the NEWEST non-daemon comment (smallest age); None if the owner
    // has never commented. Nudge comments newer than that (smaller age) are the ones that went unanswered.
    let owner_age = comments
        .iter()
        .filter(|c| c.get("author").and_then(serde_json::Value::as_str) != Some(NUDGE_AUTHOR))
        .filter_map(|c| c.get("created_at").and_then(serde_json::Value::as_str))
        .filter_map(|ts| last_seen_age_secs(ts, now))
        .min();
    comments
        .iter()
        .filter(|c| c.get("author").and_then(serde_json::Value::as_str) == Some(NUDGE_AUTHOR))
        .filter_map(|c| c.get("created_at").and_then(serde_json::Value::as_str))
        .filter_map(|ts| last_seen_age_secs(ts, now))
        .filter(|&age| owner_age.is_none_or(|o| age < o))
        .count()
}

/// task_627: the three tiers a stale-task nudge can take, chosen by the 1-based nudge `round`. The ladder keeps
/// the operator out of the loop while chasing the owner, then pulls the router (board-pm) in as the owner stays
/// silent (board-pm seq-9931).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NudgeTier {
    /// Round 1: ping the owner only.
    Owner,
    /// The owner has gone silent: tag the router to make the call — chase an ETA, reassign, block, or close.
    PmTag,
    /// Escalate: tag the router to REASSIGN to a fresh agent / mint a helper.
    Reassign,
}

/// task_627: which [`NudgeTier`] a nudge at 1-based `round` falls into, given the config-tunable round
/// thresholds (defaults 2 and 3). `reassign_round` wins when both thresholds are met, so the ladder is
/// owner (round 1) → pm-tag (round `pm_tag_round`) → reassign (round `reassign_round`). Pure — unit-tested.
fn nudge_tier(round: usize, pm_tag_round: usize, reassign_round: usize) -> NudgeTier {
    if round >= reassign_round {
        NudgeTier::Reassign
    } else if round >= pm_tag_round {
        NudgeTier::PmTag
    } else {
        NudgeTier::Owner
    }
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

/// task_627 (PM-TAG tier, round 2+): the body when the owner has gone silent for a round and the daemon pulls
/// the `router` (the operator-accountable backstop) in to make the call — chase an ETA, reassign, mark it
/// blocked, or close it — while still giving the owner the chance to respond. Names the owner so the trail
/// shows who went silent. Pure — unit-tested.
fn pm_tag_body(
    threshold_hours: f64,
    assignee: &str,
    idle_secs: i64,
    unanswered_rounds: usize,
    router: &str,
) -> String {
    format!(
        "fleet escalation: this task is still idle (over {threshold_hours}h, idle {}) after {unanswered_rounds} \
         unanswered nudge(s) to {assignee}. {router}, please make the call: chase an ETA, reassign it to an \
         available agent, mark it explicitly blocked with a blocked_on note, or close it. The owner has not \
         responded across {unanswered_rounds} round(s), so this is now an accountability decision, not another \
         owner ping.",
        format_hm(idle_secs)
    )
}

/// task_627 (REASSIGN tier, round 3+ = N): the body when the owner has stayed silent long enough that the
/// daemon escalates to a REASSIGN — it tags the `router` to hand the task to a fresh agent (mint a helper if
/// needed) rather than keep waiting on an owner who is not picking it up. The router still makes the call (no
/// blind auto-reassign). Pure — unit-tested.
fn escalation_body(
    threshold_hours: f64,
    assignee: &str,
    idle_secs: i64,
    unanswered_rounds: usize,
    router: &str,
) -> String {
    format!(
        "fleet escalation (REASSIGN): this task is still idle (over {threshold_hours}h, idle {}) after \
         {unanswered_rounds} unanswered nudge(s) to {assignee}, who is not picking it up. {router}, please \
         REASSIGN it to an available agent (mint a helper if needed); if a reassign is wrong, mark it blocked \
         with a blocked_on note or close it. The owner has had {unanswered_rounds} round(s) and gone silent, so \
         it needs a fresh owner, not another ping.",
        format_hm(idle_secs)
    )
}

/// #540 inc2: is a task's assigned OWNER GONE — truly retired/removed, so its stale task is genuinely orphaned
/// and should be ROUTED to the router for re-placement? This is the FALSE-POSITIVE-FREE reroute signal: an
/// `offline`/`away` owner, or one with a stale heartbeat, is NOT gone — it is a DELIBERATELY spun-down
/// (operator-directed, RESUMABLE) or slow-cadence owner whose tasks are its own correct work parked until it
/// resumes, and rerouting those just bounces (board-pm #540 inc2 review: all 4 offline/stale-owner reroutes
/// were spun-down-resumable false positives).
///
/// task_1032 (KB 8f001c48 retire-marker follow-on): gone requires absence from BOTH the live board roster AND
/// the DECLARED fleet manifest. A mass spin-down can drop an agent's board record while its registry.json entry
/// survives (parked-resumable) — board-roster absence ALONE would then falsely orphan 60+ parked owners' tasks.
/// Declared-manifest membership is the explicit resume-vs-retire marker: a `fleet remove` (true retire) drops
/// the registry entry, a spin-down does not. `board_roster` is the set of currently-registered board agent ids;
/// `declared_fleet` is the set of agent names the fleet manifest still manages. Pure — unit-tested.
fn owner_is_gone(
    owner: &str,
    board_roster: &std::collections::BTreeSet<String>,
    declared_fleet: &std::collections::BTreeSet<String>,
) -> bool {
    !board_roster.contains(owner) && !declared_fleet.contains(owner)
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

/// task_627 inc2b: one router-escalation captured during a sweep for the end-of-sweep accountability digest.
/// `reassign` marks a round-3 REASSIGN escalation (vs a round-2 PM-tag); the rest name the task, the silent
/// owner, how long it has been idle, and the unanswered-round count so the router can act straight from the
/// digest.
struct Escalation {
    id: i64,
    title: String,
    owner: String,
    idle_secs: i64,
    unanswered_rounds: usize,
    reassign: bool,
}

/// task_627 inc2b: the once-per-sweep accountability digest posted to the [`ACCOUNTABILITY_CHANNEL`] when a
/// sweep escalated one or more tasks past their owner (round-2 PM-tags and/or round-3 reassigns). ONE post per
/// sweep — not one per task — so the router ([`NUDGE_ROUTER`], a channel member) is woken a single time with
/// every actionable item rather than N times. Reassigns are listed first (more urgent), then PM-tags; each line
/// names the task with a typed `task_<id>` ref (never a bare `#<id>`, which board content hard-rejects) and the
/// silence detail. Pure — unit-tested.
fn accountability_digest_body(escalations: &[Escalation]) -> String {
    let reassign = escalations.iter().filter(|e| e.reassign).count();
    let pm_tag = escalations.len() - reassign;
    let mut lines = String::new();
    // Reassigns (round 3, more urgent) before PM-tags (round 2); within each tier, in sweep order.
    for e in escalations
        .iter()
        .filter(|e| e.reassign)
        .chain(escalations.iter().filter(|e| !e.reassign))
    {
        let kind = if e.reassign { "REASSIGN" } else { "PM-TAG" };
        lines.push_str(&format!(
            "\n- {kind} task_{} \"{}\" - owner {} silent, {} unanswered round(s), idle {}",
            e.id,
            e.title,
            e.owner,
            e.unanswered_rounds,
            format_hm(e.idle_secs)
        ));
    }
    format!(
        "fleet accountability digest: {} task(s) escalated past their owner this sweep ({reassign} reassign, \
         {pm_tag} pm-tag). {NUDGE_ROUTER}, each needs a call - chase an ETA, reassign to an available agent, \
         mark it explicitly blocked with a blocked_on note, or close it:{lines}",
        escalations.len()
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
/// task_627 inc2b: after the per-task pass, a sweep that escalated any task past its owner (a round-2 PM-tag or
/// round-3 reassign) posts ONE accountability digest to the [`ACCOUNTABILITY_CHANNEL`] ([`NUDGE_ROUTER`]
/// subscribes), so the router is woken once with every actionable item; a channel resolve/post failure is
/// logged but never fails the sweep or trips the outage guard.
/// Report-only unless `apply` — a dry run prints exactly what it WOULD do without writing (the #478/#540 review
/// gate).
/// task_609: whether a nudge-stale run is a total OUTAGE — it attempted at least one post but landed ZERO.
/// Only meaningful in `apply` mode (a dry run posts nothing by design). `posted` = the nudges + routes that
/// succeeded; `failed` = posts that were attempted and errored. Since attempted == posted + failed, the
/// condition "attempted > 0 && posted == 0" reduces to `posted == 0 && failed > 0`. A sweep with nothing to do
/// (no stale tasks → posted == 0, failed == 0) is NOT an outage — it is a healthy quiet run that must exit 0.
/// Pure; unit-tested.
fn nudge_run_is_outage(apply: bool, posted: usize, failed: usize) -> bool {
    apply && posted == 0 && failed > 0
}

/// task_859 facet 2: the nudge threshold scaled by task PRIORITY. A LOW-priority task keeps nudging — on a
/// longer interval (`low_scale`x the base) — rather than dropping out of oversight entirely; normal / high /
/// absent priority keep the base threshold. Pure — unit-tested.
fn priority_scaled_threshold(
    base_threshold_secs: i64,
    priority: Option<&str>,
    low_scale: i64,
) -> i64 {
    match priority {
        Some("low") => base_threshold_secs.saturating_mul(low_scale.max(1)),
        _ => base_threshold_secs,
    }
}

/// task_859 facet 1: the OPEN (non-done, non-cancelled) children of a parent task, each rendered as a rollup
/// line `task_<id> [<status>[, blocked_on <kind> <ref>]] <title>`, from the `children` projection the board
/// returns on get_task (id / status / blocked_on_kind / blocked_on_ref / title). Empty when the task has no
/// open children (so a non-parent, or an all-children-done parent, falls through to the normal nudge). Pure —
/// unit-tested.
fn open_children_rollup(task: &serde_json::Value) -> Vec<String> {
    task.get("children")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|c| {
            let status = c
                .get("status")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            if status == "done" || status == "cancelled" {
                return None;
            }
            let id = c.get("id").and_then(serde_json::Value::as_i64)?;
            let title = c
                .get("title")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            let blocked = c
                .get("blocked_on_kind")
                .and_then(serde_json::Value::as_str)
                .filter(|s| !s.is_empty())
                .map(|k| {
                    match c
                        .get("blocked_on_ref")
                        .and_then(serde_json::Value::as_str)
                        .filter(|s| !s.is_empty())
                    {
                        Some(r) => format!(", blocked_on {k} {r}"),
                        None => format!(", blocked_on {k}"),
                    }
                })
                .unwrap_or_default();
            Some(format!("task_{id} [{status}{blocked}] {title}"))
        })
        .collect()
}

/// task_859 facet 1: the owner-nudge body for an idle PARENT whose progress lives in still-open children — a
/// child rollup so the owner posts a status rollup or escalates a blocked child, instead of the parent sitting
/// silently un-nudged (task_486: a parent read quiet ~16h with its one child blocked_on=operator and nobody
/// surfaced it). The idle is the parent's OWN activity, never rolled up from children (that would re-suppress
/// the nudge). Pure — unit-tested.
fn parent_rollup_body(
    threshold_hours: f64,
    assignee: &str,
    idle_secs: i64,
    open_children: &[String],
) -> String {
    let kids = open_children
        .iter()
        .map(|c| format!("  - {c}"))
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "fleet nudge: this parent task has had no activity of its OWN for over {threshold_hours}h (idle {}), \
         but it has open children whose progress it tracks. {assignee}, please post a rollup of where the \
         children stand, or escalate any that are blocked:\n{kids}",
        format_hm(idle_secs)
    )
}

/// task_1032: is the fleet currently QUIESCED — a mass spin-down/cutover in effect — so the nudge-stale daemon
/// must suppress every nudge and reroute this sweep? True only when the sentinel is PRESENT (`Some(age)`) AND
/// still within the TTL; an absent sentinel (`None`) is the normal running fleet, and a sentinel older than the
/// TTL has self-expired (a crashed cutover must not mute the daemon forever). Pure — unit-tested.
fn fleet_is_quiesced(sentinel_age_secs: Option<i64>, ttl_secs: i64) -> bool {
    matches!(sentinel_age_secs, Some(age) if age < ttl_secs)
}

/// task_1032: the age in seconds of the fleet-quiesce sentinel's recorded start instant, or `None` when the
/// sentinel is absent (the normal case). Reads the RFC3339 instant written on the first line (what
/// [`set_fleet_quiesce`] writes); falls back to the file mtime so a hand-touched sentinel still works.
/// Best-effort — any read error degrades to `None` (not quiesced), so the daemon never wedges on an unreadable
/// marker (it fails OPEN: it would rather nudge than go permanently silent on a corrupt sentinel).
fn quiesce_sentinel_age_secs(now: time::OffsetDateTime) -> Option<i64> {
    let path = quiesce_sentinel_path();
    let text = std::fs::read_to_string(&path).ok()?;
    if let Some(age) = text
        .lines()
        .next()
        .and_then(|l| last_seen_age_secs(l.trim(), now))
    {
        return Some(age);
    }
    // No parseable timestamp on line 1 (a hand-created sentinel) → use the file mtime as the quiesce start.
    let modified = std::fs::metadata(&path).ok()?.modified().ok()?;
    Some(modified.elapsed().ok()?.as_secs() as i64)
}

/// task_1032: WRITE the fleet-quiesce sentinel (maintenance mode ON) — called by `spin-down-all --apply` before
/// it parks any agent, so the nudge-stale daemon goes silent for the whole wind-down. Line 1 is the RFC3339
/// start instant (the TTL clock); the rest is a human note. Best-effort: a write failure is logged, never fatal
/// (a missed sentinel degrades to the pre-task_1032 behavior — the daemon still nudges — not a crash).
fn set_fleet_quiesce() {
    let path = quiesce_sentinel_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let stamp = time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default();
    let body = format!(
        "{stamp}\n# fleet-quiesce sentinel (task_1032): a fleet-wide spin-down is in effect; the nudge-stale \
         daemon is suppressed. `fleet spin-up-all --apply` (final wave) clears it; it self-expires after \
         {FLEET_QUIESCE_TTL_HOURS}h.\n"
    );
    match std::fs::write(&path, body) {
        Ok(()) => println!(
            "  fleet-quiesce ON — nudge-stale suppressed (sentinel {})",
            path.display()
        ),
        Err(e) => eprintln!(
            "  WARN: could not write quiesce sentinel {}: {e} (nudge daemon NOT suppressed this wind-down)",
            path.display()
        ),
    }
}

/// task_1032: CLEAR the fleet-quiesce sentinel (maintenance mode OFF) — called by `spin-up-all --apply` once a
/// reconstitution wave completes the fan-out, so the nudge-stale daemon resumes. An already-absent sentinel is
/// a no-op (idempotent); any other remove error is logged (the TTL backstop clears it regardless).
fn clear_fleet_quiesce() {
    let path = quiesce_sentinel_path();
    match std::fs::remove_file(&path) {
        Ok(()) => println!(
            "  fleet-quiesce OFF — nudge-stale resumes (removed {})",
            path.display()
        ),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => eprintln!(
            "  WARN: could not remove quiesce sentinel {}: {e}",
            path.display()
        ),
    }
}

/// task_1032: the one-line human status of the fleet-quiesce sentinel for `fleet quiesce` (no flag), given its
/// age (`None` = absent) and the TTL. OFF when absent, ON (with age + time-to-TTL-expiry) when present and
/// fresh, EXPIRED when present but past the TTL (treated as OFF by [`fleet_is_quiesced`], but still on disk so
/// it is worth flagging for cleanup). Pure — unit-tested.
fn quiesce_status_line(sentinel_age_secs: Option<i64>, ttl_secs: i64) -> String {
    match sentinel_age_secs {
        None => {
            "fleet-quiesce: OFF (no sentinel) — nudge-stale + watchdog auto-actions run normally"
                .to_string()
        }
        Some(age) if age < ttl_secs => format!(
            "fleet-quiesce: ON — set {} ago, {} until TTL self-expiry; nudge-stale + watchdog auto-actions (revive-stranded / hire-signal / recover-wedged) suppressed",
            format_hm(age),
            format_hm(ttl_secs - age)
        ),
        Some(age) => format!(
            "fleet-quiesce: EXPIRED — sentinel is {} old (past the {} TTL) so it is treated as OFF; clear the stale file with `fleet quiesce --off`",
            format_hm(age),
            format_hm(ttl_secs)
        ),
    }
}

/// `fleet quiesce [--on|--off]` (task_1032): inspect or set the fleet-quiesce maintenance-mode sentinel. No flag
/// → report status. `--on` raises it, `--off` clears it; after either, the resulting status is printed so the
/// mutation is confirmed. Reuses [`set_fleet_quiesce`] / [`clear_fleet_quiesce`] so the manual path writes the
/// exact same marker `spin-down-all` does and the daemons read.
fn quiesce_cmd(on: bool, off: bool) {
    let ttl_secs = (FLEET_QUIESCE_TTL_HOURS * 3600.0).round() as i64;
    if on && off {
        eprintln!("fleet quiesce: pass at most one of --on / --off (reporting status only)");
    } else if on {
        set_fleet_quiesce();
    } else if off {
        clear_fleet_quiesce();
    }
    let now = time::OffsetDateTime::now_utc();
    println!(
        "{}",
        quiesce_status_line(quiesce_sentinel_age_secs(now), ttl_secs)
    );
}

/// task_1032: does THIS `spin-up-all` wave COMPLETE the fan-out (no down candidates remain after it), so the
/// fleet-quiesce sentinel should be cleared and the nudge daemon resume? True when every down-non-excepted
/// agent is launched this pass — `limit` None (bring everyone up) or the remaining count fits the limit. A
/// partial wave leaves the sentinel set (the fleet is still mostly parked); the TTL backstop clears it if the
/// final wave never comes. Mirrors the inverse of the "more down than this wave's limit" message. Pure —
/// unit-tested.
fn spin_up_all_completes_fanout(down_non_except: usize, limit: Option<usize>) -> bool {
    match limit {
        None => true,
        Some(n) => down_non_except <= n,
    }
}

/// task_1032: should the watchdog RUN an opt-in auto-ACTION (revive-stranded / hire-signal) this sweep? The flag
/// must be set AND the fleet NOT quiesced. During a fleet-wide quiesce (mass spin-down/cutover) the watchdog's
/// auto-actions must stand down: reviving a "stranded" agent would fight the operator's spin-down (every parked
/// agent still holding work looks stranded), and a hire-signal post is a board write during the freeze. The
/// report WARNINGS still print — only the ACT is gated. Pure — unit-tested.
fn watchdog_action_enabled(flag: bool, fleet_quiesced: bool) -> bool {
    flag && !fleet_quiesced
}

fn nudge_stale(apply: bool, threshold_hours: f64, cooldown_hours: f64) {
    // task_1032: fleet-quiesce gate. During an operator-directed mass spin-down/cutover a sentinel is set
    // (`fleet spin-down-all --apply`) and the whole fleet is parked-resumable; nudging every parked owner or
    // routing their stale/unassigned tasks to the router then is pure coordinator noise at the most delicate
    // moment (and risks a board write during a snapshot freeze — observer task_1032). While the sentinel is
    // present and within its TTL, emit ONE quiesce-ack line and do nothing else. It self-expires (TTL) so a
    // crashed cutover cannot mute the daemon forever, and `spin-up-all` clears it when the fan-out completes.
    // The check is a local file read, so a quiesced daemon never even opens a board connection.
    let now = time::OffsetDateTime::now_utc();
    let quiesce_ttl_secs = (FLEET_QUIESCE_TTL_HOURS * 3600.0).round() as i64;
    if fleet_is_quiesced(quiesce_sentinel_age_secs(now), quiesce_ttl_secs) {
        println!(
            "fleet nudge-stale: fleet-quiesce in effect (mass spin-down) — suppressing all nudges + routes this sweep"
        );
        return;
    }
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
        .filter_map(|a| {
            a.get("id")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string)
        })
        .collect();
    // task_1032 (KB 8f001c48 retire-marker follow-on): the DECLARED fleet roster — the agents registry.json
    // still manages. A spun-down-RESUMABLE agent stays declared (its registry entry survives a park); only a
    // true retire (`fleet remove`) drops it. So an owner absent from the BOARD roster but STILL declared is
    // parked-resumable, not an orphan — its stale task stays with it, which is what keeps a mass spin-down from
    // rerouting 60+ parked owners' work the moment their board records go (the predicted migration wave). This
    // registry-membership check is the explicit resume-vs-retire marker KB 8f001c48 parked. Best-effort: a load
    // failure yields an empty set, degrading [`owner_is_gone`] to the board-roster-only signal (prior behavior).
    let declared_fleet: std::collections::BTreeSet<String> = Fleet::resolve()
        .load()
        .agents
        .into_iter()
        .map(|a| a.name)
        .collect();
    // #540: nudge stale ASSIGNED work in in_progress AND todo. A task where work started (a plan comment) but
    // was never flipped to in_progress still stalls, and the operator wants it caught (task_512). A bare
    // untouched todo is NOT nudged — the per-task check below requires worker activity for a todo — so widening
    // to todo stays high-signal (real stalls only, not unstarted backlog).
    let mut candidates = board
        .list_tasks_by_status("in_progress")
        .unwrap_or_else(|e| {
            eprintln!("fleet nudge-stale: {e}");
            std::process::exit(1);
        });
    match board.list_tasks_by_status("todo") {
        Ok(mut todo) => candidates.append(&mut todo),
        Err(e) => eprintln!(
            "fleet nudge-stale: listing todo tasks failed ({e}); nudging in_progress only"
        ),
    }

    println!(
        "fleet nudge-stale: {} assigned in_progress/todo task(s), threshold {threshold_hours}h, cooldown {cooldown_hours}h{}",
        candidates.len(),
        if apply {
            ""
        } else {
            " (DRY RUN — no comments will be posted)"
        }
    );

    // The operator's own tasks are their work queue, not a stall, so a task assigned to the operator is never
    // nudged/routed (#478 exclusion). The operator id is a deployment-specific value read from config; absent →
    // NO exemption (the generic case: a fleet with no designated operator). The fleet code holds no operator id.
    let operator_id = config::get().operator_id.as_deref();
    // task_627: the 1-based nudge rounds at which the ladder pulls in the router. Config-tunable
    // (generic-boundary); defaults keep round 1 the owner's alone, tag board-pm from round 2, and escalate to a
    // reassign at round 3 (= N).
    let pm_tag_round = config::get().nudge_pm_tag_round.unwrap_or(2);
    let reassign_round = config::get().nudge_reassign_round.unwrap_or(3);
    let mut nudged = 0usize;
    let mut routed = 0usize;
    // task_609: count posts that were ATTEMPTED but FAILED (apply mode only). A run that tried to post N
    // nudges/routes and landed zero of them is a total outage (e.g. the board is unreachable) — it must NOT
    // exit 0, or systemd/the watchdog sees a healthy sweep and nothing chases stale work (the operator then
    // fills the gap by hand). See the outage check after the loop.
    let mut failed = 0usize;
    // task_627 inc2b: router escalations (PM-tag / reassign tiers) captured this sweep for the end-of-sweep
    // accountability digest posted to the fleet-accountability channel. Collected only on a SUCCESSFUL apply
    // comment, so a total outage (zero landed posts) yields no digest; `would_escalate` is the dry-run count.
    let mut escalations: Vec<Escalation> = Vec::new();
    let mut would_escalate = 0usize;
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
        let status = full
            .get("status")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("");
        if status != "in_progress" && status != "todo" {
            continue;
        }
        // board-pm race-hardening (#540 inc2): re-read the AUTHORITATIVE assignee from the FRESH get_task
        // record, not the older list snapshot — a task reassigned or unassigned between the list query and now
        // must route/nudge on its LIVE owner, closing the read-vs-act race. Re-apply the operator exclusion on
        // the fresh value too (a task just handed to the operator must not be nudged).
        let assignee = full
            .get("assignee")
            .and_then(serde_json::Value::as_str)
            .filter(|s| !s.is_empty());
        if operator_id.is_some() && assignee == operator_id {
            continue;
        }
        // #540(b): a todo is only a stall once work actually STARTED on it (a real comment) — a bare untouched
        // todo is backlog waiting to be picked up, not a stalled deliverable, so it is never nudged. in_progress
        // needs no such guard (being in_progress IS the work-started signal).
        if status == "todo" && !task_has_worker_activity(&full) {
            continue;
        }
        // task_859 facet 1: a tracking/epic parent with still-open children is no longer SKIPPED (#294 used to
        // skip it to avoid false stalls, which hid an idle parent whose blocked child nobody surfaced —
        // task_486). Instead, if the parent is idle beyond threshold on its OWN activity (computed just below;
        // child activity is deliberately NOT rolled up, which would re-suppress the nudge), it is nudged with a
        // child rollup (open child + status + blocked_on) via `parent_rollup_body`. Empty => not a
        // parent-with-open-children => the normal owner nudge.
        let open_children = open_children_rollup(&full);
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
        // task_627 (task_499 responsive-owner backoff): when the owner keeps ACKing (ack_fresher) but the task
        // stays deliberately sequenced, each prior nudge widens the acked cooldown (4h -> 8h -> 16h -> 32h cap),
        // so a consistently-responsive owner is pinged progressively less often instead of hourly — without
        // going fully silent (the task stays visible, not monitor_exempt). A silent owner (ack not fresher) is
        // unaffected: stale_task_should_nudge then uses the normal cooldown and the escalation ladder proceeds.
        let effective_acked_cooldown =
            acked_cooldown_with_backoff(acked_cooldown_secs, nudge_comment_count(&full));
        // task_859 facet 2: scale the first-nudge threshold by priority so a LOW-priority task still surfaces
        // when it goes silent, just on a longer interval, instead of dropping out of oversight entirely.
        let priority = full.get("priority").and_then(serde_json::Value::as_str);
        let eff_threshold_secs =
            priority_scaled_threshold(threshold_secs, priority, LOW_PRIORITY_NUDGE_SCALE);
        if !stale_task_should_nudge(
            idle_secs,
            last_nudge_secs,
            ack_fresher,
            eff_threshold_secs,
            cooldown_secs,
            effective_acked_cooldown,
        ) {
            continue;
        }

        let title = t
            .get("title")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("");
        // #540 inc2: decide ROUTE (reassign to the router) vs NUDGE (ping a live owner). Route an UNASSIGNED
        // task, or one whose owner is GONE from the roster — retired/removed, not merely offline (an offline
        // owner is usually a DELIBERATELY spun-down, resumable agent whose task must stay with it, board-pm
        // seq-7848). Never re-route one already owned by the router itself (self-reassign loop). Otherwise nudge
        // the owner (inc1). Guard: an empty roster means the roster query failed — do NOT treat every owner as
        // gone and mass-reroute; skip the gone-owner route entirely in that case.
        let route_reason: Option<String> = match assignee {
            None => Some("unassigned".to_string()),
            Some(owner) if owner != NUDGE_ROUTER => (!roster_ids.is_empty()
                && owner_is_gone(owner, &roster_ids, &declared_fleet))
            .then(|| format!("owner {owner} is gone from the roster")),
            Some(_) => None, // already the router → nudge it, don't self-reassign
        };

        match route_reason {
            Some(reason) => {
                if apply {
                    let res = board
                        .reassign_task(id, NUDGE_ROUTER, NUDGE_AUTHOR)
                        .and_then(|()| {
                            board.comment_task(
                                id,
                                NUDGE_AUTHOR,
                                &route_body(&reason, threshold_hours, idle_secs),
                            )
                        });
                    match res {
                        Ok(()) => {
                            routed += 1;
                            println!(
                                "  routed #{id} \"{title}\" → {NUDGE_ROUTER} ({reason}, idle={})",
                                format_hm(idle_secs)
                            );
                        }
                        Err(e) => {
                            failed += 1;
                            eprintln!("  #{id} \"{title}\": route FAILED: {e}");
                        }
                    }
                } else {
                    routed += 1;
                    println!(
                        "  would route #{id} \"{title}\" → {NUDGE_ROUTER} ({reason}, idle={})",
                        format_hm(idle_secs)
                    );
                }
            }
            None => {
                let owner = assignee.expect("Some (a live non-router owner) when not routing");
                // task_627: the nudge ladder by round. Round 1 pings the owner; from pm_tag_round the nudge
                // also tags the router (board-pm) to make the call; from reassign_round it escalates to a
                // reassign. `round` is this nudge's 1-based round number (prior unanswered rounds + 1).
                let unanswered = nudge_round_count(&full, now);
                let round = unanswered + 1;
                // task_627 (task_499): never escalate a RESPONSIVE owner — one who has commented since our last
                // nudge (ack_fresher) is engaged, so the ladder is capped at the Owner tier (no router PM-tag or
                // reassign). board-pm's hard requirement.
                let tier = responsive_capped_tier(
                    nudge_tier(round, pm_tag_round, reassign_round),
                    ack_fresher,
                );
                // task_859 facet 1: at the Owner tier, a parent with open children gets a child-rollup body
                // instead of the generic nudge. Higher tiers (the owner has gone silent) keep the escalation
                // ladder bodies that pull in the router.
                let parent_rollup = matches!(tier, NudgeTier::Owner) && !open_children.is_empty();
                let kind = match tier {
                    NudgeTier::Owner if parent_rollup => "parent-rollup",
                    NudgeTier::Owner if last_nudge_secs.is_some() => "re-nudge",
                    NudgeTier::Owner => "first nudge",
                    NudgeTier::PmTag => "PM-TAG",
                    NudgeTier::Reassign => "ESCALATION/reassign",
                };
                if apply {
                    let body = match tier {
                        NudgeTier::Owner if parent_rollup => {
                            parent_rollup_body(threshold_hours, owner, idle_secs, &open_children)
                        }
                        NudgeTier::Owner => nudge_body(threshold_hours, owner, idle_secs),
                        NudgeTier::PmTag => {
                            pm_tag_body(threshold_hours, owner, idle_secs, unanswered, NUDGE_ROUTER)
                        }
                        NudgeTier::Reassign => escalation_body(
                            threshold_hours,
                            owner,
                            idle_secs,
                            unanswered,
                            NUDGE_ROUTER,
                        ),
                    };
                    match board.comment_task(id, NUDGE_AUTHOR, &body) {
                        Ok(()) => {
                            nudged += 1;
                            println!(
                                "  nudged #{id} \"{title}\" ({kind}, round {round}, assignee={owner}, idle={})",
                                format_hm(idle_secs)
                            );
                            // task_627 inc2b: a PM-tag or reassign is a ROUTER escalation — capture it for the
                            // once-per-sweep accountability digest so the router is woken with it. A plain
                            // owner nudge (round 1) is owner-facing only and does not go to the channel.
                            if matches!(tier, NudgeTier::PmTag | NudgeTier::Reassign) {
                                escalations.push(Escalation {
                                    id,
                                    title: title.to_string(),
                                    owner: owner.to_string(),
                                    idle_secs,
                                    unanswered_rounds: unanswered,
                                    reassign: matches!(tier, NudgeTier::Reassign),
                                });
                            }
                        }
                        Err(e) => {
                            failed += 1;
                            eprintln!("  #{id} \"{title}\": nudge FAILED: {e}");
                        }
                    }
                } else {
                    nudged += 1;
                    println!(
                        "  would nudge #{id} \"{title}\" ({kind}, round {round}, assignee={owner}, idle={})",
                        format_hm(idle_secs)
                    );
                    if matches!(tier, NudgeTier::PmTag | NudgeTier::Reassign) {
                        would_escalate += 1;
                    }
                }
            }
        }
    }

    println!(
        "\n{} {} nudge(s) + {} route(s) to {NUDGE_ROUTER}{}{}",
        if apply { "posted" } else { "would post" },
        nudged,
        routed,
        if failed > 0 {
            format!(" ({failed} FAILED)")
        } else {
            String::new()
        },
        if apply {
            ""
        } else {
            " — re-run with --apply to act"
        }
    );

    // task_627 inc2b: push this sweep's router escalations (PM-tags + reassigns) to the fleet-accountability
    // channel as ONE digest so the router ([`NUDGE_ROUTER`], a subscriber) is woken once with every actionable
    // item. The per-task comments above are the owner-facing trail; this is the router-facing push. Apply-only
    // (a dry run neither creates the channel nor posts). Best-effort: a resolve/post failure is logged but does
    // NOT fail the sweep or feed the outage guard — the per-task comments already landed, and the digest is an
    // additional wake path, not the authoritative record.
    if apply {
        if !escalations.is_empty() {
            match board.create_or_get_channel(ACCOUNTABILITY_CHANNEL, NUDGE_AUTHOR) {
                Ok(channel_id) => {
                    let body = accountability_digest_body(&escalations);
                    match board.post_to_channel(channel_id, NUDGE_AUTHOR, &body) {
                        Ok(()) => println!(
                            "  posted accountability digest ({} escalation(s)) to #{ACCOUNTABILITY_CHANNEL} (id {channel_id})",
                            escalations.len()
                        ),
                        Err(e) => eprintln!(
                            "  accountability digest post to #{ACCOUNTABILITY_CHANNEL} failed: {e} (per-task comments already landed)"
                        ),
                    }
                }
                Err(e) => eprintln!(
                    "  resolve '{ACCOUNTABILITY_CHANNEL}' channel failed: {e} (per-task comments already landed; skipping digest)"
                ),
            }
        }
    } else if would_escalate > 0 {
        println!(
            "  would post accountability digest ({would_escalate} escalation(s)) to #{ACCOUNTABILITY_CHANNEL}"
        );
    }

    // task_609: a total-outage guard. If this apply run ATTEMPTED posts but landed ZERO (every nudge/route
    // failed — the board unreachable, auth broken, etc.), exiting 0 would mask the outage: systemd marks the
    // oneshot succeeded and the push-based accountability loop silently stops chasing stale work (the operator
    // then fills the PM gap by hand). Exit non-zero so the FAILED unit is itself the signal — visible to
    // `systemctl --user` and the watchdog health sweep. Posting a board signal is pointless when the board is
    // the thing that is down, so the exit code is the signal, not a board write.
    if nudge_run_is_outage(apply, nudged + routed, failed) {
        eprintln!(
            "fleet nudge-stale: OUTAGE — attempted {failed} post(s) and landed 0 (every one failed); exiting non-zero so the stale-task accountability loop is not silently down"
        );
        std::process::exit(1);
    }
}

/// The watchdog invocation a cadence unit runs: the liveness sweep (`--rearm --stale-only`) when `rearm`, plus
/// the observer cadence (`--observe --spawn`) and/or the host filter (`--pinned-only`) when requested. An
/// OBSERVER-ONLY unit (`rearm=false, observe=true` → `watchdog --observe --spawn`) can run alongside an
/// existing rearm watchdog without double-rearming — the host-a coexistence case. Pure — unit-tested.
fn watchdog_exec_args(
    rearm: bool,
    observe: bool,
    pinned_only: bool,
    self_redeploy: bool,
    hire_signal: bool,
    reap_stale_observers: bool,
) -> String {
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
    if hire_signal {
        args.push_str(" --hire-signal");
    }
    if reap_stale_observers {
        args.push_str(" --reap-stale-observers");
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

/// Write pre-rendered systemd unit files `(filename, body)` into `dir` for a PURE emit -- `--out-dir` mode
/// (task_464 flake wiring). Unlike `--install` (which targets the fixed `~/.config/systemd/user`), this writes
/// correctly-named files to an ARBITRARY dir with NO HOME dependency and NO systemctl guidance, so a nix
/// derivation can point it at `$out`. Creates `dir`. Fatal on a write error (a derivation must fail loudly).
fn write_units_to_dir(dir: &str, units: &[(&str, &str)]) {
    let base = std::path::Path::new(dir);
    if let Err(e) = std::fs::create_dir_all(base) {
        eprintln!("fleet unit --out-dir: mkdir {dir}: {e}");
        std::process::exit(1);
    }
    for (name, body) in units {
        let path = base.join(name);
        if let Err(e) = std::fs::write(&path, body) {
            eprintln!("fleet unit --out-dir: write {}: {e}", path.display());
            std::process::exit(1);
        }
        println!("wrote {}", path.display());
    }
}

/// The observer runtime-config env block ([`OBSERVER_ENV_ALLOWLIST`]) read from THIS process's environment and
/// rendered as systemd `Environment=` lines. Run at install time from the working interactive session so the
/// captured values are the ones under which a Claude session actually launches. Reads the environment (not pure).
fn captured_observer_env() -> String {
    let vars: Vec<(&str, Option<String>)> = OBSERVER_ENV_ALLOWLIST
        .iter()
        .map(|n| (*n, std::env::var(n).ok()))
        .collect();
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
fn render_watchdog_units(
    fleet_bin: &str,
    exec_args: &str,
    interval_secs: u64,
    env_block: &str,
) -> String {
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
/// (the host-a go-live: the system rearm service is left untouched, no sudo needed). `bin` defaults to this
/// binary's absolute path. `host` (task_948) is emitted as `Environment="FLEET_HOST=<host>"` so the unit text
/// never bakes in a specific box — a flake-managed install (the task_495/912 lane) passes the target host as
/// a generator argument, making a later target-swap (task_937 Phase B) a one-argument change, not a re-landing.
/// Mutating flags (`--rearm`/`--observe`/`--spawn`/`--revive-stranded`) stay opt-in regardless of `host`, so
/// the unit is inert-but-present by default (green-machine-ops's task_711 posture) until a host's own
/// invocation opts into them explicitly.
#[allow(clippy::too_many_arguments)]
fn watchdog_unit(
    rearm: bool,
    observe: bool,
    pinned_only: bool,
    self_redeploy: bool,
    hire_signal: bool,
    reap_stale_observers: bool,
    interval_secs: u64,
    bin: Option<String>,
    install: bool,
    uninstall: bool,
    host: Option<String>,
    out_dir: Option<String>,
) {
    let fleet_bin = bin.unwrap_or_else(|| {
        std::env::current_exe()
            .ok()
            .and_then(|p| p.to_str().map(str::to_string))
            .unwrap_or_else(|| "fleet".to_string())
    });
    let exec_args = watchdog_exec_args(
        rearm,
        observe,
        pinned_only,
        self_redeploy,
        hire_signal,
        reap_stale_observers,
    );
    // The target-host line (task_948) comes first regardless of --observe, so a bare rearm-only unit still
    // names which host it manages. Only an observer-spawning watchdog ALSO needs a launch environment (a
    // rearm-only sweep just sends keys to an existing window) -- captured from this (working) session so the
    // installed service can launch Claude.
    let mut env_block = render_service_env_lines(&[("FLEET_HOST", host)]);
    if observe {
        env_block.push_str(&captured_observer_env());
    }
    // --out-dir (task_464 flake wiring): emit the correctly-named unit files into a GIVEN dir purely (no HOME,
    // no systemctl guidance) so a nix derivation can point it at $out. Takes precedence over install/print.
    if let Some(dir) = out_dir {
        let (service, timer) =
            watchdog_unit_files(&fleet_bin, &exec_args, interval_secs, &env_block);
        write_units_to_dir(
            &dir,
            &[
                ("fleet-watchdog.service", &service),
                ("fleet-watchdog.timer", &timer),
            ],
        );
        return;
    }
    if uninstall {
        watchdog_unit_uninstall();
        return;
    }
    if install {
        watchdog_unit_install(&fleet_bin, &exec_args, interval_secs, &env_block);
        return;
    }
    print!(
        "{}",
        render_watchdog_units(&fleet_bin, &exec_args, interval_secs, &env_block)
    );
}

/// Write the watchdog service + timer into `~/.config/systemd/user/` and print the enable command. User-level
/// (no sudo). Idempotent (overwrites). Non-fatal guidance to stop any ad-hoc watchdog loop and to reverse.
fn watchdog_unit_install(fleet_bin: &str, exec_args: &str, interval_secs: u64, env_block: &str) {
    let Some(dir) = user_unit_dir() else {
        eprintln!(
            "fleet watchdog-unit --install: cannot resolve ~/.config/systemd/user (no HOME/XDG_CONFIG_HOME)"
        );
        std::process::exit(1);
    };
    if let Err(e) = std::fs::create_dir_all(&dir) {
        eprintln!(
            "fleet watchdog-unit --install: mkdir {}: {e}",
            dir.display()
        );
        std::process::exit(1);
    }
    let (service, timer) = watchdog_unit_files(fleet_bin, exec_args, interval_secs, env_block);
    for (name, body) in [
        ("fleet-watchdog.service", &service),
        ("fleet-watchdog.timer", &timer),
    ] {
        let path = dir.join(name);
        if let Err(e) = std::fs::write(&path, body) {
            eprintln!(
                "fleet watchdog-unit --install: write {}: {e}",
                path.display()
            );
            std::process::exit(1);
        }
        println!("installed {}", path.display());
    }
    println!("  ExecStart: {fleet_bin} {exec_args}");
    println!(
        "  enable:  systemctl --user daemon-reload && systemctl --user enable --now fleet-watchdog.timer"
    );
    println!(
        "  reverse: fleet watchdog-unit --uninstall  (or: systemctl --user disable --now fleet-watchdog.timer)"
    );
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
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                println!("absent (ok): {}", path.display())
            }
            Err(e) => eprintln!("  WARN: remove {}: {e}", path.display()),
        }
    }
    println!("  then: systemctl --user daemon-reload");
}

/// The launcher's exec arguments: `up-board --launch` reconstitutes every board-native agent pinned to this host
/// from the board roster (launching a tmux window for any that is declared-but-missing); `--pinned-only` narrows
/// it to agents EXPLICITLY pinned here (a secondary box). `--launch` is always present -- a unit that only
/// reported would never bring the fleet up. Pure -- unit-tested.
fn up_exec_args(pinned_only: bool) -> String {
    let mut args = String::from("up-board --launch");
    if pinned_only {
        args.push_str(" --pinned-only");
    }
    args
}

/// The systemd USER service + timer for the LAUNCHER cadence as `(service_text, timer_text)` -- pure unit text.
/// The service is a `oneshot` running `fleet up-board --launch` (reconstitute the board-native fleet from the
/// board, no file-hub); it needs the network up (the board API) so it is ordered After `network-online.target`.
/// The timer fires it shortly after boot (`OnBootSec`) and periodically re-reconciles on `OnUnitActiveSec`, so a
/// rebooted host brings its fleet back up and a window that died between watchdog re-arms is relaunched.
/// `env_block` is the pre-rendered `Environment=` lines (the FLEET_HOST host-parameterization). Pure -- unit-tested.
fn up_unit_files(
    fleet_bin: &str,
    exec_args: &str,
    interval_secs: u64,
    env_block: &str,
) -> (String, String) {
    let service = format!(
        "[Unit]\n\
         Description=Fleet launcher — reconstitute board-native agents (up-board) from the board, no file-hub\n\
         After=network-online.target\n\
         Wants=network-online.target\n\n\
         [Service]\n\
         Type=oneshot\n\
         {env_block}\
         ExecStart={fleet_bin} {exec_args}\n"
    );
    let timer = format!(
        "[Unit]\n\
         Description=Fleet launcher cadence (boot + periodic reconcile)\n\n\
         [Timer]\n\
         OnBootSec=30\n\
         OnUnitActiveSec={interval_secs}\n\
         Persistent=true\n\n\
         [Install]\n\
         WantedBy=timers.target\n"
    );
    (service, timer)
}

/// The two launcher units concatenated with display headers, for `fleet up-unit` stdout -- a host installs these
/// DECLARATIVELY (home-manager); the emitted text is the canonical shape to translate. Pure -- unit-tested.
fn render_up_units(
    fleet_bin: &str,
    exec_args: &str,
    interval_secs: u64,
    env_block: &str,
) -> String {
    let (service, timer) = up_unit_files(fleet_bin, exec_args, interval_secs, env_block);
    format!(
        "# ---- fleet-up.service (systemd USER oneshot — fleet up-board --launch) ----\n{service}\n\
         # ---- fleet-up.timer (fires the launcher at boot + every {interval_secs}s) ----\n{timer}"
    )
}

/// `fleet up-unit` -- the LAUNCHER's systemd USER service + timer (task_464 launcher relocation off the file-hub,
/// the counterpart to `watchdog-unit`). It runs `fleet up-board --launch` so the fleet comes up from the BOARD
/// roster on boot and is periodically reconciled, replacing the hub's window.sh launch path. Default: PRINT the
/// units (a declarative/nix host translates the text); `--install` writes them to `~/.config/systemd/user/` (no
/// sudo); `--uninstall` removes them. `host` (task_937 Phase A) is emitted as `Environment="FLEET_HOST=<host>"`
/// so the unit text never bakes in a box -- the flake-managed install passes the target host as an argument,
/// making a later target-swap (Phase B) a one-argument change.
fn up_unit(
    pinned_only: bool,
    interval_secs: u64,
    bin: Option<String>,
    install: bool,
    uninstall: bool,
    host: Option<String>,
    out_dir: Option<String>,
) {
    let fleet_bin = bin.unwrap_or_else(|| {
        std::env::current_exe()
            .ok()
            .and_then(|p| p.to_str().map(str::to_string))
            .unwrap_or_else(|| "fleet".to_string())
    });
    let exec_args = up_exec_args(pinned_only);
    let env_block = render_service_env_lines(&[("FLEET_HOST", host)]);
    // --out-dir (task_464 flake wiring): emit the named unit files into a GIVEN dir purely so a nix derivation
    // can point it at $out. Takes precedence over install/print.
    if let Some(dir) = out_dir {
        let (service, timer) = up_unit_files(&fleet_bin, &exec_args, interval_secs, &env_block);
        write_units_to_dir(
            &dir,
            &[("fleet-up.service", &service), ("fleet-up.timer", &timer)],
        );
        return;
    }
    if uninstall {
        up_unit_uninstall();
        return;
    }
    if install {
        up_unit_install(&fleet_bin, &exec_args, interval_secs, &env_block);
        return;
    }
    print!(
        "{}",
        render_up_units(&fleet_bin, &exec_args, interval_secs, &env_block)
    );
}

/// Write the launcher service + timer into `~/.config/systemd/user/` and print the enable command. User-level
/// (no sudo). Idempotent (overwrites).
fn up_unit_install(fleet_bin: &str, exec_args: &str, interval_secs: u64, env_block: &str) {
    let Some(dir) = user_unit_dir() else {
        eprintln!(
            "fleet up-unit --install: cannot resolve ~/.config/systemd/user (no HOME/XDG_CONFIG_HOME)"
        );
        std::process::exit(1);
    };
    if let Err(e) = std::fs::create_dir_all(&dir) {
        eprintln!("fleet up-unit --install: mkdir {}: {e}", dir.display());
        std::process::exit(1);
    }
    let (service, timer) = up_unit_files(fleet_bin, exec_args, interval_secs, env_block);
    for (name, body) in [("fleet-up.service", &service), ("fleet-up.timer", &timer)] {
        let path = dir.join(name);
        if let Err(e) = std::fs::write(&path, body) {
            eprintln!("fleet up-unit --install: write {}: {e}", path.display());
            std::process::exit(1);
        }
        println!("installed {}", path.display());
    }
    println!("  ExecStart: {fleet_bin} {exec_args}");
    println!(
        "  enable:  systemctl --user daemon-reload && systemctl --user enable --now fleet-up.timer"
    );
    println!(
        "  reverse: fleet up-unit --uninstall  (or: systemctl --user disable --now fleet-up.timer)"
    );
}

/// Remove the user launcher units this installed and print the disable command. Best-effort (a missing file is
/// fine -- the inverse of an install that never happened).
fn up_unit_uninstall() {
    let Some(dir) = user_unit_dir() else {
        eprintln!("fleet up-unit --uninstall: cannot resolve ~/.config/systemd/user");
        std::process::exit(1);
    };
    println!("  disable FIRST: systemctl --user disable --now fleet-up.timer");
    for name in ["fleet-up.service", "fleet-up.timer"] {
        let path = dir.join(name);
        match std::fs::remove_file(&path) {
            Ok(()) => println!("removed {}", path.display()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                println!("absent (ok): {}", path.display())
            }
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
        vec![
            "--user".into(),
            "enable".into(),
            "--now".into(),
            unit.into(),
        ],
    ]
}

/// `fleet daemon-unit <name>` — supervise a fleet-host daemon under systemd (see [`daemon_unit_file`]). Default:
/// PRINT the unit (a declarative host translates it). `--install`: WRITE `fleet-<name>.service` into
/// `~/.config/systemd/user/` (no sudo). `--enable`: write it AND bring it up in one shot (`daemon-reload` +
/// `enable --now`) — the #359 launch default, so a host daemon comes up supervised rather than as a bare tmux
/// window. `--uninstall`: remove it. `exec` is the daemon command; the built-in `notifier` defaults to
/// `<bin> notify`, any other name requires `--exec`. `host` (task_937 Phase A) emits `FLEET_HOST`.
#[allow(clippy::too_many_arguments)]
fn daemon_unit(
    name: &str,
    exec: Option<String>,
    restart_sec: u64,
    bin: Option<String>,
    install: bool,
    enable: bool,
    uninstall: bool,
    host: Option<String>,
    out_dir: Option<String>,
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
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                println!("absent (ok): {}", path.display())
            }
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
            eprintln!(
                "fleet daemon-unit {name}: --exec is required (no built-in command for '{name}'; the notifier defaults to `<bin> notify`)"
            );
            std::process::exit(1);
        }
    };
    // Host-parameterize like watchdog-unit/up-unit (task_937 Phase A): FLEET_HOST first (so the unit names the
    // host it belongs to; the flake passes --host, making a Phase-B retarget a one-value change), then the
    // captured PATH so the daemon still resolves tmux/git/curl at runtime.
    let mut env_block = render_service_env_lines(&[("FLEET_HOST", host)]);
    env_block.push_str(&captured_daemon_env());
    let body = daemon_unit_file(name, &exec, restart_sec, &env_block);
    // --out-dir (task_464 flake wiring): emit the single .service file into a GIVEN dir purely so a nix
    // derivation can point it at $out. Takes precedence over install/enable/print.
    if let Some(dir) = out_dir {
        write_units_to_dir(&dir, &[(unit.as_str(), body.as_str())]);
        return;
    }
    // `--enable` implies the write (it is the one-shot bring-up), so either flag lands the unit file.
    if install || enable {
        let Some(dir) = user_unit_dir() else {
            eprintln!(
                "fleet daemon-unit: cannot resolve ~/.config/systemd/user (no HOME/XDG_CONFIG_HOME)"
            );
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
                    Ok(_) => eprintln!(
                        "  WARN: systemctl {} returned non-zero (enable it later once the user manager is up)",
                        args.join(" ")
                    ),
                    Err(e) => eprintln!(
                        "  WARN: systemctl {} failed: {e} (enable it later)",
                        args.join(" ")
                    ),
                }
            }
            println!(
                "  reverse: fleet daemon-unit {name} --uninstall  (or: systemctl --user disable --now {unit})"
            );
        } else {
            println!(
                "  enable:  systemctl --user daemon-reload && systemctl --user enable --now {unit}"
            );
            println!(
                "  reverse: fleet daemon-unit {name} --uninstall  (or: systemctl --user disable --now {unit})"
            );
        }
        return;
    }
    print!("# ---- {unit} (systemd USER daemon, Restart=on-failure) ----\n{body}");
}

/// The build-provenance line: package version + the revision the binary was built from (baked by `build.rs`
/// into `FLEET_BUILD_REV`). Lets an operator or agent tell whether a deployed binary is current by comparing
/// the rev to `origin/main` — the signal that was missing when a stale watchdog binary silently ran old logic.
fn version_line() -> String {
    format!(
        "fleet {} (rev {})",
        env!("CARGO_PKG_VERSION"),
        env!("FLEET_BUILD_REV")
    )
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
fn watchdog_stale_self_action(
    baked_rev: &str,
    checkout_head: Option<&str>,
    self_redeploy: bool,
) -> StaleSelfAction {
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
    exe.ancestors()
        .find(|p| p.join(".git").exists())
        .map(Path::to_path_buf)
}

/// Run `git -C <root> <args...>` and return trimmed stdout on success, else `None`. Best-effort git helper.
fn git_capture(root: &Path, args: &[&str]) -> Option<String> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8(out.stdout).ok()?.trim().to_string();
    (!s.is_empty()).then_some(s)
}

/// Run `git -C <root> <args...>` for its EXIT STATUS only — for commands like `merge-base --is-ancestor` that
/// signal via the exit code and print NOTHING, so [`git_capture`] (which treats empty stdout as `None`) cannot
/// tell success from failure. True iff git ran and exited 0; a non-zero exit, a missing ref, or a spawn error
/// is `false` (the conservative answer for a safety check). Best-effort.
fn git_ok(root: &Path, args: &[&str]) -> bool {
    std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// The short HEAD sha of the git checkout the running binary was built from. `None` when there is no source
/// tree (a deployed binary) or git is unavailable. Best-effort — only used to warn about a stale binary.
fn checkout_head_short() -> Option<String> {
    git_capture(&checkout_root()?, &["rev-parse", "--short", "HEAD"])
}

/// The default systemd USER units `fleet redeploy` restarts (the long-running fleet daemons) when the config
/// does not override `redeploy_services`. A unit not present on this host is skipped, not an error.
const DEFAULT_REDEPLOY_SERVICES: &[&str] = &[
    "fleet-watchdog.timer",
    "fleet-notify.service",
    "fleet-tunnel.service",
];

/// The release builds `fleet redeploy` runs before restarting the daemons — one per binary that BACKS a
/// service in [`DEFAULT_REDEPLOY_SERVICES`]. `fleet-watchdog`/`fleet-notify` are the `fleet` binary
/// (`fleet watchdog` / `fleet notify`); `fleet-tunnel` is its OWN crate + binary, built only with
/// `--features transport`. Each entry is the cargo args AFTER `build --release`. Keep in sync with the
/// restarted services: a daemon binary missing here would be restarted STALE — the #451 gap, where a
/// fleet-tunnel change did not go live via redeploy because only `--bin fleet` was rebuilt.
const REDEPLOY_BUILDS: &[&[&str]] = &[
    &["--bin", "fleet"],
    &["-p", "fleet-tunnel", "--features", "transport"],
];

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
fn redeploy_action(
    baked_rev: &str,
    remote_sha: &str,
    dirty: bool,
    on_main: bool,
) -> RedeployAction {
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
    // On a NIX-MANAGED host the daemon units' ExecStart points into /nix/store (a pinned nix generation), so a
    // `cargo build --release` + restart would restart them onto the SAME store binary and silently no-op — an
    // ineffective redeploy that still LOOKS successful (the silent-success footgun). Refuse up front and direct
    // to the canonical nix path, rather than fetch/rebuild and misreport a deploy that changed nothing.
    let planned_services: Vec<String> =
        config::get().redeploy_services.clone().unwrap_or_else(|| {
            DEFAULT_REDEPLOY_SERVICES
                .iter()
                .map(|s| s.to_string())
                .collect()
        });
    let nix_units: Vec<String> = planned_services
        .iter()
        .filter(|s| unit_is_nix_managed(s) == Some(true))
        .cloned()
        .collect();
    if !nix_units.is_empty() {
        return Err(format!(
            "nix-managed host — {} daemon unit(s) run a /nix/store binary ({}); `fleet redeploy`'s cargo rebuild would not take effect (the units keep their pinned nix generation). Use the canonical nix path: `nix run .#deploy-fleet-binary`.",
            nix_units.len(),
            nix_units.join(", ")
        ));
    }
    // Refresh the remote ref so the comparison is against the current origin/main.
    if git_capture(&root, &["fetch", "--quiet", "origin", "main"]).is_none() {
        // fetch prints nothing on success, so None here can be a clean fetch OR a failure; probe the ref next.
    }
    let Some(remote_sha) = git_capture(&root, &["rev-parse", "--short", "origin/main"]) else {
        return Err(format!(
            "cannot resolve origin/main (fetch failed or no such remote) in {}",
            root.display()
        ));
    };
    let dirty = git_capture(&root, &["status", "--porcelain"]).is_some();
    let on_main =
        git_capture(&root, &["symbolic-ref", "--short", "HEAD"]).as_deref() == Some("main");
    let baked = env!("FLEET_BUILD_REV");
    let action = redeploy_action(baked, &remote_sha, dirty, on_main);

    println!(
        "fleet redeploy ({}): built rev {baked}, origin/main {remote_sha} — {}",
        if apply { "APPLY" } else { "report" },
        match &action {
            RedeployAction::UpToDate => "UP TO DATE".to_string(),
            RedeployAction::Rebuild => "REBUILD NEEDED (clean, on main)".to_string(),
            RedeployAction::NeedsManual(why) =>
                format!("BEHIND but MANUAL redeploy needed ({why})"),
        }
    );
    match action {
        RedeployAction::UpToDate => {
            return Ok(format!("up to date at {remote_sha}; nothing to redeploy"));
        }
        RedeployAction::NeedsManual(why) => {
            return Err(format!(
                "not acting — {why}. Resolve it, then re-run (or rebuild by hand)."
            ));
        }
        RedeployAction::Rebuild => {}
    }
    if !apply {
        return Ok(
            "report only — re-run with --apply to fast-forward, rebuild, and restart the daemons"
                .to_string(),
        );
    }
    // Fast-forward to origin/main (guaranteed possible: clean + on main + behind).
    println!("  fast-forwarding to origin/main…");
    if git_capture(&root, &["merge", "--ff-only", "origin/main"]).is_none()
        && git_capture(&root, &["rev-parse", "--short", "HEAD"]).as_deref()
            != Some(remote_sha.as_str())
    {
        return Err("fast-forward to origin/main failed; aborting before rebuild".to_string());
    }
    // Rebuild EVERY daemon binary before restarting anything (#451): the fleet binary AND the separate
    // fleet-tunnel binary. If any build fails, abort before restarting so the daemons keep their old, working
    // binaries rather than being restarted onto a half-rebuilt tree.
    for &extra in REDEPLOY_BUILDS {
        println!(
            "  building release (cargo build --release {})…",
            extra.join(" ")
        );
        let mut args = vec!["build", "--release"];
        args.extend_from_slice(extra);
        let build = std::process::Command::new("cargo")
            .current_dir(&root)
            .args(&args)
            .status();
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
    // Reuse the set resolved up front for the nix-managed guard (same source of truth).
    let services = planned_services;
    println!("  restarting {} daemon service(s)…", services.len());
    for svc in &services {
        let st = std::process::Command::new("systemctl")
            .args(["--user", "restart", svc])
            .status();
        match st {
            Ok(s) if s.success() => println!("    restarted {svc}"),
            // A unit not installed on this host is not an error — the default set is a superset across hosts.
            Ok(_) => println!(
                "    skipped {svc} (not present on this host, or restart returned non-zero)"
            ),
            Err(e) => println!("    could not restart {svc} ({e})"),
        }
    }
    Ok(format!(
        "now at {remote_sha}, binary rebuilt, daemons restarted."
    ))
}

/// Whether a systemd unit's `ExecStart` runs a binary out of the nix store — the signal that the unit is
/// NIX-MANAGED (its binary path is pinned by a nix generation). `systemctl show -p ExecStart` renders the
/// resolved exec as `ExecStart={ path=/nix/store/…/bin/fleet ; argv[]=… }`, so a `/nix/store/` substring is the
/// tell. Pure over the `systemctl show` output — unit-tested.
fn execstart_is_nix_store(show_output: &str) -> bool {
    show_output.contains("/nix/store/")
}

/// Query `systemctl --user show <unit> -p ExecStart` and report whether its ExecStart binary lives in the nix
/// store (nix-managed). `None` when the unit is absent / empty ExecStart / systemctl can't be queried — that is
/// "unknown", NOT a nix signal, so the caller treats only an explicit `Some(true)` as nix-managed and never
/// refuses a redeploy on a host it simply couldn't probe. Best-effort.
fn unit_is_nix_managed(unit: &str) -> Option<bool> {
    let out = std::process::Command::new("systemctl")
        .args(["--user", "show", unit, "-p", "ExecStart"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8(out.stdout).ok()?;
    // An absent/empty ExecStart (`ExecStart=`) means the unit is not installed here — unknown, not nix.
    if s.trim().is_empty() || s.trim() == "ExecStart=" {
        return None;
    }
    Some(execstart_is_nix_store(&s))
}

/// The next step in the `confirm-kill` poll loop (task_866), as a pure decision over the loop's state so the
/// kill-confirm logic is unit-tested without real processes: whether the pid is still alive, whether the
/// current deadline has passed, whether `--sigkill` was requested, and whether SIGKILL was already sent.
#[derive(Debug, PartialEq, Eq)]
enum KillStep {
    /// The pid is gone — the kill is confirmed and the caller may start the replacement.
    ConfirmedGone,
    /// Still alive and within the deadline — keep polling.
    KeepPolling,
    /// Past the deadline, still alive, `--sigkill` requested and not yet sent — send SIGKILL now.
    EscalateSigkill,
    /// Past the deadline, still alive, with no (further) escalation available — give up (still alive).
    GaveUp,
}

fn kill_confirm_step(
    alive: bool,
    deadline_passed: bool,
    sigkill_enabled: bool,
    sigkill_sent: bool,
) -> KillStep {
    if !alive {
        return KillStep::ConfirmedGone;
    }
    if !deadline_passed {
        return KillStep::KeepPolling;
    }
    if sigkill_enabled && !sigkill_sent {
        return KillStep::EscalateSigkill;
    }
    KillStep::GaveUp
}

/// Is `pid` still alive? `kill -0 <pid>` sends no signal — it only checks that the pid exists and is
/// signallable (exit 0), returning non-zero (ESRCH) once the process is gone. Unlike an exe-basename /
/// `/proc/<pid>/exe` scan it is immune to the deleted-inode false negative a rebuilt binary creates, which is
/// the whole point (task_866). A not-yet-reaped zombie still reads alive — it holds its pid until reaped —
/// and for a deploy that is the correct answer: the old daemon has not fully gone.
fn process_is_alive(pid: i64) -> bool {
    std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn send_signal(sig: &str, pid: i64) {
    let _ = std::process::Command::new("kill")
        .args([sig, &pid.to_string()])
        .status();
}

/// Does a `/proc/<pid>/exe` readlink target name the daemon `daemon`? The shell-agnostic core of the
/// daemon-IDENTIFICATION scan (task_924). A rebuilt binary leaves the old process on a deleted inode, so the
/// readlink reads `<path> (deleted)`; strip that trailing suffix with a plain `strip_suffix` (NOT a shell
/// pattern) and match the file name. This replaces the `${exe% (deleted)}` strip an agent reaches for in its
/// zsh Bash-tool shell, where the parentheses in ` (deleted)` are a glob GROUP so the strip silently fails and
/// a live rebuilt daemon reads as absent — the false-negative that double-started the bridge (task_866). The
/// strip is suffix-only, so a mid-path `(deleted)` directory never corrupts the match. Pure — unit-tested.
fn exe_matches_daemon(exe_target: &str, daemon: &str) -> bool {
    let stripped = exe_target.strip_suffix(" (deleted)").unwrap_or(exe_target);
    std::path::Path::new(stripped)
        .file_name()
        .and_then(|n| n.to_str())
        == Some(daemon)
}

/// `fleet daemon-pids <daemon>` (task_924): print the pid of every LIVE process whose running binary is named
/// `<daemon>`, so an agent finds a daemon shell-agnostically instead of a zsh-fragile `${exe% (deleted)}` scan
/// (see [`exe_matches_daemon`]). Scans `/proc/<pid>/exe` with a plain readlink (preserving the deleted-inode
/// ` (deleted)` suffix the way `confirm-kill`'s rationale requires), matches the basename in Rust, and confirms
/// `kill -0` liveness so a half-exited pid is never reported. Prints one matching pid per line, ascending;
/// exit 0 whether or not any matched — absence is a valid answer the caller (a guard/redeploy) acts on.
/// Extract the agent name from a board-native agent's working directory: the path segment immediately after an
/// `agents/` component. The fleet stores each board-native agent's workspace at `<root>/.fleet/agents/<agent>[/…]`
/// (e.g. `/home/u/.fleet/agents/ticket-ingest/fleet` → `ticket-ingest`), so a process's cwd names its agent even
/// when a resumed session's argv no longer carries the kickoff — the authoritative identity signal for the
/// OS-process dedup check (task_1063). None if the path has no `agents/<segment>`. Pure — unit-tested.
fn agent_from_cwd(cwd: &str) -> Option<&str> {
    let mut segs = cwd.split('/');
    while let Some(s) = segs.next() {
        if s == "agents" {
            return segs.next().filter(|a| !a.is_empty());
        }
    }
    None
}

/// Group live agent instances `(pid, agent, start_tick)` by agent and decide the dedup action (task_1063): for
/// each agent with MORE THAN ONE live instance, keep the OLDEST (smallest start tick = longest-running
/// canonical) and mark the rest to kill (newest-first). Agents with a single instance are omitted — nothing to
/// do. Deterministic (agents sorted, kills ordered). Pure — unit-tested; the /proc scan and the actual kill
/// wrap this.
fn dedup_decision(instances: &[(u32, String, u64)]) -> Vec<(String, u32, Vec<u32>)> {
    let mut by_agent: std::collections::BTreeMap<&str, Vec<(u32, u64)>> =
        std::collections::BTreeMap::new();
    for (pid, agent, start) in instances {
        by_agent.entry(agent).or_default().push((*pid, *start));
    }
    let mut out = Vec::new();
    for (agent, mut procs) in by_agent {
        if procs.len() < 2 {
            continue;
        }
        // Oldest first (smallest start tick); keep[0] is the canonical, the rest are extras to kill.
        procs.sort_by_key(|(pid, start)| (*start, *pid));
        let keep = procs[0].0;
        let kill: Vec<u32> = procs[1..].iter().map(|(pid, _)| *pid).collect();
        out.push((agent.to_string(), keep, kill));
    }
    out
}

/// Read a numeric field from `/proc/<pid>/stat` by its 1-indexed overall field number, parsing PAST the
/// `(comm)` field (which may itself contain spaces/parens) by splitting after the last `)`. Field 4 = ppid,
/// field 5 = pgrp (process group), field 22 = starttime (clock ticks since boot). None if the stat file is
/// unreadable/malformed or the field is below 3 (pid/comm, before the split point).
fn proc_stat_field(pid: u32, field: usize) -> Option<u64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let after_comm = &stat[stat.rfind(')')? + 1..];
    // Fields after comm begin at `state` (overall field 3), so overall field N is index N-3 here.
    after_comm
        .split_whitespace()
        .nth(field.checked_sub(3)?)?
        .parse::<u64>()
        .ok()
}

/// The process start time (field 22 of `/proc/<pid>/stat`, clock ticks since boot) — a smaller value is an older
/// process.
fn proc_starttime(pid: u32) -> Option<u64> {
    proc_stat_field(pid, 22)
}

/// Walk the parent chain upward from `start`, collecting each ancestor pid (NOT `start` itself). `parent_of`
/// yields the ppid of a pid; the walk stops at the root (ppid <= 1) or on a cycle. Pure (the `/proc` ppid lookup
/// is injected) — unit-tested. Used by [`safe_pkill`] to protect the caller's whole ancestor chain (the invoking
/// shell, the Bash-tool wrapper, the agent's claude session) from a self-match.
fn ancestor_pids(start: u32, parent_of: &dyn Fn(u32) -> Option<u32>) -> Vec<u32> {
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    seen.insert(start); // never re-collect the start node, even on a cycle
    let mut cur = start;
    while let Some(p) = parent_of(cur) {
        if p <= 1 || !seen.insert(p) {
            break;
        }
        out.push(p);
        cur = p;
    }
    out
}

/// The pids to actually kill: those matching the pattern, MINUS the protected set (the caller's own pid, its
/// ancestor chain, and its process group), sorted + deduped. This is what makes `fleet safe-pkill` self-safe
/// where `pkill -f` is not — the pattern can appear in the caller's own argv (an inline/heredoc command), but a
/// protected pid is never returned. Pure — unit-tested.
fn safe_kill_targets(matches: &[u32], protected: &std::collections::HashSet<u32>) -> Vec<u32> {
    let mut out: Vec<u32> = matches
        .iter()
        .copied()
        .filter(|p| !protected.contains(p))
        .collect();
    out.sort_unstable();
    out.dedup();
    out
}

/// `fleet safe-pkill <pattern>` (task_1068): kill processes whose full command line contains `pattern`, but
/// NEVER the caller's own process, ancestor chain, or process group — so it cannot self-match the way
/// `pkill -f` does when the pattern string appears in the invoking shell's own argv (killing the agent's shell,
/// exit 143/144, no output). Reports matches + the kill set; `--dry_run` skips the kill.
fn safe_pkill(pattern: &str, dry_run: bool, signal: &str) {
    if pattern.is_empty() {
        eprintln!("fleet safe-pkill: refusing an empty pattern (would match every process)");
        std::process::exit(1);
    }
    let own = std::process::id();
    let own_pgrp = proc_stat_field(own, 5);
    let mut protected: std::collections::HashSet<u32> = std::collections::HashSet::new();
    protected.insert(own);
    for a in ancestor_pids(own, &|p| proc_stat_field(p, 4).map(|v| v as u32)) {
        protected.insert(a);
    }
    // One /proc pass: collect pattern matches, and protect every pid sharing the caller's process group.
    let mut matches: Vec<u32> = Vec::new();
    let mut cmdlines: std::collections::HashMap<u32, String> = std::collections::HashMap::new();
    if let Ok(entries) = std::fs::read_dir("/proc") {
        for e in entries.flatten() {
            let Some(pid) = e.file_name().to_str().and_then(|n| n.parse::<u32>().ok()) else {
                continue;
            };
            if own_pgrp.is_some() && proc_stat_field(pid, 5) == own_pgrp {
                protected.insert(pid);
            }
            let raw = std::fs::read(e.path().join("cmdline")).unwrap_or_default();
            let cmd = String::from_utf8_lossy(&raw).replace('\0', " ");
            let cmd = cmd.trim();
            if cmd.contains(pattern) {
                matches.push(pid);
                cmdlines.insert(pid, cmd.to_string());
            }
        }
    }
    let targets = safe_kill_targets(&matches, &protected);
    let excluded = matches.len().saturating_sub(targets.len());
    println!(
        "safe-pkill '{pattern}': {} match(es); {} target(s) after excluding self/ancestors/process-group ({excluded} self-protected)",
        matches.len(),
        targets.len()
    );
    for pid in &targets {
        let snip: String = cmdlines
            .get(pid)
            .map(|c| c.chars().take(100).collect())
            .unwrap_or_default();
        println!("  {pid}: {snip}");
    }
    if dry_run {
        println!("  (dry-run — nothing killed; re-run without --dry-run to send SIG{signal})");
        return;
    }
    for pid in &targets {
        let ok = std::process::Command::new("kill")
            .args(["-s", signal, &pid.to_string()])
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        println!(
            "  {} pid {pid}",
            if ok {
                format!("SIG{signal} ->")
            } else {
                "FAILED to signal".to_string()
            }
        );
    }
}

/// `fleet dedup-check` (task_1063): the OS-process-authoritative single-instance check. Scans `/proc` for live
/// `claude` sessions, maps each to its agent by cwd ([`agent_from_cwd`]), and reports any agent running more
/// than one instance (optionally filtered to `agent`). On `--kill`, SIGTERMs the extra instance(s), keeping the
/// oldest canonical per agent ([`dedup_decision`]). This is the method to use for a duplicate-instance report —
/// it sees a detached or differently-launched duplicate that the tmux-window guard (camshaft/fleet#327) misses.
fn dedup_check(filter: Option<&str>, kill: bool) {
    let mut instances: Vec<(u32, String, u64)> = Vec::new();
    if let Ok(entries) = std::fs::read_dir("/proc") {
        for e in entries.flatten() {
            let Some(pid) = e.file_name().to_str().and_then(|n| n.parse::<u32>().ok()) else {
                continue;
            };
            // A Claude Code agent session's process comm is exactly `claude`.
            if std::fs::read_to_string(e.path().join("comm"))
                .unwrap_or_default()
                .trim()
                != "claude"
            {
                continue;
            }
            let Ok(cwd) = std::fs::read_link(e.path().join("cwd")) else {
                continue;
            };
            let Some(agent) = agent_from_cwd(&cwd.to_string_lossy()).map(str::to_string) else {
                continue;
            };
            if filter.is_some_and(|f| f != agent) {
                continue;
            }
            instances.push((pid, agent, proc_starttime(pid).unwrap_or(0)));
        }
    }
    let dups = dedup_decision(&instances);
    println!(
        "dedup-check: {} live agent process(es){}",
        instances.len(),
        filter
            .map(|f| format!(" (filter: {f})"))
            .unwrap_or_default()
    );
    if dups.is_empty() {
        println!("  OK — every agent has at most one live instance (1:1 process:identity).");
        return;
    }
    for (agent, keep, killset) in &dups {
        println!(
            "  DUPLICATE '{agent}': {} live instances — canonical (oldest) pid {keep}, extra(s): {killset:?}",
            killset.len() + 1
        );
        if kill {
            for pid in killset {
                let ok = std::process::Command::new("kill")
                    .arg(pid.to_string())
                    .status()
                    .map(|s| s.success())
                    .unwrap_or(false);
                println!(
                    "    {} pid {pid}",
                    if ok { "SIGTERM ->" } else { "FAILED to kill" }
                );
            }
        }
    }
    if !kill {
        println!(
            "  (report-only — re-run with --kill to SIGTERM the extra instance(s), keeping the oldest canonical)"
        );
    }
}

/// The auto-force-identity wiring the `task-board` MCP server entry carries in a parsed `~/.claude.json`
/// (task_1039). `present` is whether a `mcpServers.task-board` entry exists at all; `static_header` is the value
/// of its `headers["X-Fleet-Agent"]` if set (e.g. `${FLEET_AGENT}`); `has_helper` is whether a `headersHelper`
/// is configured (a command that emits the header fresh at each connection). Extracted purely so the decision is
/// unit-testable without the real config.
#[derive(Debug, Clone, PartialEq, Eq)]
struct TaskboardIdentityConfig {
    present: bool,
    static_header: Option<String>,
    has_helper: bool,
}

/// Parse the identity wiring out of a `~/.claude.json` value — the `mcpServers.task-board` entry's
/// `headers["X-Fleet-Agent"]` and `headersHelper`. Pure — unit-tested.
fn taskboard_identity_config(config: &serde_json::Value) -> TaskboardIdentityConfig {
    let Some(entry) = config.get("mcpServers").and_then(|m| m.get("task-board")) else {
        return TaskboardIdentityConfig {
            present: false,
            static_header: None,
            has_helper: false,
        };
    };
    let static_header = entry
        .get("headers")
        .and_then(|h| h.get("X-Fleet-Agent"))
        .and_then(serde_json::Value::as_str)
        .map(str::to_string);
    let has_helper = entry
        .get("headersHelper")
        .and_then(serde_json::Value::as_str)
        .is_some_and(|s| !s.is_empty());
    TaskboardIdentityConfig {
        present: true,
        static_header,
        has_helper,
    }
}

/// The verdict of an [`mcp_check_decision`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum McpCheckVerdict {
    /// Identity is wired and the process carries the matching `FLEET_AGENT` — the client should force correctly.
    Pass,
    /// Wired, but something warrants a look (a `--mcp-config` that may shadow, or a static header value that is
    /// neither `${FLEET_AGENT}` nor the agent name with no helper to override it).
    Warn,
    /// The client cannot force identity as-is (no entry/header wired, or the process has no/wrong `FLEET_AGENT`).
    Fail,
}

/// Report produced by [`mcp_check_decision`].
#[derive(Debug, Clone, PartialEq, Eq)]
struct McpCheckReport {
    fleet_agent_matches: bool,
    uses_config_override: bool,
    identity_wired: bool,
    verdict: McpCheckVerdict,
    notes: Vec<String>,
}

/// Decide, purely, whether `agent`'s live claude session is wired to auto-force its board identity (task_1039):
/// the process environment must carry `FLEET_AGENT=<agent>` (what the `${FLEET_AGENT}` header / `headersHelper`
/// expands to), the user-scope `task-board` entry must wire the `X-Fleet-Agent` header, and a `--mcp-config` on
/// the argv is flagged because it may shadow the user-scope entry (which this check reads). Pure — the /proc and
/// config reads wrap this. Unit-tested.
fn mcp_check_decision(
    agent: &str,
    fleet_agent_env: Option<&str>,
    claude_argv: &[String],
    tb: &TaskboardIdentityConfig,
) -> McpCheckReport {
    let fleet_agent_matches = fleet_agent_env == Some(agent);
    let uses_config_override = claude_argv
        .iter()
        .any(|a| a == "--mcp-config" || a == "--strict-mcp-config");
    let identity_wired = tb.present && (tb.has_helper || tb.static_header.is_some());
    let mut notes = Vec::new();

    match fleet_agent_env {
        Some(v) if v == agent => notes.push(format!("FLEET_AGENT=\"{v}\" in the process env (matches the agent).")),
        Some(v) => notes.push(format!(
            "FLEET_AGENT=\"{v}\" in the process env but the agent is \"{agent}\" — identity would force to the WRONG id."
        )),
        None => notes.push(
            "FLEET_AGENT is NOT set in the process env — the ${FLEET_AGENT} header / helper expands to EMPTY, so no identity is forced."
                .to_string(),
        ),
    }
    if !tb.present {
        notes.push("No mcpServers.task-board entry in ~/.claude.json — no X-Fleet-Agent header is configured.".to_string());
    } else {
        match &tb.static_header {
            Some(h) => notes.push(format!("task-board headers[\"X-Fleet-Agent\"] = \"{h}\".")),
            None => notes.push("task-board has no static headers[\"X-Fleet-Agent\"].".to_string()),
        }
        notes.push(if tb.has_helper {
            "task-board headersHelper is configured (emits the header fresh at each connection)."
                .to_string()
        } else {
            "task-board has no headersHelper.".to_string()
        });
    }
    if uses_config_override {
        notes.push(
            "The claude argv passes --mcp-config/--strict-mcp-config — that config may SHADOW the user-scope task-board entry this check read; verify the override carries the header."
                .to_string(),
        );
    }

    let verdict = if !identity_wired || fleet_agent_env.is_none() {
        McpCheckVerdict::Fail
    } else if !fleet_agent_matches || uses_config_override {
        McpCheckVerdict::Warn
    } else {
        // Header wired and FLEET_AGENT matches. A helper emits the agent's value directly, so it is fine; a
        // static-only header must be `${FLEET_AGENT}` (expands to the matching env) or the literal agent name.
        let static_ok = tb.has_helper
            || tb
                .static_header
                .as_deref()
                .is_some_and(|h| h == "${FLEET_AGENT}" || h == agent);
        if static_ok {
            McpCheckVerdict::Pass
        } else {
            McpCheckVerdict::Warn
        }
    };
    McpCheckReport {
        fleet_agent_matches,
        uses_config_override,
        identity_wired,
        verdict,
        notes,
    }
}

/// Read a single environment variable `key` from `/proc/<pid>/environ` (NUL-separated `KEY=VALUE`). None if the
/// file is unreadable (not our process / gone) or the key is absent.
fn proc_environ_var(pid: u32, key: &str) -> Option<String> {
    let raw = std::fs::read(format!("/proc/{pid}/environ")).ok()?;
    let prefix = format!("{key}=");
    raw.split(|&b| b == 0)
        .filter_map(|kv| std::str::from_utf8(kv).ok())
        .find_map(|kv| kv.strip_prefix(&prefix).map(str::to_string))
}

/// Read a process's argv from `/proc/<pid>/cmdline` (NUL-separated). Empty when unreadable.
fn proc_cmdline(pid: u32) -> Vec<String> {
    std::fs::read(format!("/proc/{pid}/cmdline"))
        .map(|raw| {
            raw.split(|&b| b == 0)
                .filter(|s| !s.is_empty())
                .map(|s| String::from_utf8_lossy(s).into_owned())
                .collect()
        })
        .unwrap_or_default()
}

/// `fleet mcp-check <agent>` (task_1039): inspect a board-native agent's live Claude Code session and report
/// whether its auto-force-identity MCP wiring is in place. Finds the agent's `claude` process by cwd
/// ([`agent_from_cwd`], keeping the oldest if duplicated), reads `FLEET_AGENT` from its environment and its argv,
/// reads the user-scope `~/.claude.json` task-board entry, and prints the [`mcp_check_decision`] verdict.
/// Read-only — never writes config or signals a process.
fn mcp_check(agent: &str) {
    let mut pids: Vec<u32> = Vec::new();
    if let Ok(entries) = std::fs::read_dir("/proc") {
        for e in entries.flatten() {
            let Some(pid) = e.file_name().to_str().and_then(|n| n.parse::<u32>().ok()) else {
                continue;
            };
            // A Claude Code agent session's process comm is exactly `claude`.
            if std::fs::read_to_string(e.path().join("comm"))
                .unwrap_or_default()
                .trim()
                != "claude"
            {
                continue;
            }
            let Ok(cwd) = std::fs::read_link(e.path().join("cwd")) else {
                continue;
            };
            if agent_from_cwd(&cwd.to_string_lossy()) == Some(agent) {
                pids.push(pid);
            }
        }
    }
    if pids.is_empty() {
        println!(
            "mcp-check '{agent}': no live claude session found (no /proc entry with comm=claude cwd'd under agents/{agent})."
        );
        println!(
            "  (the agent may be down, or launched outside a worktree under .../agents/{agent})"
        );
        return;
    }
    // Keep the oldest (canonical) if duplicated — mirrors dedup-check's canonical choice.
    pids.sort_by_key(|p| (proc_starttime(*p).unwrap_or(0), *p));
    let pid = pids[0];
    if pids.len() > 1 {
        println!(
            "mcp-check '{agent}': {} live claude instances {pids:?} — inspecting the oldest canonical pid {pid} (run `fleet dedup-check {agent}` to resolve the duplication).",
            pids.len()
        );
    }

    let fleet_agent_env = proc_environ_var(pid, "FLEET_AGENT");
    let argv = proc_cmdline(pid);
    let tb = match std::env::var("HOME")
        .map_err(|_| "no HOME".to_string())
        .and_then(|home| {
            std::fs::read_to_string(format!("{home}/.claude.json")).map_err(|e| e.to_string())
        })
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).map_err(|e| e.to_string()))
    {
        Ok(v) => taskboard_identity_config(&v),
        Err(e) => {
            println!(
                "mcp-check '{agent}': could not read ~/.claude.json ({e}) — reporting without the task-board entry."
            );
            TaskboardIdentityConfig {
                present: false,
                static_header: None,
                has_helper: false,
            }
        }
    };

    let report = mcp_check_decision(agent, fleet_agent_env.as_deref(), &argv, &tb);
    let label = match report.verdict {
        McpCheckVerdict::Pass => "PASS",
        McpCheckVerdict::Warn => "WARN",
        McpCheckVerdict::Fail => "FAIL",
    };
    println!("mcp-check '{agent}' (claude pid {pid}): {label}");
    for note in &report.notes {
        println!("  - {note}");
    }
    match report.verdict {
        McpCheckVerdict::Pass => println!(
            "  => client is wired to auto-force identity for '{agent}'. A null attribution is then NOT a client-config problem — look server/transport-side."
        ),
        McpCheckVerdict::Warn => println!(
            "  => wired, but review the note(s) above before trusting auto-force for '{agent}'."
        ),
        McpCheckVerdict::Fail => println!(
            "  => client will NOT auto-force identity for '{agent}' as configured; fix the note(s) above, then re-bounce (headersHelper is cached per connection, so a config change needs a reconnect)."
        ),
    }
}

fn daemon_pids(daemon: &str) {
    let mut found: Vec<i64> = Vec::new();
    if let Ok(entries) = std::fs::read_dir("/proc") {
        for entry in entries.flatten() {
            // Only numeric /proc/<pid> entries are processes.
            let Ok(pid) = entry.file_name().to_string_lossy().parse::<i64>() else {
                continue;
            };
            let Ok(target) = std::fs::read_link(entry.path().join("exe")) else {
                continue;
            };
            let Some(target) = target.to_str() else {
                continue;
            };
            if exe_matches_daemon(target, daemon) && process_is_alive(pid) {
                found.push(pid);
            }
        }
    }
    found.sort_unstable();
    for pid in found {
        println!("{pid}");
    }
}

/// `fleet confirm-kill` (task_866): confirm a pid is dead with `kill -0` polling before a deploy/guard starts
/// the replacement, so a kill that false-negatived on an exe-scan can never leave two daemon instances
/// relaying to a live channel. Optionally SIGTERM first, poll until gone or the timeout, and optionally
/// escalate to SIGKILL + a short grace. Exits 1 (never returns) if the pid is still alive when it gives up.
fn confirm_kill(pid: i64, term: bool, timeout: u64, sigkill: bool) {
    const POLL: std::time::Duration = std::time::Duration::from_millis(200);
    const GRACE: std::time::Duration = std::time::Duration::from_secs(3);
    if term {
        send_signal("-TERM", pid);
    }
    let mut deadline = std::time::Instant::now() + std::time::Duration::from_secs(timeout);
    let mut sigkill_sent = false;
    loop {
        let alive = process_is_alive(pid);
        let passed = std::time::Instant::now() >= deadline;
        match kill_confirm_step(alive, passed, sigkill, sigkill_sent) {
            KillStep::ConfirmedGone => {
                println!("pid {pid} confirmed gone");
                return;
            }
            KillStep::KeepPolling => std::thread::sleep(POLL),
            KillStep::EscalateSigkill => {
                eprintln!("pid {pid} still alive after {timeout}s — escalating to SIGKILL");
                send_signal("-KILL", pid);
                sigkill_sent = true;
                deadline = std::time::Instant::now() + GRACE;
            }
            KillStep::GaveUp => {
                eprintln!(
                    "pid {pid} STILL ALIVE after kill-confirm — do NOT start a replacement (would double-run)"
                );
                std::process::exit(1);
            }
        }
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
    fn tunnel_pid_for_name_reads_the_meta_pid_of_the_matching_tunnel() {
        // The real `tunnel list --json` shape: tunnels[].tunnelName with the client pid at tunnels[].meta.pid.
        let json = r#"{
            "tunnels": [
                {"tunnelName": "other-tunnel", "meta": {"localPort": 9000, "pid": 111}},
                {"tunnelName": "membrain-board", "url": "https://x", "meta": {"localPort": 8880, "pid": 222}}
            ],
            "hasMore": false
        }"#;
        assert_eq!(tunnel_pid_for_name(json, "membrain-board"), Some(222));
        assert_eq!(tunnel_pid_for_name(json, "other-tunnel"), Some(111));
        // A name that is not registered yields None (nothing to shed — the handler then recreates fresh).
        assert_eq!(tunnel_pid_for_name(json, "absent"), None);
        // No tunnels at all, and malformed input, both yield None rather than panicking.
        assert_eq!(
            tunnel_pid_for_name(r#"{"tunnels": [], "hasMore": false}"#, "membrain-board"),
            None
        );
        assert_eq!(tunnel_pid_for_name("not json", "membrain-board"), None);
    }

    #[test]
    fn tunnel_probe_healthy_accepts_a_2xx_or_3xx_three_digit_code() {
        assert!(tunnel_probe_healthy("200"));
        assert!(
            tunnel_probe_healthy(" 204\n"),
            "trims surrounding whitespace"
        );
        // A 3xx means the request reached the origin through the client — the live board root 302-redirects,
        // so a redirect is a FORWARDING-healthy signal, not a failure.
        assert!(tunnel_probe_healthy("302"));
        assert!(tunnel_probe_healthy("301"));
        // A tunnel-edge gateway error means the client is alive-but-not-forwarding — unhealthy.
        assert!(!tunnel_probe_healthy("502"));
        assert!(!tunnel_probe_healthy("504"));
        // Empty output (curl error / timeout — the tunnel is not forwarding at all) is unhealthy.
        assert!(!tunnel_probe_healthy(""));
        assert!(!tunnel_probe_healthy("   "));
        assert!(!tunnel_probe_healthy("000"));
        // Guard against a stray non-3-digit or non-numeric string being read as a 2xx/3xx.
        assert!(!tunnel_probe_healthy("2"));
        assert!(!tunnel_probe_healthy("2000"));
        assert!(!tunnel_probe_healthy("3xx"));
    }

    #[test]
    fn tunnel_recovery_plan_prefers_systemd_then_recreate_then_errors() {
        // task_1182: --systemd-unit wins (systemd owns the lifecycle), and recreate is then irrelevant.
        assert_eq!(
            tunnel_recovery_plan(Some("tunnel@board.service"), ""),
            Ok(TunnelRecovery::SystemdRestart("tunnel@board.service"))
        );
        assert_eq!(
            tunnel_recovery_plan(Some("tunnel@board.service"), "tunnel create 8880 --name x"),
            Ok(TunnelRecovery::SystemdRestart("tunnel@board.service")),
            "systemd-unit takes precedence over a recreate string"
        );
        // No unit but a recreate command -> the standalone shed+recreate path.
        assert_eq!(
            tunnel_recovery_plan(None, "tunnel create 8880 --name x"),
            Ok(TunnelRecovery::ShedAndRecreate)
        );
        // A blank unit is treated as absent (falls through to recreate).
        assert_eq!(
            tunnel_recovery_plan(Some("  "), "tunnel create 8880 --name x"),
            Ok(TunnelRecovery::ShedAndRecreate)
        );
        // Neither given (both blank) is a misconfiguration -> Err, so the guard fails fast.
        assert!(tunnel_recovery_plan(None, "").is_err());
        assert!(tunnel_recovery_plan(Some(""), "   ").is_err());
    }

    #[test]
    fn build_kickoff_carries_the_comms_norms_and_shell_safety_clauses() {
        // task_1178: every agent's kickoff carries the two comms norms the hard-reject flips enforce
        // (seed == enforce), and task_1107: the pkill self-match shell-safety line.
        let k = build_kickoff("v-x", "/wt/x", "30m", Some("op-x"), false, false);
        // task_1164: do not DM a human identity; reach the operator via a routed question or concierge.
        assert!(
            k.contains("OPERATOR COMMS GO THROUGH CONCIERGE")
                && k.contains("do NOT send_message or DM a human identity"),
            "carries the operator-comms-via-concierge clause"
        );
        assert!(
            k.contains("routed_to=operator"),
            "names the pose_question(routed_to=operator) path to the operator"
        );
        // task_1150: an operator-block must carry an open routed question first.
        assert!(
            k.contains("OPERATOR-BLOCK NEEDS A QUESTION") && k.contains("bare operator-block"),
            "carries the operator-block-needs-a-question clause"
        );
        // task_1107: the pkill/pgrep self-match shell-safety line + the safe-pkill pointer.
        assert!(
            k.contains("SHELL SAFETY"),
            "carries the shell-safety clause"
        );
        assert!(
            k.contains("fleet safe-pkill"),
            "points at the safe-pkill helper instead of raw pkill -f"
        );
        // Reactive + proactive variants carry them too (the clauses are outside the tick-mode branch).
        for (reactive, proactive) in [(true, false), (false, true)] {
            let v = build_kickoff("v-y", "/wt/y", "30m", Some("op-x"), reactive, proactive);
            assert!(v.contains("OPERATOR COMMS GO THROUGH CONCIERGE"));
            assert!(v.contains("SHELL SAFETY"));
        }
    }

    #[test]
    fn build_kickoff_carries_the_core_tenets() {
        // task_1192 + task_1230 + task_1200: every agent's kickoff carries a MANDATE to read the CORE
        // tenets at the single source of truth (fleet contract doc_3330), not a baked verbatim copy that
        // would drift as the doc versions (operator directive 2026-10-02: pointer + mandate-to-read + a
        // changed-since-last-read re-read check). Universal across operator/non-operator/reactive/proactive.
        for (operator, reactive, proactive) in [
            (Some("op-x"), false, false),
            (None, false, false),
            (Some("op-x"), true, false),
            (None, false, true),
        ] {
            let k = build_kickoff("v-x", "/wt/x", "30m", operator, reactive, proactive);
            assert!(
                k.contains("CORE TENETS") && k.contains("doc_3330"),
                "carries the CORE TENETS clause pointing at doc_3330"
            );
            // Mandate to read the authoritative doc in full before acting (not a baked paraphrase).
            assert!(
                k.contains("get_document(doc_3330)") && k.contains("read it in full"),
                "mandates reading doc_3330 in full at the source"
            );
            // Changed-since-last-read re-read check.
            assert!(
                k.contains("if it changed since you last read it"),
                "mandates re-reading doc_3330 when its approved version has changed"
            );
        }
    }

    #[test]
    fn build_kickoff_proactive_ownership_forces_a_work_accomplished_sleep_gate() {
        // task_1138: a proactive-ownership agent's kickoff must FORBID idling until a work-accomplished gate,
        // overriding the work-conserving idle-when-no-task default — the loop is the lever, not charter prose.
        let p = build_kickoff("cameron-promo-assist", "/wt/x", "1h", None, false, true);
        assert!(
            p.contains("PROACTIVE-OWNERSHIP OVERRIDE"),
            "carries the override clause"
        );
        assert!(
            p.contains("STANDING production unit"),
            "names the always-actionable standing unit"
        );
        assert!(
            p.contains("do NOT fall back to the idle cadence") && p.contains("never idle-sleep"),
            "forbids idling until the gate"
        );
        assert!(
            p.contains("WORK-ACCOMPLISHED, not elapsed time"),
            "the sleep-gate is work-accomplished, not time (cameron's rule)"
        );
        // A NON-proactive worker (the default) must NOT carry the override — it keeps the plain work-conserving
        // pacing so a normal vertical is unchanged.
        let n = build_kickoff("v-x", "/wt/x", "1h", None, false, false);
        assert!(
            !n.contains("PROACTIVE-OWNERSHIP OVERRIDE"),
            "default worker is unchanged"
        );
        // Reactive responders are also unaffected by the proactive flag (mutually exclusive loop stances).
        let r = build_kickoff("frank", "/wt/x", "30m", None, true, false);
        assert!(!r.contains("PROACTIVE-OWNERSHIP OVERRIDE"));
    }

    #[test]
    fn build_kickoff_is_work_conserving_and_self_discovering() {
        let k = build_kickoff("v-x", "/wt/v-x", "30m", Some("op-x"), false, false);
        // Identity (#336): the board does NOT bind the session (a fresh unbound board per call), so
        // register_agent can't make later id-less calls work — the kickoff must say pass ids EXPLICITLY on
        // EVERY call, and still register once + self-discover the charter via get_agent.
        assert!(
            k.contains("register_agent"),
            "registers the record once (idempotent)"
        );
        assert!(k.contains("get_agent"), "self-discovers its charter");
        assert!(
            k.contains("does NOT bind your session"),
            "states the board never binds the session (#336)"
        );
        assert!(
            k.contains("every board call THAT TAKES ONE"),
            "explicit ids on every call that takes one, not a fallback"
        );
        assert!(
            k.contains("from_agent") && k.contains("created_by") && k.contains("actor"),
            "names the explicit-id params incl. send_message's from_agent"
        );
        // task_1024 (rule-level, not list_tasks-specific): pure READ/FILTER tools take NO caller identity, so an
        // agent_id bolted onto list_tasks/get_task hard-rejects — 5 agents hit this on their post-flip boot ticks.
        assert!(
            k.contains("PURE READ / FILTER tools take NO caller identity"),
            "states the no-identity-on-reads rule"
        );
        assert!(
            k.contains("get_task") && k.contains("unknown field agent_id"),
            "names get_task + the exact reject so an agent does not bolt agent_id onto a read"
        );
        // task_955: the typed-ref rule is for WRITTEN content; an id PARAMETER takes the bare integer, not a token.
        assert!(
            k.contains("id PARAMETER is the opposite") && k.contains("takes the BARE INTEGER"),
            "carves out that an id argument takes a bare integer, not a typed/quoted token"
        );
        // Per-tool identity map (#531): each tool names identity DIFFERENTLY, and the wrong field records
        // actor=null and self-notifies you on your own comment — the exact footgun task_531 exists to stop.
        assert!(
            k.contains("author on comment_task"),
            "names comment_task's author field explicitly (#531)"
        );
        assert!(
            k.contains("actor on update_task") && k.contains("created_by on create_task"),
            "per-tool write-identity map"
        );
        assert!(
            k.contains("wake on your OWN comment"),
            "warns that the wrong identity field self-notifies"
        );
        // Never precompute a task id (#526): reference only the id create_task returns.
        assert!(
            k.contains("Never PRECOMPUTE") && k.contains("id create_task RETURNS"),
            "bans guessing a task id (#526)"
        );
        // Typed references (task_584): a bare `#N` is hard-rejected on board bodies, so the kickoff mandates
        // a TYPED id (`task_N` or `owner/repo#N`) in any comment/message/post — else a reword-retry every time.
        assert!(
            k.contains("TYPED REFERENCES") && k.contains("owner/repo#N"),
            "mandates typed task/PR refs, not a bare #N (task_584)"
        );
        assert!(
            k.contains("hard-rejects a bare"),
            "states the board hard-rejects a bare #N in posted content"
        );
        assert!(k.contains("'v-x'") && k.contains("/wt/v-x"));
        // OWNER-CONFIRM gate (#352): a trace-derived destructive/operator action against a service you don't
        // own must be owner-confirmed before executing or routing (a near-miss almost restarted a stale unit).
        assert!(
            k.contains("OWNER-CONFIRM gate") && k.contains("OWNER-UNCONFIRMED"),
            "binds the owner-confirm gate for trace-derived destructive actions"
        );
        // Shared-task coordination (task_565): R1 give-the-owner-a-beat (owner files a flagged spin-off; a
        // coordinator only if the owner is absent/stalled) + R2 dedup-is-single-writer (one owner picks the
        // survivor and FREEZES; never symmetric-cancel your own dup, which deadlocks).
        assert!(
            k.contains("SHARED-TASK COORDINATION") && k.contains("the OWNER files the spin-off"),
            "R1: give the live owner a beat before a coordinator race-creates a flagged spin-off"
        );
        assert!(
            k.contains("DEDUP IS SINGLE-WRITER") && k.contains("symmetric-cancel"),
            "R2: dedup is single-writer; never symmetric-cancel your own dup (deadlock)"
        );
        // Dynamic loop (no fixed interval arg after /loop) — the agent self-paces.
        assert!(
            k.contains("/loop run one tick"),
            "dynamic /loop, not `/loop 30m`"
        );
        assert!(
            !k.contains("/loop 30m"),
            "must NOT pin a fixed interval on the loop"
        );
        // Work-conserving: gate the next wake on open assigned work + unread, long idle only when drained.
        assert!(k.contains("WORK-CONSERVING PACING"));
        assert!(k.contains("list_tasks with assignee 'v-x'"));
        assert!(k.contains("NEVER idle-sleep"));
        assert!(
            k.contains("about 30m"),
            "the interval is the idle-fallback ceiling"
        );
        // blocked/parked ≠ actionable (board-pm refinement): the self-check counts only todo/in_progress and
        // excludes a blocked/parked task, so a task parked on a blocker doesn't keep the loop hot.
        assert!(
            k.contains("todo/in_progress") && k.contains("NOT blocked/parked"),
            "blocked/parked is not actionable work"
        );
        // Doc-writing style guide clause (board-pm, operator-approved): every future author carries it.
        assert!(
            k.contains("Fleet Doc-Writing Style Guide"),
            "kickoff points authors at the doc-writing style guide"
        );
        // Banned-phrases self-check (operator writing policy): reference the maintained list (data-driven),
        // applied to any doc OR comment, until the #308 pre-submit scanner lands.
        assert!(
            k.contains("banned-phrases") && k.contains("doc OR comment"),
            "kickoff points authors at the banned-phrases list for docs and comments"
        );
        // task_589: never pass a shell $(cat file) substitution as an MCP content arg (stored verbatim,
        // silent clobber) + read back after a write; and no commit/PR attribution lines in board bodies.
        assert!(
            k.contains("$(cat file)") && k.contains("READ BACK"),
            "warns against a $(cat) MCP arg + mandates read-back-after-write (task_589)"
        );
        assert!(
            k.contains("Co-Authored-By:") && k.contains("not board content"),
            "bans commit/PR attribution lines in board bodies"
        );
        // External-dependency blocked → long/event-woken cadence (#349): the block carve-out is generalized
        // beyond the operator to ANY external dep (another agent, a pending deploy/CI), so an agent holding a
        // sole externally-blocked task sets it blocked + drops to the long cadence instead of SOON-polling.
        assert!(
            k.contains("BLOCKED ON AN EXTERNAL DEPENDENCY"),
            "generalizes the block carve-out beyond the operator (#349)"
        );
        assert!(
            k.contains("another agent") && k.contains("deploy/CI"),
            "names the non-operator external deps"
        );
        assert!(
            k.contains("long/event-woken cadence"),
            "a sole blocked task drops to the long/event-woken cadence, not SOON polling"
        );
        // Operator-blocked dashboard convention (operator seq-2292, re-pointed task_936) is preserved as a
        // sub-case, with the operator id INTERPOLATED from config (task_611), not hard-coded: OWNER-HELD +
        // typed blocked_on=operator, never a reassignment to the operator (operator-tenet 10).
        assert!(
            k.contains("ON THE OPERATOR specifically") && k.contains("OWNER-HELD"),
            "interpolates the configured operator id into the owner-held operator-blocked convention (task_936)"
        );
        assert!(
            k.contains("NEVER assign the task to 'op-x'"),
            "bans assigning the task to the operator (task_936, operator-tenet 10)"
        );
        assert!(
            k.contains("blocked_on=operator FILTER"),
            "the operator dashboard signal is the blocked_on=operator filter, not an assignment"
        );
        assert!(
            !k.contains("metadata.blocked_owner"),
            "drops the stale blocked_owner reassign-back stash now that ownership never changes (task_936)"
        );
        // No designated operator → the operator-blocked clause is omitted, but the surrounding external-dep
        // guidance stays intact (generic fleet with no operator; task_611).
        let k_no_op = build_kickoff("v-x", "/wt/v-x", "30m", None, false, false);
        assert!(
            !k_no_op.contains("ON THE OPERATOR specifically"),
            "omits the operator-blocked clause when no operator is configured"
        );
        assert!(
            k_no_op.contains("buys nothing. (If your MCP cannot set a typed blocked_on"),
            "the surrounding external-dep clause reads cleanly with the operator clause omitted"
        );
        // Status honesty (task_506 Layer 1): never stand down (offline/away) holding a live in_progress task —
        // progress it or re-state it blocked/done first; the companion watchdog warning flags the violation.
        assert!(
            k.contains("STATUS HONESTY") && k.contains("in_progress"),
            "bans standing down on a live in_progress task (task_506 Layer 1)"
        );
        assert!(
            k.contains("status-honesty violation the watchdog flags"),
            "ties the kickoff clause to the watchdog #506 warning"
        );
        // Self-close (task_604): the implementer flips its own completed task to done, never leaving a
        // "recommend closing" comment for the proposer/triage to flip hours later; a delegated-builder on a
        // task it does not own instead flags ready-to-close and the owner closes it promptly.
        assert!(
            k.contains("SELF-CLOSE") && k.contains("set its status to done YOURSELF"),
            "implementer self-closes a completed task (task_604)"
        );
        assert!(
            k.contains("DELEGATED to BUILD a task you do not OWN"),
            "carves out the delegated-builder case, which still closes promptly (task_604)"
        );
        // Drained / at-rest → persist a long cadence on the BOARD metadata (#383 + task_566): the lever is the
        // board metadata.interval (update_agent, the cadence the watchdog reads) which works even for a
        // board-only agent — NOT the frozen `cargo xtask fleet set-interval` which only writes the file-hub
        // registry (fails board-only, leaves the mirror stale). A raw reschedule also does not persist.
        assert!(
            k.contains("DONE / at-rest"),
            "routes a drained/done cluster to the rest-cadence path (#383)"
        );
        assert!(
            k.contains("metadata.interval") && k.contains("update_agent"),
            "board metadata.interval via update_agent is the board-native cadence lever (task_566)"
        );
        assert!(
            k.contains("board-only agent with no file-hub registry row"),
            "the board lever works for a board-only agent, unlike the frozen cargo xtask set-interval"
        );
        assert!(
            k.contains("does NOT persist"),
            "explains a raw next-tick reschedule does not stick against the watchdog"
        );
        // task_765 Tier A: a <=1h cadence (30m here) is schedulable by the dynamic self-wake, so the kickoff
        // carries NO cron-escalation clause — only a >1h cadence does.
        assert!(
            !k.contains("CronCreate"),
            "a <=1h cadence gets no cron-cadence escalation (task_765)"
        );
        assert!(
            !k.contains("hard-clamped to a ONE-HOUR maximum"),
            "no clamp note for a cadence the self-wake can honor"
        );
    }

    #[test]
    fn build_kickoff_above_one_hour_escalates_to_a_fixed_interval_cron() {
        // task_765 Tier A: the dynamic /loop self-wake clamps to a 1h max, so a monitor whose idle cadence
        // exceeds an hour (3h here) idle-polls hourly unless it rests off a CronCreate fixed-interval loop.
        // The kickoff must say so and hand it the concrete cron.
        let k = build_kickoff("v-mon", "/wt/v-mon", "3h", Some("op-x"), false, false);
        assert!(
            k.contains("hard-clamped to a ONE-HOUR maximum"),
            "names the dynamic self-wake 1h clamp (the root cause)"
        );
        assert!(
            k.contains("CronCreate"),
            "routes a >1h cadence to a CronCreate fixed-interval loop"
        );
        assert!(
            k.contains("'0 */3 * * *'"),
            "hands a 3h cadence its concrete cron expression"
        );
        assert!(
            k.contains("do NOT also self-re-arm"),
            "warns against a dynamic re-arm fighting the cron (the v-hivemind failure)"
        );
        assert!(
            k.contains("never your revival latency"),
            "clarifies event-wake still revives immediately, so the cron only sets the idle floor"
        );
        // A reactive responder with a >1h cadence hits the same clamp, so it carries the clause too.
        let r = build_kickoff("frank", "/wt/frank", "2h", None, true, false);
        assert!(
            r.contains("CronCreate") && r.contains("'0 */2 * * *'"),
            "a >1h reactive cadence also gets the cron escalation"
        );
    }

    #[test]
    fn long_cadence_cron_clause_only_fires_above_one_hour_and_matches_the_cadence() {
        // <=1h (incl. exactly 1h = 3600s, the clamp ceiling) is schedulable by the dynamic self-wake -> empty.
        assert!(
            long_cadence_cron_clause("30m").is_empty(),
            "30m needs no escalation"
        );
        assert!(
            long_cadence_cron_clause("1h").is_empty(),
            "exactly 1h is at the clamp ceiling, still schedulable"
        );
        assert!(
            long_cadence_cron_clause("3600s").is_empty(),
            "3600s == 1h, no escalation"
        );
        // Whole-hour cadences over 1h get a concrete `0 */H * * *` cron.
        assert!(long_cadence_cron_clause("2h").contains("'0 */2 * * *'"));
        assert!(long_cadence_cron_clause("6h").contains("'0 */6 * * *'"));
        // A non-whole-hour cadence over 1h (90m) still escalates, but with the generic form (cron is
        // minute-granular, so there is no tidy `0 */H` to suggest) — never a bogus hourly cron.
        let c = long_cadence_cron_clause("90m");
        assert!(
            c.contains("a fixed-interval cron matching your cadence"),
            "90m gets the generic cron form"
        );
        assert!(
            !c.contains("*/1"),
            "never suggests an hourly cron for a 90m cadence"
        );
        // A garbage interval parses to nothing -> no clause (never panics).
        assert!(long_cadence_cron_clause("nonsense").is_empty());
    }

    #[test]
    fn kill_confirm_step_covers_the_poll_states() {
        use KillStep::*;
        // Gone is confirmed regardless of the other inputs (the whole point: once kill -0 says ESRCH, stop).
        assert_eq!(kill_confirm_step(false, false, false, false), ConfirmedGone);
        assert_eq!(kill_confirm_step(false, true, true, true), ConfirmedGone);
        // Alive and within the deadline -> keep polling.
        assert_eq!(kill_confirm_step(true, false, true, false), KeepPolling);
        assert_eq!(kill_confirm_step(true, false, false, false), KeepPolling);
        // Alive past the deadline, --sigkill requested and not yet sent -> escalate once.
        assert_eq!(kill_confirm_step(true, true, true, false), EscalateSigkill);
        // Alive past the deadline, SIGKILL already sent -> give up (no second escalation).
        assert_eq!(kill_confirm_step(true, true, true, true), GaveUp);
        // Alive past the deadline, --sigkill NOT requested -> give up (still alive, do not start replacement).
        assert_eq!(kill_confirm_step(true, true, false, false), GaveUp);
    }

    #[test]
    fn process_is_alive_tracks_real_liveness() {
        // Our own pid is alive right now.
        assert!(process_is_alive(std::process::id() as i64));
        // A pid at the top of the i32 range is not a live process on this host; kill -0 reports ESRCH.
        assert!(!process_is_alive(i64::from(i32::MAX)));
    }

    #[test]
    fn exe_matches_daemon_is_shell_agnostic_about_the_deleted_inode_suffix() {
        // The exact case the zsh `${exe% (deleted)}` strip false-negatived (task_866/task_924): a rebuilt
        // binary's /proc/<pid>/exe reads `<path> (deleted)`, which under zsh's parens-glob did NOT strip, so
        // the live daemon read as absent and a second instance was started. In Rust the suffix strips cleanly.
        assert!(exe_matches_daemon(
            "/local/home/u/membrain-skynet-bridge/target/debug/membrain-skynet-bridge (deleted)",
            "membrain-skynet-bridge"
        ));
        // A clean (non-deleted) binary matches by basename too.
        assert!(exe_matches_daemon(
            "/nix/store/abc/bin/membrain-skynet-bridge",
            "membrain-skynet-bridge"
        ));
        // A different binary does not match.
        assert!(!exe_matches_daemon(
            "/usr/bin/other (deleted)",
            "membrain-skynet-bridge"
        ));
        // A sibling whose name merely starts with the daemon is NOT a basename match (no false positive).
        assert!(!exe_matches_daemon(
            "/x/membrain-skynet-bridge-helper",
            "membrain-skynet-bridge"
        ));
        // The strip is SUFFIX-only: a mid-path `(deleted)` directory never corrupts the basename match.
        assert!(exe_matches_daemon(
            "/x/y (deleted)/membrain-skynet-bridge",
            "membrain-skynet-bridge"
        ));
    }

    #[test]
    fn redeploy_action_rebuilds_only_when_behind_clean_and_on_main() {
        // Built rev already matches origin/main -> nothing to do (even if dirty / off main).
        assert_eq!(
            redeploy_action("abc123", "abc123", false, true),
            RedeployAction::UpToDate
        );
        assert_eq!(
            redeploy_action("abc123", "abc123", true, false),
            RedeployAction::UpToDate
        );
        // A -dirty build of the SAME commit is current (compares by base sha).
        assert_eq!(
            redeploy_action("abc123-dirty", "abc123", false, true),
            RedeployAction::UpToDate
        );
        // Behind + clean + on main -> safe to fast-forward + rebuild.
        assert_eq!(
            redeploy_action("old111", "new222", false, true),
            RedeployAction::Rebuild
        );
        // Behind but DIRTY -> refuse (never clobber uncommitted work).
        assert!(matches!(
            redeploy_action("old111", "new222", true, true),
            RedeployAction::NeedsManual(_)
        ));
        // Behind but OFF main (a feature branch) -> refuse (a ff-only would fail / clobber intent).
        assert!(matches!(
            redeploy_action("old111", "new222", false, false),
            RedeployAction::NeedsManual(_)
        ));
    }

    #[test]
    fn redeploy_builds_cover_every_daemon_binary() {
        // Each build is non-empty cargo args, and the set covers BOTH daemon binaries: the `fleet` bin
        // (fleet-watchdog + fleet-notify) and the separate fleet-tunnel bin (its own crate + transport
        // feature). The #451 regression guard: dropping fleet-tunnel here restarts it stale.
        assert!(
            REDEPLOY_BUILDS.iter().all(|b| !b.is_empty()),
            "no empty build arg-set"
        );
        let flat: Vec<&str> = REDEPLOY_BUILDS
            .iter()
            .flat_map(|b| b.iter().copied())
            .collect();
        assert!(
            flat.contains(&"fleet") && flat.windows(2).any(|w| w == ["--bin", "fleet"]),
            "builds --bin fleet"
        );
        assert!(
            flat.contains(&"fleet-tunnel"),
            "builds the fleet-tunnel crate"
        );
        assert!(
            flat.contains(&"transport"),
            "fleet-tunnel needs its transport feature"
        );
        // Every service backed by a binary that must exist has a build (both counts stay aligned).
        assert!(!REDEPLOY_BUILDS.is_empty() && !DEFAULT_REDEPLOY_SERVICES.is_empty());
    }

    #[test]
    fn build_kickoff_reactive_mode_only_acts_when_addressed() {
        let r = build_kickoff("frank", "/wt/frank", "30m", Some("op-x"), true, false);
        // Still self-discovering + explicit-identity like every kickoff (the boot contract is shared).
        assert!(
            r.contains("register_agent") && r.contains("get_agent"),
            "reactive kickoff still self-discovers"
        );
        assert!(r.contains("'frank'"), "carries the agent id");
        assert!(r.contains("/loop run one tick"), "still a dynamic /loop");
        // The reactive discipline (#438): only being ADDRESSED is actionable; ambient chatter is NOT work.
        assert!(r.contains("REACTIVE responder"), "declares reactive mode");
        assert!(
            r.contains("EXPLICITLY") && r.contains("ADDRESSED"),
            "an explicit address is a trigger"
        );
        // #438 thread-engagement: an in-thread follow-up on a conversation it is already in ALSO triggers it —
        // the live thread-subscription delivery wakes it on a reply_to=subscribed-root post, and the prompt
        // must count that as addressed so it continues the exchange instead of treating it as ambient chatter.
        assert!(
            r.contains("reply_to") && r.contains("thread_subscribed"),
            "an in-thread follow-up is a trigger"
        );
        assert!(
            r.contains("FOLLOW-UP"),
            "names the thread follow-up trigger"
        );
        assert!(
            r.contains("WAIT TO BE WOKEN"),
            "goes idle and waits for an event-wake when unaddressed"
        );
        // CRITICAL: it must NOT carry the work-conserving 'unread => keep going SOON' pacing that mis-fires on
        // ambient channel chatter (the exact anti-pattern the observer caught in Frank's boot).
        assert!(
            !r.contains("WORK-CONSERVING PACING"),
            "reactive mode drops the work-conserving pacing"
        );
        assert!(
            !r.contains("keep going — schedule your next tick SOON"),
            "no SOON re-poll on unread chatter"
        );
        // And the default (non-reactive) kickoff must be UNCHANGED — it keeps the work-conserving pacing.
        let w = build_kickoff("v-x", "/wt/v-x", "30m", Some("op-x"), false, false);
        assert!(
            w.contains("WORK-CONSERVING PACING") && !w.contains("REACTIVE responder"),
            "default worker unchanged"
        );
    }

    #[test]
    fn build_launch_cmd_wires_claude_and_codex_and_rejects_unknown() {
        // claude is fully wired: the exec line carries the model/effort and reads the kickoff from the env.
        let c = build_launch_cmd("claude", "claude-x", "high", None).expect("claude wired");
        assert!(c.starts_with("exec claude "));
        assert!(c.contains("--model 'claude-x'") && c.contains("--effort 'high'"));
        assert!(
            c.contains("\"$CDZ_KICKOFF\""),
            "kickoff rides in the env var, not interpolated"
        );
        // devshell (#214): opt-in launch inside the workdir's flake devShell so the pinned toolchain is on PATH.
        let d =
            build_launch_cmd("claude", "claude-x", "high", Some("/wt/v-x")).expect("claude wired");
        assert!(
            d.starts_with("exec nix develop \"path:/wt/v-x\" --command claude "),
            "wrapped in nix develop"
        );
        assert!(
            d.contains("--model 'claude-x'") && d.contains("\"$CDZ_KICKOFF\""),
            "same claude args inside the devShell"
        );
        // codex is wired: bypass flag for unattended run, model passed through single-quoted, kickoff from env.
        let x = build_launch_cmd("codex", "codex-m", "high", None).expect("codex wired");
        assert!(x.starts_with("exec codex "));
        assert!(
            x.contains("--dangerously-bypass-approvals-and-sandbox"),
            "unattended: no approval/sandbox gate"
        );
        assert!(
            x.contains("--model 'codex-m'"),
            "model passed through (board data supplies the concrete name)"
        );
        assert!(
            x.contains("\"$CDZ_KICKOFF\""),
            "codex reads the same kickoff env var, not an interpolated prompt"
        );
        assert!(
            !x.contains("--effort"),
            "codex takes no --effort flag (claude-only)"
        );
        // codex honors the same devShell wrapping as claude.
        let xd =
            build_launch_cmd("codex", "codex-m", "high", Some("/wt/v-x")).expect("codex wired");
        assert!(
            xd.starts_with("exec nix develop \"path:/wt/v-x\" --command codex "),
            "codex wrapped in nix develop too"
        );
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
        assert!(
            !codex_trust_missing(&v, "/wt/v-x"),
            "the rendered table marks the dir trusted"
        );
        // A key with TOML-special chars is escaped so the table still parses and round-trips.
        let weird = codex_trust_table(r#"/wt/a"b\c"#);
        let vw: toml::Value = weird.parse().expect("escaped key is valid toml");
        assert!(
            !codex_trust_missing(&vw, r#"/wt/a"b\c"#),
            "escaped key round-trips to the same dir"
        );
    }

    #[test]
    fn spin_down_action_covers_native_busy_window_and_windowless() {
        use SpinDownAction::*;
        // Not board-native → refuse regardless of window/assignments/force (file-hub uses cargo xtask fleet remove).
        assert_eq!(
            spin_down_action(false, true, false, 0, false),
            NotBoardNative
        );
        assert_eq!(
            spin_down_action(false, false, false, 3, true),
            NotBoardNative
        );
        // Native + a working pane + no --force → refuse so a running agent is never killed mid-turn.
        assert_eq!(spin_down_action(true, true, true, 0, false), RefuseBusy);
        // --force overrides the busy refusal → offline + kill.
        assert_eq!(spin_down_action(true, true, true, 0, true), OfflineAndKill);
        // Native + an IDLE live window → offline then kill (stops the loop).
        assert_eq!(
            spin_down_action(true, true, false, 0, false),
            OfflineAndKill
        );
        // Native + no window → offline ONLY (still mark offline so up-board leaves it stood down); force moot.
        assert_eq!(spin_down_action(true, false, false, 0, false), OfflineOnly);
        assert_eq!(spin_down_action(true, false, true, 0, true), OfflineOnly);
    }

    #[test]
    fn batch_targets_excludes_the_stay_up_set_and_sorts() {
        let native: Vec<String> = ["v-b", "concierge", "v-a", "v-fleet-tooling", "v-nix", "v-c"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let except: std::collections::BTreeSet<String> = ["concierge", "v-nix", "v-fleet-tooling"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        // Only non-excepted natives, sorted; the stay-up set (incl. self) is never a target.
        assert_eq!(batch_targets(&native, &except), vec!["v-a", "v-b", "v-c"]);
        // Empty except = every native is a candidate.
        let none = std::collections::BTreeSet::new();
        assert_eq!(batch_targets(&native, &none).len(), native.len());
    }

    #[test]
    fn reconstitute_targets_excludes_up_and_except_sorts_and_waves() {
        // down_native carries a dup (v-a) and is unsorted — the selector sorts + dedups like batch_targets.
        let down: Vec<String> = ["v-b", "v-a", "v-c", "v-a"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let except: std::collections::BTreeSet<String> =
            ["v-c"].iter().map(|s| s.to_string()).collect();
        // No limit: sorted, deduped, the leave-as-is set removed.
        assert_eq!(
            reconstitute_targets(&down, &except, None),
            vec!["v-a", "v-b"]
        );
        // --limit caps the wave to the first N (sorted) candidates — the next wave picks up the rest.
        assert_eq!(reconstitute_targets(&down, &except, Some(1)), vec!["v-a"]);
        // Empty except = every DOWN agent is a candidate (deduped), still wave-limited.
        let none = std::collections::BTreeSet::new();
        assert_eq!(
            reconstitute_targets(&down, &none, Some(2)),
            vec!["v-a", "v-b"]
        );
        assert_eq!(
            reconstitute_targets(&down, &none, None),
            vec!["v-a", "v-b", "v-c"]
        );
    }

    #[test]
    fn host_service_registry_parses_and_resolves_by_name_alias_and_port() {
        // task_1027 lookup, task_1031 fix: the real registry is a LOCAL, uncommitted resource (host topology
        // must not live in the public repo), so this tests the pure parser + lookup against an INLINE fixture
        // — no include_str!, no filesystem, no real host/service names in the repo.
        let fixture = r#"
            [[service]]
            name = "svc-proxy"
            aliases = [":9999", "svc-proxy-alias"]
            owner = "owner-a"
            definition = "<pointer>"
            lifeline = true
            note = "example lifeline"

            [[service]]
            name = "svc-worker"
            owner = "owner-b"
            definition = "<pointer>"
        "#;
        let reg = parse_host_service_registry(fixture).expect("fixture parses");
        assert_eq!(reg.service.len(), 2);
        // Resolve by name, by a word alias, and by a port alias — all the one service, case-insensitively.
        let by_name = find_host_service(&reg.service, "svc-proxy").expect("by name");
        let by_alias = find_host_service(&reg.service, "svc-proxy-alias").expect("by alias");
        let by_port = find_host_service(&reg.service, ":9999").expect("by port alias");
        assert_eq!(by_name.name, "svc-proxy");
        assert_eq!(by_alias.name, "svc-proxy");
        assert_eq!(by_port.name, "svc-proxy");
        assert!(by_name.lifeline && by_name.owner == "owner-a");
        assert_eq!(
            find_host_service(&reg.service, "SVC-PROXY").map(|s| s.name.as_str()),
            Some("svc-proxy")
        );
        // `lifeline` defaults to false when omitted; an unknown query resolves to None.
        assert!(
            !find_host_service(&reg.service, "svc-worker")
                .expect("worker present")
                .lifeline
        );
        assert!(find_host_service(&reg.service, "no-such-service").is_none());
    }

    #[test]
    fn spin_down_action_refuses_an_agent_holding_open_assignments_unless_forced() {
        use SpinDownAction::*;
        // task_786: the stranding guard — an agent holding open assignments must not be stood down. Refuse in
        // BOTH the windowless (the task_311 class — spun down yet still holding dispatched work) and windowed
        // cases, taking priority over the busy check so the operator sees the reassign requirement first.
        assert_eq!(
            spin_down_action(true, false, false, 1, false),
            RefuseHoldsAssignments { open: 1 }
        );
        assert_eq!(
            spin_down_action(true, true, false, 2, false),
            RefuseHoldsAssignments { open: 2 }
        );
        assert_eq!(
            spin_down_action(true, true, true, 5, false),
            RefuseHoldsAssignments { open: 5 }
        );
        // --force overrides the stranding refusal → it falls through to the normal window/windowless action.
        assert_eq!(spin_down_action(true, true, false, 3, true), OfflineAndKill);
        assert_eq!(spin_down_action(true, false, false, 3, true), OfflineOnly);
        // Zero open assignments → the guard is inert (normal behavior).
        assert_eq!(
            spin_down_action(true, true, false, 0, false),
            OfflineAndKill
        );
    }

    #[test]
    fn bounce_action_fences_non_native_windowless_busy_and_cooldown() {
        use BounceAction::*;
        // Not board-native → refuse regardless of window/working/cooldown/force (only board-native agents
        // relaunch via the fleet; there is nothing for a bounce to relaunch otherwise).
        assert_eq!(
            bounce_action(false, true, false, false, false),
            NotBoardNative
        );
        assert_eq!(bounce_action(false, true, true, true, true), NotBoardNative);
        // Native but no live window → nothing to bounce (not running); --force does not conjure a session.
        assert_eq!(bounce_action(true, false, false, false, false), NoWindow);
        assert_eq!(bounce_action(true, false, false, false, true), NoWindow);
        // Native + a working pane + no --force → refuse so a reconnect never interrupts a live turn.
        assert_eq!(bounce_action(true, true, true, false, false), RefuseBusy);
        // --force overrides the busy fence → bounce (the owner accepts interrupting the live turn).
        assert_eq!(bounce_action(true, true, true, false, true), Bounce);
        // Native + idle + on cooldown + no --force → skip so a healthy session is not thrashed.
        assert_eq!(bounce_action(true, true, false, true, false), Cooldown);
        // --force overrides the cooldown fence → bounce.
        assert_eq!(bounce_action(true, true, false, true, true), Bounce);
        // Busy takes priority over cooldown (both set, no force → the busy refusal is surfaced first).
        assert_eq!(bounce_action(true, true, true, true, false), RefuseBusy);
        // Native + an idle live window + not on cooldown → bounce (the normal case).
        assert_eq!(bounce_action(true, true, false, false, false), Bounce);
    }

    #[test]
    fn bounce_on_cooldown_suppresses_only_within_the_window() {
        // Never bounced → not on cooldown.
        assert!(!bounce_on_cooldown(None, 10_000, BOUNCE_COOLDOWN_SECS));
        // Bounced just now / within the window → on cooldown.
        assert!(bounce_on_cooldown(Some(10_000), 10_000, 1_800));
        assert!(bounce_on_cooldown(Some(10_000), 10_000 + 1_799, 1_800));
        // Exactly at / past the window → no longer on cooldown.
        assert!(!bounce_on_cooldown(Some(10_000), 10_000 + 1_800, 1_800));
        assert!(!bounce_on_cooldown(Some(10_000), 20_000, 1_800));
        // Clock skew (now < last) saturates to 0 elapsed → treated as on cooldown (fail-safe, no thrash).
        assert!(bounce_on_cooldown(Some(10_000), 9_000, 1_800));
    }

    #[test]
    fn stale_launch_for_deploy_flags_a_mismatch_or_unknown_rev() {
        // task_752: a session launched under the SAME rev as the running sweep is fresh — not a candidate.
        assert!(!stale_launch_for_deploy(Some("abc123"), "abc123"));
        // Launched under a different (older) rev → stale, a bounce candidate to refresh its tools/list.
        assert!(stale_launch_for_deploy(Some("old999"), "abc123"));
        // No recorded launch rev (a session predating stamping, or launched outside spin-up) → treated as
        // stale: a one-time catch-up bounce establishes the stamp. The cooldown fence + report-only-first
        // default keep that safe, and a bounced session re-stamps the current rev so it drops off the list.
        assert!(stale_launch_for_deploy(None, "abc123"));
        // An empty stamp reads as None upstream (read_launch_rev), so the Some("") shape never reaches here;
        // but even if it did, it differs from a real rev and is correctly stale.
        assert!(stale_launch_for_deploy(Some(""), "abc123"));
    }

    #[test]
    fn revive_stranded_on_cooldown_suppresses_only_within_the_window() {
        // task_818: a stranded agent auto-revive is suppressed only while the last revive is within the window,
        // so a revive that did not immediately clear the stranded state is not retried every sweep.
        // Never revived → not on cooldown.
        assert!(!revive_stranded_on_cooldown(
            None,
            10_000,
            REVIVE_STRANDED_COOLDOWN_SECS
        ));
        // Within the window → on cooldown.
        assert!(revive_stranded_on_cooldown(Some(10_000), 10_000, 3_600));
        assert!(revive_stranded_on_cooldown(
            Some(10_000),
            10_000 + 3_599,
            3_600
        ));
        // At / past the window → no longer on cooldown (eligible to re-revive if still stranded).
        assert!(!revive_stranded_on_cooldown(
            Some(10_000),
            10_000 + 3_600,
            3_600
        ));
        assert!(!revive_stranded_on_cooldown(Some(10_000), 20_000, 3_600));
        // Clock skew (now < last) saturates to 0 elapsed → treated as on cooldown (fail-safe, no re-revive).
        assert!(revive_stranded_on_cooldown(Some(10_000), 9_000, 3_600));
    }

    #[test]
    fn recover_wedged_on_cooldown_suppresses_only_within_the_window() {
        // task_582: a wedged-agent auto-recovery (force spin-down + spin-up) is suppressed only while the last
        // recovery is within the window, so a fresh session that re-wedges on the same input is not thrashed.
        assert!(!recover_wedged_on_cooldown(
            None,
            10_000,
            RECOVER_WEDGED_COOLDOWN_SECS
        ));
        assert!(recover_wedged_on_cooldown(Some(10_000), 10_000, 3_600));
        assert!(recover_wedged_on_cooldown(
            Some(10_000),
            10_000 + 3_599,
            3_600
        ));
        assert!(!recover_wedged_on_cooldown(
            Some(10_000),
            10_000 + 3_600,
            3_600
        ));
        assert!(!recover_wedged_on_cooldown(Some(10_000), 20_000, 3_600));
        // Clock skew (now < last) saturates to 0 elapsed → on cooldown (fail-safe, no re-recover).
        assert!(recover_wedged_on_cooldown(Some(10_000), 9_000, 3_600));
    }

    #[test]
    fn recover_wedged_gated_by_flag_and_quiesce() {
        // task_582 + task_1032: auto-recovery runs only when --recover-wedged is set AND the fleet is not
        // quiesced (force-restarting a parked agent during a mass spin-down would fight the operator).
        assert!(watchdog_action_enabled(true, false));
        assert!(!watchdog_action_enabled(true, true));
        assert!(!watchdog_action_enabled(false, false));
    }

    #[test]
    fn agent_is_stranded_flags_a_stood_down_agent_holding_open_work() {
        // task_786 corrective: stood down + >=1 open assignment → STRANDED (the cr-reviewer class).
        assert!(agent_is_stranded(true, 1));
        assert!(agent_is_stranded(true, 4));
        // A stood-down agent with an empty queue is a legitimate rest — not stranded.
        assert!(!agent_is_stranded(true, 0));
        // A RUNNING agent holding work is not stranded (it has a live loop) — that is task_794's lane.
        assert!(!agent_is_stranded(false, 3));
        assert!(!agent_is_stranded(false, 0));
    }

    #[test]
    fn backlog_over_depth_signals_a_running_agent_above_threshold_only() {
        // task_794: a RUNNING agent strictly OVER the threshold is the hire/route signal.
        assert!(backlog_over_depth(false, 3, BACKLOG_DEPTH_DEFAULT)); // 3 > 2
        assert!(!backlog_over_depth(false, 2, BACKLOG_DEPTH_DEFAULT)); // at threshold, not over
        assert!(!backlog_over_depth(false, 0, BACKLOG_DEPTH_DEFAULT));
        // A STOOD-DOWN agent is excluded here (its unworked queue is the task_786 STRANDED signal instead) so
        // the two mechanisms never double-signal the same agent.
        assert!(!backlog_over_depth(true, 9, BACKLOG_DEPTH_DEFAULT));
        // Threshold is tunable.
        assert!(backlog_over_depth(false, 2, 1)); // 2 > 1
        assert!(!backlog_over_depth(false, 2, 5));
    }

    #[test]
    fn fmt_hook_install_action_never_clobbers_a_foreign_hook() {
        use FmtHookAction::*;
        // Absent → install ours.
        assert_eq!(fmt_hook_install_action(None), Install);
        // Our own hook (marker present) → refresh (idempotent).
        assert_eq!(
            fmt_hook_install_action(Some(&fmt_precommit_hook_body())),
            Refresh
        );
        // A foreign hook → NEVER clobber.
        assert_eq!(
            fmt_hook_install_action(Some("#!/bin/sh\n# someone else's pre-commit\n")),
            SkipForeign
        );
    }

    #[test]
    fn fmt_precommit_hook_is_fail_open_valid_bash() {
        let b = fmt_precommit_hook_body();
        assert!(b.contains(FMT_HOOK_MARKER), "carries the ownership marker");
        assert!(b.contains("FLEET_SKIP_FMT_HOOK"), "has the silencer");
        // task_617: no bare `cargo fmt --all --check` left in the installed script — that was the bug (a
        // pre-existing skew in an unrelated crate tripped it on every commit). The smart per-crate logic now
        // lives in `fmt_hook_run`, tested separately; the script only delegates to it.
        assert!(
            !b.contains("--all"),
            "no workspace-wide fmt check in the installed script"
        );
        assert!(
            b.contains("fleet fmt-hook-run"),
            "delegates the real logic to the testable Rust subcommand"
        );
        // task_782: the content guardrail step delegates to `fleet scan-content --staged`, is OPT-IN via
        // FLEET_CONTENT_TAXONOMY (no-op where unset, so a generic worktree is unchanged), is FAIL-CLOSED
        // (`|| exit 1` blocks the commit on a hit), and has its OWN bypass so the fmt silencer cannot disable
        // the guardrail.
        assert!(
            b.contains("fleet scan-content --staged"),
            "runs the content guardrail on staged content"
        );
        assert!(
            b.contains("FLEET_CONTENT_TAXONOMY"),
            "the content guardrail is opt-in by the private-taxonomy env (no-op where unset)"
        );
        assert!(
            b.contains("|| exit 1"),
            "FAIL-CLOSED: a blocking content hit stops the commit"
        );
        assert!(
            b.contains("FLEET_SKIP_CONTENT_HOOK"),
            "content guardrail has its own bypass, independent of the fmt silencer"
        );
        assert!(
            b.trim_end().ends_with("exit 0"),
            "the hook still exits 0 by default (fmt is fail-open; only a content hit blocks)"
        );
        // Syntax-check with `bash -n` (a broken hook would fail every commit in the shared mirror); skip if absent.
        let dir = std::env::temp_dir().join(format!("fleet-fmthook-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let f = dir.join("pre-commit");
        if std::fs::write(&f, &b).is_err() {
            return; // can't stage the file — skip the syntax check rather than false-fail
        }
        if let Ok(o) = std::process::Command::new("bash")
            .arg("-n")
            .arg(&f)
            .output()
        {
            assert!(
                o.status.success(),
                "hook bash syntax error:\n{}",
                String::from_utf8_lossy(&o.stderr)
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn owning_manifest_finds_the_nearest_package_manifest_above_a_staged_file() {
        let repo_root = Path::new("/repo");
        // crates/fleet/src/main.rs owned by crates/fleet/Cargo.toml, not the workspace root.
        let is_package = |p: &Path| p == Path::new("/repo/crates/fleet/Cargo.toml");
        let found = owning_manifest(repo_root, Path::new("/repo/crates/fleet/src"), is_package);
        assert_eq!(found, PathBuf::from("/repo/crates/fleet/Cargo.toml"));
    }

    #[test]
    fn owning_manifest_falls_back_to_the_repo_root_when_no_crate_manifest_is_found() {
        let repo_root = Path::new("/repo");
        let found = owning_manifest(repo_root, Path::new("/repo/loose/dir"), |_| false);
        assert_eq!(found, PathBuf::from("/repo/Cargo.toml"));
    }

    #[test]
    fn owning_manifest_never_walks_above_repo_root() {
        // A pathological file_dir outside repo_root must not escape upward past it looking for a manifest.
        let repo_root = Path::new("/repo");
        let found = owning_manifest(repo_root, Path::new("/repo"), |p| {
            p == Path::new("/Cargo.toml")
        });
        assert_eq!(
            found,
            PathBuf::from("/repo/Cargo.toml"),
            "stops at repo_root, never checks above it"
        );
    }

    #[test]
    fn is_cargo_package_manifest_distinguishes_crate_from_workspace_root() {
        let dir =
            std::env::temp_dir().join(format!("fleet-fmthook-manifest-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let crate_manifest = dir.join("crate-Cargo.toml");
        let workspace_manifest = dir.join("workspace-Cargo.toml");
        std::fs::write(&crate_manifest, "[package]\nname = \"x\"\n").unwrap();
        std::fs::write(&workspace_manifest, "[workspace]\nmembers = []\n").unwrap();
        assert!(is_cargo_package_manifest(&crate_manifest));
        assert!(!is_cargo_package_manifest(&workspace_manifest));
        assert!(!is_cargo_package_manifest(&dir.join("missing-Cargo.toml")));
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
        assert!(
            s.body.contains("CHILD proposal task"),
            "body states the child-filing contract"
        );
        assert!(
            s.body.contains("Close this task"),
            "body states the close-on-done contract"
        );
    }

    #[test]
    fn observer_kickoff_drives_from_the_observation_task_when_given_one() {
        let with = build_observer_kickoff(
            "v-t",
            "sess-9",
            42,
            "loops/observer.md",
            "/abs/fleet",
            Some(207),
        );
        // The observation-task variant carries the child-filing + close-the-parent contract.
        assert!(
            with.contains("OBSERVATION TASK #207"),
            "names the observation task"
        );
        assert!(
            with.contains("parent_id=207"),
            "proposals are children of the observation task"
        );
        assert!(
            with.contains("update_task 207"),
            "closes the observation task"
        );
        assert!(with.contains("status=\"done\""), "close = mark done");

        let without =
            build_observer_kickoff("v-t", "sess-9", 42, "loops/observer.md", "/abs/fleet", None);
        // The rollout-compat variant files standalone proposals — no parent linkage, no task close. ("update_task"
        // alone appears in the shared identity preamble; the CLOSE contract is `status="done"` on the task.)
        assert!(
            !without.contains("parent_id="),
            "no child linkage without an observation task"
        );
        assert!(
            !without.contains("OBSERVATION TASK #"),
            "not driven by an observation task"
        );
        assert!(
            !without.contains("status=\"done\""),
            "no task close without an observation task"
        );
        assert!(
            without.contains("above-floor"),
            "still files curated proposals"
        );

        // Both variants keep the invariant boot + confirm contract.
        for k in [&with, &without] {
            assert!(
                k.contains("register_agent 'observer'"),
                "binds the observer identity"
            );
            assert!(
                k.contains("observe-record v-t --session sess-9"),
                "confirms via observe-record last"
            );
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
            vec![
                "correctness-completeness",
                "clarity-writing",
                "risk-security",
                "alternatives"
            ]
        );
        // The clarity angle names the MAINTAINED lists as run-time truth (not a hardcoded pattern copy).
        let clarity = REVIEW_ANGLES
            .iter()
            .find(|(k, _)| *k == "clarity-writing")
            .unwrap()
            .1;
        assert!(
            clarity.contains("three-pass"),
            "clarity angle applies the humanize three-pass"
        );
        assert!(
            clarity.contains("Document #7") && clarity.contains("Document #8"),
            "names the maintained writing lists"
        );
        assert!(
            clarity.contains("READ AT REVIEW TIME"),
            "reads the lists at review time, not a frozen copy"
        );
    }

    #[test]
    fn build_reviewer_kickoff_reviews_one_review_on_one_angle_and_never_transitions() {
        let (key, focus) = REVIEW_ANGLES[0]; // correctness-completeness
        let k = build_reviewer_kickoff(88, key, focus, "/repo/loops/reviewer.md", "/abs/fleet");
        // Identity + ephemeral (one review, no loop) — the reviewer analog of the observer kickoff.
        assert!(
            k.contains("register_agent 'reviewer'"),
            "binds the stable reviewer identity"
        );
        assert!(
            k.contains("author=\"reviewer\""),
            "authors board writes as reviewer"
        );
        assert!(
            k.contains("do NOT start a /loop") && k.contains("you do not loop"),
            "ephemeral one-shot, not a loop"
        );
        // Drives from the review + its angle.
        assert!(
            k.contains("review #88") && k.contains("get_review 88"),
            "reads the assigned review"
        );
        assert!(
            k.contains("ANGLE `correctness-completeness`"),
            "carries the assigned angle"
        );
        assert!(k.contains(focus), "carries the angle's review lens");
        assert!(
            k.contains("/repo/loops/reviewer.md") && k.contains("/abs/fleet"),
            "names the role + this binary"
        );
        // Files findings on the review log (+ actionable child tasks); does NOT transition status (D17 person-gate).
        assert!(
            k.contains("append_review_log") && k.contains("review_id=88"),
            "records findings on the review log"
        );
        assert!(k.contains("finding"), "entries are findings");
        assert!(
            k.contains("CHILD task"),
            "an actionable finding links a child task"
        );
        assert!(
            k.contains("DO NOT call set_review_status"),
            "the reviewer never transitions the review (D17 person-gate)"
        );
        assert!(
            k.contains("no-op finding"),
            "a clean angle still records that it ran"
        );
        // The clarity angle carries the maintained-lists-at-review-time contract when built for it.
        let clarity_k = build_reviewer_kickoff(
            88,
            REVIEW_ANGLES[1].0,
            REVIEW_ANGLES[1].1,
            "/r/loops/reviewer.md",
            "/abs/fleet",
        );
        assert!(
            clarity_k.contains("three-pass") && clarity_k.contains("Document #7"),
            "clarity kickoff points at the writing guide + three-pass"
        );
    }

    #[test]
    fn parse_workspace_kind_reads_setup_script_and_config_hints() {
        let rec = serde_json::json!({
            "name": "example-env",
            "description": "a board-defined environment",
            "setup_script": "echo materialize\n",
            "verify": "test -d \"$FLEET_WORKSPACE_CWD/bin\"\n",
            "config": {
                "cwd": "/work/example",
                "pre_trust": ["/work/example/sub", "/opt/toolchain"],
                "env": { "FOO": "bar", "IGNORED_NUM": 7 }
            }
        });
        let p = parse_workspace_kind("v-example", "/home/u/.fleet", &rec, None);
        assert_eq!(p.name, "example-env");
        assert_eq!(
            p.description.as_deref(),
            Some("a board-defined environment")
        );
        assert_eq!(p.setup_script.as_deref(), Some("echo materialize\n"));
        assert_eq!(
            p.verify.as_deref(),
            Some("test -d \"$FLEET_WORKSPACE_CWD/bin\"\n"),
            "task_1174: the post-setup verify command is read"
        );
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
        let rec = serde_json::json!({ "name": "bare", "setup_script": "   \n", "verify": "  \n" });
        let p = parse_workspace_kind("v-bare", "/home/u/.fleet", &rec, None);
        assert_eq!(p.cwd, workspace::agent_root_dir("/home/u/.fleet", "v-bare"));
        assert_eq!(
            p.pre_trust,
            vec!["/home/u/.fleet".to_string(), p.cwd.clone()]
        );
        assert!(
            p.setup_script.is_none(),
            "whitespace-only setup_script is treated as absent"
        );
        assert!(
            p.verify.is_none(),
            "whitespace-only verify is treated as absent (task_1174)"
        );
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
        assert_eq!(
            p.cwd, "/work/agent-a",
            "per-agent override beats config.cwd"
        );
        assert_eq!(
            p.env,
            vec![("K".to_string(), "v".to_string())],
            "shared env still comes from the kind"
        );
        // A blank/whitespace override is ignored → falls back to config.cwd.
        let p2 = parse_workspace_kind("m-b", "/home/u/.fleet", &rec, Some("   "));
        assert_eq!(
            p2.cwd, "/shared/default",
            "blank override falls back to config.cwd"
        );
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
            verify: None,
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
                (
                    "FLEET_WORKSPACE_CWD".to_string(),
                    "/work/agent-a".to_string()
                ),
                ("K".to_string(), "v".to_string()),
            ],
            "built-ins first (incl. the resolved launch cwd), then the kind's config env"
        );
    }

    #[test]
    fn read_native_is_tristate_absent_is_unknown_not_false() {
        use serde_json::json;
        // Explicit true/false are decisive.
        assert!(matches!(
            read_native(Some(&json!({"native": true}))),
            NativeVerdict::Native
        ));
        assert!(matches!(
            read_native(Some(&json!({"native": false}))),
            NativeVerdict::NotNative
        ));
        // task_500: an ABSENT native key, entirely missing metadata, and a non-bool native are ALL Unknown
        // (fail-safe) — the task_418 regression dropped metadata, which must NOT read as a hard not-native.
        assert!(matches!(
            read_native(Some(&json!({"interval": "3h"}))),
            NativeVerdict::Unknown
        ));
        assert!(matches!(read_native(None), NativeVerdict::Unknown));
        assert!(matches!(
            read_native(Some(&json!({"native": "true"}))),
            NativeVerdict::Unknown
        ));
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
        assert_eq!(
            watchdog_verdict(ivi * 3, iv),
            "STALE",
            "beyond the overdue window → re-arm candidate"
        );
        assert_eq!(
            watchdog_verdict(-120, iv),
            "ok",
            "clock skew (future last_seen) is ok"
        );
        assert_eq!(
            watchdog_verdict(999999, 0),
            "ok",
            "unknown interval → never flagged"
        );
    }

    #[test]
    fn is_retighten_candidate_flags_stale_or_work_on_a_long_interval_but_not_late() {
        // A DRAINED queue (0 open tasks) is NEVER a candidate — any verdict, any interval (#332): a
        // mission-complete agent that lengthened its own cadence is doing sanctioned idle, not stalling, so
        // re-arming it ("keep looping until your queue drains") is a phantom nag (v-capmeshd / v-nmidid: 0 open
        // tasks yet STALE against a short registered interval).
        assert!(
            !is_retighten_candidate("STALE", 0, 600),
            "drained + STALE → NOT a candidate (was the bug)"
        );
        assert!(
            !is_retighten_candidate("late", 0, 600),
            "late is normal idle, and the queue is empty anyway"
        );
        assert!(
            !is_retighten_candidate("ok", 0, 6 * 3600),
            "drained on a long interval → sanctioned idle"
        );
        // HOLDS open work → candidate when the loop stalled (STALE) or it idles on a long interval it should
        // be draining tighter.
        assert!(
            is_retighten_candidate("STALE", 1, 600),
            "stalled loop holding work → re-arm"
        );
        assert!(is_retighten_candidate("ok", 2, 3600), "1h+ with open tasks");
        assert!(is_retighten_candidate("late", 1, 6 * 3600));
        // Open work on a SHORT interval that is cycling (ok/late) is fine — it's already tight.
        assert!(
            !is_retighten_candidate("ok", 3, 600),
            "10m with tasks is already tight"
        );
        assert!(
            !is_retighten_candidate("late", 3, 600),
            "short-interval late is the normal cycling band"
        );
    }

    #[test]
    fn work_driven_rearm_catches_a_quiet_work_holder_regardless_of_interval() {
        let wc = WATCHDOG_WORK_CADENCE_SECS;
        // Holds work and has been quiet beyond the short work cadence → candidate, even though (elsewhere) its
        // interval may be moderate and is_retighten_candidate would call it "already tight" (the #535 gap).
        assert!(work_driven_rearm(1, Some(wc), false));
        assert!(
            work_driven_rearm(3, Some(wc * 100), false),
            "long quiet with work → still a candidate"
        );
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
        assert!(drained_idle_candidate(
            0, short, false, false, false, false, "ok"
        ));
        assert!(
            drained_idle_candidate(0, short, false, false, false, false, "late"),
            "late still counts (ticking)"
        );
        // Has actionable work → not this path (the work-driven/retighten paths own that).
        assert!(!drained_idle_candidate(
            1, short, false, false, false, false, "ok"
        ));
        // Already at a long interval → nothing to lengthen (this is what convergence looks like).
        assert!(!drained_idle_candidate(
            0, long, false, false, false, false, "ok"
        ));
        // Stood down → a spun-down agent is left alone, not lengthened.
        assert!(!drained_idle_candidate(
            0, short, true, false, false, false, "ok"
        ));
        // Reactive responder paces on mentions, not on an idle interval → excluded.
        assert!(!drained_idle_candidate(
            0, short, false, true, false, false, "ok"
        ));
        // Deliberate continuous monitor (holds a monitor_exempt task) is MEANT to poll → never lengthened.
        assert!(!drained_idle_candidate(
            0, short, false, false, true, false, "ok"
        ));
        // #544 fix: a PATROL/sweep agent (board-follow-up / board-triage) runs 0 tasks at a short cadence BY
        // DESIGN — lengthening it would blind the anti-stall layer, so it is excluded even though every other
        // signal says "drained self-poller".
        assert!(
            !drained_idle_candidate(0, short, false, false, false, true, "ok"),
            "patrol agent is never lengthened"
        );
        // STALE = a stopped loop (a re-arm/relaunch case), not an over-eager poller → not this path.
        assert!(!drained_idle_candidate(
            0, short, false, false, false, false, "STALE"
        ));
        // Unknown/zero interval → can't call it a short self-poller.
        assert!(!drained_idle_candidate(
            0, 0, false, false, false, false, "ok"
        ));
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
        assert!(
            !owners.contains("v-worker"),
            "a non-exempt task's owner is not a deliberate monitor"
        );
        assert_eq!(owners.len(), 1);
    }

    #[test]
    fn agent_never_ticked_flags_a_launched_but_dead_on_arrival_loop() {
        use time::{Duration, format_description::well_known::Rfc3339};
        let now = time::OffsetDateTime::now_utc();
        let stamp = |d: Duration| (now - d).format(&Rfc3339).unwrap();
        let grace = 600; // 10m

        // last_seen never advanced past created_at, and it registered well before the grace window → the
        // #417 launch-crash signature (board-triage/board-follow-up: online but never ticked).
        let old = stamp(Duration::minutes(20));
        assert!(
            agent_never_ticked(&old, &old, now, grace),
            "equal stamps past the grace window → never-ticked"
        );

        // Still inside the grace window: a just-registered agent legitimately has last_seen == created_at
        // until its first tick lands — must NOT flag.
        let fresh = stamp(Duration::minutes(2));
        assert!(
            !agent_never_ticked(&fresh, &fresh, now, grace),
            "within grace → still booting, not a crash"
        );

        // Ticked at least once (last_seen advanced past created_at) → live/idle, never a never-ticked flag,
        // however old the heartbeat is.
        let created = stamp(Duration::hours(4));
        let seen = stamp(Duration::minutes(30));
        assert!(
            !agent_never_ticked(&created, &seen, now, grace),
            "advanced last_seen → it ticked, not dead-on-arrival"
        );

        // Epsilon: a sub-2s precision difference between the two columns still counts as equal ...
        let created_eps = stamp(Duration::minutes(20));
        let seen_eps = stamp(Duration::minutes(20) - Duration::seconds(1));
        assert!(
            agent_never_ticked(&created_eps, &seen_eps, now, grace),
            "1s column drift is within epsilon"
        );
        // ... but a real multi-second advance is a genuine tick.
        let seen_ticked = stamp(Duration::minutes(20) - Duration::seconds(5));
        assert!(
            !agent_never_ticked(&created_eps, &seen_ticked, now, grace),
            "5s advance is a real tick"
        );

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
        assert_eq!(
            match_session_id_in("sess-9", &candidates),
            Some(candidates[1].clone())
        );
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
        assert!(
            !pane_shows_working(idle),
            "idle ❯ overrides lingering footer/token remnant"
        );
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
        assert!(!roster_metadata_stripped(&[
            serde_json::json!({"id":"a","metadata":{"native":true}})
        ]));
        // An empty roster is a board outage / no agents, NOT a metadata-stripping bug → do not warn.
        assert!(!roster_metadata_stripped(&[]));
    }

    #[test]
    fn inprogress_task_assignees_collects_nonempty_owners_deduped() {
        let tasks = vec![
            serde_json::json!({"id":1,"assignee":"v-bolero","status":"in_progress"}),
            serde_json::json!({"id":2,"assignee":"v-bolero","status":"in_progress"}), // same owner → deduped
            serde_json::json!({"id":3,"assignee":"librarian","status":"in_progress"}),
            serde_json::json!({"id":4,"assignee":"","status":"in_progress"}), // unassigned → dropped
            serde_json::json!({"id":5,"status":"in_progress"}), // no assignee → dropped
        ];
        let owners = inprogress_task_assignees(&tasks);
        assert!(owners.contains("v-bolero") && owners.contains("librarian"));
        assert_eq!(
            owners.len(),
            2,
            "deduped, and empty/absent assignees dropped"
        );
        // Empty task list → empty set (a board query error degrades here → no false #506 violations).
        assert!(inprogress_task_assignees(&[]).is_empty());
    }

    #[test]
    fn presence_is_at_rest_covers_offline_and_away_only() {
        // task_506 Phase A.5: a not-working presence while holding a live in_progress task is a violation.
        assert!(
            presence_is_at_rest(Some("offline")),
            "stood down is at-rest (Phase A)"
        );
        assert!(
            presence_is_at_rest(Some("away")),
            "away is a not-working presence (Phase A.5)"
        );
        // Working presences are never a violation.
        assert!(!presence_is_at_rest(Some("online")), "online is working");
        assert!(!presence_is_at_rest(Some("busy")), "busy is working");
        // Unknown/absent presence is not treated as a violation (fail-safe: no false flag).
        assert!(
            !presence_is_at_rest(None),
            "unknown presence is not a violation"
        );
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
        assert!(
            !owners.contains("v-monitor"),
            "a purely monitor-exempt owner is not holding work at rest"
        );
        assert_eq!(owners.len(), 2);
    }

    #[test]
    fn blocked_task_owners_collects_only_genuinely_parked_task_owners() {
        let tasks = vec![
            // Parked on an external party → its owner is a switch-to-actionable candidate (task_736).
            serde_json::json!({"id":1,"assignee":"v-a","status":"blocked","blocked_on_kind":"operator"}),
            serde_json::json!({"id":2,"assignee":"v-b","status":"blocked","blocked_on_kind":"task"}),
            // A `blocked` status with NO blocked_on_kind is malformed → excluded (defensive).
            serde_json::json!({"id":3,"assignee":"v-c","status":"blocked","blocked_on_kind":""}),
            serde_json::json!({"id":4,"assignee":"v-d","status":"blocked"}),
        ];
        let owners = blocked_task_owners(&tasks);
        assert!(owners.contains("v-a") && owners.contains("v-b"));
        assert!(
            !owners.contains("v-c") && !owners.contains("v-d"),
            "a blocked task with no blocker is excluded"
        );
        assert_eq!(owners.len(), 2);
        assert!(blocked_task_owners(&[]).is_empty());
    }

    #[test]
    fn should_switch_to_actionable_requires_blocked_plus_other_actionable_plus_idle() {
        // The canonical fire: owns a blocked task, owns >=1 actionable other, and is at-rest.
        assert!(should_switch_to_actionable(true, true, 1, false));
        // Not at-rest but quiet while holding work (#535 work-driven) also fires.
        assert!(should_switch_to_actionable(false, true, 2, true));
        // No OTHER actionable task → nothing to switch to; idling on the block is legitimate.
        assert!(!should_switch_to_actionable(true, true, 0, false));
        // Owns actionable work but NO blocked task → the plain never-idle guards handle it, not this nudge.
        assert!(!should_switch_to_actionable(true, false, 2, true));
        // Blocked + actionable but actively progressing (not at-rest, not quiet) → presumed working, no nudge.
        assert!(!should_switch_to_actionable(false, true, 2, false));
    }

    #[test]
    fn task_is_monitor_exempt_reads_the_derived_bool_defaulting_false() {
        assert!(task_is_monitor_exempt(
            &serde_json::json!({"id":1,"monitor_exempt":true})
        ));
        assert!(!task_is_monitor_exempt(
            &serde_json::json!({"id":1,"monitor_exempt":false})
        ));
        // Absent (pre-#167 payload or a non-exempt row) → false, so nothing is wrongly skipped.
        assert!(!task_is_monitor_exempt(&serde_json::json!({"id":1})));
    }

    #[test]
    fn task_is_parked_on_blocker_flags_any_non_empty_blocked_on_kind() {
        // task-board#178: an external/infra wait is parked, not stalled → skip its nudge.
        assert!(task_is_parked_on_blocker(
            &serde_json::json!({"id":1,"blocked_on_kind":"external"})
        ));
        // Any other blocker kind (operator/task) is likewise parked.
        assert!(task_is_parked_on_blocker(
            &serde_json::json!({"id":1,"blocked_on_kind":"operator"})
        ));
        assert!(task_is_parked_on_blocker(
            &serde_json::json!({"id":1,"blocked_on_kind":"task"})
        ));
        // Absent or empty blocker → NOT parked, so a genuinely stale unblocked task is still nudged.
        assert!(!task_is_parked_on_blocker(&serde_json::json!({"id":1})));
        assert!(!task_is_parked_on_blocker(
            &serde_json::json!({"id":1,"blocked_on_kind":""})
        ));
        assert!(!task_is_parked_on_blocker(
            &serde_json::json!({"id":1,"blocked_on_kind":null})
        ));
    }

    #[test]
    fn seam_glob_matches_handles_star_doublestar_and_literals() {
        // Literal exact path.
        assert!(seam_glob_matches(
            "crates/foo/perform_arg_ground.rs",
            "crates/foo/perform_arg_ground.rs"
        ));
        assert!(!seam_glob_matches(
            "crates/foo/other.rs",
            "crates/foo/perform_arg_ground.rs"
        ));
        // `**` spans path segments.
        assert!(seam_glob_matches("crates/foo/a/b.rs", "crates/foo/**"));
        assert!(seam_glob_matches("crates/foo/a/b/c.rs", "crates/foo/**"));
        assert!(!seam_glob_matches("crates/bar/a.rs", "crates/foo/**"));
        // `**/<file>` matches that file at ANY depth.
        assert!(seam_glob_matches(
            "a/b/perform_arg_ground.rs",
            "**/perform_arg_ground.rs"
        ));
        assert!(seam_glob_matches(
            "perform_arg_ground.rs",
            "**/perform_arg_ground.rs"
        ));
        // Single `*` is one segment only — does NOT cross `/`.
        assert!(seam_glob_matches("crates/foo/mod.rs", "crates/*/mod.rs"));
        assert!(
            !seam_glob_matches("crates/foo/bar/mod.rs", "crates/*/mod.rs"),
            "* must not cross a slash"
        );
        // `*.rs` is top-level only.
        assert!(seam_glob_matches("x.rs", "*.rs"));
        assert!(
            !seam_glob_matches("a/x.rs", "*.rs"),
            "*.rs is one segment, not recursive"
        );
        // `**.rs` (or **/*.rs) matches nested.
        assert!(seam_glob_matches("a/x.rs", "**/*.rs"));
    }

    #[test]
    fn seam_touched_returns_only_on_seam_paths_and_is_green_when_empty() {
        let seams = vec![
            "crates/effects/**".to_string(),
            "**/const_fold.rs".to_string(),
        ];
        let changed = vec![
            "crates/effects/perform_arg_ground.rs".to_string(), // on seam (** dir)
            "crates/opt/const_fold.rs".to_string(),             // on seam (** file)
            "docs/readme.md".to_string(),                       // off seam
            "crates/syntax/lexer.rs".to_string(),               // off seam
        ];
        let hit = seam_touched(&changed, &seams);
        assert_eq!(
            hit,
            vec![
                "crates/effects/perform_arg_ground.rs",
                "crates/opt/const_fold.rs"
            ]
        );
        // No seam-touching change -> GREEN (empty) -> caller need not wake the model.
        let clean = vec![
            "docs/x.md".to_string(),
            "crates/syntax/lexer.rs".to_string(),
        ];
        assert!(
            seam_touched(&clean, &seams).is_empty(),
            "no on-seam change is GREEN"
        );
        // No declared seam -> nothing can match -> GREEN (the command treats 'no seam' separately).
        assert!(seam_touched(&changed, &[]).is_empty());
    }

    #[test]
    fn split_seam_globs_partitions_includes_and_excludes() {
        let seams = vec![
            "backend/cadenza/**".to_string(),
            "!backend/cadenza/select/**".to_string(),
            "**/lower.rs".to_string(),
        ];
        let (inc, exc) = split_seam_globs(&seams);
        assert_eq!(inc, vec!["backend/cadenza/**", "**/lower.rs"]);
        assert_eq!(exc, vec!["backend/cadenza/select/**"]);
        // All-include list -> no excludes.
        assert!(split_seam_globs(&["a/**".to_string()]).1.is_empty());
    }

    #[test]
    fn seam_touched_excludes_shared_subtrees_with_bang_globs() {
        // v-cadenza-backend's real curation (observer comment_4445): own the re-emit lowering broadly, but
        // EXCLUDE the HOP2-shared select/** subtree so another lane's churn there does not wake this monitor.
        let seams = vec![
            "backend/cadenza/**".to_string(),
            "!backend/cadenza/select/**".to_string(),
        ];
        let changed = vec![
            "backend/cadenza/lower.rs".to_string(), // owned seam -> on
            "backend/cadenza/opt.rs".to_string(),   // owned seam -> on
            "backend/cadenza/select/marshal.rs".to_string(), // shared, excluded -> off
            "backend/cadenza/select/reclaim.rs".to_string(), // shared, excluded -> off
        ];
        assert_eq!(
            seam_touched(&changed, &seams),
            vec!["backend/cadenza/lower.rs", "backend/cadenza/opt.rs"],
            "an excluded shared subtree must not count as an on-seam change"
        );
        // An exclude that shadows the ONLY changed path -> GREEN (do not wake).
        let only_shared = vec!["backend/cadenza/select/marshal.rs".to_string()];
        assert!(
            seam_touched(&only_shared, &seams).is_empty(),
            "only-excluded change is GREEN"
        );
    }

    #[test]
    fn assistant_stop_reason_reads_only_completed_assistant_turns() {
        assert_eq!(
            assistant_stop_reason(
                &serde_json::json!({"type":"assistant","message":{"stop_reason":"refusal"}})
            ),
            Some("refusal".to_string())
        );
        assert_eq!(
            assistant_stop_reason(
                &serde_json::json!({"type":"assistant","message":{"stop_reason":"end_turn"}})
            ),
            Some("end_turn".to_string())
        );
        // Non-assistant records / missing stop_reason -> None (not counted as a turn).
        assert_eq!(
            assistant_stop_reason(
                &serde_json::json!({"type":"user","message":{"stop_reason":"refusal"}})
            ),
            None
        );
        assert_eq!(
            assistant_stop_reason(
                &serde_json::json!({"type":"assistant","message":{"role":"assistant"}})
            ),
            None
        );
        assert_eq!(
            assistant_stop_reason(&serde_json::json!({"type":"system"})),
            None
        );
    }

    #[test]
    fn safeguard_wedge_flags_only_a_trailing_run_of_refusals() {
        let r = |s: &[&str]| s.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        // A trailing run of >= threshold refusals -> WEDGED.
        assert!(safeguard_wedge(
            &r(&["end_turn", "refusal", "refusal", "refusal"]),
            3
        ));
        assert!(safeguard_wedge(&r(&["refusal", "refusal"]), 2));
        // Fewer than threshold trailing refusals -> not wedged.
        assert!(!safeguard_wedge(&r(&["refusal", "refusal"]), 3));
        // A refusal run that is NOT at the tail (recovered after) -> not wedged (the agent is working again).
        assert!(!safeguard_wedge(
            &r(&["refusal", "refusal", "refusal", "end_turn"]),
            3
        ));
        // Mixed tail -> not wedged.
        assert!(!safeguard_wedge(&r(&["refusal", "tool_use", "refusal"]), 3));
        // Empty / threshold 0 -> never wedged (no false positive on a fresh or empty transcript).
        assert!(!safeguard_wedge(&[], 3));
        assert!(!safeguard_wedge(&r(&["refusal", "refusal", "refusal"]), 0));
    }

    #[test]
    fn tail_stop_reasons_parses_only_trailing_assistant_turns() {
        let line =
            |t: &str, sr: &str| format!(r#"{{"type":"{t}","message":{{"stop_reason":"{sr}"}}}}"#);
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
        assert_eq!(
            tail_stop_reasons(&content, 80),
            vec!["end_turn", "refusal", "refusal"]
        );
        // A small tail window keeps only the LAST N lines (here the two trailing refusals).
        assert_eq!(tail_stop_reasons(&content, 2), vec!["refusal", "refusal"]);
    }

    #[test]
    fn read_file_tail_bounds_the_read_and_drops_a_partial_leading_line() {
        let path =
            std::env::temp_dir().join(format!("fleet-tail-{}-{}.txt", std::process::id(), line!()));
        std::fs::write(&path, "aaaa\nbbbb\ncccc\ndddd\n").unwrap();
        // A budget covering the whole 20-byte file starts at offset 0 -> returned verbatim (no partial drop).
        assert_eq!(
            read_file_tail(&path, 10_000).as_deref(),
            Some("aaaa\nbbbb\ncccc\ndddd\n")
        );
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
        assert!(!rearm_on_cooldown(
            Some(10_000),
            10_000 + 4 * 3_600,
            4 * 3_600
        ));
        // A short/zero interval is floored at 5 min: a re-arm 60s ago is still on cooldown.
        assert!(rearm_on_cooldown(Some(10_000), 10_000 + 60, 0));
        assert!(!rearm_on_cooldown(Some(10_000), 10_000 + 301, 0));
    }

    #[test]
    fn hire_signal_cooldown_suppresses_within_the_window_and_lapses_after() {
        // task_794: never signalled → free to post.
        assert!(!hire_signal_on_cooldown(
            None,
            10_000,
            HIRE_SIGNAL_COOLDOWN_SECS
        ));
        // Signalled 1h ago with a 6h cooldown → still suppressed.
        assert!(hire_signal_on_cooldown(
            Some(10_000),
            10_000 + 3_600,
            HIRE_SIGNAL_COOLDOWN_SECS
        ));
        // …free once the full 6h window has elapsed (no floor, unlike the re-arm cooldown).
        assert!(!hire_signal_on_cooldown(
            Some(10_000),
            10_000 + HIRE_SIGNAL_COOLDOWN_SECS,
            HIRE_SIGNAL_COOLDOWN_SECS
        ));
        // The window is exactly `cooldown_secs`: one second short is still suppressed.
        assert!(hire_signal_on_cooldown(
            Some(10_000),
            10_000 + HIRE_SIGNAL_COOLDOWN_SECS - 1,
            HIRE_SIGNAL_COOLDOWN_SECS
        ));
    }

    #[test]
    fn inbox_pending_count_counts_files_not_the_processed_dir() {
        let (base, fleet) = tmp_hub();
        fleet.ensure_inbox("a1"); // creates inbox/a1/processed
        assert_eq!(
            inbox_pending_count(&fleet, "a1"),
            0,
            "empty inbox (processed dir excluded)"
        );
        std::fs::write(fleet.inbox("a1").join("0001-msg.json"), "{}").unwrap();
        std::fs::write(fleet.inbox("a1").join("0002-msg.json"), "{}").unwrap();
        assert_eq!(
            inbox_pending_count(&fleet, "a1"),
            2,
            "two undrained messages; processed/ not counted"
        );
        // A non-message kickoff SEED file (not `.json`) must NOT count — else a lingering seed reads as a
        // perpetual pending message and the watchdog false-nudges the agent every sweep (v-s2n-quic/v-etude).
        std::fs::write(fleet.inbox("a1").join("s2n_seed.txt"), "seed").unwrap();
        std::fs::write(fleet.inbox("a1").join("seed-a1.md"), "seed").unwrap();
        assert_eq!(
            inbox_pending_count(&fleet, "a1"),
            2,
            "seed files excluded; only .json messages count"
        );
        assert_eq!(inbox_pending_count(&fleet, "nobody"), 0, "absent inbox → 0");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn parse_repo_spec_splits_branch_and_defaults_to_main() {
        assert_eq!(
            parse_repo_spec("camshaft/bolero@master"),
            serde_json::json!({"repo":"camshaft/bolero","branch":"master"})
        );
        assert_eq!(
            parse_repo_spec("camshaft/backbeat"),
            serde_json::json!({"repo":"camshaft/backbeat","branch":"main"})
        );
        assert_eq!(
            parse_repo_spec("camshaft/x@"),
            serde_json::json!({"repo":"camshaft/x","branch":"main"}),
            "empty branch → main"
        );
    }

    #[test]
    fn normalize_repos_accepts_structured_bare_string_and_csv_shapes() {
        // Canonical structured form (what `fleet set-meta --repo` writes) → kept as-is.
        let structured = serde_json::json!([{"repo":"camshaft/fleet"},{"repo":"camshaft/cadenza","branch":"main"}]);
        assert_eq!(
            normalize_repos(Some(&structured)),
            vec![
                serde_json::json!({"repo":"camshaft/fleet"}),
                serde_json::json!({"repo":"camshaft/cadenza","branch":"main"}),
            ]
        );
        // A CSV STRING (the hand-mint shape, #472) → split + trimmed into {repo} entries, NOT silently dropped.
        let csv = serde_json::json!("Membrain, MembrainCDK, ElasticShuffleCDK");
        assert_eq!(
            normalize_repos(Some(&csv)),
            vec![
                serde_json::json!({"repo":"Membrain"}),
                serde_json::json!({"repo":"MembrainCDK"}),
                serde_json::json!({"repo":"ElasticShuffleCDK"}),
            ]
        );
        // A bare-string array → each wrapped as {repo}.
        let bare = serde_json::json!(["Membrain", "MembrainCDK"]);
        assert_eq!(
            normalize_repos(Some(&bare)),
            vec![
                serde_json::json!({"repo":"Membrain"}),
                serde_json::json!({"repo":"MembrainCDK"}),
            ]
        );
        // Absent / empty / other shapes → no entries (the workspace_kind path handles off-tree agents).
        assert!(normalize_repos(None).is_empty());
        assert!(
            normalize_repos(Some(&serde_json::json!(""))).is_empty(),
            "empty string → no entries"
        );
        assert!(
            normalize_repos(Some(&serde_json::json!("  ,  , "))).is_empty(),
            "blank CSV parts filtered out"
        );
        assert!(normalize_repos(Some(&serde_json::json!(42))).is_empty());
    }

    #[test]
    fn build_meta_patch_includes_only_requested_keys_and_errors_when_empty() {
        let p = build_meta_patch(&["o/r@b".to_string()], Some("2m"), None, None, None).unwrap();
        assert_eq!(p["repos"], serde_json::json!([{"repo":"o/r","branch":"b"}]));
        assert_eq!(p["interval"], "2m");
        // repos only — no interval/host/native/devshell key
        let p = build_meta_patch(&["o/r".to_string()], None, None, None, None).unwrap();
        assert!(
            p.get("interval").is_none()
                && p.get("host").is_none()
                && p.get("native").is_none()
                && p.get("devshell").is_none()
        );
        assert!(p.get("repos").is_some());
        // interval only — no repos key
        let p = build_meta_patch(&[], Some("30m"), None, None, None).unwrap();
        assert!(p.get("repos").is_none());
        assert_eq!(p["interval"], "30m");
        // host set, and "" clears the pin (JSON null)
        let p = build_meta_patch(&[], None, Some("host-b"), None, None).unwrap();
        assert_eq!(p["host"], "host-b");
        let p = build_meta_patch(&[], None, Some(""), None, None).unwrap();
        assert_eq!(
            p["host"],
            serde_json::Value::Null,
            "empty host clears the pin"
        );
        // native: tri-state — Some(true)/Some(false) emit the bool; None omits the key entirely
        let p = build_meta_patch(&[], None, None, Some(true), None).unwrap();
        assert_eq!(
            p["native"],
            serde_json::Value::Bool(true),
            "--native true → the board-native marker"
        );
        let p = build_meta_patch(&[], None, None, Some(false), None).unwrap();
        assert_eq!(
            p["native"],
            serde_json::Value::Bool(false),
            "--native false → clear back to file-hub"
        );
        assert!(
            build_meta_patch(&["o/r".to_string()], None, None, None, None)
                .unwrap()
                .get("native")
                .is_none(),
            "native untouched when not requested"
        );
        // devshell: tri-state, same shape (#214 opt-in launch-in-nix-develop marker)
        let p = build_meta_patch(&[], None, None, None, Some(true)).unwrap();
        assert_eq!(
            p["devshell"],
            serde_json::Value::Bool(true),
            "--devshell true → launch inside the flake devShell"
        );
        assert!(
            build_meta_patch(&["o/r".to_string()], None, None, None, None)
                .unwrap()
                .get("devshell")
                .is_none(),
            "devshell untouched when not requested"
        );
        // nothing requested → error (guards a no-op PATCH)
        assert!(build_meta_patch(&[], None, None, None, None).is_err());
    }

    #[test]
    fn agent_host_matches_honors_pin_and_treats_unset_as_run_anywhere() {
        // unpinned (no host / null / empty) → managed everywhere
        assert!(agent_host_matches(Some(&serde_json::json!({})), "host-a"));
        assert!(agent_host_matches(
            Some(&serde_json::json!({"host":null})),
            "host-a"
        ));
        assert!(agent_host_matches(
            Some(&serde_json::json!({"host":""})),
            "host-a"
        ));
        assert!(agent_host_matches(None, "host-a"));
        // string pin: matches only its host
        assert!(agent_host_matches(
            Some(&serde_json::json!({"host":"host-b"})),
            "host-b"
        ));
        assert!(!agent_host_matches(
            Some(&serde_json::json!({"host":"host-b"})),
            "host-a"
        ));
        // array pin: matches if listed; empty array = unpinned
        assert!(agent_host_matches(
            Some(&serde_json::json!({"host":["host-b","host-a"]})),
            "host-a"
        ));
        assert!(!agent_host_matches(
            Some(&serde_json::json!({"host":["host-b"]})),
            "host-a"
        ));
        assert!(agent_host_matches(
            Some(&serde_json::json!({"host":[]})),
            "host-a"
        ));
    }

    #[test]
    fn version_line_reports_package_version_and_a_baked_rev() {
        let v = version_line();
        assert!(
            v.starts_with(&format!("fleet {} (rev ", env!("CARGO_PKG_VERSION"))),
            "names the pkg version"
        );
        assert!(v.ends_with(")"), "wraps the rev");
        // build.rs always bakes a non-empty rev (a real short-sha, or the "unknown" fallback).
        assert!(
            !env!("FLEET_BUILD_REV").is_empty(),
            "the build rev is always baked"
        );
    }

    #[test]
    fn build_freshness_warning_flags_only_a_binary_behind_its_checkout() {
        assert!(
            build_freshness_warning("abc123", Some("def456")).is_some(),
            "baked != head → stale"
        );
        assert!(
            build_freshness_warning("abc123", Some("abc123")).is_none(),
            "baked == head → current"
        );
        assert!(
            build_freshness_warning("abc123-dirty", Some("abc123")).is_none(),
            "a dirty build of the same commit is not stale"
        );
        assert!(
            build_freshness_warning("unknown", Some("abc123")).is_none(),
            "unknown baked rev → can't tell"
        );
        assert!(
            build_freshness_warning("", Some("abc123")).is_none(),
            "empty baked rev → can't tell"
        );
        assert!(
            build_freshness_warning("abc123", None).is_none(),
            "no checkout (deployed binary) → not applicable"
        );
    }

    #[test]
    fn watchdog_exec_args_builds_the_liveness_base_plus_opt_ins() {
        // rearm base, opt-in observe + pinned.
        assert_eq!(
            watchdog_exec_args(true, false, false, false, false, false),
            "watchdog --rearm --stale-only"
        );
        assert_eq!(
            watchdog_exec_args(true, true, false, false, false, false),
            "watchdog --rearm --stale-only --observe --spawn"
        );
        assert_eq!(
            watchdog_exec_args(true, false, true, false, false, false),
            "watchdog --rearm --stale-only --pinned-only"
        );
        // The secondary-box go-live shape: liveness + observer cadence + host filter.
        assert_eq!(
            watchdog_exec_args(true, true, true, false, false, false),
            "watchdog --rearm --stale-only --observe --spawn --pinned-only"
        );
        // OBSERVER-ONLY (rearm=false): coexists with an existing rearm watchdog without double-rearming (host-a).
        assert_eq!(
            watchdog_exec_args(false, true, false, false, false, false),
            "watchdog --observe --spawn"
        );
        // --self-redeploy (#388) appends last: a local-checkout host installs the self-healing watchdog.
        assert_eq!(
            watchdog_exec_args(true, false, false, true, false, false),
            "watchdog --rearm --stale-only --self-redeploy"
        );
        // --hire-signal (task_794) appends after self-redeploy: a host turns on the backlog hire-signal post.
        assert_eq!(
            watchdog_exec_args(true, false, false, false, true, false),
            "watchdog --rearm --stale-only --hire-signal"
        );
        assert_eq!(
            watchdog_exec_args(true, false, false, true, true, false),
            "watchdog --rearm --stale-only --self-redeploy --hire-signal"
        );
        // --reap-stale-observers (task_1045) appends last: this is the passthrough a flake-generated unit needs
        // so the observer-reaper reaches the deploy (the hardcoded fleet-daemons.nix ExecStart is a separate path).
        assert_eq!(
            watchdog_exec_args(true, false, false, false, false, true),
            "watchdog --rearm --stale-only --reap-stale-observers"
        );
        // The full installed-cadence shape AGENTS.md documents: liveness + observer cadence + reaper.
        assert_eq!(
            watchdog_exec_args(true, true, false, false, false, true),
            "watchdog --rearm --stale-only --observe --spawn --reap-stale-observers"
        );
    }

    #[test]
    fn classify_wake_path_prefers_webhook_then_tunnel_then_poll_only() {
        // A non-empty webhook is a direct POST target — it wins even if a tunnel also covers the agent.
        assert_eq!(
            classify_wake_path(Some("http://127.0.0.1:8899/wake"), true),
            WakePath::Webhook
        );
        assert_eq!(
            classify_wake_path(Some("http://127.0.0.1:8899/wake"), false),
            WakePath::Webhook
        );
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
    fn classify_observe_coverage_flags_the_host_local_blind_spot() {
        use serde_json::json;
        let dev = ObserveCadence {
            host: "host-a".to_string(),
            pinned_only: false,
        };
        let green = ObserveCadence {
            host: "host-b".to_string(),
            pinned_only: true,
        };

        // task_711: an agent pinned to host-b, with ONLY host-a observing, is covered by nobody → BLIND.
        let green_pinned = json!({"host": "host-b"});
        assert_eq!(
            classify_observe_coverage(
                Some(&green_pinned),
                std::slice::from_ref(&dev),
                "host-a",
                None
            ),
            ObserveCoverage::Blind
        );
        // Add the host-b pinned-only cadence and the SAME agent becomes covered (assumed — host-b is not the
        // local host, so transcript presence is not probed here).
        assert_eq!(
            classify_observe_coverage(
                Some(&green_pinned),
                &[dev.clone(), green.clone()],
                "host-a",
                None
            ),
            ObserveCoverage::CoveredAssumed
        );

        // An agent managed by the LOCAL cadence: transcript present → verified; absent → soft gap.
        let dev_pinned = json!({"host": "host-a"});
        assert_eq!(
            classify_observe_coverage(
                Some(&dev_pinned),
                std::slice::from_ref(&dev),
                "host-a",
                Some(true)
            ),
            ObserveCoverage::CoveredLocalTranscript
        );
        assert_eq!(
            classify_observe_coverage(
                Some(&dev_pinned),
                std::slice::from_ref(&dev),
                "host-a",
                Some(false)
            ),
            ObserveCoverage::ManagedNoLocalTranscript
        );

        // An UNPINNED agent is covered by the non-pinned local cadence (transcript present here).
        let unpinned = json!({});
        assert_eq!(
            classify_observe_coverage(
                Some(&unpinned),
                std::slice::from_ref(&dev),
                "host-a",
                Some(true)
            ),
            ObserveCoverage::CoveredLocalTranscript
        );
        // ...but if EVERY declared cadence is pinned-only, an unpinned agent is served by none → BLIND.
        assert_eq!(
            classify_observe_coverage(
                Some(&unpinned),
                std::slice::from_ref(&green),
                "host-a",
                None
            ),
            ObserveCoverage::Blind
        );

        // A STAGED agent is managed by no cadence (watchdog_manages_agent returns false) → BLIND here, though
        // the handler excludes it upstream via agent_expected_running, so it never reaches this classifier.
        let staged = json!({"host": "host-a", "staged": true});
        assert_eq!(
            classify_observe_coverage(Some(&staged), std::slice::from_ref(&dev), "host-a", None),
            ObserveCoverage::Blind
        );
    }

    #[test]
    fn host_pin_display_renders_unset_single_and_list() {
        use serde_json::json;
        assert_eq!(host_pin_display(None), "unpinned");
        assert_eq!(host_pin_display(Some(&json!({}))), "unpinned");
        assert_eq!(host_pin_display(Some(&json!({"host": null}))), "unpinned");
        assert_eq!(host_pin_display(Some(&json!({"host": ""}))), "unpinned");
        assert_eq!(host_pin_display(Some(&json!({"host": "  "}))), "unpinned");
        assert_eq!(host_pin_display(Some(&json!({"host": []}))), "unpinned");
        assert_eq!(host_pin_display(Some(&json!({"host": "host-b"}))), "host-b");
        assert_eq!(
            host_pin_display(Some(&json!({"host": ["host-b", "host-a"]}))),
            "[host-b, host-a]"
        );
    }

    #[test]
    fn classify_reclaim_never_calls_a_no_upstream_checkout_safe() {
        // Dirty wins outright — unsafe regardless of upstream / merge state.
        assert_eq!(classify_reclaim(true, true, 0, true), ReclaimState::Dirty);
        assert_eq!(classify_reclaim(true, false, 0, false), ReclaimState::Dirty);
        assert!(!ReclaimState::Dirty.reclaimable());

        // Clean + has upstream + not ahead → fully pushed → reclaimable.
        let s = classify_reclaim(false, true, 0, false);
        assert_eq!(s, ReclaimState::Clean);
        assert!(s.reclaimable());

        // Clean + has upstream + AHEAD + not merged → unpushed commits → unsafe.
        let s = classify_reclaim(false, true, 3, false);
        assert_eq!(s, ReclaimState::Unpushed);
        assert!(!s.reclaimable());

        // THE FOOTGUN (task_714): no upstream + not merged → unprovable → UNSAFE, never reclaimable.
        let s = classify_reclaim(false, false, 0, false);
        assert_eq!(s, ReclaimState::NoUpstreamUnmerged);
        assert!(!s.reclaimable());

        // No upstream BUT already an ancestor of the mainline → commits preserved there → reclaimable.
        let s = classify_reclaim(false, false, 0, true);
        assert_eq!(s, ReclaimState::MergedIntoMainline);
        assert!(s.reclaimable());

        // Ahead of a STALE upstream but already merged into the mainline → preserved there → reclaimable
        // (the proof is mainline-ancestry, not the upstream).
        let s = classify_reclaim(false, true, 5, true);
        assert_eq!(s, ReclaimState::MergedIntoMainline);
        assert!(s.reclaimable());
    }

    #[test]
    fn reclaim_safety_override_keeps_live_primary_and_agent_checkouts() {
        // task_774: a disposable linked worktree (not live / not primary / not agent) → no override, truly reclaimable.
        assert_eq!(reclaim_safety_override(false, false, false), None);
        // LIVE wins (highest priority): a running process is cwd'd here.
        assert_eq!(
            reclaim_safety_override(true, false, false),
            Some("LIVE (a running process is cwd'd at/under it)")
        );
        // A primary clone (git-clean + pushed, but the main checkout) is kept.
        assert_eq!(
            reclaim_safety_override(false, true, false),
            Some("PRIMARY-ROOT (main checkout, not a disposable linked worktree)")
        );
        // An off-tree-agent worktree is kept even when idle (no live cwd) and a linked (non-primary) worktree.
        assert_eq!(
            reclaim_safety_override(false, false, true),
            Some("AGENT-WORKTREE (.v-<agent> off-tree agent)")
        );
        // Live takes precedence over the other reasons when several hold.
        assert_eq!(
            reclaim_safety_override(true, true, true),
            Some("LIVE (a running process is cwd'd at/under it)")
        );
    }

    #[test]
    fn path_is_live_matches_the_checkout_and_nested_cwds_only() {
        let wt = PathBuf::from("/home/u/.fleet/agents/a1/repo");
        // A process cwd'd exactly at the worktree → live.
        assert!(path_is_live(
            &wt,
            &[PathBuf::from("/home/u/.fleet/agents/a1/repo")]
        ));
        // A process cwd'd in a SUBDIR of the worktree → live.
        assert!(path_is_live(
            &wt,
            &[PathBuf::from("/home/u/.fleet/agents/a1/repo/crates/fleet")]
        ));
        // A process elsewhere → not live.
        assert!(!path_is_live(&wt, &[PathBuf::from("/home/u/other")]));
        // A PARENT of the worktree is NOT a match (prefix must be the worktree, not the other way round).
        assert!(!path_is_live(
            &wt,
            &[PathBuf::from("/home/u/.fleet/agents/a1")]
        ));
        // A sibling that shares a path PREFIX string but not a path component → not live.
        assert!(!path_is_live(
            &wt,
            &[PathBuf::from("/home/u/.fleet/agents/a1/repo-two")]
        ));
        // No live cwds → not live.
        assert!(!path_is_live(&wt, &[]));
    }

    #[test]
    fn is_agent_suffix_worktree_matches_the_dot_v_infix() {
        assert!(is_agent_suffix_worktree(Path::new(
            "/wt/backbeat.v-backbeat"
        )));
        assert!(is_agent_suffix_worktree(Path::new("/wt/bolero.v-bolero")));
        // A plain repo or a linked topic worktree is not an off-tree-agent dir.
        assert!(!is_agent_suffix_worktree(Path::new("/wt/cadenza")));
        assert!(!is_agent_suffix_worktree(Path::new("/wt/topic-a")));
        // "v-" without the leading dot (a board agent id, not the off-tree dir pattern) does not match.
        assert!(!is_agent_suffix_worktree(Path::new("/wt/v-fleet-tooling")));
    }

    #[test]
    fn parse_worktree_paths_collects_paths_and_skips_the_bare_entry() {
        // A bare hub's porcelain: the bare repo itself (no working tree) + two linked worktrees.
        let porcelain = "\
worktree /hub/cadenza.git
bare

worktree /hub/cadenza/.claude/worktrees/topic-a
HEAD abc123
branch refs/heads/topic-a

worktree /hub/cadenza/.claude/worktrees/topic-b
HEAD def456
detached
";
        let paths = parse_worktree_paths(porcelain);
        assert_eq!(
            paths,
            vec![
                PathBuf::from("/hub/cadenza/.claude/worktrees/topic-a"),
                PathBuf::from("/hub/cadenza/.claude/worktrees/topic-b"),
            ]
        );
        // A normal (non-bare) repo: its own worktree is kept.
        let paths =
            parse_worktree_paths("worktree /home/u/repo\nHEAD aaa\nbranch refs/heads/main\n");
        assert_eq!(paths, vec![PathBuf::from("/home/u/repo")]);
        // Empty / no worktree lines → empty.
        assert!(parse_worktree_paths("").is_empty());
    }

    #[test]
    fn mainline_candidates_puts_primary_first_then_brazil_and_plain_git_fallbacks_deduped() {
        // Default primary (github): origin/main first, then brazil + plain-git fallbacks, no dup of origin/main.
        assert_eq!(
            mainline_candidates("origin/main"),
            vec!["origin/main", "mainline", "origin/mainline", "main"]
        );
        // A brazil-primary: "mainline" leads, the rest follow deduped (mainline not repeated).
        assert_eq!(
            mainline_candidates("mainline"),
            vec!["mainline", "origin/mainline", "main", "origin/main"]
        );
        // An arbitrary primary is kept first and the full fallback set follows.
        assert_eq!(
            mainline_candidates("origin/release"),
            vec![
                "origin/release",
                "mainline",
                "origin/mainline",
                "main",
                "origin/main"
            ]
        );
    }

    #[test]
    fn execstart_is_nix_store_detects_a_nix_managed_unit() {
        // systemctl show renders the resolved exec; a /nix/store path = nix-managed (the host-a after task_717).
        assert!(execstart_is_nix_store(
            "ExecStart={ path=/nix/store/abc123-fleet-0.0.0/bin/fleet ; argv[]=/nix/store/abc123-fleet-0.0.0/bin/fleet watchdog ; ignore_errors=no }"
        ));
        // A local-checkout unit points at the cargo target/release (or a staged run path) binary — NOT nix.
        assert!(!execstart_is_nix_store(
            "ExecStart={ path=/home/u/Projects/camshaft/fleet/target/release/fleet ; argv[]=... }"
        ));
        assert!(!execstart_is_nix_store(
            "ExecStart={ path=/run/fleet/bin/fleet ; argv[]=... }"
        ));
        // Empty / absent ExecStart carries no nix signal.
        assert!(!execstart_is_nix_store("ExecStart="));
        assert!(!execstart_is_nix_store(""));
    }

    #[test]
    fn gate_steps_match_cis_clippy_test_workspace_commands() {
        // The clippy step MUST be byte-identical to CI (.github/workflows/checks.yml): a drift here is exactly
        // the task_720 trap (a local gate that does not match CI).
        let full = gate_steps(false);
        assert_eq!(full.len(), 2);
        assert_eq!(
            full[0].1,
            vec![
                "clippy",
                "--workspace",
                "--all-targets",
                "--",
                "-D",
                "warnings"
            ]
        );
        assert_eq!(full[1].1, vec!["test", "--workspace"]);
        // --clippy-only drops the test step but keeps the identical clippy command.
        let clippy_only = gate_steps(true);
        assert_eq!(clippy_only.len(), 1);
        assert_eq!(
            clippy_only[0].1,
            vec![
                "clippy",
                "--workspace",
                "--all-targets",
                "--",
                "-D",
                "warnings"
            ]
        );
    }

    #[test]
    fn workspace_members_parses_sorts_and_dedups_cargo_metadata() {
        // The shape of `cargo metadata --no-deps --format-version 1` — packages[] is the workspace members.
        let json = r#"{"packages":[{"name":"fleet"},{"name":"bridge-core"},{"name":"kb"}],"workspace_members":[]}"#;
        assert_eq!(workspace_members(json), vec!["bridge-core", "fleet", "kb"]);
        // Unparseable / missing packages -> empty (the handler treats empty as a hard error, never a silent pass).
        assert!(workspace_members("not json").is_empty());
        assert!(workspace_members(r#"{"other":1}"#).is_empty());
    }

    #[test]
    fn monitor_tick_wakes_only_skips_the_model_on_green() {
        // GREEN is the ONLY verdict that skips the model turn.
        assert!(!monitor_tick_wakes(&SeamVerdict::Green {
            incoming: 0,
            seams: 2,
            head: "abc123".into()
        }));
        // Everything else wakes — including the fail-safe no-seam / error cases (never silently skip a tick we
        // could not gate).
        assert!(monitor_tick_wakes(&SeamVerdict::Changed(vec![
            "crates/x/seam.rs".into()
        ])));
        assert!(monitor_tick_wakes(&SeamVerdict::NoSeam));
        assert!(monitor_tick_wakes(&SeamVerdict::Error(
            "git fetch failed".into()
        )));
    }

    #[test]
    fn classify_worktree_health_flags_a_stale_pointer_and_unlanded_commits() {
        // No metadata.worktree -> nothing to check (resolvable/path/ahead irrelevant).
        assert_eq!(
            classify_worktree_health(false, false, false, 0),
            WorktreeHealth::Unset
        );
        assert_eq!(
            classify_worktree_health(false, true, true, 5),
            WorktreeHealth::Unset
        );
        // Relative path with no hub to resolve -> Indeterminate, NEVER a false stale (the false-positive guard).
        assert_eq!(
            classify_worktree_health(true, false, false, 0),
            WorktreeHealth::Indeterminate
        );
        // task_730: set + resolvable but the resolved path is gone -> STALE, regardless of commit count.
        assert_eq!(
            classify_worktree_health(true, true, false, 0),
            WorktreeHealth::MissingPath
        );
        assert_eq!(
            classify_worktree_health(true, true, false, 9),
            WorktreeHealth::MissingPath
        );
        // Exists + commits ahead of mainline -> at-risk unlanded work.
        assert_eq!(
            classify_worktree_health(true, true, true, 2),
            WorktreeHealth::UnlandedCommits
        );
        // Exists + level/behind -> healthy.
        assert_eq!(
            classify_worktree_health(true, true, true, 0),
            WorktreeHealth::Clean
        );
    }

    #[test]
    fn worktree_sync_only_fixes_a_single_repo_existing_differing_tree() {
        use WorktreeSyncVerdict::*;
        let real = "/f/agents/a/cadenza";
        // No worktree set -> sync never ADDS one (only corrects an existing pointer).
        assert_eq!(
            classify_worktree_sync(1, Some(real), true, None),
            NoWorktree
        );
        assert_eq!(
            classify_worktree_sync(1, Some(real), true, Some("   ")),
            NoWorktree
        );
        // Multi-repo or repo-less -> AMBIGUOUS primary, never guessed.
        assert_eq!(
            classify_worktree_sync(2, None, false, Some(".claude/worktrees/a")),
            AmbiguousRepos
        );
        assert_eq!(
            classify_worktree_sync(0, None, false, Some(".claude/worktrees/a")),
            AmbiguousRepos
        );
        // Single repo but the real fleet tree is missing -> leave the pointer untouched (never point at nothing).
        assert_eq!(
            classify_worktree_sync(1, Some(real), false, Some(".claude/worktrees/a")),
            NoRealTree
        );
        // Already pointing at the real tree -> no-op (idempotent).
        assert_eq!(
            classify_worktree_sync(1, Some(real), true, Some(real)),
            AlreadyCorrect
        );
        // Stale pointer + real tree exists -> fixable, carrying the correction target.
        assert_eq!(
            classify_worktree_sync(1, Some(real), true, Some(".claude/worktrees/a")),
            Fixable(real.to_string())
        );
    }

    #[test]
    fn classify_commits_carry_flags_ahead_or_dirty_but_not_a_clean_old_path() {
        use CommitsCarryVerdict::*;
        // No upstream (ahead=0 by convention) and clean -> nothing at risk.
        assert_eq!(classify_commits_carry(0, false), Clean);
        // Unpushed commits alone are at-risk.
        assert_eq!(
            classify_commits_carry(3, false),
            AtRisk {
                ahead: 3,
                dirty: false
            }
        );
        // An unclean working tree alone is at-risk, even with nothing committed-ahead.
        assert_eq!(
            classify_commits_carry(0, true),
            AtRisk {
                ahead: 0,
                dirty: true
            }
        );
        // Both at once is still just one AtRisk verdict carrying both facts.
        assert_eq!(
            classify_commits_carry(2, true),
            AtRisk {
                ahead: 2,
                dirty: true
            }
        );
    }

    #[test]
    fn classify_intake_task_flags_dwell_and_bad_state_independently() {
        // A fresh `todo` under the SLA is clean — the intake inbox is doing its job, no alert.
        let ok = classify_intake_task("todo", 30, 120);
        assert!(!ok.is_violation());
        assert_eq!(ok, IntakeVerdict::default());

        // A `todo` dwelling PAST the SLA trips invariant 1 (age) only, not the state invariant.
        let dwelt = classify_intake_task("todo", 300, 120);
        assert_eq!(dwelt.dwell_over_secs, Some(300));
        assert_eq!(dwelt.bad_state, None);
        assert!(dwelt.is_violation());

        // `in_progress` while still in the inbox trips invariant 2 REGARDLESS of age — even brand new.
        let working = classify_intake_task("in_progress", 5, 120);
        assert_eq!(working.bad_state.as_deref(), Some("in_progress"));
        assert_eq!(working.dwell_over_secs, None);
        assert!(working.is_violation());

        // `blocked` in the inbox is likewise an invariant-2 violation on its own.
        assert_eq!(
            classify_intake_task("blocked", 10, 120)
                .bad_state
                .as_deref(),
            Some("blocked")
        );

        // Both at once: a long-dwelling in_progress task carries BOTH facts on one verdict (one alert).
        let both = classify_intake_task("in_progress", 999, 120);
        assert_eq!(both.bad_state.as_deref(), Some("in_progress"));
        assert_eq!(both.dwell_over_secs, Some(999));

        // A TERMINAL task is never a violation — no longer awaiting routing, even if very old.
        assert!(!classify_intake_task("done", 100_000, 120).is_violation());
        assert!(!classify_intake_task("cancelled", 100_000, 120).is_violation());
        assert!(!classify_intake_task("archived", 100_000, 120).is_violation());
    }

    #[test]
    fn intake_report_lists_only_offenders_and_names_why() {
        let entries = vec![
            // Clean: fresh todo under SLA — no line.
            ("task_1".to_string(), "todo".to_string(), 30),
            // Dwell only.
            ("task_2".to_string(), "todo".to_string(), 300),
            // Bad state only (fresh in_progress).
            ("task_3".to_string(), "in_progress".to_string(), 5),
            // Both at once — one line carrying both reasons.
            ("task_4".to_string(), "blocked".to_string(), 999),
            // Terminal and old — never reported.
            ("task_5".to_string(), "done".to_string(), 100_000),
        ];
        let lines = intake_report(&entries, 120);
        assert_eq!(lines.len(), 3, "only the three live offenders are reported");
        assert!(lines.iter().all(|l| !l.starts_with("task_1:")));
        assert!(lines.iter().all(|l| !l.starts_with("task_5:")));
        // Dwell-only names the dwell, not a state.
        let two = lines.iter().find(|l| l.starts_with("task_2:")).unwrap();
        assert!(
            two.contains("dwell 300s"),
            "names the over-SLA dwell: {two}"
        );
        assert!(!two.contains("state="), "a todo has no bad state: {two}");
        // Bad-state-only names the state, not a dwell.
        let three = lines.iter().find(|l| l.starts_with("task_3:")).unwrap();
        assert!(
            three.contains("state=in_progress"),
            "names the bad state: {three}"
        );
        assert!(
            !three.contains("dwell"),
            "a fresh task has no dwell breach: {three}"
        );
        // Both-at-once carries BOTH reasons on one line.
        let four = lines.iter().find(|l| l.starts_with("task_4:")).unwrap();
        assert!(four.contains("state=blocked"), "names the state: {four}");
        assert!(four.contains("dwell 999s"), "names the dwell: {four}");
    }

    #[test]
    fn resolve_recipient_forces_exact_id_and_disambiguates_a_prefix_collision() {
        let known = vec![
            "v-fleet-tooling".to_string(),
            "v-fleet-tooling-helper".to_string(),
            "v-nix".to_string(),
        ];
        // An exact id wins even though it is also a strict prefix of the helper — a real recipient is never
        // refused by the guard.
        assert_eq!(
            resolve_recipient("v-fleet-tooling", &known),
            RecipientResolution::Exact
        );
        assert_eq!(
            resolve_recipient("v-nix", &known),
            RecipientResolution::Exact
        );
        // A non-exact name that is a strict prefix of known ids is ambiguous — refuse + list the candidates,
        // sorted, never auto-pick.
        assert_eq!(
            resolve_recipient("v-fleet", &known),
            RecipientResolution::Ambiguous(vec![
                "v-fleet-tooling".to_string(),
                "v-fleet-tooling-helper".to_string(),
            ])
        );
        // A wholly unknown name is first-contact — delivered unchanged, so the guard never blocks a new agent.
        assert_eq!(
            resolve_recipient("v-brand-new", &known),
            RecipientResolution::Unknown
        );
    }

    #[test]
    fn intake_violation_reason_names_state_dwell_or_both_and_is_none_when_clean() {
        // Clean and terminal both yield no reason.
        assert_eq!(intake_violation_reason("todo", 30, 120), None);
        assert_eq!(intake_violation_reason("done", 10_000, 120), None);
        // Dwell-only, state-only, and both.
        assert_eq!(
            intake_violation_reason("todo", 300, 120).as_deref(),
            Some("dwell 300s > 120s SLA")
        );
        assert_eq!(
            intake_violation_reason("in_progress", 5, 120).as_deref(),
            Some("state=in_progress (never in-inbox)")
        );
        let both = intake_violation_reason("blocked", 999, 120).unwrap();
        assert!(
            both.contains("state=blocked") && both.contains("dwell 999s"),
            "carries both reasons: {both}"
        );
    }

    #[test]
    fn intake_alert_comment_tags_board_triage_and_names_the_reason() {
        let c = intake_alert_comment("dwell 300s > 120s SLA");
        assert!(c.contains("board-triage"), "escalates to board-triage: {c}");
        assert!(
            c.contains("dwell 300s > 120s SLA"),
            "names the violation: {c}"
        );
        assert!(
            c.to_lowercase().contains("rout"),
            "asks to route it out of the inbox: {c}"
        );
    }

    #[test]
    fn mandate_set_paths_orders_core_role_targets_capabilities() {
        let paths = mandate_set_paths(
            Some("vertical"),
            &["camshaft/fleet".to_string(), "camshaft/cadenza".to_string()],
            &["gate".to_string(), "deploy".to_string()],
        );
        assert_eq!(
            paths,
            vec![
                "core".to_string(),
                "roles/vertical".to_string(),
                "targets/camshaft/cadenza".to_string(), // repos sorted by name
                "targets/camshaft/fleet".to_string(),
                "capabilities/deploy".to_string(), // capabilities sorted
                "capabilities/gate".to_string(),
            ]
        );
        // No role / repos / caps -> just the universal core.
        assert_eq!(mandate_set_paths(None, &[], &[]), vec!["core".to_string()]);
    }

    #[test]
    fn mandate_set_paths_from_metadata_reads_role_repos_capabilities() {
        let md = serde_json::json!({
            "role": "vertical",
            "repos": [{"repo":"camshaft/fleet","branch":"main"},{"repo":"camshaft/cadenza","branch":"main"}],
            "capabilities": ["deploy","gate"]
        });
        assert_eq!(
            mandate_set_paths_from_metadata(&md),
            [
                "core",
                "roles/vertical",
                "targets/camshaft/cadenza",
                "targets/camshaft/fleet",
                "capabilities/deploy",
                "capabilities/gate"
            ]
            .into_iter()
            .map(String::from)
            .collect::<Vec<_>>()
        );
    }

    #[test]
    fn compose_mandate_requires_core_and_fail_soft_skips_unapproved_sets() {
        // No approved core -> Err (hold + alarm), never boot an agent with no universal contract.
        let no_core = compose_mandate_artifact(&[
            ("core".to_string(), None),
            ("roles/vertical".to_string(), Some("R".to_string())),
        ]);
        assert!(no_core.is_err());
        // Core present; an unapproved target is skipped, approved sets concatenated under headers in order.
        let out = compose_mandate_artifact(&[
            ("core".to_string(), Some("CORE BODY".to_string())),
            ("roles/vertical".to_string(), Some("ROLE BODY".to_string())),
            ("targets/camshaft/fleet".to_string(), None),
        ])
        .unwrap();
        assert_eq!(out.skipped, vec!["targets/camshaft/fleet".to_string()]);
        assert!(out.artifact.contains("## mandate-set: core"));
        assert!(out.artifact.contains("CORE BODY"));
        assert!(out.artifact.contains("## mandate-set: roles/vertical"));
        assert!(out.artifact.contains("ROLE BODY"));
        assert!(
            !out.artifact.contains("targets/camshaft/fleet"),
            "a skipped set is absent from the artifact"
        );
        // Order preserved: core header precedes the role header.
        assert!(out.artifact.find("core").unwrap() < out.artifact.find("roles/vertical").unwrap());
    }

    #[test]
    fn artifact_changed_is_a_pure_content_gate() {
        assert!(artifact_changed("A", None), "no existing file -> write");
        assert!(
            artifact_changed("A", Some("B")),
            "differing content -> write"
        );
        assert!(
            !artifact_changed("A", Some("A")),
            "identical content -> skip (no boot-file churn)"
        );
    }

    #[test]
    fn board_hygiene_sweeps_builds_both_rows_and_respects_skip() {
        let both = board_hygiene_sweeps(28, 7, 14, false);
        assert_eq!(both.len(), 2, "archive-done + age-out-todos");
        assert_eq!(both[0].name, "archive-done");
        assert_eq!(both[0].path, "/projects/28/archive-done");
        assert_eq!(both[0].older_than_days, 7);
        assert_eq!(both[1].name, "age-out-todos");
        assert_eq!(both[1].path, "/projects/28/age-out-todos");
        assert_eq!(both[1].older_than_days, 14);
        // skip_age_out drops the second row, leaving only the confirmed archive-done sweep.
        let only = board_hygiene_sweeps(29, 7, 14, true);
        assert_eq!(only.len(), 1);
        assert_eq!(only[0].name, "archive-done");
        assert_eq!(only[0].path, "/projects/29/archive-done");
    }

    #[test]
    fn hygiene_sweep_body_carries_days_and_principal() {
        let b = hygiene_sweep_body(7, "v-fleet-tooling");
        assert_eq!(
            b.get("older_than_days").and_then(serde_json::Value::as_u64),
            Some(7)
        );
        assert_eq!(
            b.get("principal").and_then(serde_json::Value::as_str),
            Some("v-fleet-tooling")
        );
    }

    #[test]
    fn commits_carry_notice_names_the_path_and_the_specific_risk() {
        let ahead_only = commits_carry_notice("/old/path", 3, false);
        assert!(
            ahead_only.contains("/old/path"),
            "names the old path so the agent can cd there"
        );
        assert!(
            ahead_only.contains("3 commits ahead"),
            "pluralizes multiple commits"
        );
        assert!(
            !ahead_only.contains("unclean working tree"),
            "clean tree is not mentioned when not dirty"
        );

        let singular = commits_carry_notice("/old/path", 1, false);
        assert!(
            singular.contains("1 commit ahead") && !singular.contains("1 commits"),
            "singular commit"
        );

        let dirty_only = commits_carry_notice("/old/path", 0, true);
        assert!(dirty_only.contains("an unclean working tree"));
        assert!(
            !dirty_only.contains("ahead of its upstream"),
            "no ahead-count mentioned when ahead is 0"
        );

        let both = commits_carry_notice("/old/path", 2, true);
        assert!(
            both.contains("2 commits ahead")
                && both.contains("an unclean working tree")
                && both.contains(" and ")
        );
    }

    #[test]
    fn old_worktree_risk_is_none_for_a_path_that_is_not_a_git_worktree() {
        let dir =
            std::env::temp_dir().join(format!("fleet-test-not-a-worktree-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        assert_eq!(old_worktree_risk(dir.to_str().unwrap()), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn old_worktree_risk_reports_ahead_and_dirty_for_a_real_scratch_repo() {
        let dir =
            std::env::temp_dir().join(format!("fleet-test-old-worktree-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let git = |args: &[&str]| {
            let status = std::process::Command::new("git")
                .arg("-C")
                .arg(&dir)
                .args(args)
                .status()
                .unwrap();
            assert!(status.success(), "git {args:?} failed");
        };
        git(&["init", "-q"]);
        git(&["config", "user.email", "test@example.com"]);
        git(&["config", "user.name", "test"]);
        std::fs::write(dir.join("f.txt"), "a\n").unwrap();
        git(&["add", "f.txt"]);
        git(&["commit", "-q", "-m", "init"]);
        // No upstream configured yet: ahead=0 by convention (not an error), clean tree.
        assert_eq!(old_worktree_risk(dir.to_str().unwrap()), Some((0, false)));
        // Dirty the tree (untracked file): still ahead=0, now dirty=true.
        std::fs::write(dir.join("g.txt"), "b\n").unwrap();
        assert_eq!(old_worktree_risk(dir.to_str().unwrap()), Some((0, true)));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ensure_launch_cwd_creates_a_missing_nested_dir_and_is_idempotent() {
        // task_1105: a fresh membrain-kind mint has no launch dir yet; spin-up must create it before the
        // setup_script `cd`s in. create a nested path that does not exist, then confirm a second call no-ops.
        let base =
            std::env::temp_dir().join(format!("fleet-elc-{}-{}", std::process::id(), line!()));
        let nested = base.join("a/b/c");
        let _ = std::fs::remove_dir_all(&base);
        assert!(ensure_launch_cwd(nested.to_str().unwrap()).is_ok());
        assert!(nested.is_dir());
        // Idempotent: a kind whose own script also materializes the dir (or a re-provision) is a clean no-op.
        assert!(ensure_launch_cwd(nested.to_str().unwrap()).is_ok());
        assert!(nested.is_dir());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn ensure_launch_cwd_errors_with_the_path_when_target_is_a_file() {
        // The genuinely-unexpected case (metadata.workspace_cwd points at a file): surface a clear, path-named
        // error rather than leaking a raw errno or silently proceeding.
        let base =
            std::env::temp_dir().join(format!("fleet-elc-file-{}-{}", std::process::id(), line!()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let file = base.join("not-a-dir");
        std::fs::write(&file, b"x").unwrap();
        let err = ensure_launch_cwd(file.to_str().unwrap()).unwrap_err();
        assert!(err.contains("mkdir launch cwd"));
        assert!(err.contains(file.to_str().unwrap()));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn worktree_metadata_patch_is_idempotent_and_syncs_a_stale_pointer() {
        use serde_json::json;
        // Already pointing at the real workdir -> no patch (idempotent re-provision).
        assert_eq!(
            worktree_metadata_patch(
                Some("/home/u/.fleet/agents/a/repo"),
                "/home/u/.fleet/agents/a/repo"
            ),
            None
        );
        // Stale pointer (the task_730 symptom: charter names an old/retired tree) -> patch to the real path.
        assert_eq!(
            worktree_metadata_patch(Some(".claude/worktrees/a"), "/home/u/.fleet/agents/a/repo"),
            Some(json!({ "worktree": "/home/u/.fleet/agents/a/repo" }))
        );
        // Unset -> patch (first provision records the path).
        assert_eq!(
            worktree_metadata_patch(None, "/home/u/.fleet/agents/a/repo"),
            Some(json!({ "worktree": "/home/u/.fleet/agents/a/repo" }))
        );
    }

    #[test]
    fn agent_expected_running_excludes_non_running_statuses_kinds_and_standdown() {
        use serde_json::json;
        // A live loop agent (vertical/named/worker/legacy-None kind) in a running status is in scope.
        assert!(agent_expected_running(
            &json!({"status": "online", "kind": "vertical"})
        ));
        assert!(agent_expected_running(
            &json!({"status": "away", "kind": "named-agent"})
        ));
        assert!(agent_expected_running(&json!({"status": "busy"}))); // kind absent → legacy loop agent
        // Not-running statuses (any case) are excluded — a stood-down/finished/cancelled agent needs no wake.
        assert!(!agent_expected_running(&json!({"status": "offline"})));
        assert!(!agent_expected_running(&json!({"status": "OFFLINE"})));
        assert!(!agent_expected_running(
            &json!({"status": "done", "kind": "worker"})
        ));
        assert!(!agent_expected_running(&json!({"status": "cancelled"})));
        // A pending stand-down request winds the agent down even before status flips.
        assert!(!agent_expected_running(
            &json!({"status": "online", "stand_down_requested_at": "2026-09-30T00:00:00Z"})
        ));
        // A null stand-down field is NOT a stand-down.
        assert!(agent_expected_running(
            &json!({"status": "online", "stand_down_requested_at": null})
        ));
        // Non-loop kinds have no event loop to strand: an interactive assistant session, an ephemeral observer.
        assert!(!agent_expected_running(
            &json!({"status": "online", "kind": "assistant"})
        ));
        assert!(!agent_expected_running(
            &json!({"status": "online", "kind": "observer"})
        ));
        // A STAGED reserve helper (#392) is expected-dormant — wake wired at launch — so not a poll-only gap,
        // mirroring the watchdog's own staged skip (PR #122).
        assert!(!agent_expected_running(
            &json!({"status": "online", "kind": "vertical", "metadata": {"staged": true}})
        ));
        assert!(agent_expected_running(
            &json!({"status": "online", "kind": "vertical", "metadata": {"staged": false}})
        ));
    }

    #[test]
    fn watchdog_stale_self_action_gates_redeploy_on_the_flag() {
        // Fresh binary (baked == HEAD) → nothing, regardless of the flag.
        assert_eq!(
            watchdog_stale_self_action("abc123", Some("abc123"), false),
            StaleSelfAction::Fresh
        );
        assert_eq!(
            watchdog_stale_self_action("abc123", Some("abc123"), true),
            StaleSelfAction::Fresh
        );
        // Can't tell (unknown rev / no checkout) → Fresh, never a spurious redeploy.
        assert_eq!(
            watchdog_stale_self_action("unknown", Some("def456"), true),
            StaleSelfAction::Fresh
        );
        assert_eq!(
            watchdog_stale_self_action("abc123", None, true),
            StaleSelfAction::Fresh
        );
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
        let u = render_watchdog_units(
            "/run/fleet/bin/fleet",
            &watchdog_exec_args(true, true, true, false, false, false),
            60,
            "",
        );
        // A oneshot service (the watchdog is single-sweep) driven by a timer — not a Restart loop.
        assert!(
            u.contains("Type=oneshot"),
            "single-sweep → oneshot, not a loop"
        );
        assert!(u.contains("ExecStart=/run/fleet/bin/fleet watchdog --rearm --stale-only --observe --spawn --pinned-only"));
        assert!(
            u.contains("OnUnitActiveSec=60"),
            "the timer re-fires on the cadence"
        );
        // Ordered after the wake path it complements (the secondary box's request), and installable as a user timer.
        assert!(
            u.contains("After=fleet-notify.service") && u.contains("Wants=fleet-notify.service")
        );
        assert!(u.contains("WantedBy=timers.target"));
    }

    #[test]
    fn watchdog_unit_files_splits_service_and_timer_cleanly() {
        // Observer-only exec, for the host-a coexistence install.
        let (service, timer) = watchdog_unit_files(
            "/bin/fleet",
            &watchdog_exec_args(false, true, false, false, false, false),
            90,
            "",
        );
        // The service file has the oneshot + ExecStart, NO timer/header lines.
        assert!(service.contains("Type=oneshot"));
        assert!(service.contains("ExecStart=/bin/fleet watchdog --observe --spawn"));
        assert!(
            !service.contains("OnUnitActiveSec"),
            "timer stanza belongs in the timer file, not the service"
        );
        assert!(
            !service.contains("# ----"),
            "unit files carry no display headers"
        );
        // The timer file drives the cadence + is enable-able.
        assert!(timer.contains("OnUnitActiveSec=90") && timer.contains("WantedBy=timers.target"));
        assert!(!timer.contains("ExecStart"), "no ExecStart in the timer");
    }

    #[test]
    fn watchdog_unit_host_line_is_present_or_omitted_per_the_host_arg() {
        // task_948: a declared host emits Environment="FLEET_HOST=<host>" -- the unit text names the host as
        // DATA, never a literal baked into the ExecStart/args, so a target-swap (task_937 Phase B) is a
        // different --host value through the SAME generator, not a re-landing.
        let with_host =
            render_service_env_lines(&[("FLEET_HOST", Some("dev-dsk-foo".to_string()))]);
        let (service, _) = watchdog_unit_files(
            "/bin/fleet",
            &watchdog_exec_args(true, false, false, false, false, false),
            60,
            &with_host,
        );
        assert!(service.contains("Environment=\"FLEET_HOST=dev-dsk-foo\""));

        // No host declared -> no FLEET_HOST line at all (today's green-machine-less behavior, unchanged).
        let no_host = render_service_env_lines(&[("FLEET_HOST", None)]);
        assert_eq!(no_host, "");
        let (service2, _) = watchdog_unit_files(
            "/bin/fleet",
            &watchdog_exec_args(true, false, false, false, false, false),
            60,
            &no_host,
        );
        assert!(!service2.contains("FLEET_HOST"));
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
        assert!(
            !block.contains("AWS_PROFILE"),
            "an unset var is skipped, not emitted empty"
        );
    }

    #[test]
    fn watchdog_unit_env_block_lands_in_the_service_before_execstart() {
        // The captured env block sits in [Service] ahead of ExecStart so the spawned observer inherits PATH
        // (else `exec claude` is not found under the stripped systemd env and the window closes with 127).
        let env = render_service_env_lines(&[("PATH", Some("/home/u/.local/bin".into()))]);
        let (service, _timer) = watchdog_unit_files(
            "/bin/fleet",
            &watchdog_exec_args(true, true, false, false, false, false),
            60,
            &env,
        );
        let env_at = service
            .find("Environment=\"PATH=")
            .expect("env line present");
        let exec_at = service.find("ExecStart=").expect("ExecStart present");
        assert!(
            env_at < exec_at,
            "Environment= must precede ExecStart in the unit"
        );
        // A rearm-only unit (no observe) is emitted with an empty env block — no launch environment needed.
        let (rearm_only, _) = watchdog_unit_files(
            "/bin/fleet",
            &watchdog_exec_args(true, false, false, false, false, false),
            60,
            "",
        );
        assert!(
            !rearm_only.contains("Environment="),
            "rearm-only watchdog spawns nothing → no env block"
        );
    }

    #[test]
    fn up_exec_args_always_launches_and_adds_pinned_only() {
        // task_464 launcher: the unit must actually bring the fleet up (--launch), not just report.
        assert_eq!(up_exec_args(false), "up-board --launch");
        assert_eq!(up_exec_args(true), "up-board --launch --pinned-only");
    }

    #[test]
    fn up_unit_files_is_a_oneshot_launcher_plus_boot_and_reconcile_timer() {
        let (service, timer) = up_unit_files("/run/fleet/bin/fleet", &up_exec_args(false), 300, "");
        // The launcher is a oneshot that reconstitutes the fleet, ordered after the network (it reads the board).
        assert!(
            service.contains("Type=oneshot"),
            "launcher is a single reconcile pass, not long-running"
        );
        assert!(
            service.contains("After=network-online.target"),
            "needs the network/board up"
        );
        assert!(service.contains("ExecStart=/run/fleet/bin/fleet up-board --launch"));
        // The timer fires it at boot (bring the fleet up) and periodically (reconcile a died window).
        assert!(
            timer.contains("OnBootSec=30") && timer.contains("OnUnitActiveSec=300"),
            "boot + reconcile"
        );
        assert!(timer.contains("Persistent=true") && timer.contains("WantedBy=timers.target"));
    }

    #[test]
    fn up_unit_host_line_is_present_or_omitted_per_the_host_arg() {
        // task_937 Phase A: a declared host emits Environment="FLEET_HOST=<host>" so the unit text never hardcodes
        // a box -- the host is a generator argument, making the Phase-B target-swap a one-argument change.
        let with_host =
            render_service_env_lines(&[("FLEET_HOST", Some("dev-dsk-foo".to_string()))]);
        let (service, _) = up_unit_files("/bin/fleet", &up_exec_args(true), 300, &with_host);
        assert!(service.contains("Environment=\"FLEET_HOST=dev-dsk-foo\""));
        assert!(service.contains("up-board --launch --pinned-only"));
        // No host declared → no FLEET_HOST line at all (the host-less default, same as the watchdog unit).
        let (service2, _) = up_unit_files("/bin/fleet", &up_exec_args(false), 300, "");
        assert!(
            !service2.contains("FLEET_HOST"),
            "no host arg → no FLEET_HOST line"
        );
    }

    #[test]
    fn daemon_unit_file_is_a_restarting_simple_service_with_a_known_good_path() {
        let env = render_service_env_lines(&[("PATH", Some("/usr/bin:/bin".into()))]);
        let u = daemon_unit_file("notifier", "/run/fleet/bin/fleet notify", 2, &env);
        // A long-running daemon that survives a crash — NOT a oneshot; this is the durable replacement for the
        // bare keep-alive tmux window a reap silently kills (#359).
        assert!(
            u.contains("Type=simple"),
            "long-running daemon, not oneshot"
        );
        assert!(
            u.contains("Restart=on-failure") && u.contains("RestartSec=2"),
            "restarts on crash"
        );
        assert!(u.contains("ExecStart=/run/fleet/bin/fleet notify"));
        assert!(
            u.contains("WantedBy=default.target"),
            "enabled comes up on login/boot"
        );
        // The captured PATH is seeded ahead of ExecStart so the daemon resolves tmux/git/curl at runtime (#347/#359).
        let env_at = u
            .find("Environment=\"PATH=/usr/bin:/bin\"")
            .expect("PATH env line present");
        assert!(
            env_at < u.find("ExecStart=").expect("ExecStart present"),
            "env precedes ExecStart"
        );
    }

    #[test]
    fn daemon_unit_file_carries_a_declared_fleet_host_before_execstart() {
        // task_464/task_937 Phase A: daemon-unit --host emits FLEET_HOST so tunnel/notifier are host-parameterized
        // like watchdog-unit/up-unit — the flake passes --host and a Phase-B retarget is a one-value change. The
        // FLEET_HOST line is seeded ahead of ExecStart alongside the captured PATH (the env_block daemon_unit builds).
        let mut env = render_service_env_lines(&[("FLEET_HOST", Some("dev-dsk-foo".into()))]);
        env.push_str(&render_service_env_lines(&[(
            "PATH",
            Some("/usr/bin".into()),
        )]));
        let u = daemon_unit_file("tunnel", "/run/fleet/bin/fleet tunnel", 2, &env);
        assert!(
            u.contains("Environment=\"FLEET_HOST=dev-dsk-foo\""),
            "declared host emits a FLEET_HOST line"
        );
        assert!(
            u.find("FLEET_HOST").unwrap() < u.find("ExecStart=").expect("ExecStart present"),
            "FLEET_HOST must precede ExecStart"
        );
        // No host → no FLEET_HOST line at all (host-less default, same as watchdog-unit/up-unit).
        let no_host =
            render_service_env_lines(&[("FLEET_HOST", None), ("PATH", Some("/usr/bin".into()))]);
        assert!(
            !daemon_unit_file("tunnel", "x", 2, &no_host).contains("FLEET_HOST"),
            "no host arg → no line"
        );
    }

    #[test]
    fn write_units_to_dir_writes_named_files_for_a_pure_emit() {
        // task_464 --out-dir: a nix derivation points this at $out, so it must write correctly-named files with
        // the exact bodies into an ARBITRARY dir (not HOME), creating the dir.
        let dir = std::env::temp_dir().join(format!("fleet-out-dir-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let d = dir.to_str().expect("temp dir path is utf8");
        write_units_to_dir(
            d,
            &[
                ("fleet-up.service", "SVC-BODY"),
                ("fleet-up.timer", "TIMER-BODY"),
            ],
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("fleet-up.service")).unwrap(),
            "SVC-BODY"
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("fleet-up.timer")).unwrap(),
            "TIMER-BODY"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn enable_argv_reloads_then_enables_now_the_named_unit() {
        let steps = enable_argv("fleet-notifier.service");
        // Two invocations, IN ORDER: reload so systemd sees the freshly-written unit, THEN enable --now.
        assert_eq!(steps.len(), 2);
        assert_eq!(steps[0], vec!["--user", "daemon-reload"]);
        assert_eq!(
            steps[1],
            vec!["--user", "enable", "--now", "fleet-notifier.service"]
        );
        // Every invocation is user-level (no sudo) — the same no-privilege install path as --install.
        assert!(
            steps
                .iter()
                .all(|a| a.first().map(String::as_str) == Some("--user")),
            "all user-level"
        );
        // The unit name is carried verbatim into the enable step (a tunnel unit enables the same way).
        assert_eq!(
            enable_argv("fleet-tunnel.service")[1].last().unwrap(),
            "fleet-tunnel.service"
        );
    }

    #[test]
    fn watchdog_manages_agent_uses_strict_pin_only_under_pinned_only() {
        let green = serde_json::json!({ "host": "host-b" });
        let unpinned = serde_json::json!({});
        // Default (loose): this-host-pinned OR unpinned are managed here.
        assert!(watchdog_manages_agent(Some(&green), "host-b", false));
        assert!(
            watchdog_manages_agent(Some(&unpinned), "host-b", false),
            "unpinned managed everywhere by default"
        );
        assert!(
            !watchdog_manages_agent(Some(&green), "host-a", false),
            "other-box pin never managed here"
        );
        // --pinned-only: ONLY agents explicitly pinned here — unpinned run-anywhere agents are excluded, so a
        // secondary box never re-arms/observes an agent whose window/transcript is on another box.
        assert!(watchdog_manages_agent(Some(&green), "host-b", true));
        assert!(
            !watchdog_manages_agent(Some(&unpinned), "host-b", true),
            "unpinned EXCLUDED under --pinned-only"
        );
        assert!(!watchdog_manages_agent(Some(&green), "host-a", true));
    }

    #[test]
    fn agent_is_staged_reads_the_reserve_flag() {
        assert!(agent_is_staged(Some(
            &serde_json::json!({ "staged": true })
        )));
        assert!(!agent_is_staged(Some(
            &serde_json::json!({ "staged": false })
        )));
        assert!(
            !agent_is_staged(Some(&serde_json::json!({}))),
            "absent flag → not staged"
        );
        assert!(
            !agent_is_staged(Some(&serde_json::json!({ "staged": "true" }))),
            "non-bool → not staged"
        );
        assert!(!agent_is_staged(None));
    }

    #[test]
    fn watchdog_never_manages_a_staged_agent() {
        // A staged reserve helper is not meant to be running, so the watchdog must never re-arm or observe it —
        // even when it is native + pinned to this exact box (which would otherwise be managed).
        let staged_here = serde_json::json!({ "host": "host-b", "staged": true });
        assert!(!watchdog_manages_agent(Some(&staged_here), "host-b", false));
        assert!(!watchdog_manages_agent(Some(&staged_here), "host-b", true));
    }

    #[test]
    fn agent_host_is_explicit_requires_a_deliberate_pin_to_this_box() {
        // EXPLICIT pin → true only for the named box (this is the --pinned-only launch predicate).
        assert!(agent_host_is_explicit(
            Some(&serde_json::json!({"host":"host-b"})),
            "host-b"
        ));
        assert!(!agent_host_is_explicit(
            Some(&serde_json::json!({"host":"host-b"})),
            "host-a"
        ));
        assert!(agent_host_is_explicit(
            Some(&serde_json::json!({"host":["host-b","host-a"]})),
            "host-a"
        ));
        assert!(!agent_host_is_explicit(
            Some(&serde_json::json!({"host":["host-b"]})),
            "host-a"
        ));
        // UNPINNED (unset / null / empty string / empty array) → FALSE — the key difference from
        // agent_host_matches: an unpinned run-anywhere agent is NOT an explicit launch candidate here, so a
        // second box's --pinned-only reconcile won't double-launch it.
        assert!(!agent_host_is_explicit(None, "host-a"));
        assert!(!agent_host_is_explicit(
            Some(&serde_json::json!({})),
            "host-a"
        ));
        assert!(!agent_host_is_explicit(
            Some(&serde_json::json!({"host":null})),
            "host-a"
        ));
        assert!(!agent_host_is_explicit(
            Some(&serde_json::json!({"host":""})),
            "host-a"
        ));
        assert!(!agent_host_is_explicit(
            Some(&serde_json::json!({"host":[]})),
            "host-a"
        ));
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
            (
                "v-b".to_string(),
                Some(serde_json::json!({"host": "host-a"})),
            ),
            (
                "v-elsewhere".to_string(),
                Some(serde_json::json!({"host": "host-b"})),
            ),
            ("v-nometa".to_string(), None),
        ];
        // tmux windows: some agents, plus daemon/scratch windows that are NOT board agents, plus a board
        // agent (v-noagent-window ... actually a board agent with no window is v-noagent) — cover both drops.
        let windows = vec![
            "v-a".to_string(),
            "v-b".to_string(),
            "v-elsewhere".to_string(), // pinned to host-b → dropped even though a window exists here
            "v-nometa".to_string(),
            "notify".to_string(), // a daemon window, not a board agent → dropped
            "scratch".to_string(), // not a board agent → dropped
        ];
        // A board agent with NO window here (v-c) must also be absent (nothing to wake on this host).
        let mut with_windowless = agents.clone();
        with_windowless.push(("v-c".to_string(), Some(serde_json::json!({}))));
        let served = derive_served_set(&windows, &with_windowless, "host-a");
        assert_eq!(
            served,
            vec!["v-a", "v-b", "v-nometa"],
            "window∩board, minus off-host pins and non-agents"
        );
        // sorted + deduped even if the board lists a duplicate id
        let dupe = vec![("v-a".to_string(), None), ("v-a".to_string(), None)];
        assert_eq!(
            derive_served_set(&["v-a".to_string()], &dupe, "host-a"),
            vec!["v-a"]
        );
    }

    #[test]
    fn liveness_verdict_buckets_by_heartbeat_age() {
        assert_eq!(
            liveness_verdict(-30),
            "live",
            "clock skew (future) is not stale"
        );
        assert_eq!(liveness_verdict(0), "live");
        assert_eq!(liveness_verdict(LIVE_SECS - 1), "live");
        assert_eq!(
            liveness_verdict(LIVE_SECS),
            "quiet",
            "at the live bound → quiet"
        );
        assert_eq!(liveness_verdict(QUIET_SECS - 1), "quiet");
        assert_eq!(
            liveness_verdict(QUIET_SECS),
            "STALE",
            "at the quiet bound → STALE"
        );
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
        assert_eq!(
            v["projects"]["/root/.fleet"]["hasTrustDialogAccepted"],
            true
        );
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
        assert_eq!(
            resolve_model("claude-sonnet-5-5"),
            "us.anthropic.claude-sonnet-5"
        );
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
            deploy_event_body("camshaft/task-board", "abc123", "host-b", "live"),
            "deploy camshaft/task-board@abc123 → host-b: LIVE"
        );
        // status is upper-cased + trimmed so a waiter keys on LIVE vs FAILED regardless of caller casing.
        assert_eq!(
            deploy_event_body("o/r", "deadbeef", "host-b", " failed "),
            "deploy o/r@deadbeef → host-b: FAILED"
        );
    }

    #[test]
    fn observe_spawn_cooldown_holds_within_window_and_lapses_after() {
        assert!(
            !observe_on_spawn_cooldown(None, 10_000, 1800),
            "never spawned → not on cooldown"
        );
        assert!(
            observe_on_spawn_cooldown(Some(9_000), 10_000, 1800),
            "1000s < 1800 → on cooldown"
        );
        assert!(
            !observe_on_spawn_cooldown(Some(8_000), 10_000, 1800),
            "2000s ≥ 1800 → lapsed"
        );
        // saturating: a future stamp (clock skew) is treated as just-spawned → on cooldown, never underflows.
        assert!(observe_on_spawn_cooldown(Some(11_000), 10_000, 1800));
    }

    #[test]
    fn observe_respawn_cooldown_backs_off_per_unconfirmed_attempt_capped() {
        // task_610: 0 or 1 prior attempt → base; then doubles per attempt, capped at max_doublings.
        assert_eq!(
            observe_respawn_cooldown(1800, 0, 3),
            1800,
            "first spawn → base 30m"
        );
        assert_eq!(
            observe_respawn_cooldown(1800, 1, 3),
            1800,
            "1 prior → still base (no backoff yet)"
        );
        assert_eq!(
            observe_respawn_cooldown(1800, 2, 3),
            3600,
            "2 prior → 2x = 1h"
        );
        assert_eq!(
            observe_respawn_cooldown(1800, 3, 3),
            7200,
            "3 prior → 4x = 2h"
        );
        assert_eq!(
            observe_respawn_cooldown(1800, 4, 3),
            14400,
            "4 prior → 8x = 4h"
        );
        assert_eq!(
            observe_respawn_cooldown(1800, 9, 3),
            14400,
            "capped at 8x even after many attempts"
        );
    }

    #[test]
    fn observe_spawn_attempts_roundtrip_and_reset_on_watermark_change() {
        let (_base, fleet) = tmp_hub();
        // Absent → ("", 0): never a spurious backoff.
        assert_eq!(
            read_observe_spawn_attempts(&fleet, "v-x"),
            (String::new(), 0)
        );
        // Record attempts against a watermark; they read back verbatim.
        write_observe_spawn_attempts(&fleet, "v-x", "sess-a:100", 2);
        assert_eq!(
            read_observe_spawn_attempts(&fleet, "v-x"),
            ("sess-a:100".to_string(), 2)
        );
        // The spawn loop treats a DIFFERENT current watermark (a confirmed observation advanced it) as a reset:
        // rec_wm != cur_wm ⇒ attempts = 0, so the backoff reverts to base. (The loop's own `if rec_wm == cur_wm`.)
        let (rec_wm, prior) = read_observe_spawn_attempts(&fleet, "v-x");
        let attempts_after_advance = if rec_wm == "sess-a:200" { prior } else { 0 };
        assert_eq!(
            attempts_after_advance, 0,
            "watermark advanced ⇒ attempts reset ⇒ base cooldown"
        );
    }

    #[test]
    fn review_needs_adversarial_pass_only_for_unvetted_in_review() {
        // task_374 S2c sweep filter (v-task-board contract comment_5794): the list endpoint has no vetted
        // filter, so review-sweep keeps status==in_review AND vetted!=true client-side.
        let needs = |v: serde_json::Value| review_needs_adversarial_pass(&v);
        // in_review + not vetted → needs the pass.
        assert!(needs(
            serde_json::json!({ "status": "in_review", "vetted": false })
        ));
        // in_review but a missing vetted → safe default is "needs a pass".
        assert!(needs(serde_json::json!({ "status": "in_review" })));
        // in_review but already vetted → the pass ran + was addressed → skip.
        assert!(!needs(
            serde_json::json!({ "status": "in_review", "vetted": true })
        ));
        // not in_review → no pass regardless of vetted.
        assert!(!needs(
            serde_json::json!({ "status": "draft", "vetted": false })
        ));
        assert!(!needs(
            serde_json::json!({ "status": "vetted", "vetted": true })
        ));
    }

    #[test]
    fn observer_window_is_stale_spares_a_recent_in_flight_observer_only() {
        let bound = 3600;
        // Recent stamp (a healthy in-flight observer) → NOT stale, never reaped.
        assert!(
            !observer_window_is_stale(Some(10_000), 10_500, bound),
            "500s < 3600 → in-flight, spare it"
        );
        assert!(
            !observer_window_is_stale(Some(10_000), 13_599, bound),
            "just under the bound → still spare"
        );
        // Stamp older than the bound → the observer crashed before observe-record → reap.
        assert!(
            observer_window_is_stale(Some(10_000), 13_600, bound),
            "exactly the bound → stale"
        );
        assert!(
            observer_window_is_stale(Some(10_000), 99_999, bound),
            "long overdue → stale"
        );
        // No stamp on a still-open window → confirmed-but-self-close-missed (nothing in-flight) → reap.
        assert!(observer_window_is_stale(None, 10_000, bound));
        // saturating: a future stamp (clock skew) → treated as just-spawned → not stale (never underflows).
        assert!(!observer_window_is_stale(Some(20_000), 10_000, bound));
    }

    #[test]
    fn obs_window_name_mangles_slashes_colons_dots() {
        assert_eq!(obs_window_name("v-fleet-tooling"), "obs-v-fleet-tooling");
        assert_eq!(obs_window_name("camshaft/cadenza"), "obs-camshaft-cadenza");
        assert_eq!(obs_window_name("a.b:c/d"), "obs-a-b-c-d");
    }

    #[test]
    fn reviewer_window_name_is_per_review_and_angle_and_window_safe() {
        // task_374 S2a: one window per (review, angle) so angles + reviews don't clash. Angle keys are kebab
        // already; defensively mangle any `/`:`.` to `-` so it stays a single valid tmux window token.
        assert_eq!(
            reviewer_window_name(42, "risk-security"),
            "rev-42-risk-security"
        );
        assert_eq!(
            reviewer_window_name(7, "clarity-writing"),
            "rev-7-clarity-writing"
        );
        assert_eq!(reviewer_window_name(1, "a/b.c:d"), "rev-1-a-b-c-d");
        // distinct review ids and distinct angles both yield distinct windows (no collision).
        assert_ne!(
            reviewer_window_name(1, "alternatives"),
            reviewer_window_name(2, "alternatives")
        );
        assert_ne!(
            reviewer_window_name(1, "alternatives"),
            reviewer_window_name(1, "risk-security")
        );
    }

    #[test]
    fn reviewer_claim_external_id_matches_the_v_task_board_contract() {
        // task_374 S2 claim contract (v-task-board comment_1093): adversarial-claim:<review_id>:<angle>, PER
        // ANGLE so a partial re-sweep re-claims only the un-claimed angle. Append-first-then-spawn-on-appended.
        assert_eq!(
            reviewer_claim_external_id(42, "risk-security"),
            "adversarial-claim:42:risk-security"
        );
        assert_eq!(
            reviewer_claim_external_id(7, "correctness-completeness"),
            "adversarial-claim:7:correctness-completeness"
        );
        // every shipped angle yields a DISTINCT claim id for a given review (independent per-angle spawn).
        let ids: std::collections::BTreeSet<_> = REVIEW_ANGLES
            .iter()
            .map(|(k, _)| reviewer_claim_external_id(42, k))
            .collect();
        assert_eq!(
            ids.len(),
            REVIEW_ANGLES.len(),
            "per-angle claim ids are unique"
        );
    }

    #[test]
    fn observe_watermark_round_trips_and_defaults_when_absent() {
        let (base, fleet) = tmp_hub();
        assert_eq!(
            read_observe_watermark(&fleet, "a"),
            (String::new(), 0),
            "absent → empty/0"
        );
        write_observe_watermark(&fleet, "a", "sess-1", 4200);
        assert_eq!(
            read_observe_watermark(&fleet, "a"),
            ("sess-1".to_string(), 4200)
        );
        // observe-record advances it (and a later read sees the new offset).
        write_observe_watermark(&fleet, "a", "sess-1", 5000);
        assert_eq!(
            read_observe_watermark(&fleet, "a"),
            ("sess-1".to_string(), 5000)
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn build_observer_kickoff_carries_identity_window_and_completion_command() {
        let k = build_observer_kickoff(
            "v-x",
            "sess-9",
            1200,
            "/repo/loops/observer.md",
            "/repo/target/release/fleet",
            None,
        );
        // Ephemeral + single stable board identity (one-shot; author as `observer`).
        assert!(k.contains("EPHEMERAL"));
        assert!(
            k.contains("ONE observation") && k.contains("do NOT start a /loop"),
            "one-shot, not looping"
        );
        assert!(k.contains("`observer`") && k.contains("register_agent"));
        // Author identity must be PASSED explicitly on each board write (board defaults to null) — the gap
        // the first live dry-run surfaced (created_by came out null).
        assert!(k.contains("created_by=\"observer\"") && k.contains("author=\"observer\""));
        // Uses the ABSOLUTE standalone binary (not PATH `fleet`) for transcripts + observe-record, with the
        // exact target window (agent, session, offset).
        assert!(k.contains(
            "/repo/target/release/fleet transcripts v-x --session sess-9 --since sess-9:1200"
        ));
        assert!(
            k.contains("/repo/target/release/fleet observe-record v-x --session sess-9 --offset")
        );
        assert!(
            k.contains("/repo/loops/observer.md"),
            "points at the full role body"
        );
        assert!(
            k.contains("project #28"),
            "files into the fleet-self-improve lane"
        );
        // No commit/PR attribution lines in the observer's board task bodies (board-pm; the observer was
        // leaking a "Generated with ..." line into task bodies).
        assert!(
            k.contains("Co-Authored-By:") && k.contains("not board content"),
            "observer kickoff bans attribution lines in board bodies"
        );
    }

    #[test]
    fn observe_record_self_closes_only_an_observer_window_inside_tmux() {
        // Inside tmux AND in the observer's own `obs-…` window → close it (the last act of a one-shot observer).
        assert!(observer_should_self_close(true, "obs-v-task-board"));
        assert!(observer_should_self_close(true, "obs-v-x"));
        // A manual `observe-record` from any other window must NOT self-close (only obs- windows are observers).
        assert!(!observer_should_self_close(true, "main"));
        assert!(!observer_should_self_close(true, "v-fleet-tooling"));
        assert!(
            !observer_should_self_close(true, "observer"),
            "the identity name is not the window prefix"
        );
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
    fn agent_from_cwd_extracts_the_segment_after_agents() {
        assert_eq!(
            agent_from_cwd("/local/home/u/.fleet/agents/ticket-ingest/fleet"),
            Some("ticket-ingest")
        );
        assert_eq!(
            agent_from_cwd("/home/u/.fleet/agents/membrain-ops"),
            Some("membrain-ops")
        );
        // No `agents/` component (e.g. a worktree-based agent) -> cannot name an agent.
        assert_eq!(agent_from_cwd("/home/u/.claude/worktrees/some-topic"), None);
        // `agents` with nothing after it -> None.
        assert_eq!(agent_from_cwd("/home/u/.fleet/agents"), None);
    }

    #[test]
    fn ancestor_pids_walks_the_parent_chain_and_stops_at_root() {
        // chain: 500 -> 400 -> 300 -> 1 (root). Ancestors of 500 are [400, 300].
        let parent = |p: u32| match p {
            500 => Some(400),
            400 => Some(300),
            300 => Some(1),
            _ => None,
        };
        assert_eq!(ancestor_pids(500, &parent), vec![400, 300]);
        // A cycle (A->B->A) is broken, not looped forever.
        let cyc = |p: u32| match p {
            10 => Some(20),
            20 => Some(10),
            _ => None,
        };
        assert_eq!(ancestor_pids(10, &cyc), vec![20]);
    }

    #[test]
    fn safe_kill_targets_excludes_the_protected_self_set() {
        // 300/400 are protected (caller's shell + wrapper); 900/901 are real matches to kill.
        let protected: std::collections::HashSet<u32> = [111u32, 300, 400].into_iter().collect();
        let matches = vec![400u32, 900, 300, 901, 111];
        assert_eq!(safe_kill_targets(&matches, &protected), vec![900, 901]);
        // If every match is protected (the pkill -f self-match case) -> nothing to kill.
        assert!(safe_kill_targets(&[300, 400], &protected).is_empty());
    }

    #[test]
    fn dedup_decision_keeps_oldest_and_kills_the_rest_only_for_duplicated_agents() {
        // ticket-ingest has two instances (pid 100 start=5 is older, pid 200 start=50) -> keep 100, kill [200].
        // membrain-ops has one -> omitted (nothing to do).
        let inst = vec![
            (200u32, "ticket-ingest".to_string(), 50u64),
            (100u32, "ticket-ingest".to_string(), 5u64),
            (300u32, "membrain-ops".to_string(), 7u64),
        ];
        assert_eq!(
            dedup_decision(&inst),
            vec![("ticket-ingest".to_string(), 100u32, vec![200u32])]
        );
        // All-singleton -> empty (clean 1:1).
        assert!(dedup_decision(&[(1, "a".to_string(), 1), (2, "b".to_string(), 1)]).is_empty());
        // Three instances of one agent -> keep oldest, kill the other two (newest-first order preserved by start).
        let three = vec![
            (9u32, "x".to_string(), 30u64),
            (7u32, "x".to_string(), 10u64),
            (8u32, "x".to_string(), 20u64),
        ];
        assert_eq!(
            dedup_decision(&three),
            vec![("x".to_string(), 7u32, vec![8u32, 9u32])]
        );
    }

    #[test]
    fn taskboard_identity_config_extracts_header_and_helper() {
        let v = serde_json::json!({
            "mcpServers": { "task-board": {
                "headers": {"X-Fleet-Agent": "${FLEET_AGENT}"},
                "headersHelper": "echo '{}'"
            }}
        });
        let tb = taskboard_identity_config(&v);
        assert!(tb.present && tb.has_helper);
        assert_eq!(tb.static_header.as_deref(), Some("${FLEET_AGENT}"));
        // No task-board entry -> absent.
        assert!(!taskboard_identity_config(&serde_json::json!({"mcpServers": {}})).present);
        // Entry with neither header nor helper -> present but unwired.
        let bare = taskboard_identity_config(
            &serde_json::json!({"mcpServers": {"task-board": {"url": "http://x"}}}),
        );
        assert!(bare.present && !bare.has_helper && bare.static_header.is_none());
    }

    #[test]
    fn mcp_check_passes_when_fleet_agent_matches_and_header_wired() {
        let tb = TaskboardIdentityConfig {
            present: true,
            static_header: Some("${FLEET_AGENT}".to_string()),
            has_helper: true,
        };
        let r = mcp_check_decision(
            "membrain-ops",
            Some("membrain-ops"),
            &["claude".to_string()],
            &tb,
        );
        assert_eq!(r.verdict, McpCheckVerdict::Pass);
        assert!(r.fleet_agent_matches && r.identity_wired && !r.uses_config_override);
    }

    #[test]
    fn mcp_check_fails_when_fleet_agent_unset_or_unwired() {
        // FLEET_AGENT absent -> the ${FLEET_AGENT} header expands empty, no identity forced.
        let wired = TaskboardIdentityConfig {
            present: true,
            static_header: Some("${FLEET_AGENT}".to_string()),
            has_helper: true,
        };
        assert_eq!(
            mcp_check_decision("x", None, &[], &wired).verdict,
            McpCheckVerdict::Fail
        );
        // No header and no helper -> nothing to force from.
        let unwired = TaskboardIdentityConfig {
            present: true,
            static_header: None,
            has_helper: false,
        };
        let r = mcp_check_decision("x", Some("x"), &[], &unwired);
        assert_eq!(r.verdict, McpCheckVerdict::Fail);
        assert!(!r.identity_wired);
    }

    #[test]
    fn mcp_check_warns_on_mismatch_or_shadowing_override() {
        let tb = TaskboardIdentityConfig {
            present: true,
            static_header: Some("${FLEET_AGENT}".to_string()),
            has_helper: false,
        };
        // FLEET_AGENT set but to the wrong id -> would force the wrong identity.
        let mismatch = mcp_check_decision("want", Some("other"), &[], &tb);
        assert_eq!(mismatch.verdict, McpCheckVerdict::Warn);
        assert!(!mismatch.fleet_agent_matches);
        // A --mcp-config override may shadow the user-scope entry this check read.
        let argv = [
            "claude".to_string(),
            "--mcp-config".to_string(),
            "/x.json".to_string(),
        ];
        let shadow = mcp_check_decision("a", Some("a"), &argv, &tb);
        assert_eq!(shadow.verdict, McpCheckVerdict::Warn);
        assert!(shadow.uses_config_override);
        // A static header that is neither ${FLEET_AGENT} nor the agent name, with no helper -> warn.
        let hardcoded = TaskboardIdentityConfig {
            present: true,
            static_header: Some("someone-else".to_string()),
            has_helper: false,
        };
        assert_eq!(
            mcp_check_decision("a", Some("a"), &["claude".to_string()], &hardcoded).verdict,
            McpCheckVerdict::Warn
        );
    }

    #[test]
    fn board_window_argv_exports_fleet_agent_and_kickoff_into_the_window_env() {
        let argv = board_window_argv(
            "main",
            "ticket-ingest",
            "/wt/ti",
            "do a tick",
            "claude --model x",
        );
        // The agent identity is in the window env so the per-session X-Fleet-Agent header (task_1039) is
        // non-empty for a board-native launch (which never execs window.sh). Window name == FLEET_AGENT value.
        assert!(
            argv.windows(2)
                .any(|w| w[0] == "-e" && w[1] == "FLEET_AGENT=ticket-ingest"),
            "board-native launch must export FLEET_AGENT: {argv:?}"
        );
        assert!(
            argv.windows(2)
                .any(|w| w[0] == "-e" && w[1] == "CDZ_KICKOFF=do a tick")
        );
        assert_eq!(argv.first().map(String::as_str), Some("new-window"));
        assert_eq!(argv.last().map(String::as_str), Some("claude --model x"));
    }

    #[test]
    fn window_exists_matches_a_whole_window_name_line() {
        let listing = "concierge\nticket-ingest\nv-fleet-tooling\n";
        // Present -> the launch guard refuses a second window (idempotent launch, no double-spawn).
        assert!(window_exists(listing, "ticket-ingest"));
        assert!(window_exists(listing, "concierge"));
        // Absent -> launchable.
        assert!(!window_exists(listing, "v-nix"));
        // Whole-line match only: a prefix/substring is NOT a window (so `obs-ticket-ingest` != `ticket-ingest`).
        assert!(!window_exists(
            "obs-ticket-ingest\nticket-ingest-helper\n",
            "ticket-ingest"
        ));
        assert!(!window_exists("", "anything"));
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
        let mut reg = Registry {
            agents: vec![mk_agent("a", "active"), mk_agent("b", "active")],
        };
        assert!(
            registry_set_interval(&mut reg, "a", "2h"),
            "row present → updated"
        );
        assert_eq!(
            reg.agents.iter().find(|x| x.name == "a").unwrap().interval,
            "2h"
        );
        assert_eq!(
            reg.agents.iter().find(|x| x.name == "b").unwrap().interval,
            "10m",
            "other rows untouched"
        );
        assert!(
            !registry_set_interval(&mut reg, "nobody", "5m"),
            "absent agent → false, no-op"
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

    #[test]
    fn nudge_run_is_outage_only_when_apply_attempted_but_landed_zero() {
        // task_609: a healthy quiet sweep (no stale tasks → posted 0, failed 0) is NOT an outage, so it exits 0.
        assert!(
            !nudge_run_is_outage(true, 0, 0),
            "no stale tasks is a healthy run, not an outage"
        );
        // Total outage: posts were attempted and every one failed (posted 0, failed>0) → outage, exit non-zero.
        assert!(
            nudge_run_is_outage(true, 0, 3),
            "attempted>0 && posted==0 is a total outage"
        );
        // Partial: at least one landed → the path is up, not an outage.
        assert!(
            !nudge_run_is_outage(true, 1, 2),
            "any successful post means the board path is up"
        );
        assert!(
            !nudge_run_is_outage(true, 5, 0),
            "all posts succeeded → not an outage"
        );
        // A dry run posts nothing by design, so it is never an outage regardless of the counts.
        assert!(
            !nudge_run_is_outage(false, 0, 3),
            "a dry run never posts, so never an outage"
        );
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
        assert!(!stale_task_should_nudge(
            threshold * 5,
            Some(cooldown + 1),
            true,
            threshold,
            cooldown,
            acked
        ));
        assert!(stale_task_should_nudge(
            threshold * 5,
            Some(cooldown + 1),
            false,
            threshold,
            cooldown,
            acked
        ));
        // Anti-parking: even with a fresh ack, once the prior nudge is past the ACKED cooldown it re-nudges.
        assert!(stale_task_should_nudge(
            threshold * 5,
            Some(acked),
            true,
            threshold,
            cooldown,
            acked
        ));
        // The ack flag never overrides the threshold gate, and never fabricates a first nudge early.
        assert!(!stale_task_should_nudge(
            threshold - 1,
            None,
            true,
            threshold,
            cooldown,
            acked
        ));
        assert!(stale_task_should_nudge(
            threshold, None, true, threshold, cooldown, acked
        ));
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
        use time::{Duration, format_description::well_known::Rfc3339};
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
        assert!(
            (age - 3600).abs() < 2,
            "newest assignee comment is 1h old, got {age}"
        );
        // An assignee who never commented → None.
        assert_eq!(
            newest_assignee_comment_age_secs(&task, "v-never", now),
            None
        );
    }

    #[test]
    fn nudge_round_count_counts_daemon_nudges_since_the_last_owner_comment() {
        use time::{Duration, format_description::well_known::Rfc3339};
        let now = time::OffsetDateTime::now_utc();
        let stamp = |d: Duration| (now - d).format(&Rfc3339).unwrap();
        const DAEMON: &str = "fleet-nudge-daemon";
        // Owner commented 5h ago, then the daemon nudged at 4h/2h/30m with no owner reply since → 3 unanswered.
        let unanswered_3 = serde_json::json!({
            "comments": [
                { "author": "v-x", "created_at": stamp(Duration::hours(5)) },
                { "author": DAEMON, "created_at": stamp(Duration::hours(4)) },
                { "author": DAEMON, "created_at": stamp(Duration::hours(2)) },
                { "author": DAEMON, "created_at": stamp(Duration::minutes(30)) },
            ],
        });
        assert_eq!(nudge_round_count(&unanswered_3, now), 3);
        // A fresh owner reply (10m ago) AFTER the nudges resets the count: no nudge is newer than it.
        let answered = serde_json::json!({
            "comments": [
                { "author": DAEMON, "created_at": stamp(Duration::hours(4)) },
                { "author": DAEMON, "created_at": stamp(Duration::hours(2)) },
                { "author": "v-x", "created_at": stamp(Duration::minutes(10)) },
            ],
        });
        assert_eq!(
            nudge_round_count(&answered, now),
            0,
            "an owner reply after the nudges resets the rounds"
        );
        // No owner comment ever → every daemon nudge counts.
        let never_answered = serde_json::json!({
            "comments": [
                { "author": DAEMON, "created_at": stamp(Duration::hours(3)) },
                { "author": DAEMON, "created_at": stamp(Duration::hours(1)) },
            ],
        });
        assert_eq!(nudge_round_count(&never_answered, now), 2);
        // No comments at all → 0 rounds.
        assert_eq!(nudge_round_count(&serde_json::json!({}), now), 0);
    }

    #[test]
    fn nudge_tier_climbs_owner_then_pm_tag_then_reassign() {
        use NudgeTier::{Owner, PmTag, Reassign};
        // Defaults: pm-tag at round 2, reassign at round 3.
        assert_eq!(nudge_tier(1, 2, 3), Owner, "round 1 is the owner's alone");
        assert_eq!(
            nudge_tier(2, 2, 3),
            PmTag,
            "round 2 tags the router to make the call"
        );
        assert_eq!(
            nudge_tier(3, 2, 3),
            Reassign,
            "round 3 escalates to a reassign"
        );
        assert_eq!(nudge_tier(9, 2, 3), Reassign, "stays escalated past N");
        // reassign takes precedence when both thresholds are met.
        assert_eq!(nudge_tier(5, 2, 2), Reassign);
        // Tunable: push the thresholds out and round 2 is still the owner's.
        assert_eq!(nudge_tier(2, 3, 4), Owner);
    }

    #[test]
    fn pm_tag_body_tags_the_router_with_the_full_set_of_options() {
        let b = pm_tag_body(1.0, "v-x", 7200, 1, "board-pm");
        assert!(
            b.contains("board-pm") && b.contains("make the call"),
            "tags the router to decide"
        );
        assert!(
            b.contains("v-x") && b.contains("reassign") && b.contains("close"),
            "names the owner + the options"
        );
    }

    #[test]
    fn escalation_body_tags_the_router_to_reassign() {
        let b = escalation_body(1.0, "v-x", 7200, 3, "board-pm");
        assert!(
            b.contains("REASSIGN") && b.contains("board-pm"),
            "escalation tags the router to reassign"
        );
        assert!(
            b.contains("v-x") && b.contains('3'),
            "names the silent owner + the unanswered round count"
        );
        assert!(b.contains("mint a helper"), "offers minting a fresh owner");
    }

    #[test]
    fn accountability_digest_lists_escalations_router_first_with_typed_refs() {
        let esc = vec![
            Escalation {
                id: 10,
                title: "pm one".into(),
                owner: "v-a".into(),
                idle_secs: 7200,
                unanswered_rounds: 1,
                reassign: false,
            },
            Escalation {
                id: 20,
                title: "reassign one".into(),
                owner: "v-b".into(),
                idle_secs: 10800,
                unanswered_rounds: 2,
                reassign: true,
            },
        ];
        let b = accountability_digest_body(&esc);
        // Counts both tiers and names the router to make the call.
        assert!(
            b.contains("2 task(s) escalated") && b.contains("1 reassign") && b.contains("1 pm-tag")
        );
        assert!(b.contains("board-pm"), "names the router");
        // Typed task_<id> refs only — never a bare #<id>, which board content hard-rejects.
        assert!(
            b.contains("task_10") && b.contains("task_20"),
            "uses typed task refs"
        );
        assert!(
            !b.contains("#10") && !b.contains("#20"),
            "no bare #N refs (board hard-rejects them)"
        );
        // Reassign (more urgent) is listed before the PM-tag.
        let r = b.find("REASSIGN").expect("has a reassign line");
        let p = b.find("PM-TAG").expect("has a pm-tag line");
        assert!(r < p, "reassigns are listed before pm-tags");
        // ASCII-only (board content rule).
        assert!(b.is_ascii(), "digest body must be ASCII");
    }

    #[test]
    fn acked_cooldown_backs_off_by_doubling_per_prior_nudge_capped() {
        let base = 4 * 3600; // 4h
        // 0 or 1 prior nudge -> base (no backoff yet); then double each step, capped at 8x (32h).
        assert_eq!(acked_cooldown_with_backoff(base, 0), base);
        assert_eq!(acked_cooldown_with_backoff(base, 1), base);
        assert_eq!(acked_cooldown_with_backoff(base, 2), 2 * base, "8h");
        assert_eq!(acked_cooldown_with_backoff(base, 3), 4 * base, "16h");
        assert_eq!(acked_cooldown_with_backoff(base, 4), 8 * base, "32h");
        assert_eq!(
            acked_cooldown_with_backoff(base, 9),
            8 * base,
            "capped at 8x, never grows unbounded"
        );
    }

    #[test]
    fn nudge_comment_count_counts_only_this_daemons_nudges() {
        let t = serde_json::json!({
            "comments": [
                { "author": NUDGE_AUTHOR, "body": "n1" },
                { "author": "v-x", "body": "ack" },
                { "author": NUDGE_AUTHOR, "body": "n2" },
                { "author": "board-pm", "body": "note" },
                { "author": NUDGE_AUTHOR, "body": "n3" },
            ],
        });
        assert_eq!(
            nudge_comment_count(&t),
            3,
            "counts the 3 daemon nudges, not the owner/pm comments"
        );
        assert_eq!(
            nudge_comment_count(&serde_json::json!({})),
            0,
            "no comments -> 0"
        );
    }

    #[test]
    fn responsive_owner_is_never_escalated() {
        use NudgeTier::{Owner, PmTag, Reassign};
        // A responsive owner (acked since the last nudge) is capped at the Owner tier regardless of round.
        assert_eq!(
            responsive_capped_tier(PmTag, true),
            Owner,
            "a responsive owner is never PM-tagged"
        );
        assert_eq!(
            responsive_capped_tier(Reassign, true),
            Owner,
            "a responsive owner is never reassigned"
        );
        // A silent owner (ack not fresher) escalates normally.
        assert_eq!(responsive_capped_tier(PmTag, false), PmTag);
        assert_eq!(responsive_capped_tier(Reassign, false), Reassign);
        assert_eq!(responsive_capped_tier(Owner, true), Owner);
    }

    #[test]
    fn task_latest_activity_age_secs_is_the_freshest_of_updated_at_and_any_comment() {
        use time::{Duration, format_description::well_known::Rfc3339};
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
        assert!(
            age < 3600,
            "the 20m-old comment wins over the 10h-old updated_at, got {age}"
        );

        // Every comment older than updated_at: updated_at (the most recent real event) wins.
        let stale_comments = serde_json::json!({
            "updated_at": stamp(Duration::minutes(5)),
            "comments": [{ "author": "someone", "created_at": stamp(Duration::hours(4)) }],
        });
        let age = task_latest_activity_age_secs(&stale_comments, now).unwrap();
        assert!(
            age < 600,
            "updated_at (5m old) beats an older comment, got {age}"
        );
    }

    #[test]
    fn task_last_nudge_age_secs_only_counts_this_daemons_own_comments() {
        use time::{Duration, format_description::well_known::Rfc3339};
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
        assert!(
            (age - 3600).abs() < 2,
            "picks the MOST RECENT own nudge (1h), not the older one, got {age}"
        );
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
    fn open_children_rollup_lists_only_open_children_with_status_and_blocker() {
        // task_859 facet 1: open children (not done/cancelled) are listed with status + blocked_on; done and
        // cancelled children are excluded; a task with no children yields an empty rollup.
        let parent = serde_json::json!({"children": [
            {"id": 816, "status": "done", "title": "designed"},
            {"id": 841, "status": "in_progress", "title": "authoring"},
            {"id": 843, "status": "todo", "title": "materialize"},
            {"id": 845, "status": "cancelled", "title": "dropped"},
            {"id": 495, "status": "blocked", "blocked_on_kind": "operator", "blocked_on_ref": "", "title": "install window"},
            {"id": 500, "status": "blocked", "blocked_on_kind": "task", "blocked_on_ref": "task_499", "title": "waits on dep"}
        ]});
        let rollup = open_children_rollup(&parent);
        assert_eq!(rollup.len(), 4, "done + cancelled excluded, 4 open remain");
        assert!(
            rollup
                .iter()
                .any(|l| l == "task_841 [in_progress] authoring")
        );
        assert!(
            rollup
                .iter()
                .any(|l| l == "task_495 [blocked, blocked_on operator] install window")
        );
        assert!(
            rollup
                .iter()
                .any(|l| l == "task_500 [blocked, blocked_on task task_499] waits on dep")
        );
        assert!(
            !rollup.iter().any(|l| l.contains("task_816"))
                && !rollup.iter().any(|l| l.contains("task_845"))
        );
        // No children → empty (a leaf task falls through to the normal nudge).
        assert!(open_children_rollup(&serde_json::json!({"title": "leaf"})).is_empty());
        assert!(open_children_rollup(&serde_json::json!({"children": []})).is_empty());
    }

    #[test]
    fn priority_scaled_threshold_lengthens_low_priority_only() {
        // task_859 facet 2: low priority lengthens the threshold (does not zero it); others keep the base.
        assert_eq!(priority_scaled_threshold(3600, Some("low"), 3), 10800);
        assert_eq!(priority_scaled_threshold(3600, Some("normal"), 3), 3600);
        assert_eq!(priority_scaled_threshold(3600, Some("high"), 3), 3600);
        assert_eq!(priority_scaled_threshold(3600, None, 3), 3600);
        // A scale < 1 is floored to 1 so low-pri never nudges SOONER than base.
        assert_eq!(priority_scaled_threshold(3600, Some("low"), 0), 3600);
    }

    #[test]
    fn parent_rollup_body_lists_the_open_children() {
        let body = parent_rollup_body(
            4.0,
            "v-x",
            7200,
            &[
                "task_495 [blocked, blocked_on operator] install window".to_string(),
                "task_843 [todo] materialize".to_string(),
            ],
        );
        assert!(
            body.contains("no activity of its OWN"),
            "idle is the parent's own, not rolled up from children"
        );
        assert!(body.contains("v-x"));
        assert!(
            body.contains("task_495 [blocked, blocked_on operator]")
                && body.contains("task_843 [todo]")
        );
    }

    #[test]
    fn task_has_worker_activity_needs_a_non_nudge_comment() {
        // A real (non-daemon) comment = work started → a todo with this qualifies for a nudge.
        let planned = serde_json::json!({"comments":[{"author":"board-pm","body":"plan: ..."}]});
        assert!(task_has_worker_activity(&planned));
        // Only the nudge daemon's own comments do NOT count — a bare todo the daemon has never legitimately
        // nudged can't self-qualify (and this avoids a self-sustaining nudge loop).
        let only_nudges =
            serde_json::json!({"comments":[{"author":NUDGE_AUTHOR,"body":"fleet nudge: ..."}]});
        assert!(!task_has_worker_activity(&only_nudges));
        // No comments at all → untouched backlog, not a stall.
        assert!(!task_has_worker_activity(
            &serde_json::json!({"comments":[]})
        ));
        assert!(!task_has_worker_activity(&serde_json::json!({})));
        // A mix (worker + nudge) still counts — the worker comment is present.
        let mixed =
            serde_json::json!({"comments":[{"author":NUDGE_AUTHOR},{"author":"v-runtime"}]});
        assert!(task_has_worker_activity(&mixed));
    }

    #[test]
    fn owner_is_gone_flags_only_an_owner_absent_from_the_roster() {
        let board: std::collections::BTreeSet<String> = ["v-alpha", "v-beta"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        // The declared manifest mirrors the board here (the steady-state fleet).
        let declared = board.clone();
        // A registered owner is present regardless of its status (offline is deliberate/resumable, board-pm
        // seq-7848) → NOT gone, so its stale task stays with it.
        assert!(!owner_is_gone("v-alpha", &board, &declared));
        assert!(!owner_is_gone("v-beta", &board, &declared));
        // An owner in NEITHER the board roster NOR the declared manifest (truly retired/removed) → gone → its
        // stale task is orphaned and routes.
        assert!(owner_is_gone("v-retired", &board, &declared));
    }

    #[test]
    fn owner_is_gone_keeps_a_deregistered_but_declared_owner_parked() {
        // task_1032: a mass spin-down can drop an agent's BOARD record while its registry.json entry survives
        // (parked-resumable). Board-roster absence alone would falsely orphan it; declared-manifest membership
        // is the resume-vs-retire marker, so a deregistered-but-declared owner is NOT gone — its task stays.
        let board: std::collections::BTreeSet<String> =
            ["v-still-up"].iter().map(|s| s.to_string()).collect();
        let declared: std::collections::BTreeSet<String> = ["v-still-up", "v-parked"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        // v-parked fell off the board (deregistered) but is still a declared fleet member → spun-down-resumable.
        assert!(!board.contains("v-parked") && declared.contains("v-parked"));
        assert!(
            !owner_is_gone("v-parked", &board, &declared),
            "a declared-but-deregistered owner must not reroute"
        );
        // Only an owner absent from BOTH is a true orphan.
        assert!(owner_is_gone("v-retired", &board, &declared));
    }

    #[test]
    fn fleet_is_quiesced_only_within_the_ttl() {
        let ttl = (FLEET_QUIESCE_TTL_HOURS * 3600.0) as i64;
        // No sentinel → the normal running fleet → not quiesced.
        assert!(!fleet_is_quiesced(None, ttl));
        // Sentinel present and fresh → quiesced (suppress nudges + routes).
        assert!(fleet_is_quiesced(Some(0), ttl));
        assert!(fleet_is_quiesced(Some(ttl - 1), ttl));
        // Sentinel aged past the TTL → self-expired → NOT quiesced (a crashed cutover must not mute forever).
        assert!(!fleet_is_quiesced(Some(ttl), ttl));
        assert!(!fleet_is_quiesced(Some(ttl + 10_000), ttl));
    }

    #[test]
    fn quiesce_status_line_reports_off_on_and_expired() {
        let ttl = (FLEET_QUIESCE_TTL_HOURS * 3600.0) as i64;
        assert!(quiesce_status_line(None, ttl).contains("OFF"));
        // Fresh sentinel → ON, names the TTL-remaining.
        let on = quiesce_status_line(Some(3600), ttl);
        assert!(on.contains("ON") && on.contains("until TTL"));
        // Past the TTL → EXPIRED (still on disk, flagged for cleanup).
        assert!(quiesce_status_line(Some(ttl + 1), ttl).contains("EXPIRED"));
    }

    #[test]
    fn watchdog_action_gated_by_flag_and_quiesce() {
        // task_1032: the opt-in auto-action runs only when its flag is set AND the fleet is not quiesced.
        assert!(
            watchdog_action_enabled(true, false),
            "flag set, not quiesced -> act"
        );
        // Quiesce (mass spin-down) stands the action down even when the flag is set — reviving would fight the
        // operator spin-down; a hire-signal post is a board write mid-freeze.
        assert!(
            !watchdog_action_enabled(true, true),
            "flag set but quiesced -> suppressed"
        );
        // The flag unset is a no-op regardless of quiesce.
        assert!(!watchdog_action_enabled(false, false));
        assert!(!watchdog_action_enabled(false, true));
    }

    #[test]
    fn spin_up_hold_reason_holds_unlaunchable_agents_by_default() {
        let host = "dev-dsk-a";
        // A plain this-host-pinned native agent (or unpinned) is launchable → no hold.
        assert_eq!(
            spin_up_hold_reason(Some(&serde_json::json!({"host": host})), host),
            None
        );
        assert_eq!(
            spin_up_hold_reason(Some(&serde_json::json!({})), host),
            None
        );
        assert_eq!(spin_up_hold_reason(None, host), None);
        // Staged reserve helper (board-core-helper) → HELD. This is the task_1037 mistaken-launch case.
        assert!(
            spin_up_hold_reason(Some(&serde_json::json!({"staged": true})), host)
                .is_some_and(|r| r.contains("staged"))
        );
        // Pinned to another box (green-machine-ops) → HELD off-host.
        assert!(
            spin_up_hold_reason(Some(&serde_json::json!({"host": "green"})), host)
                .is_some_and(|r| r.contains("off-host"))
        );
        // Launch-gated (frank) → HELD, via either marker shape.
        assert!(
            spin_up_hold_reason(
                Some(&serde_json::json!({"host": host, "launch_gated": true})),
                host
            )
            .is_some_and(|r| r.contains("launch-gated"))
        );
        assert!(spin_up_hold_reason(
            Some(&serde_json::json!({"host": host, "launch_gated_on": "daemon mention-wake wiring"})),
            host
        )
        .is_some_and(|r| r.contains("launch-gated")));
        // Charter-deferred vertical (v-cas-http / v-bach) → HELD.
        assert!(
            spin_up_hold_reason(
                Some(&serde_json::json!({"host": host, "charter_projection": "deferred"})),
                host
            )
            .is_some_and(|r| r.contains("deferred"))
        );
        // Staged wins the report order even when other markers also apply (common reserve case first).
        assert!(
            spin_up_hold_reason(
                Some(&serde_json::json!({"staged": true, "host": "green"})),
                host
            )
            .is_some_and(|r| r.contains("staged"))
        );
    }

    #[test]
    fn relaunch_missing_candidate_only_fires_for_a_crashed_should_be_up_launchable_agent() {
        // task_1152: relaunch ONLY a managed agent that should be up but lost its window.
        // Crashed/vanished — no window, not offline, launchable -> relaunch.
        assert!(relaunch_missing_candidate(false, false, None));
        // Has a live window -> running, nothing to do.
        assert!(!relaunch_missing_candidate(true, false, None));
        // Deliberately stood down (offline) + windowless -> left down, never relaunched (the membrain-core /
        // spin-down --all wind-down case).
        assert!(!relaunch_missing_candidate(false, true, None));
        // Held out of auto-launch (staged / off-host / launch-gated / charter-deferred) -> never relaunched,
        // even windowless-and-not-offline.
        assert!(!relaunch_missing_candidate(
            false,
            false,
            Some("staged (reserve helper)")
        ));
        assert!(!relaunch_missing_candidate(
            false,
            false,
            Some("launch-gated (metadata.launch_gated)")
        ));
        // A window present AND offline AND held -> still nothing (all gates independently block).
        assert!(!relaunch_missing_candidate(true, true, Some("off-host")));
    }

    #[test]
    fn launch_gated_and_charter_deferred_markers() {
        assert!(!agent_is_launch_gated(None));
        assert!(!agent_is_launch_gated(Some(
            &serde_json::json!({"launch_gated": false})
        )));
        assert!(!agent_is_launch_gated(Some(
            &serde_json::json!({"launch_gated_on": serde_json::Value::Null})
        )));
        assert!(agent_is_launch_gated(Some(
            &serde_json::json!({"launch_gated": true})
        )));
        assert!(agent_is_launch_gated(Some(
            &serde_json::json!({"launch_gated_on": "x"})
        )));
        assert!(!agent_charter_deferred(None));
        assert!(!agent_charter_deferred(Some(
            &serde_json::json!({"charter_projection": "active"})
        )));
        assert!(agent_charter_deferred(Some(
            &serde_json::json!({"charter_projection": "deferred"})
        )));
    }

    #[test]
    fn launch_metadata_patch_stamps_comms_ack_and_clears_a_set_gate() {
        // task_1178 + task_1144: a launch always stamps comms_norm_ack to the current version (unless already
        // there), and when a bringup gate is set it ALSO clears both gate markers so a now-live agent (frank)
        // is not held out of a reconstitution.
        for set in [
            serde_json::json!({"launch_gated_on": "daemon mention-wake wiring"}),
            serde_json::json!({"launch_gated": true}),
            serde_json::json!({"launch_gated": true, "launch_gated_on": "x"}),
        ] {
            let patch = launch_metadata_patch(Some(&set)).expect("a set gate yields a patch");
            assert_eq!(patch["launch_gated"], serde_json::json!(false));
            assert_eq!(patch["launch_gated_on"], serde_json::Value::Null);
            assert_eq!(
                patch["comms_norm_ack"],
                serde_json::json!(COMMS_NORM_VERSION)
            );
            // Applying the patch (metadata merge) makes the agent read as not-gated.
            let mut merged = set.clone();
            for (k, v) in patch.as_object().unwrap() {
                merged[k] = v.clone();
            }
            assert!(
                !agent_is_launch_gated(Some(&merged)),
                "cleared gate must read as not-gated"
            );
        }
        // No gate, not yet acked: the patch stamps ONLY the comms ack (no gate keys).
        assert_eq!(
            launch_metadata_patch(Some(&serde_json::json!({}))),
            Some(serde_json::json!({"comms_norm_ack": COMMS_NORM_VERSION})),
            "an un-acked agent with no gate is stamped with the ack"
        );
        // No metadata at all is still stamped (an agent record may carry none yet).
        assert_eq!(
            launch_metadata_patch(None),
            Some(serde_json::json!({"comms_norm_ack": COMMS_NORM_VERSION}))
        );
        // Idempotent: already acked at the current version AND no gate set -> no patch (no churn on re-launch).
        assert!(
            launch_metadata_patch(Some(
                &serde_json::json!({"comms_norm_ack": COMMS_NORM_VERSION})
            ))
            .is_none(),
            "a steady-state re-launch of an acked, ungated agent writes nothing"
        );
        // Already acked but a gate is set -> still patches (the gate must clear), keeping the ack.
        let gated_acked = launch_metadata_patch(Some(&serde_json::json!({
            "comms_norm_ack": COMMS_NORM_VERSION, "launch_gated": true
        })))
        .expect("a set gate yields a patch even when already acked");
        assert_eq!(gated_acked["launch_gated"], serde_json::json!(false));
        assert_eq!(
            gated_acked["comms_norm_ack"],
            serde_json::json!(COMMS_NORM_VERSION)
        );
    }

    #[test]
    fn spin_up_all_completes_fanout_only_on_the_final_wave() {
        // No limit → one pass brings everyone up → completes the fan-out → clear the sentinel.
        assert!(spin_up_all_completes_fanout(42, None));
        // Limited wave with MORE down than the limit → partial → leave the sentinel set.
        assert!(!spin_up_all_completes_fanout(10, Some(5)));
        // Limited wave that covers the remaining down candidates → final wave → clear the sentinel.
        assert!(spin_up_all_completes_fanout(5, Some(5)));
        assert!(spin_up_all_completes_fanout(3, Some(5)));
    }

    #[test]
    fn route_body_names_the_router_reason_and_actions() {
        let b = route_body("unassigned", 1.0, 7200);
        assert!(b.contains(NUDGE_ROUTER), "names the router (board-pm)");
        assert!(
            b.contains("unassigned") && b.contains("idle 2h"),
            "carries the reason + idle age"
        );
        assert!(
            b.contains("assign it to a capable agent") && b.contains("update its status"),
            "actionable for the router"
        );
        // The idle-owner reason is carried verbatim too.
        assert!(route_body("owner v-x is idle/dead", 1.0, 3600).contains("owner v-x is idle/dead"));
    }

    #[test]
    fn nudge_body_is_actionable_reassign_or_status() {
        let b = nudge_body(1.0, "v-runtime", 7200);
        // Names the assignee and the idle duration.
        assert!(b.contains("v-runtime") && b.contains("idle 2h"));
        // Spells out the actionable choices the operator asked for (#540), not just "post an update".
        assert!(
            b.contains("progress update or ETA"),
            "keeps the update/ETA option"
        );
        assert!(
            b.contains("reassign"),
            "offers reassignment when the owner can't progress it"
        );
        assert!(
            b.contains("done") && b.contains("blocked with a blocked_on note"),
            "offers the status transitions"
        );
        assert!(
            b.contains("unsure whether it is blocked"),
            "covers the maybe-blocked case"
        );
    }
}
