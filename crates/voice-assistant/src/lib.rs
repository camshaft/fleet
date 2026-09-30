//! `voice-assistant` — a local voice loop: a custom wake phrase opens the mic, speech is transcribed,
//! a Claude session (wired to the knowledge-base / task-board / surfaces MCP servers) answers, and the
//! reply is spoken back. Ported from the Python `shop-assistant` (operator seq-1375/1376: it must live
//! in a repo + flake, and it must be Rust).
//!
//! This lib is the pure, synchronously-testable core — no audio device, no ONNX backend, no subprocess:
//!   - [`config`]  — the TOML config surface (every knob).
//!   - [`chime`]   — the wake/done/ready cue tone synthesis (pure sample math).
//!   - [`bridge`]  — the board-bridge layer on `bridge-core`: reply rendering, the OUTBOUND reply planner,
//!     cursor persistence, and the async board session the audio loop drives.
//!   - [`retry`]   — the capped-exponential backoff schedule the runtime's audio-device reconnect uses.
//!
//! The live runtime — microphone capture + VAD, sherpa-onnx STT/TTS/wake, and the main loop (posting
//! transcripts to the board + speaking George's replies) — lives behind the `runtime` feature (see
//! [`runtime`]) so the default `cargo test` / `nix flake check` never builds the native/GPU tree.

pub mod bridge;
pub mod chime;
pub mod config;
pub mod retry;

#[cfg(feature = "runtime")]
pub mod runtime;
