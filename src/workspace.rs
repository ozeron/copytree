//! The workspace lifecycle: `new`, `prepare` (shared with hook mode), `list`, `find` and `rm`.

use crate::clone;
use crate::config::Config;
use crate::deps::{self, DepReport};
use crate::git::{self, Repo};
use crate::seed::{self, SeedReport};
use crate::state::{self, Origin, Status, Workspace};
use anyhow::{Context, Result, bail};
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::time::Instant;

/// `new` refuses to start with less free space than this on the pool's volume.
pub const MIN_FREE_BYTES: u64 = 1 << 30;

#[derive(Debug, Clone, Default)]
pub enum BranchMode {
    /// Create a branch, named `<prefix><name>` unless given.
    #[default]
    Create,
    CreateNamed(String),
    CheckoutExisting(String),
    Detach,
}

#[derive(Debug, Clone, Default)]
pub struct NewOptions {
    pub name: Option<String>,
    /// Commit to start from. Default: the main worktree's HEAD.
    pub base: Option<String>,
    pub branch: BranchMode,
    /// Directory for the workspace. Default: `<pool>/<name>`.
    pub path: Option<PathBuf>,
    pub prepare: PrepareOptions,
    /// Keep the worktree when a step fails, instead of rolling it back.
    pub keep_on_failure: bool,
}

#[derive(Debug, Clone, Default)]
pub struct PrepareOptions {
    pub no_deps: bool,
    pub no_seed: bool,
    pub no_carry: bool,
    pub force_carry: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct Prepared {
    pub workspace: Workspace,
    pub deps: Vec<DepReport>,
    pub seed: Vec<SeedReport>,
    /// Each step's name and duration in milliseconds.
    pub timings: Vec<(String, u128)>,
}

pub fn valid_name(name: &str) -> Result<()> {
    let ok = !name.is_empty()
        && name.len() <= 100
        && !name.starts_with(['.', '-'])
        && !name.ends_with(".lock")
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        && !name.contains("..");
    if !ok {
        bail!("invalid workspace name {name:?}: use letters, digits, '-', '_' and '.'");
    }
    Ok(())
}

/// Replaces characters that are not allowed in a workspace name.
pub fn sanitize_name(raw: &str) -> String {
    let cleaned: String = raw
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_') {
                c
            } else {
                '-'
            }
        })
        .collect();
    let cleaned = cleaned.trim_matches('-');
    if cleaned.is_empty() {
        "worktree".into()
    } else {
        cleaned.to_string()
    }
}

pub fn pool_dir(repo: &Repo, cfg: &Config) -> PathBuf {
    match &cfg.pool {
        Some(pool) => repo.main.join(pool),
        None => repo.default_pool(),
    }
}

#[cfg(unix)]
pub fn free_bytes(path: &Path) -> Option<u64> {
    use std::os::unix::ffi::OsStrExt;
    let existing = path.ancestors().find(|p| p.exists())?;
    let c_path = std::ffi::CString::new(existing.as_os_str().as_bytes()).ok()?;
    let mut stats: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c_path.as_ptr(), &mut stats) } != 0 {
        return None;
    }
    Some(stats.f_bavail as u64 * stats.f_frsize as u64)
}

#[cfg(not(unix))]
pub fn free_bytes(_path: &Path) -> Option<u64> {
    None
}

fn branch_exists(repo: &Repo, branch: &str) -> bool {
    git::succeeds(
        &repo.main,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("refs/heads/{branch}"),
        ],
    )
}

fn first_free_name(repo: &Repo, cfg: &Config) -> String {
    (1..)
        .map(|i| format!("ws-{i}"))
        .find(|name| {
            state::load(repo, name).is_none()
                && !pool_dir(repo, cfg).join(name).exists()
                && !branch_exists(repo, &format!("{}{name}", cfg.branch_prefix))
        })
        .expect("an unused name exists")
}

