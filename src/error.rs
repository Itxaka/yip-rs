//! Crate-wide error type and result alias.
//!
//! `yip` exposes a single [`Error`] enum that every public fallible
//! function returns inside [`Result`]. Variants are grouped by source
//! (I/O, YAML parse, regex, subprocess, …) plus an [`Error::Multi`]
//! aggregate used by the executor to collect plugin failures without
//! aborting the whole stage — this mirrors the Go original's use of
//! [`hashicorp/go-multierror`](https://github.com/hashicorp/go-multierror).
//!
//! ## When to use which variant
//!
//! - Construct [`Error::Io`] via [`Error::io`] or [`Error::io_at`] so the
//!   offending path is attached.
//! - Use [`Error::other`] for one-off messages that don't fit any other
//!   variant.
//! - Let the [`From`] impls convert [`std::io::Error`], `String`, `&str`,
//!   [`serde_yaml::Error`], [`serde_json::Error`], and [`regex::Error`]
//!   automatically — the `?` operator handles it.
//!
//! # Examples
//!
//! ```
//! use yip::error::{Error, Result};
//!
//! fn do_thing() -> Result<()> {
//!     Err(Error::other("nope"))
//! }
//!
//! assert!(matches!(do_thing(), Err(Error::Other(_))));
//! ```
//!
//! # Stability
//!
//! Public API. The variant set is closed but `#[non_exhaustive]` is NOT
//! used today — adding a new variant is a breaking change.

use std::path::PathBuf;

use thiserror::Error;

/// All errors yip can raise.
///
/// Most variants carry enough context (path, command, plugin name) to
/// surface a useful error message without an extra wrapping layer. The
/// [`Error::Multi`] variant aggregates multiple errors for the executor's
/// "run every plugin, collect failures" semantics.
///
/// # Examples
///
/// ```
/// use yip::error::Error;
///
/// let e = Error::other("boom");
/// assert!(matches!(e, Error::Other(_)));
/// assert_eq!(e.to_string(), "boom");
/// ```
#[derive(Debug, Error)]
pub enum Error {
    /// I/O failure, optionally tagged with the path being touched.
    ///
    /// Build via [`Error::io`] (no path) or [`Error::io_at`] (with path).
    #[error("io error at {path:?}: {source}")]
    Io {
        /// The path being read/written/touched when the error happened,
        /// if known.
        path: Option<PathBuf>,
        /// The underlying OS error.
        #[source]
        source: std::io::Error,
    },

    /// YAML parse failure surfaced from [`serde_yaml`].
    #[error("yaml parse error: {0}")]
    Yaml(#[from] serde_yaml::Error),

    /// JSON parse/encode failure surfaced from [`serde_json`].
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),

    /// A spawned subprocess exited non-zero, or could not be spawned at
    /// all. `status = None` means the process never started or was
    /// killed by a signal.
    #[error("command `{cmd}` failed: exit {status:?}: {stderr}")]
    Cmd {
        /// The shell command that was run (as passed to `/bin/sh -c`).
        cmd: String,
        /// Exit code, or `None` if the process never produced one.
        status: Option<i32>,
        /// Captured stderr.
        stderr: String,
        /// Captured stdout.
        stdout: String,
    },

    /// Template rendering failed (sprig-subset engine error). Wraps a
    /// string because [`tera::Error`] is not `Send + Sync` everywhere.
    #[error("template error: {0}")]
    Template(String),

    /// Regex compile/match failure from the [`regex`] crate.
    #[error("regex error: {0}")]
    Regex(#[from] regex::Error),

    /// Schema-level validation error (e.g. unknown field, bad reference).
    #[error("schema error: {0}")]
    Schema(String),

    /// A plugin failed. The executor wraps the inner error so logs can
    /// attribute the failure to the right action.
    #[error("plugin `{plugin}` failed: {source}")]
    Plugin {
        /// Name the plugin was registered under (e.g. `"files"`).
        plugin: String,
        /// The original error from the plugin.
        #[source]
        source: Box<Error>,
    },

    /// More than one error occurred during a stage. Mirrors Go's
    /// [`multierror.Error`](https://pkg.go.dev/github.com/hashicorp/go-multierror).
    #[error("aggregate error ({} errors)", .0.len())]
    Multi(Vec<Error>),

    /// Catch-all for messages that don't fit any structured variant.
    /// Build via [`Error::other`] or one of the `From<String>` /
    /// `From<&str>` impls.
    #[error("{0}")]
    Other(String),
}

impl Error {
    /// Build an [`Error::Io`] without an associated path.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::io;
    /// use yip::error::Error;
    ///
    /// let e = Error::io(io::Error::new(io::ErrorKind::NotFound, "missing"));
    /// assert!(matches!(e, Error::Io { path: None, .. }));
    /// ```
    pub fn io(err: std::io::Error) -> Self {
        Self::Io { path: None, source: err }
    }

