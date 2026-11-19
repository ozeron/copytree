//! Command-line front end, shared by the `copytree` and `git-copytree` binaries.

use crate::config::Config;
use crate::git::{self, Repo};
use crate::workspace::{self, BranchMode, NewOptions, PrepareOptions, RmOptions};
use crate::{clone, deps, hook};
use anyhow::Result;
use clap::{Args, Parser, Subcommand};
use std::path::{Path, PathBuf};

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
struct Cli {
    /// Run as if started in this directory.
    #[arg(short = 'C', global = true, value_name = "DIR")]
    dir: Option<PathBuf>,
    /// Print only errors and the requested output.
    #[arg(short, long, global = true)]
    quiet: bool,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Create a workspace: git worktree, carried config, deps and caches.
    New(NewArgs),
    /// List linked worktrees and their Copytree status.
    #[command(alias = "ls")]
    List {
        #[arg(long)]
        json: bool,
    },
    /// Print a workspace's path, for `cd "$(copytree path NAME)"`.
    Path { name: String },
    /// Save refs/copytree/<name>, then remove the workspace.
    #[command(alias = "remove")]
    Rm(RmArgs),
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
    /// Entry point for the installed git hook.
    #[command(hide = true)]
    Hook {
        kind: String,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
}

#[derive(Args, Debug)]
struct NewArgs {
    /// Workspace name. Default: ws-1, ws-2, and so on.
    name: Option<String>,
    /// Commit to start from. Default: the main worktree's HEAD.
    #[arg(long, value_name = "COMMIT", conflicts_with = "checkout")]
    base: Option<String>,
    /// Name of the new branch. Default: <branch_prefix><name>.
    #[arg(short = 'b', long, value_name = "BRANCH", conflicts_with_all = ["checkout", "detach"])]
    branch: Option<String>,
    /// Check out an existing branch instead of creating one.
    #[arg(long, value_name = "BRANCH", conflicts_with = "detach")]
    checkout: Option<String>,
    /// Use a detached HEAD instead of a branch.
    #[arg(long)]
    detach: bool,
    /// Put the workspace here instead of in the pool.
    #[arg(long, value_name = "DIR")]
    path: Option<PathBuf>,
    #[arg(long)]
    no_deps: bool,
    #[arg(long)]
    no_seed: bool,
    /// Don't carry .env and .worktreeinclude files.
    #[arg(long)]
    no_carry: bool,
    /// Carry listed files even when they aren't gitignored.
    #[arg(long)]
    force_carry: bool,
    /// Keep the workspace when a step fails, instead of rolling it back.
    #[arg(long)]
    keep_on_failure: bool,
    /// Print only the workspace path on stdout. This is also the default output.
    #[arg(long, conflicts_with = "json")]
    print_path: bool,
    /// Print a JSON report on stdout.
    #[arg(long)]
    json: bool,
    /// Print how long each step took.
    #[arg(long)]
    timings: bool,
}

#[derive(Args, Debug)]
struct RmArgs {
    #[arg(required = true)]
    names: Vec<String>,
    /// Remove even with untracked files or a lock.
    #[arg(short, long)]
    force: bool,
    /// Also delete the branch. The saved ref still keeps its commits.
    #[arg(short = 'D', long)]
    delete_branch: bool,
    /// Delete the files before returning instead of in the background.
    #[arg(long)]
    wait: bool,
    #[arg(long)]
    json: bool,
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
    let cwd = match cli.dir {
        Some(dir) => dir,
        None => std::env::current_dir()?,
    };
    let quiet = cli.quiet;
    let mut log = |message: &str| {
        if !quiet {
            eprintln!("copytree: {message}");
        }
    };
    if let Command::Hook { kind, args } = &cli.command {
        run_hook(&cwd, kind, args, &mut log);
        return Ok(0);
    }

