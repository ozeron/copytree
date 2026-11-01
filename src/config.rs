//! Project configuration: `.copytree.toml` at the root of the main worktree,
//! plus the shared `.worktreeinclude` convention (Claude Code, worktrunk, krowk).

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub const CONFIG_FILE: &str = ".copytree.toml";
pub const WORKTREEINCLUDE: &str = ".worktreeinclude";

/// Ignored build caches worth seeding with copy-on-write. Each entry is a path
/// that may appear at the repo root or under any ignored directory with the same
/// first component (so `.next/cache` also matches `apps/web/.next/cache`).
/// Entries starting with `*` match file names by suffix.
pub const DEFAULT_SEED: &[&str] = &[
    // Node / web
    ".next/cache",
    ".turbo",
    ".nx/cache",
    ".parcel-cache",
    ".eslintcache",
    ".vite",
    "node_modules/.cache",
    "*.tsbuildinfo",
    // Python
    ".mypy_cache",
    ".ruff_cache",
    ".pytest_cache",
    // Rust
    "target",
];

pub const DEFAULT_CARRY: &[&str] = &[
    ".env",
    ".env.local",
    ".env.development.local",
    ".env.test.local",
];

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Where workspaces are created. Relative paths resolve against the main
    /// worktree. Default: `../.copytree/<repo>/`.
    pub pool: Option<PathBuf>,
    /// Branch prefix for new workspaces.
    pub branch_prefix: String,
    pub seed: SeedConfig,
    pub carry: CarryConfig,
    pub deps: DepsConfig,
    pub hooks: HooksConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SeedConfig {
    /// Paths to seed. Replaces the default list when set.
    pub paths: Vec<String>,
    /// Extra paths added to `paths`.
    pub extra: Vec<String>,
    /// Without copy-on-write, byte-copy a seed path only if it is smaller
    /// than this many MiB. 0 disables plain copies.
    pub copy_limit_mib: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CarryConfig {
    /// Small untracked files copied byte for byte (`.env` and friends).
    pub files: Vec<String>,
    /// Also read `.worktreeinclude`.
    pub worktreeinclude: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DepsConfig {
    /// `auto` installs Node and Python dependencies, `off` skips them.
    pub mode: DepsMode,
    /// Overrides the detected Node install command (run with `sh -c`).
    pub node: Option<String>,
    /// Overrides the detected Python install command (run with `sh -c`).
    pub python: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum DepsMode {
    #[default]
    Auto,
    Off,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct HooksConfig {
    /// Shell commands run inside the new workspace after everything else,
    /// e.g. `pnpm prisma generate`.
    pub post_create: Vec<String>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            pool: None,
            branch_prefix: "ct/".into(),
            seed: SeedConfig::default(),
            carry: CarryConfig::default(),
            deps: DepsConfig::default(),
            hooks: HooksConfig::default(),
        }
    }
}

impl Default for SeedConfig {
    fn default() -> Self {
        SeedConfig {
            paths: DEFAULT_SEED.iter().map(|s| s.to_string()).collect(),
            extra: Vec::new(),
            copy_limit_mib: 64,
        }
    }
}

impl Default for CarryConfig {
    fn default() -> Self {
        CarryConfig {
            files: DEFAULT_CARRY.iter().map(|s| s.to_string()).collect(),
            worktreeinclude: true,
        }
    }
}

impl Default for DepsConfig {
    fn default() -> Self {
        DepsConfig {
            mode: DepsMode::Auto,
            node: None,
            python: None,
        }
    }
}

impl Config {
    pub fn load(main: &Path) -> Result<Config> {
        let path = main.join(CONFIG_FILE);
        if !path.exists() {
            return Ok(Config::default());
        }
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))
    }

    pub fn seed_paths(&self) -> Vec<String> {
        let mut v = self.seed.paths.clone();
        v.extend(self.seed.extra.iter().cloned());
        v
    }

    /// Carry list from config plus `.worktreeinclude` (one path or simple glob
    /// per line; `#` comments and blank lines ignored; negations unsupported).
    pub fn carry_patterns(&self, main: &Path) -> Vec<String> {
        let mut v = self.carry.files.clone();
        if self.carry.worktreeinclude
            && let Ok(text) = std::fs::read_to_string(main.join(WORKTREEINCLUDE))
        {
            for line in text.lines() {
                let l = line.trim();
                if l.is_empty() || l.starts_with('#') || l.starts_with('!') {
                    continue;
                }
                let l = l.trim_start_matches('/').trim_end_matches('/').to_string();
                if !v.contains(&l) {
                    v.push(l);
                }
            }
        }
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_partial_config() {
        let c: Config = toml::from_str(
            "branch_prefix = \"agent/\"\n[seed]\nextra = [\"dist\"]\n[deps]\nmode = \"off\"\n",
        )
        .unwrap();
        assert_eq!(c.branch_prefix, "agent/");
        assert!(c.seed_paths().contains(&"dist".to_string()));
        assert!(c.seed_paths().contains(&".turbo".to_string()));
        assert_eq!(c.deps.mode, DepsMode::Off);
    }

    #[test]
    fn rejects_unknown_keys() {
        assert!(toml::from_str::<Config>("nope = 1").is_err());
    }
}
