//! Top-level YAML config types.
//!
//! [`Config`] (alias [`YipConfig`]) is the root document yip parses. It
//! holds a friendly `name` plus a map of `stages` keyed by stage name —
//! e.g. `rootfs`, `rootfs.before`, `network.after`, `initramfs`. Each
//! stage entry is a list of [`Stage`] structs, which carry the actual
//! actions (files to write, commands to run, packages to install, …).
//!
//! ## Loading
//!
//! - [`Config::load`] parses raw bytes (already template-rendered).
//! - [`Config::load_file`] reads a path and tags `source` automatically.
//! - [`Config::to_string_yaml`] serialises back to canonical YAML.
//!
//! The executor calls `load` itself after rendering templates and
//! applying any pre-parse modifier, so end-users normally don't have to
//! touch these methods.
//!
//! # Examples
//!
//! ```
//! use yip::schema::Config;
//!
//! let cfg = Config::load(b"name: demo\nstages:\n  rootfs:\n    - name: hi\n").unwrap();
//! assert_eq!(cfg.name, "demo");
//! assert_eq!(cfg.stages["rootfs"].len(), 1);
//! ```
//!
//! # Stability
//!
//! Public API. Field names match the YAML keys (via `serde rename`);
//! changing them is a breaking change.

use std::collections::HashMap;
use std::path::Path;

use saphyr::{LoadableYamlNode, Mapping as SaphyrMapping, Yaml, YamlEmitter};
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::schema::file::File;
use crate::schema::stage::Stage;
use crate::schema::user::User;

/// Root of a yip YAML document.
///
/// Two YAML keys: `name` (a free-form label) and `stages` (a map from
/// stage key to a list of [`Stage`] structs). Anything else in the YAML
/// is rejected by `serde` unless `Stage` accepts it.
///
/// `source` is metadata set by [`Config::load_file`] (and the executor
/// when it knows the origin) — it does not round-trip through YAML.
///
/// # Examples
///
/// ```
/// use yip::schema::Config;
///
/// let cfg = Config::default();
/// assert!(cfg.name.is_empty());
/// assert!(cfg.stages.is_empty());
/// ```
#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Config {
    /// Path or URL this config was loaded from. Not stored in YAML —
    /// populated by [`Config::load_file`] / by the executor.
    #[serde(skip)]
    pub source: String,

    /// Free-form label for the config. Maps to YAML key `name`.
    #[serde(default, rename = "name", skip_serializing_if = "String::is_empty")]
    pub name: String,

    /// Map of stage key (e.g. `"rootfs"`, `"rootfs.before"`) to the
    /// ordered list of [`Stage`] entries that run for it. Maps to YAML
    /// key `stages`.
    #[serde(default, rename = "stages", skip_serializing_if = "HashMap::is_empty")]
    pub stages: HashMap<String, Vec<Stage>>,
}

/// Go-flavoured alias so a porting reader can still type `YipConfig`.
///
/// # Examples
///
/// ```
/// use yip::schema::{Config, YipConfig};
///
/// let _: YipConfig = Config::default();
/// ```
pub type YipConfig = Config;

impl Config {
    /// Parse a YAML byte slice into a [`Config`].
    ///
    /// Mirrors `schema.Load` from Go yip minus the cloud-init detection
    /// branch — this is the yip-native YAML path. The caller is
    /// responsible for template rendering and any pre-parse modifier
    /// (the executor handles both for you).
    ///
    /// # Errors
    ///
    /// Returns [`Error::Yaml`](crate::error::Error::Yaml) when the bytes
    /// aren't valid YAML or don't match the schema.
    ///
    /// # Examples
    ///
    /// ```
    /// use yip::schema::Config;
    ///
    /// let cfg = Config::load(b"name: t\n").unwrap();
    /// assert_eq!(cfg.name, "t");
    /// ```
    pub fn load(bytes: &[u8]) -> Result<Self> {
        if has_cloud_config_header(bytes) {
            return load_from_cloud_config(bytes);
        }
        let expanded = expand_merge_keys(bytes)?;
        let cfg: Config = serde_yaml::from_str(&expanded)?;
        Ok(cfg)
    }

    /// Load a config from a file path; sets `source` to the path string.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Io`](crate::error::Error::Io) if the file can't
    /// be read, or [`Error::Yaml`](crate::error::Error::Yaml) if the
    /// contents don't parse.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use yip::schema::Config;
    ///
    /// let cfg = Config::load_file("/etc/yip/conf.yaml").unwrap();
    /// assert_eq!(cfg.source, "/etc/yip/conf.yaml");
    /// ```
    pub fn load_file(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let bytes = std::fs::read(path).map_err(|e| Error::io_at(path, e))?;
        let mut cfg = Self::load(&bytes)?;
        cfg.source = path.display().to_string();
        Ok(cfg)
    }

