//! Port of `pkg/console/console.go` plus the `plugins.Console` interface
//! from `pkg/plugins/common.go` (lines 27-31).
//!
//! The trait abstracts subprocess execution so plugins can shell out on
//! real hosts while tests inject a `RecordingConsole` that captures
//! invocations without running anything.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::Mutex;

use crate::error::{Error, Result};

/// Abstraction over subprocess execution. Lets plugins shell out for
/// real on production hosts while staying mockable in tests.
///
/// Ports the Go `plugins.Console` interface from `pkg/plugins/common.go`.
pub trait Console: Send + Sync {
    /// Run a shell command. The command is passed verbatim to `/bin/sh -c`
    /// (same as Go's `exec.Command("sh", "-c", cmd)`). Returns combined
    /// stdout+stderr as a `String`. Errors when the exit code is non-zero.
    fn run(&self, cmd: &str) -> Result<String>;

    /// Run a shell command in a specific working directory. Same semantics
    /// as [`Console::run`] but with the child's cwd set to `cwd`.
    fn run_in(&self, cwd: &Path, cmd: &str) -> Result<String>;

    /// Spawn a command and return its [`Output`] for callers that need
    /// separate stdout/stderr handles. Most plugins use [`Console::run`]
    /// instead. The default impl delegates to `run` and stuffs the
    /// combined output into `stdout`.
    fn run_with_output(&self, cmd: &str) -> Result<Output> {
        use std::os::unix::process::ExitStatusExt;
        let combined = self.run(cmd)?;
        Ok(Output {
            status: std::process::ExitStatus::from_raw(0),
            stdout: combined.into_bytes(),
            stderr: Vec::new(),
        })
    }

    /// Run a list of commands as a templated unit. Mirrors Go
    /// `Console.RunTemplate([]string, string)`. The `template` arg is a
    /// printf-style format string with a single `%s` marker (matches Go's
    /// `fmt.Sprintf(template, svc)`); each entry in `cmds` is substituted
    /// in turn and the resulting command run through [`Console::run`].
    /// Errors from individual commands are collected and returned as
    /// [`Error::Multi`] (mirrors Go's `multierror.Append`).
    fn run_template(&self, cmds: &[String], template: &str) -> Result<()> {
        let mut errs: Vec<Error> = Vec::new();
        for svc in cmds {
            let rendered = render_printf(template, svc);
            if let Err(e) = self.run(&rendered) {
                errs.push(e);
            }
        }
        match errs.len() {
            0 => Ok(()),
            _ => Err(Error::Multi(errs)),
        }
    }
}