/// Creates a workspace with `git worktree add`, then runs [`prepare`]. When a
/// step fails, the worktree and the branch it created are removed again.
pub fn new(
    repo: &Repo,
    cfg: &Config,
    opts: &NewOptions,
    log: &mut dyn FnMut(&str),
) -> Result<Prepared> {
    let started = Instant::now();
    let name = opts
        .name
        .clone()
        .unwrap_or_else(|| first_free_name(repo, cfg));
    valid_name(&name)?;
    if state::load(repo, &name).is_some() {
        bail!("workspace {name:?} already exists (see `copytree list`)");
    }
    let dir = opts
        .path
        .clone()
        .unwrap_or_else(|| pool_dir(repo, cfg).join(&name));
    if dir.exists() {
        bail!("{} already exists", dir.display());
    }
    let parent = dir
        .parent()
        .context("workspace path has no parent directory")?;
    std::fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    if let Some(free) = free_bytes(parent).filter(|&free| free < MIN_FREE_BYTES) {
        bail!(
            "only {} MiB free next to {}; need {} MiB",
            free >> 20,
            dir.display(),
            MIN_FREE_BYTES >> 20
        );
    }

    let base = opts.base.as_deref().unwrap_or("HEAD");
    let base_sha = git::output(
        &repo.main,
        &["rev-parse", "--verify", &format!("{base}^{{commit}}")],
    )
    .with_context(|| format!("unknown base {base:?}"))?;
    let dir_arg = dir.to_string_lossy().into_owned();
    let new_branch = match &opts.branch {
        BranchMode::Create => Some(format!("{}{name}", cfg.branch_prefix)),
        BranchMode::CreateNamed(branch) => Some(branch.clone()),
        BranchMode::CheckoutExisting(_) | BranchMode::Detach => None,
    };
    let args: Vec<&str> = match (&opts.branch, &new_branch) {
        (BranchMode::Detach, _) => vec!["worktree", "add", "--detach", &dir_arg, &base_sha],
        (BranchMode::CheckoutExisting(branch), _) => vec!["worktree", "add", &dir_arg, branch],
        (_, Some(branch)) => vec!["worktree", "add", "-b", branch, &dir_arg, &base_sha],
        (_, None) => unreachable!("a created branch always has a name"),
    };
    log(&format!("checkout: git {}", args.join(" ")));
    let checkout_started = Instant::now();
    git::run(&repo.main, &args)?;
    let checkout_ms = checkout_started.elapsed().as_millis();

    let path = dir.canonicalize().unwrap_or(dir);
    let ws = Workspace::new(name, path, Origin::New);
    state::save(repo, &ws)?;
    match prepare(repo, cfg, ws.clone(), &opts.prepare, log) {
        Ok(mut prepared) => {
            prepared.timings.insert(0, ("checkout".into(), checkout_ms));
            prepared
                .timings
                .push(("total".into(), started.elapsed().as_millis()));
            Ok(prepared)
        }
        Err(e) if opts.keep_on_failure => {
            log(&format!(
                "keeping the failed workspace at {}",
                ws.path.display()
            ));
            Err(e)
        }
        Err(e) => {
            log("rolling back the half-made workspace");
            roll_back(repo, &ws, new_branch.as_deref());
            Err(e)
        }
    }
}

fn roll_back(repo: &Repo, ws: &Workspace, created_branch: Option<&str>) {
    let path = ws.path.to_string_lossy();
    let _ = git::run(&repo.main, &["worktree", "remove", "--force", &path]);
    let _ = std::fs::remove_dir_all(&ws.path);
    let _ = git::run(&repo.main, &["worktree", "prune"]);
    if let Some(branch) = created_branch {
        let _ = git::run(&repo.main, &["branch", "-D", branch]);
    }
    state::remove(repo, &ws.name);
}

/// Fills a freshly checked-out worktree: carry config, install deps, seed
/// caches, then run `post_create` hooks. The record is saved as ready or failed.
pub fn prepare(
    repo: &Repo,
    cfg: &Config,
    mut ws: Workspace,
    opts: &PrepareOptions,
    log: &mut dyn FnMut(&str),
) -> Result<Prepared> {
    let mut timings = Vec::new();
    let result = run_steps(repo, cfg, &mut ws, opts, &mut timings, log);
    ws.status = if result.is_ok() {
        Status::Ready
    } else {
        Status::Failed
    };
    ws.error = result.as_ref().err().map(|e| format!("{e:#}"));
    state::save(repo, &ws)?;
    let (deps, seed) = result?;
    Ok(Prepared {
        workspace: ws,
        deps,
        seed,
        timings,
    })
}

