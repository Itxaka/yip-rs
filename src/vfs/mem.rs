//! In-memory [`Vfs`] backed by `HashMap`s. Fastest for pure unit tests —
//! no filesystem syscalls, no tempdir cleanup.
//!
//! Files, directories, and symlinks live in separate maps so we can
//! cheaply distinguish them in `metadata` / `walk`. chmod/chown state
//! lives in a side-map keyed by path.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::error::{Error, Result};
use crate::vfs::vfs::{Metadata, Vfs};

#[derive(Default)]
struct Inner {
    files: HashMap<PathBuf, Vec<u8>>,
    dirs: HashSet<PathBuf>,
    symlinks: HashMap<PathBuf, PathBuf>,
    modes: HashMap<PathBuf, u32>,
    owners: HashMap<PathBuf, (u32, u32)>,
}

/// In-memory impl backed by `HashMap`s.
pub struct MemVfs {
    inner: Mutex<Inner>,
}

impl Default for MemVfs {
    fn default() -> Self {
        Self::new()
    }
}

impl MemVfs {
    pub fn new() -> Self {
        let mut inner = Inner::default();
        // Root always exists so mkdir_all("/x")'s ancestor walk has a base.
        inner.dirs.insert(PathBuf::from("/"));
        Self {
            inner: Mutex::new(inner),
        }
    }

    fn normalize(path: &Path) -> PathBuf {
        // Keep paths as-is; callers pass absolute-ish paths and we don't
        // try to canonicalize against an imaginary CWD. Trim trailing "/".
        let s = path.to_string_lossy();
        if s.len() > 1 && s.ends_with('/') {
            PathBuf::from(s.trim_end_matches('/').to_string())
        } else {
            path.to_path_buf()
        }
    }

    fn not_found(path: &Path) -> Error {
        Error::io_at(
            path.to_path_buf(),
            std::io::Error::new(std::io::ErrorKind::NotFound, "not found"),
        )
    }
}

impl Vfs for MemVfs {
    fn read(&self, path: &Path) -> Result<Vec<u8>> {
        let p = Self::normalize(path);
        let inner = self.inner.lock().unwrap();
        // Follow one level of symlink — yip's plugins don't chain symlinks
        // and Go's `vfs.ReadFile` follows symlinks transparently.
        let resolved = inner.symlinks.get(&p).cloned().unwrap_or(p.clone());
        inner
            .files
            .get(&resolved)
            .cloned()
            .ok_or_else(|| Self::not_found(path))
    }

    fn write(&self, path: &Path, bytes: &[u8]) -> Result<()> {
        let p = Self::normalize(path);
        let mut inner = self.inner.lock().unwrap();
        // Auto-create ancestor dirs (matches RealVfs::write behaviour).
        let mut cur = PathBuf::new();
        if let Some(parent) = p.parent() {
            for comp in parent.components() {
                cur.push(comp.as_os_str());
                inner.dirs.insert(cur.clone());
            }
        }
        inner.files.insert(p, bytes.to_vec());
        Ok(())
    }

    fn mkdir_all(&self, path: &Path) -> Result<()> {
        let p = Self::normalize(path);
        let mut inner = self.inner.lock().unwrap();
        let mut cur = PathBuf::new();
        for comp in p.components() {
            cur.push(comp.as_os_str());
            inner.dirs.insert(cur.clone());
        }
        Ok(())
    }

    fn read_dir(&self, path: &Path) -> Result<Vec<PathBuf>> {
        let p = Self::normalize(path);
        let inner = self.inner.lock().unwrap();
        if !inner.dirs.contains(&p) {
            return Err(Self::not_found(path));
        }
        let mut out: Vec<PathBuf> = Vec::new();
        for k in inner.files.keys() {
            if k.parent() == Some(p.as_path()) {
                out.push(k.clone());
            }
        }
        for d in inner.dirs.iter() {
            if d.as_path() == p.as_path() {
                continue;
            }
            if d.parent() == Some(p.as_path()) {
                out.push(d.clone());
            }
        }
        for k in inner.symlinks.keys() {
            if k.parent() == Some(p.as_path()) {
                out.push(k.clone());
            }
        }
        out.sort();
        out.dedup();
        Ok(out)
    }

