//! `download` plugin — fetch each entry in `stage.downloads` over HTTP and
//! materialise it under [`Download::path`].
//!
//! Port of `pkg/plugins/download.go::Download`. For each [`Download`]:
//!   1. Ensure the parent directory of `download.path` exists.
//!   2. HTTP `GET` `download.url` using a `reqwest::blocking` client with
//!      `timeout = download.timeout` (seconds; 0 → default 30s).
//!   3. Read the entire body into memory and write it to `download.path`
//!      via [`Vfs::write`]. We don't stream chunks to disk because the
//!      [`Vfs`] trait deliberately exposes only `write(&[u8])` — keeping it
//!      tiny is more useful than supporting GB-sized downloads, which yip
//!      doesn't do in practice.
//!   4. Apply `chmod` if `permissions != 0`.
//!   5. Apply `chown` for numeric owner/group; name-based owners are
//!      skipped with a warning (mirrors [`crate::plugins::files`]).
//!
//! All per-download errors are aggregated into [`Error::Multi`]; the loop
//! never aborts early, matching Go's `multierror.Append`.
//!
//! Differences from Go:
//!   - Go uses `cavaliergopher/grab` to write the body to disk directly
//!     (with progress ticks). We don't — see point 3 above. The progress
//!     log is therefore omitted; a single `debug!` is emitted per download.
//!   - Go falls back to `OwnerString` parsing via `/etc/passwd`. We log a
//!     warning and skip (same behaviour as the `files` plugin).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use tracing::{debug, info, warn};

use crate::console::Console;
use crate::error::{Error, Result};
use crate::executor::Plugin;
use crate::schema::file::Download;
#[cfg(test)]
use crate::schema::file::OwnerId;
use crate::schema::Stage;
use crate::vfs::Vfs;

/// Default HTTP timeout when `Download::timeout` is 0 (seconds). Matches
/// `grab`'s implicit "no timeout means use whatever the http.Client gives
/// you" — we pick something finite so a hanging server can't wedge a stage.
const DEFAULT_TIMEOUT_SECS: u64 = 30;

/// Build a [`Plugin`] arc-closure.
pub fn build() -> Plugin {
    Arc::new(run)
}

/// Pure entry point — exposed so tests don't have to go through `Arc`.
pub fn run(stage: &Stage, fs: &dyn Vfs, _console: &dyn Console) -> Result<()> {
    if stage.downloads.is_empty() {
        return Ok(());
    }

    info!(count = stage.downloads.len(), "downloading stage files");

    let mut errs: Vec<Error> = Vec::new();
    for dl in &stage.downloads {
        if let Err(e) = download_one(dl, fs) {
            warn!(path = %dl.path, url = %dl.url, error = %e, "download failed");
            errs.push(e);
        }
    }

    match errs.len() {
        0 => Ok(()),
        _ => Err(Error::Multi(errs)),
    }
}

