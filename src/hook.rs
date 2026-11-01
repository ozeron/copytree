//! Hook mode: a git `post-checkout` hook that prepares every new linked
//! worktree, whichever tool created it (`git worktree add`, Claude Code,
//! VS Code, worktrunk, ...).
//!
//! git runs `post-checkout <old> <new> <flag>` inside the new worktree after
//! `git worktree add`, with an all-zeros `<old>`. `git clone` does the same in
//! the main worktree, so Copytree also checks that it's in a linked worktree.

use crate::config::Config;
use crate::git::{self, IN_PROGRESS_ENV, Repo};
use crate::state::{self, Origin, Status, Workspace};
use crate::workspace::{self, PrepareOptions};
use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};

const MARKER: &str = "# copytree-hook";
const CHAINED: &str = "post-checkout.pre-copytree";

/// Where git looks for hooks: `core.hooksPath` (relative to the main worktree)
/// or `<common-dir>/hooks`.
pub fn hooks_dir(repo: &Repo) -> PathBuf {
    match git::output(&repo.main, ["config", "--get", "core.hooksPath"]) {
        Ok(p) if !p.is_empty() => {
            let p = PathBuf::from(p);
            if p.is_absolute() {
                p
            } else {
                repo.main.join(p)
            }
        }
        _ => repo.common_dir.join("hooks"),
    }
}

pub fn is_installed(repo: &Repo) -> bool {
    std::fs::read_to_string(hooks_dir(repo).join("post-checkout")).is_ok_and(|s| s.contains(MARKER))
}

fn script(exe: &Path) -> String {
    let exe = exe.display().to_string().replace('\'', "'\\''");
    format!(
        r#"#!/bin/sh
{MARKER} (installed by `copytree install-hook`; remove with `copytree uninstall-hook`)
# Run the hook this one replaced first, then let Copytree prepare new worktrees.
prev="$(dirname "$0")/{CHAINED}"
if [ -x "$prev" ]; then "$prev" "$@" || exit $?; fi
if [ -x '{exe}' ]; then exec '{exe}' hook post-checkout "$@"; fi
if command -v copytree >/dev/null 2>&1; then exec copytree hook post-checkout "$@"; fi
exit 0
"#
    )
}

/// Install the hook, chaining to any existing post-checkout hook.
pub fn install(repo: &Repo) -> Result<PathBuf> {
    let dir = hooks_dir(repo);
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let hook = dir.join("post-checkout");
    if let Ok(existing) = std::fs::read_to_string(&hook)
        && !existing.contains(MARKER)
    {
        let chained = dir.join(CHAINED);
        if chained.exists() {
            bail!(
                "{} already exists; refusing to overwrite it",
                chained.display()
            );
        }
        std::fs::rename(&hook, &chained)?;
    }
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("copytree"));
    std::fs::write(&hook, script(&exe))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755))?;
    }
    Ok(hook)
}

/// Remove the hook and restore the one it replaced, if any.
pub fn uninstall(repo: &Repo) -> Result<bool> {
    let dir = hooks_dir(repo);
    let hook = dir.join("post-checkout");
    if !is_installed(repo) {
        return Ok(false);
    }
    std::fs::remove_file(&hook)?;
    let chained = dir.join(CHAINED);
    if chained.exists() {
        std::fs::rename(&chained, &hook)?;
    }
    Ok(true)
}

fn is_null_sha(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b == b'0')
}

/// Handle `post-checkout <old> <new> <flag>`. Returns `Ok(false)` when there was
/// nothing to do. Callers should never fail the user's git command over it.
pub fn post_checkout(cwd: &Path, args: &[String], log: &mut dyn FnMut(&str)) -> Result<bool> {
    if std::env::var_os(IN_PROGRESS_ENV).is_some() {
        return Ok(false); // `copytree new` prepares the workspace itself.
    }
    if !args.first().is_some_and(|old| is_null_sha(old)) {
        return Ok(false); // An ordinary checkout, not a new worktree.
    }
    let git_dir = git::output(cwd, ["rev-parse", "--path-format=absolute", "--git-dir"])?;
    let repo = Repo::discover(cwd)?;
    if Path::new(&git_dir) == repo.common_dir {
        return Ok(false); // Main worktree (e.g. `git clone`).
    }
    let top = PathBuf::from(git::output(cwd, ["rev-parse", "--show-toplevel"])?);
    let name = top
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "worktree".into());
    let name = sanitize(&name);
    if state::all(&repo).iter().any(|w| w.path == top) {
        return Ok(false);
    }
    let cfg = Config::load(&repo.main)?;
    let ws = Workspace {
        name: unique_name(&repo, &name),
        path: top.clone(),
        branch: git::output(&top, ["symbolic-ref", "--quiet", "--short", "HEAD"]).ok(),
        base: args.get(1).cloned().unwrap_or_default(),
        created_at: state::now(),
        status: Status::Preparing,
        origin: Origin::Hook,
        backend: None,
        main_fingerprints: Default::default(),
        carried: Vec::new(),
        error: None,
    };
    state::save(&repo, &ws)?;
    log(&format!(
        "copytree: preparing new worktree {}",
        top.display()
    ));
    workspace::prepare(&repo, &cfg, ws, &PrepareOptions::default(), log)?;
    Ok(true)
}

fn sanitize(name: &str) -> String {
    let s: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '-'
            }
        })
        .collect();
    let s = s.trim_start_matches(['.', '-']).to_string();
    if s.is_empty() { "worktree".into() } else { s }
}

fn unique_name(repo: &Repo, base: &str) -> String {
    if state::load(repo, base).is_none() {
        return base.to_string();
    }
    (2..)
        .map(|i| format!("{base}-{i}"))
        .find(|n| state::load(repo, n).is_none())
        .unwrap()
}
