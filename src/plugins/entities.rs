//! `entities` / `delete_entities` plugin — mutate `/etc/passwd`-style files.
//!
//! Port of `pkg/plugins/entities.go` (which delegates to `mudler/entities`).
//! Each [`YipEntity`] holds:
//!
//! - `path` — the target file to mutate (e.g. `/etc/passwd`, `/etc/group`,
//!   `/etc/shadow`, `/etc/gshadow`). When empty, a sensible default is picked
//!   from `kind` (matching `UserDefault`, `GroupsDefault`, `ShadowDefault`,
//!   `GShadowDefault` in the Go library).
//! - `entity` — a YAML document describing the entity. Format mirrors
//!   `mudler/entities`:
//!
//!   ```yaml
//!   kind: user        # or "group", "shadow", "gshadow"
//!   username: alice   # group uses `group_name`, gshadow uses `name`
//!   password: x
//!   uid: 1000
//!   gid: 1000
//!   info: "Alice"
//!   homedir: /home/alice
//!   shell: /bin/bash
//!   ```
//!
//! The plugin parses the YAML, renders it to a single colon-separated line,
//! reads the target file from the [`Vfs`], and either:
//!
//! - **ensure** — replace any existing line keyed by the entity's identifier
//!   (username / group name), or append if missing;
//! - **delete** — remove the matching line.
//!
//! Errors are aggregated into [`Error::Multi`] so one bad entry doesn't abort
//! the rest, matching the Go multierror semantics.
//!
//! Note: the Go plugin runs the `entity` string through `templateSysData`
//! (Sprig templating). The Rust port leaves templating to a higher layer for
//! now — the entity body is consumed verbatim.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use nix::fcntl::{Flock, FlockArg};
use serde::Deserialize;
use tracing::{debug, info, warn};

use crate::console::Console;
use crate::error::{Error, Result};
use crate::executor::Plugin;
use crate::schema::Stage;
use crate::schema::user::YipEntity;
use crate::vfs::Vfs;

// ---------------------------------------------------------------------------
// Plugin entry points
// ---------------------------------------------------------------------------

/// Build a [`Plugin`] arc-closure for the **ensure_entities** action.
pub fn build() -> Plugin {
    Arc::new(run)
}

/// `ensure_entities` plugin body.
pub fn run(stage: &Stage, fs: &dyn Vfs, _console: &dyn Console) -> Result<()> {
    if stage.ensure_entities.is_empty() {
        return Ok(());
    }
    info!(count = stage.ensure_entities.len(), "ensuring entities");

    let mut errs: Vec<Error> = Vec::new();
    for e in &stage.ensure_entities {
        let target = ParsedEntity::target_for_yip_entity(e);
        let res = with_passwd_lock(target.as_deref(), || ensure_one(fs, e));
        if let Err(err) = res {
            warn!(path = %e.path, error = %err, "ensure entity failed");
            errs.push(err);
        }
    }
    match errs.len() {
        0 => Ok(()),
        _ => Err(Error::Multi(errs)),
    }
}

/// Build a [`Plugin`] arc-closure for the **delete_entities** action.
pub fn build_delete() -> Plugin {
    Arc::new(run_delete)
}

/// `delete_entities` plugin body.
pub fn run_delete(stage: &Stage, fs: &dyn Vfs, _console: &dyn Console) -> Result<()> {
    if stage.delete_entities.is_empty() {
        return Ok(());
    }
    info!(count = stage.delete_entities.len(), "deleting entities");

    let mut errs: Vec<Error> = Vec::new();
    for e in &stage.delete_entities {
        let target = ParsedEntity::target_for_yip_entity(e);
        let res = with_passwd_lock(target.as_deref(), || delete_one(fs, e));
        if let Err(err) = res {
            warn!(path = %e.path, error = %err, "delete entity failed");
            errs.push(err);
        }
    }
    match errs.len() {
        0 => Ok(()),
        _ => Err(Error::Multi(errs)),
    }
}

// ---------------------------------------------------------------------------
// File locking
// ---------------------------------------------------------------------------

