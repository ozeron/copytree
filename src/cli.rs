//! Command-line front end, shared by the `copytree` and `git-copytree` binaries.

use crate::clone;
use crate::config::Config;
use crate::deps;
use crate::git::{self, Repo};
use crate::hook;
use crate::workspace::{self, NewOptions, RmOptions};
use anyhow::{Result, bail};
use clap::{Args, Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(
    name = "copytree",
    version,
    about = "Fast git worktrees for parallel coding agents",
    long_about = "Fast git worktrees for parallel coding agents.\n\n\
        Each workspace is a normal linked git worktree. Copytree adds what worktrees lack: \
        dependencies installed from your package manager's shared store, build caches cloned \
        with copy-on-write, .env carried over, and a saved ref before anything is deleted."
)]
pub struct Cli {
    /// Run as if started in this directory.
    #[arg(short = 'C', global = true, value_name = "DIR")]
    dir: Option<PathBuf>,
    /// Only print errors (and the requested output).
    #[arg(short, long, global = true)]
    quiet: bool,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Create a workspace: git worktree + config + deps + caches.
    New(NewArgs),
    /// List linked worktrees and their Copytree status.
    #[command(alias = "ls")]
    List {
        #[arg(long)]
        json: bool,
    },
    /// Print a workspace's path (for `cd "$(copytree path NAME)"`).
    Path { name: String },
    /// Save refs/copytree/<name>, then remove the workspace.
    #[command(alias = "remove")]
    Rm {
        #[arg(required = true)]
        names: Vec<String>,
        /// Remove even with untracked files or a lock.
        #[arg(short, long)]
        force: bool,
        /// Also delete the workspace's branch (the saved ref still keeps the commits).
        #[arg(short = 'D', long)]
        delete_branch: bool,
        /// Delete files before returning instead of in the background.
        #[arg(long)]
        wait: bool,
        #[arg(long)]
        json: bool,
    },
    /// Check git, the clone backend, package managers and the hook.
    Doctor {
        #[arg(long)]
        json: bool,
    },
    /// Print the effective configuration as TOML.
    Config,
    /// Install a post-checkout hook so worktrees made by any tool get prepared.
    InstallHook,
    /// Remove the post-checkout hook and restore the one it replaced.
    UninstallHook,
    /// Entry point for git hooks (used by the installed hook).
    #[command(hide = true)]
    Hook {
        kind: String,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
}

#[derive(Args, Debug)]
struct NewArgs {
    /// Workspace name (default: ws-1, ws-2, ...). Also names the branch <prefix><name>.
    name: Option<String>,
    /// Commit-ish to start from (default: the main worktree's HEAD).
    #[arg(long, value_name = "COMMIT")]
    base: Option<String>,
    /// Name of the new branch (default: ct/<name>).
    #[arg(short = 'b', long, value_name = "BRANCH", conflicts_with_all = ["checkout", "detach"])]
    branch: Option<String>,
    /// Check out an existing branch instead of creating one.
    #[arg(long, value_name = "BRANCH", conflicts_with = "detach")]
    checkout: Option<String>,
    /// Detached HEAD, no branch.
    #[arg(long)]
    detach: bool,
    /// Put the workspace here instead of in the pool.
    #[arg(long, value_name = "DIR")]
    path: Option<PathBuf>,
    /// Skip dependency installs.
    #[arg(long)]
    no_deps: bool,
    /// Skip cache seeding.
    #[arg(long)]
    no_seed: bool,
    /// Don't carry .env and .worktreeinclude files.
    #[arg(long)]
    no_carry: bool,
    /// Carry listed files even when they are not gitignored.
    #[arg(long)]
    force_carry: bool,
    /// Keep the workspace if a step fails (default: roll back).
    #[arg(long)]
    keep_on_failure: bool,
    /// Print only the workspace path on stdout.
    #[arg(long, conflicts_with = "json")]
    print_path: bool,
    /// Print a JSON report on stdout.
    #[arg(long)]
    json: bool,
    /// Print how long each step took.
    #[arg(long)]
    timings: bool,
}

pub fn main_with(args: impl IntoIterator<Item = std::ffi::OsString>) -> i32 {
    let cli = Cli::parse_from(args);
    match run(cli) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("copytree: error: {e:#}");
            1
        }
    }
}

