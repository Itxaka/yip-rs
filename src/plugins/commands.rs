//! `commands` plugin — execute each shell command from `stage.commands`.
//!
//! Port of `pkg/plugins/commands.go::Commands`. For every entry in
//! `stage.commands` we render the string through `templateSysData`
//! (sprig templating against the gathered system facts), then shell out
//! via [`Console::run`]. Errors do not abort the loop — every command is
//! attempted, and per-command errors are aggregated into [`Error::Multi`]
//! so the executor's multierror semantics match the Go side.
//!
//! Empty `stage.commands` is a silent no-op.
//!
//! Templating: matches Go's per-command pass. The executor already
//! template-renders the WHOLE config blob once; doing it again per
//! command lets configs reference `{{ env "USER" }}` / `{{ .Values.* }}`
//! values that aren't substituted at parse time (typically because the
//! config-blob render skipped a malformed segment). A render failure for
//! an individual command falls back to running the raw string, matching
//! the Go implementation's behaviour.

use std::sync::Arc;

use tracing::{debug, info, warn};

use crate::console::Console;
use crate::error::{Error, Result};
use crate::executor::Plugin;
use crate::schema::Stage;
use crate::vfs::Vfs;

/// Build a [`Plugin`] arc-closure.
pub fn build() -> Plugin {
    Arc::new(run)
}