    /// Build an [`Error::Io`] tagged with the path that was being touched.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::io;
    /// use std::path::PathBuf;
    /// use yip::error::Error;
    ///
    /// let e = Error::io_at(
    ///     "/etc/missing",
    ///     io::Error::new(io::ErrorKind::NotFound, "nope"),
    /// );
    /// match e {
    ///     Error::Io { path: Some(p), .. } => assert_eq!(p, PathBuf::from("/etc/missing")),
    ///     _ => panic!("expected Io variant with path"),
    /// }
    /// ```
    pub fn io_at<P: Into<PathBuf>>(path: P, err: std::io::Error) -> Self {
        Self::Io { path: Some(path.into()), source: err }
    }

    /// Build an [`Error::Other`] from any string-ish input. The escape
    /// hatch for messages that don't fit a structured variant.
    ///
    /// # Examples
    ///
    /// ```
    /// use yip::error::Error;
    ///
    /// let e = Error::other("widget broke");
    /// assert_eq!(e.to_string(), "widget broke");
    /// ```
    pub fn other<S: Into<String>>(msg: S) -> Self {
        Self::Other(msg.into())
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Self::io(e)
    }
}

impl From<String> for Error {
    fn from(s: String) -> Self {
        Self::Other(s)
    }
}

impl From<&str> for Error {
    fn from(s: &str) -> Self {
        Self::Other(s.to_string())
    }
}

/// Convenience alias used throughout the crate.
///
/// # Examples
///
/// ```
/// use yip::error::{Error, Result};
///
/// fn ok_path() -> Result<u32> { Ok(42) }
/// fn err_path() -> Result<u32> { Err(Error::other("nope")) }
///
/// assert_eq!(ok_path().unwrap(), 42);
/// assert!(err_path().is_err());
/// ```
pub type Result<T> = std::result::Result<T, Error>;

#[cfg(test)]
mod tests {
    use super::*;
    use std::io;

    #[test]
    fn io_at_preserves_path() {
        let e = Error::io_at(
            "/etc/passwd",
            io::Error::new(io::ErrorKind::PermissionDenied, "denied"),
        );
        match e {
            Error::Io { path: Some(p), source } => {
                assert_eq!(p, PathBuf::from("/etc/passwd"));
                assert_eq!(source.kind(), io::ErrorKind::PermissionDenied);
            }
            other => panic!("expected Error::Io with path, got {other:?}"),
        }
    }

    #[test]
    fn multi_display_includes_child_count() {
        let multi = Error::Multi(vec![
            Error::other("first"),
            Error::other("second"),
            Error::other("third"),
        ]);
        let s = multi.to_string();
        // Display format embeds the count.
        assert!(s.contains("3"), "Multi display should mention count, got {s:?}");

        // And iterating the inner vector exposes every child's Display.
        if let Error::Multi(children) = multi {
            let rendered: Vec<String> = children.iter().map(|e| e.to_string()).collect();
            assert_eq!(rendered, vec!["first", "second", "third"]);
        } else {
            unreachable!();
        }
    }

    #[test]
    fn plugin_display_includes_plugin_name_and_source() {
        let inner = Error::other("inner-cause");
        let wrapped = Error::Plugin {
            plugin: "files".into(),
            source: Box::new(inner),
        };
        let s = wrapped.to_string();
        assert!(s.contains("files"), "expected plugin name in Display, got {s:?}");
        assert!(s.contains("inner-cause"), "expected source message in Display, got {s:?}");
    }

    #[test]
    fn cmd_display_includes_status_stderr_and_stdout_via_debug() {
        let e = Error::Cmd {
            cmd: "do-thing".into(),
            status: Some(42),
            stderr: "boom-on-stderr".into(),
            stdout: "ignored-stdout".into(),
        };
        let s = e.to_string();
        // Display contract from thiserror attribute: cmd, status, stderr.
        assert!(s.contains("do-thing"));
        assert!(s.contains("42"));
        assert!(s.contains("boom-on-stderr"));
        // stdout is preserved on the struct even though Display doesn't print it —
        // verify via Debug so a future Display rewrite that drops the field is caught.
        let dbg = format!("{e:?}");
        assert!(dbg.contains("ignored-stdout"));
    }

    #[test]
    fn from_impls_route_correctly() {
        // io::Error -> Error::Io
        let io_err: io::Error = io::Error::new(io::ErrorKind::NotFound, "missing");
        let e: Error = io_err.into();
        assert!(matches!(e, Error::Io { path: None, .. }));

        // serde_yaml::Error -> Error::Yaml. Build one by parsing garbage.
        let yaml_err = serde_yaml::from_str::<serde_yaml::Value>("foo: : :").unwrap_err();
        let e: Error = yaml_err.into();
        assert!(matches!(e, Error::Yaml(_)));

        // &str -> Error::Other
        let e: Error = "free-form".into();
        match e {
            Error::Other(s) => assert_eq!(s, "free-form"),
            other => panic!("expected Other, got {other:?}"),
        }

        // String -> Error::Other (extra sanity)
        let e: Error = String::from("owned-msg").into();
        match e {
            Error::Other(s) => assert_eq!(s, "owned-msg"),
            other => panic!("expected Other, got {other:?}"),
        }
    }
}
