//! `git` plugin — clone or update a git repository as part of a stage.
//!
//! Port of `pkg/plugins/git_binary.go::Git`. The Go side has three build
//! flavours (`go-git` library, `gitbinary` shell-out, `nogit` stub); the
//! Rust port unifies on the shell-out path because:
//!
//!   - `gix` is heavy and feature-gated via `git-builtin`. Shelling out
//!     keeps the binary small and matches dracut/initramfs reality where a
//!     `git` binary is the realistic dependency anyway.
//!   - Every operation goes through [`Console::run`], which means tests
//!     using [`crate::console::RecordingConsole`] can assert exactly which
//!     command strings the plugin issues — no real git invocation required.
//!
//! Behaviour summary:
//!   1. If `stage.git.url` is empty, return `Ok(())` (no-op).
//!   2. Ensure the parent of `git.path` exists via `Vfs::mkdir_all`.
//!   3. If `<path>/.git` already exists, run `git -C <path> fetch origin <branch>`,
//!      then `git -C <path> reset --hard origin/<branch>`, and if
//!      `branch_only` is set, finally `git -C <path> checkout <branch>`.
//!   4. Otherwise, run `git clone --branch <branch> [--single-branch] <url> <path>`.
//!   5. Default branch when unset is `master` (matches Go).
//!
//! Authentication:
//!   - `username` + `password` → embedded in the URL as
//!     `https://user:pass@host/...`. The Go binary plugin uses a
//!     `GIT_ASKPASS` helper script; embedding in the URL is simpler, has
//!     the same effect for HTTPS, and avoids writing a temporary helper.
//!   - `private_key` → written to a `tempfile::NamedTempFile` whose path is
//!     fed to `GIT_SSH_COMMAND="ssh -i <file>"`. The tempfile is dropped
//!     (and removed) when the plugin returns. If `insecure` is true,
//!     `-o StrictHostKeyChecking=no` is added (matches the Go binary impl).
//!   - `public_key` → no-op for now. The Go go-git path uses it as a fixed
//!     host key; the Go binary path ignores it. The Rust port matches the
//!     binary path: silently ignored.
//!
//! Errors:
//!   - Failures from `console.run` already surface as [`Error::Cmd`]; the
//!     plugin propagates them unchanged.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use tempfile::NamedTempFile;
use tracing::{debug, info};
use url::Url;

use crate::console::Console;
use crate::error::{Error, Result};
use crate::executor::Plugin;
use crate::schema::git::{Auth, Git};
use crate::schema::Stage;
use crate::vfs::Vfs;

/// Build a [`Plugin`] arc-closure for executor registration.
pub fn build() -> Plugin {
    Arc::new(run)
}

/// Pure entry point — bypasses `Arc` so tests can call directly.
pub fn run(stage: &Stage, fs: &dyn Vfs, console: &dyn Console) -> Result<()> {
    let g = &stage.git;
    if g.url.is_empty() {
        debug!("git plugin: empty url, skipping");
        return Ok(());
    }
    if g.path.is_empty() {
        return Err(Error::other("git plugin: empty path"));
    }

    let branch = if g.branch.is_empty() {
        "master"
    } else {
        g.branch.as_str()
    };

    info!(url = %g.url, path = %g.path, branch = %branch, "git: starting");

    // 1. Ensure parent directory exists. We don't create `path` itself —
    //    `git clone` wants to do that. But the parent must exist or the
    //    clone fails. Using mkdir_all is a no-op when the parent is "/".
    let target = PathBuf::from(&g.path);
    if let Some(parent) = target.parent() {
        if !parent.as_os_str().is_empty() && parent != Path::new("") {
            debug!(parent = %parent.display(), "ensuring parent dir exists");
            fs.mkdir_all(parent)?;
        }
    }

    // 2. Build the auth context. `_key_guard` keeps the SSH key tempfile
    //    alive until this function returns; dropping it removes the file.
    let (effective_url, env_prefix, _key_guard) = build_auth(&g.url, &g.auth)?;

    // 3. Decide: update existing repo, or fresh clone?
    let git_marker = target.join(".git");
    if fs.exists(&git_marker) {
        info!(path = %g.path, "repository already exists, updating");
        update_existing(&g.path, branch, g.branch_only, &env_prefix, console)
    } else {
        info!(url = %effective_url, path = %g.path, "cloning fresh repository");
        clone_fresh(
            &effective_url,
            &g.path,
            branch,
            g.branch_only,
            &env_prefix,
            console,
        )
    }
}

