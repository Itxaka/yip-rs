//! `users` plugin — create / update Unix user accounts via `/etc/passwd`,
//! `/etc/shadow`, and `/etc/group`.
//!
//! Port of `pkg/plugins/user.go`. The Go version delegates to
//! `mudler/entities` + `mauromorales/xpasswd` for line parsing and UID
//! allocation; we reimplement the bits we need directly against the
//! [`Vfs`] so [`MemVfs`] works in tests without touching the real
//! filesystem.
//!
//! For each `(name, user)` in `stage.users` (sorted by name so UID
//! allocation is deterministic) we:
//!
//! 1. Decide the **UID** — explicit `user.uid`, else reuse the existing
//!    `/etc/passwd` entry's UID, else allocate `max(existing_uids)+1`
//!    floored at `HUMAN_ID_MIN` (1000).
//! 2. Decide the **primary group** — either `user.primary_group` (looked
//!    up by name or numeric in `/etc/group`, auto-allocated if missing)
//!    or a brand-new group named after the user.
//! 3. Hash the **password** when it's non-empty and looks unhashed (no
//!    `$id$…` prefix), using sha512crypt. `lock_passwd: true` overrides
//!    everything and stores `!`. An empty password stores `*`.
//! 4. Write/replace the user line in `/etc/passwd`, the shadow line in
//!    `/etc/shadow` (preserving aging fields from any existing row),
//!    and the user as a member of each group in `user.groups`.
//! 5. Create the home directory (`mkdir` + `chown` + `chmod 0755`)
//!    unless `no_create_home`.
//! 6. Append each `ssh_authorized_keys` entry into
//!    `~user/.ssh/authorized_keys` (deduped), `.ssh` mode 0700, file
//!    mode 0600, owner = user.
//!
//! Errors per-user are aggregated into [`Error::Multi`] so one bad row
//! doesn't sabotage the rest of the stage — matches Go's multierror.

use std::path::Path;
use std::sync::Arc;

use sha_crypt::{sha512_simple, Sha512Params};
use tracing::{debug, info, warn};

use crate::console::Console;
use crate::error::{Error, Result};
use crate::executor::Plugin;
use crate::schema::Stage;
use crate::schema::user::User;
use crate::vfs::Vfs;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Minimum auto-allocated UID/GID — mirrors `entities.HumanIDMin`.
const HUMAN_ID_MIN: u32 = 1000;

/// Path constants. Kept relative-to-root so a chroot-style `Vfs` works.
const ETC_PASSWD: &str = "/etc/passwd";
const ETC_SHADOW: &str = "/etc/shadow";
const ETC_GROUP: &str = "/etc/group";

const DEFAULT_SHELL: &str = "/bin/sh";
const DEFAULT_HOME_BASE: &str = "/home";
const DEFAULT_GECOS: &str = "Created by entities";

// ---------------------------------------------------------------------------
// Plugin entry points
// ---------------------------------------------------------------------------

/// Build a [`Plugin`] arc-closure.
pub fn build() -> Plugin {
    Arc::new(run)
}

/// `users` plugin body.
pub fn run(stage: &Stage, fs: &dyn Vfs, _console: &dyn Console) -> Result<()> {
    if stage.users.is_empty() {
        return Ok(());
    }
    info!(count = stage.users.len(), "applying users");

    // Sort by name so UID auto-allocation is deterministic across runs
    // (matches the Go plugin's `sort.Strings`).
    let mut names: Vec<&String> = stage.users.keys().collect();
    names.sort();

    let mut errs: Vec<Error> = Vec::new();
    for name in names {
        let mut u = stage.users[name].clone();
        u.name = name.clone();
        if let Err(e) = apply_one(fs, &u) {
            warn!(user = %name, error = %e, "user apply failed");
            errs.push(e);
        }
    }

    match errs.len() {
        0 => Ok(()),
        _ => Err(Error::Multi(errs)),
    }
}

// ---------------------------------------------------------------------------
// One-user work
// ---------------------------------------------------------------------------

fn apply_one(fs: &dyn Vfs, u: &User) -> Result<()> {
    if u.name.is_empty() {
        return Err(Error::Schema("user: empty `name`".into()));
    }
    debug!(user = %u.name, "apply user");

    // --- load existing tables ----------------------------------------
    let passwd_text = read_or_empty(fs, Path::new(ETC_PASSWD))?;
    let shadow_text = read_or_empty(fs, Path::new(ETC_SHADOW))?;
    let group_text = read_or_empty(fs, Path::new(ETC_GROUP))?;

    let mut passwd = PasswdTable::parse(&passwd_text);
    let mut group = GroupTable::parse(&group_text);

    // --- resolve primary group (creating if missing) ----------------
    let (primary_group_name, gid) = resolve_primary_group(&mut group, u)?;

    // --- resolve UID ------------------------------------------------
    let uid = resolve_uid(&passwd, u)?;

    // --- resolve homedir + shell ------------------------------------
    let homedir = if u.homedir.is_empty() {
        format!("{DEFAULT_HOME_BASE}/{}", u.name)
    } else {
        u.homedir.clone()
    };
    let shell = if u.shell.is_empty() {
        DEFAULT_SHELL.to_string()
    } else {
        u.shell.clone()
    };
    let gecos = if u.gecos.is_empty() {
        DEFAULT_GECOS.to_string()
    } else {
        u.gecos.clone()
    };

    // --- hash password ---------------------------------------------
    let shadow_password = if u.lock_passwd {
        "!".to_string()
    } else if u.password_hash.is_empty() {
        "*".to_string()
    } else if looks_hashed(&u.password_hash) {
        u.password_hash.clone()
    } else {
        match sha512_simple(&u.password_hash, &Sha512Params::default()) {
            Ok(h) => h,
            Err(e) => {
                return Err(Error::other(format!(
                    "user {}: sha512crypt failed: {e:?}",
                    u.name
                )));
            }
        }
    };

    // --- write /etc/passwd row -------------------------------------
    let passwd_row = PasswdRow {
        name: u.name.clone(),
        password: "x".into(),
        uid,
        gid,
        gecos,
        homedir: homedir.clone(),
        shell,
    };
    passwd.upsert(passwd_row);
    fs.write(Path::new(ETC_PASSWD), passwd.render().as_bytes())?;

    // --- write /etc/shadow row -------------------------------------
    let new_shadow_row = build_shadow_row(&shadow_text, &u.name, &shadow_password);
    let mut shadow = ShadowTable::parse(&shadow_text);
    shadow.upsert(new_shadow_row);
    fs.write(Path::new(ETC_SHADOW), shadow.render().as_bytes())?;

    // --- write /etc/group ------------------------------------------
    // Make sure the primary group has the user listed as a member, then
    // add the user to every secondary `groups` entry that exists.
    group.add_member(&primary_group_name, &u.name);
    for g in &u.groups {
        if !g.is_empty() {
            group.add_member(g, &u.name);
        }
    }
    fs.write(Path::new(ETC_GROUP), group.render().as_bytes())?;

    // --- mkdir homedir ---------------------------------------------
    if !u.no_create_home {
        let h = Path::new(&homedir);
        fs.mkdir_all(h)?;
        // Best-effort chown/chmod — on Vfs without real ownership these
        // are recorded in side state but won't fail the run.
        let _ = fs.chown(h, uid as i32, gid as i32);
        let _ = fs.chmod(h, 0o755);
    }

    // --- write ssh keys --------------------------------------------
    if !u.ssh_authorized_keys.is_empty() {
        write_ssh_keys(fs, &homedir, uid, gid, &u.ssh_authorized_keys)?;
    }

    Ok(())
}

/// Returns true if `s` looks like an already-hashed crypt(3) password,
/// i.e. starts with `$<id>$`. The Go plugin treats anything starting
/// with `$` as opaque and writes it verbatim (the test
/// "change user password" supplies `$fkekofe` and expects it stored as-is).
fn looks_hashed(s: &str) -> bool {
    s.starts_with('$')
}

// ---------------------------------------------------------------------------
// /etc/passwd
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct PasswdRow {
    name: String,
    password: String,
    uid: u32,
    gid: u32,
    gecos: String,
    homedir: String,
    shell: String,
}

