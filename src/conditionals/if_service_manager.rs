//! `IfServiceManager` — regex-match `stage.only_if_service_manager` against
//! the host's detected init system.
//!
//! Detection algorithm (per spec):
//!   1. Read `/proc/1/comm` via the [`Vfs`]. If unreadable, treat the host
//!      as having no detectable service manager and skip the stage.
//!   2. `comm == "systemd"` → `"systemd"`.
//!      `comm == "init"` AND `/sbin/openrc-run` exists → `"openrc"`.
//!      Otherwise → `"unknown"`.
//!   3. Compile `stage.only_if_service_manager` as a regex and match it
//!      against the detected name. A match means [`ConditionalOutcome::Run`];
//!      anything else (no match, invalid regex, unreadable `/proc/1/comm`)
//!      means [`ConditionalOutcome::Skip`].
//!
//! An empty `only_if_service_manager` is a no-op → always `Run`.
//!
//! Note: this differs from Go's `IfServiceManager`, which stat-checks well
//! known `systemctl` / `openrc` binary paths instead of reading
//! `/proc/1/comm`. The Rust port intentionally uses pid-1's `comm` because
//! it is a more reliable signal of the *running* init system (a host can
//! ship both binaries but only one PID 1).

use std::path::Path;
use std::sync::Arc;

use regex::Regex;

use crate::console::Console;
use crate::error::Result;
use crate::executor::{Conditional, ConditionalOutcome};
use crate::schema::Stage;
use crate::vfs::Vfs;

/// Construct the conditional in the form the executor expects.
pub fn build() -> Conditional {
    Arc::new(check)
}

/// Decide whether `stage` should run based on the host's init system.
pub fn check(
    stage: &Stage,
    fs: &dyn Vfs,
    _console: &dyn Console,
) -> Result<ConditionalOutcome> {
    // Empty filter: stage has no opinion, always run.
    if stage.only_if_service_manager.is_empty() {
        return Ok(ConditionalOutcome::Run);
    }

    let detected = detect_service_manager(fs);

    // Bad regex → Skip (mirrors Go's "any conditional error skips the stage"
    // behaviour). We swallow the error here because the user spec is
    // explicit: bad regex should yield Skip, not propagate.
    let re = match Regex::new(&stage.only_if_service_manager) {
        Ok(r) => r,
        Err(_) => return Ok(ConditionalOutcome::Skip),
    };

    if re.is_match(detected) {
        Ok(ConditionalOutcome::Run)
    } else {
        Ok(ConditionalOutcome::Skip)
    }
}