    fn metadata(&self, path: &Path) -> Result<Metadata> {
        let p = Self::normalize(path);
        let inner = self.inner.lock().unwrap();
        let is_symlink = inner.symlinks.contains_key(&p);
        let is_dir = inner.dirs.contains(&p);
        let is_file = inner.files.contains_key(&p);
        if !is_symlink && !is_dir && !is_file {
            return Err(Self::not_found(path));
        }
        let size = if is_file {
            inner.files.get(&p).map(|v| v.len() as u64).unwrap_or(0)
        } else {
            0
        };
        let mode = inner.modes.get(&p).copied().unwrap_or_else(|| {
            if is_dir {
                0o40755
            } else if is_symlink {
                0o120777
            } else {
                0o100644
            }
        });
        let (uid, gid) = inner.owners.get(&p).copied().unwrap_or((0, 0));
        Ok(Metadata {
            is_file,
            is_dir,
            is_symlink,
            size,
            mode,
            uid,
            gid,
        })
    }

    fn exists(&self, path: &Path) -> bool {
        let p = Self::normalize(path);
        let inner = self.inner.lock().unwrap();
        inner.files.contains_key(&p)
            || inner.dirs.contains(&p)
            || inner.symlinks.contains_key(&p)
    }

    fn remove(&self, path: &Path) -> Result<()> {
        let p = Self::normalize(path);
        let mut inner = self.inner.lock().unwrap();
        if inner.files.remove(&p).is_some() {
            inner.modes.remove(&p);
            inner.owners.remove(&p);
            return Ok(());
        }
        if inner.symlinks.remove(&p).is_some() {
            return Ok(());
        }
        if inner.dirs.contains(&p) {
            // Must be empty
            let has_children = inner
                .files
                .keys()
                .chain(inner.dirs.iter())
                .chain(inner.symlinks.keys())
                .any(|k| k.parent() == Some(p.as_path()) && k.as_path() != p.as_path());
            if has_children {
                return Err(Error::io_at(
                    p.clone(),
                    std::io::Error::new(std::io::ErrorKind::Other, "directory not empty"),
                ));
            }
            inner.dirs.remove(&p);
            return Ok(());
        }
        Err(Self::not_found(path))
    }

    fn remove_all(&self, path: &Path) -> Result<()> {
        let p = Self::normalize(path);
        let mut inner = self.inner.lock().unwrap();
        let prefix = p.clone();
        // Helper: keep only paths NOT under prefix (and != prefix).
        let keep = |k: &PathBuf| !(k == &prefix || k.starts_with(&prefix));
        inner.files.retain(|k, _| keep(k));
        inner.dirs.retain(keep);
        inner.symlinks.retain(|k, _| keep(k));
        inner.modes.retain(|k, _| keep(k));
        inner.owners.retain(|k, _| keep(k));
        Ok(())
    }

    fn chmod(&self, path: &Path, mode: u32) -> Result<()> {
        let p = Self::normalize(path);
        let mut inner = self.inner.lock().unwrap();
        if !inner.files.contains_key(&p)
            && !inner.dirs.contains(&p)
            && !inner.symlinks.contains_key(&p)
        {
            return Err(Self::not_found(path));
        }
        inner.modes.insert(p, mode);
        Ok(())
    }

    fn chown(&self, path: &Path, uid: i32, gid: i32) -> Result<()> {
        let p = Self::normalize(path);
        let mut inner = self.inner.lock().unwrap();
        if !inner.files.contains_key(&p)
            && !inner.dirs.contains(&p)
            && !inner.symlinks.contains_key(&p)
        {
            return Err(Self::not_found(path));
        }
        let (cur_uid, cur_gid) = inner.owners.get(&p).copied().unwrap_or((0, 0));
        let new_uid = if uid < 0 { cur_uid } else { uid as u32 };
        let new_gid = if gid < 0 { cur_gid } else { gid as u32 };
        inner.owners.insert(p, (new_uid, new_gid));
        Ok(())
    }

    fn symlink(&self, target: &Path, link: &Path) -> Result<()> {
        let l = Self::normalize(link);
        let mut inner = self.inner.lock().unwrap();
        // ensure parent dirs exist (no-op if already)
        let mut cur = PathBuf::new();
        if let Some(parent) = l.parent() {
            for comp in parent.components() {
                cur.push(comp.as_os_str());
                inner.dirs.insert(cur.clone());
            }
        }
        inner.symlinks.insert(l, target.to_path_buf());
        Ok(())
    }