fn run_steps(
    repo: &Repo,
    cfg: &Config,
    ws: &mut Workspace,
    opts: &PrepareOptions,
    timings: &mut Vec<(String, u128)>,
    log: &mut dyn FnMut(&str),
) -> Result<(Vec<DepReport>, Vec<SeedReport>)> {
    let main = &repo.main;
    let mut step = |name: &str, started: Instant| {
        timings.push((name.to_string(), started.elapsed().as_millis()))
    };

    let started = Instant::now();
    let backend = clone::probe(&repo.state_dir(), &ws.path);
    step("probe", started);
    log(if backend.is_cow() {
        "volume: copy-on-write clones available"
    } else {
        "volume: no copy-on-write, so caches are copied only when small"
    });

    if !opts.no_carry {
        let started = Instant::now();
        ws.carried = seed::carry(
            main,
            &ws.path,
            &cfg.carry_patterns(main),
            opts.force_carry,
            log,
        )?;
        step("carry", started);
    }

    let mut dep_reports = Vec::new();
    if !opts.no_deps {
        let started = Instant::now();
        dep_reports = deps::install(main, &ws.path, cfg, backend, log)?;
        step("deps", started);
    }

    let mut seed_reports = Vec::new();
    if !opts.no_seed {
        let started = Instant::now();
        let targets = seed::resolve_targets(main, &cfg.seed_patterns(), &git::ignored_roots(main)?);
        seed_reports = seed::seed(
            main,
            &ws.path,
            &targets,
            backend,
            cfg.seed.copy_limit_mib << 20,
            log,
        );
        step("seed", started);
    }

    if !cfg.hooks.post_create.is_empty() {
        let started = Instant::now();
        let env = [
            ("COPYTREE_NAME", ws.name.as_str()),
            ("COPYTREE_PATH", &*ws.path.to_string_lossy()),
        ];
        for cmdline in &cfg.hooks.post_create {
            log(&format!("post_create: {cmdline}"));
            deps::run_shell(&ws.path, cmdline, &env)?;
        }
        step("post_create", started);
    }
    Ok((dep_reports, seed_reports))
}

#[derive(Debug, Clone, Serialize)]
pub struct ListEntry {
    pub name: String,
    pub path: PathBuf,
    pub branch: Option<String>,
    pub head: Option<String>,
    /// `ready`, `preparing`, `failed`, or `unmanaged` for worktrees Copytree did not prepare.
    pub status: String,
    pub origin: Option<Origin>,
    pub created_at: Option<u64>,
    pub locked: bool,
}

fn same_path(a: &Path, b: &Path) -> bool {
    a == b || matches!((a.canonicalize(), b.canonicalize()), (Ok(x), Ok(y)) if x == y)
}

fn dir_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// Linked worktrees joined with Copytree's records. Records of worktrees that
/// were removed with plain git are deleted along the way.
pub fn list(repo: &Repo) -> Result<Vec<ListEntry>> {
    let worktrees = git::list_worktrees(&repo.main)?;
    let records = state::all(repo);
    for record in &records {
        if !worktrees.iter().any(|w| same_path(&w.path, &record.path)) {
            state::remove(repo, &record.name);
        }
    }
    let entries = worktrees
        .into_iter()
        .skip(1)
        .map(|w| {
            let record = records.iter().find(|r| same_path(&r.path, &w.path));
            ListEntry {
                name: record.map_or_else(|| dir_name(&w.path), |r| r.name.clone()),
                status: record.map_or("unmanaged".into(), |r| {
                    format!("{:?}", r.status).to_lowercase()
                }),
                origin: record.map(|r| r.origin),
                created_at: record.map(|r| r.created_at),
                path: w.path,
                branch: w.branch,
                head: w.head,
                locked: w.locked,
            }
        })
        .collect();
    Ok(entries)
}

/// Finds a workspace by its name, or a linked worktree by directory name or path.
pub fn find(repo: &Repo, name: &str) -> Result<(String, PathBuf)> {
    if let Some(record) = state::load(repo, name) {
        return Ok((record.name, record.path));
    }
    let wanted = Path::new(name);
    git::list_worktrees(&repo.main)?
        .into_iter()
        .skip(1)
        .find(|w| dir_name(&w.path) == name || (wanted.is_absolute() && same_path(&w.path, wanted)))
        .map(|w| (sanitize_name(&dir_name(&w.path)), w.path))
        .with_context(|| format!("no workspace named {name:?} (see `copytree list`)"))
}

