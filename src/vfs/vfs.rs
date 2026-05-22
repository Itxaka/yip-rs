//! The [`Vfs`] trait — minimal filesystem abstraction used by every
//! yip plugin.
//!
//! Keeping this small means mock impls
//! ([`crate::vfs::TempVfs`], [`crate::vfs::MemVfs`]) stay easy to write
//! and unit tests stay fast. The production impl
//! [`crate::vfs::RealVfs`] forwards to [`std::fs`].
//!
//! ## When to use which impl
//!
//! - **Production / the binary**: [`crate::vfs::RealVfs`] — operates on
//!   the host filesystem directly.
//! - **Tests with on-disk paths**: [`crate::vfs::TempVfs`] — wraps a
//!   [`tempfile::TempDir`] so reads/writes go to a scratch dir that gets
//!   cleaned up on drop.
//! - **Pure unit tests**: [`crate::vfs::MemVfs`] — an in-memory tree, no
//!   syscalls at all.
//!
//! ## Why a trait (instead of `&dyn`-ing `std::fs` directly)?
//!
//! Plugins that take a `&dyn Vfs` are trivially testable: pass `MemVfs`
//! and assert against `read_to_string` afterwards, no `tempdir` ceremony
//! and no flaky cleanup. The trait also lets us synthesise [`Metadata`]
//! for impls that have no real inode underneath (mem fs has no `mode`,
//! but we can return a synthetic 0o644 for files).
//!
//! # Examples
//!
//! ```
//! use std::path::Path;
//! use yip::vfs::{MemVfs, Vfs};
//!
//! let fs = MemVfs::new();
//! fs.write(Path::new("/hello"), b"world").unwrap();
//! assert_eq!(fs.read_to_string(Path::new("/hello")).unwrap(), "world");
//! ```
//!
//! # Stability
//!
//! Public API. Adding methods without a default impl is breaking for
//! downstream impls; prefer providing a default that calls the existing
//! primitives.

use std::path::{Path, PathBuf};

use crate::error::{Error, Result};

/// Minimal filesystem trait — only the operations yip's plugins call.
///
/// Mirrors the subset of [twpayne/go-vfs.FS](https://github.com/twpayne/go-vfs)
/// that yip actually uses. Production impl is [`crate::vfs::RealVfs`];
/// tests use [`crate::vfs::TempVfs`] (real I/O against a tempdir) or
/// [`crate::vfs::MemVfs`] (no I/O at all).
///
/// All methods take `&self` so a single VFS can be shared across
/// threads; impls must be `Send + Sync`.
///
/// # Examples
///
/// ```
/// use std::path::Path;
/// use yip::vfs::{MemVfs, Vfs};
///
/// let fs = MemVfs::new();
/// fs.mkdir_all(Path::new("/a/b")).unwrap();
/// fs.write(Path::new("/a/b/c"), b"hi").unwrap();
/// assert!(fs.exists(Path::new("/a/b/c")));
/// ```
pub trait Vfs: Send + Sync {
    /// Read a file's bytes.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Io`] if the file doesn't exist, can't be read,
    /// or is otherwise inaccessible.
    fn read(&self, path: &Path) -> Result<Vec<u8>>;

    /// Read a file as UTF-8.
    ///
    /// Convenience wrapper over [`Vfs::read`].
    ///
    /// # Errors
    ///
    /// Returns [`Error::Io`] on read failure, or [`Error::Other`] if the
    /// bytes aren't valid UTF-8.
    fn read_to_string(&self, path: &Path) -> Result<String> {
        let bytes = self.read(path)?;
        String::from_utf8(bytes).map_err(|e| Error::other(e.to_string()))
    }

    /// Write `bytes` to `path`, creating parent dirs as needed.
    ///
    /// Overwrites existing files.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Io`] if the parent dir can't be created or the
    /// write fails.
    fn write(&self, path: &Path, bytes: &[u8]) -> Result<()>;

    /// Create a directory and any missing parents (`mkdir -p`).
    ///
    /// No-op if the directory already exists.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Io`] if a path component exists but is not a
    /// directory, or on permission failure.
    fn mkdir_all(&self, path: &Path) -> Result<()>;

