//! `directories` plugin — ensure each entry in `stage.directories` exists
//! with the requested permissions and ownership.
//!
//! Port of `pkg/plugins/dir.go::EnsureDirectories`. The Go version walks
//! up the path tree manually, creating missing parents one at a time and
//! applying the user-supplied permissions / ownership to every component
//! it creates. We replicate that behaviour here: walk the path's
//! ancestor chain from the root, `mkdir` each segment, then apply
//! `chmod` + `chown` (when set) at every level. This matches Go's
//! observable side-effect where `/a/b/c/d` with mode `0o755` ends up
//! with `0o755` on `/a`, `/a/b`, `/a/b/c`, and `/a/b/c/d`.
//!
//! Name-based owner/group is currently not supported: we log a warning and
//! skip the chown. Adding `users`-crate lookups is tracked as a TODO.
//!
//! Per-directory errors are aggregated into [`Error::Multi`]; the loop
//! never aborts early.

use std::path::{Component, Path, PathBuf};
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

    let ancestors = iter_ancestors(path);
    let target_idx = ancestors.len().saturating_sub(1);

    // Walk the components. Mirrors Go's `writePath` recursion in
    // `pkg/plugins/dir.go`:
    //
    //   * Top-level target: if it already exists, only chmod+chown it
    //     (does not touch parents); if it doesn't, recurse upward.
    //   * Intermediate parents: chmod+chown only when *newly created*.
    //     Pre-existing intermediates are left alone.
    //
    // We approximate this by checking `fs.exists` before mkdir. If it
    // doesn't exist (or it's the top-level target), we mkdir + chmod +
    // chown. If it already exists and isn't the leaf, leave it alone.
    for (idx, segment) in ancestors.iter().enumerate() {
        let is_leaf = idx == target_idx;
        let pre_existed = fs.exists(segment);
        fs.mkdir_all(segment)?;

        // Apply perms+chown when we just created this dir, OR when it's
        // the top-level target (Go always touches the top-level even if
        // it already exists).
        if !pre_existed || is_leaf {
            if dir.permissions != 0 {
                debug!(path = %segment.display(), mode = format!("{:o}", dir.permissions), "chmod");
                fs.chmod(segment, dir.permissions)?;
            }
            apply_chown(segment, &dir.owner, dir.group, fs)?;
        }
    }
    Ok(())
}

