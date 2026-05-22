//! `IfArch` — port of `pkg/plugins/if_arch.go`.
//!
//! Skips a stage when `stage.only_arch` is set and does not regex-match the
//! current architecture. Mirrors Go's
//! `regexp.MatchString(stage.OnlyIfArch, runtime.GOARCH)` semantics: partial
//! / pattern matches succeed, and an empty `only_arch` field is treated as
//! "no filter, always run".
//!
//! Notable difference from Go: `runtime.GOARCH` uses Go names ("amd64",
//! "arm64", ...) while Rust's `std::env::consts::ARCH` uses target-triple
//! names ("x86_64", "aarch64", ...). User configs targeting yip-rs need to
//! match against the Rust set.

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
    // Empty filter = always run. Matches Go's `s.OnlyIfArch != ""` guard.
    if stage.only_if_arch.is_empty() {
        return Ok(ConditionalOutcome::Run);
    }

    // Go's `regexp.Compile` failure bubbles an error out of `IfArch`, which
    // the executor then treats as "skip this stage" (see `applyStage`).
    // We collapse that to `Skip` directly so conditional errors aren't
    // confused with real plugin errors.
    let re = match regex::Regex::new(&stage.only_if_arch) {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(
                only_arch = %stage.only_if_arch,
                error = %e,
                "invalid regex for only_arch, skipping stage",
            );
            return Ok(ConditionalOutcome::Skip);
        }
    };

    let arch = std::env::consts::ARCH;
    if re.is_match(arch) {
        Ok(ConditionalOutcome::Run)
    } else {
        tracing::debug!(
            arch = %arch,
            only_arch = %stage.only_if_arch,
            "arch does not match only_arch, skipping stage",
        );
        Ok(ConditionalOutcome::Skip)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::console::RecordingConsole;
    use crate::vfs::RealVfs;

    #[test]
    fn empty_only_arch_runs() {
        let stage = Stage::default();
        let fs = RealVfs::new();
        let console = RecordingConsole::new();
        let out = check(&stage, &fs, &console).expect("check ok");
        assert_eq!(out, ConditionalOutcome::Run);
    }

    #[test]
    fn matching_arch_runs() {
        // Use the actual runtime ARCH so the test works on every CI host
        // (x86_64, aarch64, riscv64, ...).
        let stage = Stage {
            only_if_arch: std::env::consts::ARCH.to_string(),
            ..Default::default()
        };
        let fs = RealVfs::new();
        let console = RecordingConsole::new();
        let out = check(&stage, &fs, &console).expect("check ok");
        assert_eq!(out, ConditionalOutcome::Run);
    }

    #[test]
    fn nonsense_arch_skips() {
        let stage = Stage {
            only_if_arch: "no_such_arch_xyz".to_string(),
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
            only_if_arch: ".*".to_string(),
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
            only_if_arch: "[(invalid".to_string(),
            ..Default::default()
        };
        let fs = RealVfs::new();
        let console = RecordingConsole::new();
        let out = check(&stage, &fs, &console).expect("check ok");
        assert_eq!(out, ConditionalOutcome::Skip);
    }
}
