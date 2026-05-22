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

    // --- Additional tests ported from Go behaviour expectations ---

    #[test]
    fn pre_release_version_matches_with_prefix_regex() {
        // VERSION_ID values like "22.04-rc" are valid and should match
        // a prefix-style anchor.
        let stage = Stage {
            only_if_os_version: r"^22\.04".into(),
            ..Default::default()
        };
        let fs = fs_with_version("22.04-rc");
        assert_eq!(run(&stage, &fs), ConditionalOutcome::Run);
    }

    #[test]
    fn pre_release_version_skipped_by_exact_anchor() {
        // An anchored exact regex must NOT match a pre-release suffix.
        let stage = Stage {
            only_if_os_version: r"^22\.04$".into(),
            ..Default::default()
        };
        let fs = fs_with_version("22.04-rc");
        assert_eq!(run(&stage, &fs), ConditionalOutcome::Skip);
    }

    #[test]
    fn version_field_is_not_used_only_version_id() {
        // Per the Rust port spec, we read VERSION_ID — not VERSION.
        // A file that only has VERSION should therefore Skip.
        let stage = Stage {
            only_if_os_version: "22.04".into(),
            ..Default::default()
        };
        let fs = MemVfs::new();
        fs.write(
            Path::new("/etc/os-release"),
            b"NAME=\"Ubuntu\"\nVERSION=\"22.04 LTS\"\n",
        )
        .expect("write");
        assert_eq!(run(&stage, &fs), ConditionalOutcome::Skip);
    }

    #[test]
    fn cpe_name_field_is_ignored() {
        // CPE_NAME contains a version string in its CPE URI but we explicitly
        // only inspect VERSION_ID, not the CPE field.
        let stage = Stage {
            only_if_os_version: "22.04".into(),
            ..Default::default()
        };
        let fs = MemVfs::new();
        fs.write(
            Path::new("/etc/os-release"),
            b"NAME=\"Ubuntu\"\nCPE_NAME=\"cpe:/o:canonical:ubuntu_linux:22.04\"\n",
        )
        .expect("write");
        // No VERSION_ID -> Skip.
        assert_eq!(run(&stage, &fs), ConditionalOutcome::Skip);
    }

    #[test]
    fn weird_pattern_skips_on_real_versions() {
        // Direct port of the Go test which configures
        // `only_if_os_version: "weird"` — must Skip on a normal version.
        let stage = Stage {
            only_if_os_version: "weird".into(),
            ..Default::default()
        };
        let fs = fs_with_version("22.04");
        assert_eq!(run(&stage, &fs), ConditionalOutcome::Skip);
    }

    #[test]
    fn version_id_with_pre_release_underscore() {
        // Some distros (rolling) emit suffixes like 22.04_rc1.
        let stage = Stage {
            only_if_os_version: r"^22\.".into(),
            ..Default::default()
        };
        let fs = fs_with_version("22.04_rc1");
        assert_eq!(run(&stage, &fs), ConditionalOutcome::Run);
    }

    // --- Direct ports of Go's `if_test.go` `IfOsVersionConditional` Describe ---
    // Source: yip/pkg/plugins/if_test.go, Describe("IfOsVersionConditional"),
    // 1 It block.
    //
    // Divergence note: Go asserts on the error message containing
    // `SkipOnlyOsVersion` formatted with "weird". The Rust port returns
    // `ConditionalOutcome::Skip` instead of propagating the error string,
    // so we assert on the outcome.

    /// Go: `Describe("IfOsVersionConditional") It("Executes")` —
    /// `OnlyIfOsVersion: "weird"` must Skip. Go's BeforeEach does not seed
    /// `/etc/os-release` so VERSION_ID is empty on the host's real fs; here
    /// we use `MemVfs` with no os-release file to match that deterministically.
    #[test]
    fn go_port_if_os_version_conditional_executes_weird_pattern_skips() {
        let fs = MemVfs::new();
        let console = RecordingConsole::default();
        let stage = Stage {
            only_if_os_version: "weird".into(),
            ..Default::default()
        };
        let out = check(&stage, &fs, &console).expect("check ok");
        assert_eq!(out, ConditionalOutcome::Skip);
        assert!(console.commands().is_empty());
    }
}
