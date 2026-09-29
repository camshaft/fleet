//! fleet-tunnel — reverse HTTP-over-websocket bridge (fleet-host side).
//!
//! The fleet host has no inbound path, so the daemon dials OUT to the board's websocket endpoint
//! and lets the board ride HTTP requests back down the socket. Each request frame is forwarded to a
//! single configured local upstream (the notifier) and the response is sent back over the same
//! socket. The daemon is the websocket CLIENT; the board is the SERVER that originates the HTTP
//! requests. A single long-lived socket carries many multiplexed request/response pairs, correlated
//! by an integer `id` the board allocates.
//!
//! This lib is the pure, synchronously-testable core: [`config`] (TOML parse/validate) and
//! [`frame`] (the wire protocol + base64 body codec). The async transport that drives them lives in
//! the `fleet-tunnel` binary (`src/main.rs`), behind the `transport` feature.

pub mod config;
pub mod frame;