fn run(cli: Cli) -> Result<i32> {
    let cwd = match &cli.dir {
        Some(d) => d.clone(),
        None => std::env::current_dir()?,
    };
    let quiet = cli.quiet;
    let mut log = |m: &str| {
        if !quiet {
            eprintln!("copytree: {m}");
        }
    };

    // The hook must never break the user's git command.
    if let Cmd::Hook { kind, args } = &cli.cmd {
        if kind == "post-checkout"
            && let Err(e) = hook::post_checkout(&cwd, args, &mut log)
        {
            eprintln!("copytree: warning: could not prepare worktree: {e:#}");
        }
        return Ok(0);
    }

    let repo = Repo::discover(&cwd)?;
    let cfg = Config::load(&repo.main)?;

    match cli.cmd {
        Cmd::New(a) => {
            let opts = NewOptions {
                name: a.name,
                base: a.base,
                branch: a.branch.or(a.checkout.clone()),
                checkout_existing: a.checkout.is_some(),
                detach: a.detach,
                path: a
                    .path
                    .map(|p| if p.is_absolute() { p } else { cwd.join(p) }),
                no_deps: a.no_deps,
                no_seed: a.no_seed,
                no_carry: a.no_carry,
                force_carry: a.force_carry,
                keep_on_failure: a.keep_on_failure,
            };
            let p = workspace::new(&repo, &cfg, &opts, &mut log)?;
            if a.timings {
                for (step, ms) in &p.timings {
                    eprintln!("  {step:<12} {ms:>7} ms");
                }
            }
            if a.json {
                println!("{}", serde_json::to_string_pretty(&p)?);
            } else {
                // Default and --print-path both print just the path on stdout.
                println!("{}", p.workspace.path.display());
            }
        }
        Cmd::List { json } => {
            let entries = workspace::list(&repo)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&entries)?);
            } else if entries.is_empty() {
                eprintln!("no workspaces; create one with `copytree new <name>`");
            } else {
                let w = entries
                    .iter()
                    .map(|e| e.name.len())
                    .max()
                    .unwrap_or(4)
                    .max(4);
                let bw = entries
                    .iter()
                    .map(|e| e.branch.as_deref().unwrap_or("(detached)").len())
                    .max()
                    .unwrap_or(6)
                    .max(6);
                println!("{:<w$}  {:<bw$}  {:<9}  PATH", "NAME", "BRANCH", "STATUS");
                for e in entries {
                    println!(
                        "{:<w$}  {:<bw$}  {:<9}  {}{}",
                        e.name,
                        e.branch.as_deref().unwrap_or("(detached)"),
                        e.status,
                        e.path.display(),
                        if e.locked { "  [locked]" } else { "" }
                    );
                }
            }
        }
        Cmd::Path { name } => {
            let (path, _) = workspace::find(&repo, &name)?;
            println!("{}", path.display());
        }
        Cmd::Rm {
            names,
            force,
            delete_branch,
            wait,
            json,
        } => {
            let opts = RmOptions {
                force,
                delete_branch,
                wait,
            };
            let mut removed = Vec::new();
            let mut failed = false;
            for n in names {
                match workspace::rm(&repo, &n, &opts, &mut log) {
                    Ok(r) => {
                        if !json {
                            match &r.saved_ref {
                                Some(rf) => println!(
                                    "removed {} (restore with `git checkout -b <branch> {rf}`)",
                                    r.name
                                ),
                                None => println!("removed {}", r.name),
                            }
                        }
                        removed.push(r);
                    }
                    Err(e) => {
                        eprintln!("copytree: error: {n}: {e:#}");
                        failed = true;
                    }
                }
            }
            if json {
                println!("{}", serde_json::to_string_pretty(&removed)?);
            }
            return Ok(if failed { 1 } else { 0 });
        }
        Cmd::Doctor { json } => doctor(&repo, &cfg, json)?,
        Cmd::Config => print!("{}", toml::to_string_pretty(&cfg)?),
        Cmd::InstallHook => {
            let p = hook::install(&repo)?;
            println!("installed {}", p.display());
            if git::output(&repo.main, ["config", "--get", "core.hooksPath"]).is_ok() {
                eprintln!(
                    "note: core.hooksPath is set; if a tool like husky regenerates that directory, run `copytree install-hook` again afterwards"
                );
            }
        }
        Cmd::UninstallHook => {
            if hook::uninstall(&repo)? {
                println!("removed the post-checkout hook");
            } else {
                println!("no Copytree hook installed");
            }
        }
        Cmd::Hook { .. } => unreachable!(),
    }
    Ok(0)
}