fn download_one(dl: &Download, fs: &dyn Vfs) -> Result<()> {
    if dl.path.is_empty() {
        return Err(Error::other("download entry has empty path"));
    }
    if dl.url.is_empty() {
        return Err(Error::other(format!(
            "download entry for {} has empty url",
            dl.path
        )));
    }

    let requested = Path::new(&dl.path);
    // Resolve the on-disk path. Mirrors Go's `grab` behaviour: if the
    // user-supplied path ends with `/` (or already names an existing
    // directory on disk), the filename is derived from the URL.
    let resolved = resolve_download_path(&dl.url, requested, fs);
    let path = resolved.as_path();

    // 1. mkdir parent.
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            debug!(parent = %parent.display(), "ensuring parent dir");
            fs.mkdir_all(parent)?;
        }
    }

    // 2. HTTP GET with timeout.
    let timeout_secs = if dl.timeout > 0 {
        dl.timeout as u64
    } else {
        DEFAULT_TIMEOUT_SECS
    };
    debug!(url = %dl.url, path = %dl.path, timeout = timeout_secs, "downloading");

    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(timeout_secs))
        .user_agent("yip")
        .build()
        .map_err(|e| Error::other(format!("build http client: {e}")))?;

    let resp = client
        .get(&dl.url)
        .send()
        .map_err(|e| Error::other(format!("http get {}: {e}", dl.url)))?;

    if !resp.status().is_success() {
        return Err(Error::other(format!(
            "http get {}: status {}",
            dl.url,
            resp.status()
        )));
    }

    let bytes = resp
        .bytes()
        .map_err(|e| Error::other(format!("http body {}: {e}", dl.url)))?;

    // 3. Write to disk via Vfs.
    debug!(path = %dl.path, size = bytes.len(), "writing downloaded file");
    fs.write(path, &bytes)?;

    // 4. chmod.
    if dl.permissions != 0 {
        debug!(path = %dl.path, mode = format!("{:o}", dl.permissions), "chmod");
        fs.chmod(path, dl.permissions)?;
    }

    // 5. chown.
    apply_chown(path, dl, fs)?;

    Ok(())
}

/// Resolve the on-disk destination path for a download. Matches Go's
/// `cavaliergopher/grab` behaviour:
///
///   * If `requested` ends with a path separator (`/` or `\`), treat it
///     as a directory and append the filename derived from the URL.
///   * If `requested` already names an existing directory on the VFS,
///     same: append URL-derived filename.
///   * Otherwise, use `requested` verbatim.
///
/// The URL filename is the last non-empty path segment with the query
/// string stripped. Falls back to `"download"` when the URL is degenerate
/// (only scheme + host, for instance).
fn resolve_download_path(url: &str, requested: &Path, fs: &dyn Vfs) -> PathBuf {
    let s = requested.to_string_lossy();
    let looks_like_dir = s.ends_with('/') || s.ends_with('\\');
    let is_existing_dir = fs
        .metadata(requested)
        .map(|m| m.is_dir)
        .unwrap_or(false);

    if looks_like_dir || is_existing_dir {
        let filename = filename_from_url(url);
        requested.join(filename)
    } else {
        requested.to_path_buf()
    }
}

fn filename_from_url(url: &str) -> &str {
    // Strip query string first, THEN find the last non-empty path segment.
    // Doing it in reverse order would yield "" for paths like /a/b/?q=1
    // because the rsplit's first hit (`""` between `/` and `?`) wins.
    let no_query = url.split('?').next().unwrap_or(url);
    no_query
        .rsplit('/')
        .find(|s| !s.is_empty())
        .unwrap_or("download")
}

