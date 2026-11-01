//! The workspace lifecycle: `new`, `prepare` (also run by the hook), `list`, `rm`.

use crate::clone;
use crate::config::Config;
use crate::deps::{self, DepReport};
use crate::git::{self, Repo};
use crate::seed::{self, SeedReport};
use crate::state::{self, Origin, Status, Workspace};
use anyhow::{Context, Result, anyhow, bail};
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::time::Instant;

/// Minimum free space on the pool's volume before `new` will create a workspace.
pub const MIN_FREE_BYTES: u64 = 1 << 30;

#[derive(Debug, Clone, Default)]
pub struct NewOptions {
    pub name: Option<String>,
    /// Commit-ish to start from. Default: main worktree's HEAD.
    pub base: Option<String>,
    /// Use this branch name instead of `<prefix><name>`.
    pub branch: Option<String>,
    /// Check out an existing branch instead of creating one.
    pub checkout_existing: bool,
    pub detach: bool,
    pub path: Option<PathBuf>,
    pub no_deps: bool,
    pub no_seed: bool,
    pub no_carry: bool,
    pub force_carry: bool,
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
    /// Step name and duration in milliseconds.
    pub timings: Vec<(String, u128)>,
}

pub fn valid_name(name: &str) -> Result<()> {
    let ok = !name.is_empty()
        && name.len() <= 100
        && !name.starts_with(['.', '-'])
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    if !ok {
        bail!("invalid workspace name {name:?}: use letters, digits, '-', '_' and '.'");
    }
    Ok(())
}

fn generate_name(repo: &Repo, prefix: &str) -> String {
    let taken: Vec<String> = state::all(repo).into_iter().map(|w| w.name).collect();
    (1..)
        .map(|i| format!("ws-{i}"))
        .find(|n| {
            !taken.contains(n)
                && !git::succeeds(
                    &repo.main,
                    [
                        "rev-parse",
                        "--verify",
                        "--quiet",
                        &format!("refs/heads/{prefix}{n}"),
                    ],
                )
        })
        .unwrap()
}

pub fn pool_dir(repo: &Repo, cfg: &Config) -> PathBuf {
    match &cfg.pool {
        Some(p) if p.is_absolute() => p.clone(),
        Some(p) => repo.main.join(p),
        None => repo.default_pool(),
    }
}

#[cfg(unix)]
pub fn free_bytes(path: &Path) -> Option<u64> {
    use std::os::unix::ffi::OsStrExt;
    let mut p = path;
    while !p.exists() {
        p = p.parent()?;
    }
    let c = std::ffi::CString::new(p.as_os_str().as_bytes()).ok()?;
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c.as_ptr(), &mut st) } != 0 {
        return None;
    }
    Some(st.f_bavail as u64 * st.f_frsize as u64)
}

#[cfg(not(unix))]
pub fn free_bytes(_path: &Path) -> Option<u64> {
    None
}

