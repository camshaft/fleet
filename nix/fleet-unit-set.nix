# The fleet user-daemon set as structured data: the single source of truth consumed by BOTH the unit-file
# text generator (nix/fleet-daemons.nix) and the home-manager module (nix/fleet-daemons-hm.nix). Each record
# is the exact argument set its builder takes, tagged with `kind` (service | timer | guard). ExecStart binary
# paths and the shared watchdog env are passed in so the data carries no store path or host-specific value.
#
# The unit shape is the declarative mirror of the fleet binary's own text templates
# (crates/fleet/src/main.rs :: daemon_unit_file and watchdog_unit_files). A `kind = "service"` record is a
# long-running Type=simple daemon; `kind = "timer"` is an oneshot .service plus a .timer; `kind = "guard"`
# is a cadenza hub .claude/fleet/<script> oneshot plus a .timer (task_495). Field names match the builder
# parameters exactly; a record carries only the fields its inline form set, so builder defaults apply identically.
{
  fleetBin,
  fleetTunnelBin,
  sweepBin,
  watchdogEnv,
}:
[
  # Phase 0 pilot: nudge-stale (single-sweep oneshot on a 30-min timer; 1h cooldown enforced in-logic).
  {
    kind = "timer";
    name = "fleet-nudge-stale";
    description = "Fleet stale-task nudger (board #478)";
    exec = "${fleetBin} nudge-stale --apply --threshold-hours 1 --cooldown-hours 1";
    intervalSecs = 1800;
    persistent = true;
    restartSec = 5;
  }
  # Group A: fleet-notify (long-running push-wake notifier; other daemons order after it).
  {
    kind = "service";
    name = "fleet-notify";
    description = "Fleet push-wake notifier";
    exec = "${fleetBin} notify --port 8899";
    restart = "always";
    restartSec = 3;
    after = [ "default.target" ];
    wants = [ ];
  }
  # Group A: fleet-tunnel — the ACTIVE event-wake bridge. DO NOT RETIRE: it is NOT subsumed by edge-proxy.
  # It dials the board WS (ws://127.0.0.1:8880/board/tunnel/ws), receives the board's /wake `req` frames, and
  # forwards them to the notifier (http://127.0.0.1:8899); the notifier does NOT connect to the board itself,
  # so this tunnel IS the notifier's board feed. Retiring it (the 2026-10-02 reboot-sweep misstep, briefly in
  # #315) broke fleet-wide event-wake for ~1h until it was restored — see task_1027 (verify-the-wake-path).
  {
    kind = "service";
    name = "fleet-tunnel";
    description = "Fleet reverse tunnel bridge (board WS -> notifier :8899 event-wake)";
    exec = "${fleetTunnelBin} --config %h/.config/fleet/tunnel.toml";
    restart = "always";
    restartSec = 5;
    after = [
      "network-online.target"
      "fleet-notify.service"
    ];
    wants = [ "network-online.target" ];
  }
  # Group A: fleet-litellm (long-running local litellm proxy; external binary, unit-only migration).
  {
    kind = "service";
    name = "fleet-litellm";
    description = "Fleet litellm proxy";
    exec = "%h/.fleet-litellm/bin/litellm --config %h/.fleet-litellm/config.yaml --host 127.0.0.1 --port 8098";
    restart = "on-failure";
    restartSec = 3;
    after = [ "network-online.target" ];
    wants = [ ];
    environment = {
      AWS_PROFILE = "cline-profile";
      AWS_REGION = "us-west-2";
    };
  }
  # Group A: fleet-watchdog (periodic oneshot; rearm/observe/spawn; --self-redeploy DROPPED under the flake).
  # --reap-stale-observers (task_1045): the observer cadence SPAWNS obs-<target> windows, and an observer that
  # crashes/hangs before `observe-record` (or whose self-close is missed) leaks its window; without the reaper
  # in the always-on sweep those obs-* windows accumulate unbounded (157 leaked before this landed). The reaper
  # only kills windows whose spawn stamp is stale/absent, so a healthy in-flight observer is never touched.
  # --recover-wedged (task_582, cameron ENABLE): auto-recover an agent stuck in a model-safeguard refusal loop
  # (which keeps last_seen advancing, so the liveness sweep misses it) via a force spin-down + spin-up. Fenced:
  # fires only on a trailing run of stop_reason=refusal turns, per-agent 3600s cooldown, suppressed during a
  # fleet-quiesce. Cameron directed the detect-AND-restart capability (task_582) and approved enabling it here.
  # --review-sweep (task_374 S2c): the automatic adversarial-review trigger — each pass, spawn the per-angle
  # ephemeral reviewers for every review in_review + not yet vetted. Idempotent via the S2b per-angle claim (a
  # review already covered re-spawns nothing), so running it on the 60s cadence is safe; this is what makes the
  # review engine live without a manual `review-spawn`/`review-sweep` invocation.
  # --intake-watch (task_1217 enable, v-fleet-tooling comment_7017): run the project_29 dwell+state intake sweep
  # each pass and ALERT (per-offender comment + one fenced digest to the fleet-intake-watch channel), so an
  # intake task past the ~2min dwell SLA or stuck in_progress/blocked surfaces rather than piling silently. This
  # is alert-only (no destructive auto-action), already blessed by cameron's task framing, so it is a deploy not
  # a fresh operator posture. It reads the intake project from config.intake_project; absent = no-op (zero blast
  # radius), so this flag only goes live once config.intake_project = 29 is set on this host (co-applied with
  # v-nix's home-manager host-config migration). Gated like the sibling acts: board-reachable, quiesce-suppressed,
  # best-effort, logged; its per-offender + per-sweep cooldown stamps make the 60s cadence safe.
  {
    kind = "timer";
    name = "fleet-watchdog";
    description = "Fleet watchdog";
    exec = "${fleetBin} watchdog --rearm --stale-only --observe --spawn --reap-stale-observers --recover-wedged --review-sweep --intake-watch";
    intervalSecs = 60;
    onBootSec = 60;
    persistent = true;
    after = [ "fleet-notify.service" ];
    wants = [ "fleet-notify.service" ];
    environment = watchdogEnv;
  }
  # fleet-up (task_937 reboot-survival / task_464 launcher relocation): periodic oneshot that reconstitutes
  # the board-native roster pinned to this host from the BOARD (not the file-hub) — `up-board --launch` skips
  # staged/stood-down/off-host agents, so the fleet comes up shortly after boot and is reconciled every 300s.
  # Mirrors the watchdog's launcher env (watchdogEnv; the full login PATH is injected by install-fleet-daemons
  # at install time, same as the watchdog). This replaces the prior v-fleet-tooling hand-install of fleet-up.
  {
    kind = "timer";
    name = "fleet-up";
    description = "Fleet launcher — reconstitute board-native agents (up-board)";
    exec = "${fleetBin} up-board --launch";
    intervalSecs = 300;
    onBootSec = 30;
    persistent = true;
    after = [ "fleet-notify.service" ];
    wants = [ "fleet-notify.service" ];
    environment = watchdogEnv;
  }
  # fleet-dream (task_1123): scheduled dreaming — a daily oneshot that runs `fleet dream-run`, which
  # enumerates every repos/* memory scope from the board and runs the dream-analyze+publish pass per scope.
  # OnCalendar at a quiet-hours slot (staggered off the other 04:xx guards) rather than a 24h interval, so the
  # run never drifts into daytime fleet activity; Persistent catches a missed slot on the next wake. The
  # DREAM-NEW notify to the librarian is self-contained in dream-run (board-native post), so this timer stays
  # a bare oneshot with no journal plumbing. watchdogEnv supplies the board env; dream-run also shells out to
  # the `board-memory` CLI (to publish each dreams/<scope> doc) and the model tooling, so install-fleet-daemons
  # injects the login PATH + the fleet repo's bin/ (where board-memory lives) into the rendered service — a
  # machine path, never git-frozen into the flake, same install-time pattern as the watchdog.
  {
    kind = "timer";
    name = "fleet-dream";
    description = "Fleet scheduled dreaming (dream-run over all repo scopes)";
    exec = "${fleetBin} dream-run";
    onCalendar = "*-*-* 04:07:00";
    persistent = true;
    after = [ "fleet-notify.service" ];
    wants = [ "fleet-notify.service" ];
    environment = watchdogEnv;
  }
  # Group A: fleet-materialize (periodic oneshot; external origin/main-materialize script, unit-only).
  {
    kind = "timer";
    name = "fleet-materialize";
    description = "Fleet origin/main materializer";
    exec = "%h/.config/fleet/fleet-materialize-from-origin-main.sh";
    intervalSecs = 600;
    onBootSec = 60;
    persistent = true;
  }
  # Group B batch 1 (task_495): the fast-interval cron guards migrated to flake timers (hub scripts).
  {
    kind = "guard";
    name = "fleet-cpu-monitor";
    script = "cpu-monitor.sh";
    intervalSecs = 120;
  }
  {
    kind = "guard";
    name = "fleet-reap-leases";
    script = "reap-leases.sh";
    intervalSecs = 60;
  }
  {
    kind = "guard";
    name = "fleet-drain-nudge";
    script = "drain-nudge.sh";
    intervalSecs = 180;
  }
  {
    kind = "guard";
    name = "fleet-throttle-unleased-nix";
    script = "throttle-unleased-nix.sh";
    args = "--apply";
    intervalSecs = 180;
  }
  # Group B batch 2 (task_495): the mid-interval cron guards migrated to flake timers (hub scripts).
  {
    kind = "guard";
    name = "fleet-compact-nudge";
    script = "compact-nudge.sh";
    intervalSecs = 300;
  }
  # slack-bridge-guard supervises the single live bridge daemon that carries operator comms, and its
  # single-supervisor invariant is enforced by v-slack-bridge's coordinated flag-day (retire the cron guard +
  # arm this unit in ONE move). Until that flag-day the cron guard stays the sole supervisor, so this timer
  # ships DISABLED: the unit file installs but carries no [Install] and is never armed (the same inert pattern
  # as the disabled watchdog and rearm-stale's toggle above), so install-fleet-daemons brings up the other
  # Group B guards WITHOUT lighting a second bridge supervisor early (task_495, coordinated with v-nix +
  # v-slack-bridge). At the flag-day: flip `enabled = true` + reinstall + retire the cron line, together.
  {
    kind = "guard";
    name = "fleet-slack-bridge-guard";
    script = "slack-bridge-guard.sh";
    intervalSecs = 300;
    enabled = false;
  }
  {
    kind = "guard";
    name = "fleet-prune-tmp-inodes";
    script = "prune-tmp-inodes.sh";
    args = "--apply";
    intervalSecs = 900;
    environment = {
      INODE_THRESHOLD_PCT = "0";
    };
  }
  # rearm-stale: SENDS KEYS into agent windows (operator-sensitive, same class as the disabled watchdog). It is
  # LIVE today (cadenza REARM_STALE_ENABLED=true, operator seq 1251), so `enabled` defaults true here --
  # behavior-preserving. If the operator decides the send-keys guard should join the watchdog in disabled-land,
  # flip `enabled = false` + reinstall: the units stay present but inert (no [Install], not armed).
  {
    kind = "guard";
    name = "fleet-rearm-stale";
    script = "rearm-stale.sh";
    intervalSecs = 240;
    enabled = true;
  }
  # Group B batch 3 (task_495): the mid/long-interval hygiene guards. Their crontab forms used off-minute
  # schedules (warm-keep :17, reap-orphans :7,:37) purely for cron herd-avoidance; a systemd timer's
  # OnBootSec + OnUnitActiveSec is already offset from the top-of-hour by activation time, so a plain
  # interval preserves the cadence + the herd-avoidance intent without needing a specific-minute OnCalendar
  # (which only the fixed-time daily B4 guards actually require -- the 6-hourly one is likewise a plain interval).
  {
    kind = "guard";
    name = "fleet-disk-guard";
    script = "disk-guard.sh";
    intervalSecs = 900;
  }
  {
    kind = "guard";
    name = "fleet-aea-refresh";
    script = "aea-refresh.sh";
    intervalSecs = 1800;
  }
  {
    kind = "guard";
    name = "fleet-warm-keep";
    script = "warm-keep.sh";
    intervalSecs = 3600;
  }
  {
    kind = "guard";
    name = "fleet-reap-orphans";
    script = "reap-wedged-nix-clients.sh";
    args = "--orphans-only --apply";
    intervalSecs = 1800;
  }
  # Group B batch 4 (task_495): the daily/6-hourly tail guards -- COMPLETES Group B. prune-stale-targets ran
  # `0 */6` (every 6h on the hour), a cadence a plain 6h interval reproduces (herd-avoided by activation offset,
  # same as batch 3). baseline-drift (`23 4`) and oracle-lean (`41 4`) ran at a DELIBERATE early-morning slot
  # in the quiet window; those keep their wall-clock time-of-day via the new OnCalendar param, since a plain 24h
  # interval would anchor to install time and drift the heavy jobs into daytime fleet activity.
  {
    kind = "guard";
    name = "fleet-prune-stale-targets";
    script = "prune-stale-targets.sh";
    args = "--apply";
    intervalSecs = 21600;
  }
  {
    kind = "guard";
    name = "fleet-baseline-drift";
    script = "baseline-drift-monitor.sh";
    onCalendar = "*-*-* 04:23:00";
  }
  {
    kind = "guard";
    name = "fleet-oracle-lean";
    script = "stage-oracle-lean.sh";
    onCalendar = "*-*-* 04:41:00";
  }
  # Binary-freshness sweep (task_719): the Class-2 analogue of fleet-materialize (which keeps the Class-1 hub
  # SHELL scripts fresh). The fleet RUST binary is a pinned package with no auto-cadence, so merged binary items
  # silently accumulate DARK between deploys. This hourly oneshot DETECTS a dark binary (deployed `fleet version`
  # rev vs origin/main, content-diffed over crates/fleet) and NUDGES toward `nix run .#deploy-fleet-binary` --
  # detect-and-nudge only, nothing deployed unattended (the watchdog is cameron's banned-from-unattended
  # component). FLEET_REPO (the camshaft/fleet checkout for the read-only git probe) is injected at install time.
  {
    kind = "timer";
    name = "fleet-binary-sweep";
    description = "Fleet dark-binary freshness sweep (task_719)";
    exec = sweepBin;
    intervalSecs = 3600;
    onBootSec = 300;
    persistent = true;
  }
  # Prose-style ruleset drift check (task_1334): the clean-prose lint reads crates/fleet/prose-style.toml, a
  # committed projection of the live board banned-phrases list. Public CI + the pre-commit hook cannot reach the
  # board, so the committed projection can fall behind it. `fleet prose-sync --check` compares the committed
  # ruleset against a fresh board fetch (exit 1 on drift); --alert-on-drift additionally opens or reuses a
  # deduped board task naming the refresh command (ft-hygiene, camshaft/fleet#400), so drift SURFACES rather
  # than passing silently. Runs only where the board is reachable (never public CI). The committed ruleset lives
  # in the repo (not under $HOME), so FLEET_PROSE_RULESET is injected at install time, same pattern as FLEET_REPO.
  {
    kind = "timer";
    name = "fleet-prose-sync-check";
    description = "Fleet prose-style ruleset drift check (task_1334)";
    exec = "${fleetBin} prose-sync --check --alert-on-drift";
    intervalSecs = 21600;
    onBootSec = 600;
    persistent = true;
  }
]