/// Serialise concurrent edits to `/etc/passwd`-style files via an advisory
/// `flock` on a sibling `.yip-lock` file. Prevents corruption when immucore
/// and a manual `useradd` (or two parallel yip executors) race to mutate the
/// same passwd/shadow file.
///
/// The lock file is created on the host filesystem regardless of the Vfs in
/// play. This is intentional: we want the lock to be visible to other
/// processes on the real system. For unit tests using `MemVfs` the lock file
/// lives in the real `/tmp`-style path the test picks (or fails to create —
/// in which case we fall through to a warn-and-proceed path, matching Go
/// yip's best-effort locking).
///
/// `target` is the resolved target path (e.g. `/etc/passwd`). When `None`
/// (entity body could not be parsed), the lock step is skipped and the
/// closure runs unguarded — `ensure_one` will then surface the parse error.
fn with_passwd_lock<F>(target: Option<&Path>, f: F) -> Result<()>
where
    F: FnOnce() -> Result<()>,
{
    let Some(target) = target else {
        return f();
    };
    let lock_path = lock_file_for(target);

    let lock_file = match std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&lock_path)
    {
        Ok(lf) => lf,
        Err(e) => {
            // Lock file unobtainable (e.g. /etc not writable, ro-fs, MemVfs
            // test environment). Proceed unlocked — matches Go yip's
            // best-effort posture. The mutating work itself will surface
            // any real permission problems.
            warn!(
                lock = %lock_path.display(),
                error = %e,
                "entities: could not open lock file, proceeding without flock",
            );
            return f();
        }
    };

    let flock = match Flock::lock(lock_file, FlockArg::LockExclusive) {
        Ok(g) => Some(g),
        Err((file, e)) => {
            warn!(
                lock = %lock_path.display(),
                error = %e,
                "entities: flock(LOCK_EX) failed, proceeding without lock",
            );
            // File handle preserved so the path still gets removed below.
            drop(file);
            None
        }
    };

    let result = f();

    // Drop the lock explicitly so a subsequent caller in the same process
    // doesn't have to wait on the guard's Drop.
    if let Some(g) = flock {
        let _ = g.unlock();
    }
    // Best-effort cleanup; the lock file may legitimately still be held by
    // another waiter, in which case remove() will fail silently.
    let _ = std::fs::remove_file(&lock_path);
    result
}

/// Sibling lock-file path for `target` (e.g. `/etc/passwd` →
/// `/etc/passwd.yip-lock`).
fn lock_file_for(target: &Path) -> PathBuf {
    let mut s = target.as_os_str().to_owned();
    s.push(".yip-lock");
    PathBuf::from(s)
}

// ---------------------------------------------------------------------------
// One-entity work
// ---------------------------------------------------------------------------

fn ensure_one(fs: &dyn Vfs, e: &YipEntity) -> Result<()> {
    let parsed = ParsedEntity::from_yaml(&e.entity)?;
    parsed.validate_for_apply()?;

    let target = parsed.resolve_path(&e.path);
    let line = parsed.render_line();
    let key = parsed.identifier();
    debug!(target = %target.display(), key = %key, "ensure entity");

    let existing = read_or_empty(fs, &target)?;
    let new = merge_line(&existing, &key, &line);
    fs.write(&target, new.as_bytes())?;
    Ok(())
}

fn delete_one(fs: &dyn Vfs, e: &YipEntity) -> Result<()> {
    let parsed = ParsedEntity::from_yaml(&e.entity)?;
    let target = parsed.resolve_path(&e.path);
    let line = parsed.render_line();
    debug!(target = %target.display(), line = %line, "delete entity");

    let existing = read_or_empty(fs, &target)?;
    // Go uses `bytes.Replace(input, line+"\n", "", 1)` — first occurrence,
    // exact match (including all fields). We replicate that.
    let needle = format!("{line}\n");
    let new = if let Some(idx) = existing.find(&needle) {
        let mut out = String::with_capacity(existing.len() - needle.len());
        out.push_str(&existing[..idx]);
        out.push_str(&existing[idx + needle.len()..]);
        out
    } else {
        // Not present — Go silently no-ops here too (Replace with count=1).
        existing
    };
    fs.write(&target, new.as_bytes())?;
    Ok(())
}

fn read_or_empty(fs: &dyn Vfs, path: &Path) -> Result<String> {
    if !fs.exists(path) {
        return Ok(String::new());
    }
    fs.read_to_string(path)
}