/// Run the "update an existing checkout" sequence: fetch + reset, optionally
/// followed by a branch checkout when `branch_only` is set.
fn update_existing(
    path: &str,
    branch: &str,
    branch_only: bool,
    env_prefix: &str,
    console: &dyn Console,
) -> Result<()> {
    let fetch = format!(
        "{env}git -C {path} fetch origin {branch}",
        env = env_prefix,
        path = shell_quote(path),
        branch = shell_quote(branch),
    );
    debug!(cmd = %fetch, "git fetch");
    console.run(&fetch)?;

    let reset = format!(
        "{env}git -C {path} reset --hard origin/{branch}",
        env = env_prefix,
        path = shell_quote(path),
        branch = shell_arg_with_prefix(branch),
    );
    debug!(cmd = %reset, "git reset");
    console.run(&reset)?;

    if branch_only {
        let checkout = format!(
            "{env}git -C {path} checkout {branch}",
            env = env_prefix,
            path = shell_quote(path),
            branch = shell_quote(branch),
        );
        debug!(cmd = %checkout, "git checkout");
        console.run(&checkout)?;
    }

    Ok(())
}

/// Run `git clone` with appropriate flags. Mirrors the Go binary impl which
/// always passes `--branch <branch>` (defaulting to `master`) and toggles
/// `--single-branch` from `branch_only`.
fn clone_fresh(
    url: &str,
    path: &str,
    branch: &str,
    branch_only: bool,
    env_prefix: &str,
    console: &dyn Console,
) -> Result<()> {
    let mut cmd = format!(
        "{env}git clone --branch {branch}",
        env = env_prefix,
        branch = shell_quote(branch),
    );
    if branch_only {
        cmd.push_str(" --single-branch");
    }
    cmd.push(' ');
    cmd.push_str(&shell_quote(url));
    cmd.push(' ');
    cmd.push_str(&shell_quote(path));

    debug!(cmd = %cmd, "git clone");
    console.run(&cmd)?;
    Ok(())
}

/// Prepare the auth context: returns
///   - the effective URL (with credentials embedded for HTTP basic auth)
///   - an env-var prefix (e.g. `"GIT_SSH_COMMAND='ssh -i /tmp/abc' "`) to
///     paste at the front of each `git` invocation; empty when no auth
///   - an `Option<NamedTempFile>` holding any temp key file so it survives
///     until the caller is done.
fn build_auth(url: &str, auth: &Auth) -> Result<(String, String, Option<NamedTempFile>)> {
    let mut env_prefix = String::new();
    let mut effective_url = url.to_string();
    let mut key_guard: Option<NamedTempFile> = None;

    // HTTP basic auth via URL embedding. Skipped when private_key is set
    // since SSH auth doesn't use HTTP creds.
    if auth.private_key.is_empty()
        && !auth.username.is_empty()
        && !auth.password.is_empty()
    {
        effective_url = embed_basic_auth(url, &auth.username, &auth.password)?;
    }

    // SSH private key auth.
    if !auth.private_key.is_empty() {
        let mut tf = NamedTempFile::new().map_err(|e| {
            Error::other(format!("git: cannot create tempfile for ssh key: {e}"))
        })?;
        // Ensure the private key has a trailing newline — OpenSSH refuses
        // keys without one.
        let mut bytes = auth.private_key.as_bytes().to_vec();
        if !bytes.ends_with(b"\n") {
            bytes.push(b'\n');
        }
        use std::io::Write;
        tf.write_all(&bytes)
            .map_err(|e| Error::other(format!("git: write ssh key: {e}")))?;
        tf.flush()
            .map_err(|e| Error::other(format!("git: flush ssh key: {e}")))?;

        let key_path = tf.path().to_path_buf();
        let ssh_cmd = if auth.insecure {
            format!(
                "ssh -i {key} -o StrictHostKeyChecking=no",
                key = shell_quote_path(&key_path),
            )
        } else {
            format!("ssh -i {key}", key = shell_quote_path(&key_path))
        };
        env_prefix.push_str(&format!(
            "GIT_SSH_COMMAND={cmd} ",
            cmd = shell_quote(&ssh_cmd),
        ));

        key_guard = Some(tf);
    }

    Ok((effective_url, env_prefix, key_guard))
}

