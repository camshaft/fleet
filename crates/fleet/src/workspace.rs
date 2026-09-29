//! `workspace` — per-agent git worktrees off a shared bare-mirror store under the fleet root.
//!
//! The generic, repo-agnostic workspace model (../../DESIGN.md): an agent's directory holds one worktree
//! per repo it works in, and every worktree of a repo shares ONE bare mirror
//! (`$FLEET_ROOT/mirrors/<repo>.git` + `$FLEET_ROOT/agents/<agent>/<repo>`), so an agent works across many
//! repos and N agents share a repo's objects. Upstream branches live under `refs/remotes/origin/*` and
//! agent worktree branches under `refs/heads/*`, so `fetch --prune` never prunes a peer agent's branch.

use std::path::Path;
use std::process::Command;

fn git(args: &[&str]) -> Result<String, String> {
    let out = Command::new("git")
        .args(args)
        .output()
        .map_err(|e| format!("git {args:?}: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// The repo's basename (drops a trailing `.git` and any owner prefix): `camshaft/task-board` -> `task-board`.
pub fn repo_name(repo: &str) -> &str {
    repo.trim_end_matches('/').trim_end_matches(".git").rsplit('/').next().unwrap_or(repo)
}

/// Resolve a `repos` entry into a cloneable URL: an `owner/name` -> GitHub; a URL / path / `git@` passes through.
pub fn repo_url(spec: &str) -> String {
    if spec.contains("://") || spec.starts_with('/') || spec.starts_with("git@") {
        spec.to_string()
    } else if spec.matches('/').count() == 1 {
        format!("https://github.com/{spec}.git")
    } else {
        spec.to_string()
    }
}

/// The workspace directory for `<agent>`'s checkout of `<repo>` under the fleet root.
pub fn workspace_dir(fleet_root: &str, agent: &str, repo: &str) -> String {
    format!("{fleet_root}/agents/{agent}/{}", repo_name(repo))
}

/// The base workspace directory for `<agent>` (no repo subdir) — where a repo-less agent (e.g. a board
/// orchestrator that works via the board MCP rather than a code checkout) is run.
pub fn agent_root_dir(fleet_root: &str, agent: &str) -> String {
    format!("{fleet_root}/agents/{agent}")
}

/// The shared bare-mirror directory for `<repo>` under the fleet root. This is the git *common dir* of
/// every agent's worktree of the repo, and therefore the path claude resolves a worktree to for its
/// folder-trust check — so it is what must be pre-trusted for an unattended launch.
pub fn mirror_dir(fleet_root: &str, repo: &str) -> String {
    format!("{fleet_root}/mirrors/{}.git", repo_name(repo))
}

/// The remote-tracking ref a NEW agent branch is cut from (never a local head, so a peer's `fetch --prune`
/// can't delete it): `origin/HEAD`, else `origin/main`/`origin/master`.
fn mirror_default_base(mirror: &str) -> Result<String, String> {
    if let Ok(b) = git(&["-C", mirror, "symbolic-ref", "--short", "refs/remotes/origin/HEAD"])
        && !b.is_empty()
    {
        return Ok(b);
    }
    for c in ["origin/main", "origin/master"] {
        if git(&["-C", mirror, "show-ref", "--verify", "--quiet", &format!("refs/remotes/{c}")]).is_ok() {
            return Ok(c.to_string());
        }
    }
    Err(format!("no remote-tracking base in {mirror} to branch from"))
}

/// Ensure `<agent>`'s worktree of `<repo_spec>` on `<branch>` exists off a shared bare mirror; return the
/// workspace dir. Idempotent: refreshes an existing mirror (prune touches only remote-tracking refs) and
/// leaves an existing worktree as-is.
pub fn ensure(fleet_root: &str, agent: &str, repo_spec: &str, branch: &str) -> Result<String, String> {
    let name = repo_name(repo_spec);
    let mirrors = format!("{fleet_root}/mirrors");
    let mirror = format!("{mirrors}/{name}.git");
    let workdir = workspace_dir(fleet_root, agent, repo_spec);

    if Path::new(&mirror).exists() {
        git(&["-C", &mirror, "fetch", "origin", "--prune", "--quiet"])?;
    } else {
        std::fs::create_dir_all(&mirrors).map_err(|e| format!("mkdir {mirrors}: {e}"))?;
        git(&["init", "--quiet", "--bare", &mirror])?;
        git(&["-C", &mirror, "remote", "add", "origin", &repo_url(repo_spec)])?;
        git(&["-C", &mirror, "fetch", "origin", "--prune", "--quiet"])?;
    }
    let _ = git(&["-C", &mirror, "remote", "set-head", "origin", "-a"]); // best-effort default-branch pointer

    if !Path::new(&workdir).exists() {
        let adir = format!("{fleet_root}/agents/{agent}");
        std::fs::create_dir_all(&adir).map_err(|e| format!("mkdir {adir}: {e}"))?;
        let head_ref = format!("refs/heads/{branch}");
        if git(&["-C", &mirror, "show-ref", "--verify", "--quiet", &head_ref]).is_ok() {
            git(&["-C", &mirror, "worktree", "add", "--quiet", &workdir, branch])?; // resume existing branch
        } else {
            let base = mirror_default_base(&mirror)?;
            git(&["-C", &mirror, "worktree", "add", "--quiet", "-b", branch, &workdir, &base])?;
        }
    }
    Ok(workdir)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repo_name_drops_owner_and_dotgit() {
        assert_eq!(repo_name("camshaft/task-board"), "task-board");
        assert_eq!(repo_name("camshaft/task-board.git"), "task-board");
        assert_eq!(repo_name("https://github.com/camshaft/cadenza.git"), "cadenza");
    }

    #[test]
    fn repo_url_expands_owner_name_but_passes_urls_and_paths() {
        assert_eq!(repo_url("camshaft/task-board"), "https://github.com/camshaft/task-board.git");
        assert_eq!(repo_url("https://x/y.git"), "https://x/y.git");
        assert_eq!(repo_url("/abs/path/repo"), "/abs/path/repo");
        assert_eq!(repo_url("git@github.com:o/r.git"), "git@github.com:o/r.git");
    }

    #[test]
    fn workspace_dir_is_agent_slash_reponame() {
        assert_eq!(
            workspace_dir("/root/.fleet", "v-x", "camshaft/task-board"),
            "/root/.fleet/agents/v-x/task-board"
        );
    }

    #[test]
    fn mirror_dir_and_agent_root_dir() {
        assert_eq!(mirror_dir("/root/.fleet", "camshaft/task-board"), "/root/.fleet/mirrors/task-board.git");
        assert_eq!(mirror_dir("/root/.fleet", "camshaft/bolero.git"), "/root/.fleet/mirrors/bolero.git");
        assert_eq!(agent_root_dir("/root/.fleet", "board-pm"), "/root/.fleet/agents/board-pm");
    }
}
