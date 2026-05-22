//! `ssh` plugin — port of `pkg/plugins/ssh.go`.
//!
//! Materialises `stage.authorized_keys` (Go: `SSHKeys`) into per-user
//! `~/.ssh/authorized_keys` files. Each value in the map is a list of key
//! specs; a spec is either:
//!   - `github:USERNAME`  → fetch from `https://github.com/USERNAME.keys`
//!   - `gitlab:USERNAME`  → fetch from `https://gitlab.com/USERNAME.keys`
//!   - `http://...` / `https://...` → GET that URL
//!   - anything else → treated as a raw `ssh-rsa AAAA...` line verbatim
//!
//! Resolved keys are written to `<home>/.ssh/authorized_keys` (mode 0600,
//! owned by the target user), creating `<home>/.ssh` (mode 0700) if needed.
//! If the file already exists we APPEND, deduping by exact line match —
//! never clobber existing keys. Home directory + uid/gid come from parsing
//! `/etc/passwd` via the [`Vfs`] (i.e. respects the test FS).
//!
//! HTTP fetch failures are logged at WARN and skipped — other keys for the
//! same user still proceed. Per-user failures are aggregated into
//! [`Error::Multi`]; the loop never aborts early.
//!
//! Test seam: in unit tests we override the `github:` / `gitlab:`
//! base URLs via the `YIP_SSH_GITHUB_URL_TEMPLATE` /
//! `YIP_SSH_GITLAB_URL_TEMPLATE` env vars so `mockito` can stand in for
//! the real endpoints. In production these are never set and the
//! hardcoded URLs are used.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tracing::{debug, info, warn};

use crate::console::Console;
use crate::error::{Error, Result};
use crate::executor::Plugin;
use crate::schema::Stage;
use crate::vfs::Vfs;

const SSH_DIR: &str = ".ssh";
const AUTHORIZED_FILE: &str = "authorized_keys";
const PASSWD_FILE: &str = "/etc/passwd";

const DEFAULT_GITHUB_URL: &str = "https://github.com/%s.keys";
const DEFAULT_GITLAB_URL: &str = "https://gitlab.com/%s.keys";

/// Build a [`Plugin`] arc-closure.
pub fn build() -> Plugin {
    Arc::new(run)
}

/// Pure entry point — exposed so tests don't have to go through `Arc`.
pub fn run(stage: &Stage, fs: &dyn Vfs, _console: &dyn Console) -> Result<()> {
    if stage.ssh_keys.is_empty() {
        return Ok(());
    }

    info!(users = stage.ssh_keys.len(), "configuring authorized_keys");

    let mut errs: Vec<Error> = Vec::new();
    // Iterate in a stable (sorted) order so behaviour is deterministic
    // even though HashMap iteration is randomised.
    let mut users: Vec<&String> = stage.ssh_keys.keys().collect();
    users.sort();
    for user in users {
        let keys = &stage.ssh_keys[user];
        if let Err(e) = ensure_keys(user, keys, fs) {
            warn!(user = %user, error = %e, "failed configuring authorized_keys for user");
            errs.push(e);
        }
    }

    match errs.len() {
        0 => Ok(()),
        _ => Err(Error::Multi(errs)),
    }
}