/// Embed username/password into an HTTPS URL: `https://host/p` →
/// `https://user:pass@host/p`. Returns the original URL unchanged if it
/// doesn't parse (caller still gets *something* to pass to git, and the
/// resulting auth failure will surface as an [`Error::Cmd`]).
fn embed_basic_auth(url: &str, user: &str, pass: &str) -> Result<String> {
    let mut parsed = match Url::parse(url) {
        Ok(u) => u,
        Err(_) => {
            // Not a URL we can manipulate; leave it alone.
            return Ok(url.to_string());
        }
    };
    // `Url::set_username` / `set_password` percent-encode for us.
    parsed
        .set_username(user)
        .map_err(|_| Error::other("git: cannot set username on URL"))?;
    parsed
        .set_password(Some(pass))
        .map_err(|_| Error::other("git: cannot set password on URL"))?;
    Ok(parsed.to_string())
}

/// Quote an argument for `/bin/sh -c`. Uses single-quote wrapping with the
/// classic `'\''` escape so any character (including `$`, backticks,
/// spaces) is passed through literally to `git`.
fn shell_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for ch in s.chars() {
        if ch == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(ch);
        }
    }
    out.push('\'');
    out
}

fn shell_quote_path(p: &Path) -> String {
    shell_quote(&p.to_string_lossy())
}

