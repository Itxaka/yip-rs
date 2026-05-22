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
}