/// Per-user authorized_keys handling. Looks up the user in /etc/passwd
/// (via `fs`), resolves each key spec, then appends new keys to the
/// user's authorized_keys file (mode 0600) inside `~/.ssh` (mode 0700).
fn ensure_keys(user: &str, keys: &[String], fs: &dyn Vfs) -> Result<()> {
    let passwd = match fs.read_to_string(Path::new(PASSWD_FILE)) {
        Ok(s) => s,
        Err(e) => {
            // Match Go's behaviour: a failure to parse /etc/passwd bubbles
            // up as a single error for this user. Tests with no /etc/passwd
            // entry at all exercise the "user not found" path below.
            return Err(Error::other(format!("failed reading {PASSWD_FILE}: {e}")));
        }
    };

    let Some(entry) = lookup_user(&passwd, user) else {
        // User absent → warn and skip silently (per spec for the
        // "User without /etc/passwd entry" test case). We return Ok so
        // the executor doesn't accumulate a hard error for what's really
        // a config-vs-system mismatch.
        warn!(user = %user, "user not found in /etc/passwd; skipping");
        return Ok(());
    };

    let uid = entry.uid;
    let gid = entry.gid;
    let home = entry.home;

    // Resolve every key spec up front; HTTP failures are warned + dropped
    // rather than aborting (other keys for the same user still apply).
    let mut resolved: Vec<String> = Vec::new();
    for spec in keys {
        match resolve_key(spec) {
            Ok(lines) => {
                for line in lines {
                    let trimmed = line.trim();
                    if trimmed.is_empty() {
                        continue;
                    }
                    resolved.push(trimmed.to_string());
                }
            }
            Err(e) => {
                warn!(user = %user, spec = %spec, error = %e, "failed resolving ssh key spec");
            }
        }
    }

    let ssh_dir = PathBuf::from(&home).join(SSH_DIR);
    if !fs.exists(&ssh_dir) {
        fs.mkdir_all(&ssh_dir)?;
    }
    // 0700 dir, owned by user.
    fs.chmod(&ssh_dir, 0o700)?;
    fs.chown(&ssh_dir, uid as i32, gid as i32)?;

    let auth_file = ssh_dir.join(AUTHORIZED_FILE);

    // Read existing content (if any) so we can dedupe by exact line match.
    let existing = if fs.exists(&auth_file) {
        fs.read_to_string(&auth_file).unwrap_or_default()
    } else {
        String::new()
    };

    let mut seen: HashSet<String> = HashSet::new();
    for line in existing.lines() {
        let t = line.trim();
        if !t.is_empty() {
            seen.insert(t.to_string());
        }
    }

    let mut out = existing.clone();
    // Make sure the existing content ends with a newline before we append
    // (a hand-written file may not). Skip when empty so we don't write a
    // bare "\n".
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }

    let mut appended = 0usize;
    for key in &resolved {
        if seen.contains(key) {
            continue;
        }
        seen.insert(key.clone());
        out.push_str(key);
        out.push('\n');
        appended += 1;
    }

    debug!(
        user = %user,
        file = %auth_file.display(),
        appended,
        total = seen.len(),
        "writing authorized_keys",
    );

    fs.write(&auth_file, out.as_bytes())?;
    fs.chmod(&auth_file, 0o600)?;
    fs.chown(&auth_file, uid as i32, gid as i32)?;

    Ok(())
}

/// Resolve a single key spec into one or more raw authorized_keys lines.
/// Returns the spec verbatim (as one line) when it isn't a URL or a
/// known `provider:user` form.
fn resolve_key(spec: &str) -> Result<Vec<String>> {
    let trimmed = spec.trim();
    if trimmed.is_empty() {
        return Ok(Vec::new());
    }

    // provider:user shorthand.
    if let Some(rest) = trimmed.strip_prefix("github:") {
        let tmpl = std::env::var("YIP_SSH_GITHUB_URL_TEMPLATE")
            .unwrap_or_else(|_| DEFAULT_GITHUB_URL.to_string());
        let url = tmpl.replacen("%s", rest, 1);
        return fetch_http(&url);
    }
    if let Some(rest) = trimmed.strip_prefix("gitlab:") {
        let tmpl = std::env::var("YIP_SSH_GITLAB_URL_TEMPLATE")
            .unwrap_or_else(|_| DEFAULT_GITLAB_URL.to_string());
        let url = tmpl.replacen("%s", rest, 1);
        return fetch_http(&url);
    }

    // Direct URL.
    if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
        return fetch_http(trimmed);
    }

    // Raw key line — pass through verbatim. Multi-line raw values are
    // split into one entry per line so they get individually deduped.
    Ok(trimmed.lines().map(|l| l.to_string()).collect())
}