/// Create a workspace: `git worktree add`, then [`prepare`].
/// If any step fails, the worktree and its new branch are removed again
/// unless `keep_on_failure` is set.
pub fn new(
    repo: &Repo,
    cfg: &Config,
    opts: &NewOptions,
    log: &mut dyn FnMut(&str),
) -> Result<Prepared> {
    let t0 = Instant::now();
    let name = match &opts.name {
        Some(n) => n.clone(),
        None => generate_name(repo, &cfg.branch_prefix),
    };
    valid_name(&name)?;
    if state::load(repo, &name).is_some() {
        bail!("workspace {name:?} already exists (see `copytree list`)");
    }

    // 1. Admit.
    let pool = pool_dir(repo, cfg);
    let dir = opts.path.clone().unwrap_or_else(|| pool.join(&name));
    if dir.exists() {
        bail!("{} already exists", dir.display());
    }
    std::fs::create_dir_all(dir.parent().unwrap_or(&pool))
        .with_context(|| format!("creating {}", pool.display()))?;
    if let Some(free) = free_bytes(&dir)
        && free < MIN_FREE_BYTES
    {
        bail!(
            "only {} MiB free on the pool volume; need at least {} MiB",
            free >> 20,
            MIN_FREE_BYTES >> 20
        );
    }

    // 2. Check out.
    let base = opts.base.clone().unwrap_or_else(|| "HEAD".into());
    let base_sha = git::output(
        &repo.main,
        ["rev-parse", "--verify", &format!("{base}^{{commit}}")],
    )
    .with_context(|| format!("unknown base {base:?}"))?;
    let mut args: Vec<String> = vec!["worktree".into(), "add".into()];
    let mut created_branch = None;
    let branch = if opts.detach {
        args.extend([
            "--detach".into(),
            dir.display().to_string(),
            base_sha.clone(),
        ]);
        None
    } else if opts.checkout_existing {
        let b = opts
            .branch
            .clone()
            .ok_or_else(|| anyhow!("--checkout needs a branch name"))?;
        args.extend([dir.display().to_string(), b.clone()]);
        Some(b)
    } else {
        let b = opts
            .branch
            .clone()
            .unwrap_or_else(|| format!("{}{name}", cfg.branch_prefix));
        args.extend([
            "-b".into(),
            b.clone(),
            dir.display().to_string(),
            base_sha.clone(),
        ]);
        created_branch = Some(b.clone());
        Some(b)
    };
    let t = Instant::now();
    log(&format!("checkout: git {}", args.join(" ")));
    git::run(&repo.main, &args)?;
    let checkout_ms = t.elapsed().as_millis();

    let ws = Workspace {
        name: name.clone(),
        path: dir.clone(),
        branch,
        base: base_sha,
        created_at: state::now(),
        status: Status::Preparing,
        origin: Origin::New,
        backend: None,
        main_fingerprints: Default::default(),
        carried: Vec::new(),
        error: None,
    };
    state::save(repo, &ws)?;

    let popts = PrepareOptions {
        no_deps: opts.no_deps,
        no_seed: opts.no_seed,
        no_carry: opts.no_carry,
        force_carry: opts.force_carry,
    };
    match prepare(repo, cfg, ws, &popts, log) {
        Ok(mut p) => {
            p.timings.insert(0, ("checkout".into(), checkout_ms));
            p.timings.push(("total".into(), t0.elapsed().as_millis()));
            Ok(p)
        }
        Err(e) => {
            if opts.keep_on_failure {
                log(&format!("keeping failed workspace at {}", dir.display()));
            } else {
                log("rolling back the half-made workspace");
                let _ = git::run(
                    &repo.main,
                    ["worktree", "remove", "--force", &dir.display().to_string()],
                );
                let _ = std::fs::remove_dir_all(&dir);
                let _ = git::run(&repo.main, ["worktree", "prune"]);
                if let Some(b) = created_branch {
                    let _ = git::run(&repo.main, ["branch", "-D", &b]);
                }
                state::remove(repo, &name);
            }
            Err(e)
        }
    }
}

/// Fill a freshly checked-out worktree: carry config, install deps, seed caches,
/// run `post_create` hooks. Shared by `new` and hook mode.
pub fn prepare(
    repo: &Repo,
    cfg: &Config,
    mut ws: Workspace,
    opts: &PrepareOptions,
    log: &mut dyn FnMut(&str),
) -> Result<Prepared> {
    let mut timings = Vec::new();
    let main = &repo.main;
    let dir = ws.path.clone();

    let t = Instant::now();
    let backend = clone::probe(dir.parent().unwrap_or(&dir));
    ws.backend = Some(backend);
    ws.main_fingerprints = deps::fingerprints(main);
    timings.push(("probe".into(), t.elapsed().as_millis()));
    log(&format!(
        "volume: {}",
        if backend.is_cow() {
            "copy-on-write clones available"
        } else {
            "no copy-on-write; caches are copied only when small"
        }
    ));

    let result = (|| -> Result<(Vec<DepReport>, Vec<SeedReport>)> {
        // Carry first: installs and codegen often read .env.
        if !opts.no_carry {
            let t = Instant::now();
            ws.carried = seed::carry(main, &dir, &cfg.carry_patterns(main), opts.force_carry, log)?;
            timings.push(("carry".into(), t.elapsed().as_millis()));
        }
        let mut deps_r = Vec::new();
        if !opts.no_deps {
            let t = Instant::now();
            deps_r = deps::install(main, &dir, cfg, backend, log)?;
            timings.push(("deps".into(), t.elapsed().as_millis()));
        }
        let mut seed_r = Vec::new();
        if !opts.no_seed {
            let t = Instant::now();
            let ignored = git::ignored_roots(main)?;
            let targets = seed::resolve_targets(main, &cfg.seed_paths(), &ignored);
            seed_r = seed::seed(
                main,
                &dir,
                &targets,
                backend,
                cfg.seed.copy_limit_mib << 20,
                log,
            );
            timings.push(("seed".into(), t.elapsed().as_millis()));
        }
        if !cfg.hooks.post_create.is_empty() {
            let t = Instant::now();
            for cmd in &cfg.hooks.post_create {
                log(&format!("post_create: {cmd}"));
                run_hook(&dir, cmd, &ws)?;
            }
            timings.push(("post_create".into(), t.elapsed().as_millis()));
        }
        Ok((deps_r, seed_r))
    })();

    match result {
        Ok((deps_r, seed_r)) => {
            ws.status = Status::Ready;
            state::save(repo, &ws)?;
            Ok(Prepared {
                workspace: ws,
                deps: deps_r,
                seed: seed_r,
                timings,
            })
        }
        Err(e) => {
            ws.status = Status::Failed;
            ws.error = Some(format!("{e:#}"));
            let _ = state::save(repo, &ws);
            Err(e)
        }
    }
}

