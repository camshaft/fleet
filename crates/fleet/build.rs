//! Bakes the build revision into the binary as `FLEET_BUILD_REV` so `fleet version` can report which commit
//! the running binary was built from — the signal that was missing when a stale deployed binary silently ran
//! old logic. Precedence: the flake-supplied `FLEET_BUILD_REV` (the nix hermetic build has no `.git`), else
//! `git rev-parse` (a dev cargo build, with a `-dirty` marker for an uncommitted tree), else `unknown`. Never
//! fails the build.

use std::process::Command;

fn main() {
    // Re-run when the flake env changes or HEAD moves (best-effort path from the crate dir to the repo .git;
    // a missing path is harmless), so a rebuild after a new commit updates the baked rev.
    println!("cargo:rerun-if-env-changed=FLEET_BUILD_REV");
    println!("cargo:rerun-if-changed=../../.git/HEAD");

    let rev = std::env::var("FLEET_BUILD_REV")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty() && s != "unknown")
        .or_else(git_short_rev)
        .unwrap_or_else(|| "unknown".to_string());
    println!("cargo:rustc-env=FLEET_BUILD_REV={rev}");
}

/// The short HEAD rev of a dev checkout, with a `-dirty` suffix when the working tree has uncommitted changes.
/// `None` when git is unavailable or there is no repo (e.g. the nix sandbox) — the caller then falls back.
fn git_short_rev() -> Option<String> {
    let out = Command::new("git").args(["rev-parse", "--short", "HEAD"]).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let rev = String::from_utf8(out.stdout).ok()?.trim().to_string();
    if rev.is_empty() {
        return None;
    }
    let dirty = Command::new("git")
        .args(["status", "--porcelain"])
        .output()
        .ok()
        .map(|o| !o.stdout.is_empty())
        .unwrap_or(false);
    Some(if dirty { format!("{rev}-dirty") } else { rev })
}
