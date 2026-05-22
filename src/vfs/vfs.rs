//! The [`Vfs`] trait — the subset of `twpayne/go-vfs.FS` that yip plugins
//! actually call. Keeping this small means mock impls (TempVfs, MemVfs)
//! stay easy to write and unit tests stay fast.

use std::path::{Path, PathBuf};

use crate::error::{Error, Result};

/// Minimal filesystem trait — only the operations yip's plugins call.
///
/// Mirrors the subset of `twpayne/go-vfs.FS` that yip uses.
pub trait Vfs: Send + Sync {
    /// Read a file's bytes.
    fn read(&self, path: &Path) -> Result<Vec<u8>>;

    /// Read a file as UTF-8 (convenience).
    fn read_to_string(&self, path: &Path) -> Result<String> {
        let bytes = self.read(path)?;
        String::from_utf8(bytes).map_err(|e| Error::other(e.to_string()))
    }

    /// Write `bytes` to `path` (creating parent dirs as needed).
    fn write(&self, path: &Path, bytes: &[u8]) -> Result<()>;

    /// Create a directory and any missing parents.
    fn mkdir_all(&self, path: &Path) -> Result<()>;

    /// List the entries (file + dir names) of a directory.
    fn read_dir(&self, path: &Path) -> Result<Vec<PathBuf>>;

    /// Fetch metadata. Real impl returns std::fs::Metadata; mock impls
    /// return a synthesised version.
    fn metadata(&self, path: &Path) -> Result<Metadata>;

    /// True if the path exists.
    fn exists(&self, path: &Path) -> bool;

    /// Remove a file or empty directory.
    fn remove(&self, path: &Path) -> Result<()>;

    /// Recursively remove a directory and its contents.
    fn remove_all(&self, path: &Path) -> Result<()>;

    /// Set file permissions (Unix mode).
    fn chmod(&self, path: &Path, mode: u32) -> Result<()>;

    /// Set file owner. uid/gid of -1 means "leave unchanged".
    fn chown(&self, path: &Path, uid: i32, gid: i32) -> Result<()>;

    /// Create a symlink at `link` pointing to `target`.
    fn symlink(&self, target: &Path, link: &Path) -> Result<()>;

    /// Recursive walk yielding every file path under `root`. Errors at
    /// individual entries are skipped silently (mirrors `filepath.Walk`
    /// callback returning nil on error).
    fn walk(&self, root: &Path) -> Result<Vec<PathBuf>>;
}

/// File metadata struct that doesn't tie us to `std::fs::Metadata` so
/// `MemVfs` / `TempVfs` can synthesise it.
#[derive(Debug, Clone)]
pub struct Metadata {
    pub is_file: bool,
    pub is_dir: bool,
    pub is_symlink: bool,
    pub size: u64,
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
}

impl Metadata {
    /// Build from `std::fs::Metadata` (Unix-only fields fall back to 0 on
    /// non-Unix targets; we only ship Linux so in practice they're always
    /// populated).
    #[cfg(unix)]
    pub fn from_std(m: &std::fs::Metadata) -> Self {
        use std::os::unix::fs::MetadataExt;
        Self {
            is_file: m.is_file(),
            is_dir: m.is_dir(),
            is_symlink: m.file_type().is_symlink(),
            size: m.len(),
            mode: m.mode(),
            uid: m.uid(),
            gid: m.gid(),
        }
    }

    #[cfg(not(unix))]
    pub fn from_std(m: &std::fs::Metadata) -> Self {
        Self {
            is_file: m.is_file(),
            is_dir: m.is_dir(),
            is_symlink: m.file_type().is_symlink(),
            size: m.len(),
            mode: 0,
            uid: 0,
            gid: 0,
        }
    }
}
