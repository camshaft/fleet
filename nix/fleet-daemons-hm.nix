# Fleet user daemons as a home-manager module: the single `home-manager switch` path for the flake-managed
# fleet daemon set. home-manager owns install, daemon-reload, enable, and generation reconcile (a unit this
# module stops defining is pruned on the next switch), so a host that imports this module needs no imperative
# install step.
#
# The unit set is the shared data in ./fleet-unit-set.nix (also drives the unit-file text generator in
# ./fleet-daemons.nix). This module maps each record to a structured systemd.user.{service,timer}, so the
# rendered unit is the declarative mirror of the fleet binary's own templates
# (crates/fleet/src/main.rs :: daemon_unit_file / watchdog_unit_files).
#
# Host-specific values the daemon set needs (the full login PATH for spawned sessions, the hub guard-script
# directory, the camshaft/fleet checkout) are module options a per-host config sets, so no machine path is
# baked into the shared module.
{
  config,
  lib,
  pkgs,
  ...
}:
let
  inherit (lib)
    mkOption
    mkEnableOption
    mkIf
    types
    optionalAttrs
    mapAttrsToList
    ;
  cfg = config.services.fleet-daemons;

  # The board env the watchdog / up / dream sweeps run under.
  watchdogEnv = {
    AWS_REGION = cfg.awsRegion;
    AWS_PROFILE = cfg.awsProfile;
    CLAUDE_CODE_USE_BEDROCK = "1";
    ANTHROPIC_DEFAULT_OPUS_MODEL = cfg.opusModel;
  };

  # The dark-binary freshness sweep (task_719) has one definition in the text generator; reuse it so the
  # sweep app is not declared twice.
  generator = import ./fleet-daemons.nix {
    inherit pkgs;
    fleet = cfg.package;
    fleetTunnel = cfg.tunnelPackage;
    proseRuleset = cfg.proseRuleset;
  };

  unitSet = import ./fleet-unit-set.nix {
    fleetBin = "${cfg.package}/bin/fleet";
    fleetTunnelBin = "${cfg.tunnelPackage}/bin/fleet-tunnel";
    sweepBin = "${generator.sweepApp}/bin/fleet-binary-sweep";
    inherit watchdogEnv;
  };

  envList = env: mapAttrsToList (k: v: "${k}=${v}") env;

  # The per-unit environment the imperative installer injected at install time (task_347/_719/_1334/_1123),
  # lifted to declarative per-host options. The watchdog + dream oneshots need the full login PATH for the
  # sessions / board-memory they spawn; the guards read their hub scripts via $FLEET_RT; the binary-sweep and
  # dream units read the camshaft/fleet checkout; the prose-sync-check unit reads the immutable store copy of
  # the ruleset (task_1334). Keyed by the same unit name / guard kind the installer matched on.
  extraEnv =
    u:
    if u.name == "fleet-watchdog" then
      { PATH = cfg.extraPath; }
    else if u.name == "fleet-dream" then
      { PATH = "${cfg.extraPath}:${cfg.fleetRepo}/bin"; }
    else if u.name == "fleet-binary-sweep" then
      { FLEET_REPO = cfg.fleetRepo; }
    else if u.name == "fleet-prose-sync-check" then
      { FLEET_PROSE_RULESET = "${cfg.proseRuleset}"; }
    else if u.kind == "guard" then
      { FLEET_RT = cfg.fleetRt; }
    else
      { };

  # The operator-sensitive guards expose their armed state as an option (the send-keys rearm-stale guard is
  # armed; the slack-bridge-guard stays inert until its single-supervisor flag-day). Every other unit uses the
  # record's own `enabled`.
  isEnabled =
    u:
    if u.name == "fleet-slack-bridge-guard" then
      cfg.slackBridgeGuard.enable
    else if u.name == "fleet-rearm-stale" then
      cfg.rearmStale.enable
    else
      (u.enabled or true);

  unitDesc = u: u.description or "Fleet ${u.name} daemon";

  afterWants =
    u:
    optionalAttrs ((u.after or [ ]) != [ ]) { After = u.after; }
    // optionalAttrs ((u.wants or [ ]) != [ ]) { Wants = u.wants; };

  mergedEnv = u: (u.environment or { }) // extraEnv u;
  envSection = u: optionalAttrs (mergedEnv u != { }) { Environment = envList (mergedEnv u); };

  cadence =
    u:
    if (u.onCalendar or null) != null then
      { OnCalendar = u.onCalendar; }
    else
      {
        OnBootSec = toString (u.onBootSec or 60);
        OnUnitActiveSec = toString u.intervalSecs;
      };

  timerSection = u: cadence u // { Persistent = (u.persistent or true); };

  # A long-running Type=simple daemon enabled into its WantedBy target.
  serviceUnit = u: {
    services."${u.name}" = {
      Unit = {
        Description = unitDesc u;
      }
      // afterWants u;
      Service = {
        Type = "simple";
        ExecStart = u.exec;
        Restart = u.restart or "on-failure";
        RestartSec = toString (u.restartSec or 5);
      }
      // envSection u;
      Install.WantedBy = [ (u.wantedBy or "default.target") ];
    };
  };

  # A timer pair: the oneshot .service (triggered by its timer, so no [Install]) + the .timer (armed via
  # [Install] unless the unit is inert).
  timerUnit = u: {
    services."${u.name}" = {
      Unit = {
        Description = "${unitDesc u} (oneshot)";
      }
      // afterWants u;
      Service = {
        Type = "oneshot";
        ExecStart = u.exec;
      }
      // envSection u
      // optionalAttrs ((u.restartSec or null) != null) {
        Restart = "on-failure";
        RestartSec = toString u.restartSec;
      };
    };
    timers."${u.name}" = {
      Unit.Description = "${unitDesc u} cadence";
      Timer = timerSection u;
      Install.WantedBy = lib.optionals (isEnabled u) [ "timers.target" ];
    };
  };

  # A Group B guard: a hub .claude/fleet/<script> oneshot (fail-clean if the script is not materialized) + a
  # timer. WorkingDirectory=%h matches the hub cron cwd; $FLEET_RT is supplied by extraEnv.
  guardUnit =
    u:
    let
      argSuffix = lib.optionalString ((u.args or "") != "") " ${u.args}";
      execLine = "/bin/sh -c 'test -x \"$FLEET_RT/${u.script}\" && exec bash \"$FLEET_RT/${u.script}\"${argSuffix} || exit 0'";
    in
    {
      services."${u.name}" = {
        Unit.Description = "Fleet guard ${u.name} (oneshot)";
        Service = {
          Type = "oneshot";
          WorkingDirectory = "%h";
          ExecStart = execLine;
        }
        // envSection u;
      };
      timers."${u.name}" = {
        Unit.Description = "Fleet guard ${u.name} cadence";
        Timer = timerSection u;
        Install.WantedBy = lib.optionals (isEnabled u) [ "timers.target" ];
      };
    };

  built = map (
    u:
    if u.kind == "service" then
      serviceUnit u
    else if u.kind == "timer" then
      timerUnit u
    else if u.kind == "guard" then
      guardUnit u
    else
      throw "fleet-daemons-hm: unknown kind '${u.kind}' for ${u.name or "<unnamed>"}"
  ) unitSet;

  mergeKey = key: lib.foldl' (acc: b: acc // (b.${key} or { })) { } built;
