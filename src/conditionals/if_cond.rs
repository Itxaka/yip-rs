//! `IfConditional` — runs `stage.if` as a shell command; non-zero exit (or
//! spawn / shell error) skips the stage.
//!
//! Ports `pkg/plugins/if.go` from Go yip. The Go version logs the output of
//! the command at debug level and wraps `console.Run` errors in an
//! `"if statement error"` message that the executor then treats as
//! "skip this stage". In Rust we encode that "skip" outcome explicitly
//! via [`ConditionalOutcome::Skip`] rather than tunnelling it through
//! `Err`.
//!
//! Note: Go yip pipes `stage.If` through `templateSysData` before exec'ing
//! it (substitutes `{{ .Values }}` etc). The template engine lives in
//! `src/template/` but is out of scope for this conditional in wave 2 —
//! the command string is sent to the console verbatim. When the template
//! plugin lands, the call site below can wrap the `if` string the same
//! way without changing this file's signature.

use std::sync::Arc;

use crate::console::Console;
use crate::error::Result;
use crate::executor::{Conditional, ConditionalOutcome};
use crate::schema::Stage;
use crate::vfs::Vfs;

/// Constructor — returns the boxed conditional ready to register with the
/// executor. Matches the `build()` convention used by the other
/// conditionals in this module.
pub fn build() -> Conditional {
    Arc::new(check)
}

/// Evaluate `stage.if`:
///   - empty string → [`ConditionalOutcome::Run`] (no gate configured)
///   - shell exit 0 → [`ConditionalOutcome::Run`]
///   - shell non-zero / spawn failure → [`ConditionalOutcome::Skip`]
///
/// Never returns `Err` — the Go side swallows the underlying error into
/// a "skip this stage" decision, and we preserve that behaviour so a
/// failing `if` command doesn't abort the whole config.
pub fn check(stage: &Stage, _fs: &dyn Vfs, console: &dyn Console) -> Result<ConditionalOutcome> {
    if stage.r#if.is_empty() {
        return Ok(ConditionalOutcome::Run);
    }

    match console.run(&stage.r#if) {
        Ok(_) => Ok(ConditionalOutcome::Run),
        Err(_) => Ok(ConditionalOutcome::Skip),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::console::RecordingConsole;
    use crate::vfs::MemVfs;

    fn stage_with_if(s: &str) -> Stage {
        Stage {
            r#if: s.to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn empty_if_runs_without_calling_console() {
        let fs = MemVfs::new();
        let console = RecordingConsole::new();
        let stage = Stage::default();

        let outcome = check(&stage, &fs, &console).expect("never errors");

        assert_eq!(outcome, ConditionalOutcome::Run);
        assert!(
            console.calls().is_empty(),
            "empty `if` must not invoke the console, got {:?}",
            console.calls()
        );
    }

    #[test]
    fn true_command_runs_stage() {
        let fs = MemVfs::new();
        let console = RecordingConsole::new();
        // RecordingConsole default response is Ok("") — simulates exit 0.
        let stage = stage_with_if("true");

        let outcome = check(&stage, &fs, &console).expect("never errors");

        assert_eq!(outcome, ConditionalOutcome::Run);
        assert_eq!(console.commands(), vec!["true".to_string()]);
    }

    #[test]
    fn false_command_skips_stage() {
        let fs = MemVfs::new();
        let console = RecordingConsole::new();
        console.expect("false", Err("exit 1".to_string()));
        let stage = stage_with_if("false");

        let outcome = check(&stage, &fs, &console).expect("never errors");

        assert_eq!(outcome, ConditionalOutcome::Skip);
        assert_eq!(console.commands(), vec!["false".to_string()]);
    }

    #[test]
    fn bogus_command_skips_without_panicking() {
        let fs = MemVfs::new();
        let console = RecordingConsole::new();
        console.expect(
            "some-bogus-command",
            Err("sh: some-bogus-command: not found".to_string()),
        );
        let stage = stage_with_if("some-bogus-command");

        let outcome = check(&stage, &fs, &console).expect("never errors");

        assert_eq!(outcome, ConditionalOutcome::Skip);
        assert_eq!(console.commands(), vec!["some-bogus-command".to_string()]);
    }

    #[test]
    fn build_returns_callable_conditional() {
        let fs = MemVfs::new();
        let console = RecordingConsole::new();
        let stage = stage_with_if("ok");

        let cond = build();
        let outcome = cond(&stage, &fs, &console).expect("never errors");

        assert_eq!(outcome, ConditionalOutcome::Run);
        assert_eq!(console.commands(), vec!["ok".to_string()]);
    }
}