/// Identify the running init system. Returns one of `"systemd"`, `"openrc"`,
/// or `"unknown"`. Never errors — if `/proc/1/comm` cannot be read, we
/// fall through to `"unknown"` so the caller can match it against the
/// user's regex without special-casing.
fn detect_service_manager(fs: &dyn Vfs) -> &'static str {
    let raw = match fs.read(Path::new("/proc/1/comm")) {
        Ok(b) => b,
        Err(_) => return "unknown",
    };
    let comm = String::from_utf8_lossy(&raw);
    let comm = comm.trim();

    match comm {
        "systemd" => "systemd",
        "init" if fs.exists(Path::new("/sbin/openrc-run")) => "openrc",
        _ => "unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::console::RecordingConsole;
    use crate::vfs::MemVfs;

    fn stage_with(sm: &str) -> Stage {
        Stage {
            only_if_service_manager: sm.to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn empty_filter_runs() {
        let fs = MemVfs::new();
        let console = RecordingConsole::default();
        let stage = Stage::default();
        let out = check(&stage, &fs, &console).unwrap();
        assert_eq!(out, ConditionalOutcome::Run);
    }

    #[test]
    fn systemd_matches_systemd() {
        let fs = MemVfs::new();
        fs.write(Path::new("/proc/1/comm"), b"systemd\n").unwrap();
        let console = RecordingConsole::default();
        let stage = stage_with("systemd");
        let out = check(&stage, &fs, &console).unwrap();
        assert_eq!(out, ConditionalOutcome::Run);
    }

    #[test]
    fn systemd_does_not_match_openrc() {
        let fs = MemVfs::new();
        fs.write(Path::new("/proc/1/comm"), b"systemd\n").unwrap();
        let console = RecordingConsole::default();
        let stage = stage_with("openrc");
        let out = check(&stage, &fs, &console).unwrap();
        assert_eq!(out, ConditionalOutcome::Skip);
    }

    #[test]
    fn init_with_openrc_run_matches_openrc() {
        let fs = MemVfs::new();
        fs.write(Path::new("/proc/1/comm"), b"init\n").unwrap();
        fs.write(Path::new("/sbin/openrc-run"), b"#!/bin/sh\n")
            .unwrap();
        let console = RecordingConsole::default();
        let stage = stage_with("openrc");
        let out = check(&stage, &fs, &console).unwrap();
        assert_eq!(out, ConditionalOutcome::Run);
    }

    #[test]
    fn init_without_openrc_run_is_unknown() {
        let fs = MemVfs::new();
        fs.write(Path::new("/proc/1/comm"), b"init\n").unwrap();
        let console = RecordingConsole::default();
        // openrc filter should not match "unknown"
        let stage = stage_with("openrc");
        let out = check(&stage, &fs, &console).unwrap();
        assert_eq!(out, ConditionalOutcome::Skip);
    }

    #[test]
    fn missing_proc_comm_skips() {
        let fs = MemVfs::new();
        let console = RecordingConsole::default();
        let stage = stage_with("systemd");
        let out = check(&stage, &fs, &console).unwrap();
        assert_eq!(out, ConditionalOutcome::Skip);
    }

    #[test]
    fn bad_regex_skips() {
        let fs = MemVfs::new();
        fs.write(Path::new("/proc/1/comm"), b"systemd\n").unwrap();
        let console = RecordingConsole::default();
        // Unbalanced bracket — invalid regex.
        let stage = stage_with("[invalid");
        let out = check(&stage, &fs, &console).unwrap();
        assert_eq!(out, ConditionalOutcome::Skip);
    }

    #[test]
    fn regex_pattern_matches() {
        let fs = MemVfs::new();
        fs.write(Path::new("/proc/1/comm"), b"systemd\n").unwrap();
        let console = RecordingConsole::default();
        // ".*" matches anything.
        let stage = stage_with(".*");
        let out = check(&stage, &fs, &console).unwrap();
        assert_eq!(out, ConditionalOutcome::Run);
    }

    #[test]
    fn build_returns_callable_conditional() {
        let cond = build();
        let fs = MemVfs::new();
        fs.write(Path::new("/proc/1/comm"), b"systemd\n").unwrap();
        let console = RecordingConsole::default();
        let stage = stage_with("systemd");
        let out = cond(&stage, &fs, &console).unwrap();
        assert_eq!(out, ConditionalOutcome::Run);
    }

    // --- Additional tests ported from Go behaviour expectations ---

    #[test]
    fn openrc_and_systemd_both_present_init_pid1_picks_openrc() {
        // The Go test "Fails if it finds both" stat-checks both binaries
        // and reports SkipBothServices. Our detector instead reads PID 1's
        // comm: when it's `init`, openrc-run presence wins. This means
        // both-binaries-present + init pid1 -> openrc (not unknown).
        let fs = MemVfs::new();
        fs.write(Path::new("/proc/1/comm"), b"init\n").unwrap();
        fs.write(Path::new("/sbin/openrc-run"), b"#!/bin/sh\n")
            .unwrap();
        // A bogus /sbin/systemctl should not influence the decision.
        fs.write(Path::new("/sbin/systemctl"), b"#!/bin/sh\n")
            .unwrap();
        let console = RecordingConsole::default();
        let stage = stage_with("openrc");
        let out = check(&stage, &fs, &console).unwrap();
        assert_eq!(out, ConditionalOutcome::Run);

        // And the systemd matcher should NOT pick up the host even though
        // /sbin/systemctl exists — because pid1's comm is `init`.
        let stage_sd = stage_with("systemd");
        let out = check(&stage_sd, &fs, &console).unwrap();
        assert_eq!(out, ConditionalOutcome::Skip);
    }

    #[test]
    fn both_binaries_present_with_systemd_pid1_matches_systemd() {
        // pid1 = systemd, both binaries on disk -> systemd matcher wins.
        let fs = MemVfs::new();
        fs.write(Path::new("/proc/1/comm"), b"systemd\n").unwrap();
        fs.write(Path::new("/sbin/openrc-run"), b"#!/bin/sh\n")
            .unwrap();
        fs.write(Path::new("/sbin/systemctl"), b"#!/bin/sh\n")
            .unwrap();
        let console = RecordingConsole::default();
        let stage = stage_with("systemd");
        let out = check(&stage, &fs, &console).unwrap();
        assert_eq!(out, ConditionalOutcome::Run);
    }

    #[test]
    fn upstart_pid1_is_unknown_and_skips_specific_matchers() {
        // PID 1 named `init` without openrc-run on disk yields "unknown" —
        // matches neither systemd nor openrc.
        let fs = MemVfs::new();
        fs.write(Path::new("/proc/1/comm"), b"init\n").unwrap();
        // no /sbin/openrc-run -> "unknown"
        let console = RecordingConsole::default();
        // Specific matchers should Skip.
        assert_eq!(
            check(&stage_with("systemd"), &fs, &console).unwrap(),
            ConditionalOutcome::Skip
        );
        assert_eq!(
            check(&stage_with("openrc"), &fs, &console).unwrap(),
            ConditionalOutcome::Skip
        );
        // But the "unknown" matcher itself runs.
        assert_eq!(
            check(&stage_with("unknown"), &fs, &console).unwrap(),
            ConditionalOutcome::Run
        );
    }

    #[test]
    fn runit_pid1_is_treated_as_unknown() {
        // Any comm that isn't systemd or init -> unknown.
        let fs = MemVfs::new();
        fs.write(Path::new("/proc/1/comm"), b"runit\n").unwrap();
        let console = RecordingConsole::default();
        assert_eq!(
            check(&stage_with("runit"), &fs, &console).unwrap(),
            ConditionalOutcome::Skip
        );
        assert_eq!(
            check(&stage_with("unknown"), &fs, &console).unwrap(),
            ConditionalOutcome::Run
        );
    }

    #[test]
    fn weird_regex_against_systemd_pid1_skips() {
        // Direct port of Go's "Fails if not supported".
        let fs = MemVfs::new();
        fs.write(Path::new("/proc/1/comm"), b"systemd\n").unwrap();
        let console = RecordingConsole::default();
        let stage = stage_with("weird");
        assert_eq!(
            check(&stage, &fs, &console).unwrap(),
            ConditionalOutcome::Skip
        );
    }

    #[test]
    fn comm_with_trailing_whitespace_is_trimmed() {
        // /proc/1/comm includes a trailing newline on Linux. We strip it
        // before matching, so a "systemd\n" file matches the literal regex.
        let fs = MemVfs::new();
        fs.write(Path::new("/proc/1/comm"), b"systemd\n\n  ").unwrap();
        let console = RecordingConsole::default();
        let stage = stage_with("^systemd$");
        assert_eq!(
            check(&stage, &fs, &console).unwrap(),
            ConditionalOutcome::Run
        );
    }

    #[test]
    fn alternation_systemd_or_openrc_matches_either() {
        let fs = MemVfs::new();
        fs.write(Path::new("/proc/1/comm"), b"systemd\n").unwrap();
        let console = RecordingConsole::default();
        let stage = stage_with("systemd|openrc");
        assert_eq!(
            check(&stage, &fs, &console).unwrap(),
            ConditionalOutcome::Run
        );

        // Swap to openrc pid1.
        let fs2 = MemVfs::new();
        fs2.write(Path::new("/proc/1/comm"), b"init\n").unwrap();
        fs2.write(Path::new("/sbin/openrc-run"), b"#!/bin/sh\n")
            .unwrap();
        assert_eq!(
            check(&stage, &fs2, &console).unwrap(),
            ConditionalOutcome::Run
        );
    }
}