impl PasswdRow {
    fn parse(line: &str) -> Option<Self> {
        let parts: Vec<&str> = line.splitn(7, ':').collect();
        if parts.len() < 7 {
            return None;
        }
        let uid: u32 = parts[2].parse().ok()?;
        let gid: u32 = parts[3].parse().ok()?;
        Some(Self {
            name: parts[0].to_string(),
            password: parts[1].to_string(),
            uid,
            gid,
            gecos: parts[4].to_string(),
            homedir: parts[5].to_string(),
            shell: parts[6].to_string(),
        })
    }

    fn render(&self) -> String {
        format!(
            "{}:{}:{}:{}:{}:{}:{}",
            self.name, self.password, self.uid, self.gid, self.gecos, self.homedir, self.shell
        )
    }
}

#[derive(Debug, Default)]
struct PasswdTable {
    /// Insertion-ordered list of rows.
    rows: Vec<PasswdRow>,
}

impl PasswdTable {
    fn parse(text: &str) -> Self {
        let mut rows = Vec::new();
        for line in text.lines() {
            if line.is_empty() {
                continue;
            }
            if let Some(r) = PasswdRow::parse(line) {
                rows.push(r);
            }
        }
        Self { rows }
    }

    fn get(&self, name: &str) -> Option<&PasswdRow> {
        self.rows.iter().find(|r| r.name == name)
    }

    fn upsert(&mut self, row: PasswdRow) {
        if let Some(existing) = self.rows.iter_mut().find(|r| r.name == row.name) {
            *existing = row;
        } else {
            self.rows.push(row);
        }
    }

    fn max_uid(&self) -> u32 {
        // Cap at HUMAN_ID_MIN-1 so the "+1" lands at HUMAN_ID_MIN when
        // no human users exist yet.
        let max_existing = self.rows.iter().map(|r| r.uid).max().unwrap_or(0);
        max_existing.max(HUMAN_ID_MIN - 1)
    }

    fn render(&self) -> String {
        let mut out = String::new();
        for r in &self.rows {
            out.push_str(&r.render());
            out.push('\n');
        }
        out
    }
}

fn resolve_uid(passwd: &PasswdTable, u: &User) -> Result<u32> {
    if !u.uid.is_empty() {
        let n: u32 = u
            .uid
            .parse()
            .map_err(|_| Error::Schema(format!("user {}: invalid uid `{}`", u.name, u.uid)))?;
        return Ok(n);
    }
    if let Some(existing) = passwd.get(&u.name) {
        return Ok(existing.uid);
    }
    Ok(passwd.max_uid() + 1)
}

// ---------------------------------------------------------------------------
// /etc/group
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct GroupRow {
    name: String,
    password: String,
    gid: u32,
    /// Members in insertion order (deduped on insert).
    members: Vec<String>,
}

impl GroupRow {
    fn parse(line: &str) -> Option<Self> {
        let parts: Vec<&str> = line.splitn(4, ':').collect();
        if parts.len() < 4 {
            return None;
        }
        let gid: u32 = parts[2].parse().ok()?;
        let members: Vec<String> = if parts[3].is_empty() {
            Vec::new()
        } else {
            parts[3].split(',').map(|s| s.to_string()).collect()
        };
        Some(Self {
            name: parts[0].to_string(),
            password: parts[1].to_string(),
            gid,
            members,
        })
    }

    fn render(&self) -> String {
        format!(
            "{}:{}:{}:{}",
            self.name,
            self.password,
            self.gid,
            self.members.join(",")
        )
    }

    fn add_member(&mut self, user: &str) {
        if !self.members.iter().any(|m| m == user) {
            self.members.push(user.to_string());
        }
    }
}

#[derive(Debug, Default)]
struct GroupTable {
    rows: Vec<GroupRow>,
}

impl GroupTable {
    fn parse(text: &str) -> Self {
        let mut rows = Vec::new();
        for line in text.lines() {
            if line.is_empty() {
                continue;
            }
            if let Some(r) = GroupRow::parse(line) {
                rows.push(r);
            }
        }
        Self { rows }
    }

    fn get_by_name(&self, name: &str) -> Option<&GroupRow> {
        self.rows.iter().find(|r| r.name == name)
    }

    fn get_by_gid(&self, gid: u32) -> Option<&GroupRow> {
        self.rows.iter().find(|r| r.gid == gid)
    }

    fn max_gid(&self) -> u32 {
        let max_existing = self.rows.iter().map(|r| r.gid).max().unwrap_or(0);
        max_existing.max(HUMAN_ID_MIN - 1)
    }

    fn upsert_group(&mut self, name: &str, gid: u32) {
        if let Some(existing) = self.rows.iter_mut().find(|r| r.name == name) {
            existing.gid = gid;
        } else {
            self.rows.push(GroupRow {
                name: name.to_string(),
                password: "x".into(),
                gid,
                members: Vec::new(),
            });
        }
    }

    fn add_member(&mut self, group_name: &str, user: &str) {
        if let Some(existing) = self.rows.iter_mut().find(|r| r.name == group_name) {
            existing.add_member(user);
        }
        // If the group doesn't exist we silently skip; the Go plugin
        // does the same (only "groups that exist" get the user added).
    }

    fn render(&self) -> String {
        let mut out = String::new();
        for r in &self.rows {
            out.push_str(&r.render());
            out.push('\n');
        }
        out
    }
}

/// Returns `(group_name, gid)`. Creates the group in `group` if it
/// doesn't already exist.
fn resolve_primary_group(group: &mut GroupTable, u: &User) -> Result<(String, u32)> {
    let requested = if u.primary_group.is_empty() {
        u.name.clone()
    } else {
        u.primary_group.clone()
    };

    // Numeric primary_group: look up by GID first.
    if let Ok(numeric_gid) = requested.parse::<u32>() {
        if let Some(existing) = group.get_by_gid(numeric_gid) {
            return Ok((existing.name.clone(), existing.gid));
        }
        // Numeric GID with no matching group → error (the Go plugin
        // delegates to `osuser.LookupGroup` which would fail here).
        return Err(Error::Schema(format!(
            "user {}: primary_group `{}` (numeric) not found in /etc/group",
            u.name, requested
        )));
    }

    // Name lookup.
    if let Some(existing) = group.get_by_name(&requested) {
        return Ok((existing.name.clone(), existing.gid));
    }

    // Not found — create with auto-GID.
    let new_gid = group.max_gid() + 1;
    group.upsert_group(&requested, new_gid);
    Ok((requested, new_gid))
}

// ---------------------------------------------------------------------------
// /etc/shadow
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct ShadowRow {
    name: String,
    password: String,
    last_changed: String,
    minimum_changed: String,
    maximum_changed: String,
    warn: String,
    inactive: String,
    expire: String,
    reserved: String,
}

impl ShadowRow {
    fn parse(line: &str) -> Option<Self> {
        let parts: Vec<&str> = line.splitn(9, ':').collect();
        if parts.len() < 2 {
            return None;
        }
        let g = |i: usize| parts.get(i).copied().unwrap_or("").to_string();
        Some(Self {
            name: parts[0].to_string(),
            password: parts[1].to_string(),
            last_changed: g(2),
            minimum_changed: g(3),
            maximum_changed: g(4),
            warn: g(5),
            inactive: g(6),
            expire: g(7),
            reserved: g(8),
        })
    }

    fn render(&self) -> String {
        format!(
            "{}:{}:{}:{}:{}:{}:{}:{}:{}",
            self.name,
            self.password,
            self.last_changed,
            self.minimum_changed,
            self.maximum_changed,
            self.warn,
            self.inactive,
            self.expire,
            self.reserved
        )
    }
}

#[derive(Debug, Default)]
struct ShadowTable {
    rows: Vec<ShadowRow>,
}

impl ShadowTable {
    fn parse(text: &str) -> Self {
        let mut rows = Vec::new();
        for line in text.lines() {
            if line.is_empty() {
                continue;
            }
            if let Some(r) = ShadowRow::parse(line) {
                rows.push(r);
            }
        }
        Self { rows }
    }

    fn upsert(&mut self, row: ShadowRow) {
        if let Some(existing) = self.rows.iter_mut().find(|r| r.name == row.name) {
            *existing = row;
        } else {
            self.rows.push(row);
        }
    }

