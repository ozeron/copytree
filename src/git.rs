//! Thin wrappers around the `git` binary.
//!
//! Copytree never reimplements git. Everything that writes goes through the
//! user's own `git`, so their version, config, hooks, LFS and sparse-checkout
//! behave exactly as they do without Copytree.

use anyhow::{Context, Result, anyhow, bail};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Environment variable set while Copytree itself runs `git worktree add`, so
/// the post-checkout hook (if installed) does not prepare the workspace twice.
pub const IN_PROGRESS_ENV: &str = "COPYTREE_IN_PROGRESS";

pub fn git(cwd: &Path) -> Command {
    let mut c = Command::new("git");
    c.current_dir(cwd);
    c
}

/// Run git and return trimmed stdout. Fails with git's stderr on non-zero exit.
pub fn output<I, S>(cwd: &Path, args: I) -> Result<String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let args: Vec<_> = args.into_iter().map(|a| a.as_ref().to_owned()).collect();
    let out = git(cwd)
        .args(&args)
        .stdin(Stdio::null())
        .output()
        .with_context(|| "failed to run git (is it installed and on PATH?)")?;
    if !out.status.success() {
        bail!(
            "git {} failed: {}",
            args.iter()
                .map(|a| a.to_string_lossy())
                .collect::<Vec<_>>()
                .join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim_end().to_string())
}

/// Run git, letting it print to the terminal's stderr (stdout is redirected to
/// stderr so `--print-path` and `--json` output stay clean).
pub fn run<I, S>(cwd: &Path, args: I) -> Result<()>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let args: Vec<_> = args.into_iter().map(|a| a.as_ref().to_owned()).collect();
    let status = git(cwd)
        .args(&args)
        .env(IN_PROGRESS_ENV, "1")
        .stdin(Stdio::null())
        .stdout(std::io::stderr())
        .status()
        .context("failed to run git")?;
    if !status.success() {
        bail!(
            "git {} failed",
            args.iter()
                .map(|a| a.to_string_lossy())
                .collect::<Vec<_>>()
                .join(" ")
        );
    }
    Ok(())
}

pub fn succeeds<I, S>(cwd: &Path, args: I) -> bool
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    git(cwd)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// The repository as Copytree sees it: the main worktree plus the shared git dir.
#[derive(Debug, Clone)]
pub struct Repo {
    /// The main (first) worktree. Dependencies and caches are seeded from here.
    pub main: PathBuf,
    /// The shared `.git` directory (`--git-common-dir`).
    pub common_dir: PathBuf,
}

impl Repo {
    /// Discover the repository that contains `cwd`, from the main worktree or
    /// from any linked worktree.
    pub fn discover(cwd: &Path) -> Result<Repo> {
        let common = output(
            cwd,
            ["rev-parse", "--path-format=absolute", "--git-common-dir"],
        )
        .map_err(|_| anyhow!("not inside a git repository: {}", cwd.display()))?;
        let common_dir = PathBuf::from(common);
        let worktrees = list_worktrees(cwd)?;
        let main = worktrees
            .iter()
            .find(|w| !w.bare)
            .map(|w| w.path.clone())
            .ok_or_else(|| {
                anyhow!("bare repositories without a main worktree are not supported")
            })?;
        Ok(Repo { main, common_dir })
    }

    /// Directory for Copytree's own metadata. It lives in the shared git dir,
    /// outside every workspace, so an agent working in a workspace doesn't see it.
    pub fn state_dir(&self) -> PathBuf {
        self.common_dir.join("copytree")
    }

    pub fn name(&self) -> String {
        self.main
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "repo".into())
    }

    /// Default pool location: `../.copytree/<repo>/`, on the same volume as the
    /// repo (needed for copy-on-write) and outside it (so file watchers ignore it).
    pub fn default_pool(&self) -> PathBuf {
        let parent = self.main.parent().unwrap_or(&self.main);
        parent.join(".copytree").join(self.name())
    }
}

#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct Worktree {
    pub path: PathBuf,
    pub head: Option<String>,
    pub branch: Option<String>,
    pub bare: bool,
    pub detached: bool,
    pub locked: bool,
    pub prunable: bool,
}

pub fn list_worktrees(cwd: &Path) -> Result<Vec<Worktree>> {
    let out = output(cwd, ["worktree", "list", "--porcelain"])?;
    Ok(parse_worktree_porcelain(&out))
}

pub fn parse_worktree_porcelain(out: &str) -> Vec<Worktree> {
    let mut list = Vec::new();
    let mut cur: Option<Worktree> = None;
    for line in out.lines() {
        if let Some(p) = line.strip_prefix("worktree ") {
            if let Some(w) = cur.take() {
                list.push(w);
            }
            cur = Some(Worktree {
                path: PathBuf::from(p),
                ..Default::default()
            });
            continue;
        }
        let Some(w) = cur.as_mut() else { continue };
        if let Some(h) = line.strip_prefix("HEAD ") {
            w.head = Some(h.to_string());
        } else if let Some(b) = line.strip_prefix("branch ") {
            w.branch = Some(b.strip_prefix("refs/heads/").unwrap_or(b).to_string());
        } else if line == "bare" {
            w.bare = true;
        } else if line == "detached" {
            w.detached = true;
        } else if line.starts_with("locked") {
            w.locked = true;
        } else if line.starts_with("prunable") {
            w.prunable = true;
        }
    }
    if let Some(w) = cur {
        list.push(w);
    }
    list
}

/// Ignored entries at the top of each ignored subtree, relative to `root`.
/// Directories end with `/`. git stops at each ignored directory instead of
/// listing every file inside, so this is fast even with huge `node_modules`.
pub fn ignored_roots(root: &Path) -> Result<Vec<String>> {
    let out = output(
        root,
        [
            "ls-files",
            "--others",
            "--ignored",
            "--exclude-standard",
            "--directory",
            "-z",
        ],
    )?;
    Ok(out
        .split('\0')
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect())
}

pub fn is_ignored(root: &Path, rel: &str) -> bool {
    succeeds(root, ["check-ignore", "-q", "--no-index", rel])
}

pub fn is_tracked(root: &Path, rel: &str) -> bool {
    succeeds(root, ["ls-files", "--error-unmatch", "--", rel])
}

pub fn version() -> Option<String> {
    let out = Command::new("git").arg("--version").output().ok()?;
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_porcelain() {
        let s = "worktree /a\nHEAD 111\nbranch refs/heads/main\n\nworktree /b\nHEAD 222\ndetached\nlocked reason\n\nworktree /c\nbare\n";
        let w = parse_worktree_porcelain(s);
        assert_eq!(w.len(), 3);
        assert_eq!(w[0].branch.as_deref(), Some("main"));
        assert!(w[1].detached && w[1].locked);
        assert!(w[2].bare);
    }
}