/// Merge `line` (keyed by `key`) into `existing`, replacing the first matching
/// line if present, otherwise appending. Trailing newline is always preserved.
fn merge_line(existing: &str, key: &str, line: &str) -> String {
    if existing.is_empty() {
        return format!("{line}\n");
    }

    // Track whether the input ended in a newline so we can reproduce it.
    let had_trailing_newline = existing.ends_with('\n');
    let body = if had_trailing_newline {
        &existing[..existing.len() - 1]
    } else {
        existing
    };

    let mut replaced = false;
    let mut out_lines: Vec<String> = body
        .split('\n')
        .map(|l| {
            if !replaced && line_identifier(l) == key {
                replaced = true;
                line.to_string()
            } else {
                l.to_string()
            }
        })
        .collect();

    if !replaced {
        out_lines.push(line.to_string());
    }

    let mut out = out_lines.join("\n");
    out.push('\n');
    out
}

fn line_identifier(line: &str) -> &str {
    line.split(':').next().unwrap_or("")
}

// ---------------------------------------------------------------------------
// Entity parsing / rendering
// ---------------------------------------------------------------------------

/// What was decoded from the `entity:` YAML blob.
#[derive(Debug)]
enum ParsedEntity {
    User(UserPasswd),
    Group(GroupEntity),
    Shadow(ShadowEntity),
    GShadow(GShadowEntity),
}

#[derive(Debug, Default, Deserialize)]
struct Signature {
    #[serde(default)]
    kind: String,
}

#[derive(Debug, Default, Deserialize)]
struct UserPasswd {
    #[serde(default)]
    username: String,
    #[serde(default)]
    password: String,
    #[serde(default)]
    uid: i64,
    #[serde(default)]
    gid: i64,
    #[serde(default)]
    info: String,
    #[serde(default)]
    homedir: String,
    #[serde(default)]
    shell: String,
}

#[derive(Debug, Default, Deserialize)]
struct GroupEntity {
    #[serde(default)]
    group_name: String,
    #[serde(default)]
    password: String,
    #[serde(default)]
    gid: Option<i64>,
    #[serde(default)]
    users: String,
}

#[derive(Debug, Default, Deserialize)]
struct ShadowEntity {
    #[serde(default)]
    username: String,
    #[serde(default)]
    password: String,
    #[serde(default)]
    last_changed: String,
    #[serde(default)]
    minimum_changed: String,
    #[serde(default)]
    maximum_changed: String,
    #[serde(default)]
    warn: String,
    #[serde(default)]
    inactive: String,
    #[serde(default)]
    expire: String,
    #[serde(default)]
    reserved: String,
}

#[derive(Debug, Default, Deserialize)]
struct GShadowEntity {
    #[serde(default)]
    name: String,
    #[serde(default)]
    password: String,
    #[serde(default)]
    administrators: String,
    #[serde(default)]
    members: String,
}

impl ParsedEntity {
    /// Resolve the on-disk target path for `e` without surfacing parse
    /// errors. Returns `None` if the entity YAML doesn't parse (the
    /// downstream `from_yaml` call inside `ensure_one`/`delete_one` will
    /// surface that error in turn — we just skip locking).
    fn target_for_yip_entity(e: &YipEntity) -> Option<PathBuf> {
        match Self::from_yaml(&e.entity) {
            Ok(parsed) => Some(parsed.resolve_path(&e.path)),
            Err(_) => None,
        }
    }

    fn from_yaml(body: &str) -> Result<Self> {
        let sig: Signature = serde_yaml::from_str(body).map_err(Error::from)?;
        match sig.kind.as_str() {
            "user" => {
                let u: UserPasswd = serde_yaml::from_str(body)?;
                Ok(ParsedEntity::User(u))
            }
            "group" => {
                let g: GroupEntity = serde_yaml::from_str(body)?;
                Ok(ParsedEntity::Group(g))
            }
            "shadow" => {
                let s: ShadowEntity = serde_yaml::from_str(body)?;
                Ok(ParsedEntity::Shadow(s))
            }
            "gshadow" => {
                let g: GShadowEntity = serde_yaml::from_str(body)?;
                Ok(ParsedEntity::GShadow(g))
            }
            "" => Err(Error::Schema(
                "entity: missing `kind` field (expected user/group/shadow/gshadow)".into(),
            )),
            other => Err(Error::Schema(format!(
                "entity: unsupported kind `{other}`"
            ))),
        }
    }