    fn walk(&self, root: &Path) -> Result<Vec<PathBuf>> {
        let r = Self::normalize(root);
        let inner = self.inner.lock().unwrap();
        let mut out: Vec<PathBuf> = inner
            .files
            .keys()
            .filter(|k| k.starts_with(&r))
            .cloned()
            .collect();
        out.sort();
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vfs::tests_common::trait_roundtrip;

    #[test]
    fn read_write_roundtrip() {
        let vfs = MemVfs::new();
        vfs.write(Path::new("/a"), b"hello").unwrap();
        assert_eq!(vfs.read(Path::new("/a")).unwrap(), b"hello");
        assert_eq!(vfs.read_to_string(Path::new("/a")).unwrap(), "hello");
    }

    #[test]
    fn mkdir_all_then_exists() {
        let vfs = MemVfs::new();
        vfs.mkdir_all(Path::new("/a/b/c")).unwrap();
        assert!(vfs.exists(Path::new("/a")));
        assert!(vfs.exists(Path::new("/a/b")));
        assert!(vfs.exists(Path::new("/a/b/c")));
        assert!(vfs.metadata(Path::new("/a/b/c")).unwrap().is_dir);
    }

    #[test]
    fn read_dir_returns_entries() {
        let vfs = MemVfs::new();
        vfs.write(Path::new("/d/x"), b"1").unwrap();
        vfs.write(Path::new("/d/y"), b"22").unwrap();
        vfs.mkdir_all(Path::new("/d/sub")).unwrap();
        let entries = vfs.read_dir(Path::new("/d")).unwrap();
        assert_eq!(entries.len(), 3);
    }

    #[test]
    fn metadata_reports_file_and_size() {
        let vfs = MemVfs::new();
        vfs.write(Path::new("/f"), b"abcd").unwrap();
        let m = vfs.metadata(Path::new("/f")).unwrap();
        assert!(m.is_file);
        assert_eq!(m.size, 4);
        // default mode for files
        assert_eq!(m.mode & 0o777, 0o644);
    }

    #[test]
    fn walk_yields_all_files() {
        let vfs = MemVfs::new();
        vfs.write(Path::new("/r/a/b/1"), b"x").unwrap();
        vfs.write(Path::new("/r/a/c/2"), b"y").unwrap();
        vfs.write(Path::new("/r/d/3"), b"z").unwrap();
        vfs.write(Path::new("/other"), b"q").unwrap();
        let files = vfs.walk(Path::new("/r")).unwrap();
        assert_eq!(files.len(), 3);
    }

    #[test]
    fn chmod_persists_and_reads_back() {
        let vfs = MemVfs::new();
        vfs.write(Path::new("/m"), b"x").unwrap();
        vfs.chmod(Path::new("/m"), 0o600).unwrap();
        let m = vfs.metadata(Path::new("/m")).unwrap();
        assert_eq!(m.mode, 0o600);
    }

    #[test]
    fn chown_persists_and_neg_one_keeps() {
        let vfs = MemVfs::new();
        vfs.write(Path::new("/o"), b"x").unwrap();
        vfs.chown(Path::new("/o"), 1000, 1000).unwrap();
        let m = vfs.metadata(Path::new("/o")).unwrap();
        assert_eq!((m.uid, m.gid), (1000, 1000));
        // -1 keeps existing
        vfs.chown(Path::new("/o"), -1, 42).unwrap();
        let m2 = vfs.metadata(Path::new("/o")).unwrap();
        assert_eq!((m2.uid, m2.gid), (1000, 42));
    }

    #[test]
    fn symlink_and_walk() {
        let vfs = MemVfs::new();
        vfs.write(Path::new("/t.txt"), b"hi").unwrap();
        vfs.symlink(Path::new("/t.txt"), Path::new("/l.txt")).unwrap();
        let m = vfs.metadata(Path::new("/l.txt")).unwrap();
        assert!(m.is_symlink);
        // reading symlink follows it
        assert_eq!(vfs.read(Path::new("/l.txt")).unwrap(), b"hi");
    }

    #[test]
    fn remove_and_remove_all() {
        let vfs = MemVfs::new();
        vfs.write(Path::new("/f"), b"x").unwrap();
        vfs.remove(Path::new("/f")).unwrap();
        assert!(!vfs.exists(Path::new("/f")));

        vfs.write(Path::new("/sub/inner/a"), b"a").unwrap();
        vfs.write(Path::new("/sub/inner/b"), b"b").unwrap();
        // remove on non-empty dir should fail
        assert!(vfs.remove(Path::new("/sub")).is_err());
        vfs.remove_all(Path::new("/sub")).unwrap();
        assert!(!vfs.exists(Path::new("/sub")));
        assert!(!vfs.exists(Path::new("/sub/inner/a")));
        // idempotent
        vfs.remove_all(Path::new("/sub")).unwrap();
    }

    #[test]
    fn trait_roundtrip_mem() {
        let vfs = MemVfs::new();
        trait_roundtrip(&vfs, Path::new("/"));
    }
}
