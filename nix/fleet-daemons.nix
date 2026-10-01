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

  fleetBin = "${fleet}/bin/fleet";
  fleetTunnelBin = "${fleetTunnel}/bin/fleet-tunnel";

  # The watchdog spawns Claude sessions via window.sh, which depend on this exact captured known-good PATH
  # and Bedrock env. It is reproduced VERBATIM from the deployed unit (a repoint, not a behavior change) and
  # NOT derived -- deriving a fresh PATH risks dropping an entry a session needs (the task_347 broken-PATH
  # failure mode). FLAG: this freezes a dev-desk-specific PATH in the flake; capturing $PATH at install time
  # is a possible later refinement (owner v-fleet-tooling).
  watchdogEnv = {
    PATH = "/local/home/bythewc/.aim/mcp-servers:/local/home/bythewc/.aim/mcp-servers:/local/home/bythewc/.aim/mcp-servers:/local/home/bythewc/.aim/mcp-servers:/home/bythewc/.cargo/bin:/home/bythewc/.local/bin:/usr/bin:/bin:/local/home/bythewc/.aim/cc-plugins/AmazonBuilderCoreAIAgents-core/bin:/local/home/bythewc/.aim/cc-plugins/AmazonBuilderCoreAIAgents-pipeline-assistant/bin:/local/home/bythewc/.aim/cc-plugins/AmazonBuilderCoreAIAgents-runtime-configuration-assistant/bin:/local/home/bythewc/.aim/cc-plugins/AmazonBuilderCoreAIAgents-core/bin:/local/home/bythewc/.aim/cc-plugins/AmazonBuilderCoreAIAgents-pipeline-assistant/bin:/local/home/bythewc/.aim/cc-plugins/AmazonBuilderCoreAIAgents-runtime-configuration-assistant/bin";
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
  # Enable the .timer of a pair (its oneshot .service is timer-triggered, never enabled directly); enable a
  # standalone long-running .service (one with no sibling .timer).
  enableUnits =
    timerUnits
    ++ lib.filter (s: !(lib.elem ((lib.removeSuffix ".service" s) + ".timer") timerUnits)) serviceUnits;

  installApp = pkgs.writeShellApplication {
    name = "install-fleet-daemons";
    runtimeInputs = [
      pkgs.systemd
      pkgs.coreutils
      pkgs.gnugrep
    ];
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
      # Install/refresh the current managed set, then reload + enable.
      cp -f ${unitsDir}/* "$UNIT_DIR"/
      systemctl --user daemon-reload
      ${lib.concatStrings (map (u: ''
        systemctl --user enable --now "${u}"
      '') enableUnits)}
      echo "install-fleet-daemons: installed ${toString (lib.length (lib.attrNames units))} unit file(s); enabled: ${lib.concatStringsSep " " enableUnits}"
    '';
  };
in
{
  inherit units unitsDir installApp enableUnits;
}
