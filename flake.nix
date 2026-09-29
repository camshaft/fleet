{
  description = "fleet — the standalone multi-repo agent-fleet orchestrator (the `fleet` binary on PATH)";

  # Indirect ref: resolves via the flake registry (on a Determinate host this points at FlakeHub, avoiding
  # the rate-limited GitHub API). `nix build`/`flake check` pins a concrete rev into flake.lock.
  inputs.nixpkgs.url = "nixpkgs";

  outputs =
    { self, nixpkgs }:
    let
      # The fleet host is aarch64-linux; keep the common desktop/CI systems too.
      systems = [
        "aarch64-linux"
        "x86_64-linux"
        "aarch64-darwin"
        "x86_64-darwin"
      ];
      forAllSystems = f: nixpkgs.lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});

      fleetPackage =
        pkgs:
        pkgs.rustPlatform.buildRustPackage {
          pname = "fleet";
          version = "0.0.0";
          src = ./.;
          cargoLock.lockFile = ./Cargo.lock;
          # The crate shells out to tmux/git at RUNTIME (never at build time), so no extra buildInputs are
          # needed to compile. Tests are run in CI / `cargo test`, not under the nix sandbox: some spawn
          # git/tmux which aren't in the build sandbox, so building the artifact does not run them.
          doCheck = false;
          meta = {
            description = "Standalone multi-repo agent-fleet orchestrator";
            mainProgram = "fleet";
          };
        };

      # The slack-bridge daemon (crates/slack-bridge). Its Socket Mode transport binary is
      # `required-features = ["transport"]`-gated, so the `transport` feature is REQUIRED to produce the
      # binary (it pulls the async tree: slack-morphism/tokio/hyper/rustls). The dotfiles slack-bridge role
      # (#153) consumes this as inputs.fleet.packages.${system}.slack-bridge.
      slackBridgePackage =
        pkgs:
        pkgs.rustPlatform.buildRustPackage {
          pname = "slack-bridge";
          version = "0.0.0";
          src = ./.;
          cargoLock.lockFile = ./Cargo.lock;
          # Build just the slack-bridge crate, with the transport feature (produces bin/slack-bridge).
          buildAndTestSubdir = "crates/slack-bridge";
          buildFeatures = [ "transport" ];
          # The daemon shells no build-time deps; it talks to Slack + the board over the network at
          # RUNTIME. Tests aren't run under the sandbox (the lib's `cargo test` is the gate).
          doCheck = false;
          meta = {
            description = "Fleet Slack↔board bridge daemon (design #141)";
            mainProgram = "slack-bridge";
          };
        };

      # The fleet-tunnel daemon (crates/fleet-tunnel): a reverse HTTP-over-websocket bridge run on a
      # fleet host — dials OUT to the board so the board can reach the host-local notifier. Its async
      # transport binary is `required-features = ["transport"]`-gated, so the `transport` feature is
      # REQUIRED to produce the binary (it pulls the async tree: tokio/tokio-tungstenite/rustls). The
      # dotfiles fleet-tunnel role consumes this as inputs.fleet.packages.${system}.fleet-tunnel; it
      # also gives the fleet host a pinned binary to run instead of the old Python + uv.
      fleetTunnelPackage =
        pkgs:
        pkgs.rustPlatform.buildRustPackage {
          pname = "fleet-tunnel";
          version = "0.0.0";
          src = ./.;
          cargoLock.lockFile = ./Cargo.lock;
          buildAndTestSubdir = "crates/fleet-tunnel";
          buildFeatures = [ "transport" ];
          # Pure network daemon: no build-time deps, and its tests are the lib's `cargo test` gate,
          # not run under the sandbox.
          doCheck = false;
          meta = {
            description = "Fleet reverse HTTP-over-websocket bridge daemon";
            mainProgram = "fleet-tunnel";
          };
        };
    in
    {
      packages = forAllSystems (pkgs: rec {
        fleet = fleetPackage pkgs;
        slack-bridge = slackBridgePackage pkgs;
        fleet-tunnel = fleetTunnelPackage pkgs;
        default = fleet;
      });

      apps = forAllSystems (
        pkgs:
        let
          fleet = fleetPackage pkgs;
          slackBridge = slackBridgePackage pkgs;
          fleetTunnel = fleetTunnelPackage pkgs;
        in
        {
          fleet = {
            type = "app";
            program = "${fleet}/bin/fleet";
          };
          slack-bridge = {
            type = "app";
            program = "${slackBridge}/bin/slack-bridge";
          };
          fleet-tunnel = {
            type = "app";
            program = "${fleetTunnel}/bin/fleet-tunnel";
          };
          default = {
            type = "app";
            program = "${fleet}/bin/fleet";
          };
        }
      );

      # `nix flake check` only EVALUATES bare `packages` (it reports "build skipped"); a `checks` entry is
      # what it actually builds. Point it at the package so a compile break fails `nix flake check` — the
      # gate CI runs. (The crate's `cargo test` stays the dev/CI test gate; it isn't run here because some
      # tests spawn git/tmux, absent in the build sandbox.)
      checks = forAllSystems (pkgs: {
        fleet = fleetPackage pkgs;
      });

      # `nix develop` — the toolchain to build/lint/test the crate, plus the git/tmux the fleet drives at
      # runtime, so a contributor gets a working environment without a host rust install.
      devShells = forAllSystems (pkgs: {
        default = pkgs.mkShell {
          packages = [
            pkgs.cargo
            pkgs.rustc
            pkgs.clippy
            pkgs.rustfmt
            pkgs.git
            pkgs.tmux
          ];
        };
      });
    };
}
