//! `fleet` — the standalone multi-repo agent-fleet orchestrator.
//!
//! P1 SCAFFOLD. The behavior (message bus, window management, watchdog, worktree lifecycle, check-lease,
//! hub-config) is lifted incrementally from cadenza's `xtask/src/fleet.rs` on top of this skeleton, in
//! byte-identical-behavior slices, while the LIVE fleet keeps running on cadenza-xtask until the P3
//! cutover is separately blessed. See ../../DESIGN.md for the phased plan + the core/adapter boundary.

fn main() {
    // Placeholder entry — the real subcommand dispatch (add/remove/heartbeat/inbox/send/up/watchdog/…)
    // lands in the P1 core lift. Kept trivial so the scaffold builds green from commit one.
    eprintln!(
        "fleet: standalone orchestrator (P1 scaffold). Core lift in progress — see DESIGN.md. \
         The live fleet still runs on cadenza-xtask until the P3 cutover."
    );
}
