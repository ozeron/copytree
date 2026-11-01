//! Per-workspace metadata, stored in `<git-common-dir>/copytree/workspaces/`.

use crate::clone::Backend;
use crate::deps::Fingerprints;
use crate::git::Repo;
use crate::seed::Carried;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Preparing,
    Ready,
    Failed,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Origin {
    /// Created by `copytree new`.
    New,
    /// A worktree made by another tool and prepared by the post-checkout hook.
    Hook,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Workspace {
    pub name: String,
    pub path: PathBuf,
    pub branch: Option<String>,
    pub base: String,
    pub created_at: u64,
    pub status: Status,
    pub origin: Origin,
    pub backend: Option<Backend>,
    /// Main's fingerprints when the workspace was prepared.
    pub main_fingerprints: Fingerprints,
    pub carried: Vec<Carried>,
    #[serde(default)]
    pub error: Option<String>,
}

pub fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn dir(repo: &Repo) -> PathBuf {
    repo.state_dir().join("workspaces")
}

fn file(repo: &Repo, name: &str) -> PathBuf {
    dir(repo).join(format!("{name}.json"))
}

pub fn save(repo: &Repo, ws: &Workspace) -> Result<()> {
    let d = dir(repo);
    std::fs::create_dir_all(&d).with_context(|| format!("creating {}", d.display()))?;
    let tmp = d.join(format!(".{}.json.tmp", ws.name));
    std::fs::write(&tmp, serde_json::to_vec_pretty(ws)?)?;
    std::fs::rename(&tmp, file(repo, &ws.name))?;
    Ok(())
}

pub fn load(repo: &Repo, name: &str) -> Option<Workspace> {
    let bytes = std::fs::read(file(repo, name)).ok()?;
    serde_json::from_slice(&bytes).ok()
}

pub fn remove(repo: &Repo, name: &str) {
    let _ = std::fs::remove_file(file(repo, name));
}

pub fn all(repo: &Repo) -> Vec<Workspace> {
    let Ok(rd) = std::fs::read_dir(dir(repo)) else {
        return Vec::new();
    };
    let mut v: Vec<Workspace> = rd
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
        .filter_map(|e| serde_json::from_slice(&std::fs::read(e.path()).ok()?).ok())
        .collect();
    v.sort_by(|a, b| a.created_at.cmp(&b.created_at).then(a.name.cmp(&b.name)));
    v
}