in
{
  options.services.fleet-daemons = {
    enable = mkEnableOption "the flake-managed fleet user daemon set";

    package = mkOption {
      type = types.package;
      description = "The fleet binary package (provides bin/fleet).";
    };
    tunnelPackage = mkOption {
      type = types.package;
      description = "The fleet-tunnel binary package (provides bin/fleet-tunnel).";
    };

    extraPath = mkOption {
      type = types.str;
      description = "The full login PATH the watchdog and dream oneshots run their spawned sessions and board-memory under (task_347).";
    };
    fleetRt = mkOption {
      type = types.str;
      description = "The hub .claude/fleet directory the Group B guard oneshots read their scripts from.";
    };
    fleetRepo = mkOption {
      type = types.str;
      description = "The camshaft/fleet checkout path: the binary-sweep git probe and the dream board-memory bin dir read from it.";
    };
    proseRuleset = mkOption {
      type = types.path;
      default = ../crates/fleet/prose-style.toml;
      description = "The committed prose-style ruleset as an immutable store copy, read by prose-sync-check via FLEET_PROSE_RULESET (task_1334). Defaults to the copy in this flake.";
    };

    awsProfile = mkOption {
      type = types.str;
      default = "cline-profile";
      description = "AWS_PROFILE for the watchdog, up, and dream board env.";
    };
    awsRegion = mkOption {
      type = types.str;
      default = "us-west-2";
      description = "AWS_REGION for the board env.";
    };
    opusModel = mkOption {
      type = types.str;
      default = "us.anthropic.claude-opus-4-8[1m]";
      description = "ANTHROPIC_DEFAULT_OPUS_MODEL for the watchdog board env.";
    };

    rearmStale.enable = mkOption {
      type = types.bool;
      default = true;
      description = "Arm the send-keys rearm-stale guard (operator-sensitive). Set false to ship it present-but-inert.";
    };
    slackBridgeGuard.enable = mkOption {
      type = types.bool;
      default = false;
      description = "Arm the slack-bridge-guard supervisor. Flip true only at its flag-day, retiring the cron supervisor in the same move.";
    };
  };

  config = mkIf cfg.enable {
    systemd.user.services = mergeKey "services";
    systemd.user.timers = mergeKey "timers";

    # The agent-window fleet CLI points at this generation's fleet binary; the home-manager generation is a
    # gcroot, so the store path survives a store gc without an explicit indirect root (task_509).
    home.file.".local/bin/fleet".source = "${cfg.package}/bin/fleet";

    # User units must survive logout on the headless host.
    home.activation.fleetLinger = lib.hm.dag.entryAfter [ "writeBoundary" ] ''
      ${pkgs.systemd}/bin/loginctl enable-linger "''${USER:-$(${pkgs.coreutils}/bin/id -un)}" 2>/dev/null || true
    '';
  };
}