    /// Serialise to YAML. Mirrors Go's `YipConfig.ToString`.
    ///
    /// Empty fields are skipped (`skip_serializing_if`) so the output is
    /// minimal.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Yaml`](crate::error::Error::Yaml) on serialiser
    /// failure (rare — usually only happens on cycles).
    ///
    /// # Examples
    ///
    /// ```
    /// use yip::schema::Config;
    ///
    /// let cfg = Config { name: "t".into(), ..Default::default() };
    /// let s = cfg.to_string_yaml().unwrap();
    /// assert!(s.contains("name: t"));
    /// ```
    pub fn to_string_yaml(&self) -> Result<String> {
        Ok(serde_yaml::to_string(self)?)
    }
}

/// Pre-process YAML bytes to expand `<<: *anchor` merge keys.
///
/// `serde_yaml` 0.9 does not implement the YAML 1.1 merge-key extension, but
/// Kairos configs frequently rely on it to share defaults across stages.
/// We parse the YAML with [`saphyr`] (which resolves anchors/aliases by
/// inlining the referenced node), walk the resulting tree, and for every
/// mapping that contains a literal `<<` key we merge the referenced
/// mapping(s) into it before re-emitting the document as canonical YAML.
///
/// Merge semantics follow the YAML 1.1 spec: keys already present in the
/// owning mapping take precedence over keys pulled in from the merge value.
/// When the merge value is a sequence of mappings, earlier elements take
/// precedence over later ones (again, with the owning mapping winning over
/// all of them).
///
/// If the input does not parse as YAML the corresponding error is surfaced
/// as [`Error::Schema`]. Inputs without any `<<` keys are still round-tripped
/// through saphyr — this is intentional, since saphyr also doubles as an
/// anchor resolver should the user define anchors without merge keys.
fn expand_merge_keys(bytes: &[u8]) -> Result<String> {
    let raw = std::str::from_utf8(bytes)
        .map_err(|e| Error::Schema(format!("yaml not utf8: {e}")))?;
    let mut docs = Yaml::load_from_str(raw)
        .map_err(|e| Error::Schema(format!("yaml parse: {e}")))?;
    // Empty input → empty output. saphyr 0.0.6's `Yaml` has no `Null`
    // variant exposed at the top level, so we bail early on empty docs.
    if docs.is_empty() {
        return Ok(String::new());
    }
    let mut doc = docs.drain(..).next().expect("docs non-empty");
    expand_merges_in_node(&mut doc);

    let mut out = String::new();
    {
        let mut emitter = YamlEmitter::new(&mut out);
        emitter
            .dump(&doc)
            .map_err(|e| Error::Schema(format!("yaml emit: {e}")))?;
    }
    Ok(out)
}

/// Recursively expand `<<` merge keys in a saphyr [`Yaml`] tree, in place.
fn expand_merges_in_node(node: &mut Yaml<'_>) {
    match node {
        Yaml::Mapping(map) => {
            // First recurse into every value so nested mappings get expanded
            // before we merge anything at this level.
            for (_, v) in map.iter_mut() {
                expand_merges_in_node(v);
            }

            // Extract and remove the `<<` entry if present. We can't mutate
            // the map while iterating, so we look up the key explicitly.
            let merge_key = Yaml::Value(saphyr::Scalar::String(std::borrow::Cow::Borrowed("<<")));
            if let Some(merge_value) = map.remove(&merge_key) {
                merge_into(map, merge_value);
            }
        }
        Yaml::Sequence(seq) => {
            for item in seq.iter_mut() {
                expand_merges_in_node(item);
            }
        }
        Yaml::Tagged(_, inner) => expand_merges_in_node(inner.as_mut()),
        _ => {}
    }
}

/// Merge the value of a `<<` key into the owning mapping.
///
/// The value may be a single mapping (`<<: *anchor`) or a sequence of
/// mappings (`<<: [*a, *b]`). Existing keys in `target` are never overwritten;
/// when the value is a sequence, earlier elements take precedence over later
/// ones.
fn merge_into<'input>(target: &mut SaphyrMapping<'input>, value: Yaml<'input>) {
    match value {
        Yaml::Mapping(src) => merge_mapping(target, src),
        Yaml::Sequence(items) => {
            for item in items {
                if let Yaml::Mapping(src) = item {
                    merge_mapping(target, src);
                }
                // Non-mapping entries in a `<<` sequence are spec-invalid;
                // we silently skip them to stay forgiving of weird input.
            }
        }
        // A `<<` whose value is a scalar/null is meaningless — drop it.
        _ => {}
    }
}

/// Insert every (k, v) from `src` into `target`, but only when `k` is not
/// already present. Mirrors YAML 1.1 merge precedence: explicit keys in the
/// owning mapping win.
fn merge_mapping<'input>(
    target: &mut SaphyrMapping<'input>,
    src: SaphyrMapping<'input>,
) {
    for (k, v) in src {
        if !target.contains_key(&k) {
            target.insert(k, v);
        }
    }
}

