//! Dependency fingerprints and installs for Node and Python.
//!
//! The rule from the design: rebuild dependencies from each package manager's
//! shared store, which links or clones files instead of downloading them. Clone
//! `node_modules` directly only for npm and Yarn classic, only when the
//! lockfile matches main, and only on a copy-on-write volume. Never clone
//! `.venv`: its scripts and the project's editable install point at main.

use crate::clone::{self, Backend};
use crate::config::{Config, DepsMode};
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

const NODE_FP_FILES: &[&str] = &[
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
const PY_FP_FILES: &[&str] = &[
    "uv.lock",
    "poetry.lock",
    "pixi.lock",
    "requirements.txt",
    "requirements-dev.txt",
    ".python-version",
];

pub fn detect_node(dir: &Path) -> Option<NodePm> {
    let has = |f: &str| dir.join(f).exists();
    if has("pnpm-lock.yaml") {
        Some(NodePm::Pnpm)
    } else if has("bun.lock") || has("bun.lockb") {
        Some(NodePm::Bun)
    } else if has("yarn.lock") {
        Some(if has(".yarnrc.yml") {
            NodePm::YarnBerry
        } else {
            NodePm::YarnClassic
        })
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

/// Hash of the lockfiles and toolchain pins, plus OS and architecture.
/// `None` when none of the files exist.
fn fingerprint(dir: &Path, files: &[&str]) -> Option<String> {
    let mut h = Sha256::new();
    let mut any = false;
    for f in files {
        if let Ok(bytes) = std::fs::read(dir.join(f)) {
            any = true;
            h.update(f.as_bytes());
            h.update([0]);
            h.update(&bytes);
            h.update([0]);
        }
    }
    if !any {
        return None;
    }
    h.update(std::env::consts::OS);
    h.update(std::env::consts::ARCH);
    Some(hex::encode(&h.finalize()[..12]))
}

#[derive(Debug, Clone, Default, Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct Fingerprints {
    pub node: Option<String>,
    pub python: Option<String>,
}

pub fn fingerprints(dir: &Path) -> Fingerprints {
    Fingerprints {
        node: fingerprint(dir, NODE_FP_FILES),
        python: fingerprint(dir, PY_FP_FILES),
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct DepReport {
    pub ecosystem: &'static str,
    pub tool: String,
    /// `cloned`, `installed`, `skipped`
    pub action: &'static str,
    pub detail: String,
    pub millis: u128,
}

pub fn which(bin: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|p| {
        let c = p.join(bin);
        c.is_file() || (cfg!(windows) && p.join(format!("{bin}.exe")).is_file())
    })
}

fn run_shell(ws: &Path, cmdline: &str, envs: &[(&str, &str)]) -> Result<()> {
    let mut c = if cfg!(windows) {
        let mut c = Command::new("cmd");
        c.args(["/C", cmdline]);
        c
    } else {
        let mut c = Command::new("sh");
        c.args(["-c", cmdline]);
        c
    };
    for (k, v) in envs {
        if std::env::var_os(k).is_none() {
            c.env(k, v);
        }
    }
    let status = c
        .current_dir(ws)
        .stdin(Stdio::null())
        .stdout(std::io::stderr())
        .status()
        .with_context(|| format!("running `{cmdline}`"))?;
    if !status.success() {
        bail!("`{cmdline}` failed in {} ({status})", ws.display());
    }
    Ok(())
}

/// Install or clone dependencies into the workspace `ws`.
pub fn install(
    main: &Path,
    ws: &Path,
    cfg: &Config,
    backend: Backend,
    log: &mut dyn FnMut(&str),
) -> Result<Vec<DepReport>> {
    let mut reports = Vec::new();
    if cfg.deps.mode == DepsMode::Off {
        return Ok(reports);
    }
    let main_fp = fingerprints(main);
    let ws_fp = fingerprints(ws);

    // ---- Node ----
    if let Some(cmd) = &cfg.deps.node {
        let t = Instant::now();
        log(&format!("node: running `{cmd}`"));
        run_shell(ws, cmd, &[])?;
        reports.push(DepReport {
            ecosystem: "node",
            tool: "custom".into(),
            action: "installed",
            detail: cmd.clone(),
            millis: t.elapsed().as_millis(),
        });
    } else if let Some(pm) = detect_node(ws) {
        let t = Instant::now();
        let same = ws_fp.node.is_some() && ws_fp.node == main_fp.node;
        let clonable = matches!(pm, NodePm::Npm | NodePm::YarnClassic);
        let main_nm = main.join("node_modules");
        if clonable
            && same
            && backend.is_cow()
            && main_nm.is_dir()
            && !ws.join("node_modules").exists()
        {
            // Lockfile matches main on a CoW volume: clone every node_modules dir.
            let mut stats = clone::Stats::default();
            for root in node_modules_roots(main)? {
                let s = clone::clone_path(&main.join(&root), &ws.join(&root), backend)?;
                stats.files += s.files;
                stats.bytes += s.bytes;
            }
            log(&format!(
                "node: cloned node_modules from main ({} files)",
                stats.files
            ));
            reports.push(DepReport {
                ecosystem: "node",
                tool: format!("{pm:?}").to_lowercase(),
                action: "cloned",
                detail: "lockfile matches main".into(),
                millis: t.elapsed().as_millis(),
            });
        } else {
            let (bin, cmd) = match pm {
                NodePm::Pnpm => ("pnpm", "pnpm install --frozen-lockfile --prefer-offline"),
                NodePm::Bun => ("bun", "bun install --frozen-lockfile"),
                NodePm::Npm => ("npm", "npm ci --prefer-offline --no-audit --no-fund"),
                NodePm::YarnClassic => ("yarn", "yarn install --frozen-lockfile --prefer-offline"),
                NodePm::YarnBerry => ("yarn", "yarn install --immutable"),
            };
            if which(bin) {
                log(&format!("node: running `{cmd}`"));
                run_shell(ws, cmd, &[])?;
                reports.push(DepReport {
                    ecosystem: "node",
                    tool: bin.into(),
                    action: "installed",
                    detail: if same {
                        "lockfile matches main".into()
                    } else {
                        "lockfile differs from main".into()
                    },
                    millis: t.elapsed().as_millis(),
                });
            } else {
                log(&format!("node: `{bin}` not on PATH, skipping install"));
                reports.push(skipped("node", bin, format!("{bin} not on PATH")));
            }
        }
    }

    // ---- Python ----
    if let Some(cmd) = &cfg.deps.python {
        let t = Instant::now();
        log(&format!("python: running `{cmd}`"));
        run_shell(ws, cmd, &[])?;
        reports.push(DepReport {
            ecosystem: "python",
            tool: "custom".into(),
            action: "installed",
            detail: cmd.clone(),
            millis: t.elapsed().as_millis(),
        });
    } else if let Some(tool) = detect_python(ws) {
        let t = Instant::now();
        // uv links from its cache: clone on CoW volumes, hardlink elsewhere.
        let link_mode = if backend.is_cow() {
            "clone"
        } else {
            "hardlink"
        };
        let envs = [("UV_LINK_MODE", link_mode)];
        let (bin, cmd) = match tool {
            PythonTool::Uv => ("uv", "uv sync --frozen"),
            PythonTool::UvPip => (
                "uv",
                "uv venv --allow-existing && uv pip sync requirements.txt",
            ),
            PythonTool::Poetry => ("poetry", "poetry install --no-interaction"),
            PythonTool::Pixi => ("pixi", "pixi install --frozen"),
        };
        if which(bin) {
            log(&format!("python: running `{cmd}`"));
            run_shell(ws, cmd, &envs)?;
            reports.push(DepReport {
                ecosystem: "python",
                tool: bin.into(),
                action: "installed",
                detail: format!("UV_LINK_MODE={link_mode}"),
                millis: t.elapsed().as_millis(),
            });
        } else {
            log(&format!("python: `{bin}` not on PATH, skipping install"));
            reports.push(skipped("python", bin, format!("{bin} not on PATH")));
        }
    }
    Ok(reports)
}

fn skipped(eco: &'static str, tool: &str, detail: String) -> DepReport {
    DepReport {
        ecosystem: eco,
        tool: tool.into(),
        action: "skipped",
        detail,
        millis: 0,
    }
}

/// Every ignored `node_modules` directory in main (root and workspace packages).
fn node_modules_roots(main: &Path) -> Result<Vec<String>> {
    Ok(crate::git::ignored_roots(main)?
        .into_iter()
        .filter_map(|r| {
            let r = r.trim_end_matches('/').to_string();
            (r == "node_modules" || r.ends_with("/node_modules")).then_some(r)
        })
        .collect())
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
        assert_eq!(fingerprints(&d), Fingerprints::default());
        std::fs::write(d.join("yarn.lock"), "a").unwrap();
        assert_eq!(detect_node(&d), Some(NodePm::YarnClassic));
        std::fs::write(d.join(".yarnrc.yml"), "").unwrap();
        assert_eq!(detect_node(&d), Some(NodePm::YarnBerry));
        let fp1 = fingerprints(&d).node;
        std::fs::write(d.join("yarn.lock"), "b").unwrap();
        assert_ne!(fp1, fingerprints(&d).node);
        std::fs::write(d.join("uv.lock"), "").unwrap();
        assert_eq!(detect_python(&d), Some(PythonTool::Uv));
        std::fs::remove_dir_all(&d).unwrap();
    }
}