/// Blocking HTTP GET via `reqwest`. Body is split on newlines so callers
/// can dedupe per-line; empty lines are dropped upstream.
fn fetch_http(url: &str) -> Result<Vec<String>> {
    debug!(url, "fetching ssh keys via http");
    let resp = reqwest::blocking::get(url)
        .map_err(|e| Error::other(format!("http get {url}: {e}")))?;
    let status = resp.status();
    if !status.is_success() {
        return Err(Error::other(format!("http get {url}: status {status}")));
    }
    let body = resp
        .text()
        .map_err(|e| Error::other(format!("http body {url}: {e}")))?;
    Ok(body.lines().map(|l| l.to_string()).collect())
}

/// Subset of a passwd entry that the plugin needs.
struct PasswdEntry {
    uid: u32,
    gid: u32,
    home: String,
}

/// Parse `/etc/passwd` text and look up a user by name. Mirrors what
/// `mauromorales/xpasswd` does in Go: a colon-separated line of
/// `name:passwd:uid:gid:gecos:home:shell` — we only care about the
/// first six fields and skip malformed lines silently.
fn lookup_user(passwd: &str, user: &str) -> Option<PasswdEntry> {
    for raw in passwd.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut parts = line.splitn(7, ':');
        let name = parts.next()?;
        let _password = parts.next()?;
        let uid_s = parts.next()?;
        let gid_s = parts.next()?;
        let _gecos = parts.next()?;
        let home = parts.next()?;
        if name != user {
            continue;
        }
        let uid: u32 = uid_s.parse().ok()?;
        let gid: u32 = gid_s.parse().ok()?;
        return Some(PasswdEntry {
            uid,
            gid,
            home: home.to_string(),
        });
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::console::RecordingConsole;
    use crate::vfs::MemVfs;
    use std::sync::Mutex;

    // `std::env::set_var` mutates a process-global; serialise tests that
    // touch the github/gitlab template envs to keep them deterministic.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn mem_with_passwd(entries: &[(&str, u32, u32, &str)]) -> MemVfs {
        let mut body = String::new();
        for (name, uid, gid, home) in entries {
            body.push_str(&format!("{name}:x:{uid}:{gid}:{name}:{home}:/bin/sh\n"));
        }
        let fs = MemVfs::new();
        fs.write(Path::new(PASSWD_FILE), body.as_bytes()).unwrap();
        // Pre-create the home dir so chown on it doesn't fail in MemVfs
        // (chown requires the path to exist; mkdir_all in ensure_keys
        // takes care of `.ssh`, but the parent home itself isn't created
        // by the plugin — Go relies on the host already having it).
        for (_, _, _, home) in entries {
            fs.mkdir_all(Path::new(home)).unwrap();
        }
        fs
    }

    fn stage_with_keys(user: &str, keys: &[&str]) -> Stage {
        let mut s = Stage::default();
        s.ssh_keys
            .insert(user.to_string(), keys.iter().map(|k| k.to_string()).collect());
        s
    }

    #[test]
    fn plain_key_written_verbatim() {
        let fs = mem_with_passwd(&[("foo", 1000, 100, "/home/foo")]);
        let console = RecordingConsole::new();
        let stage = stage_with_keys("foo", &["ssh-rsa AAAA-plain-key user@host"]);
        run(&stage, &fs, &console).expect("run ok");

        let got = fs
            .read_to_string(Path::new("/home/foo/.ssh/authorized_keys"))
            .expect("auth file present");
        assert_eq!(got, "ssh-rsa AAAA-plain-key user@host\n");

        // .ssh dir must be 0700 + owned by foo.
        let dir_md = fs.metadata(Path::new("/home/foo/.ssh")).unwrap();
        assert_eq!(dir_md.mode & 0o777, 0o700);
        assert_eq!((dir_md.uid, dir_md.gid), (1000, 100));

        // Auth file must be 0600 + owned by foo.
        let file_md = fs
            .metadata(Path::new("/home/foo/.ssh/authorized_keys"))
            .unwrap();
        assert_eq!(file_md.mode & 0o777, 0o600);
        assert_eq!((file_md.uid, file_md.gid), (1000, 100));
    }

    #[test]
    fn github_prefix_fetches_and_writes() {
        let _g = ENV_LOCK.lock().unwrap();
        let mut server = mockito::Server::new();
        let body = "ssh-rsa AAAA-bob-key-from-github bob@hub\n";
        let m = server
            .mock("GET", "/bob.keys")
            .with_status(200)
            .with_body(body)
            .create();

        // Point the github template at the mock.
        let template = format!("{}/%s.keys", server.url());
        std::env::set_var("YIP_SSH_GITHUB_URL_TEMPLATE", &template);

        let fs = mem_with_passwd(&[("bob", 1001, 1001, "/home/bob")]);
        let console = RecordingConsole::new();
        let stage = stage_with_keys("bob", &["github:bob"]);
        let res = run(&stage, &fs, &console);

        std::env::remove_var("YIP_SSH_GITHUB_URL_TEMPLATE");
        res.expect("run ok");
        m.assert();

        let got = fs
            .read_to_string(Path::new("/home/bob/.ssh/authorized_keys"))
            .expect("auth file present");
        assert!(
            got.contains("ssh-rsa AAAA-bob-key-from-github bob@hub"),
            "expected fetched key in {got:?}",
        );
    }

    #[test]
    fn multiple_keys_for_one_user_all_written_and_deduped() {
        // Three keys provided, but with one duplicate. Final file should
        // contain exactly the three distinct lines.
        let fs = mem_with_passwd(&[("alice", 1002, 1002, "/home/alice")]);
        let console = RecordingConsole::new();
        let stage = stage_with_keys(
            "alice",
            &[
                "ssh-rsa AAAA-key-1 alice@one",
                "ssh-rsa AAAA-key-2 alice@two",
                "ssh-rsa AAAA-key-1 alice@one", // exact dup
                "ssh-rsa AAAA-key-3 alice@three",
            ],
        );
        run(&stage, &fs, &console).expect("run ok");

        let got = fs
            .read_to_string(Path::new("/home/alice/.ssh/authorized_keys"))
            .unwrap();
        let mut lines: Vec<&str> = got.lines().filter(|l| !l.is_empty()).collect();
        lines.sort();
        assert_eq!(
            lines,
            vec![
                "ssh-rsa AAAA-key-1 alice@one",
                "ssh-rsa AAAA-key-2 alice@two",
                "ssh-rsa AAAA-key-3 alice@three",
            ],
        );
    }

    #[test]
    fn rerunning_with_same_keys_does_not_duplicate() {
        let fs = mem_with_passwd(&[("charlie", 1003, 1003, "/home/charlie")]);
        let console = RecordingConsole::new();
        let stage =
            stage_with_keys("charlie", &["ssh-rsa AAAA-only-key charlie@host"]);

        run(&stage, &fs, &console).expect("first run ok");
        run(&stage, &fs, &console).expect("second run ok");

        let got = fs
            .read_to_string(Path::new("/home/charlie/.ssh/authorized_keys"))
            .unwrap();
        let count = got
            .lines()
            .filter(|l| *l == "ssh-rsa AAAA-only-key charlie@host")
            .count();
        assert_eq!(count, 1, "expected exactly one line, got {got:?}");
    }

    #[test]
    fn user_without_passwd_entry_is_skipped() {
        // No matching user → plugin logs a warn and returns Ok.
        let fs = mem_with_passwd(&[("someone-else", 1, 1, "/home/someone-else")]);
        let console = RecordingConsole::new();
        let stage = stage_with_keys("ghost", &["ssh-rsa AAAA-ghost ghost@nowhere"]);
        run(&stage, &fs, &console).expect("missing-user is non-fatal");

        // Nothing should have been written under any /home/ghost path.
        assert!(!fs.exists(Path::new("/home/ghost/.ssh/authorized_keys")));
    }

    #[test]
    fn http_fetch_failure_warns_and_continues() {
        let _g = ENV_LOCK.lock().unwrap();
        let mut server = mockito::Server::new();
        // Mock returns 500 — the plugin should warn and move on, still
        // writing the *other* (plain) key.
        let m = server
            .mock("GET", "/dave.keys")
            .with_status(500)
            .with_body("boom")
            .create();
        let template = format!("{}/%s.keys", server.url());
        std::env::set_var("YIP_SSH_GITHUB_URL_TEMPLATE", &template);

        let fs = mem_with_passwd(&[("dave", 1004, 1004, "/home/dave")]);
        let console = RecordingConsole::new();
        let stage = stage_with_keys(
            "dave",
            &["github:dave", "ssh-rsa AAAA-plain-fallback dave@host"],
        );
        let res = run(&stage, &fs, &console);
        std::env::remove_var("YIP_SSH_GITHUB_URL_TEMPLATE");

        // Whole stage still succeeds (HTTP errors are warn-and-skip).
        res.expect("run ok");
        m.assert();

        let got = fs
            .read_to_string(Path::new("/home/dave/.ssh/authorized_keys"))
            .unwrap();
        assert!(got.contains("ssh-rsa AAAA-plain-fallback dave@host"));
        assert!(!got.contains("boom"), "errored body must not leak: {got:?}");
    }

    #[test]
    fn raw_http_url_is_fetched() {
        let mut server = mockito::Server::new();
        let body = "ssh-rsa AAAA-direct-url-key eve@host\n";
        let m = server
            .mock("GET", "/eve-keys")
            .with_status(200)
            .with_body(body)
            .create();
        let url = format!("{}/eve-keys", server.url());

        let fs = mem_with_passwd(&[("eve", 1005, 1005, "/home/eve")]);
        let console = RecordingConsole::new();
        let stage = stage_with_keys("eve", &[&url]);
        run(&stage, &fs, &console).expect("run ok");
        m.assert();

        let got = fs
            .read_to_string(Path::new("/home/eve/.ssh/authorized_keys"))
            .unwrap();
        assert!(got.contains("ssh-rsa AAAA-direct-url-key eve@host"));
    }

    #[test]
    fn empty_ssh_keys_is_noop() {
        let fs = MemVfs::new();
        let console = RecordingConsole::new();
        run(&Stage::default(), &fs, &console).expect("noop ok");
        assert!(!fs.exists(Path::new(PASSWD_FILE)));
    }

    #[test]
    fn lookup_user_parses_typical_passwd_line() {
        let body =
            "root:x:0:0:root:/root:/bin/bash\nfoo:x:1000:100:foo gecos:/home/foo:/bin/zsh\n";
        let got = lookup_user(body, "foo").expect("present");
        assert_eq!(got.uid, 1000);
        assert_eq!(got.gid, 100);
        assert_eq!(got.home, "/home/foo");
        assert!(lookup_user(body, "missing").is_none());
    }

    #[test]
    fn build_returns_callable_plugin() {
        let fs = mem_with_passwd(&[("xy", 1, 1, "/home/xy")]);
        let console = RecordingConsole::new();
        let stage = stage_with_keys("xy", &["ssh-rsa AAAA-build-test xy@host"]);
        let plugin = build();
        plugin(&stage, &fs, &console).expect("plugin closure ok");
        assert!(fs.exists(Path::new("/home/xy/.ssh/authorized_keys")));
    }
}