/// Returns true if `bytes` begins with a `#cloud-config` header line.
///
/// Matches Go yip's `cloudinit.IsCloudConfig`: any leading blank lines or
/// comment lines other than `#cloud-config` reject the header. Case-insensitive
/// on the literal text, tolerant of `# cloud-config` (with a space) and
/// trailing whitespace before the newline.
fn has_cloud_config_header(bytes: &[u8]) -> bool {
    // Try the first non-empty line. We allow up to ~10 leading lines like Go
    // does (in practice the header has to be on line 1 for cloud-init, but
    // Go is lenient about leading blank lines).
    let s = match std::str::from_utf8(bytes) {
        Ok(s) => s,
        Err(_) => return false,
    };
    for (idx, line) in s.split('\n').enumerate() {
        if idx >= 10 {
            return false;
        }
        let trimmed = line.trim_end_matches([' ', '\t', '\r']);
        if trimmed.is_empty() {
            continue;
        }
        // Strip leading whitespace just for the header check.
        let header = trimmed.trim_start();
        if !header.starts_with('#') {
            return false;
        }
        // After `#`, allow optional whitespace, then literal "cloud-config".
        let after_hash = header[1..].trim_start();
        if after_hash.eq_ignore_ascii_case("cloud-config") {
            return true;
        }
        // Any other comment line: keep scanning (matches Go's loop).
    }
    false
}

