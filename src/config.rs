//! `.copytree.toml` at the root of the main worktree, plus `.worktreeinclude`.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

const CONFIG_FILE: &str = ".copytree.toml";
const WORKTREEINCLUDE: &str = ".worktreeinclude";

/// Ignored build caches seeded with copy-on-write. A path also matches under
/// nested packages (`.next/cache` matches `apps/web/.next/cache`), and a
/// pattern with `*` matches file names.
const DEFAULT_SEED: &[&str] = &[
    ".next/cache",
    ".turbo",
    ".nx/cache",
    ".parcel-cache",
    ".eslintcache",
    "node_modules/.cache",
    "node_modules/.vite",
    "*.tsbuildinfo",
    ".mypy_cache",
    ".ruff_cache",
    ".pytest_cache",
    "target",
];

const DEFAULT_CARRY: &[&str] = &[
    ".env",
    ".env.local",
    ".env.development.local",
    ".env.test.local",
];

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Where workspaces go, relative to the main worktree. Default `../.copytree/<repo>/`.
    pub pool: Option<PathBuf>,
    pub branch_prefix: String,
    pub seed: SeedConfig,
    pub carry: CarryConfig,
    pub deps: DepsConfig,
    pub hooks: HooksConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SeedConfig {
    /// Replaces the default list.
    pub paths: Vec<String>,
    /// Added to `paths`.
    pub extra: Vec<String>,
    /// Without copy-on-write, copy a cache only below this size. 0 disables plain copies.
    pub copy_limit_mib: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CarryConfig {
    pub files: Vec<String>,
    pub worktreeinclude: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DepsConfig {
    pub mode: DepsMode,
    /// Replaces the detected Node install command.
    pub node: Option<String>,
    /// Replaces the detected Python install command.
    pub python: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum DepsMode {
    #[default]
    Auto,
    Off,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct HooksConfig {
    /// Shell commands run in the new workspace after everything else.
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
            paths: strings(DEFAULT_SEED),
            extra: Vec::new(),
            copy_limit_mib: 64,
        }
    }
}

impl Default for CarryConfig {
    fn default() -> Self {
        CarryConfig {
            files: strings(DEFAULT_CARRY),
            worktreeinclude: true,
        }
    }
}

fn strings(list: &[&str]) -> Vec<String> {
    list.iter().map(|s| s.to_string()).collect()
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

    pub fn seed_patterns(&self) -> Vec<String> {
        [self.seed.paths.clone(), self.seed.extra.clone()].concat()
    }

    /// `carry.files` plus the lines of `.worktreeinclude`, skipping blanks, `#` comments and `!` negations.
    pub fn carry_patterns(&self, main: &Path) -> Vec<String> {
        let mut patterns = self.carry.files.clone();
        if !self.carry.worktreeinclude {
            return patterns;
        }
        let text = std::fs::read_to_string(main.join(WORKTREEINCLUDE)).unwrap_or_default();
        for line in text.lines().map(str::trim) {
            if line.is_empty() || line.starts_with('#') || line.starts_with('!') {
                continue;
            }
            let line = line.trim_matches('/').to_string();
            if !patterns.contains(&line) {
                patterns.push(line);
            }
        }
        patterns
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
        assert!(c.seed_patterns().contains(&"dist".to_string()));
        assert!(c.seed_patterns().contains(&".turbo".to_string()));
        assert_eq!(c.deps.mode, DepsMode::Off);
    }

    #[test]
    fn rejects_unknown_keys() {
        assert!(toml::from_str::<Config>("nope = 1").is_err());
    }
}
