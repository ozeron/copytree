//! Copy-on-write cloning with a plain-copy fallback.
//!
//! macOS APFS clones whole directories with `clonefile(2)`. Linux btrfs and
//! XFS, and Windows ReFS, clone file by file through `reflink-copy`. Other
//! volumes get plain copies, and callers decide whether a copy is worth it.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Backend {
    Reflink,
    Copy,
}

impl Backend {
    pub fn is_cow(self) -> bool {
        self == Backend::Reflink
    }
}

/// Clones one byte from a file in `from_dir` into `to_dir`. Both must be on the
/// volumes that real clones will use, since reflinks never cross volumes.
pub fn probe(from_dir: &Path, to_dir: &Path) -> Backend {
    // One name per side: with from_dir == to_dir a shared name would make the
    // clone's source its own existing target, so every probe would fail.
    let name = format!(".copytree-probe-{}", std::process::id());
    let (src, dst) = (
        from_dir.join(format!("{name}-src")),
        to_dir.join(format!("{name}-dst")),
    );
    let ok = fs::create_dir_all(from_dir).is_ok()
        && fs::create_dir_all(to_dir).is_ok()
        && fs::write(&src, b"x").is_ok()
        && reflink_copy::reflink(&src, &dst).is_ok();
    let _ = fs::remove_file(&src);
    let _ = fs::remove_file(&dst);
    if ok { Backend::Reflink } else { Backend::Copy }
}

/// Total size of the regular files under `path`, not following symlinks.
pub fn tree_size(path: &Path) -> u64 {
    let mut total = 0;
    let mut stack = vec![path.to_path_buf()];
    while let Some(p) = stack.pop() {
        let Ok(meta) = fs::symlink_metadata(&p) else {
            continue;
        };
        if meta.is_dir() {
            stack.extend(
                fs::read_dir(&p)
                    .into_iter()
                    .flatten()
                    .flatten()
                    .map(|e| e.path()),
            );
        } else if meta.is_file() {
            total += meta.len();
        }
    }
    total
}

