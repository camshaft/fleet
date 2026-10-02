# Fleet daemons as flake-managed USER systemd units (board task #486 / #493 / task_494, operator seq-6441).
#
# The fleet host (dev-desk) is Amazon Linux 2023, NOT NixOS -> there is no nixos-rebuild and no
# NixOS-module `systemd.services`. So this flake BUILDS the fleet binary (packages.<sys>.fleet) and
# GENERATES user systemd unit files whose ExecStart points at that store binary; `install-fleet-daemons`
# installs them into ~/.config/systemd/user/ and RECONCILES -- it prunes flake-managed units the flake no
# longer defines, so the flake OWNS the managed set (seq-6441). Units run as the fleet user (no root);
# enable-linger keeps them running across logout on the headless desk.
#
# The unit shape is the DECLARATIVE MIRROR of the fleet binary's own text templates
# (crates/fleet/src/main.rs :: daemon_unit_file and watchdog_unit_files) -- byte-identical
# [Unit]/[Service]/[Install] sections -- plus a leading `# managed-by:` comment (systemd ignores comment
# lines) so the installer can identify and prune the managed set without a filename convention.
{
  pkgs,
  fleet,
  fleetTunnel,
  lib ? pkgs.lib,
}:
let
  marker = "# managed-by: fleet-flake (nix/fleet-daemons.nix)";

  # Render a unit from a list of lines (plain strings, no leading whitespace) so nix `''`-indent stripping
  # can never corrupt a systemd key. Empty strings are dropped, then joined with newlines + trailing "\n".
  renderUnit = lines: lib.concatStringsSep "\n" (lib.filter (l: l != null) lines) + "\n";

  envLines = env: lib.mapAttrsToList (k: v: "Environment=${k}=${v}") env;

  # Long-running daemon (Type=simple). After/Wants/Restart/WantedBy are per-daemon so each Group A service
  # keeps its exact deployed shape (e.g. notify Restart=always After=default.target; tunnel ordered after
  # notify). Defaults mirror daemon_unit_file (network-online ordering, Restart=on-failure, default.target).
  mkService =
    {
      name,
      exec,
      description ? "Fleet ${name} daemon",
      restart ? "on-failure",
      restartSec ? 5,
      environment ? { },
      after ? [ "network-online.target" ],
      wants ? [ "network-online.target" ],
      wantedBy ? "default.target",
    }:
    {
      "${name}.service" = renderUnit (
        [
          marker
          "[Unit]"
          "Description=${description}"
        ]
        ++ (map (a: "After=${a}") after)
        ++ (map (w: "Wants=${w}") wants)
        ++ [
          ""
          "[Service]"
          "Type=simple"
        ]
        ++ (envLines environment)
        ++ [
          "ExecStart=${exec}"
          "Restart=${restart}"
          "RestartSec=${toString restartSec}"
          ""
          "[Install]"
          "WantedBy=${wantedBy}"
        ]
      );
    };

  # Periodic guard -- mirrors watchdog_unit_files: an oneshot .service + a .timer pair. Description is
  # generalized; After/Wants are optional; the oneshot may carry Restart=on-failure (the nudge pilot wants
  # it, the watchdog template omits it -- both are valid, the timer re-fires regardless).
  mkTimer =
    {
      name,
      description,
      exec,
      intervalSecs,
      onBootSec ? 60,
      persistent ? true,
      restartSec ? null,
      environment ? { },
      after ? [ ],
      wants ? [ ],
    }:
    {
      "${name}.service" = renderUnit (
        [
          marker
          "[Unit]"
          "Description=${description} (oneshot)"
        ]
        ++ (map (a: "After=${a}") after)
        ++ (map (w: "Wants=${w}") wants)
        ++ [
          ""
          "[Service]"
          "Type=oneshot"
        ]
        ++ (envLines environment)
        ++ [ "ExecStart=${exec}" ]
        ++ (lib.optionals (restartSec != null) [
          "Restart=on-failure"
          "RestartSec=${toString restartSec}"
        ])
      );
      "${name}.timer" = renderUnit [
        marker
        "[Unit]"
        "Description=${description} cadence"
        ""
        "[Timer]"
        "OnBootSec=${toString onBootSec}"
        "OnUnitActiveSec=${toString intervalSecs}"
        "Persistent=${if persistent then "true" else "false"}"
        ""
        "[Install]"
        "WantedBy=timers.target"
      ];
    };

  # Group B cron guard (task_495): a cadenza hub .claude/fleet/<script> run on a timer, migrated from the
  # crontab. Unit-only -- the flake owns the timer+oneshot, the script stays hub-materialized (change a guard =
  # cadenza merge + materialize, no flake rebuild). The hub dir is NOT under $HOME, so %h cannot reach it;
  # install-fleet-daemons resolves FLEET_HUB the same way the materialize script does and injects
  # Environment=FLEET_RT=<hub>/.claude/fleet into each guard unit at install time (not git-frozen). The oneshot
  # is FAIL-CLEAN: an un-materialized script exits 0 and the timer retries next interval, never wedging. args /
  # environment are the exact crontab invocation; WorkingDirectory=%h matches the cron cwd ($HOME).
  mkGuard =
    {
      name,
      script,
      intervalSecs ? null,
      onCalendar ? null,
      args ? "",
      onBootSec ? 60,
      persistent ? true,
      environment ? { },
      enabled ? true,
    }:
    let
      argSuffix = lib.optionalString (args != "") " ${args}";
      # A guard fires EITHER on a monotonic interval (OnBootSec + OnUnitActiveSec -- the herd-avoided cadence
      # most guards use, offset from the top-of-hour by activation time) OR at a wall-clock time-of-day
      # (OnCalendar), for the quiet-hours daily guards whose exact slot is deliberate: a plain 24h interval
      # would anchor to whenever install happened and drift the run into daytime fleet activity. Exactly one of
      # intervalSecs / onCalendar is set (asserted below). Persistent=true applies to both: a missed run (box
      # asleep past the scheduled time / interval boundary) fires once on the next wake, never a thundering backlog.
      cadenceLines =
        if onCalendar != null then
          [ "OnCalendar=${onCalendar}" ]
        else
          [
            "OnBootSec=${toString onBootSec}"
            "OnUnitActiveSec=${toString intervalSecs}"
          ];
    in
    assert lib.assertMsg (
      (intervalSecs == null) != (onCalendar == null)
    ) "mkGuard ${name}: set exactly one of intervalSecs or onCalendar";
    {
      "${name}.service" = renderUnit (
        [
          marker
          "[Unit]"
          "Description=Fleet guard ${name} (oneshot)"
          ""
          "[Service]"
          "Type=oneshot"
          "WorkingDirectory=%h"
        ]
        ++ (envLines environment)
        ++ [
          "ExecStart=/bin/sh -c 'test -x \"$FLEET_RT/${script}\" && exec bash \"$FLEET_RT/${script}\"${argSuffix} || exit 0'"
        ]
      );
      # enabled => [Install]/WantedBy arms the timer into timers.target. disabled => OMIT [Install] so the units
      # are present-but-INERT: visible + declarative but started by nothing, the flake analogue of a crontab
      # #DISABLED- line. Flip `enabled` + reinstall to arm/disarm. Used for operator-sensitive send-keys guards
      # (rearm-stale) whose enable/disable state must stay a first-class, greppable, reversible control.
      "${name}.timer" = renderUnit (
        [
          marker
          "[Unit]"
          "Description=Fleet guard ${name} cadence"
          ""
          "[Timer]"
        ]
        ++ cadenceLines
        ++ [
          "Persistent=${if persistent then "true" else "false"}"
        ]
        ++ lib.optionals enabled [
          ""
          "[Install]"
          "WantedBy=timers.target"
        ]
      );
    };

  fleetBin = "${fleet}/bin/fleet";
  fleetTunnelBin = "${fleetTunnel}/bin/fleet-tunnel";

  # The watchdog spawns Claude sessions via window.sh, which need the full known-good login PATH (dropping an
  # entry is the task_347 broken-PATH failure mode) plus this Bedrock env. PATH is intentionally NOT set here:
  # it is a dev-desk-specific absolute path, so freezing it in the flake would bake machine paths into the repo
  # and not track drift. install-fleet-daemons instead captures the live login PATH at install time and injects
  # it into the watchdog unit (see below) -- same full PATH (nothing dropped), no git-frozen machine path.
  watchdogEnv = {
    AWS_REGION = "us-west-2";
    AWS_PROFILE = "cline-profile";
    CLAUDE_CODE_USE_BEDROCK = "1";
    ANTHROPIC_DEFAULT_OPUS_MODEL = "us.anthropic.claude-opus-4-8[1m]";
  };

  # ── The managed daemon set ──────────────────────────────────────────────────────────────────────────
  # Phase 0 (#493): the #478 nudge-stale pilot (timer pair). Phase 1 (task_494): Group A -- the 5 hand-written
  # user daemons migrated to flake-generated units. notify/tunnel/watchdog repoint off the target/release cargo
  # binaries onto the nix store binaries (retire-manual-cargo, task_509-adjacent); the watchdog drops
  # --self-redeploy (an immutable store binary cannot self-rebuild -- activation is now an explicit
  # nix build + install-fleet-daemons, the same model the nudge daemon uses). materialize + litellm are
  # unit-only: ExecStart stays on the external artifacts (a shell script / a litellm install), the flake owns
  # only the unit. (fleet-materialize is on a path to retirement once task_591 moves origin/main-materialize
  # into the fleet binary.)
  units =
    # Phase 0 pilot: nudge-stale (single-sweep oneshot on a 30-min timer; 1h cooldown enforced in-logic).
    (mkTimer {
      name = "fleet-nudge-stale";
      description = "Fleet stale-task nudger (board #478)";
      exec = "${fleetBin} nudge-stale --apply --threshold-hours 1 --cooldown-hours 1";
      intervalSecs = 1800;
      persistent = true;
      restartSec = 5;
    })
    # Group A: fleet-notify (long-running push-wake notifier; other daemons order after it).
    // (mkService {
      name = "fleet-notify";
      description = "Fleet push-wake notifier";
      exec = "${fleetBin} notify --port 8899";
      restart = "always";
      restartSec = 3;
      after = [ "default.target" ];
      wants = [ ];
    })
    # Group A: fleet-tunnel — the ACTIVE event-wake bridge. DO NOT RETIRE: it is NOT subsumed by edge-proxy.
    # It dials the board WS (ws://127.0.0.1:8880/board/tunnel/ws), receives the board's /wake `req` frames, and
    # forwards them to the notifier (http://127.0.0.1:8899); the notifier does NOT connect to the board itself,
    # so this tunnel IS the notifier's board feed. Retiring it (the 2026-10-02 reboot-sweep misstep, briefly in
    # #315) broke fleet-wide event-wake for ~1h until it was restored — see task_1027 (verify-the-wake-path).
    // (mkService {
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
    })
    # Group A: fleet-litellm (long-running local litellm proxy; external binary, unit-only migration).
    // (mkService {
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
    })
    # Group A: fleet-watchdog (periodic oneshot; rearm/observe/spawn; --self-redeploy DROPPED under the flake).
    # --reap-stale-observers (task_1045): the observer cadence SPAWNS obs-<target> windows, and an observer that
    # crashes/hangs before `observe-record` (or whose self-close is missed) leaks its window; without the reaper
    # in the always-on sweep those obs-* windows accumulate unbounded (157 leaked before this landed). The reaper
    # only kills windows whose spawn stamp is stale/absent, so a healthy in-flight observer is never touched.
    // (mkTimer {
      name = "fleet-watchdog";
      description = "Fleet watchdog";
      exec = "${fleetBin} watchdog --rearm --stale-only --observe --spawn --reap-stale-observers";
      intervalSecs = 60;
      onBootSec = 60;
      persistent = true;
      after = [ "fleet-notify.service" ];
      wants = [ "fleet-notify.service" ];
      environment = watchdogEnv;
    })
    # fleet-up (task_937 reboot-survival / task_464 launcher relocation): periodic oneshot that reconstitutes
    # the board-native roster pinned to this host from the BOARD (not the file-hub) — `up-board --launch` skips
    # staged/stood-down/off-host agents, so the fleet comes up shortly after boot and is reconciled every 300s.
    # Mirrors the watchdog's launcher env (watchdogEnv; the full login PATH is injected by install-fleet-daemons
    # at install time, same as the watchdog). This replaces the prior v-fleet-tooling hand-install of fleet-up.
    // (mkTimer {
      name = "fleet-up";
      description = "Fleet launcher — reconstitute board-native agents (up-board)";
      exec = "${fleetBin} up-board --launch";
      intervalSecs = 300;
      onBootSec = 30;
      persistent = true;
      after = [ "fleet-notify.service" ];
      wants = [ "fleet-notify.service" ];
      environment = watchdogEnv;
    })
    # Group A: fleet-materialize (periodic oneshot; external origin/main-materialize script, unit-only).
    // (mkTimer {
      name = "fleet-materialize";
      description = "Fleet origin/main materializer";
      exec = "%h/.config/fleet/fleet-materialize-from-origin-main.sh";
      intervalSecs = 600;
      onBootSec = 60;
      persistent = true;
    })
    # Group B batch 1 (task_495): the fast-interval cron guards migrated to flake timers (hub scripts).
    // (mkGuard {
      name = "fleet-cpu-monitor";
      script = "cpu-monitor.sh";
      intervalSecs = 120;
    })
    // (mkGuard {
      name = "fleet-reap-leases";
      script = "reap-leases.sh";
      intervalSecs = 60;
    })
    // (mkGuard {
      name = "fleet-drain-nudge";
      script = "drain-nudge.sh";
      intervalSecs = 180;
    })
    // (mkGuard {
      name = "fleet-throttle-unleased-nix";
      script = "throttle-unleased-nix.sh";
      args = "--apply";
      intervalSecs = 180;
    })
    # Group B batch 2 (task_495): the mid-interval cron guards migrated to flake timers (hub scripts).
    // (mkGuard {
      name = "fleet-compact-nudge";
      script = "compact-nudge.sh";
      intervalSecs = 300;
    })
    // (mkGuard {
      name = "fleet-slack-bridge-guard";
      script = "slack-bridge-guard.sh";
      intervalSecs = 300;
    })
    // (mkGuard {
      name = "fleet-prune-tmp-inodes";
      script = "prune-tmp-inodes.sh";
      args = "--apply";
      intervalSecs = 900;
      environment = {
        INODE_THRESHOLD_PCT = "0";
      };
    })
    # rearm-stale: SENDS KEYS into agent windows (operator-sensitive, same class as the disabled watchdog). It is
    # LIVE today (cadenza REARM_STALE_ENABLED=true, operator seq 1251), so `enabled` defaults true here --
    # behavior-preserving. If the operator decides the send-keys guard should join the watchdog in disabled-land,
    # flip `enabled = false` + reinstall: the units stay present but inert (no [Install], not armed).
    // (mkGuard {
      name = "fleet-rearm-stale";
      script = "rearm-stale.sh";
      intervalSecs = 240;
      enabled = true;
    })
    # Group B batch 3 (task_495): the mid/long-interval hygiene guards. Their crontab forms used off-minute
    # schedules (warm-keep :17, reap-orphans :7,:37) purely for cron herd-avoidance; a systemd timer's
    # OnBootSec + OnUnitActiveSec is already offset from the top-of-hour by activation time, so a plain
    # interval preserves the cadence + the herd-avoidance intent without needing a specific-minute OnCalendar
    # (which only the fixed-time daily B4 guards actually require -- the 6-hourly one is likewise a plain interval).
    // (mkGuard {
      name = "fleet-disk-guard";
      script = "disk-guard.sh";
      intervalSecs = 900;
    })
    // (mkGuard {
      name = "fleet-aea-refresh";
      script = "aea-refresh.sh";
      intervalSecs = 1800;
    })
    // (mkGuard {
      name = "fleet-warm-keep";
      script = "warm-keep.sh";
      intervalSecs = 3600;
    })
    // (mkGuard {
      name = "fleet-reap-orphans";
      script = "reap-wedged-nix-clients.sh";
      args = "--orphans-only --apply";
      intervalSecs = 1800;
    })
    # Group B batch 4 (task_495): the daily/6-hourly tail guards -- COMPLETES Group B. prune-stale-targets ran
    # `0 */6` (every 6h on the hour), a cadence a plain 6h interval reproduces (herd-avoided by activation offset,
    # same as batch 3). baseline-drift (`23 4`) and oracle-lean (`41 4`) ran at a DELIBERATE early-morning slot
    # in the quiet window; those keep their wall-clock time-of-day via the new OnCalendar param, since a plain 24h
    # interval would anchor to install time and drift the heavy jobs into daytime fleet activity.
    // (mkGuard {
      name = "fleet-prune-stale-targets";
      script = "prune-stale-targets.sh";
      args = "--apply";
      intervalSecs = 21600;
    })
    // (mkGuard {
      name = "fleet-baseline-drift";
      script = "baseline-drift-monitor.sh";
      onCalendar = "*-*-* 04:23:00";
    })
    // (mkGuard {
      name = "fleet-oracle-lean";
      script = "stage-oracle-lean.sh";
      onCalendar = "*-*-* 04:41:00";
    })
    # Binary-freshness sweep (task_719): the Class-2 analogue of fleet-materialize (which keeps the Class-1 hub
    # SHELL scripts fresh). The fleet RUST binary is a pinned package with no auto-cadence, so merged binary items
    # silently accumulate DARK between deploys. This hourly oneshot DETECTS a dark binary (deployed `fleet version`
    # rev vs origin/main, content-diffed over crates/fleet) and NUDGES toward `nix run .#deploy-fleet-binary` --
    # detect-and-nudge only, nothing deployed unattended (the watchdog is cameron's banned-from-unattended
    # component). FLEET_REPO (the camshaft/fleet checkout for the read-only git probe) is injected at install time.
    // (mkTimer {
      name = "fleet-binary-sweep";
      description = "Fleet dark-binary freshness sweep (task_719)";
      exec = "${sweepApp}/bin/fleet-binary-sweep";
      intervalSecs = 3600;
      onBootSec = 300;
      persistent = true;
    });

  unitsDir = pkgs.runCommand "fleet-user-units" { } (
    ''
      mkdir -p "$out"
    ''
    + lib.concatStrings (
      lib.mapAttrsToList (fname: content: ''
        cp ${pkgs.writeText fname content} "$out/${fname}"
      '') units
    )
  );

  serviceUnits = lib.filter (lib.hasSuffix ".service") (lib.attrNames units);
  timerUnits = lib.filter (lib.hasSuffix ".timer") (lib.attrNames units);
  # A long-running service is a .service with NO sibling .timer (a timer's oneshot .service is triggered by the
  # timer, never enabled/restarted directly). The timers get enable --now; the long-running services get
  # enable + restart (so a changed ExecStart takes effect immediately).
  longRunningServices =
    lib.filter (s: !(lib.elem ((lib.removeSuffix ".service" s) + ".timer") timerUnits)) serviceUnits;
  enableUnits = timerUnits ++ longRunningServices;

  # Fleet-BINARY units: the units whose ExecStart runs the fleet / fleet-tunnel binary (notify, tunnel,
  # watchdog, nudge-stale). A fleet-binary REBUILD only needs to refresh these -- the Group B guards run hub
  # shell scripts, materialize runs an external script, litellm runs litellm; none reference the binary. The
  # `deploy-fleet-binary` app (below) hot-swaps the binary for exactly this set, deliberately EXCLUDING the
  # guard timers so a binary deploy can never arm the task_495 guard flag-day against the live crontab (that is
  # install-fleet-daemons' job, at the operator-present cutover).
  fleetBinaryBaseNames = [
    "fleet-notify"
    "fleet-tunnel"
    "fleet-watchdog"
    "fleet-nudge-stale"
    # The binary-freshness sweep (task_719) is included so `deploy-fleet-binary` installs + arms it in the same
    # scoped step (without the Group B guard flag-day). It is not itself a fleet-binary-ExecStart unit; it runs
    # the sweep app, which READS `fleet version` + git to detect a dark binary and nudge.
    "fleet-binary-sweep"
  ];
  isFleetBinaryUnit = fname: lib.any (b: lib.hasPrefix (b + ".") fname) fleetBinaryBaseNames;
  fleetBinaryUnits = lib.filterAttrs (fname: _: isFleetBinaryUnit fname) units;
  fleetBinaryUnitsDir = pkgs.runCommand "fleet-binary-units" { } (
    ''
      mkdir -p "$out"
    ''
    + lib.concatStrings (
      lib.mapAttrsToList (fname: content: ''
        cp ${pkgs.writeText fname content} "$out/${fname}"
      '') fleetBinaryUnits
    )
  );
  fleetBinaryTimers = lib.filter (lib.hasSuffix ".timer") (lib.attrNames fleetBinaryUnits);
  # Long-running binary services = a binary .service with no sibling binary .timer (notify, tunnel). The
  # watchdog/nudge-stale .services are timer oneshots, driven by their timer, not restarted directly.
  fleetBinaryLongRunning = lib.filter (
    s: !(lib.elem ((lib.removeSuffix ".service" s) + ".timer") fleetBinaryTimers)
  ) (lib.filter (lib.hasSuffix ".service") (lib.attrNames fleetBinaryUnits));

  installDeps = [
    pkgs.systemd
    pkgs.coreutils
    pkgs.gnugrep
    pkgs.nix
  ];
  # writeShellApplication prepends `makeBinPath installDeps` to PATH, so strip that known prefix to recover
  # the caller's inherited login PATH (best-effort: if the prefix is absent the PATH is used unchanged -- it
  # still carries the full login PATH, so nothing is ever dropped per task_347).
  binPathPrefix = lib.makeBinPath installDeps;

  installApp = pkgs.writeShellApplication {
    name = "install-fleet-daemons";
    runtimeInputs = installDeps;
    text = ''
      set -euo pipefail
      UNIT_DIR="''${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user"
      mkdir -p "$UNIT_DIR"
      # User units must survive logout on the headless dev-desk.
      loginctl enable-linger "''${USER:-$(id -un)}" 2>/dev/null || true
      # RECONCILE (the flake OWNS the managed set): prune previously-flake-managed units no longer defined.
      shopt -s nullglob
      for f in "$UNIT_DIR"/*.service "$UNIT_DIR"/*.timer; do
        if grep -qxF '${marker}' "$f"; then
          bn="$(basename "$f")"
          if [ ! -e "${unitsDir}/$bn" ]; then
            echo "prune stale flake-managed unit: $bn"
            systemctl --user disable --now "$bn" 2>/dev/null || true
            rm -f "$f"
          fi
        fi
      done
      # Install/refresh the current managed set.
      cp -f ${unitsDir}/* "$UNIT_DIR"/
      # task_494: inject the live login PATH into the watchdog unit at INSTALL time (not git-frozen) so the
      # spawned Claude sessions get the full known-good PATH (task_347) without baking machine paths into the
      # flake, and so it re-captures on each install (tracks drift). Recover the login PATH by stripping the
      # writeShellApplication-prepended nix prefix. (Run install-fleet-daemons from a full login PATH.)
      login_path="$PATH"
      case "$login_path" in
        '${binPathPrefix}':*) login_path="''${login_path#'${binPathPrefix}':}" ;;
      esac
      if [ -e "$UNIT_DIR/fleet-watchdog.service" ]; then
        chmod u+w "$UNIT_DIR/fleet-watchdog.service" 2>/dev/null || true
        printf 'Environment=PATH=%s\n' "$login_path" >> "$UNIT_DIR/fleet-watchdog.service"
      fi
      # task_495: resolve the cadenza hub the SAME way the materialize script does (FLEET_HUB or its default)
      # and inject Environment=FLEET_RT=<hub>/.claude/fleet into every Group B guard unit that references it, so
      # the guard oneshots run the hub-materialized scripts. The hub is not under $HOME (so %h cannot reach it);
      # this is an install-time injection, not a git-frozen machine path.
      fleet_rt="''${FLEET_HUB:-/local/home/bythewc/Projects/camshaft/cadenza}/.claude/fleet"
      for f in "$UNIT_DIR"/*.service; do
        if grep -q 'FLEET_RT' "$f"; then
          chmod u+w "$f" 2>/dev/null || true
          printf 'Environment=FLEET_RT=%s\n' "$fleet_rt" >> "$f"
        fi
      done
      # task_719: inject the camshaft/fleet checkout path into the binary-sweep unit (read-only git probe). The
      # repo is not under $HOME, so this is an install-time injection, not a git-frozen machine path (same pattern
      # as FLEET_RT). FLEET_REPO overrides the default.
      fleet_repo="''${FLEET_REPO:-/local/home/bythewc/Projects/camshaft/fleet}"
      if [ -e "$UNIT_DIR/fleet-binary-sweep.service" ]; then
        chmod u+w "$UNIT_DIR/fleet-binary-sweep.service" 2>/dev/null || true
        printf 'Environment=FLEET_REPO=%s\n' "$fleet_repo" >> "$UNIT_DIR/fleet-binary-sweep.service"
      fi
      systemctl --user daemon-reload
      # Enable + start the timers (each oneshot .service is triggered by its timer, so it picks up a changed
      # ExecStart on its next fire -- no restart needed here). A guard migrated with `enabled = false` renders a
      # timer with NO [Install] section (present-but-inert, the flake analogue of a #DISABLED- crontab line); for
      # those, DON'T arm -- stop + disable so a prior-armed state is cleanly removed on a flip to disabled.
      ${lib.concatStrings (map (u: ''
        if grep -q '^\[Install\]' "$UNIT_DIR/${u}"; then
          systemctl --user enable --now "${u}"
        else
          systemctl --user stop "${u}" 2>/dev/null || true
          systemctl --user disable "${u}" 2>/dev/null || true
          echo "inert (enabled=false) timer, not armed: ${u}"
        fi
      '') timerUnits)}
      # Enable + RESTART each long-running service so a changed ExecStart (e.g. the repoint onto a new store
      # binary) actually takes effect now: `enable --now` does NOT restart an already-running unit, which would
      # leave it on the old binary. restart also starts it if it was stopped.
      ${lib.concatStrings (map (u: ''
        systemctl --user enable "${u}"
        systemctl --user restart "${u}"
      '') longRunningServices)}
      # task_509: put the fleet binary on ~/.local/bin (which agent windows inherit) so no agent runs cargo to
      # reach the fleet CLI. Pin it with an --indirect gcroot so `nix store gc` cannot delete the target, and
      # re-point the stable ~/.local/bin symlink so a rebuilt fleet swaps in live with no window restart. This
      # supersedes the hub refresh-tools.sh shim (v-fleet-tooling cedes ~/.local/bin/fleet in the cutover).
      state_dir="''${XDG_STATE_HOME:-$HOME/.local/state}/fleet"
      mkdir -p "$state_dir" "$HOME/.local/bin"
      nix-store --realise ${fleet} --add-root "$state_dir/current" --indirect >/dev/null
      ln -sfn "$state_dir/current/bin/fleet" "$HOME/.local/bin/fleet"
      echo "install-fleet-daemons: installed ${toString (lib.length (lib.attrNames units))} unit file(s); enabled: ${lib.concatStringsSep " " enableUnits}"
      echo "install-fleet-daemons: fleet on PATH -> $HOME/.local/bin/fleet -> $state_dir/current/bin/fleet (gcroot)"
    '';
  };

  # task_717: hot-swap the fleet BINARY for the running services WITHOUT the full reconcile/cutover. Rebuilds
  # fleet (so ExecStart points at the new store path), refreshes + restarts ONLY the fleet-binary units
  # (notify/tunnel/watchdog/nudge-stale) with the same task_347 login-PATH injection install-fleet-daemons uses,
  # and re-pins ~/.local/bin/fleet. It does NOT touch the Group B guard timers, materialize, or litellm, and
  # does NOT prune -- so a fleet-binary deploy can never prematurely arm the task_495 guard flag-day against the
  # live crontab. (The full cutover -- guard timers + crontab retirement -- stays install-fleet-daemons.)
  deployBinaryApp = pkgs.writeShellApplication {
    name = "deploy-fleet-binary";
    runtimeInputs = installDeps;
    text = ''
      set -euo pipefail
      UNIT_DIR="''${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user"
      mkdir -p "$UNIT_DIR"
      loginctl enable-linger "''${USER:-$(id -un)}" 2>/dev/null || true
      # Refresh ONLY the fleet-binary units (notify/tunnel/watchdog/nudge-stale). Guards + materialize + litellm
      # are left untouched; this deliberately does not arm the Group B guard timers (task_495 operator flag-day).
      cp -f ${fleetBinaryUnitsDir}/* "$UNIT_DIR"/
      # task_347 login-PATH injection on the watchdog (identical to install-fleet-daemons -- the spawned Claude
      # sessions need the full known-good PATH). Recover the login PATH by stripping the writeShellApplication prefix.
      login_path="$PATH"
      case "$login_path" in
        '${binPathPrefix}':*) login_path="''${login_path#'${binPathPrefix}':}" ;;
      esac
      if [ -e "$UNIT_DIR/fleet-watchdog.service" ]; then
        chmod u+w "$UNIT_DIR/fleet-watchdog.service" 2>/dev/null || true
        printf 'Environment=PATH=%s\n' "$login_path" >> "$UNIT_DIR/fleet-watchdog.service"
      fi
      # task_719: inject the camshaft/fleet checkout path into the binary-sweep unit (same as install-fleet-daemons;
      # the sweep is in this scoped set so it installs without the Group B guard flag-day).
      fleet_repo="''${FLEET_REPO:-/local/home/bythewc/Projects/camshaft/fleet}"
      if [ -e "$UNIT_DIR/fleet-binary-sweep.service" ]; then
        chmod u+w "$UNIT_DIR/fleet-binary-sweep.service" 2>/dev/null || true
        printf 'Environment=FLEET_REPO=%s\n' "$fleet_repo" >> "$UNIT_DIR/fleet-binary-sweep.service"
      fi
      systemctl --user daemon-reload
      # Re-arm the binary timers: their oneshot ExecStart now points at the new store binary, so the next fire
      # runs it. enable --now is idempotent for an already-armed timer.
      ${lib.concatStrings (map (u: ''
        systemctl --user enable --now "${u}"
      '') fleetBinaryTimers)}
      # Restart the long-running binary services so the new ExecStart takes effect now (enable --now does NOT
      # restart an already-running unit, which would leave it on the old binary).
      ${lib.concatStrings (map (u: ''
        systemctl --user enable "${u}"
        systemctl --user restart "${u}"
      '') fleetBinaryLongRunning)}
      # Re-pin ~/.local/bin/fleet (the agent-window CLI) + the gcroot onto the new store binary (task_509).
      state_dir="''${XDG_STATE_HOME:-$HOME/.local/state}/fleet"
      mkdir -p "$state_dir" "$HOME/.local/bin"
      nix-store --realise ${fleet} --add-root "$state_dir/current" --indirect >/dev/null
      ln -sfn "$state_dir/current/bin/fleet" "$HOME/.local/bin/fleet"
      echo "deploy-fleet-binary: refreshed ${toString (lib.length (lib.attrNames fleetBinaryUnits))} fleet-binary unit(s); restarted/re-armed: ${lib.concatStringsSep " " (fleetBinaryTimers ++ fleetBinaryLongRunning)}"
      echo "deploy-fleet-binary: fleet -> $HOME/.local/bin/fleet -> $state_dir/current/bin/fleet (gcroot); guards + crontab + materialize + litellm untouched"
    '';
  };

  # task_719: the dark-fleet-binary freshness sweep (DETECT-AND-NUDGE, concierge-greenlit flavor b). Run hourly
  # by fleet-binary-sweep.timer. It reuses the binary's own `fleet version` (baked FLEET_BUILD_REV) and
  # content-diffs that rev against origin/main over the fleet-binary source paths (crates/fleet + Cargo.lock) --
  # a content diff (not SHA/ancestor) so a squash-merged deploy rev correctly reads as up-to-date. On a dark
  # binary it NUDGES concierge (who relays) toward `nix run .#deploy-fleet-binary`; it NEVER deploys unattended
  # (the watchdog is cameron's banned-from-unattended component). Fail-SAFE throughout: any probe it cannot
  # complete (no checkout, fetch failure, pruned rev) errs toward surfacing/retry, never a false "all current".
  sweepApp = pkgs.writeShellApplication {
    name = "fleet-binary-sweep";
    runtimeInputs = [
      pkgs.git
      pkgs.coreutils
      pkgs.gnugrep
    ];
    text = ''
      set -euo pipefail
      # FLEET_REPO (the camshaft/fleet checkout) is injected into this unit at install time; the fleet CLI is the
      # deployed symlink (whose `version` reports what is actually RUNNING). Both overridable for a manual run.
      repo="''${FLEET_REPO:-/local/home/bythewc/Projects/camshaft/fleet}"
      fleet_bin="''${FLEET_BIN:-$HOME/.local/bin/fleet}"
      state_dir="''${XDG_STATE_HOME:-$HOME/.local/state}/fleet"
      mkdir -p "$state_dir"
      nudged_marker="$state_dir/binary-sweep-last-nudged-main"

      [ -d "$repo/.git" ] || { echo "fleet-binary-sweep: no fleet checkout at $repo; skip"; exit 0; }
      [ -x "$fleet_bin" ] || { echo "fleet-binary-sweep: no fleet binary at $fleet_bin; skip"; exit 0; }

      # Deployed binary's baked build rev (`fleet version` prints: `fleet <ver> (rev <hex>)`).
      deployed_rev="$("$fleet_bin" version 2>/dev/null | grep -oE '[0-9a-f]{7,40}' | head -n1 || true)"
      [ -n "$deployed_rev" ] || { echo "fleet-binary-sweep: could not read deployed rev; skip"; exit 0; }

      # Latest origin/main (read-only fetch; never touches worktrees or local branches).
      git -C "$repo" fetch --quiet origin main 2>/dev/null || { echo "fleet-binary-sweep: fetch failed; skip"; exit 0; }
      main_sha="$(git -C "$repo" rev-parse FETCH_HEAD)"

      # The fleet binary's source = the fleet crate (no intra-workspace path deps) + the lockfile (dep versions).
      # Fail-safe: if the baked rev object is gone (a gc'd feature branch), we cannot prove the binary is current,
      # so treat it as dark and nudge rather than stay silent.
      if git -C "$repo" cat-file -e "''${deployed_rev}^{commit}" 2>/dev/null; then
        changed="$(git -C "$repo" diff --name-only "$deployed_rev" "$main_sha" -- crates/fleet/ Cargo.lock || true)"
        reason="content-diff ''${deployed_rev}..main over crates/fleet + Cargo.lock"
      else
        changed="(deployed rev ''${deployed_rev} not in the checkout -- cannot verify freshness)"
        reason="deployed rev object absent"
      fi

      if [ -z "$changed" ]; then
        echo "fleet-binary-sweep: fleet binary current (rev ''${deployed_rev}); no dark binary changes"
        exit 0
      fi

      # Cooldown: nudge once per NEW dark state (don't re-ping while main hasn't advanced). A deploy clears the
      # dark state (deployed_rev updates -> diff empties), so the nudge stops naturally; the marker is harmless.
      if [ -f "$nudged_marker" ] && [ "$(cat "$nudged_marker" 2>/dev/null || true)" = "$main_sha" ]; then
        echo "fleet-binary-sweep: dark binary at main $main_sha already nudged; skip"
        exit 0
      fi

      log="$(git -C "$repo" log --oneline "''${deployed_rev}..$main_sha" -- crates/fleet/ Cargo.lock 2>/dev/null | head -n 20 || true)"
      body_file="$(mktemp)"
      trap 'rm -f "$body_file"' EXIT
      {
        echo "Dark fleet-binary merges detected on camshaft/fleet main ($reason)."
        echo "Deployed binary rev: $deployed_rev"
        echo "Current main:        $main_sha"
        echo ""
        echo "The deployed fleet binary is MISSING these crates/fleet changes:"
        echo "$log"
        echo ""
        echo "Activate (nix-canonical) on the dev-desk: nix run .#deploy-fleet-binary"
        echo "Detect-and-nudge only (task_719) -- nothing is deployed unattended."
      } > "$body_file"

      # Nudge concierge (who relays). Best-effort: on a send failure, do NOT record the marker, so the next sweep
      # retries rather than going silent.
      if "$fleet_bin" send --to concierge --from v-nix --kind note \
           --subject "dark fleet-binary merges pending deploy" --body-file "$body_file" 2>/dev/null; then
        echo "$main_sha" > "$nudged_marker"
        echo "fleet-binary-sweep: nudged concierge about dark binary at main $main_sha"
      else
        echo "fleet-binary-sweep: nudge send failed; will retry next sweep" >&2
        exit 0
      fi
    '';
  };
in
{
  inherit
    units
    unitsDir
    installApp
    enableUnits
    deployBinaryApp
    fleetBinaryUnitsDir
    sweepApp
    ;
}
