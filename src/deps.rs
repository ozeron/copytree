//! Dependency installs for Node and Python.
//!
//! Dependencies are rebuilt from each package manager's shared store, which
//! links or clones files instead of downloading them. `node_modules` is cloned
//! from main only for npm and Yarn classic, only when the lockfile matches,
//! and only with copy-on-write. `.venv` is never cloned, because its scripts
//! and the project's editable install point back at the main worktree.

use crate::clone::{self, Backend};
use crate::config::{Config, DepsMode};
use crate::git;
use anyhow::{Context, Result, bail};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Instant;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum NodePm {
    Pnpm,
    Bun,
    Npm,
    YarnClassic,
    YarnBerry,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum PythonTool {
    Uv,
    UvPip,
    Poetry,
    Pixi,
}

const NODE_LOCK_FILES: &[&str] = &[
    "pnpm-lock.yaml",
    "bun.lock",
    "bun.lockb",
    "yarn.lock",
    "package-lock.json",
    "npm-shrinkwrap.json",
    ".nvmrc",
    ".node-version",
    ".yarnrc.yml",
    ".npmrc",
];

#[derive(Debug, Clone, Serialize)]
pub struct DepReport {
    pub ecosystem: &'static str,
    pub tool: String,
    /// `cloned`, `installed` or `skipped`.
    pub action: &'static str,
    pub detail: String,
    pub millis: u128,
}

pub fn detect_node(dir: &Path) -> Option<NodePm> {
    let has = |f: &str| dir.join(f).exists();
    if has("pnpm-lock.yaml") {
        Some(NodePm::Pnpm)
    } else if has("bun.lock") || has("bun.lockb") {
        Some(NodePm::Bun)
    } else if has("yarn.lock") && has(".yarnrc.yml") {
        Some(NodePm::YarnBerry)
    } else if has("yarn.lock") {
        Some(NodePm::YarnClassic)
    } else if has("package-lock.json") || has("npm-shrinkwrap.json") {
        Some(NodePm::Npm)
    } else {
        None
    }
}

pub fn detect_python(dir: &Path) -> Option<PythonTool> {
    let has = |f: &str| dir.join(f).exists();
    if has("uv.lock") {
        Some(PythonTool::Uv)
    } else if has("pixi.lock") {
        Some(PythonTool::Pixi)
    } else if has("poetry.lock") {
        Some(PythonTool::Poetry)
    } else if has("requirements.txt") {
        Some(PythonTool::UvPip)
    } else {
        None
    }
}

/// Hash of the Node lockfiles and version pins, plus OS and architecture.
fn node_fingerprint(dir: &Path) -> Option<String> {
    let mut hasher = Sha256::new();
    let mut found = false;
    for name in NODE_LOCK_FILES {
        if let Ok(bytes) = std::fs::read(dir.join(name)) {
            found = true;
            hasher.update(name.as_bytes());
            hasher.update(bytes.len().to_le_bytes());
            hasher.update(&bytes);
        }
    }
    hasher.update(std::env::consts::OS);
    hasher.update(std::env::consts::ARCH);
    found.then(|| hex::encode(&hasher.finalize()[..12]))
}

pub fn on_path(bin: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    let names: &[String] = if cfg!(windows) {
        &[format!("{bin}.exe"), format!("{bin}.cmd")]
    } else {
        &[bin.to_string()]
    };
    std::env::split_paths(&path).any(|dir| names.iter().any(|n| dir.join(n).is_file()))
}

/// Runs `cmdline` through the shell in `dir`. `defaults` are set only when the
/// user's environment does not already set them.
pub fn run_shell(dir: &Path, cmdline: &str, defaults: &[(&str, &str)]) -> Result<()> {
    let (shell, flag) = if cfg!(windows) {
        ("cmd", "/C")
    } else {
        ("sh", "-c")
    };
    let mut command = Command::new(shell);
    command
        .args([flag, cmdline])
        .current_dir(dir)
        // A command that runs `git worktree add` of its own must not re-enter
        // the post-checkout hook and prepare a nested workspace.
        .env(git::IN_PROGRESS_ENV, "1")
        .stdin(Stdio::null())
        .stdout(std::io::stderr());
    for (key, value) in defaults {
        if std::env::var_os(key).is_none() {
            command.env(key, value);
        }
    }
    let status = command
        .status()
        .with_context(|| format!("running `{cmdline}`"))?;
    if !status.success() {
        bail!("`{cmdline}` failed in {} ({status})", dir.display());
    }
    Ok(())
}

pub fn install(
    main: &Path,
    ws: &Path,
    cfg: &Config,
    backend: Backend,
    log: &mut dyn FnMut(&str),
) -> Result<Vec<DepReport>> {
    if cfg.deps.mode == DepsMode::Off {
        return Ok(Vec::new());
    }
    let mut reports = Vec::new();
    reports.extend(install_node(main, ws, cfg, backend, log)?);
    reports.extend(install_python(ws, cfg, backend, log)?);
    Ok(reports)
}

fn install_node(
    main: &Path,
    ws: &Path,
    cfg: &Config,
    backend: Backend,
    log: &mut dyn FnMut(&str),
) -> Result<Option<DepReport>> {
    let started = Instant::now();
    if let Some(cmdline) = &cfg.deps.node {
        log(&format!("node: running `{cmdline}`"));
        run_shell(ws, cmdline, &[])?;
        return Ok(Some(report(
            "node",
            "custom",
            "installed",
            cmdline.clone(),
            started,
        )));
    }
    let Some(pm) = detect_node(ws) else {
        return Ok(None);
    };
    let lockfile_matches =
        node_fingerprint(ws).is_some() && node_fingerprint(ws) == node_fingerprint(main);
    let detail = if lockfile_matches {
        "lockfile matches main"
    } else {
        "lockfile differs from main"
    };

    let can_clone = matches!(pm, NodePm::Npm | NodePm::YarnClassic)
        && lockfile_matches
        && backend.is_cow()
        && main.join("node_modules").is_dir()
        && !ws.join("node_modules").exists();
    if can_clone {
        match clone_node_modules(main, ws, backend) {
            Ok(count) => {
                log(&format!(
                    "node: cloned {count} node_modules folder(s) from main"
                ));
                return Ok(Some(report(
                    "node",
                    "clone",
                    "cloned",
                    detail.into(),
                    started,
                )));
            }
            Err(e) => log(&format!("node: clone failed ({e:#}), installing instead")),
        }
    }

    let (bin, cmdline) = match pm {
        NodePm::Pnpm => ("pnpm", "pnpm install --frozen-lockfile --prefer-offline"),
        NodePm::Bun => ("bun", "bun install --frozen-lockfile"),
        NodePm::Npm => ("npm", "npm ci --prefer-offline --no-audit --no-fund"),
        NodePm::YarnClassic => ("yarn", "yarn install --frozen-lockfile --prefer-offline"),
        NodePm::YarnBerry => ("yarn", "yarn install --immutable"),
    };
    if !on_path(bin) {
        log(&format!("node: `{bin}` is not on PATH, skipping"));
        return Ok(Some(report(
            "node",
            bin,
            "skipped",
            format!("{bin} not on PATH"),
            started,
        )));
    }
    log(&format!("node: running `{cmdline}`"));
    run_shell(ws, cmdline, &[])?;
    Ok(Some(report(
        "node",
        bin,
        "installed",
        detail.into(),
        started,
    )))
}

/// Clones every ignored `node_modules` folder in main, including nested
/// workspace packages. On failure, removes what was cloned.
fn clone_node_modules(main: &Path, ws: &Path, backend: Backend) -> Result<usize> {
    let roots: Vec<String> = git::ignored_roots(main)?
        .into_iter()
        .map(|r| r.trim_end_matches('/').to_string())
        .filter(|r| r == "node_modules" || r.ends_with("/node_modules"))
        .collect();
    for root in &roots {
        if let Err(e) = clone::clone_path(&main.join(root), &ws.join(root), backend) {
            for root in &roots {
                let _ = std::fs::remove_dir_all(ws.join(root));
            }
            return Err(e);
        }
    }
    Ok(roots.len())
}

fn install_python(
    ws: &Path,
    cfg: &Config,
    backend: Backend,
    log: &mut dyn FnMut(&str),
) -> Result<Option<DepReport>> {
    let started = Instant::now();
    if let Some(cmdline) = &cfg.deps.python {
        log(&format!("python: running `{cmdline}`"));
        run_shell(ws, cmdline, &[])?;
        return Ok(Some(report(
            "python",
            "custom",
            "installed",
            cmdline.clone(),
            started,
        )));
    }
    let Some(tool) = detect_python(ws) else {
        return Ok(None);
    };
    let (bin, cmdline) = match tool {
        PythonTool::Uv => ("uv", "uv sync --frozen"),
        PythonTool::UvPip => (
            "uv",
            "uv venv --allow-existing && uv pip sync requirements.txt",
        ),
        PythonTool::Poetry => ("poetry", "poetry install --no-interaction"),
        PythonTool::Pixi => ("pixi", "pixi install --frozen"),
    };
    if !on_path(bin) {
        log(&format!("python: `{bin}` is not on PATH, skipping"));
        return Ok(Some(report(
            "python",
            bin,
            "skipped",
            format!("{bin} not on PATH"),
            started,
        )));
    }
    let link_mode = if backend.is_cow() {
        "clone"
    } else {
        "hardlink"
    };
    log(&format!("python: running `{cmdline}`"));
    run_shell(ws, cmdline, &[("UV_LINK_MODE", link_mode)])?;
    Ok(Some(report(
        "python",
        bin,
        "installed",
        format!("UV_LINK_MODE={link_mode}"),
        started,
    )))
}

fn report(
    ecosystem: &'static str,
    tool: &str,
    action: &'static str,
    detail: String,
    started: Instant,
) -> DepReport {
    DepReport {
        ecosystem,
        tool: tool.into(),
        action,
        detail,
        millis: started.elapsed().as_millis(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_managers_and_fingerprints() {
        let d = std::env::temp_dir().join(format!("ct-deps-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        assert_eq!(detect_node(&d), None);
        assert_eq!(node_fingerprint(&d), None);
        std::fs::write(d.join("yarn.lock"), "a").unwrap();
        assert_eq!(detect_node(&d), Some(NodePm::YarnClassic));
        std::fs::write(d.join(".yarnrc.yml"), "").unwrap();
        assert_eq!(detect_node(&d), Some(NodePm::YarnBerry));
        let before = node_fingerprint(&d);
        std::fs::write(d.join("yarn.lock"), "b").unwrap();
        assert_ne!(before, node_fingerprint(&d));
        std::fs::write(d.join("uv.lock"), "").unwrap();
        assert_eq!(detect_python(&d), Some(PythonTool::Uv));
        std::fs::remove_dir_all(&d).unwrap();
    }
}
