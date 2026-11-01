//! Seeding ignored build caches and carrying small config files from main.

use crate::clone::{self, Backend};
use crate::git;
use anyhow::Result;
use serde::Serialize;
use std::path::Path;
use std::time::Instant;

#[derive(Debug, Clone, Serialize)]
pub struct SeedReport {
    pub path: String,
    /// `cloned`, `copied` or `skipped`.
    pub action: &'static str,
    pub detail: String,
    pub millis: u128,
}

/// `*` matches any run of characters and `?` matches one.
pub fn glob_match(pattern: &str, text: &str) -> bool {
    fn go(p: &[char], t: &[char]) -> bool {
        match (p.first(), t.first()) {
            (None, None) => true,
            (Some('*'), _) => go(&p[1..], t) || (!t.is_empty() && go(p, &t[1..])),
            (Some('?'), Some(_)) => go(&p[1..], &t[1..]),
            (Some(a), Some(b)) if a == b => go(&p[1..], &t[1..]),
            _ => false,
        }
    }
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    go(&p, &t)
}

fn is_glob(pattern: &str) -> bool {
    pattern.contains(['*', '?'])
}

fn file_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// Turns seed patterns into existing ignored paths in main, relative to main.
/// A pattern `a/b` matches an ignored `a` anywhere, extended by `/b`. Paths
/// inside a virtualenv or nested in another match are dropped.
pub fn resolve_targets(main: &Path, patterns: &[String], ignored: &[String]) -> Vec<String> {
    let mut found = Vec::new();
    for pattern in patterns
        .iter()
        .map(|p| p.trim_matches('/'))
        .filter(|p| !p.is_empty())
    {
        for entry in ignored {
            let is_dir = entry.ends_with('/');
            let entry = entry.trim_end_matches('/');
            if is_glob(pattern) {
                if glob_match(pattern, file_name(entry)) {
                    found.push(entry.to_string());
                }
                continue;
            }
            let (head, rest) = pattern.split_once('/').unwrap_or((pattern, ""));
            if entry == pattern || entry.ends_with(&format!("/{pattern}")) {
                found.push(entry.to_string());
            } else if is_dir && !rest.is_empty() && file_name(entry) == head {
                found.push(format!("{entry}/{rest}"));
            }
        }
    }
    found.retain(|p| {
        main.join(p).exists()
            && !p
                .split('/')
                .any(|part| matches!(part, ".venv" | "venv" | ".git"))
    });
    found.sort();
    found.dedup();
    let mut kept: Vec<String> = Vec::new();
    for path in found {
        if !kept.iter().any(|k| path.starts_with(&format!("{k}/"))) {
            kept.push(path);
        }
    }
    kept
}

/// Clones each target from main into the workspace. Without copy-on-write, a
/// target bigger than `copy_limit` bytes is skipped. A failed cache never
/// fails the workspace.
pub fn seed(
    main: &Path,
    ws: &Path,
    targets: &[String],
    backend: Backend,
    copy_limit: u64,
    log: &mut dyn FnMut(&str),
) -> Vec<SeedReport> {
    targets
        .iter()
        .map(|rel| {
            let started = Instant::now();
            let (action, detail) = seed_one(&main.join(rel), &ws.join(rel), backend, copy_limit);
            log(&format!(
                "seed: {action} {rel}{}",
                if detail.is_empty() {
                    String::new()
                } else {
                    format!(" ({detail})")
                }
            ));
            SeedReport {
                path: rel.clone(),
                action,
                detail,
                millis: started.elapsed().as_millis(),
            }
        })
        .collect()
}

fn seed_one(src: &Path, dst: &Path, backend: Backend, copy_limit: u64) -> (&'static str, String) {
    if dst.symlink_metadata().is_ok() {
        return ("skipped", "already in the workspace".into());
    }
    if !backend.is_cow() {
        let size = clone::tree_size(src);
        if size > copy_limit {
            return (
                "skipped",
                format!(
                    "{} MiB is over the copy limit without copy-on-write",
                    size >> 20
                ),
            );
        }
    }
    match clone::clone_path(src, dst, backend) {
        Ok(()) if backend.is_cow() => ("cloned", String::new()),
        Ok(()) => ("copied", String::new()),
        Err(e) => {
            let _ = std::fs::remove_dir_all(dst);
            let _ = std::fs::remove_file(dst);
            ("skipped", format!("{e:#}"))
        }
    }
}

/// Copies small untracked files such as `.env` from main and returns the paths
/// carried. A file that is tracked or not gitignored is refused unless `force`,
/// so a carried secret is never one `git add -A` away from a commit.
pub fn carry(
    main: &Path,
    ws: &Path,
    patterns: &[String],
    force: bool,
    log: &mut dyn FnMut(&str),
) -> Result<Vec<String>> {
    let mut paths: Vec<String> = patterns
        .iter()
        .flat_map(|p| expand(main, p.trim_matches('/')))
        .collect();
    paths.sort();
    paths.dedup();

    let mut carried = Vec::new();
    for rel in paths {
        if ws.join(&rel).exists() {
            continue;
        }
        if !force && !git::is_untracked_and_ignored(main, &rel) {
            log(&format!(
                "carry: refusing {rel} because it is not gitignored (use --force-carry)"
            ));
            continue;
        }
        clone::clone_path(&main.join(&rel), &ws.join(&rel), Backend::Copy)?;
        log(&format!("carry: {rel}"));
        carried.push(rel);
    }
    Ok(carried)
}

/// Existing paths in main matching `pattern`; a glob may only appear in the last component.
fn expand(main: &Path, pattern: &str) -> Vec<String> {
    if !is_glob(pattern) {
        return if main.join(pattern).exists() {
            vec![pattern.to_string()]
        } else {
            Vec::new()
        };
    }
    let (dir, name) = pattern.rsplit_once('/').unwrap_or(("", pattern));
    let Ok(entries) = std::fs::read_dir(main.join(dir)) else {
        return Vec::new();
    };
    entries
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| glob_match(name, n))
        .map(|n| {
            if dir.is_empty() {
                n
            } else {
                format!("{dir}/{n}")
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn globbing() {
        assert!(glob_match("*.tsbuildinfo", "tsconfig.tsbuildinfo"));
        assert!(glob_match(".env*", ".env.local"));
        assert!(!glob_match(".env*", "x.env"));
        assert!(glob_match("a?c", "abc"));
        assert!(!glob_match("a?c", "ac"));
    }

    #[test]
    fn resolves_nested_targets() {
        let d = std::env::temp_dir().join(format!("ct-seed-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        for p in [
            "apps/web/.next/cache",
            ".turbo",
            "target/debug",
            ".venv/.mypy_cache",
        ] {
            std::fs::create_dir_all(d.join(p)).unwrap();
        }
        std::fs::write(d.join("tsconfig.tsbuildinfo"), "").unwrap();
        let ignored = strings(&[
            "apps/web/.next/",
            ".turbo/",
            "target/",
            ".venv/",
            "tsconfig.tsbuildinfo",
        ]);
        let patterns = strings(&[
            ".next/cache",
            ".turbo",
            "target",
            "target/debug",
            ".mypy_cache",
            "*.tsbuildinfo",
        ]);
        let got = resolve_targets(&d, &patterns, &ignored);
        assert_eq!(
            got,
            strings(&[
                ".turbo",
                "apps/web/.next/cache",
                "target",
                "tsconfig.tsbuildinfo"
            ])
        );
        std::fs::remove_dir_all(&d).unwrap();
    }
}
