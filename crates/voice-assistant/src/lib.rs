//! `voice-assistant` — a local voice loop: a custom wake phrase opens the mic, speech is transcribed,
//! a Claude session (wired to the knowledge-base / task-board / surfaces MCP servers) answers, and the
//! reply is spoken back. Ported from the Python `shop-assistant` (operator seq-1375/1376: it must live
//! in a repo + flake, and it must be Rust).
//!
//! This lib is the pure, synchronously-testable core — no audio device, no ONNX backend, no subprocess:
//!   - [`config`]  — the TOML config surface (every knob; replaces the old `SA_*` env vars).
//!   - [`chime`]   — the wake/done/ready cue tone synthesis (pure sample math).
//!   - [`events`]  — board-webhook parsing: which events become a proactive prompt (pure) + the
//!     `tiny_http` receiver and board register/call HTTP (thin, testable seams).
//!   - [`brain`]   — decoding the `claude` CLI's `stream-json` output into the final spoken text (pure).
//!   - [`scope`]   — the filesystem-scope guard: pure containment policy + the `scope-guard` hook shim.
//!   - [`retry`]   — the capped-exponential backoff schedule the runtime's audio-device reconnect uses.
//!
//! The live runtime — microphone capture + VAD, sherpa-onnx STT/TTS/wake, the `claude` subprocess, and
//! the main loop — lives behind the `runtime` feature (see [`runtime`]) so the default `cargo test` /
//! `nix flake check` never builds the native/GPU tree.

pub mod brain;
pub mod chime;
pub mod config;
pub mod events;
pub mod retry;
pub mod scope;

#[cfg(feature = "runtime")]
pub mod runtime;
