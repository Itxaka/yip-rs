//! Tempdir-rooted [`Vfs`] — every guest path is rebased under a tempdir
//! so plugins can pretend to write `/etc/passwd` without touching the host.
//!
//! Mirrors immucore-rs's `State::path` pattern: strip the leading `/` then
//! join under the root.

use std::fs;
use std::path::{Path, PathBuf};

use walkdir::WalkDir;

use crate::error::{Error, Result};
use crate::vfs::vfs::{Metadata, Vfs};

/// Tempdir-rooted impl. All paths are joined under `root` so tests can
/// pretend to write to `/etc/passwd` without touching the host's
/// `/etc/passwd`.
pub struct TempVfs {
    pub root: PathBuf,
    _td: Option<tempfile::TempDir>,
}

impl TempVfs {
    /// Create a fresh tempdir-backed Vfs. `root` is the tempdir's path.
    pub fn new() -> Result<Self> {
        let td = tempfile::tempdir().map_err(Error::io)?;
        let root = td.path().to_path_buf();
        Ok(Self { root, _td: Some(td) })
    }

    /// Construct around an externally-managed tempdir path (for tests
    /// that want to share a single tempdir across multiple Vfs instances).
    pub fn with_root(root: PathBuf) -> Self {
        Self { root, _td: None }
    }

    /// Rebase a guest path under the tempdir root.
    pub fn host(&self, guest: &Path) -> PathBuf {
        // Strip leading "/" so join() doesn't discard the prefix.
        let stripped = guest.strip_prefix("/").unwrap_or(guest);
        self.root.join(stripped)
    }
}

fn io_at<P: AsRef<Path>>(path: P, err: std::io::Error) -> Error {
    Error::io_at(path.as_ref().to_path_buf(), err)
}

impl Vfs for TempVfs {
    fn read(&self, path: &Path) -> Result<Vec<u8>> {
        let p = self.host(path);
        fs::read(&p).map_err(|e| io_at(&p, e))
    }

    fn write(&self, path: &Path, bytes: &[u8]) -> Result<()> {
        let p = self.host(path);
        if let Some(parent) = p.parent() {
            fs::create_dir_all(parent).map_err(|e| io_at(parent, e))?;
        }
        fs::write(&p, bytes).map_err(|e| io_at(&p, e))
    }

    fn mkdir_all(&self, path: &Path) -> Result<()> {
        let p = self.host(path);
        fs::create_dir_all(&p).map_err(|e| io_at(&p, e))
    }

    fn read_dir(&self, path: &Path) -> Result<Vec<PathBuf>> {
        let p = self.host(path);
        let it = fs::read_dir(&p).map_err(|e| io_at(&p, e))?;
        let mut out = Vec::new();
        for entry in it {
            let entry = entry.map_err(|e| io_at(&p, e))?;
            out.push(entry.path());
        }
        Ok(out)
    }

    fn metadata(&self, path: &Path) -> Result<Metadata> {
        let p = self.host(path);
        let m = fs::symlink_metadata(&p).map_err(|e| io_at(&p, e))?;
        Ok(Metadata::from_std(&m))
    }

    fn exists(&self, path: &Path) -> bool {
        fs::symlink_metadata(self.host(path)).is_ok()
    }

    fn remove(&self, path: &Path) -> Result<()> {
        let p = self.host(path);
        let m = fs::symlink_metadata(&p).map_err(|e| io_at(&p, e))?;
        if m.is_dir() {
            fs::remove_dir(&p).map_err(|e| io_at(&p, e))
        } else {
            fs::remove_file(&p).map_err(|e| io_at(&p, e))
        }
    }

