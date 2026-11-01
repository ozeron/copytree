//! Copy-on-write cloning with a plain-copy fallback.
//!
//! - macOS APFS: one `clonefile(2)` per directory (recursive, in-kernel).
//! - Linux btrfs / XFS (reflink=1), Windows ReFS Dev Drive: per-file reflink
//!   through the `reflink-copy` crate, on a small thread pool.
//! - Anything else: byte copy, but only below a size cap (callers decide).

use anyhow::{Context, Result, bail};
use serde::Serialize;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Backend {
    /// The volume supports copy-on-write clones.
    Reflink,
    /// No copy-on-write: clones become real copies.
    Copy,
}

impl Backend {
    pub fn is_cow(self) -> bool {
        self == Backend::Reflink
    }
}

/// Detect whether `dir`'s volume can clone files, with a 1-byte probe.
pub fn probe(dir: &Path) -> Backend {
    let _ = fs::create_dir_all(dir);
    let pid = std::process::id();
    let a = dir.join(format!(".copytree-probe-{pid}-a"));
    let b = dir.join(format!(".copytree-probe-{pid}-b"));
    let ok = fs::write(&a, b"x").is_ok() && reflink_copy::reflink(&a, &b).is_ok();
    let _ = fs::remove_file(&a);
    let _ = fs::remove_file(&b);
    if ok { Backend::Reflink } else { Backend::Copy }
}

#[derive(Debug, Default, Clone, Copy, Serialize)]
pub struct Stats {
    pub files: u64,
    pub bytes: u64,
}

/// Apparent size of a file or directory tree (symlinks not followed).
pub fn tree_size(path: &Path) -> u64 {
    let Ok(meta) = fs::symlink_metadata(path) else {
        return 0;
    };
    if !meta.is_dir() {
        return meta.len();
    }
    let mut total = 0;
    let mut stack = vec![path.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            let Ok(m) = e.metadata() else { continue };
            if m.is_dir() {
                stack.push(e.path());
            } else {
                total += m.len();
            }
        }
    }
    total
}

/// Clone `src` (file or directory) to `dst`, which must not exist yet.
/// With `Backend::Copy`, data is copied; callers should check the size first.
/// File modes and modification times are preserved so build tools that trust
/// mtimes (cargo, tsc incremental, mypy) keep their caches valid.
pub fn clone_path(src: &Path, dst: &Path, backend: Backend) -> Result<Stats> {
    if fs::symlink_metadata(dst).is_ok() {
        bail!("destination already exists: {}", dst.display());
    }
    if let Some(parent) = dst.parent() {
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    let meta = fs::symlink_metadata(src).with_context(|| format!("reading {}", src.display()))?;

    #[cfg(target_os = "macos")]
    if backend.is_cow() && (meta.is_dir() || meta.is_file()) {
        // One in-kernel call clones the whole tree, metadata included.
        if macos::clonefile(src, dst).is_ok() {
            return Ok(Stats { files: 1, bytes: 0 });
        }
    }

    if meta.file_type().is_symlink() {
        copy_symlink(src, dst)?;
        return Ok(Stats::default());
    }
    if meta.is_file() {
        let bytes = clone_file(src, dst, &meta, backend)?;
        return Ok(Stats { files: 1, bytes });
    }

    // Directory: create the skeleton serially, then clone files in parallel.
    let mut files: Vec<(PathBuf, PathBuf, fs::Metadata)> = Vec::new();
    let mut stack = vec![(src.to_path_buf(), dst.to_path_buf())];
    while let Some((s, d)) = stack.pop() {
        fs::create_dir(&d).with_context(|| format!("creating {}", d.display()))?;
        if let Ok(m) = fs::metadata(&s) {
            let _ = fs::set_permissions(&d, m.permissions());
        }
        for entry in fs::read_dir(&s).with_context(|| format!("reading {}", s.display()))? {
            let entry = entry?;
            let ft = entry.file_type()?;
            let (sp, dp) = (entry.path(), d.join(entry.file_name()));
            if ft.is_dir() {
                stack.push((sp, dp));
            } else if ft.is_symlink() {
                copy_symlink(&sp, &dp)?;
            } else if ft.is_file() {
                let m = entry.metadata()?;
                files.push((sp, dp, m));
            }
            // Sockets, fifos and devices are skipped.
        }
    }

    let workers = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .clamp(1, 8);
    let next = AtomicUsize::new(0);
    let bytes = AtomicU64::new(0);
    let first_err: Mutex<Option<anyhow::Error>> = Mutex::new(None);
    std::thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| {
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    let Some((s, d, m)) = files.get(i) else { break };
                    match clone_file(s, d, m, backend) {
                        Ok(n) => {
                            bytes.fetch_add(n, Ordering::Relaxed);
                        }
                        Err(e) => {
                            first_err.lock().unwrap().get_or_insert(e);
                            next.store(usize::MAX / 2, Ordering::Relaxed);
                            break;
                        }
                    }
                }
            });
        }
    });
    if let Some(e) = first_err.into_inner().unwrap() {
        return Err(e);
    }
    Ok(Stats {
        files: files.len() as u64,
        bytes: bytes.into_inner(),
    })
}

