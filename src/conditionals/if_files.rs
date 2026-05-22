//! `IfFiles` — file/dir existence checks (any/all/none).
//!
//! Port of `pkg/plugins/if_files_dirs.go::IfFiles`. The stage's `if_files`
//! field is a map `IfCheckType -> Vec<String>` (mirrors Go's
//! `map[IfCheckType][]string`). For each check kind:
//!
//!   - `Any`:  at least one listed path exists -> Run; none exist -> Skip.
//!   - `All`:  every listed path exists       -> Run; any missing -> Skip.
//!   - `None`: no listed path exists          -> Run; any present -> Skip.
//!
//! An empty `if_files` map (or an empty path list for a given check type)
//! imposes no constraint and yields Run. Every check kind present in the
//! map must independently pass; the first failing kind short-circuits to
//! Skip — matching the Go behaviour where any failing check causes the
//! plugin to return an error and the executor to skip the stage.

use std::path::Path;
use std::sync::Arc;

use crate::console::Console;
use crate::error::Result;
use crate::executor::{Conditional, ConditionalOutcome};
use crate::schema::if_files::IfCheckType;
use crate::schema::Stage;
use crate::vfs::Vfs;

/// Build the conditional. Closure pattern matches other conditionals.
pub fn build() -> Conditional {
    Arc::new(check)
}