fn run_hook(dir: &Path, cmd: &str, ws: &Workspace) -> Result<()> {
    let (sh, flag) = if cfg!(windows) {
        ("cmd", "/C")
    } else {
        ("sh", "-c")
    };
    let status = std::process::Command::new(sh)
        .args([flag, cmd])
        .current_dir(dir)
        .env("COPYTREE_NAME", &ws.name)
        .env("COPYTREE_PATH", &ws.path)
        .stdin(std::process::Stdio::null())
        .stdout(std::io::stderr())
        .status()
        .with_context(|| format!("running post_create `{cmd}`"))?;
    if !status.success() {
        bail!("post_create `{cmd}` failed ({status})");
    }
    Ok(())
}

#[derive(Debug, Clone, Serialize)]
pub struct ListEntry {
    pub name: String,
    pub path: PathBuf,
    pub branch: Option<String>,
    pub head: Option<String>,
    /// `ready`, `preparing`, `failed`, or `unmanaged` for worktrees Copytree didn't prepare.
    pub status: String,
    pub origin: Option<Origin>,
    pub created_at: Option<u64>,
    pub locked: bool,
}

/// All linked worktrees, joined with Copytree's state. State for worktrees that
/// were removed behind Copytree's back (`git worktree remove`, `prune`) is
/// cleaned up as a side effect.
pub fn list(repo: &Repo) -> Result<Vec<ListEntry>> {
    let wts = git::list_worktrees(&repo.main)?;
    let states = state::all(repo);
    for s in &states {
        if !wts.iter().any(|w| same_path(&w.path, &s.path)) && s.status != Status::Preparing {
            state::remove(repo, &s.name);
        }
    }
    let mut out = Vec::new();
    for w in wts.iter().skip(1).filter(|w| !w.bare) {
        let st = states.iter().find(|s| same_path(&s.path, &w.path));
        out.push(ListEntry {
            name: st.map(|s| s.name.clone()).unwrap_or_else(|| {
                w.path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default()
            }),
            path: w.path.clone(),
            branch: w.branch.clone(),
            head: w.head.clone(),
            status: st
                .map(|s| format!("{:?}", s.status).to_lowercase())
                .unwrap_or_else(|| "unmanaged".into()),
            origin: st.map(|s| s.origin),
            created_at: st.map(|s| s.created_at),
            locked: w.locked,
        });
    }
    Ok(out)
}

fn same_path(a: &Path, b: &Path) -> bool {
    a == b
        || std::fs::canonicalize(a)
            .ok()
            .is_some_and(|ca| std::fs::canonicalize(b).ok() == Some(ca))
}