/// Substitute the first `%s` in `template` with `arg`. `%%` escapes a
/// literal `%`. Anything else is passed through verbatim. This matches
/// the subset of `fmt.Sprintf` actually used by the Go `RunTemplate`
/// callers (e.g. `"systemctl enable %s"`).
fn render_printf(template: &str, arg: &str) -> String {
    let mut out = String::with_capacity(template.len() + arg.len());
    let mut chars = template.chars().peekable();
    let mut substituted = false;
    while let Some(c) = chars.next() {
        if c == '%' {
            match chars.peek() {
                Some('%') => {
                    chars.next();
                    out.push('%');
                }
                Some('s') if !substituted => {
                    chars.next();
                    out.push_str(arg);
                    substituted = true;
                }
                _ => out.push(c),
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Production impl. Shells out to `/bin/sh -c` (matching Go behaviour).
pub struct StandardConsole;

impl StandardConsole {
    pub fn new() -> Self {
        Self
    }
}

impl Default for StandardConsole {
    fn default() -> Self {
        Self::new()
    }
}

fn run_sh(cmd: &str, cwd: Option<&Path>) -> Result<String> {
    let mut c = Command::new("/bin/sh");
    c.arg("-c").arg(cmd);
    if let Some(dir) = cwd {
        c.current_dir(dir);
    }
    let output = c
        .output()
        .map_err(|e| Error::Cmd {
            cmd: cmd.to_string(),
            status: None,
            stderr: e.to_string(),
            stdout: String::new(),
        })?;

    // Match Go's CombinedOutput: stdout + stderr concatenated.
    let mut combined = output.stdout.clone();
    combined.extend_from_slice(&output.stderr);
    let combined_s = String::from_utf8_lossy(&combined).into_owned();

    if !output.status.success() {
        return Err(Error::Cmd {
            cmd: cmd.to_string(),
            status: output.status.code(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        });
    }
    Ok(combined_s)
}

impl Console for StandardConsole {
    fn run(&self, cmd: &str) -> Result<String> {
        run_sh(cmd, None)
    }

    fn run_in(&self, cwd: &Path, cmd: &str) -> Result<String> {
        run_sh(cmd, Some(cwd))
    }

    fn run_with_output(&self, cmd: &str) -> Result<Output> {
        let mut c = Command::new("/bin/sh");
        c.arg("-c").arg(cmd);
        let output = c.output().map_err(|e| Error::Cmd {
            cmd: cmd.to_string(),
            status: None,
            stderr: e.to_string(),
            stdout: String::new(),
        })?;
        if !output.status.success() {
            return Err(Error::Cmd {
                cmd: cmd.to_string(),
                status: output.status.code(),
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
                stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            });
        }
        Ok(output)
    }
}

/// One recorded `run` / `run_in` invocation.
#[derive(Debug, Clone)]
pub struct RecordedCall {
    pub cmd: String,
    pub cwd: Option<PathBuf>,
}

struct RecordingState {
    calls: Vec<RecordedCall>,
    responses: HashMap<String, std::result::Result<String, String>>,
}

/// Test-only mock. Records every `run` call without executing anything.
/// Returns a configurable canned response (default empty string, success).
///
/// Has helper methods for tests to inspect what got recorded and to
/// install per-command canned responses.
pub struct RecordingConsole {
    inner: Mutex<RecordingState>,
}

impl RecordingConsole {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(RecordingState {
                calls: Vec::new(),
                responses: HashMap::new(),
            }),
        }
    }

    /// Install a canned response for an exact command string. Subsequent
    /// matching calls return this instead of an empty success.
    pub fn expect(&self, cmd: impl Into<String>, response: std::result::Result<String, String>) {
        let mut state = self.inner.lock().expect("RecordingConsole mutex poisoned");
        state.responses.insert(cmd.into(), response);
    }

    /// Returns the list of calls recorded so far, in order.
    pub fn calls(&self) -> Vec<RecordedCall> {
        let state = self.inner.lock().expect("RecordingConsole mutex poisoned");
        state.calls.clone()
    }

    /// Convenience: returns just the command strings.
    pub fn commands(&self) -> Vec<String> {
        self.calls().into_iter().map(|c| c.cmd).collect()
    }

    fn record(&self, cmd: &str, cwd: Option<PathBuf>) -> Result<String> {
        let mut state = self.inner.lock().expect("RecordingConsole mutex poisoned");
        state.calls.push(RecordedCall {
            cmd: cmd.to_string(),
            cwd,
        });
        match state.responses.get(cmd) {
            Some(Ok(s)) => Ok(s.clone()),
            Some(Err(stderr)) => Err(Error::Cmd {
                cmd: cmd.to_string(),
                status: Some(1),
                stderr: stderr.clone(),
                stdout: String::new(),
            }),
            None => Ok(String::new()),
        }
    }
}

impl Default for RecordingConsole {
    fn default() -> Self {
        Self::new()
    }
}

impl Console for RecordingConsole {
    fn run(&self, cmd: &str) -> Result<String> {
        self.record(cmd, None)
    }

    fn run_in(&self, cwd: &Path, cmd: &str) -> Result<String> {
        self.record(cmd, Some(cwd.to_path_buf()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    // ---- StandardConsole ----

    #[test]
    fn standard_run_true_is_ok_and_empty() {
        let c = StandardConsole::new();
        let out = c.run("/bin/true").expect("true should succeed");
        assert!(out.is_empty(), "expected empty combined output, got {out:?}");
    }

    #[test]
    fn standard_run_false_returns_cmd_error() {
        let c = StandardConsole::new();
        let err = c.run("/bin/false").expect_err("false should fail");
        match err {
            Error::Cmd { status, .. } => {
                assert_ne!(status, Some(0), "non-zero exit expected, got {status:?}");
            }
            other => panic!("expected Error::Cmd, got {other:?}"),
        }
    }

    #[test]
    fn standard_run_captures_stdout() {
        let c = StandardConsole::new();
        let out = c.run("printf hello").expect("printf should succeed");
        assert_eq!(out, "hello");
    }

    #[test]
    fn standard_run_in_uses_cwd() {
        let dir = tempdir().expect("tempdir");
        let c = StandardConsole::new();
        let out = c.run_in(dir.path(), "pwd").expect("pwd should succeed");
        // `pwd` may resolve symlinks (e.g. /tmp -> /private/tmp on macOS, though
        // we're linux-only here). Compare canonicalized paths to be safe.
        let want = std::fs::canonicalize(dir.path()).expect("canonicalize tempdir");
        let got = std::fs::canonicalize(out.trim()).expect("canonicalize pwd output");
        assert_eq!(got, want);
    }

    // ---- RecordingConsole ----

    #[test]
    fn recording_default_response_is_empty_ok() {
        let c = RecordingConsole::new();
        let out = c.run("echo x").expect("default response is Ok");
        assert_eq!(out, "");
        assert_eq!(c.commands(), vec!["echo x".to_string()]);
    }

    #[test]
    fn recording_expect_ok_overrides_default() {
        let c = RecordingConsole::new();
        c.expect("foo", Ok("bar".to_string()));
        let out = c.run("foo").expect("expected Ok");
        assert_eq!(out, "bar");
    }

    #[test]
    fn recording_expect_err_returns_cmd_error() {
        let c = RecordingConsole::new();
        c.expect("foo", Err("boom".to_string()));
        let err = c.run("foo").expect_err("expected Err");
        match err {
            Error::Cmd { stderr, cmd, .. } => {
                assert_eq!(cmd, "foo");
                assert_eq!(stderr, "boom");
            }
            other => panic!("expected Error::Cmd, got {other:?}"),
        }
    }

    #[test]
    fn recording_accumulates_calls_in_order() {
        let c = RecordingConsole::new();
        c.run("one").unwrap();
        c.run("two").unwrap();
        c.run_in(Path::new("/tmp"), "three").unwrap();

        let calls = c.calls();
        assert_eq!(calls.len(), 3);
        assert_eq!(calls[0].cmd, "one");
        assert!(calls[0].cwd.is_none());
        assert_eq!(calls[1].cmd, "two");
        assert!(calls[1].cwd.is_none());
        assert_eq!(calls[2].cmd, "three");
        assert_eq!(calls[2].cwd.as_deref(), Some(Path::new("/tmp")));
    }

    #[test]
    fn recording_commands_matches_calls_mapped() {
        let c = RecordingConsole::new();
        c.run("a").unwrap();
        c.run("b").unwrap();
        c.run("c").unwrap();

        let mapped: Vec<String> = c.calls().iter().map(|x| x.cmd.clone()).collect();
        assert_eq!(c.commands(), mapped);
    }

    // ---- run_template ----

    #[test]
    fn run_template_substitutes_each_cmd() {
        let c = RecordingConsole::new();
        let cmds = vec!["sshd".to_string(), "cron".to_string()];
        c.run_template(&cmds, "systemctl enable %s")
            .expect("all default-ok");
        assert_eq!(
            c.commands(),
            vec![
                "systemctl enable sshd".to_string(),
                "systemctl enable cron".to_string(),
            ]
        );
    }

    #[test]
    fn run_template_aggregates_errors() {
        let c = RecordingConsole::new();
        c.expect("systemctl enable bad", Err("nope".to_string()));
        let cmds = vec!["good".to_string(), "bad".to_string()];
        let err = c
            .run_template(&cmds, "systemctl enable %s")
            .expect_err("bad should fail");
        match err {
            Error::Multi(errs) => assert_eq!(errs.len(), 1),
            other => panic!("expected Error::Multi, got {other:?}"),
        }
        // Both calls were still attempted.
        assert_eq!(c.commands().len(), 2);
    }

    #[test]
    fn render_printf_handles_escape_and_no_marker() {
        assert_eq!(render_printf("systemctl enable %s", "sshd"), "systemctl enable sshd");
        assert_eq!(render_printf("100%% done %s", "x"), "100% done x");
        // No marker -> template returned verbatim.
        assert_eq!(render_printf("no marker here", "x"), "no marker here");
        // Only first %s consumed.
        assert_eq!(render_printf("%s and %s", "A"), "A and %s");
    }
}
