//! `board` — kb's board client: a thin, config-aware shim over the shared `bridge-core` task-client
//! (task_355). The REST-vs-MCP distinction, the register/webhook route, and the task CRUD surface now live
//! once in `bridge_core::task_client`, so a future daemon port adopts it directly instead of re-deriving it
//! the way this crate originally did (the house pattern this module itself used to document in isolation).

pub use bridge_core::task_client::{Board, Task};

/// Build a client for the configured `board_url`, acting as `agent_id`.
pub fn connect(agent_id: impl Into<String>) -> Board {
    Board::with_base(&crate::config::get().board_url, agent_id)
}
