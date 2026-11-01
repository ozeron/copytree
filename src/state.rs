//! Workspace records, one JSON file each in `<git-common-dir>/copytree/workspaces/`.

use crate::git::Repo;
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
    /// Made by `copytree new`.
    New,
    /// Made by another tool and prepared by the post-checkout hook.
    Hook,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Workspace {
    pub name: String,
    pub path: PathBuf,
    pub created_at: u64,
    pub status: Status,
    pub origin: Origin,
    #[serde(default)]
    pub carried: Vec<String>,
    #[serde(default)]
    pub error: Option<String>,
}

impl Workspace {
    pub fn new(name: String, path: PathBuf, origin: Origin) -> Workspace {
        Workspace {
            name,
            path,
            created_at: now(),
            status: Status::Preparing,
            origin,
            carried: Vec::new(),
            error: None,
        }
    }
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn dir(repo: &Repo) -> PathBuf {
    repo.state_dir().join("workspaces")
}

fn file(repo: &Repo, name: &str) -> PathBuf {
    dir(repo).join(format!("{name}.json"))
}

pub fn save(repo: &Repo, ws: &Workspace) -> Result<()> {
    let dir = dir(repo);
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let tmp = dir.join(format!(".{}.tmp", ws.name));
    std::fs::write(&tmp, serde_json::to_vec_pretty(ws)?)?;
    std::fs::rename(&tmp, file(repo, &ws.name))?;
    Ok(())
}

pub fn load(repo: &Repo, name: &str) -> Option<Workspace> {
    serde_json::from_slice(&std::fs::read(file(repo, name)).ok()?).ok()
}

pub fn remove(repo: &Repo, name: &str) {
    let _ = std::fs::remove_file(file(repo, name));
}

pub fn all(repo: &Repo) -> Vec<Workspace> {
    let Ok(entries) = std::fs::read_dir(dir(repo)) else {
        return Vec::new();
    };
    let mut list: Vec<Workspace> = entries
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
        .filter_map(|e| serde_json::from_slice(&std::fs::read(e.path()).ok()?).ok())
        .collect();
    list.sort_by(|a, b| (a.created_at, &a.name).cmp(&(b.created_at, &b.name)));
    list
}