    let repo = Repo::discover(&cwd)?;
    let cfg = Config::load(&repo.main)?;
    match cli.command {
        Command::New(args) => new(&repo, &cfg, &cwd, args, &mut log)?,
        Command::List { json } => list(&repo, json)?,
        Command::Path { name } => println!("{}", workspace::find(&repo, &name)?.1.display()),
        Command::Rm(args) => return rm(&repo, args, &mut log),
        Command::Doctor { json } => doctor(&repo, &cfg, json)?,
        Command::Config => print!("{}", toml::to_string_pretty(&cfg)?),
        Command::InstallHook => install_hook(&repo)?,
        Command::UninstallHook => {
            let removed = hook::uninstall(&repo)?;
            println!(
                "{}",
                if removed {
                    "removed the post-checkout hook"
                } else {
                    "no Copytree hook installed"
                }
            );
        }
        Command::Hook { .. } => {}
    }
    Ok(0)
}

/// The hook never fails the user's git command.
fn run_hook(cwd: &Path, kind: &str, args: &[String], log: &mut dyn FnMut(&str)) {
    if kind != "post-checkout" {
        return;
    }
    if let Err(e) = hook::post_checkout(cwd, args, log) {
        eprintln!("copytree: warning: could not prepare the worktree: {e:#}");
    }
}

fn new(
    repo: &Repo,
    cfg: &Config,
    cwd: &Path,
    args: NewArgs,
    log: &mut dyn FnMut(&str),
) -> Result<()> {
    let branch = match (args.branch, args.checkout, args.detach) {
        (Some(name), _, _) => BranchMode::CreateNamed(name),
        (_, Some(name), _) => BranchMode::CheckoutExisting(name),
        (_, _, true) => BranchMode::Detach,
        _ => BranchMode::Create,
    };
    let opts = NewOptions {
        name: args.name,
        base: args.base,
        branch,
        path: args.path.map(|p| cwd.join(p)),
        prepare: PrepareOptions {
            no_deps: args.no_deps,
            no_seed: args.no_seed,
            no_carry: args.no_carry,
            force_carry: args.force_carry,
        },
        keep_on_failure: args.keep_on_failure,
    };
    let prepared = workspace::new(repo, cfg, &opts, log)?;
    if args.timings {
        for (step, ms) in &prepared.timings {
            eprintln!("  {step:<12} {ms:>7} ms");
        }
    }
    if args.json {
        println!("{}", serde_json::to_string_pretty(&prepared)?);
    } else {
        println!("{}", prepared.workspace.path.display());
    }
    Ok(())
}

fn list(repo: &Repo, json: bool) -> Result<()> {
    let entries = workspace::list(repo)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&entries)?);
        return Ok(());
    }
    if entries.is_empty() {
        eprintln!("no workspaces; create one with `copytree new <name>`");
        return Ok(());
    }
    let branch_of =
        |e: &workspace::ListEntry| e.branch.clone().unwrap_or_else(|| "(detached)".into());
    let name_width = entries
        .iter()
        .map(|e| e.name.len())
        .max()
        .unwrap_or(0)
        .max(4);
    let branch_width = entries
        .iter()
        .map(|e| branch_of(e).len())
        .max()
        .unwrap_or(0)
        .max(6);
    println!(
        "{:<name_width$}  {:<branch_width$}  {:<9}  PATH",
        "NAME", "BRANCH", "STATUS"
    );
    for e in &entries {
        let locked = if e.locked { "  [locked]" } else { "" };
        println!(
            "{:<name_width$}  {:<branch_width$}  {:<9}  {}{locked}",
            e.name,
            branch_of(e),
            e.status,
            e.path.display()
        );
    }
    Ok(())
}

fn rm(repo: &Repo, args: RmArgs, log: &mut dyn FnMut(&str)) -> Result<i32> {
    let opts = RmOptions {
        force: args.force,
        delete_branch: args.delete_branch,
        wait: args.wait,
    };
    let mut removed = Vec::new();
    let mut exit_code = 0;
    for name in &args.names {
        match workspace::rm(repo, name, &opts, log) {
            Ok(r) => removed.push(r),
            Err(e) => {
                eprintln!("copytree: error: {name}: {e:#}");
                exit_code = 1;
            }
        }
    }
    if args.json {
        println!("{}", serde_json::to_string_pretty(&removed)?);
        return Ok(exit_code);
    }
    for r in &removed {
        match &r.saved_ref {
            Some(saved_ref) => println!(
                "removed {} (restore with `git checkout -b <branch> {saved_ref}`)",
                r.name
            ),
            None => println!("removed {}", r.name),
        }
    }
    Ok(exit_code)
}

fn install_hook(repo: &Repo) -> Result<()> {
    println!("installed {}", hook::install(repo)?.display());
    if git::output(&repo.main, &["config", "--get", "core.hooksPath"]).is_ok() {
        eprintln!(
            "note: core.hooksPath is set; if a tool like husky rewrites that folder, run `copytree install-hook` again"
        );
    }
    Ok(())
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
    tools_on_path: Vec<&'static str>,
}

fn doctor(repo: &Repo, cfg: &Config, json: bool) -> Result<()> {
    let pool = workspace::pool_dir(repo, cfg);
    let report = Doctor {
        git: git::version(),
        main_worktree: repo.main.clone(),
        backend: clone::probe(
            &repo.state_dir(),
            pool.ancestors().find(|p| p.exists()).unwrap_or(&repo.main),
        ),
        free_mib: workspace::free_bytes(&pool).map(|b| b >> 20),
        hook_installed: hook::is_installed(repo),
        hooks_dir: hook::hooks_dir(repo),
        node: deps::detect_node(&repo.main),
        python: deps::detect_python(&repo.main),
        tools_on_path: ["pnpm", "npm", "yarn", "bun", "uv", "poetry", "pixi"]
            .into_iter()
            .filter(|t| deps::on_path(t))
            .collect(),
        pool,
    };
    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(());
    }
    let backend = if report.backend.is_cow() {
        "copy-on-write"
    } else {
        "plain copy (no copy-on-write here, so large caches are skipped)"
    };
    let or_none = |value: Option<String>| value.unwrap_or_else(|| "none detected".into());
    println!(
        "git            {}",
        report.git.as_deref().unwrap_or("NOT FOUND")
    );
    println!("main worktree  {}", report.main_worktree.display());
    println!("pool           {}", report.pool.display());
    println!("clone backend  {backend}");
    if let Some(free) = report.free_mib {
        println!("free space     {} GiB", free / 1024);
    }
    println!(
        "hook           {} ({})",
        if report.hook_installed { "yes" } else { "no" },
        report.hooks_dir.display()
    );
    println!(
        "node           {}",
        or_none(report.node.map(|p| format!("{p:?}")))
    );
    println!(
        "python         {}",
        or_none(report.python.map(|p| format!("{p:?}")))
    );
    println!("tools on PATH  {}", report.tools_on_path.join(", "));
    if report.python == Some(deps::PythonTool::Poetry) {
        println!(
            "note: Poetry copies packages instead of linking them, so installs are slower than with uv"
        );
    }
    Ok(())
}
