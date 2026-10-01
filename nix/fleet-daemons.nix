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
      intervalSecs,
      args ? "",
      onBootSec ? 60,
      persistent ? true,
      environment ? { },
      enabled ? true,
    }:
    let
      argSuffix = lib.optionalString (args != "") " ${args}";
    in
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
          "OnBootSec=${toString onBootSec}"
          "OnUnitActiveSec=${toString intervalSecs}"
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
    # Group A: fleet-tunnel (long-running reverse HTTP-over-websocket bridge; ordered after notify).
    // (mkService {
      name = "fleet-tunnel";
      description = "Fleet reverse tunnel bridge";
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
    // (mkTimer {
      name = "fleet-watchdog";
      description = "Fleet watchdog";
      exec = "${fleetBin} watchdog --rearm --stale-only --observe --spawn";
      intervalSecs = 60;
      onBootSec = 60;
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
    # (which only the daily/6-hourly B4 guards actually require).
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
in
{
  inherit units unitsDir installApp enableUnits;
}
