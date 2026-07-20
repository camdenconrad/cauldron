//! Crash-safe file replacement for buffer saves.
//!
//! `std::fs::write` opens with `O_TRUNC`: the old contents are gone the instant the call starts,
//! and anything that interrupts it — a power cut, a full disk, an OOM kill — leaves a truncated or
//! empty file where the user's work was. For a scratch artifact that is fine. For the file someone
//! has been editing for an hour it is data loss, and it is the one failure an editor must not have.
//!
//! So: write a sibling temp file, flush it to disk, then `rename` it over the target. `rename`
//! within a directory is atomic, so a reader either sees the whole old file or the whole new one.
//!
//! Two cases deliberately fall back to a plain in-place write, because replacing the directory
//! entry would be WRONG rather than merely different:
//!
//! * **Hard links.** `rename` rebinds one name to a new inode; every other name for that inode
//!   keeps the old contents. Silently unlinking someone's `ln`-ed file is worse than the crash
//!   window we are closing.
//! * **Symlinks.** The target must be followed, not replaced — saving through a symlink must edit
//!   the file it points at, not turn the link into a regular file. Resolved up front, so the
//!   atomic path still applies to the real file.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

/// Replace `path`'s contents with `text` without ever leaving it truncated.
///
/// Permissions are carried over from the existing file; a fresh file gets the process umask, same
/// as `fs::write`. Returns the same errors `fs::write` would, plus any from the rename.
pub fn write(path: &Path, text: &str) -> std::io::Result<()> {
    let target = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());

    if multiply_linked(&target) {
        // Preserving the link is the user's intent; the crash window is the lesser evil.
        return fs::write(&target, text);
    }
    let Some(dir) = target.parent() else {
        return fs::write(&target, text);
    };

    let tmp = temp_beside(&target);
    // Any failure from here on must not leave the scratch file lying next to the user's source.
    let result = (|| -> std::io::Result<()> {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(text.as_bytes())?;
        // Durability before visibility: without this the rename can land while the DATA is still
        // in the page cache, so a crash yields an intact directory entry pointing at a zero-length
        // or partially-written file — precisely the outcome this module exists to prevent.
        f.sync_all()?;
        drop(f);
        if let Ok(meta) = fs::metadata(&target) {
            // A new file inherits the umask; an existing one keeps the mode it had (an executable
            // script must stay executable, a 0600 secret must stay 0600).
            let _ = fs::set_permissions(&tmp, meta.permissions());
        }
        fs::rename(&tmp, &target)
    })();

    match result {
        Ok(()) => {
            // Fsync the DIRECTORY so the rename itself survives a crash. Best-effort: some
            // filesystems refuse to open a directory for this, and failing the save over it
            // would be worse than the residual risk.
            let _ = fs::File::open(dir).map(|d| d.sync_all());
            Ok(())
        }
        Err(e) => {
            let _ = fs::remove_file(&tmp);
            // A directory we cannot create a temp file in (read-only mount, restrictive
            // permissions, some FUSE mounts) can still accept a direct write. Try it rather than
            // failing a save that would otherwise have worked.
            match fs::write(&target, text) {
                Ok(()) => Ok(()),
                Err(_) => Err(e),
            }
        }
    }
}

/// Does `path` have more than one hard link? False when unknown — the fallback is the
/// conservative direction only for links we can actually prove.
fn multiply_linked(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        return fs::metadata(path).map(|m| m.nlink() > 1).unwrap_or(false);
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        false
    }
}

/// A scratch path in the target's own directory — it MUST share the filesystem, or `rename`
/// fails with `EXDEV` and the whole point is lost. Dot-prefixed so a directory listing (and most
/// file watchers) ignore it if we die before the rename.
fn temp_beside(target: &Path) -> PathBuf {
    let name = target.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let dir = target.parent().unwrap_or_else(|| Path::new("."));
    // pid + a counter: unique across concurrent saves in this process and against other
    // processes, without needing a random source.
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    dir.join(format!(".{name}.cauldron-{}-{}", std::process::id(), N.fetch_add(1, Ordering::Relaxed)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static N: AtomicU32 = AtomicU32::new(0);
        let d = std::env::temp_dir().join(format!(
            "cauldron-sw-{}-{tag}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn writes_contents_and_replaces_them() {
        let d = scratch("basic");
        let p = d.join("a.txt");
        write(&p, "one").unwrap();
        assert_eq!(fs::read_to_string(&p).unwrap(), "one");
        write(&p, "two").unwrap();
        assert_eq!(fs::read_to_string(&p).unwrap(), "two");
    }

    #[test]
    fn leaves_no_temp_files_behind() {
        let d = scratch("clean");
        let p = d.join("a.txt");
        write(&p, "x").unwrap();
        let strays: Vec<_> = fs::read_dir(&d)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n != "a.txt")
            .collect();
        assert!(strays.is_empty(), "scratch files left next to the source: {strays:?}");
    }

    /// The mode must survive a save, or an executable script stops being executable and a
    /// 0600 file becomes world-readable.
    #[cfg(unix)]
    #[test]
    fn preserves_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let d = scratch("perm");
        let p = d.join("run.sh");
        fs::write(&p, "#!/bin/sh\n").unwrap();
        fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
        write(&p, "#!/bin/sh\necho hi\n").unwrap();
        let mode = fs::metadata(&p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o755, "executable bit lost across a save");
    }

    /// Saving THROUGH a symlink must edit the file it points at. Replacing the link with a
    /// regular file would quietly detach the user's file from wherever it really lives.
    #[cfg(unix)]
    #[test]
    fn follows_symlinks_instead_of_replacing_them() {
        let d = scratch("link");
        let real = d.join("real.txt");
        let link = d.join("link.txt");
        fs::write(&real, "old").unwrap();
        std::os::unix::fs::symlink(&real, &link).unwrap();
        write(&link, "new").unwrap();
        assert_eq!(fs::read_to_string(&real).unwrap(), "new", "the target must be updated");
        assert!(fs::symlink_metadata(&link).unwrap().file_type().is_symlink(), "link must survive");
    }

    /// A hard-linked file keeps every name pointing at the same contents. `rename` would break
    /// that, so this case deliberately writes in place.
    #[cfg(unix)]
    #[test]
    fn hard_links_stay_linked() {
        let d = scratch("hard");
        let a = d.join("a.txt");
        let b = d.join("b.txt");
        fs::write(&a, "old").unwrap();
        fs::hard_link(&a, &b).unwrap();
        write(&a, "new").unwrap();
        assert_eq!(fs::read_to_string(&b).unwrap(), "new", "the other name must see the change");
    }

    #[test]
    fn creates_a_file_that_did_not_exist() {
        let d = scratch("create");
        let p = d.join("fresh.txt");
        write(&p, "hello").unwrap();
        assert_eq!(fs::read_to_string(&p).unwrap(), "hello");
    }
}
