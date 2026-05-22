//! `NodeConditional` — port of `pkg/plugins/network.go::NodeConditional`.
//!
//! Skips a stage when `stage.node` is set and does not regex-match the
//! current hostname. Mirrors Go's `regexp.MatchString(stage.Node, hostname)`
//! semantics: partial / pattern matches succeed, and an empty `node` field
//! is treated as "no filter, always run".

use std::sync::Arc;

use crate::console::Console;
use crate::error::Result;
use crate::executor::{Conditional, ConditionalOutcome};
use crate::schema::Stage;
use crate::vfs::Vfs;

/// Build the conditional. Closure pattern matches other conditionals.
pub fn build() -> Conditional {
    Arc::new(check)
}

/// Pure function form — also exposed so tests don't need to invoke via Arc.
pub fn check(stage: &Stage, _fs: &dyn Vfs, _console: &dyn Console) -> Result<ConditionalOutcome> {
    // Empty node = no filter, always run. Matches Go's `len(s.Node) > 0` guard.
    if stage.node.is_empty() {
        return Ok(ConditionalOutcome::Run);
    }

    let hostname = current_hostname();

    // Go uses `regexp.MatchString(stage.Node, hostname)`: a bad pattern
    // logs and skips. Treat regex compile errors the same way.
    let re = match regex::Regex::new(&stage.node) {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(
                node = %stage.node,
                error = %e,
                "invalid regex for node hostname, skipping stage",
            );
            return Ok(ConditionalOutcome::Skip);
        }
    };

    if re.is_match(&hostname) {
        Ok(ConditionalOutcome::Run)
    } else {
        tracing::debug!(
            hostname = %hostname,
            node = %stage.node,
            "node hostname does not match, skipping stage",
        );
        Ok(ConditionalOutcome::Skip)
    }
}

/// Read the current hostname. Prefers `nix::unistd::gethostname()`; falls
/// back to the `HOSTNAME` env var if the syscall fails or returns
/// non-UTF-8. Returns an empty string only when both sources fail —
/// callers then compare against the user-supplied regex, which is the
/// same outcome the Go code reaches when `system.Node.Hostname` is empty.
fn current_hostname() -> String {
    // Env override first so tests can fake the hostname without touching
    // the syscall. The Go side doesn't do this, but its tests just use the
    // real hostname; ours need a hook.
    if let Ok(h) = std::env::var("HOSTNAME") {
        if !h.is_empty() {
            return h;
        }
    }
    // Read via libc directly so we don't need an extra `nix` feature flag.
    let mut buf = [0u8; 256];
    // SAFETY: we pass a buffer we own and its length; gethostname only writes
    // into the first `len` bytes and null-terminates.
    let rc = unsafe { libc::gethostname(buf.as_mut_ptr() as *mut libc::c_char, buf.len()) };
    if rc != 0 {
        return String::new();
    }
    let end = buf.iter().position(|b| *b == 0).unwrap_or(buf.len());
    String::from_utf8_lossy(&buf[..end]).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::console::RecordingConsole;
    use crate::vfs::RealVfs;

    fn hostname_for_test() -> String {
        current_hostname()
    }

    #[test]
    fn empty_node_runs() {
        let stage = Stage::default();
        let fs = RealVfs::new();
        let console = RecordingConsole::new();
        let out = check(&stage, &fs, &console).expect("check ok");
        assert_eq!(out, ConditionalOutcome::Run);
    }

    #[test]
    fn matching_hostname_runs() {
        let stage = Stage {
            node: hostname_for_test(),
            ..Default::default()
        };
        // If the host has no resolvable hostname, the regex would be empty
        // — but empty `node` is short-circuited above, so this still
        // exercises the match-on-equal path for any non-empty hostname.
        if stage.node.is_empty() {
            // Pathological CI without a hostname: skip the assertion that
            // requires one. Treat this as a trivially-passing test rather
            // than a false failure.
            return;
        }
        let fs = RealVfs::new();
        let console = RecordingConsole::new();
        let out = check(&stage, &fs, &console).expect("check ok");
        assert_eq!(out, ConditionalOutcome::Run);
    }

    #[test]
    fn nonsense_node_skips() {
        let stage = Stage {
            node: "this-is-not-a-real-hostname-zzzzz-1234567890".to_string(),
            ..Default::default()
        };
        let fs = RealVfs::new();
        let console = RecordingConsole::new();
        let out = check(&stage, &fs, &console).expect("check ok");
        assert_eq!(out, ConditionalOutcome::Skip);
    }

    #[test]
    fn wildcard_regex_runs() {
        let stage = Stage {
            node: "^.*$".to_string(),
            ..Default::default()
        };
        let fs = RealVfs::new();
        let console = RecordingConsole::new();
        let out = check(&stage, &fs, &console).expect("check ok");
        assert_eq!(out, ConditionalOutcome::Run);
    }

    #[test]
    fn bad_regex_skips_without_panic() {
        let stage = Stage {
            node: "[(invalid".to_string(),
            ..Default::default()
        };
        let fs = RealVfs::new();
        let console = RecordingConsole::new();
        let out = check(&stage, &fs, &console).expect("check ok");
        assert_eq!(out, ConditionalOutcome::Skip);
    }
}
