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
}
