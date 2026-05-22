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

    // --- Additional tests ported from Go behaviour expectations ---

    /// Drive the matcher against every supported architecture string in a
    /// parameterised loop. We don't have a way to override `env::consts::ARCH`
    /// at runtime, so we exercise the regex / matcher side by:
    ///   - building patterns that DO match the runtime arch (Run expected)
    ///   - and patterns that don't (Skip expected)
    /// for each candidate.
    #[test]
    fn parameterised_arches_runtime_arch_runs_others_skip() {
        let runtime = std::env::consts::ARCH;
        // Each entry is the user-supplied regex.
        let cases = [
            "x86_64",
            "aarch64",
            "riscv64",
            "arm",
            // Patterns intentionally aliasing on the runtime — to catch
            // typos in the implementation.
            "x86.*",
            "aarch.*",
            "riscv.*",
            "^arm$",
        ];
        for pat in cases {
            let fs = RealVfs::new();
            let console = RecordingConsole::new();
            let stage = Stage {
                only_if_arch: pat.to_string(),
                ..Default::default()
            };
            let out = check(&stage, &fs, &console).expect("check ok");
            // The pattern matches iff a freshly-compiled regex says so.
            let want = match regex::Regex::new(pat) {
                Ok(re) if re.is_match(runtime) => ConditionalOutcome::Run,
                _ => ConditionalOutcome::Skip,
            };
            assert_eq!(out, want, "pattern {pat:?} against arch {runtime}");
        }
    }

    #[test]
    fn unrelated_arch_pattern_skips() {
        // Direct port of Go's `IfArch` "Fails with no match" — only_arch
        // = "weird" must Skip on any real host.
        let stage = Stage {
            only_if_arch: "weird".to_string(),
            ..Default::default()
        };
        let fs = RealVfs::new();
        let console = RecordingConsole::new();
        let out = check(&stage, &fs, &console).expect("check ok");
        assert_eq!(out, ConditionalOutcome::Skip);
    }

    #[test]
    fn alternation_with_runtime_arch_matches() {
        // A pattern listing several arches should match if the runtime is
        // one of them. Always include the runtime arch so the alternation
        // matches on every CI host.
        let runtime = std::env::consts::ARCH;
        let pattern = format!("({}|definitely-not-an-arch)", runtime);
        let stage = Stage {
            only_if_arch: pattern,
            ..Default::default()
        };
        let fs = RealVfs::new();
        let console = RecordingConsole::new();
        let out = check(&stage, &fs, &console).expect("check ok");
        assert_eq!(out, ConditionalOutcome::Run);
    }

    #[test]
    fn anchored_runtime_arch_matches() {
        let runtime = std::env::consts::ARCH;
        let stage = Stage {
            only_if_arch: format!("^{}$", runtime),
            ..Default::default()
        };
        let fs = RealVfs::new();
        let console = RecordingConsole::new();
        let out = check(&stage, &fs, &console).expect("check ok");
        assert_eq!(out, ConditionalOutcome::Run);
    }

    #[test]
    fn empty_filter_short_circuits_without_consulting_regex() {
        // Even a clearly invalid regex shouldn't matter if the filter is
        // empty (early-return). Combined here with an empty filter.
        let stage = Stage {
            only_if_arch: String::new(),
            ..Default::default()
        };
        let fs = RealVfs::new();
        let console = RecordingConsole::new();
        let out = check(&stage, &fs, &console).expect("check ok");
        assert_eq!(out, ConditionalOutcome::Run);
    }
}
