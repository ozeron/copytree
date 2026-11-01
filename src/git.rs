//! Wrappers around the `git` binary. Copytree never reimplements git.

use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Set while Copytree runs git itself, so the post-checkout hook stays out of the way.
pub const IN_PROGRESS_ENV: &str = "COPYTREE_IN_PROGRESS";

fn command(cwd: &Path, args: &[&str]) -> Command {
    let mut c = Command::new("git");
    c.current_dir(cwd)
        .args(args)
        .env(IN_PROGRESS_ENV, "1")
        .stdin(Stdio::null());
    c
}

/// Runs git and returns its trimmed stdout.
pub fn output(cwd: &Path, args: &[&str]) -> Result<String> {
    let out = command(cwd, args).output().context("failed to run git")?;
    if !out.status.success() {
        bail!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim_end().to_string())
}

/// Runs git with its output shown on stderr, keeping stdout clean for machine output.
pub fn run(cwd: &Path, args: &[&str]) -> Result<()> {
    let status = command(cwd, args)
        .stdout(std::io::stderr())
        .status()
        .context("failed to run git")?;
    if !status.success() {
        bail!("git {} failed", args.join(" "));
    }
    Ok(())
}

pub fn succeeds(cwd: &Path, args: &[&str]) -> bool {
    command(cwd, args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

#[derive(Debug, Clone)]
pub struct Repo {
    /// The main worktree, where dependencies and caches are copied from.
    pub main: PathBuf,
    /// The shared `.git` directory.
    pub common_dir: PathBuf,
}

impl Repo {
    pub fn discover(cwd: &Path) -> Result<Repo> {
        let common_dir = output(
            cwd,
            &["rev-parse", "--path-format=absolute", "--git-common-dir"],
        )
        .with_context(|| format!("not inside a git repository: {}", cwd.display()))?;
        let main = match list_worktrees(cwd)?.into_iter().next() {
            Some(w) if !w.bare => w.path,
            _ => bail!("bare repositories are not supported"),
        };
        Ok(Repo {
            main,
            common_dir: PathBuf::from(common_dir),
        })
    }

    pub fn state_dir(&self) -> PathBuf {
        self.common_dir.join("copytree")
    }

    /// `../.copytree/<repo>/`: on the repo's volume, outside the repo.
    pub fn default_pool(&self) -> PathBuf {
        let name = self
            .main
            .file_name()
            .map(|n| n.to_os_string())
            .unwrap_or_else(|| "repo".into());
        let parent = self.main.parent().unwrap_or(&self.main);
        parent.join(".copytree").join(name)
    }
}

#[derive(Debug, Clone, Default)]
pub struct Worktree {
    pub path: PathBuf,
    pub head: Option<String>,
    pub branch: Option<String>,
    pub bare: bool,
    pub locked: bool,
}

pub fn list_worktrees(cwd: &Path) -> Result<Vec<Worktree>> {
    Ok(parse_worktrees(&output(
        cwd,
        &["worktree", "list", "--porcelain"],
    )?))
}

fn parse_worktrees(porcelain: &str) -> Vec<Worktree> {
    let mut list: Vec<Worktree> = Vec::new();
    for line in porcelain.lines() {
        if let Some(path) = line.strip_prefix("worktree ") {
            list.push(Worktree {
                path: PathBuf::from(path),
                ..Default::default()
            });
            continue;
        }
        let Some(w) = list.last_mut() else { continue };
        if let Some(head) = line.strip_prefix("HEAD ") {
            w.head = Some(head.to_string());
        } else if let Some(branch) = line.strip_prefix("branch ") {
            w.branch = Some(branch.trim_start_matches("refs/heads/").to_string());
        } else if line == "bare" {
            w.bare = true;
        } else if line.starts_with("locked") {
            w.locked = true;
        }
    }
    list
}

/// The top of each ignored subtree in `root`; directories end with `/`.
pub fn ignored_roots(root: &Path) -> Result<Vec<String>> {
    let out = output(
        root,
        &[
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

/// True when `rel` is untracked and matched by an ignore rule.
pub fn is_untracked_and_ignored(root: &Path, rel: &str) -> bool {
    !succeeds(root, &["ls-files", "--error-unmatch", "--", rel])
        && succeeds(root, &["check-ignore", "-q", "--", rel])
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
        let s = "worktree /a\nHEAD 111\nbranch refs/heads/main\n\nworktree /b\nHEAD 222\ndetached\nlocked why\n\nworktree /c\nbare\n";
        let w = parse_worktrees(s);
        assert_eq!(w.len(), 3);
        assert_eq!(w[0].branch.as_deref(), Some("main"));
        assert!(w[1].branch.is_none() && w[1].locked);
        assert!(w[2].bare);
    }
}