/// Parse a `#cloud-config` style YAML document and translate it into a
/// yip-native [`Config`].
///
/// Mirrors Go yip's `loader_cloudinit.go` but simplified for the keys called
/// out by the immucore port spec. Unknown keys are silently ignored.
fn load_from_cloud_config(bytes: &[u8]) -> Result<Config> {
    let raw: serde_yaml::Value = serde_yaml::from_slice(bytes)?;
    let map = match raw {
        serde_yaml::Value::Mapping(m) => m,
        serde_yaml::Value::Null => serde_yaml::Mapping::new(),
        _ => {
            return Err(Error::other(
                "cloud-config root must be a YAML mapping",
            ));
        }
    };

    // Match Go yip: stages produced from #cloud-config carry no explicit name
    // (the struct is constructed anonymously).
    let mut stage = Stage::default();
    // `boot.before` stage carries bootcmd, if present.
    let mut before_commands: Vec<String> = Vec::new();
    // Track explicit per-user keys so a global ssh_authorized_keys is only
    // assigned to `root` when no users supplied any keys (matches Go).
    let mut has_user_keys = false;

    // hostname -> stage.hostname
    if let Some(v) = map.get(&serde_yaml::Value::String("hostname".into())) {
        if let Some(s) = v.as_str() {
            stage.hostname = s.to_string();
        }
    }

    // users (list) -> stage.users + stage.commands (sudo lines)
    let mut users_map: HashMap<String, User> = HashMap::new();
    let mut sudo_commands: Vec<String> = Vec::new();
    if let Some(serde_yaml::Value::Sequence(users)) =
        map.get(&serde_yaml::Value::String("users".into()))
    {
        for u in users {
            let serde_yaml::Value::Mapping(um) = u else {
                continue;
            };
            let name = match um.get(&serde_yaml::Value::String("name".into())) {
                Some(serde_yaml::Value::String(s)) if !s.is_empty() => s.clone(),
                _ => continue,
            };

            let mut user = User {
                name: name.clone(),
                ..Default::default()
            };
            if let Some(serde_yaml::Value::Sequence(g)) =
                um.get(&serde_yaml::Value::String("groups".into()))
            {
                user.groups = g
                    .iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect();
            }
            if let Some(serde_yaml::Value::Sequence(keys)) =
                um.get(&serde_yaml::Value::String("ssh_authorized_keys".into()))
            {
                user.ssh_authorized_keys = keys
                    .iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect();
                if !user.ssh_authorized_keys.is_empty() {
                    has_user_keys = true;
                }
            }
            if let Some(serde_yaml::Value::String(pw)) =
                um.get(&serde_yaml::Value::String("passwd".into()))
            {
                user.password_hash = pw.clone();
            }
            if let Some(v) = um.get(&serde_yaml::Value::String("lock_passwd".into())) {
                if let Some(b) = v.as_bool() {
                    user.lock_passwd = b;
                }
            }
            if let Some(serde_yaml::Value::String(sh)) =
                um.get(&serde_yaml::Value::String("shell".into()))
            {
                user.shell = sh.clone();
            }
            if let Some(serde_yaml::Value::String(hd)) =
                um.get(&serde_yaml::Value::String("homedir".into()))
            {
                user.homedir = hd.clone();
            }

            // sudo string -> append `echo '<sudo>' >> /etc/sudoers.d/<name>`
            if let Some(serde_yaml::Value::String(sudo)) =
                um.get(&serde_yaml::Value::String("sudo".into()))
            {
                if !sudo.is_empty() {
                    sudo_commands.push(format!(
                        "echo '{}' >> /etc/sudoers.d/{}",
                        sudo, name
                    ));
                }
            }

            users_map.insert(name, user);
        }
    }
    if !users_map.is_empty() {
        stage.users = users_map;
    }

    // write_files -> stage.files
    if let Some(serde_yaml::Value::Sequence(files)) =
        map.get(&serde_yaml::Value::String("write_files".into()))
    {
        for f in files {
            let serde_yaml::Value::Mapping(fm) = f else {
                continue;
            };
            let mut file = File::default();
            if let Some(serde_yaml::Value::String(p)) =
                fm.get(&serde_yaml::Value::String("path".into()))
            {
                file.path = p.clone();
            }
            if let Some(serde_yaml::Value::String(c)) =
                fm.get(&serde_yaml::Value::String("content".into()))
            {
                file.content = c.clone();
            }
            if let Some(serde_yaml::Value::String(e)) =
                fm.get(&serde_yaml::Value::String("encoding".into()))
            {
                file.encoding = e.clone();
            }
            if let Some(serde_yaml::Value::String(owner)) =
                fm.get(&serde_yaml::Value::String("owner".into()))
            {
                file.owner_string = owner.clone();
            }
            if let Some(perms_v) =
                fm.get(&serde_yaml::Value::String("permissions".into()))
            {
                let perms_str = match perms_v {
                    serde_yaml::Value::String(s) => s.clone(),
                    serde_yaml::Value::Number(n) => n.to_string(),
                    _ => String::new(),
                };
                if !perms_str.is_empty() {
                    file.permissions = parse_octal(&perms_str).map_err(|e| {
                        Error::other(format!(
                            "converting permission {} for {}: {}",
                            perms_str, file.path, e
                        ))
                    })?;
                }
            }
            stage.files.push(file);
        }
    }

    // runcmd -> stage.commands
    if let Some(serde_yaml::Value::Sequence(cmds)) =
        map.get(&serde_yaml::Value::String("runcmd".into()))
    {
        for c in cmds {
            if let Some(s) = c.as_str() {
                stage.commands.push(s.to_string());
            }
        }
    }
    // sudo entries derived from users get appended after runcmd commands.
    stage.commands.extend(sudo_commands);

    // final_message -> echo command
    if let Some(serde_yaml::Value::String(msg)) =
        map.get(&serde_yaml::Value::String("final_message".into()))
    {
        if !msg.is_empty() {
            stage.commands.push(format!("echo '{}'", msg));
        }
    }

    // packages list -> stage.packages.install
    if let Some(serde_yaml::Value::Sequence(pkgs)) =
        map.get(&serde_yaml::Value::String("packages".into()))
    {
        for p in pkgs {
            if let Some(s) = p.as_str() {
                stage.packages.install.push(s.to_string());
            }
        }
    }
    // package_update / package_upgrade
    if let Some(v) = map.get(&serde_yaml::Value::String("package_update".into())) {
        if v.as_bool() == Some(true) {
            stage.packages.refresh = true;
        }
    }
    if let Some(v) = map.get(&serde_yaml::Value::String("package_upgrade".into())) {
        if v.as_bool() == Some(true) {
            stage.packages.upgrade = true;
        }
    }

    // Top-level ssh_authorized_keys -> stage.ssh_keys["root"] when no user
    // supplied keys themselves (matches Go's "global keys go to root").
    if let Some(serde_yaml::Value::Sequence(keys)) =
        map.get(&serde_yaml::Value::String("ssh_authorized_keys".into()))
    {
        let collected: Vec<String> = keys
            .iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect();
        if !collected.is_empty() && !has_user_keys {
            stage.ssh_keys.insert("root".into(), collected);
        }
    }

    // bootcmd -> goes into a separate "boot.before" stage
    if let Some(serde_yaml::Value::Sequence(cmds)) =
        map.get(&serde_yaml::Value::String("bootcmd".into()))
    {
        for c in cmds {
            if let Some(s) = c.as_str() {
                before_commands.push(s.to_string());
            }
        }
    }

    let mut stages: HashMap<String, Vec<Stage>> = HashMap::new();
    stages.insert("boot".into(), vec![stage]);
    if !before_commands.is_empty() {
        stages.insert(
            "boot.before".into(),
            vec![Stage {
                commands: before_commands,
                ..Default::default()
            }],
        );
    }

    // Match Go yip's loader_cloudinit.go: optimistically also try to parse the
    // same document as a yip-native config and merge its stages in. This lets
    // a `#cloud-config` document still carry arbitrary `stages: { foo: [...] }`
    // entries that drive non-boot stages.
    if let Ok(expanded) = expand_merge_keys(bytes) {
        if let Ok(native) = serde_yaml::from_str::<Config>(&expanded) {
            for (k, v) in native.stages {
                stages.entry(k).or_default().extend(v);
            }
        }
    }

    Ok(Config {
        source: String::new(),
        name: String::new(),
        stages,
    })
}