/// Clones `src` (a file or directory) to `dst`, which must not exist. Modes and
/// modification times are kept so incremental builds still trust the files.
pub fn clone_path(src: &Path, dst: &Path, backend: Backend) -> Result<()> {
    if fs::symlink_metadata(dst).is_ok() {
        bail!("{} already exists", dst.display());
    }
    if let Some(parent) = dst.parent() {
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    #[cfg(target_os = "macos")]
    if backend.is_cow() && macos::clonefile(src, dst).is_ok() {
        return Ok(());
    }
    let skeleton = copy_skeleton(src, dst)?;
    clone_files_in_parallel(&skeleton.files, backend)?;
    // Directory modes go on after their contents exist: a read-only directory
    // would refuse the files written into it.
    for (dir, mode) in skeleton.dirs {
        fs::set_permissions(&dir, mode)?;
    }
    Ok(())
}

/// The half-built copy `copy_skeleton` leaves behind: the regular files still
/// to be cloned, and the directories whose modes wait until their contents
/// exist, because a read-only directory would refuse them.
struct Skeleton {
    files: Vec<(PathBuf, PathBuf)>,
    dirs: Vec<(PathBuf, fs::Permissions)>,
}

/// Recreates directories and symlinks under `dst` and returns the regular
/// files still to be cloned.
fn copy_skeleton(src: &Path, dst: &Path) -> Result<Skeleton> {
    let mut skeleton = Skeleton {
        files: Vec::new(),
        dirs: Vec::new(),
    };
    let mut stack = vec![(src.to_path_buf(), dst.to_path_buf())];
    while let Some((s, d)) = stack.pop() {
        let meta = fs::symlink_metadata(&s).with_context(|| format!("reading {}", s.display()))?;
        let kind = meta.file_type();
        if kind.is_symlink() {
            copy_symlink(&s, &d)?;
        } else if kind.is_file() {
            skeleton.files.push((s, d));
        } else if kind.is_dir() {
            fs::create_dir(&d).with_context(|| format!("creating {}", d.display()))?;
            skeleton.dirs.push((d.clone(), meta.permissions()));
            for entry in fs::read_dir(&s)? {
                let name = entry?.file_name();
                stack.push((s.join(&name), d.join(&name)));
            }
        }
    }
    Ok(skeleton)
}

fn clone_files_in_parallel(files: &[(PathBuf, PathBuf)], backend: Backend) -> Result<()> {
    let workers = std::thread::available_parallelism()
        .map_or(4, |n| n.get())
        .clamp(1, 8);
    let chunk = files.len().div_ceil(workers).max(1);
    let first_error: Mutex<Option<anyhow::Error>> = Mutex::new(None);
    std::thread::scope(|scope| {
        for part in files.chunks(chunk) {
            let first_error = &first_error;
            scope.spawn(move || {
                for (src, dst) in part {
                    if first_error.lock().unwrap().is_some() {
                        return;
                    }
                    if let Err(e) = clone_file(src, dst, backend) {
                        first_error.lock().unwrap().get_or_insert(e);
                        return;
                    }
                }
            });
        }
    });
    match first_error.into_inner().unwrap() {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

fn clone_file(src: &Path, dst: &Path, backend: Backend) -> Result<()> {
    let meta = fs::metadata(src).with_context(|| format!("reading {}", src.display()))?;
    match backend {
        Backend::Reflink => {
            reflink_copy::reflink(src, dst).with_context(|| format!("cloning {}", src.display()))?
        }
        Backend::Copy => {
            fs::copy(src, dst).with_context(|| format!("copying {}", src.display()))?;
        }
    }
    let times = fs::FileTimes::new()
        .set_modified(meta.modified()?)
        .set_accessed(meta.accessed()?);
    // The copy carries the source's mode, so a read-only destination cannot
    // be opened for writing. Make it writable just long enough to set the
    // times, then put the mode back.
    let mode = meta.permissions();
    if mode.readonly() {
        let mut writable = mode.clone();
        #[allow(clippy::permissions_set_readonly_false)]
        writable.set_readonly(false);
        fs::set_permissions(dst, writable)?;
    }
    fs::OpenOptions::new()
        .write(true)
        .open(dst)
        .with_context(|| format!("setting times on {}", dst.display()))?
        .set_times(times)?;
    fs::set_permissions(dst, mode)?;
    Ok(())
}

fn copy_symlink(src: &Path, dst: &Path) -> Result<()> {
    let target = fs::read_link(src)?;
    #[cfg(unix)]
    std::os::unix::fs::symlink(&target, dst)?;
    #[cfg(windows)]
    if fs::metadata(src).is_ok_and(|m| m.is_dir()) {
        std::os::windows::fs::symlink_dir(&target, dst)?;
    } else {
        std::os::windows::fs::symlink_file(&target, dst)?;
    }
    Ok(())
}

#[cfg(target_os = "macos")]
mod macos {
    use std::ffi::CString;
    use std::io;
    use std::os::unix::ffi::OsStrExt;
    use std::path::Path;

    const CLONE_NOFOLLOW: u32 = 0x0001;

    pub fn clonefile(src: &Path, dst: &Path) -> io::Result<()> {
        let src = CString::new(src.as_os_str().as_bytes())?;
        let dst = CString::new(dst.as_os_str().as_bytes())?;
        match unsafe { libc::clonefile(src.as_ptr(), dst.as_ptr(), CLONE_NOFOLLOW) } {
            0 => Ok(()),
            _ => Err(io::Error::last_os_error()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, SystemTime};

    #[test]
    fn copies_tree_keeping_mtime_mode_and_symlinks() {
        let tmp = std::env::temp_dir().join(format!("ct-clone-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        let src = tmp.join("src");
        fs::create_dir_all(src.join("a/b")).unwrap();
        let file = src.join("a/b/f.txt");
        fs::write(&file, b"hello").unwrap();
        let old = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000_000);
        fs::OpenOptions::new()
            .write(true)
            .open(&file)
            .unwrap()
            .set_modified(old)
            .unwrap();
        let mut readonly = fs::metadata(&file).unwrap().permissions();
        readonly.set_readonly(true);
        fs::set_permissions(&file, readonly).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink("b/f.txt", src.join("a/link")).unwrap();
        // A read-only directory with a file inside it.
        fs::create_dir(src.join("a/ro")).unwrap();
        fs::write(src.join("a/ro/g.txt"), b"g").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(src.join("a/ro"), fs::Permissions::from_mode(0o555)).unwrap();
        }

        let check = |dst: &Path| {
            let copied = dst.join("a/b/f.txt");
            assert_eq!(fs::read(&copied).unwrap(), b"hello");
            assert_eq!(fs::metadata(&copied).unwrap().modified().unwrap(), old);
            assert!(fs::metadata(&copied).unwrap().permissions().readonly());
            assert_eq!(fs::read(dst.join("a/ro/g.txt")).unwrap(), b"g");
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mode = fs::metadata(dst.join("a/ro")).unwrap().permissions().mode();
                assert_eq!(mode & 0o777, 0o555);
            }
            #[cfg(unix)]
            assert_eq!(
                fs::read_link(dst.join("a/link")).unwrap(),
                PathBuf::from("b/f.txt")
            );
        };

        // The probed backend: copy-on-write where the volume has it, plain
        // copies where it does not.
        let backend = probe(&tmp, &tmp);
        let dst = tmp.join("dst");
        clone_path(&src, &dst, backend).unwrap();
        check(&dst);
        assert!(clone_path(&src, &dst, Backend::Copy).is_err());

        // The plain-copy fallback must also work, on every volume.
        let dst2 = tmp.join("dst2");
        clone_path(&src, &dst2, Backend::Copy).unwrap();
        check(&dst2);
        assert_eq!(tree_size(&src), 6);

        // Cleanup: clear what the copies preserved, so removal works everywhere.
        for root in [&src, &dst, &dst2] {
            let copied = root.join("a/b/f.txt");
            if fs::metadata(&copied).is_ok_and(|m| m.permissions().readonly()) {
                let mut writable = fs::metadata(&copied).unwrap().permissions();
                #[allow(clippy::permissions_set_readonly_false)]
                writable.set_readonly(false);
                fs::set_permissions(&copied, writable).unwrap();
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let dir = root.join("a/ro");
                if dir.is_dir() {
                    let _ = fs::set_permissions(&dir, fs::Permissions::from_mode(0o755));
                }
            }
        }
        fs::remove_dir_all(&tmp).unwrap();
    }
}