/// Apply ownership. Mirrors the `files` plugin: numeric owners pass
/// through, name-based owners log a warn and skip.
fn apply_chown(path: &Path, dl: &Download, fs: &dyn Vfs) -> Result<()> {
    if !dl.owner_string.is_empty() {
        warn!(
            path = %path.display(),
            owner_string = %dl.owner_string,
            "name-based owner_string not supported yet; skipping chown",
        );
        return Ok(());
    }
    if let Some(name) = dl.owner.as_name() {
        warn!(
            path = %path.display(),
            owner = %name,
            "name-based owner not supported yet; skipping chown",
        );
        return Ok(());
    }

    let uid = dl.owner.as_int();
    if uid == 0 && dl.group == 0 {
        return Ok(());
    }

    debug!(path = %path.display(), uid, gid = dl.group, "chown");
    fs.chown(path, uid, dl.group)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::console::RecordingConsole;
    use crate::vfs::MemVfs;

    #[test]
    fn empty_downloads_is_ok() {
        let stage = Stage::default();
        let fs = MemVfs::new();
        let console = RecordingConsole::new();
        run(&stage, &fs, &console).expect("empty downloads -> Ok");
    }

    #[test]
    fn downloads_body_to_path() {
        let mut server = mockito::Server::new();
        let m = server
            .mock("GET", "/file")
            .with_status(200)
            .with_body("hello")
            .create();

        let url = format!("{}/file", server.url());
        let stage = Stage {
            downloads: vec![Download {
                path: "/tmp/test/foo".to_string(),
                url,
                permissions: 0o644,
                ..Default::default()
            }],
            ..Default::default()
        };

        let fs = MemVfs::new();
        let console = RecordingConsole::new();
        run(&stage, &fs, &console).expect("download should succeed");
        m.assert();

        let got = fs
            .read(Path::new("/tmp/test/foo"))
            .expect("read written file");
        assert_eq!(got, b"hello");

        let meta = fs.metadata(Path::new("/tmp/test/foo")).expect("metadata");
        assert!(meta.is_file);
        assert_eq!(meta.mode, 0o644);
    }

    #[test]
    fn http_404_is_aggregated_error_other_downloads_proceed() {
        let mut server = mockito::Server::new();
        let ok_mock = server
            .mock("GET", "/good")
            .with_status(200)
            .with_body("OK-BODY")
            .create();
        let bad_mock = server.mock("GET", "/bad").with_status(404).create();
        let also_ok_mock = server
            .mock("GET", "/also-good")
            .with_status(200)
            .with_body("ALSO")
            .create();

        let stage = Stage {
            downloads: vec![
                Download {
                    path: "/d/good".to_string(),
                    url: format!("{}/good", server.url()),
                    ..Default::default()
                },
                Download {
                    path: "/d/bad".to_string(),
                    url: format!("{}/bad", server.url()),
                    ..Default::default()
                },
                Download {
                    path: "/d/also-good".to_string(),
                    url: format!("{}/also-good", server.url()),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };

        let fs = MemVfs::new();
        let console = RecordingConsole::new();
        let err = run(&stage, &fs, &console).expect_err("should aggregate one error");
        match err {
            Error::Multi(errs) => assert_eq!(errs.len(), 1, "exactly one failed download"),
            other => panic!("expected Error::Multi, got {other:?}"),
        }

        // The two good downloads landed; the failing one did not.
        ok_mock.assert();
        bad_mock.assert();
        also_ok_mock.assert();

        assert_eq!(fs.read(Path::new("/d/good")).unwrap(), b"OK-BODY");
        assert_eq!(fs.read(Path::new("/d/also-good")).unwrap(), b"ALSO");
        assert!(!fs.exists(Path::new("/d/bad")));
    }

    #[test]
    fn timeout_short_circuits_slow_server() {
        let mut server = mockito::Server::new();
        let m = server
            .mock("GET", "/slow")
            .with_status(200)
            .with_body("eventually")
            .with_chunked_body(|_| {
                // Sleep longer than the per-download timeout below.
                std::thread::sleep(Duration::from_secs(5));
                Ok(())
            })
            .expect_at_most(1)
            .create();

        let stage = Stage {
            downloads: vec![Download {
                path: "/slow".to_string(),
                url: format!("{}/slow", server.url()),
                timeout: 1,
                ..Default::default()
            }],
            ..Default::default()
        };

        let fs = MemVfs::new();
        let console = RecordingConsole::new();
        let err = run(&stage, &fs, &console).expect_err("timeout should error");
        match err {
            Error::Multi(errs) => assert_eq!(errs.len(), 1),
            other => panic!("expected Error::Multi, got {other:?}"),
        }
        // The slow path was not written.
        assert!(!fs.exists(Path::new("/slow")));
        // Don't strictly assert mockito's hit count — the timeout may abort
        // before or after the handler completes — but the matcher is set up
        // so it's allowed to be hit at most once.
        drop(m);
    }

    #[test]
    fn empty_path_is_error() {
        let stage = Stage {
            downloads: vec![Download {
                path: "".to_string(),
                url: "http://example.invalid/x".to_string(),
                ..Default::default()
            }],
            ..Default::default()
        };
        let fs = MemVfs::new();
        let console = RecordingConsole::new();
        let err = run(&stage, &fs, &console).expect_err("empty path -> err");
        match err {
            Error::Multi(errs) => assert_eq!(errs.len(), 1),
            other => panic!("expected Error::Multi, got {other:?}"),
        }
    }

    #[test]
    fn numeric_owner_is_applied() {
        let mut server = mockito::Server::new();
        let _m = server
            .mock("GET", "/owned")
            .with_status(200)
            .with_body("x")
            .create();

        let stage = Stage {
            downloads: vec![Download {
                path: "/owned".to_string(),
                url: format!("{}/owned", server.url()),
                owner: OwnerId::Numeric(1000),
                group: 1000,
                ..Default::default()
            }],
            ..Default::default()
        };
        let fs = MemVfs::new();
        let console = RecordingConsole::new();
        run(&stage, &fs, &console).expect("should succeed");

        let m = fs.metadata(Path::new("/owned")).expect("metadata");
        assert_eq!((m.uid, m.gid), (1000, 1000));
    }

    // --- Additional tests ported from Go behaviour expectations ---

    #[test]
    fn missing_url_is_error() {
        // Download with no `url` must error (already covered for empty
        // path; here we cover the symmetric case).
        let stage = Stage {
            downloads: vec![Download {
                path: "/d/foo".to_string(),
                url: String::new(),
                ..Default::default()
            }],
            ..Default::default()
        };
        let fs = MemVfs::new();
        let console = RecordingConsole::new();
        let err = run(&stage, &fs, &console).expect_err("empty url -> err");
        match err {
            Error::Multi(errs) => assert_eq!(errs.len(), 1),
            other => panic!("expected Multi, got {other:?}"),
        }
        assert!(!fs.exists(Path::new("/d/foo")));
    }

    #[test]
    fn nested_dir_is_created_before_write() {
        // Path has multiple non-existent parents — mkdir_all should create
        // them and write the body.
        let mut server = mockito::Server::new();
        let _m = server
            .mock("GET", "/nested")
            .with_status(200)
            .with_body("payload")
            .create();
        let stage = Stage {
            downloads: vec![Download {
                path: "/a/b/c/d/nested-file".to_string(),
                url: format!("{}/nested", server.url()),
                ..Default::default()
            }],
            ..Default::default()
        };
        let fs = MemVfs::new();
        let console = RecordingConsole::new();
        run(&stage, &fs, &console).expect("ok");

        assert!(fs.exists(Path::new("/a/b/c/d")));
        let got = fs.read(Path::new("/a/b/c/d/nested-file")).unwrap();
        assert_eq!(got, b"payload");
    }

    #[test]
    fn http_302_redirect_is_followed() {
        // reqwest follows up to N redirects by default. Mock a redirect
        // from /redir -> /dest and verify the final body lands.
        let mut server = mockito::Server::new();
        let dest_path = "/dest";
        let _m_dest = server
            .mock("GET", dest_path)
            .with_status(200)
            .with_body("FINAL")
            .create();
        let redir_target = format!("{}{}", server.url(), dest_path);
        let _m_redir = server
            .mock("GET", "/redir")
            .with_status(302)
            .with_header("Location", &redir_target)
            .create();

        let stage = Stage {
            downloads: vec![Download {
                path: "/got".to_string(),
                url: format!("{}/redir", server.url()),
                ..Default::default()
            }],
            ..Default::default()
        };
        let fs = MemVfs::new();
        let console = RecordingConsole::new();
        run(&stage, &fs, &console).expect("redirect followed");

        let body = fs.read(Path::new("/got")).expect("body written");
        assert_eq!(body, b"FINAL");
    }

    #[test]
    fn http_basic_auth_via_url_is_honoured() {
        // user:pass embedded in URL is passed in the Authorization header
        // by reqwest. We don't have a great way to inspect headers via
        // mockito without setting an explicit matcher, but we can ensure
        // the request succeeds and the body is captured.
        let mut server = mockito::Server::new();
        let _m = server
            .mock("GET", "/secret")
            .match_header("authorization", mockito::Matcher::Regex("Basic .+".to_string()))
            .with_status(200)
            .with_body("authed")
            .create();

        // Inject userinfo into URL — mockito's matcher confirms the header.
        let base = server.url();
        // base looks like "http://127.0.0.1:port"; splice "user:pass@" after scheme.
        let url = base.replacen("http://", "http://user:pass@", 1) + "/secret";

        let stage = Stage {
            downloads: vec![Download {
                path: "/secret-out".to_string(),
                url,
                ..Default::default()
            }],
            ..Default::default()
        };
        let fs = MemVfs::new();
        let console = RecordingConsole::new();
        run(&stage, &fs, &console).expect("basic auth fetch ok");
        let body = fs.read(Path::new("/secret-out")).expect("written");
        assert_eq!(body, b"authed");
    }

    #[test]
    fn timeout_zero_uses_default_30s() {
        // Validate `timeout: 0` does NOT short-circuit to a zero-timeout
        // client (which would refuse every request). Instead it should
        // default to DEFAULT_TIMEOUT_SECS (30s) — a fast mock response
        // therefore succeeds.
        let mut server = mockito::Server::new();
        let _m = server
            .mock("GET", "/zero")
            .with_status(200)
            .with_body("ok")
            .create();
        let stage = Stage {
            downloads: vec![Download {
                path: "/zero-out".to_string(),
                url: format!("{}/zero", server.url()),
                timeout: 0,
                ..Default::default()
            }],
            ..Default::default()
        };
        let fs = MemVfs::new();
        let console = RecordingConsole::new();
        run(&stage, &fs, &console).expect("timeout 0 -> default 30s");
        assert_eq!(fs.read(Path::new("/zero-out")).unwrap(), b"ok");
    }

    #[test]
    fn multiple_downloads_to_same_parent_dir_all_succeed() {
        // Smoke test exercising the per-download mkdir_all on the same
        // parent path repeatedly — must not error on the second.
        let mut server = mockito::Server::new();
        let _m1 = server
            .mock("GET", "/a")
            .with_status(200)
            .with_body("A")
            .create();
        let _m2 = server
            .mock("GET", "/b")
            .with_status(200)
            .with_body("B")
            .create();
        let stage = Stage {
            downloads: vec![
                Download {
                    path: "/d/a".to_string(),
                    url: format!("{}/a", server.url()),
                    ..Default::default()
                },
                Download {
                    path: "/d/b".to_string(),
                    url: format!("{}/b", server.url()),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        let fs = MemVfs::new();
        let console = RecordingConsole::new();
        run(&stage, &fs, &console).expect("ok");
        assert_eq!(fs.read(Path::new("/d/a")).unwrap(), b"A");
        assert_eq!(fs.read(Path::new("/d/b")).unwrap(), b"B");
    }

    #[test]
    fn permissions_set_to_executable() {
        // Mode-aware downloads: ensure non-default permissions (0o755) are
        // recorded by the Vfs.
        let mut server = mockito::Server::new();
        let _m = server
            .mock("GET", "/bin")
            .with_status(200)
            .with_body("#!/bin/sh\nexit 0\n")
            .create();
        let stage = Stage {
            downloads: vec![Download {
                path: "/usr/local/bin/foo".to_string(),
                url: format!("{}/bin", server.url()),
                permissions: 0o755,
                ..Default::default()
            }],
            ..Default::default()
        };
        let fs = MemVfs::new();
        let console = RecordingConsole::new();
        run(&stage, &fs, &console).expect("ok");
        let meta = fs.metadata(Path::new("/usr/local/bin/foo")).unwrap();
        assert_eq!(meta.mode & 0o777, 0o755, "exec bits should be set");
    }

    // -------------------------------------------------------------------
    // Path-resolution: directory-suffixed `path` should pull the
    // filename from the URL (matches Go's `grab` behaviour).
    // -------------------------------------------------------------------

    #[test]
    fn trailing_slash_path_derives_filename_from_url() {
        // Go: when `path` ends with `/`, the filename is taken from the
        // last segment of the URL. So `path=/tmp/` + url=`.../foo.tar.gz`
        // lands at `/tmp/foo.tar.gz`.
        let mut server = mockito::Server::new();
        let _m = server
            .mock("GET", "/foo.tar.gz")
            .with_status(200)
            .with_body("TARBALL")
            .create();

        let stage = Stage {
            downloads: vec![Download {
                path: "/tmp/".to_string(),
                url: format!("{}/foo.tar.gz", server.url()),
                ..Default::default()
            }],
            ..Default::default()
        };
        let fs = MemVfs::new();
        let console = RecordingConsole::new();
        run(&stage, &fs, &console).expect("ok");

        let got = fs
            .read(Path::new("/tmp/foo.tar.gz"))
            .expect("file under /tmp");
        assert_eq!(got, b"TARBALL");
    }

    #[test]
    fn existing_directory_path_derives_filename_from_url() {
        // Path resolves to an existing on-disk directory (no trailing
        // slash). grab treats this like the `/`-suffixed case.
        let fs = MemVfs::new();
        fs.mkdir_all(Path::new("/var/cache/foo")).unwrap();

        let mut server = mockito::Server::new();
        let _m = server
            .mock("GET", "/pkg.bin")
            .with_status(200)
            .with_body("BIN")
            .create();
        let stage = Stage {
            downloads: vec![Download {
                path: "/var/cache/foo".to_string(),
                url: format!("{}/pkg.bin", server.url()),
                ..Default::default()
            }],
            ..Default::default()
        };
        let console = RecordingConsole::new();
        run(&stage, &fs, &console).expect("ok");

        let got = fs
            .read(Path::new("/var/cache/foo/pkg.bin"))
            .expect("file under existing dir");
        assert_eq!(got, b"BIN");
    }

    #[test]
    fn url_with_query_string_strips_query_for_filename() {
        // grab uses only the path portion (pre-`?`) for the filename.
        // We unit-test `filename_from_url` directly here since mockito
        // matching with query strings is route-specific; the e2e write
        // path is exercised by the two tests above.
        assert_eq!(
            filename_from_url("http://example.com/file.zip?token=abc"),
            "file.zip"
        );
        assert_eq!(
            filename_from_url("http://example.com/a/b/file.zip?x=1&y=2"),
            "file.zip"
        );
    }

    #[test]
    fn filename_from_url_helper_basic() {
        assert_eq!(filename_from_url("http://x/foo.tar.gz"), "foo.tar.gz");
        assert_eq!(filename_from_url("http://x/a/b/c"), "c");
        assert_eq!(filename_from_url("http://x/a/b/?q=1"), "b");
        assert_eq!(filename_from_url("http://example.com/"), "example.com");
    }

    #[test]
    fn explicit_filename_path_is_not_rewritten() {
        // Sanity check: when the user supplies an explicit filename (no
        // trailing slash, not a dir on disk), we use it verbatim and do
        // NOT splice in the URL filename.
        let mut server = mockito::Server::new();
        let _m = server
            .mock("GET", "/different-name.bin")
            .with_status(200)
            .with_body("DATA")
            .create();
        let stage = Stage {
            downloads: vec![Download {
                path: "/exact/name.txt".to_string(),
                url: format!("{}/different-name.bin", server.url()),
                ..Default::default()
            }],
            ..Default::default()
        };
        let fs = MemVfs::new();
        let console = RecordingConsole::new();
        run(&stage, &fs, &console).expect("ok");
        assert_eq!(fs.read(Path::new("/exact/name.txt")).unwrap(), b"DATA");
        assert!(!fs.exists(Path::new("/exact/different-name.bin")));
    }
}