/// Parse a cloud-init style permission string (`"0644"`, `"644"`) into a
/// numeric mode. Empty input parses to `0`. Matches Go yip's `parseOctal`.
fn parse_octal(s: &str) -> std::result::Result<u32, std::num::ParseIntError> {
    if s.is_empty() {
        return Ok(0);
    }
    u32::from_str_radix(s, 8)
}

#[cfg(test)]
mod tests {
    use super::*;
    use indoc::indoc;
    use pretty_assertions::assert_eq;

    #[test]
    fn parses_multi_stage_config() {
        let y = indoc! {r#"
            name: mycfg
            stages:
              rootfs.before:
                - name: pre
                  commands: [echo before]
              rootfs:
                - name: main
                  commands: [echo main]
              rootfs.after:
                - name: post
                  commands: [echo after]
        "#};
        let c = Config::load(y.as_bytes()).unwrap();
        assert_eq!(c.name, "mycfg");
        assert_eq!(c.stages.len(), 3);
        assert_eq!(c.stages["rootfs.before"][0].name, "pre");
        assert_eq!(c.stages["rootfs"][0].commands, vec!["echo main"]);
        assert_eq!(c.stages["rootfs.after"][0].name, "post");
    }

    #[test]
    fn empty_yaml_is_default() {
        let c = Config::load(b"{}").unwrap();
        assert_eq!(c, Config::default());
    }

    #[test]
    fn roundtrip() {
        let mut stages = HashMap::new();
        stages.insert(
            "boot".to_string(),
            vec![Stage {
                name: "n".into(),
                commands: vec!["echo".into()],
                ..Default::default()
            }],
        );
        let c = Config {
            source: String::new(),
            name: "cfg".into(),
            stages,
        };
        let s = c.to_string_yaml().unwrap();
        let back = Config::load(s.as_bytes()).unwrap();
        assert_eq!(back, c);
    }

    #[test]
    fn name_only_minimal_yaml() {
        // Only `name` set; stages map left empty.
        let cfg = Config::load(b"name: minimal\n").unwrap();
        assert_eq!(cfg.name, "minimal");
        assert!(cfg.stages.is_empty());
        // `source` is not in YAML — remains empty.
        assert!(cfg.source.is_empty());
    }

    #[test]
    fn source_field_is_skipped_in_yaml() {
        // `source` has #[serde(skip)] — must not appear in serialised YAML
        // and must not be settable from YAML.
        let cfg = Config {
            source: "/etc/yip/whatever.yaml".into(),
            name: "n".into(),
            stages: HashMap::new(),
        };
        let s = cfg.to_string_yaml().unwrap();
        assert!(!s.contains("source"));
        // Round-trip: deserialised source must be empty (not preserved).
        let back = Config::load(s.as_bytes()).unwrap();
        assert!(back.source.is_empty());
        assert_eq!(back.name, "n");
    }

    #[test]
    fn to_string_yaml_skips_empty_name_and_stages() {
        let cfg = Config::default();
        let s = cfg.to_string_yaml().unwrap();
        assert!(!s.contains("name:"));
        assert!(!s.contains("stages:"));
    }

    #[test]
    fn maximal_yaml_round_trip() {
        // Every field populated. Multiple stages, multiple entries per stage.
        let mut stages = HashMap::new();
        stages.insert(
            "rootfs".to_string(),
            vec![
                Stage {
                    name: "first".into(),
                    commands: vec!["echo a".into()],
                    ..Default::default()
                },
                Stage {
                    name: "second".into(),
                    commands: vec!["echo b".into(), "echo c".into()],
                    ..Default::default()
                },
            ],
        );
        stages.insert(
            "initramfs".to_string(),
            vec![Stage {
                name: "ini".into(),
                ..Default::default()
            }],
        );
        let c = Config {
            source: String::new(),
            name: "max".into(),
            stages,
        };
        let s = c.to_string_yaml().unwrap();
        let back = Config::load(s.as_bytes()).unwrap();
        assert_eq!(back, c);
    }

    #[test]
    fn load_rejects_invalid_yaml() {
        // A scalar where a mapping is expected — Config requires a mapping.
        let r = Config::load(b"- just a list\n- of strings\n");
        assert!(r.is_err());
    }

    // --- cloud-config header detection ---------------------------------------

    #[test]
    fn header_detector_recognises_canonical_form() {
        assert!(has_cloud_config_header(b"#cloud-config\nhostname: x\n"));
        assert!(has_cloud_config_header(b"#cloud-config"));
        assert!(has_cloud_config_header(b"#cloud-config   \nhostname: x\n"));
        assert!(has_cloud_config_header(b"#cloud-config\r\nhostname: x\n"));
    }

    #[test]
    fn header_detector_tolerates_variants() {
        // Space after `#` and case-insensitivity.
        assert!(has_cloud_config_header(b"# cloud-config\nhostname: x\n"));
        assert!(has_cloud_config_header(b"#Cloud-Config\nhostname: x\n"));
    }

    #[test]
    fn header_detector_rejects_non_cloud_config() {
        // Yip-native YAML: first non-comment line is `name:` — no header.
        assert!(!has_cloud_config_header(b"name: foo\nstages: {}\n"));
        // A different comment first — not cloud-config.
        assert!(!has_cloud_config_header(b"# some other comment\nname: x\n"));
        // Empty bytes.
        assert!(!has_cloud_config_header(b""));
    }

    // --- cloud-config full transform ----------------------------------------

    #[test]
    fn cloud_config_hostname_goes_into_boot_stage() {
        let y = indoc! {r#"
            #cloud-config
            hostname: my-host
        "#};
        let c = Config::load(y.as_bytes()).unwrap();
        // Go yip leaves both Config.Name and stage.Name empty for #cloud-config docs.
        assert!(c.name.is_empty());
        let boot = c.stages.get("boot").unwrap();
        assert_eq!(boot.len(), 1);
        assert_eq!(boot[0].hostname, "my-host");
        assert!(boot[0].name.is_empty());
    }

    #[test]
    fn cloud_config_write_files_with_string_perms() {
        let y = indoc! {r#"
            #cloud-config
            write_files:
              - path: /etc/foo
                content: |
                  hello
                permissions: '0644'
              - path: /etc/bar
                content: world
                permissions: '755'
                owner: alice
        "#};
        let c = Config::load(y.as_bytes()).unwrap();
        let stage = &c.stages["boot"][0];
        assert_eq!(stage.files.len(), 2);
        assert_eq!(stage.files[0].path, "/etc/foo");
        assert_eq!(stage.files[0].content, "hello\n");
        // "0644" octal = 0o644 = 420 decimal.
        assert_eq!(stage.files[0].permissions, 0o644);
        assert_eq!(stage.files[1].path, "/etc/bar");
        assert_eq!(stage.files[1].permissions, 0o755);
        // owner string preserved on the owner_string side-channel.
        assert_eq!(stage.files[1].owner_string, "alice");
    }

    #[test]
    fn cloud_config_runcmd_becomes_commands() {
        let y = indoc! {r#"
            #cloud-config
            runcmd:
              - mkdir /opt/bar
              - chown alice:alice /opt/bar
        "#};
        let c = Config::load(y.as_bytes()).unwrap();
        let stage = &c.stages["boot"][0];
        assert_eq!(
            stage.commands,
            vec!["mkdir /opt/bar", "chown alice:alice /opt/bar"]
        );
    }

    #[test]
    fn cloud_config_users_map_with_sudo_appended_as_command() {
        let y = indoc! {r#"
            #cloud-config
            users:
              - name: alice
                groups: [users, admin]
                sudo: ALL=(ALL) NOPASSWD:ALL
                shell: /bin/zsh
                ssh_authorized_keys:
                  - ssh-rsa AAAA
              - name: bob
                groups: [users]
        "#};
        let c = Config::load(y.as_bytes()).unwrap();
        let stage = &c.stages["boot"][0];
        assert_eq!(stage.users.len(), 2);
        let alice = stage.users.get("alice").unwrap();
        assert_eq!(alice.name, "alice");
        assert_eq!(alice.groups, vec!["users", "admin"]);
        assert_eq!(alice.shell, "/bin/zsh");
        assert_eq!(alice.ssh_authorized_keys, vec!["ssh-rsa AAAA"]);
        let bob = stage.users.get("bob").unwrap();
        assert_eq!(bob.groups, vec!["users"]);
        // Sudo line appended to stage.commands.
        assert!(
            stage
                .commands
                .iter()
                .any(|c| c == "echo 'ALL=(ALL) NOPASSWD:ALL' >> /etc/sudoers.d/alice"),
            "missing sudo command, got: {:?}",
            stage.commands
        );
    }

    #[test]
    fn cloud_config_packages_install_refresh_upgrade() {
        let y = indoc! {r#"
            #cloud-config
            packages:
              - vim
              - curl
            package_update: true
            package_upgrade: true
        "#};
        let c = Config::load(y.as_bytes()).unwrap();
        let stage = &c.stages["boot"][0];
        assert_eq!(stage.packages.install, vec!["vim", "curl"]);
        assert!(stage.packages.refresh);
        assert!(stage.packages.upgrade);
    }

    #[test]
    fn cloud_config_bootcmd_lands_in_boot_before_stage() {
        let y = indoc! {r#"
            #cloud-config
            bootcmd:
              - early1
              - early2
            runcmd:
              - late
        "#};
        let c = Config::load(y.as_bytes()).unwrap();
        // boot stage has runcmd
        assert_eq!(c.stages["boot"][0].commands, vec!["late"]);
        // boot.before stage has bootcmd
        let before = c.stages.get("boot.before").expect("boot.before stage");
        assert_eq!(before.len(), 1);
        assert_eq!(before[0].commands, vec!["early1", "early2"]);
        assert!(before[0].name.is_empty());
    }

    #[test]
    fn cloud_config_top_level_ssh_keys_assigned_to_root() {
        let y = indoc! {r#"
            #cloud-config
            ssh_authorized_keys:
              - ssh-rsa AAAA
              - ssh-ed25519 BBBB
        "#};
        let c = Config::load(y.as_bytes()).unwrap();
        let stage = &c.stages["boot"][0];
        let root_keys = stage.ssh_keys.get("root").expect("root ssh_keys");
        assert_eq!(root_keys, &vec!["ssh-rsa AAAA", "ssh-ed25519 BBBB"]);
    }

    #[test]
    fn cloud_config_top_level_ssh_keys_skipped_when_user_keys_present() {
        // When a user already provides ssh_authorized_keys, the global block
        // is not auto-routed to root. (Matches Go's behaviour where the
        // global block is appended into each named user's bucket instead;
        // we just guarantee root doesn't silently shadow it.)
        let y = indoc! {r#"
            #cloud-config
            users:
              - name: alice
                ssh_authorized_keys:
                  - ssh-rsa USER
            ssh_authorized_keys:
              - ssh-rsa GLOBAL
        "#};
        let c = Config::load(y.as_bytes()).unwrap();
        let stage = &c.stages["boot"][0];
        assert!(stage.ssh_keys.get("root").is_none());
        assert_eq!(
            stage.users.get("alice").unwrap().ssh_authorized_keys,
            vec!["ssh-rsa USER"]
        );
    }

    #[test]
    fn cloud_config_final_message_becomes_echo_command() {
        let y = indoc! {r#"
            #cloud-config
            final_message: boot complete
        "#};
        let c = Config::load(y.as_bytes()).unwrap();
        let stage = &c.stages["boot"][0];
        assert_eq!(stage.commands, vec!["echo 'boot complete'"]);
    }

    #[test]
    fn cloud_config_full_example_from_spec() {
        let y = indoc! {r#"
            #cloud-config
            users:
              - name: alice
                groups: [users, admin]
                sudo: ALL=(ALL) NOPASSWD:ALL
                ssh_authorized_keys:
                  - ssh-rsa AAAA...
            write_files:
              - path: /etc/foo
                content: |
                  hello
                permissions: '0644'
            runcmd:
              - mkdir /opt/bar
              - chown alice:alice /opt/bar
            hostname: my-host
        "#};
        let c = Config::load(y.as_bytes()).unwrap();
        assert!(c.name.is_empty());
        let stage = &c.stages["boot"][0];
        assert_eq!(stage.hostname, "my-host");
        assert_eq!(stage.files.len(), 1);
        assert_eq!(stage.files[0].permissions, 0o644);
        assert_eq!(stage.users.get("alice").unwrap().groups, vec!["users", "admin"]);
        // runcmd + appended sudo line.
        assert_eq!(
            stage.commands,
            vec![
                "mkdir /opt/bar".to_string(),
                "chown alice:alice /opt/bar".to_string(),
                "echo 'ALL=(ALL) NOPASSWD:ALL' >> /etc/sudoers.d/alice".to_string(),
            ]
        );
    }

    #[test]
    fn cloud_config_unknown_keys_ignored() {
        // Random unknown cloud-init keys must NOT cause a parse error.
        let y = indoc! {r#"
            #cloud-config
            hostname: x
            chpasswd:
              expire: false
            disable_root: true
            some_future_key:
              nested: {value: 42}
        "#};
        let c = Config::load(y.as_bytes()).unwrap();
        assert_eq!(c.stages["boot"][0].hostname, "x");
    }

    #[test]
    fn native_yip_yaml_still_parses_without_header() {
        // Sanity check: a bare yip-native YAML must not be routed through
        // the cloud-config transform.
        let y = indoc! {r#"
            name: native
            stages:
              boot:
                - name: hi
                  commands: [echo native]
        "#};
        let c = Config::load(y.as_bytes()).unwrap();
        assert_eq!(c.name, "native");
        assert_eq!(c.stages["boot"][0].name, "hi");
        assert_eq!(c.stages["boot"][0].commands, vec!["echo native"]);
    }

    // --- YAML merge-key (<<: *anchor) expansion ------------------------------

    #[test]
    fn merge_key_single_anchor_expanded_into_stages() {
        // The "defaults" anchor carries a `files` entry; both stages a and b
        // pull it in via `<<: *d` and add their own `commands`. After loading,
        // each stage entry must have both the merged `files` and its own
        // `commands`.
        let y = indoc! {r#"
            name: test
            defaults: &d
              files:
                - path: /etc/foo
                  content: hi
            stages:
              a:
                - <<: *d
                  commands: [echo a]
              b:
                - <<: *d
                  commands: [echo b]
        "#};
        let c = Config::load(y.as_bytes()).unwrap();
        assert_eq!(c.name, "test");

        let a = &c.stages["a"][0];
        assert_eq!(a.commands, vec!["echo a"]);
        assert_eq!(a.files.len(), 1);
        assert_eq!(a.files[0].path, "/etc/foo");
        assert_eq!(a.files[0].content, "hi");

        let b = &c.stages["b"][0];
        assert_eq!(b.commands, vec!["echo b"]);
        assert_eq!(b.files.len(), 1);
        assert_eq!(b.files[0].path, "/etc/foo");
    }

    #[test]
    fn merge_key_sequence_of_anchors_combines_keys() {
        // `<<: [*a, *b]` should pull keys from BOTH referenced mappings,
        // alongside the entry's own keys.
        let y = indoc! {r#"
            name: multi
            a: &a
              hostname: from-a
            b: &b
              node: host-b
            stages:
              boot:
                - <<: [*a, *b]
                  name: combined
        "#};
        let c = Config::load(y.as_bytes()).unwrap();
        let s = &c.stages["boot"][0];
        assert_eq!(s.name, "combined");
        assert_eq!(s.hostname, "from-a");
        assert_eq!(s.node, "host-b");
    }

    #[test]
    fn merge_key_existing_keys_win_over_merged() {
        // Per YAML 1.1 merge spec, keys already in the owning mapping take
        // precedence over keys pulled in via `<<`. Here the stage entry
        // defines its own `hostname`, which must override the anchor's.
        let y = indoc! {r#"
            name: prec
            d: &d
              hostname: from-anchor
            stages:
              boot:
                - <<: *d
                  hostname: from-entry
                  name: prec-stage
        "#};
        let c = Config::load(y.as_bytes()).unwrap();
        let s = &c.stages["boot"][0];
        assert_eq!(s.name, "prec-stage");
        assert_eq!(s.hostname, "from-entry");
    }

    #[test]
    fn merge_key_sequence_earlier_anchor_wins() {
        // When `<<` is a sequence, earlier elements take precedence over
        // later ones (still subject to the owning map winning over both).
        let y = indoc! {r#"
            name: seqprec
            a: &a
              hostname: from-a
            b: &b
              hostname: from-b
              node: host-b
            stages:
              boot:
                - <<: [*a, *b]
                  name: seqprec-stage
        "#};
        let c = Config::load(y.as_bytes()).unwrap();
        let s = &c.stages["boot"][0];
        assert_eq!(s.name, "seqprec-stage");
        // `*a` came first, so its hostname wins.
        assert_eq!(s.hostname, "from-a");
        // `node` only exists in `*b`, so it still propagates.
        assert_eq!(s.node, "host-b");
    }

    #[test]
    fn merge_key_nested_inside_stage_value_is_expanded() {
        // The `<<` is buried inside a nested mapping (a file entry), not at
        // the top of the stage. The walker must still find and expand it.
        let y = indoc! {r#"
            name: nested
            file_defaults: &fd
              permissions: 420
              ownerstring: root
            stages:
              boot:
                - name: n
                  files:
                    - <<: *fd
                      path: /etc/foo
                      content: hello
        "#};
        let c = Config::load(y.as_bytes()).unwrap();
        let f = &c.stages["boot"][0].files[0];
        assert_eq!(f.path, "/etc/foo");
        assert_eq!(f.content, "hello");
        assert_eq!(f.permissions, 420);
        assert_eq!(f.owner_string, "root");
    }

    #[test]
    fn merge_key_absent_yaml_still_parses() {
        // A regular, anchor-free yip config must parse identically after the
        // saphyr round-trip. Same content as `parses_multi_stage_config`.
        let y = indoc! {r#"
            name: plain
            stages:
              rootfs:
                - name: main
                  commands: [echo main]
              rootfs.after:
                - name: post
                  commands: [echo after]
        "#};
        let c = Config::load(y.as_bytes()).unwrap();
        assert_eq!(c.name, "plain");
        assert_eq!(c.stages.len(), 2);
        assert_eq!(c.stages["rootfs"][0].name, "main");
        assert_eq!(c.stages["rootfs"][0].commands, vec!["echo main"]);
        assert_eq!(c.stages["rootfs.after"][0].name, "post");
    }

    #[test]
    fn merge_key_anchor_without_merge_still_resolves_alias() {
        // Plain alias (no `<<`) — the alias should be inlined as-is. The
        // stage list aliased here becomes both `a` and `b`'s entries.
        let y = indoc! {r#"
            name: aliasonly
            shared: &s
              - name: shared-step
                commands: [echo shared]
            stages:
              a: *s
              b: *s
        "#};
        let c = Config::load(y.as_bytes()).unwrap();
        assert_eq!(c.stages["a"].len(), 1);
        assert_eq!(c.stages["a"][0].name, "shared-step");
        assert_eq!(c.stages["a"][0].commands, vec!["echo shared"]);
        assert_eq!(c.stages["b"][0].name, "shared-step");
    }
}