/// Pure entry point — exposed so tests don't have to go through `Arc`.
pub fn run(stage: &Stage, _fs: &dyn Vfs, console: &dyn Console) -> Result<()> {
    if stage.commands.is_empty() {
        return Ok(());
    }

    info!(count = stage.commands.len(), "running stage commands");

    let mut errs: Vec<Error> = Vec::new();
    for cmd in &stage.commands {
        // Per-command sprig render. Matches Go: failure to template
        // falls back to the raw string rather than aborting.
        let rendered = match crate::template::render_with_sysdata(cmd) {
            Ok(s) => s,
            Err(e) => {
                warn!(cmd = %cmd, error = %e, "command template render failed, using raw");
                cmd.clone()
            }
        };
        debug!(cmd = %rendered, "running command");
        match console.run(&rendered) {
            Ok(out) => {
                let trimmed = out.trim();
                if trimmed.is_empty() {
                    debug!("empty command output");
                } else {
                    debug!(output = %trimmed, "command output");
                }
            }
            Err(e) => {
                warn!(cmd = %rendered, error = %e, "command failed");
                errs.push(e);
            }
        }
    }

    match errs.len() {
        0 => Ok(()),
        _ => Err(Error::Multi(errs)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::console::RecordingConsole;
    use crate::vfs::MemVfs;

    #[test]
    fn empty_stage_is_ok() {
        let stage = Stage::default();
        let fs = MemVfs::new();
        let console = RecordingConsole::new();
        run(&stage, &fs, &console).expect("empty commands -> Ok");
        assert!(console.commands().is_empty());
    }

    #[test]
    fn three_commands_all_succeed() {
        let stage = Stage {
            commands: vec![
                "echo foo".to_string(),
                "echo bar".to_string(),
                "echo baz".to_string(),
            ],
            ..Default::default()
        };
        let fs = MemVfs::new();
        let console = RecordingConsole::new();
        run(&stage, &fs, &console).expect("all should succeed");
        assert_eq!(
            console.commands(),
            vec![
                "echo foo".to_string(),
                "echo bar".to_string(),
                "echo baz".to_string(),
            ]
        );
    }

    #[test]
    fn middle_command_fails_but_others_still_run() {
        let stage = Stage {
            commands: vec![
                "ok-1".to_string(),
                "fail-cmd".to_string(),
                "ok-2".to_string(),
            ],
            ..Default::default()
        };
        let fs = MemVfs::new();
        let console = RecordingConsole::new();
        console.expect("fail-cmd", Err("boom".to_string()));

        let err = run(&stage, &fs, &console).expect_err("middle should fail aggregate");
        match err {
            Error::Multi(errs) => assert_eq!(errs.len(), 1),
            other => panic!("expected Error::Multi, got {other:?}"),
        }

        // All three were still attempted.
        assert_eq!(
            console.commands(),
            vec![
                "ok-1".to_string(),
                "fail-cmd".to_string(),
                "ok-2".to_string(),
            ]
        );
    }

    #[test]
    fn matches_go_basic_test() {
        // Go test: Commands{"echo foo", "echo bar"} -> 2 recorded calls, no err.
        let stage = Stage {
            commands: vec!["echo foo".to_string(), "echo bar".to_string()],
            ..Default::default()
        };
        let fs = MemVfs::new();
        let console = RecordingConsole::new();
        run(&stage, &fs, &console).expect("should succeed");
        assert_eq!(
            console.commands(),
            vec!["echo foo".to_string(), "echo bar".to_string()],
        );
    }

    // ------------------------------------------------------------------
    // Ported from Go: multi-line / special-char / env-var-bearing cmds.
    // ------------------------------------------------------------------

    #[test]
    fn multi_line_command_passed_verbatim() {
        // A shell command split across lines via `\` continuation. The plugin
        // should record it byte-for-byte and let the shell interpret it.
        let cmd = "echo foo \\\n  && echo bar".to_string();
        let stage = Stage {
            commands: vec![cmd.clone()],
            ..Default::default()
        };
        let fs = MemVfs::new();
        let console = RecordingConsole::default();
        run(&stage, &fs, &console).expect("ok");
        assert_eq!(console.commands(), vec![cmd]);
    }

    #[test]
    fn command_with_shell_special_chars_is_unmodified() {
        // Pipes, semicolons, parens, redirection — all should reach the
        // console verbatim. The plugin doesn't shell-escape.
        let cmds = vec![
            "(echo hi; echo there) | tee /tmp/out".to_string(),
            "true && false || echo recovered".to_string(),
            "echo 'quoted $value with \"nested\"' > /dev/null".to_string(),
        ];
        let stage = Stage {
            commands: cmds.clone(),
            ..Default::default()
        };
        let fs = MemVfs::new();
        let console = RecordingConsole::default();
        run(&stage, &fs, &console).expect("ok");
        assert_eq!(console.commands(), cmds);
    }

    #[test]
    fn command_containing_embedded_newlines_round_trips() {
        // Real "\n" inside the command string (heredoc body, for instance).
        let cmd = "cat <<EOF\nline1\nline2\nEOF".to_string();
        let stage = Stage {
            commands: vec![cmd.clone()],
            ..Default::default()
        };
        let fs = MemVfs::new();
        let console = RecordingConsole::default();
        run(&stage, &fs, &console).expect("ok");
        assert_eq!(console.commands(), vec![cmd]);
    }

    #[test]
    fn command_with_env_var_reference_unexpanded_in_recorder() {
        // Shell-style `$HOME` / `${USER:-nobody}` are not Go-template
        // syntax (no `{{ }}`), so the plugin passes them through
        // unchanged for /bin/sh to expand at exec time. The recording
        // console captures the raw, untouched string.
        let cmd = "echo $HOME and ${USER:-nobody}".to_string();
        let stage = Stage {
            commands: vec![cmd.clone()],
            ..Default::default()
        };
        let fs = MemVfs::new();
        let console = RecordingConsole::default();
        run(&stage, &fs, &console).expect("ok");
        assert_eq!(console.commands(), vec![cmd]);
    }

    #[test]
    fn command_failure_preserves_stderr_in_multi() {
        // Failures bubble up as Error::Multi; the per-cmd stderr must survive.
        let stage = Stage {
            commands: vec!["bad-cmd".to_string()],
            ..Default::default()
        };
        let fs = MemVfs::new();
        let console = RecordingConsole::default();
        console.expect("bad-cmd", Err("missing binary".to_string()));

        let err = run(&stage, &fs, &console).expect_err("must fail");
        match err {
            Error::Multi(errs) => {
                assert_eq!(errs.len(), 1);
                match &errs[0] {
                    Error::Cmd { stderr, cmd, .. } => {
                        assert_eq!(cmd, "bad-cmd");
                        assert_eq!(stderr, "missing binary");
                    }
                    other => panic!("expected Error::Cmd inside Multi, got {other:?}"),
                }
            }
            other => panic!("expected Error::Multi, got {other:?}"),
        }
    }

    // -------------------------------------------------------------------
    // Per-command template-render pass (mirrors Go's `templateSysData`).
    // -------------------------------------------------------------------

    #[test]
    fn env_template_in_command_is_rendered() {
        // Each command goes through `render_with_sysdata` before exec.
        // A `{{ env(name="...") }}` reference must resolve to the
        // env-var's value (tera function-call syntax — Go's
        // `{{ env "X" }}` is rewritten to this form by the preprocessor).
        std::env::set_var("YIP_RS_COMMANDS_TEST_USER", "alice");
        let cmd = r#"echo {{ env(name="YIP_RS_COMMANDS_TEST_USER") }}"#.to_string();
        let stage = Stage {
            commands: vec![cmd],
            ..Default::default()
        };
        let fs = MemVfs::new();
        let console = RecordingConsole::default();
        run(&stage, &fs, &console).expect("ok");
        // The console must have received the rendered form, not the raw
        // template string.
        assert_eq!(console.commands(), vec!["echo alice".to_string()]);
        std::env::remove_var("YIP_RS_COMMANDS_TEST_USER");
    }

    #[test]
    fn invalid_template_falls_back_to_raw_command() {
        // A template syntax error in a command string must not abort
        // the command — it falls back to running the raw string (matches
        // Go's behaviour where templateSysData errors are swallowed).
        // `{{ this is not valid }}` will fail tera parsing.
        let cmd = "echo {{ ".to_string();
        let stage = Stage {
            commands: vec![cmd.clone()],
            ..Default::default()
        };
        let fs = MemVfs::new();
        let console = RecordingConsole::default();
        run(&stage, &fs, &console).expect("must not abort on bad template");
        // Recorded command equals the original raw string.
        assert_eq!(console.commands(), vec![cmd]);
    }

    #[test]
    fn plain_command_without_template_passes_through_unchanged() {
        // Commands with no `{{ ... }}` segment must be invariant under
        // the per-command render pass.
        let cmds = vec![
            "echo plain".to_string(),
            "ls -la /tmp".to_string(),
            "true && false".to_string(),
        ];
        let stage = Stage {
            commands: cmds.clone(),
            ..Default::default()
        };
        let fs = MemVfs::new();
        let console = RecordingConsole::default();
        run(&stage, &fs, &console).expect("ok");
        assert_eq!(console.commands(), cmds);
    }
}
