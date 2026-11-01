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
    /// `cloned`, `copied`, `skipped`
    pub action: &'static str,
    pub detail: String,
    pub bytes: u64,
    pub millis: u128,
}

/// Minimal glob: `*` matches any run of characters, `?` one character.
pub fn glob_match(pat: &str, s: &str) -> bool {
    let (p, t): (Vec<char>, Vec<char>) = (pat.chars().collect(), s.chars().collect());
    let (mut pi, mut ti, mut star, mut mark) = (0, 0, None, 0);
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = ti;
            pi += 1;
        } else if let Some(sp) = star {
            pi = sp + 1;
            mark += 1;
            ti = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

fn basename(p: &str) -> &str {
    p.rsplit('/').next().unwrap_or(p)
}

/// Resolve seed patterns to concrete paths (relative to main) that exist in
/// main and are gitignored. Nested duplicates are dropped, and nothing inside
/// a virtualenv is ever selected.
pub fn resolve_targets(main: &Path, patterns: &[String], ignored: &[String]) -> Vec<String> {
    let roots: Vec<(&str, bool)> = ignored
        .iter()
        .map(|r| (r.trim_end_matches('/'), r.ends_with('/')))
        .collect();
    let mut out: Vec<String> = Vec::new();
    for pat in patterns {
        let pat = pat.trim_matches('/');
        if pat.is_empty() {
            continue;
        }
        if pat.contains('*') || pat.contains('?') {
            // Name glob, matched against ignored entries anywhere.
            for (r, _) in &roots {
                if glob_match(pat, basename(r)) || glob_match(pat, r) {
                    out.push(r.to_string());
                }
            }
            continue;
        }
        let (first, rest) = match pat.split_once('/') {
            Some((f, r)) => (f, Some(r)),
            None => (pat, None),
        };
        for (r, is_dir) in &roots {
            let cand = if *r == pat || r.ends_with(&format!("/{pat}")) {
                Some(r.to_string())
            } else if *is_dir && basename(r) == first {
                rest.map(|rest| format!("{r}/{rest}"))
            } else {
                None
            };
            if let Some(c) = cand
                && main.join(&c).exists()
            {
                out.push(c);
            }
        }
    }
    out.retain(|p| {
        !p.split('/')
            .any(|c| c == ".venv" || c == "venv" || c == ".git")
    });
    out.sort();
    out.dedup();
    // Drop paths nested inside another selected path.
    let mut kept: Vec<String> = Vec::new();
    for p in out {
        if !kept.iter().any(|k| p.starts_with(&format!("{k}/"))) {
            kept.push(p);
        }
    }
    kept
}

/// Clone each target from main into the workspace. On a volume without
/// copy-on-write, targets larger than `copy_limit` bytes are skipped.
pub fn seed(
    main: &Path,
    ws: &Path,
    targets: &[String],
    backend: Backend,
    copy_limit: u64,
    log: &mut dyn FnMut(&str),
) -> Vec<SeedReport> {
    let mut reports = Vec::new();
    for rel in targets {
        let t = Instant::now();
        let (src, dst) = (main.join(rel), ws.join(rel));
        let rep = |action, detail: String, bytes| SeedReport {
            path: rel.clone(),
            action,
            detail,
            bytes,
            millis: t.elapsed().as_millis(),
        };
        if std::fs::symlink_metadata(&dst).is_ok() {
            reports.push(rep("skipped", "already present in workspace".into(), 0));
            continue;
        }
        if !backend.is_cow() {
            let size = clone::tree_size(&src);
            if size > copy_limit {
                reports.push(rep(
                    "skipped",
                    format!("no copy-on-write and {} MiB > copy limit", size >> 20),
                    0,
                ));
                continue;
            }
        }
        match clone::clone_path(&src, &dst, backend) {
            Ok(s) => {
                let action = if backend.is_cow() { "cloned" } else { "copied" };
                log(&format!("seed: {action} {rel} ({} files)", s.files));
                reports.push(rep(action, String::new(), s.bytes));
            }
            Err(e) => {
                // A cache is an optimisation: never fail the workspace over it.
                let _ = std::fs::remove_dir_all(&dst);
                log(&format!("seed: skipped {rel}: {e:#}"));
                reports.push(rep("skipped", format!("{e:#}"), 0));
            }
        }
    }
    reports
}

#[derive(Debug, Clone, Serialize, serde::Deserialize)]
pub struct Carried {
    pub path: String,
}

/// Copy small untracked config files (`.env`, `.worktreeinclude` entries)
/// from main. A file that is tracked, or not gitignored, is refused unless
/// `force`, because carrying it would put it one `git add -A` from a commit.
pub fn carry(
    main: &Path,
    ws: &Path,
    patterns: &[String],
    force: bool,
    log: &mut dyn FnMut(&str),
) -> Result<Vec<Carried>> {
    let mut paths: Vec<String> = Vec::new();
    for pat in patterns {
        let pat = pat.trim_matches('/');
        if pat.contains('*') || pat.contains('?') {
            let (dir, name) = match pat.rsplit_once('/') {
                Some((d, n)) => (d, n),
                None => ("", pat),
            };
            if let Ok(rd) = std::fs::read_dir(main.join(dir)) {
                for e in rd.flatten() {
                    let n = e.file_name().to_string_lossy().into_owned();
                    if glob_match(name, &n) {
                        paths.push(if dir.is_empty() {
                            n
                        } else {
                            format!("{dir}/{n}")
                        });
                    }
                }
            }
        } else if main.join(pat).exists() {
            paths.push(pat.to_string());
        }
    }
    paths.sort();
    paths.dedup();

    let mut carried = Vec::new();
    for rel in paths {
        let dst = ws.join(&rel);
        if dst.exists() {
            continue;
        }
        if !force && (git::is_tracked(main, &rel) || !git::is_ignored(main, &rel)) {
            log(&format!(
                "carry: refusing {rel}: it is not gitignored (use --force-carry)"
            ));
            continue;
        }
        clone::clone_path(&main.join(&rel), &dst, Backend::Copy)?;
        log(&format!("carry: {rel}"));
        carried.push(Carried { path: rel });
    }
    Ok(carried)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn globbing() {
        assert!(glob_match("*.tsbuildinfo", "tsconfig.tsbuildinfo"));
        assert!(glob_match(".env*", ".env.local"));
        assert!(!glob_match(".env*", "x.env"));
        assert!(glob_match("a?c", "abc"));
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
        let ignored: Vec<String> = [
            "apps/web/.next/",
            ".turbo/",
            "target/",
            ".venv/",
            "tsconfig.tsbuildinfo",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let pats: Vec<String> = [
            ".next/cache",
            ".turbo",
            "target",
            "target/debug",
            ".mypy_cache",
            "*.tsbuildinfo",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let got = resolve_targets(&d, &pats, &ignored);
        assert_eq!(
            got,
            vec![
                ".turbo",
                "apps/web/.next/cache",
                "target",
                "tsconfig.tsbuildinfo"
            ]
        );
        std::fs::remove_dir_all(&d).unwrap();
    }
}
