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

/// Pre-wire the fork's `upstream` remote on the shared mirror and fetch it WITH TAGS, so a fork-vs-upstream
/// parity diff (`git diff <upstream-tag> -- …`) is one command from any worktree cut from the mirror — the
/// version-parity-diff lever for a fork-maintainer vertical (task_875). BEST-EFFORT: a parity-diff
/// convenience must never fail a spin-up, so every step only warns on failure and the caller continues.
/// Idempotent: on a re-materialize it updates the URL (`set-url`) rather than erroring on an existing remote,
/// and `--tags` re-fetch is a cheap no-op when nothing changed.
fn pre_wire_upstream(mirror: &str, upstream: &str) {
    let url = repo_url(upstream);
    if git(&["-C", mirror, "remote", "get-url", "upstream"]).is_ok() {
        // Remote already present (a re-materialize) — keep its URL current in case the declared upstream changed.
        let _ = git(&["-C", mirror, "remote", "set-url", "upstream", &url]);
    } else if let Err(e) = git(&["-C", mirror, "remote", "add", "upstream", &url]) {
        eprintln!("  WARN: could not add upstream remote {url}: {e} (a fork-parity diff will need a manual `git remote add upstream`)");
        return;
    }
    // Fetch WITH TAGS so a release-tag parity diff is one command; this does not touch origin's refs.
    if let Err(e) = git(&["-C", mirror, "fetch", "upstream", "--tags", "--prune", "--quiet"]) {
        eprintln!("  WARN: could not fetch upstream {url}: {e} (a fork-parity diff will need a manual `git fetch upstream --tags`)");
    }
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

/// Ensure `<agent>`'s worktree of `<repo_spec>` exists off a shared bare mirror; return the workspace dir.
/// The worktree is on a per-agent branch `fleet/<agent>` cut from `<branch>` (the declared base), so many
/// agents can share one repo mirror without colliding on a single checked-out branch. Idempotent:
/// refreshes an existing mirror (prune touches only remote-tracking refs) and leaves an existing worktree
/// as-is.
///
/// `upstream` (optional, e.g. `aws/s2n-quic`): a fork-maintainer vertical's upstream repo. When set, an
/// `upstream` remote is pre-wired on the shared mirror and fetched WITH TAGS, so every worktree cut from the
/// mirror can diff the fork against an upstream release tag (`git diff v1.88.0 -- …`) in one command, with no
/// by-hand `git remote add` mid-investigation — the version-parity-diff lever (task_875). It is best-effort:
/// a parity-diff convenience must never fail a spin-up, so a remote-add/fetch hiccup only warns.
pub fn ensure(
    fleet_root: &str,
    agent: &str,
    repo_spec: &str,
    branch: &str,
    upstream: Option<&str>,
) -> Result<String, String> {
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
    if let Some(up) = upstream.map(str::trim).filter(|u| !u.is_empty()) {
        pre_wire_upstream(&mirror, up);
    }

    if !Path::new(&workdir).exists() {
        let adir = format!("{fleet_root}/agents/{agent}");
        std::fs::create_dir_all(&adir).map_err(|e| format!("mkdir {adir}: {e}"))?;
        // Each agent gets its OWN branch (`fleet/<agent>`) so N agents can share ONE repo mirror — git
        // refuses to check out the same branch (e.g. `main`) in two worktrees, so N agents on a shared
        // repo cannot all check out the declared branch directly. The declared `branch` is the BASE the
        // per-agent branch is cut from (its remote-tracking ref), not the checked-out branch itself.
        let agent_branch = format!("fleet/{agent}");
        let agent_head = format!("refs/heads/{agent_branch}");
        if git(&["-C", &mirror, "show-ref", "--verify", "--quiet", &agent_head]).is_ok() {
            git(&["-C", &mirror, "worktree", "add", "--quiet", &workdir, &agent_branch])?; // resume
        } else {
            let declared = format!("refs/remotes/origin/{branch}");
            let base = if git(&["-C", &mirror, "show-ref", "--verify", "--quiet", &declared]).is_ok() {
                format!("origin/{branch}")
            } else {
                mirror_default_base(&mirror)?
            };
            git(&["-C", &mirror, "worktree", "add", "--quiet", "-b", &agent_branch, &workdir, &base])?;
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
    fn ensure_pre_wires_the_upstream_remote_with_tags() {
        // task_875: a fork vertical declaring an upstream gets the `upstream` remote + its release tags
        // pre-wired on the shared mirror, so a fork-vs-upstream parity diff is one command on a fresh checkout.
        let base = std::env::temp_dir().join(format!("fleet-ws-upstream-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let up = base.join("upstream");
        let origin = base.join("origin");
        let fleet_root = base.join("fleet");
        std::fs::create_dir_all(&up).unwrap();
        std::fs::create_dir_all(&origin).unwrap();
        let run = |dir: &Path, args: &[&str]| {
            assert!(
                Command::new("git").current_dir(dir).args(args).output().unwrap().status.success(),
                "git {args:?} in {dir:?}"
            );
        };
        // upstream repo: one commit + a release tag (the parity-diff target).
        run(&up, &["init", "-q", "-b", "main"]);
        run(&up, &["config", "user.email", "u@u"]);
        run(&up, &["config", "user.name", "u"]);
        std::fs::write(up.join("lib.rs"), "// upstream v1").unwrap();
        run(&up, &["add", "-A"]);
        run(&up, &["commit", "-qm", "upstream init"]);
        run(&up, &["tag", "v1.0"]);
        // origin (the fork) repo: one commit.
        run(&origin, &["init", "-q", "-b", "main"]);
        run(&origin, &["config", "user.email", "o@o"]);
        run(&origin, &["config", "user.name", "o"]);
        std::fs::write(origin.join("lib.rs"), "// fork").unwrap();
        run(&origin, &["add", "-A"]);
        run(&origin, &["commit", "-qm", "fork init"]);

        let fr = fleet_root.to_str().unwrap();
        let origin_spec = origin.to_str().unwrap();
        let up_spec = up.to_str().unwrap();

        let wd = ensure(fr, "v-fork", origin_spec, "main", Some(up_spec)).expect("ensure with upstream");
        assert!(Path::new(&wd).is_dir(), "worktree materialized");

        let mirror = mirror_dir(fr, origin_spec);
        // the `upstream` remote is wired on the shared mirror, pointing at the declared upstream,
        assert_eq!(
            git(&["-C", &mirror, "remote", "get-url", "upstream"]).expect("upstream remote wired"),
            repo_url(up_spec)
        );
        // and the upstream's release tag was fetched, so a parity diff against it is one command.
        git(&["-C", &mirror, "rev-parse", "--verify", "v1.0"]).expect("upstream tag v1.0 fetched into the mirror");

        // idempotent: a second materialize with the same upstream is a clean no-op (set-url, re-fetch).
        let wd2 = ensure(fr, "v-fork", origin_spec, "main", Some(up_spec)).expect("idempotent re-materialize");
        assert_eq!(wd, wd2);

        // a DIFFERENT repo with NO upstream declared wires no upstream remote.
        let origin2 = base.join("origin2");
        std::fs::create_dir_all(&origin2).unwrap();
        run(&origin2, &["init", "-q", "-b", "main"]);
        run(&origin2, &["config", "user.email", "o2@o"]);
        run(&origin2, &["config", "user.name", "o2"]);
        std::fs::write(origin2.join("f"), "x").unwrap();
        run(&origin2, &["add", "-A"]);
        run(&origin2, &["commit", "-qm", "init"]);
        let _ = ensure(fr, "v-plain", origin2.to_str().unwrap(), "main", None).expect("ensure without upstream");
        assert!(
            git(&["-C", &mirror_dir(fr, origin2.to_str().unwrap()), "remote", "get-url", "upstream"]).is_err(),
            "no upstream remote when none is declared"
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn mirror_dir_and_agent_root_dir() {
        assert_eq!(mirror_dir("/root/.fleet", "camshaft/task-board"), "/root/.fleet/mirrors/task-board.git");
        assert_eq!(mirror_dir("/root/.fleet", "camshaft/bolero.git"), "/root/.fleet/mirrors/bolero.git");
        assert_eq!(agent_root_dir("/root/.fleet", "board-pm"), "/root/.fleet/agents/board-pm");
    }
}