    /// Equivalent of Go `Apply()` precondition: `Username` (or `group_name` /
    /// `name`) must be non-empty for ensure. Delete tolerates empty
    /// identifiers (matches Go: it'd just no-op since no line matches).
    fn validate_for_apply(&self) -> Result<()> {
        match self {
            ParsedEntity::User(u) if u.username.is_empty() => {
                Err(Error::Schema("entity: empty `username` field".into()))
            }
            ParsedEntity::Group(g) if g.group_name.is_empty() => {
                Err(Error::Schema("entity: empty `group_name` field".into()))
            }
            ParsedEntity::Shadow(s) if s.username.is_empty() => {
                Err(Error::Schema("entity: empty shadow `username` field".into()))
            }
            ParsedEntity::GShadow(g) if g.name.is_empty() => {
                Err(Error::Schema("entity: empty gshadow `name` field".into()))
            }
            _ => Ok(()),
        }
    }

    fn identifier(&self) -> String {
        match self {
            ParsedEntity::User(u) => u.username.clone(),
            ParsedEntity::Group(g) => g.group_name.clone(),
            ParsedEntity::Shadow(s) => s.username.clone(),
            ParsedEntity::GShadow(g) => g.name.clone(),
        }
    }

    /// Mirror of Go's `*Default` helpers: when the YipEntity `path` is empty,
    /// pick the conventional file for this kind.
    fn resolve_path(&self, supplied: &str) -> PathBuf {
        if !supplied.is_empty() {
            return PathBuf::from(supplied);
        }
        match self {
            ParsedEntity::User(_) => PathBuf::from("/etc/passwd"),
            ParsedEntity::Group(_) => PathBuf::from("/etc/group"),
            ParsedEntity::Shadow(_) => PathBuf::from("/etc/shadow"),
            ParsedEntity::GShadow(_) => PathBuf::from("/etc/gshadow"),
        }
    }

