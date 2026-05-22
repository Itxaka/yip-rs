//! `directories` plugin — ensure each entry in `stage.directories` exists
//! with the requested permissions and ownership.
//!
//! Port of `pkg/plugins/dir.go::EnsureDirectories`. The Go version walks
//! up the path tree manually, creating missing parents one at a time and
//! applying permissions; we lean on [`Vfs::mkdir_all`] which does the
//! ancestor walk for us, then apply `chmod` / `chown` on the final path.
//! This preserves the user-visible behaviour (final dir has the requested
//! perms; intermediates exist) — Go's per-level chown is a side-effect we
//! don't try to replicate, since it depends on whether each ancestor was
//! newly created vs already present, and the Go logic itself only applies
//! perms on the top-level entry in that case.
//!
//! Name-based owner/group is currently not supported: we log a warning and
//! skip the chown. Adding `users`-crate lookups is tracked as a TODO.
//!
//! Per-directory errors are aggregated into [`Error::Multi`]; the loop
//! never aborts early.

use std::path::Path;
use std::sync::Arc;

use tracing::{debug, info, warn};

use crate::console::Console;
use crate::error::{Error, Result};
use crate::executor::Plugin;
use crate::schema::file::{Directory, OwnerId};
use crate::schema::Stage;
use crate::vfs::Vfs;

/// Build a [`Plugin`] arc-closure.
pub fn build() -> Plugin {
    Arc::new(run)
}

/// Pure entry point — exposed so tests don't have to go through `Arc`.
pub fn run(stage: &Stage, fs: &dyn Vfs, _console: &dyn Console) -> Result<()> {
    if stage.directories.is_empty() {
        return Ok(());
    }

    info!(count = stage.directories.len(), "ensuring directories");

    let mut errs: Vec<Error> = Vec::new();
    for dir in &stage.directories {
        if let Err(e) = ensure_directory(dir, fs) {
            warn!(path = %dir.path, error = %e, "failed to ensure directory");
            errs.push(e);
        }
    }

    match errs.len() {
        0 => Ok(()),
        _ => Err(Error::Multi(errs)),
    }
}

fn ensure_directory(dir: &Directory, fs: &dyn Vfs) -> Result<()> {
    if dir.path.is_empty() {
        return Err(Error::other("directory entry has empty path"));
    }

    let path = Path::new(&dir.path);
    debug!(path = %dir.path, "creating directory");
    fs.mkdir_all(path)?;

    if dir.permissions != 0 {
        debug!(path = %dir.path, mode = format!("{:o}", dir.permissions), "chmod");
        fs.chmod(path, dir.permissions)?;
    }

    apply_chown(path, &dir.owner, dir.group, fs)?;
    Ok(())
}