#[derive(serde::Serialize)]
struct Doctor {
    git: Option<String>,
    main_worktree: PathBuf,
    pool: PathBuf,
    backend: clone::Backend,
    free_mib: Option<u64>,
    hook_installed: bool,
    hooks_dir: PathBuf,
    node: Option<deps::NodePm>,
    python: Option<deps::PythonTool>,
    tools: Vec<(String, bool)>,
}

fn doctor(repo: &Repo, cfg: &Config, json: bool) -> Result<()> {
    let pool = workspace::pool_dir(repo, cfg);
    let probe_dir = if pool.exists() {
        pool.clone()
    } else {
        pool.parent().unwrap_or(&repo.main).to_path_buf()
    };
    let d = Doctor {
        git: git::version(),
        main_worktree: repo.main.clone(),
        pool: pool.clone(),
        backend: clone::probe(&probe_dir),
        free_mib: workspace::free_bytes(&pool).map(|b| b >> 20),
        hook_installed: hook::is_installed(repo),
        hooks_dir: hook::hooks_dir(repo),
        node: deps::detect_node(&repo.main),
        python: deps::detect_python(&repo.main),
        tools: ["pnpm", "npm", "yarn", "bun", "uv", "poetry", "pixi"]
            .iter()
            .map(|t| (t.to_string(), deps::which(t)))
            .collect(),
    };
    if json {
        println!("{}", serde_json::to_string_pretty(&d)?);
        return Ok(());
    }
    let yes = |b: bool| if b { "yes" } else { "no" };
    println!("git            {}", d.git.as_deref().unwrap_or("NOT FOUND"));
    println!("main worktree  {}", d.main_worktree.display());
    println!("pool           {}", d.pool.display());
    println!(
        "clone backend  {}",
        if d.backend.is_cow() {
            "copy-on-write (APFS, btrfs, XFS or ReFS)"
        } else {
            "plain copy (no copy-on-write on this volume; caches over the copy limit are skipped)"
        }
    );
    if let Some(f) = d.free_mib {
        println!("free space     {} GiB", f / 1024);
    }
    println!(
        "hook           {} ({})",
        yes(d.hook_installed),
        d.hooks_dir.display()
    );
    println!(
        "node           {}",
        d.node
            .map(|p| format!("{p:?}"))
            .unwrap_or_else(|| "none detected".into())
    );
    println!(
        "python         {}",
        d.python
            .map(|p| format!("{p:?}"))
            .unwrap_or_else(|| "none detected".into())
    );
    let found: Vec<_> = d
        .tools
        .iter()
        .filter(|t| t.1)
        .map(|t| t.0.as_str())
        .collect();
    println!(
        "tools on PATH  {}",
        if found.is_empty() {
            "none".into()
        } else {
            found.join(", ")
        }
    );
    if d.python == Some(deps::PythonTool::Poetry) {
        println!(
            "note: Poetry copies packages instead of linking them, so installs are slower than uv"
        );
    }
    if d.git.is_none() {
        bail!("git not found");
    }
    Ok(())
}
