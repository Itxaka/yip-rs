//! `files` plugin — materialise each entry in `stage.files`.
//!
//! Port of `pkg/plugins/files.go::EnsureFiles`. For each [`File`]:
//!   1. Ensure the parent directory exists (Go uses an `EnsureDirectories`
//!      side-call; we use [`Vfs::mkdir_all`] which is equivalent in effect).
//!   2. Decode `content` per `encoding`:
//!      - empty / `"string"`  → raw bytes
//!      - `"b64"` / `"base64"` → base64-decoded
//!      - `"gzip"`              → gzip-decoded
//!      - `"gz+b64"` / `"b64+gz"` → base64 then gzip
//!   3. Write the file via [`Vfs::write`].
//!   4. Apply `chmod` if `permissions != 0`.
//!   5. Apply `chown` for numeric owner/group; name-based owners are
//!      skipped with a warning (see TODO in [`apply_chown`]).
//!
//! All per-file errors are aggregated into [`Error::Multi`]; the loop never
//! aborts early.

use std::io::Read;
use std::path::Path;
use std::sync::Arc;

use base64::prelude::{Engine, BASE64_STANDARD};
use flate2::read::GzDecoder;
use tracing::{debug, info, warn};

use crate::console::Console;
use crate::error::{Error, Result};
use crate::executor::Plugin;
use crate::schema::file::File;
#[cfg(test)]
use crate::schema::file::OwnerId;
use crate::schema::Stage;
use crate::vfs::Vfs;

/// Build a [`Plugin`] arc-closure.
pub fn build() -> Plugin {
    Arc::new(run)
}

/// Pure entry point — exposed so tests don't have to go through `Arc`.
pub fn run(stage: &Stage, fs: &dyn Vfs, _console: &dyn Console) -> Result<()> {
    if stage.files.is_empty() {
        return Ok(());
    }

    info!(count = stage.files.len(), "writing stage files");

    let mut errs: Vec<Error> = Vec::new();
    for file in &stage.files {
        if let Err(e) = write_one(file, fs) {
            warn!(path = %file.path, error = %e, "failed to write file");
            errs.push(e);
        }
    }

    match errs.len() {
        0 => Ok(()),
        _ => Err(Error::Multi(errs)),
    }
}

fn write_one(file: &File, fs: &dyn Vfs) -> Result<()> {
    if file.path.is_empty() {
        return Err(Error::other("file entry has empty path"));
    }

    let path = Path::new(&file.path);

    // 1. mkdir parent. The Go plugin gives the parent an executable bit if
    // the requested file mode is < 0700; here we just always mkdir_all, and
    // leave parent perms to the Vfs default (most callers don't depend on
    // intermediate-dir perms, and `MemVfs` doesn't track them anyway).
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            debug!(parent = %parent.display(), "ensuring parent dir");
            fs.mkdir_all(parent)?;
        }
    }

    // 2. Decode content.
    let bytes = decode_content(&file.content, &file.encoding)?;

    // 3. Write.
    debug!(path = %file.path, size = bytes.len(), "writing file");
    fs.write(path, &bytes)?;

    // 4. chmod.
    if file.permissions != 0 {
        debug!(path = %file.path, mode = format!("{:o}", file.permissions), "chmod");
        fs.chmod(path, file.permissions)?;
    }

    // 5. chown.
    apply_chown(path, file, fs)?;

    Ok(())
}

/// Decode `content` according to `encoding`. Mirrors yip's
/// `pkg/schema/file.go::newDecoder` switch.
fn decode_content(content: &str, encoding: &str) -> Result<Vec<u8>> {
    let enc = encoding.trim().to_ascii_lowercase();
    match enc.as_str() {
        "" | "string" | "text" | "plain" => Ok(content.as_bytes().to_vec()),
        "b64" | "base64" => decode_b64(content),
        "gz" | "gzip" => decode_gzip(content.as_bytes()),
        "gz+b64" | "gzip+base64" | "b64+gz" | "base64+gzip" => {
            let raw = decode_b64(content)?;
            decode_gzip(&raw)
        }
        other => Err(Error::other(format!("unknown file encoding: {other}"))),
    }
}

fn decode_b64(content: &str) -> Result<Vec<u8>> {
    BASE64_STANDARD
        .decode(content.trim())
        .map_err(|e| Error::other(format!("base64 decode failed: {e}")))
}