fn clone_file(src: &Path, dst: &Path, meta: &fs::Metadata, backend: Backend) -> Result<u64> {
    match backend {
        Backend::Reflink => reflink_copy::reflink_or_copy(src, dst)
            .map(|_| ())
            .with_context(|| format!("cloning {}", src.display()))?,
        Backend::Copy => {
            fs::copy(src, dst).with_context(|| format!("copying {}", src.display()))?;
        }
    }
    let _ = fs::set_permissions(dst, meta.permissions());
    if let Ok(mtime) = meta.modified() {
        let mut times = fs::FileTimes::new().set_modified(mtime);
        if let Ok(atime) = meta.accessed() {
            times = times.set_accessed(atime);
        }
        if let Ok(f) = fs::OpenOptions::new().write(true).open(dst) {
            let _ = f.set_times(times);
        }
    }
    Ok(meta.len())
}

fn copy_symlink(src: &Path, dst: &Path) -> io::Result<()> {
    let target = fs::read_link(src)?;
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(target, dst)
    }
    #[cfg(windows)]
    {
        if fs::metadata(src).map(|m| m.is_dir()).unwrap_or(false) {
            std::os::windows::fs::symlink_dir(target, dst)
        } else {
            std::os::windows::fs::symlink_file(target, dst)
        }
    }
}

#[cfg(target_os = "macos")]
mod macos {
    use std::ffi::CString;
    use std::io;
    use std::os::unix::ffi::OsStrExt;
    use std::path::Path;

    pub fn clonefile(src: &Path, dst: &Path) -> io::Result<()> {
        let s = CString::new(src.as_os_str().as_bytes())?;
        let d = CString::new(dst.as_os_str().as_bytes())?;
        // CLONE_NOFOLLOW = 0x0001: clone a symlink itself, not its target.
        let rc = unsafe { libc::clonefile(s.as_ptr(), d.as_ptr(), 0x0001) };
        if rc == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copies_tree_with_mtime_and_symlink() {
        let tmp = std::env::temp_dir().join(format!("ct-clone-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        let src = tmp.join("src");
        fs::create_dir_all(src.join("a/b")).unwrap();
        fs::write(src.join("a/b/f.txt"), b"hello").unwrap();
        let old = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000_000);
        fs::OpenOptions::new()
            .write(true)
            .open(src.join("a/b/f.txt"))
            .unwrap()
            .set_modified(old)
            .unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink("b/f.txt", src.join("a/link")).unwrap();

        let dst = tmp.join("dst");
        let backend = probe(&tmp);
        clone_path(&src, &dst, backend).unwrap();
        assert_eq!(fs::read(dst.join("a/b/f.txt")).unwrap(), b"hello");
        assert_eq!(
            fs::metadata(dst.join("a/b/f.txt"))
                .unwrap()
                .modified()
                .unwrap(),
            old
        );
        #[cfg(unix)]
        assert_eq!(
            fs::read_link(dst.join("a/link")).unwrap(),
            PathBuf::from("b/f.txt")
        );
        assert!(
            clone_path(&src, &dst, backend).is_err(),
            "refuses to overwrite"
        );
        fs::remove_dir_all(&tmp).unwrap();
    }
}
