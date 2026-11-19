//! Hook mode: a `post-checkout` hook that prepares every new linked worktree,
//! whichever tool created it.
//!
//! git runs `post-checkout <old> <new> <flag>` inside a new worktree after
//! `git worktree add`, with an all-zero `<old>`. `git clone` does the same in
//! the main worktree, so only linked worktrees are prepared.

use crate::config::Config;
use crate::git::{self, IN_PROGRESS_ENV, Repo};
use crate::state::{self, Origin, Workspace};
use crate::workspace::{self, PrepareOptions};
use anyhow::{Result, bail};
use std::path::{Path, PathBuf};

const MARKER: &str = "# copytree-hook";
const CHAINED: &str = "post-checkout.pre-copytree";

/// `core.hooksPath` when set, else `<common-dir>/hooks`.
pub fn hooks_dir(repo: &Repo) -> PathBuf {
    match git::output(&repo.main, &["config", "--get", "core.hooksPath"]) {
        Ok(path) if !path.is_empty() => expand_tilde(&path).unwrap_or_else(|| repo.main.join(path)),
        _ => repo.common_dir.join("hooks"),
    }
}

/// Expands a git-style `~` or `~/...` prefix. git honors one in
/// `core.hooksPath`; `Path::join` would treat it as a literal directory name.
fn expand_tilde(path: &str) -> Option<PathBuf> {
    if path != "~" && !path.starts_with("~/") {
        return None;
    }
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"))?;
    let rest = path.strip_prefix('~').unwrap_or_default();
    let rest = rest.trim_start_matches('/');
    if rest.is_empty() {
        Some(PathBuf::from(home))
    } else {
        Some(Path::new(&home).join(rest))
    }
}

pub fn is_installed(repo: &Repo) -> bool {
    std::fs::read_to_string(hooks_dir(repo).join("post-checkout")).is_ok_and(|s| s.contains(MARKER))
}

fn script(exe: &Path) -> String {
    let exe = exe.to_string_lossy().replace('\'', r"'\''");
    format!(
        r#"#!/bin/sh
{MARKER}: installed by `copytree install-hook`, removed by `copytree uninstall-hook`.
prev="$(dirname "$0")/{CHAINED}"
if [ -x "$prev" ]; then "$prev" "$@" || exit $?; fi
if [ -x '{exe}' ]; then exec '{exe}' hook post-checkout "$@"; fi
if command -v copytree >/dev/null 2>&1; then exec copytree hook post-checkout "$@"; fi
exit 0
"#
    )
}

/// Installs the hook. An existing hook is kept as `post-checkout.pre-copytree` and still runs first.
pub fn install(repo: &Repo) -> Result<PathBuf> {
    let dir = hooks_dir(repo);
    std::fs::create_dir_all(&dir)?;
    let hook = dir.join("post-checkout");
    let chained = dir.join(CHAINED);
    if hook.exists() && !is_installed(repo) {
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

/// Removes the hook and restores the one it replaced. Returns false when no Copytree hook was installed.
pub fn uninstall(repo: &Repo) -> Result<bool> {
    if !is_installed(repo) {
        return Ok(false);
    }
    let dir = hooks_dir(repo);
    std::fs::remove_file(dir.join("post-checkout"))?;
    if dir.join(CHAINED).exists() {
        std::fs::rename(dir.join(CHAINED), dir.join("post-checkout"))?;
    }
    Ok(true)
}

/// Handles `post-checkout <old> <new> <flag>`. Returns false when there was nothing to do.
pub fn post_checkout(cwd: &Path, args: &[String], log: &mut dyn FnMut(&str)) -> Result<bool> {
    let creates_worktree = args
        .first()
        .is_some_and(|old| !old.is_empty() && old.bytes().all(|b| b == b'0'));
    if std::env::var_os(IN_PROGRESS_ENV).is_some() || !creates_worktree {
        return Ok(false);
    }
    let repo = Repo::discover(cwd)?;
    let git_dir = git::output(cwd, &["rev-parse", "--path-format=absolute", "--git-dir"])?;
    if Path::new(&git_dir) == repo.common_dir {
        return Ok(false);
    }
    let top = PathBuf::from(git::output(cwd, &["rev-parse", "--show-toplevel"])?);
    if state::all(&repo).iter().any(|w| w.path == top) {
        return Ok(false);
    }
    let cfg = Config::load(&repo.main)?;
    let name = unused_name(
        &repo,
        &workspace::sanitize_name(&top.file_name().unwrap_or_default().to_string_lossy()),
    );
    let ws = Workspace::new(name, top, Origin::Hook);
    state::save(&repo, &ws)?;
    log(&format!("preparing new worktree {}", ws.path.display()));
    workspace::prepare(&repo, &cfg, ws, &PrepareOptions::default(), log)?;
    Ok(true)
}

fn unused_name(repo: &Repo, base: &str) -> String {
    std::iter::once(base.to_string())
        .chain((2..).map(|i| format!("{base}-{i}")))
        .find(|name| state::load(repo, name).is_none())
        .expect("an unused name exists")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expands_only_a_leading_tilde() {
        assert_eq!(expand_tilde("hooks"), None);
        assert_eq!(expand_tilde("a~"), None);
        if std::env::var_os("HOME").is_some() || std::env::var_os("USERPROFILE").is_some() {
            assert!(expand_tilde("~").is_some_and(|p| p.is_absolute()));
            assert!(expand_tilde("~/.githooks").is_some_and(|p| p.is_absolute()));
        }
    }
}