/// Same as [`shell_quote`] but used in the reset command where we want the
/// `origin/` prefix outside the quoted segment. Kept as a thin alias so the
/// intent is obvious at the call site.
fn shell_arg_with_prefix(s: &str) -> String {
    shell_quote(s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::console::RecordingConsole;
    use crate::vfs::MemVfs;

    fn rc() -> (MemVfs, RecordingConsole) {
        (MemVfs::new(), RecordingConsole::new())
    }

    // ---- no-op ----

    #[test]
    fn empty_git_is_noop() {
        let (fs, console) = rc();
        let stage = Stage::default();
        run(&stage, &fs, &console).expect("empty git -> Ok");
        assert!(console.commands().is_empty());
    }

    #[test]
    fn empty_url_is_noop() {
        let (fs, console) = rc();
        let stage = Stage {
            git: Git {
                url: "".into(),
                path: "/tmp/x".into(),
                ..Default::default()
            },
            ..Default::default()
        };
        run(&stage, &fs, &console).expect("empty url -> Ok");
        assert!(console.commands().is_empty());
    }

    #[test]
    fn empty_path_errors() {
        let (fs, console) = rc();
        let stage = Stage {
            git: Git {
                url: "https://example.com/foo.git".into(),
                path: "".into(),
                ..Default::default()
            },
            ..Default::default()
        };
        let err = run(&stage, &fs, &console).expect_err("empty path -> err");
        match err {
            Error::Other(msg) => assert!(msg.contains("empty path")),
            other => panic!("expected Error::Other, got {other:?}"),
        }
    }

    // ---- fresh clone ----

    #[test]
    fn fresh_clone_issues_clone_with_default_branch() {
        let (fs, console) = rc();
        let stage = Stage {
            git: Git {
                url: "https://example.com/foo.git".into(),
                path: "/srv/foo".into(),
                ..Default::default()
            },
            ..Default::default()
        };
        run(&stage, &fs, &console).expect("clone");
        let cmds = console.commands();
        assert_eq!(cmds.len(), 1, "exactly one git invocation");
        let c = &cmds[0];
        assert!(c.starts_with("git clone"), "got: {c}");
        assert!(c.contains("--branch 'master'"), "default branch=master: {c}");
        assert!(c.contains("'https://example.com/foo.git'"), "url: {c}");
        assert!(c.contains("'/srv/foo'"), "path: {c}");
        assert!(!c.contains("--single-branch"), "default branch_only=false: {c}");
    }

    #[test]
    fn fresh_clone_with_branch() {
        let (fs, console) = rc();
        let stage = Stage {
            git: Git {
                url: "https://example.com/foo.git".into(),
                path: "/srv/foo".into(),
                branch: "main".into(),
                ..Default::default()
            },
            ..Default::default()
        };
        run(&stage, &fs, &console).expect("clone");
        let cmds = console.commands();
        assert_eq!(cmds.len(), 1);
        assert!(cmds[0].contains("--branch 'main'"), "got: {}", cmds[0]);
    }

    #[test]
    fn fresh_clone_with_branch_only_adds_single_branch() {
        let (fs, console) = rc();
        let stage = Stage {
            git: Git {
                url: "https://example.com/foo.git".into(),
                path: "/srv/foo".into(),
                branch: "main".into(),
                branch_only: true,
                ..Default::default()
            },
            ..Default::default()
        };
        run(&stage, &fs, &console).expect("clone");
        let cmds = console.commands();
        assert_eq!(cmds.len(), 1);
        assert!(cmds[0].contains("--single-branch"), "got: {}", cmds[0]);
    }

    #[test]
    fn fresh_clone_creates_parent_dir() {
        let (fs, console) = rc();
        let stage = Stage {
            git: Git {
                url: "https://example.com/foo.git".into(),
                path: "/deep/nested/dir/foo".into(),
                ..Default::default()
            },
            ..Default::default()
        };
        run(&stage, &fs, &console).expect("clone");
        // Vfs should have the parent dir.
        let meta = fs
            .metadata(Path::new("/deep/nested/dir"))
            .expect("parent dir exists");
        assert!(meta.is_dir);
    }

    // ---- update existing ----

    #[test]
    fn existing_repo_runs_fetch_and_reset() {
        let (fs, console) = rc();
        // Prime the MemVfs to mark "/srv/foo/.git" as existing.
        fs.write(Path::new("/srv/foo/.git/HEAD"), b"ref: refs/heads/master\n")
            .expect("seed .git");

        let stage = Stage {
            git: Git {
                url: "https://example.com/foo.git".into(),
                path: "/srv/foo".into(),
                ..Default::default()
            },
            ..Default::default()
        };
        run(&stage, &fs, &console).expect("update");
        let cmds = console.commands();
        // No checkout because branch_only=false.
        assert_eq!(cmds.len(), 2, "fetch + reset only, got {cmds:?}");
        assert!(cmds[0].contains("git -C '/srv/foo' fetch origin 'master'"), "fetch: {}", cmds[0]);
        assert!(cmds[1].contains("git -C '/srv/foo' reset --hard origin/'master'"), "reset: {}", cmds[1]);
    }

    #[test]
    fn existing_repo_with_branch_only_also_checks_out() {
        let (fs, console) = rc();
        fs.write(Path::new("/srv/foo/.git/HEAD"), b"x")
            .expect("seed .git");

        let stage = Stage {
            git: Git {
                url: "https://example.com/foo.git".into(),
                path: "/srv/foo".into(),
                branch: "main".into(),
                branch_only: true,
                ..Default::default()
            },
            ..Default::default()
        };
        run(&stage, &fs, &console).expect("update");
        let cmds = console.commands();
        assert_eq!(cmds.len(), 3, "fetch + reset + checkout");
        assert!(cmds[0].contains("fetch origin 'main'"));
        assert!(cmds[1].contains("reset --hard origin/'main'"));
        assert!(cmds[2].contains("git -C '/srv/foo' checkout 'main'"));
    }

    #[test]
    fn fetch_failure_bubbles_as_cmd_error() {
        let (fs, console) = rc();
        fs.write(Path::new("/srv/foo/.git/HEAD"), b"x").unwrap();

        // Match the actual command the plugin will issue.
        let fetch_cmd = "git -C '/srv/foo' fetch origin 'master'";
        console.expect(fetch_cmd, Err("network down".into()));

        let stage = Stage {
            git: Git {
                url: "https://example.com/foo.git".into(),
                path: "/srv/foo".into(),
                ..Default::default()
            },
            ..Default::default()
        };
        let err = run(&stage, &fs, &console).expect_err("fetch fails");
        match err {
            Error::Cmd { stderr, .. } => assert_eq!(stderr, "network down"),
            other => panic!("expected Error::Cmd, got {other:?}"),
        }
    }

    // ---- auth: username/password ----

    #[test]
    fn basic_auth_embeds_credentials_in_url() {
        let (fs, console) = rc();
        let stage = Stage {
            git: Git {
                url: "https://example.com/foo.git".into(),
                path: "/srv/foo".into(),
                auth: Auth {
                    username: "alice".into(),
                    password: "hunter2".into(),
                    ..Default::default()
                },
                ..Default::default()
            },
            ..Default::default()
        };
        run(&stage, &fs, &console).expect("clone");
        let cmds = console.commands();
        assert_eq!(cmds.len(), 1);
        assert!(
            cmds[0].contains("https://alice:hunter2@example.com/foo.git"),
            "embedded creds in URL: {}",
            cmds[0]
        );
    }

    #[test]
    fn basic_auth_percent_encodes_special_chars() {
        let (fs, console) = rc();
        let stage = Stage {
            git: Git {
                url: "https://example.com/foo.git".into(),
                path: "/srv/foo".into(),
                auth: Auth {
                    username: "alice".into(),
                    // `@` must be percent-encoded so it doesn't split the URL.
                    password: "p@ss/word".into(),
                    ..Default::default()
                },
                ..Default::default()
            },
            ..Default::default()
        };
        run(&stage, &fs, &console).expect("clone");
        let cmds = console.commands();
        // `@` -> %40, `/` -> %2F
        assert!(
            cmds[0].contains("alice:p%40ss%2Fword@example.com"),
            "percent-encoded: {}",
            cmds[0]
        );
    }

    // ---- auth: ssh private key ----

    #[test]
    fn private_key_sets_git_ssh_command() {
        let (fs, console) = rc();
        let stage = Stage {
            git: Git {
                url: "git@example.com:foo/bar.git".into(),
                path: "/srv/foo".into(),
                auth: Auth {
                    private_key: "-----BEGIN KEY-----\nabcdef\n-----END KEY-----".into(),
                    ..Default::default()
                },
                ..Default::default()
            },
            ..Default::default()
        };
        run(&stage, &fs, &console).expect("clone");
        let cmds = console.commands();
        assert_eq!(cmds.len(), 1);
        assert!(
            cmds[0].starts_with("GIT_SSH_COMMAND="),
            "env prefix present: {}",
            cmds[0]
        );
        assert!(cmds[0].contains("ssh -i "), "ssh -i flag: {}", cmds[0]);
        // We don't pin the tempfile path, but it should be somewhere under /tmp
        // (or platform tempdir) and present as an arg to ssh.
        assert!(
            !cmds[0].contains("StrictHostKeyChecking=no"),
            "insecure off by default"
        );
    }

    #[test]
    fn private_key_insecure_adds_strict_host_key_no() {
        let (fs, console) = rc();
        let stage = Stage {
            git: Git {
                url: "git@example.com:foo/bar.git".into(),
                path: "/srv/foo".into(),
                auth: Auth {
                    private_key: "KEY".into(),
                    insecure: true,
                    ..Default::default()
                },
                ..Default::default()
            },
            ..Default::default()
        };
        run(&stage, &fs, &console).expect("clone");
        let cmds = console.commands();
        assert_eq!(cmds.len(), 1);
        assert!(
            cmds[0].contains("StrictHostKeyChecking=no"),
            "insecure mode: {}",
            cmds[0]
        );
    }

    #[test]
    fn private_key_overrides_basic_auth() {
        // If both are set, SSH key wins (URL is left alone, no embedded creds).
        let (fs, console) = rc();
        let stage = Stage {
            git: Git {
                url: "https://example.com/foo.git".into(),
                path: "/srv/foo".into(),
                auth: Auth {
                    username: "alice".into(),
                    password: "hunter2".into(),
                    private_key: "KEY".into(),
                    ..Default::default()
                },
                ..Default::default()
            },
            ..Default::default()
        };
        run(&stage, &fs, &console).expect("clone");
        let cmds = console.commands();
        assert!(
            !cmds[0].contains("alice:hunter2"),
            "should not embed creds when key is set: {}",
            cmds[0]
        );
        assert!(
            cmds[0].contains("GIT_SSH_COMMAND="),
            "ssh env set: {}",
            cmds[0]
        );
    }

    // ---- shell_quote ----

    #[test]
    fn shell_quote_wraps_and_escapes() {
        assert_eq!(shell_quote("foo"), "'foo'");
        assert_eq!(shell_quote("foo bar"), "'foo bar'");
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
        assert_eq!(shell_quote("$HOME"), "'$HOME'");
    }

    // ---- integration: actual clone of a public repo ----

    /// Online smoke test. Disabled by default (`#[ignore]`); run with
    /// `cargo test -- --ignored git_online_clone`. Uses [`RealVfs`] so that
    /// the path the plugin shells `git clone` to is the same path the test
    /// reads from afterwards.
    #[test]
    #[ignore = "online: hits gist.github.com"]
    fn git_online_clone() {
        use crate::console::StandardConsole;
        use crate::vfs::RealVfs;

        let tmp = tempfile::tempdir().expect("tempdir");
        let dst = tmp.path().join("repo");
        let stage = Stage {
            git: Git {
                url: "https://gist.github.com/mudler/13d2c42fd2cf7fc33cdb8cae6b5bdd57".into(),
                path: dst.to_string_lossy().into_owned(),
                ..Default::default()
            },
            ..Default::default()
        };
        let fs = RealVfs::new();
        let console = StandardConsole::new();
        run(&stage, &fs, &console).expect("online clone");
        let unittest = dst.join("unittest.txt");
        let body = std::fs::read_to_string(&unittest).expect("read cloned file");
        assert_eq!(body.trim(), "test");
    }
}
