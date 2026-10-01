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
          # Build ONLY the fleet crate — not the whole workspace. Without this, buildRustPackage compiles
          # every workspace member (incl. voice-assistant → ort-sys/onnxruntime), whose build.rs fetches a
          # binary over the network and thus can NEVER build in the no-network nix sandbox. The fleet
          # orchestrator (spin-up/notify/watchdog/board/transcripts) has no speech deps, so scoping the build
          # to crates/fleet drops ort-sys out of the tree entirely and the package builds hermetically.
          buildAndTestSubdir = "crates/fleet";
          # The crate shells out to tmux/git at RUNTIME (never at build time), so no extra buildInputs are
          # needed to compile. Tests are run in CI / `cargo test`, not under the nix sandbox: some spawn
          # git/tmux which aren't in the build sandbox, so building the artifact does not run them.
          doCheck = false;
          # Bake the build revision into the binary so `fleet version` reports which commit a deployed binary
          # was built from (the hermetic sandbox has no .git, so build.rs can't `git rev-parse` — it reads
          # this env). Diagnoses a stale deployed binary silently running old logic.
          FLEET_BUILD_REV = self.rev or self.dirtyRev or "unknown";
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

      # The github-bridge daemon (crates/github-bridge) — the second bridge adapter over the same board
      # core (design #141, task #136). Its blocking poll-loop binary is `required-features = ["daemon"]`-
      # gated, so the `daemon` feature is REQUIRED to produce the binary (it pulls clap/tracing). The
      # dotfiles github-bridge role (#265) consumes this as inputs.fleet.packages.${system}.github-bridge.
      githubBridgePackage =
        pkgs:
        pkgs.rustPlatform.buildRustPackage {
          pname = "github-bridge";
          version = "0.0.0";
          src = ./.;
          cargoLock.lockFile = ./Cargo.lock;
          # Build just the github-bridge crate, with the daemon feature (produces bin/github-bridge).
          buildAndTestSubdir = "crates/github-bridge";
          buildFeatures = [ "daemon" ];
          # The daemon shells no build-time deps; it talks to GitHub + the board over the network at
          # RUNTIME. Tests aren't run under the sandbox (the lib's `cargo test` is the gate).
          doCheck = false;
          meta = {
            description = "Fleet GitHub↔board bridge daemon (design #141, task #136)";
            mainProgram = "github-bridge";
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

      # The prebuilt sherpa-onnx shared library (v1.13.8, GPU archive), autoPatchelf'd for NixOS.
      # sherpa-onnx-sys links `libsherpa-onnx-c-api.so` + `libonnxruntime.so` at BUILD time; its build
      # script normally DOWNLOADS this archive, which the hermetic nix sandbox forbids — so we fetch the
      # exact pinned archive here and point the build at it via SHERPA_ONNX_LIB_DIR. We use the GPU
      # (CUDA-12.x / cuDNN-9.x) archive so the runtime CUDA provider is present; but note the LINK only
      # needs c-api + onnxruntime — the CUDA provider (libonnxruntime_providers_cuda.so) and TensorRT
      # provider are dlopen'd at RUNTIME, so their CUDA/cuDNN/TensorRT deps are IGNORED at patch time and
      # resolved from LD_LIBRARY_PATH on green-machine (the knowledge-base.nix pattern). That keeps THIS
      # derivation CUDA-free and buildable on any x86_64-linux — no cudaPackages, no unfree, hermetic.
      # Version is pinned to the `sherpa-onnx` crate version (crates/voice-assistant/Cargo.toml).
      sherpaOnnxLib =
        pkgs:
        pkgs.stdenvNoCC.mkDerivation rec {
          pname = "libsherpa-onnx";
          version = "1.13.8";
          src = pkgs.fetchurl {
            url = "https://github.com/k2-fsa/sherpa-onnx/releases/download/v${version}/sherpa-onnx-v${version}-cuda-12.x-cudnn-9.x-onnxruntime1.28.2-linux-x64-gpu.tar.bz2";
            hash = "sha256-ITKvGujITIT4asCR5DEYOSh4UdK/XmYDMuvOOJih4LI=";
          };
          nativeBuildInputs = [ pkgs.autoPatchelfHook ];
          buildInputs = [ pkgs.stdenv.cc.cc.lib ]; # libstdc++/libgcc_s; glibc is implicit
          # CUDA / cuDNN / TensorRT libs are dlopen'd by the ONNX providers at RUNTIME (LD_LIBRARY_PATH on
          # the host), never linked here — so autoPatchelf must not fail on them.
          autoPatchelfIgnoreMissingDeps = [
            "libcudart.so.12"
            "libcublas.so.12"
            "libcublasLt.so.12"
            "libcurand.so.10"
            "libcudnn.so.9"
            "libcuda.so.1"
            "libnvinfer.so.10"
            "libnvinfer_plugin.so.10"
            "libnvonnxparser.so.10"
          ];
          dontConfigure = true;
          dontBuild = true;
          installPhase = ''
            runHook preInstall
            mkdir -p $out
            cp -r lib $out/lib
            runHook postInstall
          '';
          meta.description = "Prebuilt sherpa-onnx ${version} shared libs (GPU archive), patched for NixOS";
        };

      # The voice-assistant daemon (crates/voice-assistant): a local voice loop (custom wake phrase → STT →
      # Claude+MCP → TTS). Its audio+ML shell is `required-features = ["runtime"]`-gated, so the `runtime`
      # feature is REQUIRED to produce the binary (it pulls sherpa-onnx for STT/TTS/wake + cpal for audio).
      # The dotfiles voice-assistant role consumes this as inputs.fleet.packages.${system}.voice-assistant.
      #
      # Native build deps: `sherpaOnnxLib` (above) supplies the prebuilt libsherpa-onnx the `shared`-feature
      # sys crate links (via SHERPA_ONNX_LIB_DIR); cpal needs alsa-lib. This is self-contained + hermetic —
      # CUDA is a pure RUNTIME concern (the dotfiles role puts the CUDA-12 libs + driver on LD_LIBRARY_PATH,
      # exactly as it does for the kb server's onnxruntime-gpu), so nothing GPU is linked here.
      voiceAssistantPackage =
        pkgs:
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
            (sherpaOnnxLib pkgs)
          ];
          # Point sherpa-onnx-sys at the prebuilt lib instead of letting it fetch (no network in sandbox).
          SHERPA_ONNX_LIB_DIR = "${sherpaOnnxLib pkgs}/lib";
          # The daemon shells no build-time deps beyond the native libs; its tests are the pure lib's
          # `cargo test` gate (the default-features build), not run under the sandbox.
          doCheck = false;
          meta = {
            description = "Local voice assistant daemon (wake → STT → Claude → TTS)";
            mainProgram = "voice-assistant";
          };
        };

      # Prebuilt libpdfium.so, pinned to bblanchon pdfium-binaries build 7881 — the EXACT Pdfium release the
      # `kb` crate's pdfium-render 0.9.4 targets (its `pdfium_latest` = `pdfium_7881`). pdfium-render binds
      # every FPDF* symbol EAGERLY at `bind_to_system_library()` time, so a libpdfium older than 7881 fails the
      # WHOLE bind on one missing symbol — e.g. nixpkgs' current `pdfium-binaries` (build 7749) lacks
      # `FPDFTextObj_SetFontSize`, which aborts the KB inbox worker's PDF path entirely (found via #444's smoke
      # test; it is NOT host-specific — dev-dsk's nixpkgs is 7749 too). Shipping the matching build as a fleet
      # output keeps the kb binary + its libpdfium in lockstep on every host, independent of the consuming
      # host's nixpkgs pin (the sherpaOnnxLib pattern). The kb-inbox/uploader roles put `${pdfiumLib pkgs}/lib`
      # on LD_LIBRARY_PATH; pdfium-render dlopens `libpdfium.so` off the loader path. Prebuilt + autoPatchelf'd,
      # so it's hermetic and CUDA-free; bump the build + both hashes in lockstep with the pdfium-render bump.
      pdfiumLib =
        pkgs:
        let
          # bblanchon publishes one archive per platform; pick by the build system's arch (green = x64).
          archive =
            {
              x86_64-linux = {
                suffix = "x64";
                hash = "sha256-FHDiG4tKO0rX+FaE4toR2U87aahtgd7hG5tnCdknrB0=";
              };
              aarch64-linux = {
                suffix = "arm64";
                hash = "sha256-7n97fVRolYM2qBjBzVgL3SCXKEa3N3sT+akj2S0dRnQ=";
              };
            }
            .${pkgs.stdenv.hostPlatform.system}
              or (throw "pdfiumLib: unsupported system ${pkgs.stdenv.hostPlatform.system}");
        in
        pkgs.stdenvNoCC.mkDerivation rec {
          pname = "pdfium";
          version = "7881";
          src = pkgs.fetchurl {
            url = "https://github.com/bblanchon/pdfium-binaries/releases/download/chromium%2F${version}/pdfium-linux-${archive.suffix}.tgz";
            hash = archive.hash;
          };
          # The tarball extracts files at the top level (lib/, LICENSE, VERSION) with no wrapping dir.
          sourceRoot = ".";
          nativeBuildInputs = [ pkgs.autoPatchelfHook ];
          buildInputs = [ pkgs.stdenv.cc.cc.lib ]; # libstdc++/libgcc_s for libpdfium.so; glibc is implicit
          dontConfigure = true;
          dontBuild = true;
          installPhase = ''
            runHook preInstall
            mkdir -p $out
            cp -r lib $out/lib
            install -Dm644 LICENSE $out/share/licenses/pdfium/LICENSE
            runHook postInstall
          '';
          meta.description = "Prebuilt Pdfium build ${version} shared lib (bblanchon), patched for NixOS; matches pdfium-render's pdfium_${version}";
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

      # Fleet daemons as flake-managed USER systemd units (task #486/#493): the declarative mirror of the
      # binary's `fleet daemon-unit`/`watchdog-unit` templates + `install-fleet-daemons` (reconcile + enable).
      # Self-contained — ExecStart points at THIS flake's fleet package, no cross-repo input. See nix/fleet-daemons.nix.
      fleetDaemons = pkgs: import ./nix/fleet-daemons.nix {
        inherit pkgs;
        fleet = fleetPackage pkgs;
        fleetTunnel = fleetTunnelPackage pkgs;
      };
    in
    {
      packages = forAllSystems (pkgs: rec {
        fleet = fleetPackage pkgs;
        slack-bridge = slackBridgePackage pkgs;
        github-bridge = githubBridgePackage pkgs;
        fleet-tunnel = fleetTunnelPackage pkgs;
        # Self-contained + hermetic: bundles its own prebuilt sherpa lib (see sherpaOnnxLib above),
        # CUDA-free at build (CUDA is runtime-only). Builds on any x86_64-linux.
        voice-assistant = voiceAssistantPackage pkgs;
        kb = kbPackage pkgs;
        # Version-locked libpdfium (build 7881) for the KB ingest workers' PDF path — see pdfiumLib above.
        # The kb-inbox/uploader roles consume this as inputs.fleet.packages.${system}.pdfium on LD_LIBRARY_PATH.
        pdfium = pdfiumLib pkgs;
        # The generated USER systemd unit files for the flake-managed daemon set (task #486/#493). Build to
        # inspect the rendered units (nix build .#fleet-user-units); `install-fleet-daemons` (an app) installs them.
        fleet-user-units = (fleetDaemons pkgs).unitsDir;
        default = fleet;
      });

      apps = forAllSystems (
        pkgs:
        let
          fleet = fleetPackage pkgs;
          slackBridge = slackBridgePackage pkgs;
          githubBridge = githubBridgePackage pkgs;
          fleetTunnel = fleetTunnelPackage pkgs;
          voiceAssistant = voiceAssistantPackage pkgs;
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
          github-bridge = {
            type = "app";
            program = "${githubBridge}/bin/github-bridge";
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
          # Install the flake-managed daemon set into ~/.config/systemd/user + reconcile + enable (task #486/#493).
          # `nix run .#install-fleet-daemons` — the deliberate cutover; it does NOT auto-run at build/land time.
          install-fleet-daemons = {
            type = "app";
            program = "${(fleetDaemons pkgs).installApp}/bin/install-fleet-daemons";
          };
          # task_717: hot-swap ONLY the fleet-binary units (notify/tunnel/watchdog/nudge-stale) onto a freshly
          # built fleet binary, leaving the Group B guard timers + crontab untouched. Use this to activate a
          # merged fleet-binary change without triggering the task_495 guard flag-day. `nix run .#deploy-fleet-binary`.
          deploy-fleet-binary = {
            type = "app";
            program = "${(fleetDaemons pkgs).deployBinaryApp}/bin/deploy-fleet-binary";
          };
          # task_719: the dark-fleet-binary freshness sweep (detect-and-nudge). Normally run hourly by its timer;
          # `nix run .#fleet-binary-sweep` runs one sweep by hand for testing.
          fleet-binary-sweep = {
            type = "app";
            program = "${(fleetDaemons pkgs).sweepApp}/bin/fleet-binary-sweep";
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
      checks = forAllSystems (
        pkgs:
        {
          fleet = fleetPackage pkgs;
          # Build the generated user units + the install app (writeShellApplication runs shellcheck) so a break
          # in the unit generator or the install script fails `nix flake check` (task #486/#493).
          fleet-user-units = (fleetDaemons pkgs).unitsDir;
          install-fleet-daemons = (fleetDaemons pkgs).installApp;
          # Build the binary-only deploy app too (writeShellApplication runs shellcheck), so a break in the
          # scoped deploy script fails `nix flake check` / CI (task_717).
          deploy-fleet-binary = (fleetDaemons pkgs).deployBinaryApp;
          # Build the binary-freshness sweep (shellcheck) so a break in it fails the gate (task_719).
          fleet-binary-sweep = (fleetDaemons pkgs).sweepApp;
        }
        # voice-assistant bundles a linux-x64 prebuilt sherpa lib, so it only builds there; gate the
        # check to that system so `nix flake check` on arm/darwin doesn't try (and fail) to build it.
        // pkgs.lib.optionalAttrs (pkgs.stdenv.hostPlatform.system == "x86_64-linux") {
          voice-assistant = voiceAssistantPackage pkgs;
        }
      );

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