/// Pure function form — also exposed so tests don't need to invoke via Arc.
pub fn check(stage: &Stage, fs: &dyn Vfs, _console: &dyn Console) -> Result<ConditionalOutcome> {
    // Empty map -> no constraint. Matches Go's `len(s.IfFiles) > 0` guard.
    if stage.if_files.is_empty() {
        return Ok(ConditionalOutcome::Run);
    }

    for (check_type, paths) in stage.if_files.iter() {
        // Empty path list for any check kind -> trivially satisfied (matches
        // the Go code which returns nil when `len(files) == 0`).
        if paths.is_empty() {
            continue;
        }

        match check_type {
            IfCheckType::All => {
                for p in paths {
                    if !fs.exists(Path::new(p)) {
                        tracing::debug!(
                            path = %p,
                            "if_files[all]: required path missing, skipping stage",
                        );
                        return Ok(ConditionalOutcome::Skip);
                    }
                }
            }
            IfCheckType::Any => {
                let found = paths.iter().any(|p| fs.exists(Path::new(p)));
                if !found {
                    tracing::debug!(
                        "if_files[any]: none of the listed paths exist, skipping stage",
                    );
                    return Ok(ConditionalOutcome::Skip);
                }
            }
            IfCheckType::None => {
                for p in paths {
                    if fs.exists(Path::new(p)) {
                        tracing::debug!(
                            path = %p,
                            "if_files[none]: forbidden path exists, skipping stage",
                        );
                        return Ok(ConditionalOutcome::Skip);
                    }
                }
            }
        }
    }

    Ok(ConditionalOutcome::Run)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::console::RecordingConsole;
    use crate::schema::if_files::IfFiles;
    use crate::vfs::MemVfs;

    fn stage_with(if_files: IfFiles) -> Stage {
        Stage {
            if_files,
            ..Default::default()
        }
    }

    #[test]
    fn empty_if_files_runs() {
        let stage = Stage::default();
        let fs = MemVfs::new();
        let console = RecordingConsole::new();
        let out = check(&stage, &fs, &console).expect("check ok");
        assert_eq!(out, ConditionalOutcome::Run);
    }

    #[test]
    fn any_with_one_present_runs() {
        let fs = MemVfs::new();
        fs.write(Path::new("/exists"), b"x").unwrap();

        let mut m: IfFiles = IfFiles::new();
        m.insert(
            IfCheckType::Any,
            vec!["/exists".to_string(), "/missing".to_string()],
        );
        let stage = stage_with(m);

        let console = RecordingConsole::new();
        let out = check(&stage, &fs, &console).expect("check ok");
        assert_eq!(out, ConditionalOutcome::Run);
    }

    #[test]
    fn any_with_none_present_skips() {
        let fs = MemVfs::new();

        let mut m: IfFiles = IfFiles::new();
        m.insert(
            IfCheckType::Any,
            vec!["/missing1".to_string(), "/missing2".to_string()],
        );
        let stage = stage_with(m);

        let console = RecordingConsole::new();
        let out = check(&stage, &fs, &console).expect("check ok");
        assert_eq!(out, ConditionalOutcome::Skip);
    }

    #[test]
    fn all_with_both_present_runs() {
        let fs = MemVfs::new();
        fs.write(Path::new("/a"), b"a").unwrap();
        fs.write(Path::new("/b"), b"b").unwrap();

        let mut m: IfFiles = IfFiles::new();
        m.insert(
            IfCheckType::All,
            vec!["/a".to_string(), "/b".to_string()],
        );
        let stage = stage_with(m);

        let console = RecordingConsole::new();
        let out = check(&stage, &fs, &console).expect("check ok");
        assert_eq!(out, ConditionalOutcome::Run);
    }

    #[test]
    fn all_with_one_missing_skips() {
        let fs = MemVfs::new();
        fs.write(Path::new("/a"), b"a").unwrap();
        // /b absent

        let mut m: IfFiles = IfFiles::new();
        m.insert(
            IfCheckType::All,
            vec!["/a".to_string(), "/b".to_string()],
        );
        let stage = stage_with(m);

        let console = RecordingConsole::new();
        let out = check(&stage, &fs, &console).expect("check ok");
        assert_eq!(out, ConditionalOutcome::Skip);
    }

    #[test]
    fn none_with_none_present_runs() {
        let fs = MemVfs::new();
        // nothing on disk

        let mut m: IfFiles = IfFiles::new();
        m.insert(IfCheckType::None, vec!["/missing".to_string()]);
        let stage = stage_with(m);

        let console = RecordingConsole::new();
        let out = check(&stage, &fs, &console).expect("check ok");
        assert_eq!(out, ConditionalOutcome::Run);
    }

    #[test]
    fn none_with_one_present_skips() {
        let fs = MemVfs::new();
        fs.write(Path::new("/exists"), b"x").unwrap();

        let mut m: IfFiles = IfFiles::new();
        m.insert(IfCheckType::None, vec!["/exists".to_string()]);
        let stage = stage_with(m);

        let console = RecordingConsole::new();
        let out = check(&stage, &fs, &console).expect("check ok");
        assert_eq!(out, ConditionalOutcome::Skip);
    }

    #[test]
    fn combined_all_and_none_pass_runs() {
        let fs = MemVfs::new();
        fs.write(Path::new("/a"), b"a").unwrap();
        // /b absent — None constraint satisfied.

        let mut m: IfFiles = IfFiles::new();
        m.insert(IfCheckType::All, vec!["/a".to_string()]);
        m.insert(IfCheckType::None, vec!["/b".to_string()]);
        let stage = stage_with(m);

        let console = RecordingConsole::new();
        let out = check(&stage, &fs, &console).expect("check ok");
        assert_eq!(out, ConditionalOutcome::Run);
    }

    #[test]
    fn combined_none_fails_skips() {
        let fs = MemVfs::new();
        fs.write(Path::new("/a"), b"a").unwrap();
        fs.write(Path::new("/b"), b"b").unwrap(); // forbidden -> None fails

        let mut m: IfFiles = IfFiles::new();
        m.insert(IfCheckType::All, vec!["/a".to_string()]);
        m.insert(IfCheckType::None, vec!["/b".to_string()]);
        let stage = stage_with(m);

        let console = RecordingConsole::new();
        let out = check(&stage, &fs, &console).expect("check ok");
        assert_eq!(out, ConditionalOutcome::Skip);
    }

    #[test]
    fn empty_path_list_for_check_kind_is_no_op() {
        // A check kind present with an empty list imposes no constraint —
        // mirrors Go's `len(files) == 0 { return nil }` early-out.
        let fs = MemVfs::new();

        let mut m: IfFiles = IfFiles::new();
        m.insert(IfCheckType::Any, vec![]);
        m.insert(IfCheckType::All, vec![]);
        m.insert(IfCheckType::None, vec![]);
        let stage = stage_with(m);

        let console = RecordingConsole::new();
        let out = check(&stage, &fs, &console).expect("check ok");
        assert_eq!(out, ConditionalOutcome::Run);
    }

    #[test]
    fn dir_existence_counts_for_any() {
        // The Go plugin stats both files and dirs equivalently. Verify that
        // directories satisfy `Any` just like regular files do.
        let fs = MemVfs::new();
        fs.mkdir_all(Path::new("/etc/some-dir")).unwrap();

        let mut m: IfFiles = IfFiles::new();
        m.insert(IfCheckType::Any, vec!["/etc/some-dir".to_string()]);
        let stage = stage_with(m);

        let console = RecordingConsole::new();
        let out = check(&stage, &fs, &console).expect("check ok");
        assert_eq!(out, ConditionalOutcome::Run);
    }

    // --- Additional tests ported from Go behaviour expectations ---

    #[test]
    fn mixed_any_all_none_all_passing_runs() {
        // Nested mix of all three check types, every condition satisfied.
        let fs = MemVfs::new();
        fs.write(Path::new("/etc/hostname"), b"x").unwrap();
        fs.write(Path::new("/etc/hosts"), b"x").unwrap();
        // /etc/some-other present for `any`.
        fs.write(Path::new("/etc/some-other"), b"x").unwrap();
        // /etc/banned absent for `none`.

        let mut m: IfFiles = IfFiles::new();
        m.insert(
            IfCheckType::All,
            vec!["/etc/hostname".into(), "/etc/hosts".into()],
        );
        m.insert(
            IfCheckType::Any,
            vec!["/etc/some-other".into(), "/etc/nowhere".into()],
        );
        m.insert(IfCheckType::None, vec!["/etc/banned".into()]);
        let stage = stage_with(m);

        let console = RecordingConsole::new();
        let out = check(&stage, &fs, &console).expect("check ok");
        assert_eq!(out, ConditionalOutcome::Run);
    }

    #[test]
    fn mixed_any_all_none_one_failing_skips() {
        // All & Any satisfied, but None constraint fails because a
        // "forbidden" file exists.
        let fs = MemVfs::new();
        fs.write(Path::new("/etc/hostname"), b"x").unwrap();
        fs.write(Path::new("/etc/hosts"), b"x").unwrap();
        fs.write(Path::new("/etc/some-other"), b"x").unwrap();
        fs.write(Path::new("/etc/banned"), b"x").unwrap();

        let mut m: IfFiles = IfFiles::new();
        m.insert(
            IfCheckType::All,
            vec!["/etc/hostname".into(), "/etc/hosts".into()],
        );
        m.insert(
            IfCheckType::Any,
            vec!["/etc/some-other".into(), "/etc/nowhere".into()],
        );
        m.insert(IfCheckType::None, vec!["/etc/banned".into()]);
        let stage = stage_with(m);

        let console = RecordingConsole::new();
        let out = check(&stage, &fs, &console).expect("check ok");
        assert_eq!(out, ConditionalOutcome::Skip);
    }

    #[test]
    fn trailing_slash_on_dir_path_is_handled() {
        // Path written without trailing slash; the user supplies one.
        let fs = MemVfs::new();
        fs.mkdir_all(Path::new("/etc/some-dir")).unwrap();

        let mut m: IfFiles = IfFiles::new();
        m.insert(IfCheckType::All, vec!["/etc/some-dir/".to_string()]);
        let stage = stage_with(m);

        let console = RecordingConsole::new();
        // MemVfs.exists with a trailing slash on a dir-existing path
        // should still report true (Path::new("/etc/some-dir/") canonicalises
        // to the same key). If this assertion fails the implementation may
        // need a path-normalisation tweak — captured here as a known case.
        let out = check(&stage, &fs, &console).expect("check ok");
        assert_eq!(out, ConditionalOutcome::Run);
    }

    #[test]
    fn all_three_present_with_one_of_three_any_missing_runs() {
        // Any: one of three paths present is enough.
        let fs = MemVfs::new();
        fs.write(Path::new("/etc/c"), b"x").unwrap();

        let mut m: IfFiles = IfFiles::new();
        m.insert(
            IfCheckType::Any,
            vec![
                "/etc/a".into(),
                "/etc/b".into(),
                "/etc/c".into(),
            ],
        );
        let stage = stage_with(m);

        let console = RecordingConsole::new();
        let out = check(&stage, &fs, &console).expect("check ok");
        assert_eq!(out, ConditionalOutcome::Run);
    }

    #[test]
    fn all_kind_one_of_three_missing_skips() {
        // All: any missing path causes skip; even if 2 of 3 exist.
        let fs = MemVfs::new();
        fs.write(Path::new("/etc/a"), b"x").unwrap();
        fs.write(Path::new("/etc/b"), b"x").unwrap();
        // /etc/c absent

        let mut m: IfFiles = IfFiles::new();
        m.insert(
            IfCheckType::All,
            vec![
                "/etc/a".into(),
                "/etc/b".into(),
                "/etc/c".into(),
            ],
        );
        let stage = stage_with(m);

        let console = RecordingConsole::new();
        let out = check(&stage, &fs, &console).expect("check ok");
        assert_eq!(out, ConditionalOutcome::Skip);
    }

    #[test]
    fn none_with_three_paths_one_present_skips() {
        // None: all listed paths must be absent. A single present file
        // is enough to skip.
        let fs = MemVfs::new();
        fs.write(Path::new("/etc/c"), b"x").unwrap();

        let mut m: IfFiles = IfFiles::new();
        m.insert(
            IfCheckType::None,
            vec![
                "/etc/a".into(),
                "/etc/b".into(),
                "/etc/c".into(),
            ],
        );
        let stage = stage_with(m);

        let console = RecordingConsole::new();
        let out = check(&stage, &fs, &console).expect("check ok");
        assert_eq!(out, ConditionalOutcome::Skip);
    }
}