/// Apply ownership when `owner` or `group` is set. Name-based owners are
/// not yet supported — we log a warning and skip rather than fail.
fn apply_chown(path: &Path, owner: &OwnerId, group: i32, fs: &dyn Vfs) -> Result<()> {
    // Name-based owner — TODO: shell out to `id -u <name>` / `id -g <name>`
    // via the console, or use the `users` crate. For now, warn + skip.
    if let Some(name) = owner.as_name() {
        warn!(
            path = %path.display(),
            owner = %name,
            "name-based owner not supported yet; skipping chown",
        );
        return Ok(());
    }

    let uid = owner.as_int();
    // Only chown if either uid or gid is explicitly set (non-zero). Matches
    // the spirit of Go's plugin: an unset Owner/Group is 0, and yip's
    // RealVfs treats chown(0,0) as "set to root" — but for default Stage
    // entries with no owner specified we want a no-op.
    if uid == 0 && group == 0 {
        return Ok(());
    }

    debug!(path = %path.display(), uid, gid = group, "chown");
    fs.chown(path, uid, group)
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
        run(&stage, &fs, &console).expect("empty directories -> Ok");
    }

    #[test]
    fn creates_one_dir_with_perm() {
        let stage = Stage {
            directories: vec![Directory {
                path: "/tmp/dir".to_string(),
                permissions: 0o755,
                ..Default::default()
            }],
            ..Default::default()
        };
        let fs = MemVfs::new();
        let console = RecordingConsole::new();
        run(&stage, &fs, &console).expect("should succeed");

        assert!(fs.exists(Path::new("/tmp/dir")));
        let m = fs.metadata(Path::new("/tmp/dir")).expect("metadata");
        assert!(m.is_dir);
        assert_eq!(m.mode, 0o755);
    }

    #[test]
    fn creates_nested_dirs() {
        // Mirrors the Go test that asks for /tmp/dir/subdir1/subdir2.
        let stage = Stage {
            directories: vec![Directory {
                path: "/tmp/dir/subdir1/subdir2".to_string(),
                permissions: 0o740,
                ..Default::default()
            }],
            ..Default::default()
        };
        let fs = MemVfs::new();
        let console = RecordingConsole::new();
        run(&stage, &fs, &console).expect("should succeed");

        assert!(fs.exists(Path::new("/tmp")));
        assert!(fs.exists(Path::new("/tmp/dir")));
        assert!(fs.exists(Path::new("/tmp/dir/subdir1")));
        assert!(fs.exists(Path::new("/tmp/dir/subdir1/subdir2")));

        let m = fs
            .metadata(Path::new("/tmp/dir/subdir1/subdir2"))
            .expect("metadata");
        assert_eq!(m.mode, 0o740);
    }

    #[test]
    fn idempotent_on_existing_dir_updates_perm() {
        // Mirrors the Go test: dir already exists, run updates permissions.
        let fs = MemVfs::new();
        fs.mkdir_all(Path::new("/tmp/dir")).unwrap();
        fs.chmod(Path::new("/tmp/dir"), 0o755).unwrap();

        let stage = Stage {
            directories: vec![Directory {
                path: "/tmp/dir".to_string(),
                permissions: 0o740,
                ..Default::default()
            }],
            ..Default::default()
        };
        let console = RecordingConsole::new();
        run(&stage, &fs, &console).expect("should succeed");

        let m = fs.metadata(Path::new("/tmp/dir")).expect("metadata");
        assert!(m.is_dir);
        assert_eq!(m.mode, 0o740);
    }

    #[test]
    fn numeric_owner_respected() {
        let stage = Stage {
            directories: vec![Directory {
                path: "/data".to_string(),
                permissions: 0o750,
                owner: OwnerId::Numeric(1000),
                group: 1000,
                ..Default::default()
            }],
            ..Default::default()
        };
        let fs = MemVfs::new();
        let console = RecordingConsole::new();
        run(&stage, &fs, &console).expect("should succeed");

        let m = fs.metadata(Path::new("/data")).expect("metadata");
        assert_eq!((m.uid, m.gid), (1000, 1000));
    }

    #[test]
    fn name_based_owner_is_skipped_with_warn() {
        let stage = Stage {
            directories: vec![Directory {
                path: "/named".to_string(),
                permissions: 0o755,
                owner: OwnerId::Name("alice".to_string()),
                group: 0,
                ..Default::default()
            }],
            ..Default::default()
        };
        let fs = MemVfs::new();
        let console = RecordingConsole::new();
        run(&stage, &fs, &console).expect("name-based owner skip should not error");

        let m = fs.metadata(Path::new("/named")).expect("metadata");
        assert!(m.is_dir);
        // No chown happened.
        assert_eq!((m.uid, m.gid), (0, 0));
    }

    #[test]
    fn aggregates_errors_from_bad_entries() {
        let stage = Stage {
            directories: vec![
                Directory {
                    path: "/ok".to_string(),
                    permissions: 0o755,
                    ..Default::default()
                },
                Directory {
                    path: "".to_string(), // bad
                    ..Default::default()
                },
                Directory {
                    path: "/also-ok".to_string(),
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
        // The good entries still got created.
        assert!(fs.exists(Path::new("/ok")));
        assert!(fs.exists(Path::new("/also-ok")));
    }
}
