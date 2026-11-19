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

/// The record file for `name`, or None when the name could leave the
/// workspaces directory (`..`, separators, empty).
fn file(repo: &Repo, name: &str) -> Option<PathBuf> {
    let safe = !name.is_empty() && name != "." && name != ".." && !name.contains(['/', '\\', '\0']);
    safe.then(|| dir(repo).join(format!("{name}.json")))
}

pub fn save(repo: &Repo, ws: &Workspace) -> Result<()> {
    let target =
        file(repo, &ws.name).with_context(|| format!("invalid workspace name {:?}", ws.name))?;
    let dir = dir(repo);
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let tmp = dir.join(format!(".{}.tmp", ws.name));
    std::fs::write(&tmp, serde_json::to_vec_pretty(ws)?)?;
    std::fs::rename(&tmp, target)?;
    Ok(())
}

pub fn load(repo: &Repo, name: &str) -> Option<Workspace> {
    let path = file(repo, name)?;
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

pub fn remove(repo: &Repo, name: &str) {
    if let Some(path) = file(repo, name) {
        let _ = std::fs::remove_file(path);
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refuses_names_that_escape_the_state_dir() {
        let dir = std::env::temp_dir().join(format!("ct-state-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let repo = Repo {
            main: dir.clone(),
            common_dir: dir.clone(),
        };
        for name in ["", ".", "..", "../x", "a/b", "a\\b"] {
            assert!(load(&repo, name).is_none(), "load accepted {name:?}");
            remove(&repo, name);
        }
        let evil = Workspace::new("../evil".into(), dir.join("ws"), Origin::New);
        assert!(save(&repo, &evil).is_err());

        let ok = Workspace::new("ok".into(), dir.join("ws"), Origin::New);
        save(&repo, &ok).unwrap();
        assert_eq!(load(&repo, "ok").map(|w| w.name).as_deref(), Some("ok"));
        remove(&repo, "ok");
        assert!(load(&repo, "ok").is_none());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