/// Find a workspace by name, falling back to any linked worktree whose
/// directory name or path matches.
pub fn find(repo: &Repo, name: &str) -> Result<(PathBuf, Option<Workspace>)> {
    if let Some(s) = state::load(repo, name) {
        return Ok((s.path.clone(), Some(s)));
    }
    let wts = git::list_worktrees(&repo.main)?;
    let p = Path::new(name);
    for w in wts.iter().skip(1) {
        if w.path.file_name().is_some_and(|n| n == name)
            || (p.is_absolute() && same_path(&w.path, p))
        {
            return Ok((w.path.clone(), None));
        }
    }
    bail!("no workspace named {name:?} (see `copytree list`)")
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
    /// `refs/copytree/<name>`, pointing at the tip or at a stash commit of
    /// uncommitted tracked changes.
    pub saved_ref: Option<String>,
    pub saved_sha: Option<String>,
}

/// Save `refs/copytree/<name>`, then remove the worktree quickly: rename it
/// into a trash dir, prune git's metadata, and delete the files in the background.
pub fn rm(repo: &Repo, name: &str, opts: &RmOptions, log: &mut dyn FnMut(&str)) -> Result<Removed> {
    let (path, st) = find(repo, name)?;
    if same_path(&path, &repo.main) {
        bail!("refusing to remove the main worktree");
    }
    let ref_name = st.as_ref().map(|s| s.name.clone()).unwrap_or_else(|| {
        path.file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| name.into())
    });
    let wt = git::list_worktrees(&repo.main)?
        .into_iter()
        .find(|w| same_path(&w.path, &path));

    let mut saved = None;
    if path.exists() {
        let untracked = git::output(&path, ["ls-files", "--others", "--exclude-standard"])?;
        let n_untracked = untracked.lines().filter(|l| !l.is_empty()).count();
        if n_untracked > 0 && !opts.force {
            bail!(
                "{} has {n_untracked} untracked file(s) that a saved ref can't keep; commit them or pass --force",
                path.display()
            );
        }
        // A stash commit captures uncommitted tracked edits without touching the tree.
        let stash = git::output(&path, ["stash", "create", "copytree rm"]).unwrap_or_default();
        let sha = if stash.is_empty() {
            git::output(&path, ["rev-parse", "HEAD"]).ok()
        } else {
            Some(stash)
        };
        if let Some(sha) = sha {
            let r = format!("refs/copytree/{ref_name}");
            git::output(&repo.main, ["update-ref", "-m", "copytree rm", &r, &sha])?;
            log(&format!("saved {r} -> {}", &sha[..sha.len().min(12)]));
            saved = Some((r, sha));
        }
    }

    if wt.as_ref().is_some_and(|w| w.locked) {
        if !opts.force {
            bail!(
                "{} is locked; pass --force to remove it anyway",
                path.display()
            );
        }
        git::run(
            &repo.main,
            ["worktree", "unlock", &path.display().to_string()],
        )?;
    }

    if path.exists() {
        let trash_root = path.parent().unwrap_or(&path).join(".trash");
        std::fs::create_dir_all(&trash_root)?;
        let trash = trash_root.join(format!(
            "{ref_name}-{}-{}",
            state::now(),
            std::process::id()
        ));
        match std::fs::rename(&path, &trash) {
            Ok(()) => {
                git::run(&repo.main, ["worktree", "prune"])?;
                delete_tree(&trash, opts.wait);
            }
            Err(_) => {
                // Different volume or a busy dir: let git do the slow removal.
                git::run(
                    &repo.main,
                    ["worktree", "remove", "--force", &path.display().to_string()],
                )?;
            }
        }
    } else {
        git::run(&repo.main, ["worktree", "prune"])?;
    }

    if opts.delete_branch
        && let Some(b) = wt.as_ref().and_then(|w| w.branch.clone())
    {
        git::run(&repo.main, ["branch", "-D", &b])?;
    }
    state::remove(repo, &ref_name);
    Ok(Removed {
        name: ref_name,
        path,
        saved_ref: saved.as_ref().map(|s| s.0.clone()),
        saved_sha: saved.map(|s| s.1),
    })
}

fn delete_tree(path: &Path, wait: bool) {
    #[cfg(unix)]
    if !wait {
        use std::process::{Command, Stdio};
        // Detached `rm -rf` so `rm` returns immediately even for huge node_modules.
        if Command::new("rm")
            .arg("-rf")
            .arg(path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .is_ok()
        {
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
    }
}