    /// List the entries (file + dir names) of a directory.
    ///
    /// Returns full paths (joined with `path`). Order is implementation
    /// defined.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Io`] if `path` is not a directory or can't be
    /// read.
    fn read_dir(&self, path: &Path) -> Result<Vec<PathBuf>>;

    /// Fetch metadata.
    ///
    /// Real impl returns data from [`std::fs::Metadata`]; mock impls
    /// return a synthesised version.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Io`] if `path` doesn't exist or can't be
    /// stat'd.
    fn metadata(&self, path: &Path) -> Result<Metadata>;

    /// True if the path exists.
    ///
    /// Does NOT follow symlinks differently from `std::fs::metadata` —
    /// behaviour matches `Path::exists()` for the production impl.
    fn exists(&self, path: &Path) -> bool;

    /// Remove a file or empty directory.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Io`] if `path` doesn't exist, is a non-empty
    /// directory, or can't be removed.
    fn remove(&self, path: &Path) -> Result<()>;

    /// Recursively remove a directory and its contents (`rm -rf`).
    ///
    /// # Errors
    ///
    /// Returns [`Error::Io`] on permission failure or if `path` is in
    /// use.
    fn remove_all(&self, path: &Path) -> Result<()>;

    /// Set file permissions (Unix mode).
    ///
    /// # Errors
    ///
    /// Returns [`Error::Io`] if `path` doesn't exist or the caller
    /// lacks permission.
    fn chmod(&self, path: &Path, mode: u32) -> Result<()>;

    /// Set file owner.
    ///
    /// `uid`/`gid` of `-1` means "leave unchanged" (matching the libc
    /// `chown(2)` convention).
    ///
    /// # Errors
    ///
    /// Returns [`Error::Io`] if `path` doesn't exist or the caller
    /// lacks `CAP_CHOWN`.
    fn chown(&self, path: &Path, uid: i32, gid: i32) -> Result<()>;

    /// Create a symlink at `link` pointing to `target`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Io`] if `link` already exists or its parent
    /// directory doesn't.
    fn symlink(&self, target: &Path, link: &Path) -> Result<()>;

    /// Recursive walk yielding every file path under `root`.
    ///
    /// Errors at individual entries are skipped silently (mirrors
    /// [`std::fs::read_dir`] semantics in `filepath.Walk` callbacks
    /// returning nil on error).
    ///
    /// # Errors
    ///
    /// Returns [`Error::Io`] if `root` itself can't be read.
    fn walk(&self, root: &Path) -> Result<Vec<PathBuf>>;
}

/// File metadata struct decoupled from [`std::fs::Metadata`].
///
/// Decoupled so [`crate::vfs::MemVfs`] / [`crate::vfs::TempVfs`] can
/// synthesise a value without going through the OS.
///
/// # Examples
///
/// ```
/// use yip::vfs::Metadata;
///
/// let m = Metadata {
///     is_file: true,
///     is_dir: false,
///     is_symlink: false,
///     size: 7,
///     mode: 0o644,
///     uid: 0,
///     gid: 0,
/// };
/// assert!(m.is_file);
/// assert_eq!(m.size, 7);
/// ```
#[derive(Debug, Clone)]
pub struct Metadata {
    /// True if the inode is a regular file.
    pub is_file: bool,
    /// True if the inode is a directory.
    pub is_dir: bool,
    /// True if the inode is a symlink (not followed).
    pub is_symlink: bool,
    /// File size in bytes (0 for directories).
    pub size: u64,
    /// Unix permission bits + type bits (e.g. `0o100644`).
    pub mode: u32,
    /// Owning user id.
    pub uid: u32,
    /// Owning group id.
    pub gid: u32,
}

impl Metadata {
    /// Build a [`Metadata`] from a [`std::fs::Metadata`].
    ///
    /// Unix-only fields fall back to `0` on non-Unix targets; we only
    /// ship Linux so in practice they're always populated.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use yip::vfs::Metadata;
    ///
    /// let std_md = std::fs::metadata("/etc/hostname").unwrap();
    /// let md = Metadata::from_std(&std_md);
    /// assert!(md.is_file);
    /// ```
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

    /// Non-unix build of [`Metadata::from_std`]: `mode`/`uid`/`gid`
    /// default to 0 because the underlying [`std::fs::Metadata`] doesn't
    /// expose them.
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
