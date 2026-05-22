//! `commands` plugin — execute each shell command from `stage.commands`.
//!
//! Port of `pkg/plugins/commands.go::Commands`. For every entry in
//! `stage.commands` we shell out via [`Console::run`]. Errors do not abort
//! the loop — every command is attempted, and per-command errors are
//! aggregated into [`Error::Multi`] so the executor's multierror semantics
//! match the Go side.
//!
//! Empty `stage.commands` is a silent no-op.
//!
//! Note: the Go plugin runs each command through `templateSysData` (Sprig
//! templating). The Rust port leaves templating to a higher layer for now —
//! commands are passed verbatim to the console.

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
        debug!(cmd = %cmd, "running command");
        match console.run(cmd) {
            Ok(out) => {
                let trimmed = out.trim();
                if trimmed.is_empty() {
                    debug!("empty command output");
                } else {
                    debug!(output = %trimmed, "command output");
                }
            }
            Err(e) => {
                warn!(cmd = %cmd, error = %e, "command failed");
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
        // The plugin does not template / expand env vars itself; the recording
        // console captures the raw string. (Real Console would let /bin/sh
        // expand it.)
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
}