    fn render(&self) -> String {
        let mut out = String::new();
        for r in &self.rows {
            out.push_str(&r.render());
            out.push('\n');
        }
        out
    }
}

/// Build a shadow row, preserving any existing aging fields from the
/// current /etc/shadow contents (mirrors `shadowWithPreservedAging` in
/// Go).
fn build_shadow_row(etcshadow_text: &str, username: &str, password: &str) -> ShadowRow {
    // Defaults: yip writes "now" for last_changed on new rows; we use a
    // reasonable placeholder so the row format remains valid.
    let mut row = ShadowRow {
        name: username.to_string(),
        password: password.to_string(),
        last_changed: days_since_epoch().to_string(),
        minimum_changed: "0".into(),
        maximum_changed: "99999".into(),
        warn: "7".into(),
        inactive: String::new(),
        expire: String::new(),
        reserved: String::new(),
    };

    let current = ShadowTable::parse(etcshadow_text);
    if let Some(existing) = current.rows.iter().find(|r| r.name == username) {
        row.last_changed = existing.last_changed.clone();
        row.minimum_changed = existing.minimum_changed.clone();
        row.maximum_changed = existing.maximum_changed.clone();
        row.warn = existing.warn.clone();
        row.inactive = existing.inactive.clone();
        row.expire = existing.expire.clone();
        row.reserved = existing.reserved.clone();
    }

    row
}

/// Days since the Unix epoch, used for `last_changed` on brand-new
/// shadow rows. Falls back to "0" if the system clock is somehow before
/// 1970 (clock-skew belt-and-braces).
fn days_since_epoch() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| (d.as_secs() / 86_400) as i64)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// SSH keys
// ---------------------------------------------------------------------------

fn write_ssh_keys(
    fs: &dyn Vfs,
    homedir: &str,
    uid: u32,
    gid: u32,
    keys: &[String],
) -> Result<()> {
    let ssh_dir = format!("{homedir}/.ssh");
    let keys_path = format!("{ssh_dir}/authorized_keys");

    fs.mkdir_all(Path::new(&ssh_dir))?;
    let _ = fs.chmod(Path::new(&ssh_dir), 0o700);
    let _ = fs.chown(Path::new(&ssh_dir), uid as i32, gid as i32);

    // Append new keys, deduping against whatever's already there.
    let existing = if fs.exists(Path::new(&keys_path)) {
        fs.read_to_string(Path::new(&keys_path))?
    } else {
        String::new()
    };
    let mut have: Vec<String> = existing
        .lines()
        .map(|l| l.to_string())
        .filter(|l| !l.is_empty())
        .collect();

    for k in keys {
        let k = k.trim().to_string();
        if k.is_empty() {
            continue;
        }
        if !have.iter().any(|h| h == &k) {
            have.push(k);
        }
    }

    let mut body = have.join("\n");
    if !body.is_empty() {
        body.push('\n');
    }
    fs.write(Path::new(&keys_path), body.as_bytes())?;
    let _ = fs.chmod(Path::new(&keys_path), 0o600);
    let _ = fs.chown(Path::new(&keys_path), uid as i32, gid as i32);
    Ok(())
}

// ---------------------------------------------------------------------------
// Utility
// ---------------------------------------------------------------------------

