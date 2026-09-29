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

      # The voice-assistant daemon (crates/voice-assistant): a local voice loop (custom wake phrase → STT →
      # Claude+MCP → TTS). Its audio+ML shell is `required-features = ["runtime"]`-gated, so the `runtime`
      # feature is REQUIRED to produce the binary (it pulls sherpa-onnx for STT/TTS/wake + cpal for audio).
      # The dotfiles voice-assistant role consumes this as inputs.fleet.packages.${system}.voice-assistant.
      #
      # Unlike the other crates, this one links NATIVE libraries at build time:
      #   • sherpa-onnx-sys — with the crate's `shared` feature it links a prebuilt libsherpa-onnx. The
      #     build script normally DOWNLOADS a CPU archive, which the hermetic nix sandbox forbids, so the
      #     build must be pointed at a lib via SHERPA_ONNX_LIB_DIR. On green-machine that's the CUDA 11.8
      #     GPU build (runs on the Pascal 1080 Ti); CUDA itself is exposed at RUNTIME via LD_LIBRARY_PATH
      #     from the dotfiles role (the knowledge-base.nix pattern), not linked here.
      #   • cpal — needs ALSA headers/libs (alsa-lib) to build.
      # `sherpaOnnxLib` is the derivation providing $out/lib/libsherpa-onnx-c-api.so; on green-machine it is
      # overridden to the GPU archive. Left as null here so a non-green host still evaluates (the package
      # only builds where the lib is supplied) — this is why voice-assistant is NOT in `checks` below: the
      # native/GPU build is not hermetic on arbitrary CI.
      voiceAssistantPackage =
        pkgs:
        { sherpaOnnxLib ? null }:
        pkgs.rustPlatform.buildRustPackage {
          pname = "voice-assistant";
          version = "0.0.0";
          src = ./.;
          cargoLock.lockFile = ./Cargo.lock;
          buildAndTestSubdir = "crates/voice-assistant";
          buildFeatures = [ "runtime" ];
          nativeBuildInputs = [
            pkgs.cmake
            pkgs.pkg-config
          ];
          buildInputs = [
            pkgs.alsa-lib
          ] ++ pkgs.lib.optional (sherpaOnnxLib != null) sherpaOnnxLib;
          # Point sherpa-onnx-sys at the prebuilt lib instead of letting it fetch (no network in sandbox).
          SHERPA_ONNX_LIB_DIR = pkgs.lib.optionalString (sherpaOnnxLib != null) "${sherpaOnnxLib}/lib";
          # The daemon shells no build-time deps beyond the native libs; its tests are the pure lib's
          # `cargo test` gate (the default-features build), not run under the sandbox.
          doCheck = false;
          meta = {
            description = "Local voice assistant daemon (wake → STT → Claude → TTS)";
            mainProgram = "voice-assistant";
          };
        };

      # The knowledge-base server (crates/kb): Qdrant vector search + local ONNX embeddings + an MCP tool
      # surface (Python→Rust port, board task #157). Built with the `load-dynamic` feature so ort/onnxruntime
      # is dlopen'd at RUNTIME (via LD_LIBRARY_PATH) rather than downloaded at build time — the default
      # `download-binaries` feature fetches onnxruntime over the network, which the Nix sandbox forbids. The
      # dotfiles kb role puts libonnxruntime.so (+ CUDA libs, for the GPU ingest path) on LD_LIBRARY_PATH.
      # Model files (bge-large / reranker) download on first use into the HF cache at runtime, not here.
      kbPackage =
        pkgs:
        pkgs.rustPlatform.buildRustPackage {
          pname = "kb";
          version = "0.1.0";
          src = ./.;
          cargoLock.lockFile = ./Cargo.lock;
          buildAndTestSubdir = "crates/kb";
          # Drop the default (download-binaries) feature; link onnxruntime dynamically at runtime instead.
          buildNoDefaultFeatures = true;
          buildFeatures = [ "load-dynamic" ];
          # No build-time native deps: onnxruntime is dlopen'd at runtime, Qdrant is reached over HTTP. Tests
          # (`cargo test -p kb`) are the dev/CI gate; several would need a live Qdrant/model, so not run here.
          doCheck = false;
          meta = {
            description = "Knowledge-base server: Qdrant vector search + ONNX embeddings + MCP (task #157)";
            mainProgram = "kb";
          };
        };
    in
    {
      packages = forAllSystems (pkgs: rec {
        fleet = fleetPackage pkgs;
        slack-bridge = slackBridgePackage pkgs;
        fleet-tunnel = fleetTunnelPackage pkgs;
        # Built with no sherpaOnnxLib by default: the derivation evaluates everywhere but only *builds*
        # where the native sherpa lib is supplied (green-machine overrides `sherpaOnnxLib` to the CUDA
        # archive). See voiceAssistantPackage above.
        voice-assistant = voiceAssistantPackage pkgs { };
        kb = kbPackage pkgs;
        default = fleet;
      });

      apps = forAllSystems (
        pkgs:
        let
          fleet = fleetPackage pkgs;
          slackBridge = slackBridgePackage pkgs;
          fleetTunnel = fleetTunnelPackage pkgs;
          voiceAssistant = voiceAssistantPackage pkgs { };
          kb = kbPackage pkgs;
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
          voice-assistant = {
            type = "app";
            program = "${voiceAssistant}/bin/voice-assistant";
          };
          kb = {
            type = "app";
            program = "${kb}/bin/kb";
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
            # For building crates/voice-assistant --features runtime (sherpa-onnx-sys + cpal):
            pkgs.cmake
            pkgs.pkg-config
            pkgs.alsa-lib
          ];
        };
      });
    };
}
