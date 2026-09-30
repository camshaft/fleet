# Fleet daemons as flake-managed USER systemd units (board task #486 / #493, operator seq-6441).
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
  lib ? pkgs.lib,
}:
let
  marker = "# managed-by: fleet-flake (nix/fleet-daemons.nix)";

  # Render a unit from a list of lines (plain strings, no leading whitespace) so nix `''`-indent stripping
  # can never corrupt a systemd key. Empty strings are dropped, then joined with newlines + trailing "\n".
  renderUnit = lines: lib.concatStringsSep "\n" (lib.filter (l: l != null) lines) + "\n";

  envLines = env: lib.mapAttrsToList (k: v: "Environment=${k}=${v}") env;

  # Long-running daemon -- mirrors daemon_unit_file: Type=simple, Restart=on-failure, WantedBy=default.target.
  mkService =
    {
      name,
      exec,
      restartSec ? 5,
      environment ? { },
    }:
    {
      "${name}.service" = renderUnit (
        [
          marker
          "[Unit]"
          "Description=Fleet ${name} daemon"
          "After=network-online.target"
          "Wants=network-online.target"
          ""
          "[Service]"
          "Type=simple"
        ]
        ++ (envLines environment)
        ++ [
          "ExecStart=${exec}"
          "Restart=on-failure"
          "RestartSec=${toString restartSec}"
          ""
          "[Install]"
          "WantedBy=default.target"
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

  # ── The managed daemon set ──────────────────────────────────────────────────────────────────────────
  # Phase 0 (#493): the #478 nudge-stale pilot -- a SINGLE-SWEEP oneshot (enumerate stale assigned in_progress
  # + todo tasks, nudge them, exit) so a timer pair, NOT a long-running service. The cooldown is enforced
  # in-logic from the daemon's own prior comments; a 1h cooldown re-nudges a task each time it re-stales past
  # the 1h threshold (operator #540), and the 30-min timer keeps it responsive without spamming inside the
  # cooldown. Group A daemons + Group B cron conversions land in Phase 1/2 (#494/#495) via mkService/mkTimer.
  units = mkTimer {
    name = "fleet-nudge-stale";
    description = "Fleet stale-task nudger (board #478)";
    exec = "${fleetBin} nudge-stale --apply --threshold-hours 1 --cooldown-hours 1";
    intervalSecs = 1800;
    persistent = true;
    restartSec = 5;
  };

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
