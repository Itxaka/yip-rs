//! `IfArch` — port of `pkg/plugins/if_arch.go`.
//!
//! Skips a stage when `stage.only_arch` is set and does not regex-match the
//! current architecture. Mirrors Go's
//! `regexp.MatchString(stage.OnlyIfArch, runtime.GOARCH)` semantics: partial
//! / pattern matches succeed, and an empty `only_arch` field is treated as
//! "no filter, always run".
//!
//! `runtime.GOARCH` uses Go names ("amd64", "arm64", ...) while Rust's
//! `std::env::consts::ARCH` uses target-triple names ("x86_64", "aarch64",
//! ...). To keep configs written for Go yip working unchanged, the regex is
//! matched against BOTH the Rust arch string AND its Go equivalent (when one
//! exists). Practically this means `only_arch: amd64` and
//! `only_arch: x86_64` both run on an x86_64 host; `only_arch: arm64` and
//! `only_arch: aarch64` both run on aarch64; etc. Arches whose Go and Rust
//! names already agree (riscv64, mips64, s390x, ...) need no aliasing.

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

/// Map a Rust `std::env::consts::ARCH` value to the equivalent
/// `runtime.GOARCH` string, when they differ. Returns `None` when the names
/// already match (riscv64, mips64, s390x, ...) or when no Go equivalent is
/// defined. The pairings come from Go's `src/internal/goarch/goarch.go` and
/// Rust's target-triple set; only the values we expect to see in practice on
/// Kairos-ish hosts are enumerated.
fn go_arch_alias(rust_arch: &str) -> Option<&'static str> {
    match rust_arch {
        "x86_64" => Some("amd64"),
        "aarch64" => Some("arm64"),
        "x86" => Some("386"),
        "powerpc64" => Some("ppc64"),
        // riscv64, mips64, mips, s390x, arm, wasm32 — Go and Rust agree.
        _ => None,
    }
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
        return Ok(ConditionalOutcome::Run);
    }
    // Try the Go-equivalent name so configs written for Go yip (e.g.
    // `only_arch: amd64`) keep working on a Rust host where
    // `std::env::consts::ARCH` is `x86_64`.
    if let Some(go) = go_arch_alias(arch) {
        if re.is_match(go) {
            return Ok(ConditionalOutcome::Run);
        }
    }
    tracing::debug!(
        arch = %arch,
        go_arch = ?go_arch_alias(arch),
        only_arch = %stage.only_if_arch,
        "arch does not match only_arch, skipping stage",
    );
    Ok(ConditionalOutcome::Skip)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::console::RecordingConsole;
    use crate::vfs::{MemVfs, RealVfs};

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

    // --- Direct ports of Go's `if_test.go` `IfArchConditional` Describe ---
    // Source: yip/pkg/plugins/if_test.go, Describe("IfArchConditional"),
    // 2 It blocks.
    //
    // Divergence notes:
    //   - Go's test asserts on a textual error containing the formatted
    //     `SkipOnlyArch` template with `runtime.GOARCH` + the pattern. The
    //     Rust port collapses conditional errors to
    //     `ConditionalOutcome::Skip`, so we assert on the outcome.
    //   - `runtime.GOARCH` differs from `std::env::consts::ARCH` (Go uses
    //     "amd64"/"arm64", Rust uses "x86_64"/"aarch64"). The "Succeeds"
    //     Go test uses `runtime.GOARCH` as the regex; we use
    //     `std::env::consts::ARCH` — semantically "match the host arch".
    //   - The Go suite uses a vfst seeded with /etc/hostname and /etc/hosts,
    //     but `IfArch` never reads the fs. We use `MemVfs` (per scope) and
    //     `RecordingConsole::default()` and verify nothing is written/run.

    /// Go: `It("Fails with no match")` — `OnlyIfArch: "weird"` must Skip.
    #[test]
    fn go_port_if_arch_fails_with_no_match() {
        let stage = Stage {
            only_if_arch: "weird".into(),
            ..Default::default()
        };
        let fs = MemVfs::new();
        let console = RecordingConsole::default();
        let out = check(&stage, &fs, &console).expect("check ok");
        assert_eq!(out, ConditionalOutcome::Skip);
        assert!(console.commands().is_empty());
    }

    /// Go: `It("Succeeds")` — `OnlyIfArch: runtime.GOARCH` (Rust equivalent:
    /// `std::env::consts::ARCH`) must Run.
    #[test]
    fn go_port_if_arch_succeeds_on_runtime_arch() {
        let stage = Stage {
            only_if_arch: std::env::consts::ARCH.to_string(),
            ..Default::default()
        };
        let fs = MemVfs::new();
        let console = RecordingConsole::default();
        let out = check(&stage, &fs, &console).expect("check ok");
        assert_eq!(out, ConditionalOutcome::Run);
        assert!(console.commands().is_empty());
    }

    // --- Go-arch aliasing tests ---
    //
    // These exercise the `go_arch_alias` path: a config written using the
    // Go `runtime.GOARCH` name (amd64, arm64, 386, ...) should still match
    // on a Rust host whose `std::env::consts::ARCH` is the equivalent
    // target-triple name (x86_64, aarch64, x86, ...).
    //
    // Each test is gated to the host arch that exercises the alias, since
    // we can't override `env::consts::ARCH` at runtime. Off-arch hosts
    // simply skip the gated assertion.

    /// `only_if_arch: amd64` on x86_64 → Run (Go name → Rust host).
    #[test]
    #[cfg(target_arch = "x86_64")]
    fn go_alias_amd64_runs_on_x86_64() {
        let stage = Stage {
            only_if_arch: "amd64".into(),
            ..Default::default()
        };
        let fs = MemVfs::new();
        let console = RecordingConsole::default();
        let out = check(&stage, &fs, &console).expect("check ok");
        assert_eq!(out, ConditionalOutcome::Run);
    }

    /// `only_if_arch: arm64` on x86_64 → Skip (different arch, alias must
    /// not over-match).
    #[test]
    #[cfg(target_arch = "x86_64")]
    fn go_alias_arm64_skips_on_x86_64() {
        let stage = Stage {
            only_if_arch: "arm64".into(),
            ..Default::default()
        };
        let fs = MemVfs::new();
        let console = RecordingConsole::default();
        let out = check(&stage, &fs, &console).expect("check ok");
        assert_eq!(out, ConditionalOutcome::Skip);
    }

    /// `only_if_arch: arm64` on aarch64 → Run (Go name → Rust host).
    #[test]
    #[cfg(target_arch = "aarch64")]
    fn go_alias_arm64_runs_on_aarch64() {
        let stage = Stage {
            only_if_arch: "arm64".into(),
            ..Default::default()
        };
        let fs = MemVfs::new();
        let console = RecordingConsole::default();
        let out = check(&stage, &fs, &console).expect("check ok");
        assert_eq!(out, ConditionalOutcome::Run);
    }

    /// `only_if_arch: (amd64|arm64)` on x86_64 → Run (alternation hitting
    /// the Go alias for the host).
    #[test]
    #[cfg(target_arch = "x86_64")]
    fn go_alias_alternation_runs_on_x86_64() {
        let stage = Stage {
            only_if_arch: "(amd64|arm64)".into(),
            ..Default::default()
        };
        let fs = MemVfs::new();
        let console = RecordingConsole::default();
        let out = check(&stage, &fs, &console).expect("check ok");
        assert_eq!(out, ConditionalOutcome::Run);
    }

    /// `only_if_arch: 386` on x86 → Run. Gated to 32-bit x86 hosts only.
    #[test]
    #[cfg(target_arch = "x86")]
    fn go_alias_386_runs_on_x86() {
        let stage = Stage {
            only_if_arch: "386".into(),
            ..Default::default()
        };
        let fs = MemVfs::new();
        let console = RecordingConsole::default();
        let out = check(&stage, &fs, &console).expect("check ok");
        assert_eq!(out, ConditionalOutcome::Run);
    }

    /// Pure function-level check of the alias table — runs on every host,
    /// no `env::consts::ARCH` dependency.
    #[test]
    fn go_arch_alias_table() {
        assert_eq!(go_arch_alias("x86_64"), Some("amd64"));
        assert_eq!(go_arch_alias("aarch64"), Some("arm64"));
        assert_eq!(go_arch_alias("x86"), Some("386"));
        assert_eq!(go_arch_alias("powerpc64"), Some("ppc64"));
        // Names that already agree between Go and Rust must not be aliased.
        assert_eq!(go_arch_alias("riscv64"), None);
        assert_eq!(go_arch_alias("s390x"), None);
        assert_eq!(go_arch_alias("mips64"), None);
        // Unknown arches map to None (caller falls back to Skip).
        assert_eq!(go_arch_alias("not-a-real-arch"), None);
    }
}