/// Build the ordered list of ancestor paths to mkdir/chmod/chown for a
/// target directory, including the target itself.
///
/// Examples:
///   - `/a/b/c`        -> [`/a`, `/a/b`, `/a/b/c`]
///   - `relative/x`    -> [`relative`, `relative/x`]
///   - `/`             -> [] (root is never created)
///
/// We skip the bare root because Go does too: the loop walks from the
/// shallowest *creatable* directory upward.
fn iter_ancestors(path: &Path) -> Vec<PathBuf> {
    let mut acc: Vec<PathBuf> = Vec::new();
    let mut current = PathBuf::new();
    for comp in path.components() {
        match comp {
            // The leading `/` on an absolute path becomes the prefix for
            // every subsequent segment; we don't emit `/` itself.
            Component::RootDir => current.push("/"),
            Component::Prefix(p) => current.push(p.as_os_str()),
            Component::CurDir => continue,
            // `..` is unusual in a directories entry but Go would walk
            // through it. Push and emit so the resulting path matches the
            // user-supplied string.
            Component::ParentDir => {
                current.push("..");
                acc.push(current.clone());
            }
            Component::Normal(seg) => {
                current.push(seg);
                acc.push(current.clone());
            }
        }
    }
    acc
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

    // -------------------------------------------------------------------
    // Ported from Go: chmod-on-existing, deep nesting, numeric vs string
    // owner, special mode bits.
    // -------------------------------------------------------------------

    #[test]
    fn chmod_on_already_existing_dir_changes_mode() {
        // Pre-create the dir with 0o755 (without our plugin) then re-run with
        // 0o700 — final mode must be the requested one.
        let fs = MemVfs::new();
        fs.mkdir_all(Path::new("/preexisting")).unwrap();
        fs.chmod(Path::new("/preexisting"), 0o755).unwrap();

        let stage = Stage {
            directories: vec![Directory {
                path: "/preexisting".to_string(),
                permissions: 0o700,
                ..Default::default()
            }],
            ..Default::default()
        };
        let console = RecordingConsole::default();
        run(&stage, &fs, &console).expect("ok");
        let m = fs.metadata(Path::new("/preexisting")).expect("metadata");
        assert_eq!(m.mode, 0o700);
    }

    #[test]
    fn deep_nested_path_five_levels() {
        let stage = Stage {
            directories: vec![Directory {
                path: "/a/b/c/d/e".to_string(),
                permissions: 0o755,
                ..Default::default()
            }],
            ..Default::default()
        };
        let fs = MemVfs::new();
        let console = RecordingConsole::default();
        run(&stage, &fs, &console).expect("ok");
        for p in ["/a", "/a/b", "/a/b/c", "/a/b/c/d", "/a/b/c/d/e"] {
            assert!(fs.exists(Path::new(p)), "missing: {p}");
        }
        let m = fs.metadata(Path::new("/a/b/c/d/e")).expect("metadata");
        assert_eq!(m.mode, 0o755);
    }

    #[test]
    fn group_only_chown_applies_with_uid_zero() {
        // OwnerId::Numeric(0) + group=42 -> chown is applied (early-return
        // only triggers when BOTH uid and gid are zero).
        let stage = Stage {
            directories: vec![Directory {
                path: "/grouponly".to_string(),
                permissions: 0o755,
                owner: OwnerId::Numeric(0),
                group: 42,
                ..Default::default()
            }],
            ..Default::default()
        };
        let fs = MemVfs::new();
        let console = RecordingConsole::default();
        run(&stage, &fs, &console).expect("ok");
        let m = fs.metadata(Path::new("/grouponly")).expect("metadata");
        assert_eq!((m.uid, m.gid), (0, 42));
    }

    #[test]
    fn string_owner_warn_path_does_not_chown() {
        // Explicit username string — plugin warns and leaves uid/gid at 0.
        let stage = Stage {
            directories: vec![Directory {
                path: "/strowner".to_string(),
                permissions: 0o755,
                owner: OwnerId::Name("bob".to_string()),
                group: 5,
                ..Default::default()
            }],
            ..Default::default()
        };
        let fs = MemVfs::new();
        let console = RecordingConsole::default();
        run(&stage, &fs, &console).expect("ok");
        let m = fs.metadata(Path::new("/strowner")).expect("metadata");
        // Name-based owner aborts chown entirely (Go matches: warn + skip).
        assert_eq!((m.uid, m.gid), (0, 0));
    }

    #[test]
    fn special_mode_bits_setuid_setgid_sticky_round_trip() {
        // 0o4755 = setuid+0755, 0o2755 = setgid, 0o1755 = sticky.
        for mode in [0o4755u32, 0o2755, 0o1755, 0o7777] {
            let path = format!("/special/{:o}", mode);
            let stage = Stage {
                directories: vec![Directory {
                    path: path.clone(),
                    permissions: mode,
                    ..Default::default()
                }],
                ..Default::default()
            };
            let fs = MemVfs::new();
            let console = RecordingConsole::default();
            run(&stage, &fs, &console).expect("ok");
            let m = fs.metadata(Path::new(&path)).expect("metadata");
            assert_eq!(m.mode, mode, "round-trip for {:o}", mode);
        }
    }

    // -------------------------------------------------------------------
    // Direct ports of the Go `It` blocks in pkg/plugins/dir_test.go.
    // Each test mirrors one Ginkgo `It` from the upstream yip test suite.
    // -------------------------------------------------------------------

    /// Go: "Creates a /tmp/dir directory"
    #[test]
    fn go_port_creates_tmp_dir_directory() {
        let fs = MemVfs::new();
        fs.mkdir_all(Path::new("/tmp")).unwrap();
        fs.chmod(Path::new("/tmp"), 0o755).unwrap();
        let console = RecordingConsole::new();

        // os.Getuid() / os.Getgid() — pick the running user's IDs.
        let uid = unsafe { libc::getuid() } as i32;
        let gid = unsafe { libc::getgid() } as i32;
        let stage = Stage {
            directories: vec![Directory {
                path: "/tmp/dir".to_string(),
                permissions: 0o740,
                owner: OwnerId::Numeric(uid),
                group: gid,
                ..Default::default()
            }],
            ..Default::default()
        };
        run(&stage, &fs, &console).expect("err nil");
        let m = fs.metadata(Path::new("/tmp/dir")).expect("stat");
        assert_eq!(m.mode & 0o7777, 0o740);
    }

    /// Go: "Changes permissions of existing directory /tmp/dir directory"
    #[test]
    fn go_port_changes_permissions_of_existing_dir() {
        let fs = MemVfs::new();
        fs.mkdir_all(Path::new("/tmp/dir")).unwrap();
        fs.chmod(Path::new("/tmp/dir"), 0o755).unwrap();
        let console = RecordingConsole::new();

        let m = fs.metadata(Path::new("/tmp/dir")).expect("pre-stat");
        assert_eq!(m.mode & 0o7777, 0o755);

        let uid = unsafe { libc::getuid() } as i32;
        let gid = unsafe { libc::getgid() } as i32;
        let stage = Stage {
            directories: vec![Directory {
                path: "/tmp/dir".to_string(),
                permissions: 0o740,
                owner: OwnerId::Numeric(uid),
                group: gid,
                ..Default::default()
            }],
            ..Default::default()
        };
        run(&stage, &fs, &console).expect("err nil");
        let m = fs.metadata(Path::new("/tmp/dir")).expect("post-stat");
        assert_eq!(m.mode & 0o7777, 0o740);
    }

    // -------------------------------------------------------------------
    // New tests for the Go-matching per-level chmod/chown behaviour.
    // -------------------------------------------------------------------

    #[test]
    fn nested_path_applies_mode_to_every_intermediate() {
        // Go applies the requested permissions to each intermediate dir
        // created along the path. Our port must do the same: every
        // ancestor of /a/b/c/d (including /a, /a/b, /a/b/c) ends up at
        // 0o750 — not just the leaf.
        let stage = Stage {
            directories: vec![Directory {
                path: "/a/b/c/d".to_string(),
                permissions: 0o750,
                ..Default::default()
            }],
            ..Default::default()
        };
        let fs = MemVfs::new();
        let console = RecordingConsole::default();
        run(&stage, &fs, &console).expect("ok");

        for p in ["/a", "/a/b", "/a/b/c", "/a/b/c/d"] {
            let m = fs.metadata(Path::new(p)).expect("metadata");
            assert!(m.is_dir, "{p} should be a directory");
            assert_eq!(
                m.mode & 0o7777,
                0o750,
                "{p} should have mode 0o750"
            );
        }
    }

    #[test]
    fn nested_path_applies_owner_group_to_every_intermediate() {
        // chown propagation: every intermediate parent gets the same
        // uid/gid as the leaf. Mirrors Go's per-segment os.Chown call.
        let stage = Stage {
            directories: vec![Directory {
                path: "/x/y/z".to_string(),
                permissions: 0o755,
                owner: OwnerId::Numeric(2000),
                group: 3000,
                ..Default::default()
            }],
            ..Default::default()
        };
        let fs = MemVfs::new();
        let console = RecordingConsole::default();
        run(&stage, &fs, &console).expect("ok");

        for p in ["/x", "/x/y", "/x/y/z"] {
            let m = fs.metadata(Path::new(p)).expect("metadata");
            assert_eq!(
                (m.uid, m.gid),
                (2000, 3000),
                "{p} should be owned by uid=2000 gid=3000"
            );
        }
    }

    #[test]
    fn iter_ancestors_returns_components_in_order() {
        let ancestors = iter_ancestors(Path::new("/a/b/c"));
        let strs: Vec<String> = ancestors
            .iter()
            .map(|p| p.display().to_string())
            .collect();
        assert_eq!(strs, vec!["/a", "/a/b", "/a/b/c"]);
    }

    /// Go: "Creates /tmp/dir/subdir1/subdir2 directory and its missing parent dirs"
    #[test]
    fn go_port_creates_nested_with_missing_parents() {
        let fs = MemVfs::new();
        fs.mkdir_all(Path::new("/tmp")).unwrap();
        fs.chmod(Path::new("/tmp"), 0o755).unwrap();
        let console = RecordingConsole::new();

        let uid = unsafe { libc::getuid() } as i32;
        let gid = unsafe { libc::getgid() } as i32;
        let stage = Stage {
            directories: vec![Directory {
                path: "/tmp/dir/subdir1/subdir2".to_string(),
                permissions: 0o740,
                owner: OwnerId::Numeric(uid),
                group: gid,
                ..Default::default()
            }],
            ..Default::default()
        };
        run(&stage, &fs, &console).expect("err nil");

        let m = fs.metadata(Path::new("/tmp")).expect("stat tmp");
        assert_eq!(m.mode & 0o7777, 0o755);
        let m = fs
            .metadata(Path::new("/tmp/dir/subdir1/subdir2"))
            .expect("stat leaf");
        assert_eq!(m.mode & 0o7777, 0o740);
    }
}
