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
        if let Err(err) = ensure_one(fs, e) {
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
        if let Err(err) = delete_one(fs, e) {
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
}