fn decode_gzip(input: &[u8]) -> Result<Vec<u8>> {
    let mut dec = GzDecoder::new(input);
    let mut out = Vec::new();
    dec.read_to_end(&mut out)
        .map_err(|e| Error::other(format!("gzip decode failed: {e}")))?;
    Ok(out)
}

/// Apply file ownership. Name-based owners (via `OwnerId::Name` or the
/// separate `owner_string` field) are not yet supported — log a warn and
/// skip. Numeric owners are passed through verbatim.
fn apply_chown(path: &Path, file: &File, fs: &dyn Vfs) -> Result<()> {
    // Name-based owner via the dedicated OwnerString cloud-init field.
    if !file.owner_string.is_empty() {
        // TODO: shell out to `id -u <name>` / `id -g <name>` via console,
        // or use the `users` crate for /etc/passwd lookups.
        warn!(
            path = %path.display(),
            owner_string = %file.owner_string,
            "name-based owner_string not supported yet; skipping chown",
        );
        return Ok(());
    }
    if let Some(name) = file.owner.as_name() {
        warn!(
            path = %path.display(),
            owner = %name,
            "name-based owner not supported yet; skipping chown",
        );
        return Ok(());
    }

    let uid = file.owner.as_int();
    // Skip chown entirely when both are zero — avoids spuriously rewriting
    // ownership to root when the user didn't specify anything.
    if uid == 0 && file.group == 0 {
        return Ok(());
    }

    debug!(path = %path.display(), uid, gid = file.group, "chown");
    fs.chown(path, uid, file.group)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::console::RecordingConsole;
    use crate::vfs::MemVfs;

    #[test]
    fn empty_stage_is_ok() {
        let stage = Stage::default();
        let fs = MemVfs::new();
        let console = RecordingConsole::new();
        run(&stage, &fs, &console).expect("empty files -> Ok");
    }

    #[test]
    fn writes_plain_text_file_with_perms() {
        let stage = Stage {
            files: vec![File {
                path: "/tmp/test/foo".to_string(),
                content: "Test".to_string(),
                permissions: 0o644,
                ..Default::default()
            }],
            ..Default::default()
        };
        let fs = MemVfs::new();
        let console = RecordingConsole::new();
        run(&stage, &fs, &console).expect("write should succeed");

        let got = fs
            .read_to_string(Path::new("/tmp/test/foo"))
            .expect("read written file");
        assert_eq!(got, "Test");

        let m = fs.metadata(Path::new("/tmp/test/foo")).expect("metadata");
        assert!(m.is_file);
        assert_eq!(m.mode, 0o644);
    }

    #[test]
    fn b64_content_is_decoded() {
        // base64("hello") = "aGVsbG8="
        let stage = Stage {
            files: vec![File {
                path: "/etc/b64".to_string(),
                content: "aGVsbG8=".to_string(),
                encoding: "b64".to_string(),
                ..Default::default()
            }],
            ..Default::default()
        };
        let fs = MemVfs::new();
        let console = RecordingConsole::new();
        run(&stage, &fs, &console).expect("write should succeed");

        let got = fs.read(Path::new("/etc/b64")).expect("read");
        assert_eq!(got, b"hello");
    }

    #[test]
    fn base64_long_alias_also_works() {
        let stage = Stage {
            files: vec![File {
                path: "/etc/b64-long".to_string(),
                content: "aGVsbG8=".to_string(),
                encoding: "base64".to_string(),
                ..Default::default()
            }],
            ..Default::default()
        };
        let fs = MemVfs::new();
        let console = RecordingConsole::new();
        run(&stage, &fs, &console).expect("write should succeed");
        assert_eq!(fs.read(Path::new("/etc/b64-long")).unwrap(), b"hello");
    }

    #[test]
    fn gzip_b64_content_is_decoded() {
        // Build a gzip+base64 payload of "hello-gzip-world".
        use base64::prelude::Engine;
        use flate2::write::GzEncoder;
        use flate2::Compression;
        use std::io::Write;

        let payload = b"hello-gzip-world";
        let mut enc = GzEncoder::new(Vec::new(), Compression::default());
        enc.write_all(payload).unwrap();
        let gz = enc.finish().unwrap();
        let b64 = BASE64_STANDARD.encode(&gz);

        let stage = Stage {
            files: vec![File {
                path: "/g".to_string(),
                content: b64,
                encoding: "gz+b64".to_string(),
                ..Default::default()
            }],
            ..Default::default()
        };
        let fs = MemVfs::new();
        let console = RecordingConsole::new();
        run(&stage, &fs, &console).expect("write should succeed");
        assert_eq!(fs.read(Path::new("/g")).unwrap(), payload);
    }

    #[test]
    fn creates_missing_parent_dir() {
        // Mirrors the Go test: file at /testarea/dir/subdir/foo with no
        // intermediates pre-existing.
        let stage = Stage {
            files: vec![File {
                path: "/testarea/dir/subdir/foo".to_string(),
                content: "Test".to_string(),
                permissions: 0o640,
                ..Default::default()
            }],
            ..Default::default()
        };
        let fs = MemVfs::new();
        let console = RecordingConsole::new();
        run(&stage, &fs, &console).expect("write should succeed");

        assert!(fs.exists(Path::new("/testarea/dir/subdir")));
        assert!(fs.exists(Path::new("/testarea/dir/subdir/foo")));
        let got = fs.read(Path::new("/testarea/dir/subdir/foo")).unwrap();
        assert_eq!(got, b"Test");
    }

    #[test]
    fn multiple_files_all_created() {
        let stage = Stage {
            files: vec![
                File {
                    path: "/a".to_string(),
                    content: "1".to_string(),
                    ..Default::default()
                },
                File {
                    path: "/b".to_string(),
                    content: "2".to_string(),
                    ..Default::default()
                },
                File {
                    path: "/c".to_string(),
                    content: "3".to_string(),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        let fs = MemVfs::new();
        let console = RecordingConsole::new();
        run(&stage, &fs, &console).expect("all writes should succeed");
        assert_eq!(fs.read(Path::new("/a")).unwrap(), b"1");
        assert_eq!(fs.read(Path::new("/b")).unwrap(), b"2");
        assert_eq!(fs.read(Path::new("/c")).unwrap(), b"3");
    }

    #[test]
    fn numeric_owner_respected() {
        let stage = Stage {
            files: vec![File {
                path: "/owned".to_string(),
                content: "x".to_string(),
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

    #[test]
    fn name_owner_skipped_with_warn() {
        let stage = Stage {
            files: vec![File {
                path: "/named".to_string(),
                content: "x".to_string(),
                owner: OwnerId::Name("alice".to_string()),
                ..Default::default()
            }],
            ..Default::default()
        };
        let fs = MemVfs::new();
        let console = RecordingConsole::new();
        run(&stage, &fs, &console).expect("name owner skip should not error");

        let m = fs.metadata(Path::new("/named")).expect("metadata");
        // No chown applied.
        assert_eq!((m.uid, m.gid), (0, 0));
    }

    #[test]
    fn bad_encoding_is_aggregated_error() {
        let stage = Stage {
            files: vec![
                File {
                    path: "/good".to_string(),
                    content: "ok".to_string(),
                    ..Default::default()
                },
                File {
                    path: "/bad".to_string(),
                    content: "abc".to_string(),
                    encoding: "nonsense-encoding".to_string(),
                    ..Default::default()
                },
                File {
                    path: "/also-good".to_string(),
                    content: "ok".to_string(),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        let fs = MemVfs::new();
        let console = RecordingConsole::new();
        let err = run(&stage, &fs, &console).expect_err("should aggregate one error");
        match err {
            Error::Multi(errs) => assert_eq!(errs.len(), 1),
            other => panic!("expected Error::Multi, got {other:?}"),
        }
        assert!(fs.exists(Path::new("/good")));
        assert!(fs.exists(Path::new("/also-good")));
        assert!(!fs.exists(Path::new("/bad")));
    }

    #[test]
    fn decoder_handles_string_and_text_aliases() {
        assert_eq!(decode_content("hi", "").unwrap(), b"hi");
        assert_eq!(decode_content("hi", "string").unwrap(), b"hi");
        assert_eq!(decode_content("hi", "text").unwrap(), b"hi");
        assert_eq!(decode_content("hi", "plain").unwrap(), b"hi");
    }

    // -------------------------------------------------------------------
    // Ported from Go: extra encoding paths, owner_string warn, perm round
    // trips, large content, tilde/$HOME literal handling.
    // -------------------------------------------------------------------

    #[test]
    fn b64_plus_gz_alias_works() {
        // "b64+gz" should be equivalent to "gz+b64": base64 first, then gzip.
        use flate2::write::GzEncoder;
        use flate2::Compression;
        use std::io::Write;

        let payload = b"alt-alias-roundtrip";
        let mut enc = GzEncoder::new(Vec::new(), Compression::default());
        enc.write_all(payload).unwrap();
        let gz = enc.finish().unwrap();
        let b64 = BASE64_STANDARD.encode(&gz);

        let stage = Stage {
            files: vec![File {
                path: "/aliased".to_string(),
                content: b64,
                encoding: "b64+gz".to_string(),
                ..Default::default()
            }],
            ..Default::default()
        };
        let fs = MemVfs::new();
        let console = RecordingConsole::default();
        run(&stage, &fs, &console).expect("write should succeed");
        assert_eq!(fs.read(Path::new("/aliased")).unwrap(), payload);
    }

    #[test]
    fn owner_string_field_skipped_with_warn_no_panic() {
        // Cloud-init style: owner is filled into the dedicated `owner_string`
        // field. Plugin must NOT panic and must NOT chown — it logs a warn.
        let stage = Stage {
            files: vec![File {
                path: "/ostr".to_string(),
                content: "x".to_string(),
                owner_string: "alice".to_string(),
                ..Default::default()
            }],
            ..Default::default()
        };
        let fs = MemVfs::new();
        let console = RecordingConsole::default();
        run(&stage, &fs, &console).expect("owner_string warn path must not error");
        let m = fs.metadata(Path::new("/ostr")).expect("metadata");
        assert_eq!((m.uid, m.gid), (0, 0));
    }

    fn write_with_perm(perm: u32) {
        let path = format!("/perm/{:o}", perm);
        let stage = Stage {
            files: vec![File {
                path: path.clone(),
                content: "x".to_string(),
                permissions: perm,
                ..Default::default()
            }],
            ..Default::default()
        };
        let fs = MemVfs::new();
        let console = RecordingConsole::default();
        run(&stage, &fs, &console).expect("write ok");
        let m = fs.metadata(Path::new(&path)).expect("metadata");
        assert_eq!(m.mode, perm, "round-trip mode for {:o}", perm);
    }

    #[test]
    fn permissions_round_trip_via_memvfs() {
        // Each perm written and read back through MemVfs metadata.
        write_with_perm(0o600);
        write_with_perm(0o755);
        write_with_perm(0o777);
    }

    #[test]
    fn large_content_above_one_mib_writes_intact() {
        // > 1 MiB blob — ensure no truncation, no surprise reallocation bug.
        let big_size = 1024 * 1024 + 17; // just over 1 MiB
        let blob: Vec<u8> = (0..big_size).map(|i| (i % 251) as u8).collect();
        let content = String::from_utf8(blob.iter().map(|&b| (b % 64) + b'0').collect()).unwrap();
        let stage = Stage {
            files: vec![File {
                path: "/big".to_string(),
                content: content.clone(),
                ..Default::default()
            }],
            ..Default::default()
        };
        let fs = MemVfs::new();
        let console = RecordingConsole::default();
        run(&stage, &fs, &console).expect("large write ok");
        let got = fs.read(Path::new("/big")).expect("read big");
        assert_eq!(got.len(), content.len());
        assert_eq!(got, content.as_bytes());
    }

    #[test]
    fn tilde_and_dollarhome_paths_are_literal_not_expanded() {
        // Go behaviour: yip does not expand `~` or `$HOME`; the path is
        // written as-is. Same here.
        let stage = Stage {
            files: vec![
                File {
                    path: "/~/literal".to_string(),
                    content: "tilde".to_string(),
                    ..Default::default()
                },
                File {
                    path: "/$HOME/literal".to_string(),
                    content: "dollar".to_string(),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        let fs = MemVfs::new();
        let console = RecordingConsole::default();
        run(&stage, &fs, &console).expect("ok");
        assert!(fs.exists(Path::new("/~/literal")));
        assert!(fs.exists(Path::new("/$HOME/literal")));
        assert_eq!(fs.read(Path::new("/~/literal")).unwrap(), b"tilde");
        assert_eq!(fs.read(Path::new("/$HOME/literal")).unwrap(), b"dollar");
    }
}
