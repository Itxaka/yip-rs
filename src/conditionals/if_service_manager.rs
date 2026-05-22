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
}
