//! `OnlyIfOSVersion` — regex match `stage.only_if_os_version` against the
//! host's `VERSION_ID` (from `/etc/os-release`).
//!
//! Mirrors Go's `OnlyIfOSVersion` in `pkg/plugins/if_os.go`. Behaviour:
//!
//! - Empty `only_if_os_version` -> [`ConditionalOutcome::Run`] (no gate).
//! - Compile the field as a Rust regex; if it fails to compile, log and
//!   return [`ConditionalOutcome::Skip`] (Go returns an error which the
//!   executor converts to "skip this stage").
//! - Read `VERSION_ID` from `/etc/os-release` via the injected [`Vfs`] so
//!   tests can inject the file with [`crate::vfs::MemVfs`]. Empty / missing
//!   file -> Skip.
//! - `regex.MatchString` is unanchored — Rust's `Regex::is_match` matches
//!   the same way (substring search), so `"20.*"` against `"22.04"` is a
//!   non-match and `"^22\\..*"` is a match, matching the Go behaviour.

use std::path::Path;
use std::sync::Arc;

use regex::Regex;
use tracing::debug;

use crate::console::Console;
use crate::error::Result;
use crate::executor::{Conditional, ConditionalOutcome};
use crate::schema::Stage;
use crate::vfs::Vfs;

/// Build the conditional closure registered by the executor.
pub fn build() -> Conditional {
    Arc::new(check)
}

/// Evaluate the conditional. See module docs for the rules.
pub fn check(stage: &Stage, fs: &dyn Vfs, _console: &dyn Console) -> Result<ConditionalOutcome> {
    if stage.only_if_os_version.is_empty() {
        return Ok(ConditionalOutcome::Run);
    }

    let pattern = &stage.only_if_os_version;
    let re = match Regex::new(pattern) {
        Ok(r) => r,
        Err(e) => {
            debug!(
                "OnlyIfOsVersion regex ({}) compile error: {}",
                pattern, e
            );
            return Ok(ConditionalOutcome::Skip);
        }
    };

    let version = match read_version_id(fs) {
        Some(v) if !v.is_empty() => v,
        _ => {
            debug!(
                "OnlyIfOsVersion regex ({}) skip: system version is empty",
                pattern
            );
            return Ok(ConditionalOutcome::Skip);
        }
    };

    if re.is_match(&version) {
        debug!(
            "running stage (OnlyIfOsVersion regex ({}) matches os version '{}'",
            pattern, version
        );
        Ok(ConditionalOutcome::Run)
    } else {
        debug!(
            "OnlyIfOsVersion regex ({}) doesn't match os version {}",
            pattern, version
        );
        Ok(ConditionalOutcome::Skip)
    }
}

/// Pull `VERSION_ID` out of `/etc/os-release`, parsed via the injected
/// [`Vfs`]. Returns `None` when the file is missing or unreadable so the
/// caller can treat it the same as "version is empty".
fn read_version_id(fs: &dyn Vfs) -> Option<String> {
    let body = fs.read_to_string(Path::new("/etc/os-release")).ok()?;
    for raw in body.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        if k.trim() == "VERSION_ID" {
            return Some(unquote(v.trim()));
        }
    }
    None
}

fn unquote(v: &str) -> String {
    let v = v.trim();
    if v.len() >= 2
        && ((v.starts_with('"') && v.ends_with('"'))
            || (v.starts_with('\'') && v.ends_with('\'')))
    {
        return v[1..v.len() - 1].to_string();
    }
    v.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::console::RecordingConsole;
    use crate::vfs::MemVfs;

    fn run(stage: &Stage, fs: &dyn Vfs) -> ConditionalOutcome {
        let c = RecordingConsole::new();
        check(stage, fs, &c).expect("conditional never returns Err")
    }

    fn fs_with_version(version_id: &str) -> MemVfs {
        let fs = MemVfs::new();
        let body = format!("NAME=\"Ubuntu\"\nVERSION_ID=\"{version_id}\"\n");
        fs.write(Path::new("/etc/os-release"), body.as_bytes())
            .expect("write os-release");
        fs
    }

    #[test]
    fn empty_only_if_os_version_runs() {
        let stage = Stage::default();
        let fs = MemVfs::new();
        assert_eq!(run(&stage, &fs), ConditionalOutcome::Run);
    }

    #[test]
    fn exact_match_runs() {
        let stage = Stage {
            only_if_os_version: "22.04".into(),
            ..Default::default()
        };
        let fs = fs_with_version("22.04");
        assert_eq!(run(&stage, &fs), ConditionalOutcome::Run);
    }

    #[test]
    fn non_matching_regex_skips() {
        let stage = Stage {
            only_if_os_version: "20.*".into(),
            ..Default::default()
        };
        let fs = fs_with_version("22.04");
        assert_eq!(run(&stage, &fs), ConditionalOutcome::Skip);
    }

    #[test]
    fn anchored_regex_matches() {
        let stage = Stage {
            only_if_os_version: r"^22\..*".into(),
            ..Default::default()
        };
        let fs = fs_with_version("22.04");
        assert_eq!(run(&stage, &fs), ConditionalOutcome::Run);
    }

    #[test]
    fn missing_os_release_skips() {
        let stage = Stage {
            only_if_os_version: "22.04".into(),
            ..Default::default()
        };
        let fs = MemVfs::new();
        assert_eq!(run(&stage, &fs), ConditionalOutcome::Skip);
    }

    #[test]
    fn missing_version_id_key_skips() {
        let stage = Stage {
            only_if_os_version: "22.04".into(),
            ..Default::default()
        };
        let fs = MemVfs::new();
        fs.write(Path::new("/etc/os-release"), b"NAME=\"Ubuntu\"\n")
            .expect("write");
        assert_eq!(run(&stage, &fs), ConditionalOutcome::Skip);
    }

    #[test]
    fn empty_version_id_skips() {
        let stage = Stage {
            only_if_os_version: "22.04".into(),
            ..Default::default()
        };
        let fs = fs_with_version("");
        assert_eq!(run(&stage, &fs), ConditionalOutcome::Skip);
    }

    #[test]
    fn bad_regex_skips() {
        let stage = Stage {
            only_if_os_version: "[".into(),
            ..Default::default()
        };
        let fs = fs_with_version("22.04");
        assert_eq!(run(&stage, &fs), ConditionalOutcome::Skip);
    }

    #[test]
    fn handles_unquoted_version_id() {
        let stage = Stage {
            only_if_os_version: "22.04".into(),
            ..Default::default()
        };
        let fs = MemVfs::new();
        // No quotes around the value.
        fs.write(Path::new("/etc/os-release"), b"VERSION_ID=22.04\n")
            .expect("write");
        assert_eq!(run(&stage, &fs), ConditionalOutcome::Run);
    }

    #[test]
    fn ignores_comments_and_blanks() {
        let stage = Stage {
            only_if_os_version: "22.04".into(),
            ..Default::default()
        };
        let fs = MemVfs::new();
        let body = "# header\n\nNAME=\"Ubuntu\"\nVERSION_ID=\"22.04\"\n";
        fs.write(Path::new("/etc/os-release"), body.as_bytes())
            .expect("write");
        assert_eq!(run(&stage, &fs), ConditionalOutcome::Run);
    }
}