#[derive(Debug, Clone, Default)]
pub struct RmOptions {
    /// Remove even with untracked files or a lock.
    pub force: bool,
    pub delete_branch: bool,
    /// Delete the files before returning instead of in the background.
    pub wait: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct Removed {
    pub name: String,
    pub path: PathBuf,
    /// `refs/copytree/<name>`: the branch tip, or a stash commit holding uncommitted edits.
    pub saved_ref: Option<String>,
    pub saved_sha: Option<String>,
}

/// Saves `refs/copytree/<name>`, moves the worktree aside, prunes git's
/// record of it, and deletes the files in the background.
pub fn rm(repo: &Repo, name: &str, opts: &RmOptions, log: &mut dyn FnMut(&str)) -> Result<Removed> {
    let (name, path) = find(repo, name)?;
    let worktree = git::list_worktrees(&repo.main)?
        .into_iter()
        .find(|w| same_path(&w.path, &path));

    let mut saved = None;
    if path.exists() {
        refuse_untracked(&path, opts.force)?;
        let saved_ref = format!("refs/copytree/{name}");
        let sha = save_ref(repo, &path, &saved_ref)?;
        log(&format!(
            "saved {saved_ref} at {}",
            &sha[..sha.len().min(12)]
        ));
        saved = Some((saved_ref, sha));
    }

    let path_arg = path.to_string_lossy().into_owned();
    if worktree.as_ref().is_some_and(|w| w.locked) {
        if !opts.force {
            bail!(
                "{} is locked; pass --force to remove it anyway",
                path.display()
            );
        }
        git::run(&repo.main, &["worktree", "unlock", &path_arg])?;
    }
    let trash = path.with_file_name(format!(".{name}.copytree-trash-{}", std::process::id()));
    if path.exists() && std::fs::rename(&path, &trash).is_err() {
        git::run(&repo.main, &["worktree", "remove", "--force", &path_arg])?;
    }
    git::run(&repo.main, &["worktree", "prune"])?;
    if trash.exists() {
        delete_in_background(&trash, opts.wait);
    }
    if let Some(branch) = worktree
        .and_then(|w| w.branch)
        .filter(|_| opts.delete_branch)
    {
        git::run(&repo.main, &["branch", "-D", &branch])?;
    }
    state::remove(repo, &name);
    let (saved_ref, saved_sha) = saved.unzip();
    Ok(Removed {
        name,
        path,
        saved_ref,
        saved_sha,
    })
}

fn refuse_untracked(path: &Path, force: bool) -> Result<()> {
    let untracked = git::output(path, &["ls-files", "--others", "--exclude-standard"])?;
    let count = untracked.lines().count();
    if count > 0 && !force {
        bail!(
            "{} has {count} untracked file(s) that the saved ref can't keep; commit them or pass --force",
            path.display()
        );
    }
    Ok(())
}

/// Points `ref_name` at a stash commit of uncommitted tracked edits, or at HEAD when clean.
fn save_ref(repo: &Repo, worktree: &Path, ref_name: &str) -> Result<String> {
    let stash = git::output(worktree, &["stash", "create", "copytree rm"])?;
    let sha = if stash.is_empty() {
        git::output(worktree, &["rev-parse", "HEAD"])?
    } else {
        stash
    };
    git::output(
        &repo.main,
        &["update-ref", "-m", "copytree rm", ref_name, &sha],
    )?;
    Ok(sha)
}

fn delete_in_background(path: &Path, wait: bool) {
    #[cfg(unix)]
    if !wait {
        use std::process::{Command, Stdio};
        let spawned = Command::new("rm")
            .arg("-rf")
            .arg(path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
        if spawned.is_ok() {
            return;
        }
    }
    let _ = wait;
    let _ = std::fs::remove_dir_all(path);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        assert!(valid_name("fix-login_2").is_ok());
        assert!(valid_name("../x").is_err());
        assert!(valid_name(".hidden").is_err());
        assert!(valid_name("").is_err());
        assert!(valid_name("a/b").is_err());
        assert!(valid_name("x.lock").is_err());
        assert_eq!(sanitize_name("my tree!"), "my-tree");
        assert_eq!(sanitize_name("..."), "worktree");
    }
}