    fn remove_all(&self, path: &Path) -> Result<()> {
        let p = self.host(path);
        let m = match fs::symlink_metadata(&p) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(io_at(&p, e)),
        };
        if m.is_dir() {
            fs::remove_dir_all(&p).map_err(|e| io_at(&p, e))
        } else {
            fs::remove_file(&p).map_err(|e| io_at(&p, e))
        }
    }

    fn chmod(&self, path: &Path, mode: u32) -> Result<()> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let p = self.host(path);
            let perms = fs::Permissions::from_mode(mode);
            fs::set_permissions(&p, perms).map_err(|e| io_at(&p, e))
        }
        #[cfg(not(unix))]
        {
            let _ = (path, mode);
            Ok(())
        }
    }

    fn chown(&self, path: &Path, uid: i32, gid: i32) -> Result<()> {
        #[cfg(unix)]
        {
            use nix::unistd::{chown, Gid, Uid};
            let p = self.host(path);
            let u = if uid < 0 { None } else { Some(Uid::from_raw(uid as u32)) };
            let g = if gid < 0 { None } else { Some(Gid::from_raw(gid as u32)) };
            chown(&p, u, g).map_err(|e| Error::io_at(p.clone(), e.into()))
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
            let link = self.host(link);
            if let Some(parent) = link.parent() {
                fs::create_dir_all(parent).map_err(|e| io_at(parent, e))?;
            }
            // target is stored verbatim — callers may want a relative
            // symlink, or an absolute one that's "guest absolute"; we
            // don't try to be clever here.
            std::os::unix::fs::symlink(target, &link).map_err(|e| io_at(&link, e))
        }
        #[cfg(not(unix))]
        {
            let _ = (target, link);
            Err(Error::other("symlink not supported on non-unix"))
        }
    }

    fn walk(&self, root: &Path) -> Result<Vec<PathBuf>> {
        let host_root = self.host(root);
        let mut out = Vec::new();
        for entry in WalkDir::new(&host_root).follow_links(false) {
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

    #[test]
    fn host_strips_leading_slash() {
        let vfs = TempVfs::new().unwrap();
        let h = vfs.host(Path::new("/etc/passwd"));
        assert!(h.starts_with(&vfs.root));
        assert!(h.ends_with("etc/passwd"));
    }

    #[test]
    fn read_write_roundtrip() {
        let vfs = TempVfs::new().unwrap();
        vfs.write(Path::new("/etc/foo"), b"hello").unwrap();
        assert_eq!(vfs.read(Path::new("/etc/foo")).unwrap(), b"hello");
        assert_eq!(vfs.read_to_string(Path::new("/etc/foo")).unwrap(), "hello");
        // The point: we wrote under the tempdir root, not the host's /etc.
        assert!(vfs.root.join("etc/foo").exists());
    }

    #[test]
    fn mkdir_all_then_exists() {
        let vfs = TempVfs::new().unwrap();
        vfs.mkdir_all(Path::new("/a/b/c")).unwrap();
        assert!(vfs.exists(Path::new("/a/b/c")));
        assert!(vfs.metadata(Path::new("/a/b/c")).unwrap().is_dir);
    }

    #[test]
    fn read_dir_returns_entries() {
        let vfs = TempVfs::new().unwrap();
        vfs.write(Path::new("/d/x"), b"1").unwrap();
        vfs.write(Path::new("/d/y"), b"22").unwrap();
        let entries = vfs.read_dir(Path::new("/d")).unwrap();
        assert_eq!(entries.len(), 2);
    }

    #[test]
    fn metadata_reports_file_and_size() {
        let vfs = TempVfs::new().unwrap();
        vfs.write(Path::new("/f"), b"abcd").unwrap();
        let m = vfs.metadata(Path::new("/f")).unwrap();
        assert!(m.is_file);
        assert_eq!(m.size, 4);
    }

    #[test]
    fn walk_yields_all_files() {
        let vfs = TempVfs::new().unwrap();
        vfs.write(Path::new("/r/a/b/1"), b"x").unwrap();
        vfs.write(Path::new("/r/a/c/2"), b"y").unwrap();
        vfs.write(Path::new("/r/d/3"), b"z").unwrap();
        let files = vfs.walk(Path::new("/r")).unwrap();
        assert_eq!(files.len(), 3);
    }

    #[test]
    fn chmod_persists() {
        let vfs = TempVfs::new().unwrap();
        vfs.write(Path::new("/m"), b"x").unwrap();
        vfs.chmod(Path::new("/m"), 0o600).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(vfs.host(Path::new("/m"))).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
    }

    #[test]
    fn symlink_and_walk() {
        let vfs = TempVfs::new().unwrap();
        vfs.write(Path::new("/t.txt"), b"hi").unwrap();
        vfs.symlink(Path::new("/t.txt"), Path::new("/l.txt")).unwrap();
        assert!(vfs.metadata(Path::new("/l.txt")).unwrap().is_symlink);
    }

    #[test]
    fn remove_and_remove_all() {
        let vfs = TempVfs::new().unwrap();
        vfs.write(Path::new("/f"), b"x").unwrap();
        vfs.remove(Path::new("/f")).unwrap();
        assert!(!vfs.exists(Path::new("/f")));

        vfs.write(Path::new("/sub/inner/a"), b"a").unwrap();
        vfs.remove_all(Path::new("/sub")).unwrap();
        assert!(!vfs.exists(Path::new("/sub")));
        vfs.remove_all(Path::new("/sub")).unwrap();
    }

    #[test]
    fn with_root_keeps_externally_managed_dir() {
        let td = tempfile::tempdir().unwrap();
        let root = td.path().to_path_buf();
        {
            let vfs = TempVfs::with_root(root.clone());
            vfs.write(Path::new("/x"), b"shared").unwrap();
        }
        // tempdir still alive; data still readable through a fresh wrapper.
        let vfs2 = TempVfs::with_root(root.clone());
        assert_eq!(vfs2.read(Path::new("/x")).unwrap(), b"shared");
    }

    #[test]
    fn trait_roundtrip_temp() {
        let vfs = TempVfs::new().unwrap();
        // For TempVfs the trait test operates on guest-relative paths,
        // which TempVfs rebases. Use "/" as the conceptual root.
        trait_roundtrip(&vfs, Path::new("/"));
    }
}
