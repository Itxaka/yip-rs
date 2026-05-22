//! Production [`Vfs`] implementation backed by `std::fs`.
//!
//! Touches the host filesystem directly — use [`TempVfs`](super::TempVfs)
//! or [`MemVfs`](super::MemVfs) in tests.

use std::fs;
use std::path::{Path, PathBuf};

use walkdir::WalkDir;

use crate::error::{Error, Result};
use crate::vfs::vfs::{Metadata, Vfs};

/// Production impl backed by `std::fs`.
#[derive(Default)]
pub struct RealVfs;

impl RealVfs {
    pub fn new() -> Self {
        Self
    }
}

fn io_at<P: AsRef<Path>>(path: P, err: std::io::Error) -> Error {
    Error::io_at(path.as_ref().to_path_buf(), err)
}

impl Vfs for RealVfs {
    fn read(&self, path: &Path) -> Result<Vec<u8>> {
        fs::read(path).map_err(|e| io_at(path, e))
    }

    fn write(&self, path: &Path, bytes: &[u8]) -> Result<()> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent).map_err(|e| io_at(parent, e))?;
            }
        }
        fs::write(path, bytes).map_err(|e| io_at(path, e))
    }

    fn mkdir_all(&self, path: &Path) -> Result<()> {
        fs::create_dir_all(path).map_err(|e| io_at(path, e))
    }

    fn read_dir(&self, path: &Path) -> Result<Vec<PathBuf>> {
        let it = fs::read_dir(path).map_err(|e| io_at(path, e))?;
        let mut out = Vec::new();
        for entry in it {
            let entry = entry.map_err(|e| io_at(path, e))?;
            out.push(entry.path());
        }
        Ok(out)
    }

    fn metadata(&self, path: &Path) -> Result<Metadata> {
        let m = fs::symlink_metadata(path).map_err(|e| io_at(path, e))?;
        Ok(Metadata::from_std(&m))
    }

    fn exists(&self, path: &Path) -> bool {
        // symlink_metadata so dangling symlinks still report "exists" —
        // matches Go's os.Lstat-based semantics for `if _, err := fs.Stat(...)`.
        fs::symlink_metadata(path).is_ok()
    }

    fn remove(&self, path: &Path) -> Result<()> {
        let m = fs::symlink_metadata(path).map_err(|e| io_at(path, e))?;
        if m.is_dir() {
            fs::remove_dir(path).map_err(|e| io_at(path, e))
        } else {
            fs::remove_file(path).map_err(|e| io_at(path, e))
        }
    }

    fn remove_all(&self, path: &Path) -> Result<()> {
        let m = match fs::symlink_metadata(path) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(io_at(path, e)),
        };
        if m.is_dir() {
            fs::remove_dir_all(path).map_err(|e| io_at(path, e))
        } else {
            fs::remove_file(path).map_err(|e| io_at(path, e))
        }
    }

    fn chmod(&self, path: &Path, mode: u32) -> Result<()> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let perms = fs::Permissions::from_mode(mode);
            fs::set_permissions(path, perms).map_err(|e| io_at(path, e))
        }
        #[cfg(not(unix))]
        {
            let _ = mode;
            let _ = path;
            Ok(())
        }
    }

    fn chown(&self, path: &Path, uid: i32, gid: i32) -> Result<()> {
        #[cfg(unix)]
        {
            use nix::unistd::{chown, Gid, Uid};
            let u = if uid < 0 { None } else { Some(Uid::from_raw(uid as u32)) };
            let g = if gid < 0 { None } else { Some(Gid::from_raw(gid as u32)) };
            chown(path, u, g).map_err(|e| Error::io_at(path.to_path_buf(), e.into()))
        }
        #[cfg(not(unix))]
        {
            let _ = (path, uid, gid);
            Ok(())
        }
    }

    fn symlink(&self, target: &Path, link: &Path) -> Result<()> {
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(target, link).map_err(|e| io_at(link, e))
        }
        #[cfg(not(unix))]
        {
            let _ = (target, link);
            Err(Error::other("symlink not supported on non-unix"))
        }
    }

    fn walk(&self, root: &Path) -> Result<Vec<PathBuf>> {
        let mut out = Vec::new();
        for entry in WalkDir::new(root).follow_links(false) {
            // Mirror filepath.Walk's "return nil on err" pattern — drop
            // broken entries instead of aborting the walk.
            let Ok(entry) = entry else { continue };
            if entry.file_type().is_file() {
                out.push(entry.path().to_path_buf());
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vfs::tests_common::trait_roundtrip;

    fn td() -> tempfile::TempDir {
        tempfile::tempdir().expect("tempdir")
    }

    #[test]
    fn read_write_roundtrip() {
        let dir = td();
        let vfs = RealVfs::new();
        let p = dir.path().join("a.txt");
        vfs.write(&p, b"hello").unwrap();
        assert_eq!(vfs.read(&p).unwrap(), b"hello");
        assert_eq!(vfs.read_to_string(&p).unwrap(), "hello");
    }

    #[test]
    fn mkdir_all_then_exists() {
        let dir = td();
        let vfs = RealVfs::new();
        let p = dir.path().join("a/b/c");
        vfs.mkdir_all(&p).unwrap();
        assert!(vfs.exists(&p));
        let m = vfs.metadata(&p).unwrap();
        assert!(m.is_dir);
    }

    #[test]
    fn read_dir_returns_entries() {
        let dir = td();
        let vfs = RealVfs::new();
        vfs.write(&dir.path().join("x"), b"1").unwrap();
        vfs.write(&dir.path().join("y"), b"22").unwrap();
        let mut got: Vec<_> = vfs
            .read_dir(dir.path())
            .unwrap()
            .into_iter()
            .map(|p| p.file_name().unwrap().to_owned())
            .collect();
        got.sort();
        assert_eq!(got, vec![std::ffi::OsString::from("x"), std::ffi::OsString::from("y")]);
    }

    #[test]
    fn metadata_reports_file_and_size() {
        let dir = td();
        let vfs = RealVfs::new();
        let p = dir.path().join("f");
        vfs.write(&p, b"abcd").unwrap();
        let m = vfs.metadata(&p).unwrap();
        assert!(m.is_file);
        assert_eq!(m.size, 4);
    }

    #[test]
    fn walk_yields_all_files() {
        let dir = td();
        let vfs = RealVfs::new();
        vfs.write(&dir.path().join("a/b/1"), b"x").unwrap();
        vfs.write(&dir.path().join("a/c/2"), b"y").unwrap();
        vfs.write(&dir.path().join("d/3"), b"z").unwrap();
        let mut files = vfs.walk(dir.path()).unwrap();
        files.sort();
        assert_eq!(files.len(), 3);
    }

    #[test]
    fn chmod_persists() {
        let dir = td();
        let vfs = RealVfs::new();
        let p = dir.path().join("m");
        vfs.write(&p, b"x").unwrap();
        vfs.chmod(&p, 0o600).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&p).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
    }

    #[test]
    fn symlink_and_walk() {
        let dir = td();
        let vfs = RealVfs::new();
        let target = dir.path().join("t.txt");
        vfs.write(&target, b"hi").unwrap();
        let link = dir.path().join("l.txt");
        vfs.symlink(&target, &link).unwrap();
        let m = vfs.metadata(&link).unwrap();
        assert!(m.is_symlink);
    }

    #[test]
    fn remove_and_remove_all() {
        let dir = td();
        let vfs = RealVfs::new();
        let f = dir.path().join("f");
        vfs.write(&f, b"x").unwrap();
        vfs.remove(&f).unwrap();
        assert!(!vfs.exists(&f));

        let sub = dir.path().join("sub/inner");
        vfs.write(&sub.join("a"), b"a").unwrap();
        vfs.remove_all(&sub).unwrap();
        assert!(!vfs.exists(&sub));
        // idempotent on missing
        vfs.remove_all(&sub).unwrap();
    }

    #[test]
    fn trait_roundtrip_real() {
        let dir = td();
        // RealVfs needs a tempdir scope so we don't pollute the host —
        // use a subdir as the "root" in the shared trait test.
        let vfs = RealVfs::new();
        trait_roundtrip(&vfs, dir.path());
    }

    // ---- edge cases ----

    #[test]
    fn binary_data_with_nulls_and_non_utf8() {
        let dir = td();
        let vfs = RealVfs::new();
        let p = dir.path().join("bin");
        // Mixed nulls + non-UTF-8 bytes (0xFF/0xFE never appear in valid UTF-8).
        let payload: Vec<u8> = vec![0x00, 0xFF, 0x00, 0xFE, 0x7F, 0x80, 0x00, 0x00];
        vfs.write(&p, &payload).unwrap();
        assert_eq!(vfs.read(&p).unwrap(), payload);
        // read_to_string must fail cleanly (not panic) for non-UTF-8.
        assert!(vfs.read_to_string(&p).is_err());
    }

    #[test]
    fn read_dir_empty_vs_hundred_entries() {
        let dir = td();
        let vfs = RealVfs::new();
        let empty = dir.path().join("empty");
        vfs.mkdir_all(&empty).unwrap();
        assert_eq!(vfs.read_dir(&empty).unwrap().len(), 0);

        let big = dir.path().join("big");
        vfs.mkdir_all(&big).unwrap();
        for i in 0..100 {
            vfs.write(&big.join(format!("f{i:03}")), b"x").unwrap();
        }
        let mut entries = vfs.read_dir(&big).unwrap();
        assert_eq!(entries.len(), 100);
        // After sorting we should see f000..f099 in order.
        entries.sort();
        let first = entries[0].file_name().unwrap().to_string_lossy().into_owned();
        let last = entries[99].file_name().unwrap().to_string_lossy().into_owned();
        assert_eq!(first, "f000");
        assert_eq!(last, "f099");
    }

    #[test]
    fn walk_with_dangling_symlink() {
        let dir = td();
        let vfs = RealVfs::new();
        let target = dir.path().join("does-not-exist");
        let link = dir.path().join("dangling");
        vfs.symlink(&target, &link).unwrap();
        // Walk shouldn't blow up. follow_links(false) means the dangling
        // symlink is just skipped (it's not a regular file).
        let files = vfs.walk(dir.path()).unwrap();
        assert!(files.iter().all(|p| p != &link));
    }

    #[test]
    fn mkdir_all_when_component_is_a_file_errors() {
        let dir = td();
        let vfs = RealVfs::new();
        let f = dir.path().join("blocker");
        vfs.write(&f, b"x").unwrap();
        // Trying to mkdir under a regular file must fail.
        let err = vfs.mkdir_all(&f.join("child")).unwrap_err();
        assert!(matches!(err, Error::Io { .. }));
    }

    #[test]
    fn chmod_on_missing_path_errors() {
        let dir = td();
        let vfs = RealVfs::new();
        let err = vfs.chmod(&dir.path().join("nope"), 0o600).unwrap_err();
        assert!(matches!(err, Error::Io { .. }));
    }

    #[test]
    fn chown_neg_one_neg_one_is_noop() {
        let dir = td();
        let vfs = RealVfs::new();
        let p = dir.path().join("f");
        vfs.write(&p, b"x").unwrap();
        // Should succeed (or at worst return EPERM if we're not root)
        // — the important thing is that (-1, -1) doesn't try to set
        // anything and the existing owner is preserved.
        let before = vfs.metadata(&p).unwrap();
        let _ = vfs.chown(&p, -1, -1);
        let after = vfs.metadata(&p).unwrap();
        assert_eq!((before.uid, before.gid), (after.uid, after.gid));
    }

    #[test]
    fn remove_all_missing_is_idempotent() {
        let dir = td();
        let vfs = RealVfs::new();
        let missing = dir.path().join("never-existed");
        vfs.remove_all(&missing).unwrap();
        vfs.remove_all(&missing).unwrap();
    }

    #[test]
    fn write_overwrites_existing_with_truncation() {
        let dir = td();
        let vfs = RealVfs::new();
        let p = dir.path().join("ow");
        vfs.write(&p, b"longer original content").unwrap();
        // Overwrite with shorter — must truncate, not leave trailing bytes.
        vfs.write(&p, b"hi").unwrap();
        assert_eq!(vfs.read(&p).unwrap(), b"hi");
        assert_eq!(vfs.metadata(&p).unwrap().size, 2);
    }

    #[test]
    fn symlink_loop_walk_terminates() {
        let dir = td();
        let vfs = RealVfs::new();
        // a -> b, b -> a. Walk uses follow_links(false) so it shouldn't
        // recurse, but make sure it definitely terminates.
        let a = dir.path().join("a");
        let b = dir.path().join("b");
        vfs.symlink(&b, &a).unwrap();
        vfs.symlink(&a, &b).unwrap();
        let _files = vfs.walk(dir.path()).unwrap();
    }
}