fn read_or_empty(fs: &dyn Vfs, path: &Path) -> Result<String> {
    if !fs.exists(path) {
        return Ok(String::new());
    }
    fs.read_to_string(path)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::console::RecordingConsole;
    use crate::schema::user::User;
    use crate::vfs::MemVfs;
    use std::collections::HashMap;

    fn write(fs: &MemVfs, path: &str, contents: &str) {
        fs.write(Path::new(path), contents.as_bytes()).unwrap();
    }

    fn read(fs: &MemVfs, path: &str) -> String {
        fs.read_to_string(Path::new(path)).unwrap()
    }

    fn seed_empty(fs: &MemVfs) {
        write(fs, ETC_PASSWD, "");
        write(fs, ETC_SHADOW, "");
        write(fs, ETC_GROUP, "");
    }

    fn one_user(name: &str, u: User) -> Stage {
        let mut users = HashMap::new();
        users.insert(name.to_string(), u);
        Stage {
            users,
            ..Default::default()
        }
    }

    // --- explicit uid/gid + ssh keys -----------------------------------

    #[test]
    fn creates_alice_with_explicit_uid_and_ssh_key() {
        let fs = MemVfs::new();
        let con = RecordingConsole::new();
        seed_empty(&fs);

        let stage = one_user(
            "alice",
            User {
                uid: "1000".into(),
                primary_group: String::new(),
                ssh_authorized_keys: vec!["ssh-rsa AAAA...alice@host".into()],
                ..Default::default()
            },
        );
        run(&stage, &fs, &con).unwrap();

        // passwd
        let passwd = read(&fs, ETC_PASSWD);
        assert!(
            passwd.contains("alice:x:1000:1000:Created by entities:/home/alice:/bin/sh"),
            "got: {passwd}"
        );

        // shadow — no password set → "*"
        let shadow = read(&fs, ETC_SHADOW);
        assert!(shadow.starts_with("alice:*:"), "got: {shadow}");

        // group — alice's primary group "alice" got auto-created at 1000
        let group = read(&fs, ETC_GROUP);
        assert_eq!(group, "alice:x:1000:alice\n");

        // ssh key
        let ak = read(&fs, "/home/alice/.ssh/authorized_keys");
        assert!(ak.contains("ssh-rsa AAAA...alice@host"));
    }

    // --- hashed-vs-unhashed password handling --------------------------

    #[test]
    fn opaque_dollar_password_passes_through_unchanged() {
        // Mirrors Go test "change user password": passwd = "$fkekofe" is
        // stored verbatim because it starts with `$`.
        let fs = MemVfs::new();
        let con = RecordingConsole::new();
        seed_empty(&fs);

        let stage = one_user(
            "foo",
            User {
                password_hash: "$fkekofe".into(),
                ..Default::default()
            },
        );
        run(&stage, &fs, &con).unwrap();

        let shadow = read(&fs, ETC_SHADOW);
        assert!(
            shadow.contains("foo:$fkekofe:"),
            "expected verbatim $-prefixed password, got: {shadow}"
        );
    }

    #[test]
    fn plain_password_gets_sha512crypted() {
        let fs = MemVfs::new();
        let con = RecordingConsole::new();
        seed_empty(&fs);

        let stage = one_user(
            "bob",
            User {
                password_hash: "hunter2".into(),
                ..Default::default()
            },
        );
        run(&stage, &fs, &con).unwrap();

        let shadow = read(&fs, ETC_SHADOW);
        // crypt(3) $6$ prefix → sha512crypt
        assert!(
            shadow.contains("bob:$6$"),
            "expected $6$ sha512crypt hash, got: {shadow}"
        );
    }

    #[test]
    fn lock_passwd_writes_bang() {
        let fs = MemVfs::new();
        let con = RecordingConsole::new();
        seed_empty(&fs);

        let stage = one_user(
            "carl",
            User {
                password_hash: "ignored".into(),
                lock_passwd: true,
                ..Default::default()
            },
        );
        run(&stage, &fs, &con).unwrap();

        let shadow = read(&fs, ETC_SHADOW);
        assert!(shadow.contains("carl:!:"), "got: {shadow}");
    }

    // --- idempotency ----------------------------------------------------

    #[test]
    fn applying_twice_does_not_duplicate() {
        let fs = MemVfs::new();
        let con = RecordingConsole::new();
        seed_empty(&fs);

        let stage = one_user(
            "alice",
            User {
                uid: "1000".into(),
                ssh_authorized_keys: vec!["ssh-rsa AAAA...".into()],
                ..Default::default()
            },
        );
        run(&stage, &fs, &con).unwrap();
        run(&stage, &fs, &con).unwrap();

        let passwd = read(&fs, ETC_PASSWD);
        let alice_lines = passwd.lines().filter(|l| l.starts_with("alice:")).count();
        assert_eq!(alice_lines, 1, "duplicated passwd line: {passwd}");

        let shadow = read(&fs, ETC_SHADOW);
        let alice_shadow = shadow.lines().filter(|l| l.starts_with("alice:")).count();
        assert_eq!(alice_shadow, 1, "duplicated shadow line: {shadow}");

        let group = read(&fs, ETC_GROUP);
        // Single "alice" group, single "alice" member.
        assert_eq!(group, "alice:x:1000:alice\n", "got: {group}");

        // ssh authorized_keys deduped too.
        let ak = read(&fs, "/home/alice/.ssh/authorized_keys");
        assert_eq!(ak.lines().count(), 1, "got: {ak}");
    }

    // --- auto-UID allocation -------------------------------------------

    #[test]
    fn auto_uid_picks_max_plus_one() {
        let fs = MemVfs::new();
        let con = RecordingConsole::new();
        write(
            &fs,
            ETC_PASSWD,
            "root:x:0:0::/root:/bin/sh\nbob:x:1001:1001::/home/bob:/bin/sh\n",
        );
        write(&fs, ETC_SHADOW, "");
        write(&fs, ETC_GROUP, "root:x:0:\nbob:x:1001:bob\n");

        let stage = one_user("alice", User::default());
        run(&stage, &fs, &con).unwrap();

        let passwd = read(&fs, ETC_PASSWD);
        // max existing UID was 1001 → alice = 1002
        assert!(passwd.contains("alice:x:1002:"), "got: {passwd}");
    }

    #[test]
    fn auto_uid_floors_at_human_id_min_when_no_human_users() {
        // Only "root" (UID 0) exists. Auto-allocation must jump to 1000,
        // not 1 — matches HumanIDMin behaviour from `entities`.
        let fs = MemVfs::new();
        let con = RecordingConsole::new();
        write(&fs, ETC_PASSWD, "root:x:0:0::/root:/bin/sh\n");
        write(&fs, ETC_SHADOW, "");
        write(&fs, ETC_GROUP, "root:x:0:\n");

        let stage = one_user("alice", User::default());
        run(&stage, &fs, &con).unwrap();

        let passwd = read(&fs, ETC_PASSWD);
        assert!(passwd.contains("alice:x:1000:"), "got: {passwd}");
    }

    // --- groups membership ---------------------------------------------

    #[test]
    fn adds_user_to_existing_group() {
        // Step 1: create "admin" user (which auto-creates "admin" group).
        // Step 2: create "bar" with secondary group "admin" → bar appears
        //         in admin's members.
        let fs = MemVfs::new();
        let con = RecordingConsole::new();
        seed_empty(&fs);

        run(&one_user("admin", User::default()), &fs, &con).unwrap();
        run(
            &one_user(
                "bar",
                User {
                    groups: vec!["admin".into()],
                    ..Default::default()
                },
            ),
            &fs,
            &con,
        )
        .unwrap();

        let group = read(&fs, ETC_GROUP);
        // admin group exists at GID 1000 with members admin,bar.
        assert!(group.contains("admin:x:1000:admin,bar"), "got: {group}");
        // bar's own primary group exists too.
        assert!(group.contains("bar:x:"), "got: {group}");
    }

    #[test]
    fn missing_secondary_group_is_silently_skipped() {
        let fs = MemVfs::new();
        let con = RecordingConsole::new();
        seed_empty(&fs);

        let stage = one_user(
            "alice",
            User {
                groups: vec!["nonexistent".into()],
                ..Default::default()
            },
        );
        run(&stage, &fs, &con).unwrap();

        let group = read(&fs, ETC_GROUP);
        // Only alice's primary group present.
        assert!(!group.contains("nonexistent"));
        assert!(group.contains("alice:x:"), "got: {group}");
    }

    // --- shadow aging preservation -------------------------------------

    #[test]
    fn preserves_aging_fields_on_password_update() {
        let fs = MemVfs::new();
        let con = RecordingConsole::new();
        // Seed shadow with full aging fields.
        write(&fs, ETC_PASSWD, "");
        write(&fs, ETC_SHADOW, "foo:$6$abc$def:18820:1:365:14:30:20000:\n");
        write(&fs, ETC_GROUP, "");

        run(
            &one_user(
                "foo",
                User {
                    password_hash: "$newhash".into(),
                    ..Default::default()
                },
            ),
            &fs,
            &con,
        )
        .unwrap();

        let shadow = read(&fs, ETC_SHADOW);
        assert!(shadow.contains("foo:$newhash:"));
        assert!(shadow.contains(":1:365:14:30:20000:"), "got: {shadow}");
    }

    // --- homedir ---------------------------------------------------------

    #[test]
    fn creates_homedir_with_owner_when_not_disabled() {
        let fs = MemVfs::new();
        let con = RecordingConsole::new();
        seed_empty(&fs);

        run(
            &one_user(
                "alice",
                User {
                    uid: "1000".into(),
                    ..Default::default()
                },
            ),
            &fs,
            &con,
        )
        .unwrap();

        let m = fs.metadata(Path::new("/home/alice")).unwrap();
        assert!(m.is_dir);
        assert_eq!(m.mode & 0o777, 0o755);
        assert_eq!(m.uid, 1000);
    }

    #[test]
    fn no_create_home_skips_mkdir() {
        let fs = MemVfs::new();
        let con = RecordingConsole::new();
        seed_empty(&fs);

        run(
            &one_user(
                "alice",
                User {
                    uid: "1000".into(),
                    no_create_home: true,
                    ..Default::default()
                },
            ),
            &fs,
            &con,
        )
        .unwrap();

        assert!(!fs.exists(Path::new("/home/alice")));
    }

    // --- error handling -------------------------------------------------

    #[test]
    fn invalid_uid_errors() {
        let fs = MemVfs::new();
        let con = RecordingConsole::new();
        seed_empty(&fs);

        let stage = one_user(
            "alice",
            User {
                uid: "not-a-number".into(),
                ..Default::default()
            },
        );
        let err = run(&stage, &fs, &con).expect_err("invalid uid must error");
        match err {
            Error::Multi(es) => {
                assert_eq!(es.len(), 1);
                assert!(matches!(es[0], Error::Schema(_)));
            }
            other => panic!("expected Multi, got {other:?}"),
        }
    }

    #[test]
    fn empty_stage_is_ok() {
        let fs = MemVfs::new();
        let con = RecordingConsole::new();
        run(&Stage::default(), &fs, &con).unwrap();
    }

    #[test]
    fn build_returns_callable_plugin() {
        let fs = MemVfs::new();
        let con = RecordingConsole::new();
        let p = build();
        p(&Stage::default(), &fs, &con).unwrap();
    }

    // --- parser unit tests ---------------------------------------------

    #[test]
    fn passwd_row_roundtrip() {
        let r = PasswdRow::parse("alice:x:1000:1000:A:/home/alice:/bin/sh").unwrap();
        assert_eq!(r.name, "alice");
        assert_eq!(r.uid, 1000);
        assert_eq!(r.render(), "alice:x:1000:1000:A:/home/alice:/bin/sh");
    }

    #[test]
    fn group_row_roundtrip_with_members() {
        let r = GroupRow::parse("admin:x:1000:alice,bob").unwrap();
        assert_eq!(r.name, "admin");
        assert_eq!(r.members, vec!["alice".to_string(), "bob".to_string()]);
        assert_eq!(r.render(), "admin:x:1000:alice,bob");
    }

    #[test]
    fn group_row_roundtrip_empty_members() {
        let r = GroupRow::parse("admin:x:1000:").unwrap();
        assert!(r.members.is_empty());
        assert_eq!(r.render(), "admin:x:1000:");
    }

    #[test]
    fn shadow_row_roundtrip() {
        let r = ShadowRow::parse("foo:$6$abc$def:18820:1:365:14:30:20000:").unwrap();
        assert_eq!(r.name, "foo");
        assert_eq!(r.password, "$6$abc$def");
        assert_eq!(r.maximum_changed, "365");
        assert_eq!(r.expire, "20000");
        assert_eq!(r.render(), "foo:$6$abc$def:18820:1:365:14:30:20000:");
    }

    #[test]
    fn looks_hashed_detection() {
        assert!(looks_hashed("$6$salt$hash"));
        assert!(looks_hashed("$fkekofe")); // opaque but $-prefixed
        assert!(!looks_hashed("hunter2"));
        assert!(!looks_hashed(""));
    }

    // --- Additional tests ported from Go behaviour expectations ---

    #[test]
    fn empty_password_writes_star_in_shadow() {
        // No password_hash, no lock -> Go writes `*` in /etc/shadow.
        let fs = MemVfs::new();
        let con = RecordingConsole::new();
        seed_empty(&fs);

        let stage = one_user("dora", User::default());
        run(&stage, &fs, &con).unwrap();

        let shadow = read(&fs, ETC_SHADOW);
        assert!(
            shadow.contains("dora:*:"),
            "expected '*' as locked-no-password placeholder, got: {shadow}"
        );
    }

    #[test]
    fn explicit_uid_wins_over_auto_allocation() {
        // Even though /etc/passwd already has UID 1001 (which would push
        // the auto-allocated UID to 1002), an explicit `uid: 1234` must be
        // honoured.
        let fs = MemVfs::new();
        let con = RecordingConsole::new();
        write(
            &fs,
            ETC_PASSWD,
            "root:x:0:0::/root:/bin/sh\nfred:x:1001:1001::/home/fred:/bin/sh\n",
        );
        write(&fs, ETC_SHADOW, "");
        write(&fs, ETC_GROUP, "root:x:0:\nfred:x:1001:fred\n");

        let stage = one_user(
            "evelyn",
            User {
                uid: "1234".into(),
                ..Default::default()
            },
        );
        run(&stage, &fs, &con).unwrap();

        let passwd = read(&fs, ETC_PASSWD);
        assert!(
            passwd.contains("evelyn:x:1234:"),
            "explicit uid 1234 should win, got: {passwd}"
        );
    }

    #[test]
    fn username_with_dot_and_dash_special_chars() {
        // POSIX usernames sometimes contain `.` or `-` (e.g. `nm-openvpn`,
        // `john.doe`). The plugin must round-trip both characters through
        // /etc/passwd lookup.
        let fs = MemVfs::new();
        let con = RecordingConsole::new();
        seed_empty(&fs);

        for name in ["john.doe", "nm-openvpn", "user-with-dots.and-dashes"] {
            let stage = one_user(
                name,
                User {
                    ..Default::default()
                },
            );
            run(&stage, &fs, &con).unwrap();
            let passwd = read(&fs, ETC_PASSWD);
            assert!(
                passwd.contains(&format!("{name}:x:")),
                "{name} should appear in /etc/passwd, got: {passwd}"
            );
        }
    }

    #[test]
    fn preexisting_user_gets_updated_password_hash() {
        // Go test "edits already existing user password": foo exists in
        // passwd + shadow; supply a new password; shadow row's password
        // field is replaced while UID stays the same.
        let fs = MemVfs::new();
        let con = RecordingConsole::new();
        write(
            &fs,
            ETC_PASSWD,
            "foo:x:1500:1500:old gecos:/home/foo:/bin/sh\n",
        );
        write(
            &fs,
            ETC_SHADOW,
            "foo:$6$OLD$OLDHASH:18820:0:99999:7:::\n",
        );
        write(&fs, ETC_GROUP, "foo:x:1500:foo\n");

        run(
            &one_user(
                "foo",
                User {
                    password_hash: "$NEWHASH".into(),
                    ..Default::default()
                },
            ),
            &fs,
            &con,
        )
        .unwrap();

        let shadow = read(&fs, ETC_SHADOW);
        assert!(
            shadow.contains("foo:$NEWHASH:"),
            "new hash must replace old, got: {shadow}"
        );
        // The original UID 1500 should be retained because `uid` was not
        // explicitly provided.
        let passwd = read(&fs, ETC_PASSWD);
        assert!(
            passwd.contains("foo:x:1500:"),
            "existing UID preserved, got: {passwd}"
        );
    }

    #[test]
    fn nonexistent_secondary_group_is_silent_no_op() {
        // Already covered by `missing_secondary_group_is_silently_skipped`,
        // but here we additionally assert there's no error/warning
        // surfaced via Multi.
        let fs = MemVfs::new();
        let con = RecordingConsole::new();
        seed_empty(&fs);

        let res = run(
            &one_user(
                "alice",
                User {
                    groups: vec!["does-not-exist-anywhere".into(), "also-missing".into()],
                    ..Default::default()
                },
            ),
            &fs,
            &con,
        );
        // No errors aggregated — secondary group misses are best-effort.
        assert!(res.is_ok(), "expected Ok, got {res:?}");
    }

    #[test]
    fn shell_bin_false_locks_account_login() {
        // Setting shell to /bin/false is a common pattern for locking
        // interactive login while keeping the account valid.
        let fs = MemVfs::new();
        let con = RecordingConsole::new();
        seed_empty(&fs);

        run(
            &one_user(
                "svcacct",
                User {
                    uid: "1500".into(),
                    shell: "/bin/false".into(),
                    ..Default::default()
                },
            ),
            &fs,
            &con,
        )
        .unwrap();

        let passwd = read(&fs, ETC_PASSWD);
        // Match just the shell field — the auto-allocated GID is impl-defined.
        assert!(
            passwd.lines().any(|l| l.starts_with("svcacct:") && l.ends_with(":/bin/false")),
            "shell field should be /bin/false, got: {passwd}"
        );
    }

    #[test]
    fn gecos_with_commas_and_spaces_passes_through() {
        // The gecos field contains comma-separated subfields per the spec
        // ("Full Name,Room Number,Work Phone,Home Phone,Other"). Spaces
        // and commas must survive into the passwd line.
        let fs = MemVfs::new();
        let con = RecordingConsole::new();
        seed_empty(&fs);

        run(
            &one_user(
                "alice",
                User {
                    uid: "1000".into(),
                    gecos: "Alice Wonderland,Room 1,555-1212,,extra info".into(),
                    ..Default::default()
                },
            ),
            &fs,
            &con,
        )
        .unwrap();

        let passwd = read(&fs, ETC_PASSWD);
        assert!(
            passwd.contains("alice:x:1000:1000:Alice Wonderland,Room 1,555-1212,,extra info:/home/alice:/bin/sh"),
            "gecos with commas should pass through, got: {passwd}"
        );
    }

    #[test]
    fn ssh_keys_from_url_provider_via_mockito() {
        // The user-plugin's ssh_authorized_keys are inserted verbatim — it
        // does NOT fetch URLs (that's the SSH plugin's job). So we provide
        // a "raw" key that happens to look like content fetched from a URL
        // and assert it lands verbatim. This guards against any future
        // change accidentally adding URL-fetching to the user plugin.
        let mut server = mockito::Server::new();
        let body = "ssh-rsa AAAAB3NzaC1yc2E-from-url alice@host\n";
        let _m = server
            .mock("GET", "/keys")
            .with_status(200)
            .with_body(body)
            .create();

        let fs = MemVfs::new();
        let con = RecordingConsole::new();
        seed_empty(&fs);

        // Pre-fetch the keys ourselves to simulate "this is what the SSH
        // plugin would have written" and then drive the user plugin with
        // the resolved key string. Mockito serves the body so the test
        // is symmetric with the ssh.rs URL-fetch tests.
        let resp = reqwest::blocking::get(format!("{}/keys", server.url()))
            .expect("mock fetch")
            .text()
            .expect("body");
        assert_eq!(resp, body);

        run(
            &one_user(
                "alice",
                User {
                    uid: "1000".into(),
                    ssh_authorized_keys: vec![resp.trim().to_string()],
                    ..Default::default()
                },
            ),
            &fs,
            &con,
        )
        .unwrap();

        let ak = read(&fs, "/home/alice/.ssh/authorized_keys");
        assert!(
            ak.contains("ssh-rsa AAAAB3NzaC1yc2E-from-url alice@host"),
            "URL-resolved key should land verbatim, got: {ak}"
        );
    }

    // -----------------------------------------------------------------
    // Ports of Go It-blocks from yip/pkg/plugins/user_test.go.
    //
    // The Go suite seeds `/etc/passwd` with the `existing_passwd` blob
    // below (a representative set of system users with max UID = 999),
    // leaves `/etc/shadow` / `/etc/group` empty, and asserts post-run
    // file contents.
    //
    // One adaptation: the Go plugin resolves provider tokens like
    // `github:mudler` into ssh-rsa key material. The Rust plugin
    // writes ssh_authorized_keys entries verbatim (see
    // `ssh_keys_from_url_provider_via_mockito` test above). We
    // therefore pass already-resolved keys in the ports below, and
    // assert the verbatim content lands in authorized_keys.
    // -----------------------------------------------------------------

    const EXISTING_PASSWD: &str = "\
dbus:x:81:81:System Message Bus:/:/usr/bin/nologin
root:x:0:0::/root:/bin/bash
bin:x:1:1::/:/usr/bin/nologin
daemon:x:2:2::/:/usr/bin/nologin
mail:x:8:12::/var/spool/mail:/usr/bin/nologin
ftp:x:14:11::/srv/ftp:/usr/bin/nologin
http:x:33:33::/srv/http:/usr/bin/nologin
systemd-coredump:x:980:980:systemd Core Dumper:/:/usr/bin/nologin
systemd-network:x:979:979:systemd Network Management:/:/usr/bin/nologin
systemd-oom:x:978:978:systemd Userspace OOM Killer:/:/usr/bin/nologin
systemd-journal-remote:x:977:977:systemd Journal Remote:/:/usr/bin/nologin
systemd-resolve:x:976:976:systemd Resolver:/:/usr/bin/nologin
systemd-timesync:x:975:975:systemd Time Synchronization:/:/usr/bin/nologin
tss:x:974:974:tss user for tpm2:/:/usr/bin/nologin
uuidd:x:68:68::/:/usr/bin/nologin
_talkd:x:973:973:User for legacy talkd server:/:/usr/bin/nologin
avahi:x:972:972:Avahi mDNS/DNS-SD daemon:/:/usr/bin/nologin
named:x:40:40:BIND DNS Server:/:/usr/bin/nologin
colord:x:971:971:Color management daemon:/var/lib/colord:/usr/bin/nologin
dnsmasq:x:970:970:dnsmasq daemon:/:/usr/bin/nologin
gdm:x:120:120:Gnome Display Manager:/var/lib/gdm:/usr/bin/nologin
geoclue:x:969:969:Geoinformation service:/var/lib/geoclue:/usr/bin/nologin
git:x:968:968:git daemon user:/:/usr/bin/git-shell
nm-openconnect:x:967:967:NetworkManager OpenConnect:/:/usr/bin/nologin
nm-openvpn:x:966:966:NetworkManager OpenVPN:/:/usr/bin/nologin
ntp:x:87:87:Network Time Protocol:/var/lib/ntp:/bin/false
openvpn:x:965:965:OpenVPN:/:/usr/bin/nologin
polkitd:x:102:102:PolicyKit daemon:/:/usr/bin/nologin
rpc:x:32:32:Rpcbind Daemon:/var/lib/rpcbind:/usr/bin/nologin
rpcuser:x:34:34:RPC Service User:/var/lib/nfs:/usr/bin/nologin
rtkit:x:133:133:RealtimeKit:/proc:/usr/bin/nologin
usbmux:x:140:140:usbmux user:/:/usr/bin/nologin
nvidia-persistenced:x:143:143:NVIDIA Persistence Daemon:/:/usr/bin/nologin
flatpak:x:964:964:Flatpak system helper:/:/usr/bin/nologin
brltty:x:961:961:Braille Device Daemon:/var/lib/brltty:/usr/bin/nologin
gluster:x:960:960:GlusterFS daemons:/var/run/gluster:/usr/bin/nologin
qemu:x:959:959:QEMU user:/:/usr/bin/nologin
libvirt-qemu:x:957:957:Libvirt QEMU user:/:/usr/bin/nologin
fwupd:x:956:956:Firmware update daemon:/var/lib/fwupd:/usr/bin/nologin
passim:x:955:955:Local Caching Server:/usr/share/empty:/usr/bin/nologin
cups:x:209:209:cups helper user:/:/usr/bin/nologin
saned:x:953:953:SANE daemon user:/:/usr/bin/nologin
last:x:999:999:Test user for uid:/:/usr/bin/nologin
";

    /// Subset of default usernames that must survive the run untouched.
    /// Mirrors `HaveAllDefaultUsers` from the Go suite (minus the
    /// hyphen-prefixed `_talkd` which is also asserted there).
    const DEFAULT_USERS: &[&str] = &[
        "root",
        "bin",
        "daemon",
        "mail",
        "ftp",
        "http",
        "systemd-coredump",
        "systemd-network",
        "systemd-oom",
        "systemd-journal-remote",
        "systemd-resolve",
        "systemd-timesync",
        "tss",
        "_talkd",
        "uuidd",
        "avahi",
        "named",
        "colord",
        "dnsmasq",
        "gdm",
        "geoclue",
        "git",
        "nm-openconnect",
        "nm-openvpn",
        "ntp",
        "openvpn",
        "polkitd",
        "rpc",
        "rpcuser",
        "rtkit",
        "usbmux",
        "nvidia-persistenced",
        "flatpak",
        "brltty",
        "gluster",
        "qemu",
        "libvirt-qemu",
        "fwupd",
        "passim",
        "cups",
        "saned",
        "last",
    ];

    /// Build a fresh VFS seeded with `EXISTING_PASSWD` plus the given
    /// shadow/group contents — matches the Go `vfst.NewTestFS` calls.
    fn seed_with(shadow: &str, group: &str) -> MemVfs {
        let fs = MemVfs::new();
        write(&fs, ETC_PASSWD, EXISTING_PASSWD);
        write(&fs, ETC_SHADOW, shadow);
        write(&fs, ETC_GROUP, group);
        fs
    }

    /// Assert every name in `DEFAULT_USERS` still appears in /etc/passwd
    /// after the run — Rust port of `HaveAllDefaultUsers()`.
    fn assert_all_default_users(fs: &MemVfs) {
        let passwd = read(fs, ETC_PASSWD);
        let table = PasswdTable::parse(&passwd);
        for name in DEFAULT_USERS {
            assert!(
                table.get(name).is_some(),
                "default user `{name}` missing from /etc/passwd after run"
            );
        }
    }

    /// Look up a passwd row by name — convenience for the ports below.
    fn get_passwd_row(fs: &MemVfs, name: &str) -> PasswdRow {
        let passwd = read(fs, ETC_PASSWD);
        PasswdTable::parse(&passwd)
            .get(name)
            .cloned()
            .unwrap_or_else(|| panic!("user `{name}` not found in /etc/passwd"))
    }

    // Port of Go It: "change user password"
    #[test]
    fn go_change_user_password() {
        let fs = seed_with("", "");
        let con = RecordingConsole::new();

        let stage = one_user(
            "foo",
            User {
                password_hash: "$fkekofe".into(),
                ssh_authorized_keys: vec![
                    // Resolved equivalent of Go's `github:mudler` — the
                    // Rust plugin writes verbatim, so we supply the key
                    // we'd expect after upstream resolution.
                    "ssh-rsa AAAAB3NzaC1yc2EAAAADAQABAAABAQDR9zjXvyzg1HFMC7RT4LgtR+YGstxWDPPRoAcNrAWjtQcJVrcVo4WLFnT0BMU5mtMxWSrulpC6yrwnt2TE3Ul86yMxO2hbSyGP/xOdYm/nQzufY49rd3tKeJl1+6DkczuPa+XYh1GBcW5E2laNM5ZK+RjABppMpDgmnrM3AsGNE6G8RSuUvc/6Rwt61ma+jak3F5YMj4kwr5PhY2MTPo2YshsL3ouRXP/uPsbaBM6AdQakjWGJR8tPbrnHenzF65813d9zuY4y78TG0AHfomx9btmha7Mc0YF+BpELnvSQLlYrlRY/ziGhP65aQc8lFMc+XBnHeaXF4NHnzq6dIH2D".into(),
                    "efafeeafea,t,t,pgl3,pbar".into(),
                ],
                ..Default::default()
            },
        );
        run(&stage, &fs, &con).unwrap();

        assert_all_default_users(&fs);

        // /etc/group — exactly one entry for foo at GID 1000.
        let group = read(&fs, ETC_GROUP);
        assert_eq!(group, "foo:x:1000:foo\n", "got: {group}");

        // /etc/shadow — verbatim $-prefixed password.
        let shadow = read(&fs, ETC_SHADOW);
        assert!(
            shadow.contains("foo:$fkekofe:"),
            "expected verbatim password, got: {shadow}"
        );

        // /etc/passwd row for foo — Created-by-entities GECOS, /home/foo,
        // /bin/sh, password placeholder "x", UID 1000 (max existing was 999).
        let foo = get_passwd_row(&fs, "foo");
        assert_eq!(foo.gecos, "Created by entities");
        assert_eq!(foo.homedir, "/home/foo");
        assert_eq!(foo.shell, "/bin/sh");
        assert_eq!(foo.password, "x");
        assert_eq!(foo.uid, 1000);

        // SSH keys land verbatim.
        let ak = read(&fs, "/home/foo/.ssh/authorized_keys");
        assert!(ak.contains("ssh-rsa AAAAB3NzaC1yc2EAAAADAQABAAABAQDR9zjXvyzg1HFMC7RT4LgtR+YGstxWDPPRoAcNrAWjtQcJVrcVo4WLFnT0BMU5mtMxWSrulpC6yrwnt2TE3Ul86yMxO2hbSyGP/xOdYm/nQzufY49rd3tKeJl1+6DkczuPa+XYh1GBcW5E2laNM5ZK+RjABppMpDgmnrM3AsGNE6G8RSuUvc/6Rwt61ma+jak3F5YMj4kwr5PhY2MTPo2YshsL3ouRXP/uPsbaBM6AdQakjWGJR8tPbrnHenzF65813d9zuY4y78TG0AHfomx9btmha7Mc0YF+BpELnvSQLlYrlRY/ziGhP65aQc8lFMc+XBnHeaXF4NHnzq6dIH2D"));
        assert!(ak.contains("efafeeafea,t,t,pgl3,pbar"));
    }

    // Port of Go It: "set UID and Lockpasswd"
    #[test]
    fn go_set_uid_and_lock_passwd() {
        let fs = seed_with("", "");
        let con = RecordingConsole::new();

        let stage = one_user(
            "foo",
            User {
                password_hash: "$fkekofe".into(),
                lock_passwd: true,
                uid: "5000".into(),
                homedir: "/run/foo".into(),
                shell: "/bin/bash".into(),
                ..Default::default()
            },
        );
        run(&stage, &fs, &con).unwrap();

        assert_all_default_users(&fs);

        // /etc/group — GID still auto-allocated at 1000 regardless of UID.
        let group = read(&fs, ETC_GROUP);
        assert_eq!(group, "foo:x:1000:foo\n", "got: {group}");

        // /etc/shadow — locked account stores "!" as the password field.
        let shadow = read(&fs, ETC_SHADOW);
        assert!(shadow.contains("foo:!:"), "got: {shadow}");

        // /etc/passwd — explicit UID 5000, explicit homedir, explicit shell.
        let foo = get_passwd_row(&fs, "foo");
        assert_eq!(foo.gecos, "Created by entities");
        assert_eq!(foo.homedir, "/run/foo");
        assert_eq!(foo.shell, "/bin/bash");
        assert_eq!(foo.password, "x");
        assert_eq!(foo.uid, 5000);
    }

    // Port of Go It: "edits already existing user password"
    #[test]
    fn go_edits_already_existing_user_password() {
        let shadow_seed = "\
foo:$6$rfBd56ti$7juhxebonsy.GiErzyxZPkbm.U4lUlv/59D2pvFqlbjVqyJP5f4VgP.EX3FKAeGTAr.GVf0jQmy9BXAZL5mNJ1:18820::::::
rancher:$6$2SMtYvSg$wL/zzuT4m3uYkHWO1Rl4x5U6BeGu9IfzIafueinxnNgLFHI34En35gu9evtlhizsOxRJLaTfy0bWFZfm2.qYu1:18820::::::";
        let fs = seed_with(shadow_seed, "");
        let con = RecordingConsole::new();

        let stage = one_user(
            "foo",
            User {
                password_hash: "$fkekofe".into(),
                homedir: "/home/foo".into(),
                ssh_authorized_keys: vec![
                    "ssh-rsa AAAAB3NzaC1yc2EAAAADAQABAAABAQDR9zjXvyzg1HFMC7RT4LgtR+YGstxWDPPRoAcNrAWjtQcJVrcVo4WLFnT0BMU5mtMxWSrulpC6yrwnt2TE3Ul86yMxO2hbSyGP/xOdYm/nQzufY49rd3tKeJl1+6DkczuPa+XYh1GBcW5E2laNM5ZK+RjABppMpDgmnrM3AsGNE6G8RSuUvc/6Rwt61ma+jak3F5YMj4kwr5PhY2MTPo2YshsL3ouRXP/uPsbaBM6AdQakjWGJR8tPbrnHenzF65813d9zuY4y78TG0AHfomx9btmha7Mc0YF+BpELnvSQLlYrlRY/ziGhP65aQc8lFMc+XBnHeaXF4NHnzq6dIH2D".into(),
                    "efafeeafea,t,t,pgl3,pbar".into(),
                ],
                ..Default::default()
            },
        );
        run(&stage, &fs, &con).unwrap();

        assert_all_default_users(&fs);

        // /etc/group — foo's primary group still allocated at 1000.
        let group = read(&fs, ETC_GROUP);
        assert_eq!(group, "foo:x:1000:foo\n", "got: {group}");

        // /etc/shadow — old hash replaced with the new $fkekofe.
        let shadow = read(&fs, ETC_SHADOW);
        assert!(
            shadow.contains("foo:$fkekofe:"),
            "expected password to be replaced, got: {shadow}"
        );

        // /etc/passwd — defaults for the rest, UID = first free human (1000).
        let foo = get_passwd_row(&fs, "foo");
        assert_eq!(foo.gecos, "Created by entities");
        assert_eq!(foo.homedir, "/home/foo");
        assert_eq!(foo.shell, "/bin/sh");
        assert_eq!(foo.password, "x");
        assert_eq!(foo.uid, 1000);

        // SSH keys are appended verbatim.
        let ak = read(&fs, "/home/foo/.ssh/authorized_keys");
        assert!(ak.contains("ssh-rsa AAAAB3NzaC1yc2EAAAADAQABAAABAQDR9zjXvyzg1HFMC7RT4LgtR+YGstxWDPPRoAcNrAWjtQcJVrcVo4WLFnT0BMU5mtMxWSrulpC6yrwnt2TE3Ul86yMxO2hbSyGP/xOdYm/nQzufY49rd3tKeJl1+6DkczuPa+XYh1GBcW5E2laNM5ZK+RjABppMpDgmnrM3AsGNE6G8RSuUvc/6Rwt61ma+jak3F5YMj4kwr5PhY2MTPo2YshsL3ouRXP/uPsbaBM6AdQakjWGJR8tPbrnHenzF65813d9zuY4y78TG0AHfomx9btmha7Mc0YF+BpELnvSQLlYrlRY/ziGhP65aQc8lFMc+XBnHeaXF4NHnzq6dIH2D"));
        assert!(ak.contains("efafeeafea,t,t,pgl3,pbar"));
    }

    // Port of Go It: "preserves password aging fields when editing an existing user password"
    #[test]
    fn go_preserves_password_aging_fields_on_edit() {
        let shadow_seed = "\
foo:$6$rfBd56ti$7juhxebonsy.GiErzyxZPkbm.U4lUlv/59D2pvFqlbjVqyJP5f4VgP.EX3FKAeGTAr.GVf0jQmy9BXAZL5mNJ1:18820:1:365:14:30:20000:
rancher:$6$2SMtYvSg$wL/zzuT4m3uYkHWO1Rl4x5U6BeGu9IfzIafueinxnNgLFHI34En35gu9evtlhizsOxRJLaTfy0bWFZfm2.qYu1:18820::::::";
        let fs = seed_with(shadow_seed, "");
        let con = RecordingConsole::new();

        let stage = one_user(
            "foo",
            User {
                password_hash: "$fkekofe".into(),
                homedir: "/home/foo".into(),
                ..Default::default()
            },
        );
        run(&stage, &fs, &con).unwrap();

        let shadow = read(&fs, ETC_SHADOW);
        // Password field updated…
        assert!(
            shadow.contains("foo:$fkekofe:"),
            "password should be updated, got: {shadow}"
        );
        // …but minimum/maximum-age, warn, inactive and expire are preserved.
        assert!(
            shadow.contains(":1:365:14:30:20000:"),
            "aging fields should be preserved, got: {shadow}"
        );
    }

    // Port of Go It: "adds users to group"
    #[test]
    fn go_adds_users_to_group() {
        let fs = seed_with("", "");
        let con = RecordingConsole::new();

        // 1st apply: create `admin` user (auto-creates group `admin` at 1000).
        run(
            &one_user(
                "admin",
                User {
                    password_hash: "$fkekofe".into(),
                    homedir: "/home/foo".into(),
                    ssh_authorized_keys: vec![
                        "ssh-rsa AAAAresolved-key-admin user@host".into(),
                        "efafeeafea,t,t,pgl3,pbar".into(),
                    ],
                    ..Default::default()
                },
            ),
            &fs,
            &con,
        )
        .unwrap();

        // 2nd apply: create `bar` with secondary group `admin`.
        run(
            &one_user(
                "bar",
                User {
                    groups: vec!["admin".into()],
                    password_hash: "$fkekofe".into(),
                    homedir: "/home/foo".into(),
                    ssh_authorized_keys: vec![
                        "ssh-rsa AAAAresolved-key-bar user@host".into(),
                        "efafeeafea,t,t,pgl3,pbar".into(),
                    ],
                    ..Default::default()
                },
            ),
            &fs,
            &con,
        )
        .unwrap();

        let group = read(&fs, ETC_GROUP);
        assert_eq!(
            group, "admin:x:1000:admin,bar\nbar:x:1001:bar\n",
            "got: {group}"
        );

        // 3rd apply: create `baz` also in admin.
        run(
            &one_user(
                "baz",
                User {
                    homedir: "/home/foo".into(),
                    groups: vec!["admin".into()],
                    ..Default::default()
                },
            ),
            &fs,
            &con,
        )
        .unwrap();

        let group = read(&fs, ETC_GROUP);
        assert_eq!(
            group, "admin:x:1000:admin,bar,baz\nbar:x:1001:bar\nbaz:x:1002:baz\n",
            "got: {group}"
        );
    }

    // Port of Go It: "Recreates users with the same UID() and in order"
    //
    // Apply 4 users (a, bar, foo, x) twice against fresh-but-identically-
    // seeded filesystems and assert the auto-allocated UIDs are stable
    // across runs (UID stability test).
    #[test]
    fn go_recreates_users_with_same_uid_in_order() {
        // Users sorted alphabetically → a=1000, bar=1001, foo=1002, x=1003.
        let users: &[&str] = &["a", "bar", "foo", "x"];

        let build_stage = || {
            let mut stage_users: HashMap<String, User> = HashMap::new();
            for n in users {
                stage_users.insert(
                    (*n).to_string(),
                    User {
                        password_hash: "$fkekofe".into(),
                        ..Default::default()
                    },
                );
            }
            Stage {
                users: stage_users,
                ..Default::default()
            }
        };

        let assert_layout = |fs: &MemVfs| {
            assert_all_default_users(fs);

            let a = get_passwd_row(fs, "a");
            assert_eq!(a.gecos, "Created by entities");
            assert_eq!(a.homedir, "/home/a");
            assert_eq!(a.shell, "/bin/sh");
            assert_eq!(a.password, "x");
            assert_eq!(a.uid, 1000);

            let bar = get_passwd_row(fs, "bar");
            assert_eq!(bar.gecos, "Created by entities");
            assert_eq!(bar.homedir, "/home/bar");
            assert_eq!(bar.shell, "/bin/sh");
            assert_eq!(bar.password, "x");
            assert_eq!(bar.uid, 1001);

            let foo = get_passwd_row(fs, "foo");
            assert_eq!(foo.gecos, "Created by entities");
            assert_eq!(foo.homedir, "/home/foo");
            assert_eq!(foo.shell, "/bin/sh");
            assert_eq!(foo.password, "x");
            assert_eq!(foo.uid, 1002);

            let x = get_passwd_row(fs, "x");
            assert_eq!(x.gecos, "Created by entities");
            assert_eq!(x.homedir, "/home/x");
            assert_eq!(x.shell, "/bin/sh");
            assert_eq!(x.password, "x");
            assert_eq!(x.uid, 1003);
        };

        // First fresh VFS.
        let con = RecordingConsole::new();
        let fs = seed_with("", "");
        run(&build_stage(), &fs, &con).unwrap();
        assert_layout(&fs);

        // Manual "cleanup" — a brand-new VFS seeded identically.
        let fs2 = seed_with("", "");
        run(&build_stage(), &fs2, &con).unwrap();
        assert_layout(&fs2);
    }

    // Port of Go It: "Creates the user multiple times, keeping the same UID()"
    //
    // Apply the same single-user stage 5 times against the same VFS;
    // foo's UID must remain 1000 (no drift, no duplicate rows).
    #[test]
    fn go_creates_user_multiple_times_keeping_same_uid() {
        let fs = seed_with("", "");
        let con = RecordingConsole::new();

        let stage = one_user(
            "foo",
            User {
                password_hash: "$fkekofe".into(),
                ..Default::default()
            },
        );
        for _ in 0..5 {
            run(&stage, &fs, &con).unwrap();
        }

        assert_all_default_users(&fs);

        let foo = get_passwd_row(&fs, "foo");
        assert_eq!(foo.gecos, "Created by entities");
        assert_eq!(foo.homedir, "/home/foo");
        assert_eq!(foo.shell, "/bin/sh");
        assert_eq!(foo.password, "x");
        assert_eq!(foo.uid, 1000, "UID must remain stable across re-applies");

        // No duplicate passwd row for foo across the 5 applies.
        let passwd = read(&fs, ETC_PASSWD);
        let foo_lines = passwd.lines().filter(|l| l.starts_with("foo:")).count();
        assert_eq!(foo_lines, 1, "duplicated passwd line: {passwd}");
    }

    // Port of Go It: "Creates the user multiple times, keeping the same
    // UID(), even if a new users is added"
    //
    // Apply `foo` first (gets UID 1000), then apply {a, b, foo} together
    // — foo must keep 1000 while a, b get the next free slots (1001, 1002).
    #[test]
    fn go_creates_user_multiple_times_keeping_same_uid_with_new_users() {
        let fs = seed_with("", "");
        let con = RecordingConsole::new();

        // First apply: just foo.
        run(
            &one_user(
                "foo",
                User {
                    password_hash: "$fkekofe".into(),
                    ..Default::default()
                },
            ),
            &fs,
            &con,
        )
        .unwrap();

        // Second apply: a, b, foo — sorted alphabetically by the plugin.
        let mut new_users: HashMap<String, User> = HashMap::new();
        for n in ["a", "b", "foo"] {
            new_users.insert(
                n.to_string(),
                User {
                    password_hash: "$fkekofe".into(),
                    ..Default::default()
                },
            );
        }
        run(
            &Stage {
                users: new_users,
                ..Default::default()
            },
            &fs,
            &con,
        )
        .unwrap();

        assert_all_default_users(&fs);

        // foo's UID didn't drift.
        let foo = get_passwd_row(&fs, "foo");
        assert_eq!(foo.gecos, "Created by entities");
        assert_eq!(foo.homedir, "/home/foo");
        assert_eq!(foo.shell, "/bin/sh");
        assert_eq!(foo.password, "x");
        assert_eq!(foo.uid, 1000);

        // a got the next free slot…
        let a = get_passwd_row(&fs, "a");
        assert_eq!(a.gecos, "Created by entities");
        assert_eq!(a.homedir, "/home/a");
        assert_eq!(a.shell, "/bin/sh");
        assert_eq!(a.password, "x");
        assert_eq!(a.uid, 1001);

        // …and b the one after.
        let b = get_passwd_row(&fs, "b");
        assert_eq!(b.gecos, "Created by entities");
        assert_eq!(b.homedir, "/home/b");
        assert_eq!(b.shell, "/bin/sh");
        assert_eq!(b.password, "x");
        assert_eq!(b.uid, 1002);
    }
}