    /// Render this entity as a single colon-separated line (no trailing `\n`).
    /// Format matches `mudler/entities` `String()` per-kind.
    fn render_line(&self) -> String {
        match self {
            ParsedEntity::User(u) => format!(
                "{}:{}:{}:{}:{}:{}:{}",
                u.username, u.password, u.uid, u.gid, u.info, u.homedir, u.shell
            ),
            ParsedEntity::Group(g) => {
                // Go renders an empty `Gid` (*int nil) as the empty string; we
                // honour that to interop with `mudler/entities`.
                let gid = g.gid.map(|n| n.to_string()).unwrap_or_default();
                format!("{}:{}:{}:{}", g.group_name, g.password, gid, g.users)
            }
            ParsedEntity::Shadow(s) => format!(
                "{}:{}:{}:{}:{}:{}:{}:{}:{}",
                s.username,
                s.password,
                s.last_changed,
                s.minimum_changed,
                s.maximum_changed,
                s.warn,
                s.inactive,
                s.expire,
                s.reserved,
            ),
            ParsedEntity::GShadow(g) => format!(
                "{}:{}:{}:{}",
                g.name, g.password, g.administrators, g.members
            ),
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::console::RecordingConsole;
    use crate::vfs::MemVfs;
    use indoc::indoc;
    use std::path::Path;

    fn write(fs: &MemVfs, path: &str, contents: &str) {
        fs.write(Path::new(path), contents.as_bytes()).unwrap();
    }

    fn read(fs: &MemVfs, path: &str) -> String {
        fs.read_to_string(Path::new(path)).unwrap()
    }

    // --- line_identifier / merge_line ----------------------------------

    #[test]
    fn line_identifier_extracts_first_field() {
        assert_eq!(line_identifier("root:x:0:0::/root:/bin/sh"), "root");
        assert_eq!(line_identifier("foo"), "foo");
        assert_eq!(line_identifier(""), "");
    }

    #[test]
    fn merge_appends_when_no_match() {
        let out = merge_line("root:x:0:0::/root:/bin/sh\n", "alice", "alice:x:1:1:::");
        assert_eq!(out, "root:x:0:0::/root:/bin/sh\nalice:x:1:1:::\n");
    }

    #[test]
    fn merge_replaces_existing_key() {
        let existing = "root:x:0:0::/root:/bin/sh\nalice:old:1:1:::\n";
        let out = merge_line(existing, "alice", "alice:new:1:1:::");
        assert_eq!(out, "root:x:0:0::/root:/bin/sh\nalice:new:1:1:::\n");
    }

    #[test]
    fn merge_into_empty_file() {
        let out = merge_line("", "alice", "alice:x:1:1:::");
        assert_eq!(out, "alice:x:1:1:::\n");
    }

    #[test]
    fn merge_preserves_no_trailing_newline_input_then_adds_one() {
        let out = merge_line("root:x:0:0::/root:/bin/sh", "alice", "alice:x:1:1:::");
        assert_eq!(out, "root:x:0:0::/root:/bin/sh\nalice:x:1:1:::\n");
    }

    // --- ensure_entities ------------------------------------------------

    #[test]
    fn ensure_appends_new_user_to_passwd() {
        let fs = MemVfs::new();
        let con = RecordingConsole::new();
        write(&fs, "/etc/passwd", "root:x:0:0:root:/root:/bin/bash\n");

        let stage = Stage {
            ensure_entities: vec![YipEntity {
                path: "/etc/passwd".into(),
                entity: indoc! {r#"
                    kind: user
                    username: alice
                    password: x
                    uid: 1000
                    gid: 1000
                    info: Alice
                    homedir: /home/alice
                    shell: /bin/bash
                "#}
                .into(),
            }],
            ..Default::default()
        };

        run(&stage, &fs, &con).unwrap();
        assert_eq!(
            read(&fs, "/etc/passwd"),
            "root:x:0:0:root:/root:/bin/bash\nalice:x:1000:1000:Alice:/home/alice:/bin/bash\n",
        );
    }

    #[test]
    fn ensure_replaces_existing_user_not_duplicate() {
        let fs = MemVfs::new();
        let con = RecordingConsole::new();
        write(
            &fs,
            "/etc/passwd",
            "root:x:0:0:root:/root:/bin/bash\nalice:OLD:1:1::/home/alice:/bin/sh\n",
        );

        let stage = Stage {
            ensure_entities: vec![YipEntity {
                path: "/etc/passwd".into(),
                entity: indoc! {r#"
                    kind: user
                    username: alice
                    password: NEW
                    uid: 1000
                    gid: 1000
                    info: Alice
                    homedir: /home/alice
                    shell: /bin/bash
                "#}
                .into(),
            }],
            ..Default::default()
        };

        run(&stage, &fs, &con).unwrap();
        let out = read(&fs, "/etc/passwd");
        // exactly one alice line, and it's the new one. Count lines that
        // START with "alice:" — substring count would double-match the
        // homedir field ("/home/alice:/bin/bash").
        let alice_lines = out.lines().filter(|l| l.starts_with("alice:")).count();
        assert_eq!(alice_lines, 1, "got: {out}");
        assert!(out.contains("alice:NEW:1000:1000:Alice:/home/alice:/bin/bash"));
        assert!(!out.contains("alice:OLD"));
    }

    #[test]
    fn ensure_creates_passwd_when_target_missing() {
        let fs = MemVfs::new();
        let con = RecordingConsole::new();

        let stage = Stage {
            ensure_entities: vec![YipEntity {
                path: "/etc/passwd".into(),
                entity: indoc! {r#"
                    kind: user
                    username: alice
                    password: x
                    uid: 1000
                    gid: 1000
                    info: A
                    homedir: /home/alice
                    shell: /bin/sh
                "#}
                .into(),
            }],
            ..Default::default()
        };

        run(&stage, &fs, &con).unwrap();
        assert_eq!(
            read(&fs, "/etc/passwd"),
            "alice:x:1000:1000:A:/home/alice:/bin/sh\n"
        );
    }

    #[test]
    fn ensure_group_merges_correctly() {
        // Mirrors the Go default_test.go "Creates Users" expectation:
        // existing: "nm-openconnect:x:979:\n"
        // ensure:   foo group with gid=1, users one,two,tree
        // expect:   appended.
        let fs = MemVfs::new();
        let con = RecordingConsole::new();
        write(&fs, "/etc/group", "nm-openconnect:x:979:\n");

        let stage = Stage {
            ensure_entities: vec![YipEntity {
                path: "/etc/group".into(),
                entity: indoc! {r#"
                    kind: group
                    group_name: foo
                    password: xx
                    gid: 1
                    users: one,two,tree
                "#}
                .into(),
            }],
            ..Default::default()
        };
        run(&stage, &fs, &con).unwrap();
        assert_eq!(
            read(&fs, "/etc/group"),
            "nm-openconnect:x:979:\nfoo:xx:1:one,two,tree\n",
        );
    }

    #[test]
    fn ensure_shadow_renders_nine_fields() {
        let fs = MemVfs::new();
        let con = RecordingConsole::new();
        write(&fs, "/etc/shadow", "root:!:19000:0:99999:7:::\n");

        let stage = Stage {
            ensure_entities: vec![YipEntity {
                path: "/etc/shadow".into(),
                entity: indoc! {r#"
                    kind: shadow
                    username: alice
                    password: "$6$abc$def"
                    last_changed: "19000"
                    minimum_changed: "0"
                    maximum_changed: "99999"
                    warn: "7"
                    inactive: ""
                    expire: ""
                    reserved: ""
                "#}
                .into(),
            }],
            ..Default::default()
        };
        run(&stage, &fs, &con).unwrap();
        let out = read(&fs, "/etc/shadow");
        assert!(out.contains("alice:$6$abc$def:19000:0:99999:7:::"));
        // Two lines total.
        assert_eq!(out.lines().count(), 2);
    }

    #[test]
    fn ensure_uses_default_path_when_empty() {
        let fs = MemVfs::new();
        let con = RecordingConsole::new();
        // Pre-populate /etc/passwd so the default-path code path is exercised.
        write(&fs, "/etc/passwd", "root:x:0:0::/root:/bin/sh\n");

        let stage = Stage {
            ensure_entities: vec![YipEntity {
                path: String::new(), // <- empty -> default to /etc/passwd
                entity: indoc! {r#"
                    kind: user
                    username: alice
                    password: x
                    uid: 1
                    gid: 1
                    info: A
                    homedir: /home/alice
                    shell: /bin/sh
                "#}
                .into(),
            }],
            ..Default::default()
        };
        run(&stage, &fs, &con).unwrap();
        let out = read(&fs, "/etc/passwd");
        assert!(out.contains("alice:x:1:1:A:/home/alice:/bin/sh"));
    }

    // --- delete_entities ------------------------------------------------

    #[test]
    fn delete_removes_matching_line() {
        // Go default_test.go "Deletes Users" matches an exact line:
        //   input  : "nm-openconnect:x:979:\nfoo:xx:1:one,two,tree\n"
        //   delete : group foo, gid=1, users one,two,tree
        //   expect : "nm-openconnect:x:979:\n"
        let fs = MemVfs::new();
        let con = RecordingConsole::new();
        write(
            &fs,
            "/etc/group",
            "nm-openconnect:x:979:\nfoo:xx:1:one,two,tree\n",
        );

        let stage = Stage {
            delete_entities: vec![YipEntity {
                path: "/etc/group".into(),
                entity: indoc! {r#"
                    kind: group
                    group_name: foo
                    password: xx
                    gid: 1
                    users: one,two,tree
                "#}
                .into(),
            }],
            ..Default::default()
        };
        run_delete(&stage, &fs, &con).unwrap();
        assert_eq!(read(&fs, "/etc/group"), "nm-openconnect:x:979:\n");
    }

    #[test]
    fn delete_when_line_absent_is_noop() {
        let fs = MemVfs::new();
        let con = RecordingConsole::new();
        write(&fs, "/etc/passwd", "root:x:0:0::/root:/bin/sh\n");

        let stage = Stage {
            delete_entities: vec![YipEntity {
                path: "/etc/passwd".into(),
                entity: indoc! {r#"
                    kind: user
                    username: ghost
                    password: x
                    uid: 9
                    gid: 9
                    info: ""
                    homedir: /tmp
                    shell: /bin/false
                "#}
                .into(),
            }],
            ..Default::default()
        };
        run_delete(&stage, &fs, &con).unwrap();
        // unchanged
        assert_eq!(read(&fs, "/etc/passwd"), "root:x:0:0::/root:/bin/sh\n");
    }

    // --- error aggregation ---------------------------------------------

    #[test]
    fn ensure_aggregates_errors() {
        let fs = MemVfs::new();
        let con = RecordingConsole::new();

        let stage = Stage {
            ensure_entities: vec![
                YipEntity {
                    path: "/etc/passwd".into(),
                    entity: "kind: unknown".into(),
                },
                YipEntity {
                    path: "/etc/passwd".into(),
                    entity: "kind: user\n".into(), // empty username
                },
            ],
            ..Default::default()
        };
        let err = run(&stage, &fs, &con).expect_err("two errors expected");
        match err {
            Error::Multi(errs) => assert_eq!(errs.len(), 2),
            other => panic!("expected Multi, got {other:?}"),
        }
    }

    #[test]
    fn empty_stage_is_ok_for_both_actions() {
        let fs = MemVfs::new();
        let con = RecordingConsole::new();
        let stage = Stage::default();
        run(&stage, &fs, &con).unwrap();
        run_delete(&stage, &fs, &con).unwrap();
    }

    // --- build()/build_delete() return callable Plugins ---------------

    #[test]
    fn build_returns_callable_plugin() {
        let fs = MemVfs::new();
        let con = RecordingConsole::new();
        let p = build();
        p(&Stage::default(), &fs, &con).unwrap();
        let p = build_delete();
        p(&Stage::default(), &fs, &con).unwrap();
    }

    // -------------------------------------------------------------------
    // Ported from Go: shadow 9-field round trip, gshadow, group members,
    // delete for groups + shadow, combined ensure + delete in one stage.
    // -------------------------------------------------------------------

    #[test]
    fn ensure_shadow_full_nine_fields_round_trip() {
        // All nine fields populated. Render order: username:password:
        // last:min:max:warn:inactive:expire:reserved.
        let fs = MemVfs::new();
        let con = RecordingConsole::default();
        let stage = Stage {
            ensure_entities: vec![YipEntity {
                path: "/etc/shadow".into(),
                entity: indoc! {r#"
                    kind: shadow
                    username: alice
                    password: "$6$abc$def"
                    last_changed: "19000"
                    minimum_changed: "0"
                    maximum_changed: "99999"
                    warn: "7"
                    inactive: "14"
                    expire: "20000"
                    reserved: "x"
                "#}
                .into(),
            }],
            ..Default::default()
        };
        run(&stage, &fs, &con).unwrap();
        let out = read(&fs, "/etc/shadow");
        assert!(
            out.contains("alice:$6$abc$def:19000:0:99999:7:14:20000:x"),
            "got: {out}"
        );
    }

    #[test]
    fn ensure_gshadow_entry_renders_four_fields() {
        // gshadow format: name:password:administrators:members
        let fs = MemVfs::new();
        let con = RecordingConsole::default();
        let stage = Stage {
            ensure_entities: vec![YipEntity {
                path: "/etc/gshadow".into(),
                entity: indoc! {r#"
                    kind: gshadow
                    name: wheel
                    password: "!"
                    administrators: root,alice
                    members: bob,carol
                "#}
                .into(),
            }],
            ..Default::default()
        };
        run(&stage, &fs, &con).unwrap();
        let out = read(&fs, "/etc/gshadow");
        assert_eq!(out, "wheel:!:root,alice:bob,carol\n");
    }

    #[test]
    fn ensure_group_with_member_list() {
        // Group with a populated members list; verify the comma-separated
        // members field is preserved as a single colon-delimited segment.
        let fs = MemVfs::new();
        let con = RecordingConsole::default();
        write(&fs, "/etc/group", "root:x:0:\n");

        let stage = Stage {
            ensure_entities: vec![YipEntity {
                path: "/etc/group".into(),
                entity: indoc! {r#"
                    kind: group
                    group_name: devs
                    password: x
                    gid: 1500
                    users: alice,bob,carol,dave
                "#}
                .into(),
            }],
            ..Default::default()
        };
        run(&stage, &fs, &con).unwrap();
        let out = read(&fs, "/etc/group");
        assert!(out.contains("devs:x:1500:alice,bob,carol,dave\n"), "got: {out}");
        // Still has root.
        assert!(out.starts_with("root:x:0:\n"));
    }

    #[test]
    fn delete_group_entry_removes_only_that_line() {
        let fs = MemVfs::new();
        let con = RecordingConsole::default();
        write(
            &fs,
            "/etc/group",
            "root:x:0:\nwheel:x:10:alice\ndevs:x:1500:alice,bob\n",
        );

        let stage = Stage {
            delete_entities: vec![YipEntity {
                path: "/etc/group".into(),
                entity: indoc! {r#"
                    kind: group
                    group_name: wheel
                    password: x
                    gid: 10
                    users: alice
                "#}
                .into(),
            }],
            ..Default::default()
        };
        run_delete(&stage, &fs, &con).unwrap();
        let out = read(&fs, "/etc/group");
        assert_eq!(out, "root:x:0:\ndevs:x:1500:alice,bob\n");
    }

    #[test]
    fn delete_shadow_entry_removes_matching_line() {
        let fs = MemVfs::new();
        let con = RecordingConsole::default();
        write(
            &fs,
            "/etc/shadow",
            "root:!:19000:0:99999:7:::\nalice:$6$abc$def:19000:0:99999:7:::\n",
        );

        let stage = Stage {
            delete_entities: vec![YipEntity {
                path: "/etc/shadow".into(),
                entity: indoc! {r#"
                    kind: shadow
                    username: alice
                    password: "$6$abc$def"
                    last_changed: "19000"
                    minimum_changed: "0"
                    maximum_changed: "99999"
                    warn: "7"
                    inactive: ""
                    expire: ""
                    reserved: ""
                "#}
                .into(),
            }],
            ..Default::default()
        };
        run_delete(&stage, &fs, &con).unwrap();
        let out = read(&fs, "/etc/shadow");
        assert_eq!(out, "root:!:19000:0:99999:7:::\n");
        assert!(!out.contains("alice"));
    }

    // -------------------------------------------------------------------
    // Lock-wrapping smoke test.
    //
    // The lock wrap (with_passwd_lock) opens an advisory flock on a sibling
    // .yip-lock file before invoking ensure_one. On a MemVfs the target
    // path lives in memory only, so the sibling lock file ends up touching
    // the real host filesystem (or failing, which is a non-fatal warn-and-
    // proceed path). Either way, ensure_one must still produce the right
    // result. This test pins that guarantee.
    // -------------------------------------------------------------------

    #[test]
    fn ensure_works_through_lock_wrap() {
        let fs = MemVfs::new();
        let con = RecordingConsole::new();
        write(&fs, "/etc/passwd", "root:x:0:0:root:/root:/bin/bash\n");

        let stage = Stage {
            ensure_entities: vec![YipEntity {
                path: "/etc/passwd".into(),
                entity: indoc! {r#"
                    kind: user
                    username: bob
                    password: x
                    uid: 1001
                    gid: 1001
                    info: Bob
                    homedir: /home/bob
                    shell: /bin/bash
                "#}
                .into(),
            }],
            ..Default::default()
        };

        // Goes through run() → with_passwd_lock() → ensure_one().
        run(&stage, &fs, &con).unwrap();
        let out = read(&fs, "/etc/passwd");
        assert!(
            out.contains("bob:x:1001:1001:Bob:/home/bob:/bin/bash"),
            "ensure_one did not produce expected line; got: {out}",
        );
        // And the original line is preserved.
        assert!(out.starts_with("root:x:0:0:root:/root:/bin/bash\n"));
    }

    #[test]
    fn combined_ensure_and_delete_in_same_stage() {
        // Same stage carries both ensure_entities AND delete_entities.
        // Running ensure then delete (mirrors the two-plugin DAG hookup)
        // must produce a file with the new entry and without the old one.
        let fs = MemVfs::new();
        let con = RecordingConsole::default();
        write(&fs, "/etc/passwd", "root:x:0:0::/root:/bin/sh\nold:x:99:99:O:/h:/sh\n");

        let stage = Stage {
            ensure_entities: vec![YipEntity {
                path: "/etc/passwd".into(),
                entity: indoc! {r#"
                    kind: user
                    username: new
                    password: x
                    uid: 100
                    gid: 100
                    info: N
                    homedir: /home/new
                    shell: /bin/bash
                "#}
                .into(),
            }],
            delete_entities: vec![YipEntity {
                path: "/etc/passwd".into(),
                entity: indoc! {r#"
                    kind: user
                    username: old
                    password: x
                    uid: 99
                    gid: 99
                    info: O
                    homedir: /h
                    shell: /sh
                "#}
                .into(),
            }],
            ..Default::default()
        };

        // Apply ensure first, then delete — same dispatch order as the
        // executor wires the two actions.
        run(&stage, &fs, &con).unwrap();
        run_delete(&stage, &fs, &con).unwrap();

        let out = read(&fs, "/etc/passwd");
        assert!(out.contains("root:x:0:0::/root:/bin/sh\n"));
        assert!(out.contains("new:x:100:100:N:/home/new:/bin/bash"));
        assert!(!out.contains("old:x:99:99:O:/h:/sh"));
    }
}
