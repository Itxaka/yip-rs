//! [`Stage`] — the unit of work executed by yip's plugins.
//!
//! One YAML stage entry maps to one [`Stage`] struct. All fields are
//! optional; every missing YAML key parses to its default value. When the
//! executor walks a config it runs the registered plugin chain against
//! each stage in turn, with each plugin only acting on the field it cares
//! about (e.g. the `files` plugin only inspects [`Stage::files`]).
//!
//! ## YAML key mapping
//!
//! Field names in this struct are Rust-idiomatic snake_case. YAML keys
//! occasionally differ (older yip naming): see the per-field docs below.
//! The most common renames are:
//!
//! - [`Stage::ssh_keys`] ↔ YAML `authorized_keys`
//! - [`Stage::data_sources`] ↔ YAML `datasource`
//! - [`Stage::only_if_os`] ↔ YAML `only_os`
//! - [`Stage::only_if_arch`] ↔ YAML `only_arch`
//! - [`Stage::only_if_os_version`] ↔ YAML `only_os_version`
//! - [`Stage::only_if_service_manager`] ↔ YAML `only_service_manager`
//!
//! # Examples
//!
//! ```
//! use yip::schema::Stage;
//!
//! let y = "name: hello\ncommands: [echo hi]\n";
//! let s: Stage = serde_yaml::from_str(y).unwrap();
//! assert_eq!(s.name, "hello");
//! assert_eq!(s.commands, vec!["echo hi".to_string()]);
//! ```
//!
//! # Stability
//!
//! Public API. Field set matches Go yip; adding a new YAML key requires
//! adding a new field here.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::schema::datasource::DataSource;
use crate::schema::dns::DNS;
use crate::schema::file::{Directory, Download, File};
use crate::schema::git::Git;
use crate::schema::if_files::IfFiles;
use crate::schema::layout::Layout;
use crate::schema::packages::{PackagePins, Packages};
use crate::schema::systemctl::Systemctl;
use crate::schema::unpack::UnpackImageConf;
use crate::schema::user::{User, YipEntity};

/// A pointer to another stage by name, used in `after:` lists.
///
/// Modelled as a struct (rather than a bare string) to match Go's YAML
/// layout — `after: [{name: foo}]` rather than `after: [foo]`.
///
/// # Examples
///
/// ```
/// use yip::schema::Dependency;
///
/// let d = Dependency { name: "other".into() };
/// assert_eq!(d.name, "other");
/// ```
#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Dependency {
    /// Name of the stage this entry depends on. YAML key: `name`.
    #[serde(default, rename = "name", skip_serializing_if = "String::is_empty")]
    pub name: String,
}

/// A single stage entry — the smallest unit of yip work.
///
/// Every field is optional; an empty `Stage {}` is valid and is a
/// silent no-op when the executor runs it.
///
/// # Examples
///
/// ```
/// use yip::schema::Stage;
///
/// let s = Stage { name: "demo".into(), ..Default::default() };
/// assert_eq!(s.name, "demo");
/// assert!(s.commands.is_empty());
/// ```
#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Stage {
    // --- core file/command actions ---
    /// Shell commands run by the `commands` plugin, in order. YAML key:
    /// `commands`.
    #[serde(default, rename = "commands", skip_serializing_if = "Vec::is_empty")]
    pub commands: Vec<String>,
    /// Files to write (path, content, permissions, …). YAML key: `files`.
    #[serde(default, rename = "files", skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<File>,
    /// Files to fetch over HTTP(S). YAML key: `downloads`.
    #[serde(default, rename = "downloads", skip_serializing_if = "Vec::is_empty")]
    pub downloads: Vec<Download>,
    /// Directories to ensure exist (with mode/owner). YAML key:
    /// `directories`.
    #[serde(default, rename = "directories", skip_serializing_if = "Vec::is_empty")]
    pub directories: Vec<Directory>,
    /// Inline shell expression evaluated by the `if` conditional —
    /// non-empty + exit 0 ⇒ run the stage. YAML key: `if`.
    #[serde(default, rename = "if", skip_serializing_if = "String::is_empty")]
    pub r#if: String,

    // --- entity / passwd-style ---
    /// Linux user/group/sudoers entries to create. YAML key:
    /// `ensure_entities`.
    #[serde(
        default,
        rename = "ensure_entities",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub ensure_entities: Vec<YipEntity>,
    /// Linux user/group/sudoers entries to delete. YAML key:
    /// `delete_entities`.
    #[serde(
        default,
        rename = "delete_entities",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub delete_entities: Vec<YipEntity>,

    // --- networking / identity ---
    /// DNS resolver configuration. YAML key: `dns`.
    #[serde(default, rename = "dns", skip_serializing_if = "dns_is_default")]
    pub dns: DNS,
    /// Hostname to set. YAML key: `hostname`.
    #[serde(default, rename = "hostname", skip_serializing_if = "String::is_empty")]
    pub hostname: String,
    /// Stage name (used for logging + DAG dependency wiring). YAML key:
    /// `name`.
    #[serde(default, rename = "name", skip_serializing_if = "String::is_empty")]
    pub name: String,
    /// `sysctl` key/value pairs. YAML key: `sysctl`.
    #[serde(
        default,
        rename = "sysctl",
        deserialize_with = "deserialize_string_map_lenient",
        skip_serializing_if = "HashMap::is_empty"
    )]
    pub sysctl: HashMap<String, String>,

    /// Authorised SSH keys per user. Maps `user → [keys]`. `SSHKeys` in
    /// Go. YAML key: `authorized_keys`.
    #[serde(
        default,
        rename = "authorized_keys",
        skip_serializing_if = "HashMap::is_empty"
    )]
    pub ssh_keys: HashMap<String, Vec<String>>,

    /// `node` conditional value — only run if the local hostname
    /// matches. YAML key: `node`.
    #[serde(default, rename = "node", skip_serializing_if = "String::is_empty")]
    pub node: String,
    /// User accounts to create/update. Maps `username → User`. YAML
    /// key: `users`.
    #[serde(default, rename = "users", skip_serializing_if = "HashMap::is_empty")]
    pub users: HashMap<String, User>,
    /// Kernel modules to load. YAML key: `modules`.
    #[serde(default, rename = "modules", skip_serializing_if = "Vec::is_empty")]
    pub modules: Vec<String>,

    /// systemd unit enable/disable/mask actions. YAML key: `systemctl`.
    #[serde(default, rename = "systemctl", skip_serializing_if = "systemctl_is_default")]
    pub systemctl: Systemctl,

    /// Global environment variables to write to `/etc/environment`. YAML
    /// key: `environment`.
    #[serde(
        default,
        rename = "environment",
        deserialize_with = "deserialize_string_map_lenient",
        skip_serializing_if = "HashMap::is_empty"
    )]
    pub environment: HashMap<String, String>,
    /// Alternative file to write `environment` into. YAML key:
    /// `environment_file`.
    #[serde(
        default,
        rename = "environment_file",
        skip_serializing_if = "String::is_empty"
    )]
    pub environment_file: String,

    /// Pinned package versions for the package manager. YAML key:
    /// `package_pins`.
    #[serde(
        default,
        rename = "package_pins",
        deserialize_with = "deserialize_string_map_lenient",
        skip_serializing_if = "HashMap::is_empty"
    )]
    pub package_pins: PackagePins,

    /// Packages to install / remove / refresh / upgrade. YAML key:
    /// `packages`.
    #[serde(default, rename = "packages", skip_serializing_if = "packages_is_default")]
    pub packages: Packages,

    /// OCI / tar images to unpack onto disk. YAML key: `unpack_images`.
    #[serde(
        default,
        rename = "unpack_images",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub unpack_images: Vec<UnpackImageConf>,

    // --- dependency wiring ---
    /// DAG dependencies — this stage runs after the named stages. YAML
    /// key: `after`.
    #[serde(default, rename = "after", skip_serializing_if = "Vec::is_empty")]
    pub after: Vec<Dependency>,

    /// Cloud datasource probe config. `DataSources` in Go. YAML key:
    /// `datasource`.
    #[serde(default, rename = "datasource", skip_serializing_if = "datasource_is_default")]
    pub data_sources: DataSource,

    /// Disk / partition / filesystem layout. YAML key: `layout`.
    #[serde(default, rename = "layout", skip_serializing_if = "layout_is_default")]
    pub layout: Layout,

    /// `systemd-firstboot` parameters (timezone, locale, …). YAML key:
    /// `systemd_firstboot`.
    #[serde(
        default,
        rename = "systemd_firstboot",
        deserialize_with = "deserialize_string_map_lenient",
        skip_serializing_if = "HashMap::is_empty"
    )]
    pub systemd_firstboot: HashMap<String, String>,

    /// `systemd-timesyncd` config (NTP server list, …). YAML key:
    /// `timesyncd`.
    #[serde(
        default,
        rename = "timesyncd",
        deserialize_with = "deserialize_string_map_lenient",
        skip_serializing_if = "HashMap::is_empty"
    )]
    pub timesyncd: HashMap<String, String>,

    /// Git checkout to perform. YAML key: `git`.
    #[serde(default, rename = "git", skip_serializing_if = "git_is_default")]
    pub git: Git,

    // --- conditionals ---
    /// Gate the stage on `os-release` ID. YAML key: `only_os`.
    #[serde(default, rename = "only_os", skip_serializing_if = "String::is_empty")]
    pub only_if_os: String,
    /// Gate the stage on `os-release` VERSION_ID. YAML key:
    /// `only_os_version`.
    #[serde(
        default,
        rename = "only_os_version",
        skip_serializing_if = "String::is_empty"
    )]
    pub only_if_os_version: String,
    /// Gate the stage on machine architecture (`amd64` / `arm64` / …).
    /// YAML key: `only_arch`.
    #[serde(default, rename = "only_arch", skip_serializing_if = "String::is_empty")]
    pub only_if_arch: String,
    /// Gate the stage on init system (`systemd` / `openrc` / …). YAML
    /// key: `only_service_manager`.
    #[serde(
        default,
        rename = "only_service_manager",
        skip_serializing_if = "String::is_empty"
    )]
    pub only_if_service_manager: String,
    /// Gate the stage on file existence (any / all / none semantics
    /// depending on the inner map). YAML key: `if_files`.
    #[serde(default, rename = "if_files", skip_serializing_if = "HashMap::is_empty")]
    pub if_files: IfFiles,
}

// --- "default" predicate helpers used by skip_serializing_if -------------

fn dns_is_default(d: &DNS) -> bool {
    d == &DNS::default()
}
fn systemctl_is_default(s: &Systemctl) -> bool {
    s == &Systemctl::default()
}
fn packages_is_default(p: &Packages) -> bool {
    p == &Packages::default()
}
fn datasource_is_default(d: &DataSource) -> bool {
    d == &DataSource::default()
}
fn layout_is_default(l: &Layout) -> bool {
    l == &Layout::default()
}
fn git_is_default(g: &Git) -> bool {
    g == &Git::default()
}

/// Lenient deserializer for `HashMap<String, String>` fields.
///
/// Accepts any scalar YAML value (string, int, float, bool, null) and
/// stringifies it. This matches real-world configs that use bare
/// integers in `sysctl`, `environment`, etc. where strict serde
/// behaviour would otherwise reject the value.
fn deserialize_string_map_lenient<'de, D>(
    d: D,
) -> std::result::Result<HashMap<String, String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::{self, MapAccess, Visitor};
    use std::fmt;
    struct V;
    impl<'de> Visitor<'de> for V {
        type Value = HashMap<String, String>;
        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("a YAML map where values may be strings, ints, floats, or bools")
        }
        fn visit_map<A: MapAccess<'de>>(
            self,
            mut m: A,
        ) -> Result<HashMap<String, String>, A::Error> {
            let mut out = HashMap::new();
            while let Some((k, v)) = m.next_entry::<String, serde_yaml::Value>()? {
                let s = match v {
                    serde_yaml::Value::String(s) => s,
                    serde_yaml::Value::Number(n) => n.to_string(),
                    serde_yaml::Value::Bool(b) => b.to_string(),
                    serde_yaml::Value::Null => String::new(),
                    other => serde_yaml::to_string(&other)
                        .map_err(de::Error::custom)?
                        .trim_end()
                        .to_string(),
                };
                out.insert(k, s);
            }
            Ok(out)
        }
    }
    d.deserialize_map(V)
}

#[cfg(test)]
mod tests {
    use super::*;
    use indoc::indoc;

    #[test]
    fn empty_stage_uses_defaults() {
        let s: Stage = serde_yaml::from_str("{}").unwrap();
        assert_eq!(s, Stage::default());
    }

    #[test]
    fn parses_basic_stage() {
        let y = indoc! {r#"
            name: bootstrap
            commands: [echo hi]
            files:
              - path: /tmp/foo
                content: bar
                permissions: 420
            environment:
              FOO: bar
            authorized_keys:
              root: [keyA, keyB]
            only_os: linux
            only_arch: amd64
            if_files:
              any: [/etc/foo]
        "#};
        let s: Stage = serde_yaml::from_str(y).unwrap();
        assert_eq!(s.name, "bootstrap");
        assert_eq!(s.commands, vec!["echo hi"]);
        assert_eq!(s.files.len(), 1);
        assert_eq!(s.files[0].path, "/tmp/foo");
        assert_eq!(s.environment.get("FOO").unwrap(), "bar");
        assert_eq!(
            s.ssh_keys.get("root").unwrap(),
            &vec!["keyA".to_string(), "keyB".to_string()]
        );
        assert_eq!(s.only_if_os, "linux");
        assert_eq!(s.only_if_arch, "amd64");
        assert!(!s.if_files.is_empty());
    }

    #[test]
    fn parses_renamed_fields() {
        let y = indoc! {r#"
            only_os: linux
            only_os_version: "22.04"
            only_arch: arm64
            only_service_manager: systemd
            systemd_firstboot:
              keymap: us
            package_pins:
              foo: "1.2.3"
            unpack_images:
              - source: x
                target: y
            after:
              - name: other
        "#};
        let s: Stage = serde_yaml::from_str(y).unwrap();
        assert_eq!(s.only_if_os, "linux");
        assert_eq!(s.only_if_os_version, "22.04");
        assert_eq!(s.only_if_arch, "arm64");
        assert_eq!(s.only_if_service_manager, "systemd");
        assert_eq!(s.systemd_firstboot.get("keymap").unwrap(), "us");
        assert_eq!(s.package_pins.get("foo").unwrap(), "1.2.3");
        assert_eq!(s.unpack_images.len(), 1);
        assert_eq!(s.unpack_images[0].source, "x");
        assert_eq!(s.after, vec![Dependency { name: "other".into() }]);
    }

    #[test]
    fn datasource_renamed() {
        let y = indoc! {r#"
            datasource:
              providers: [aws]
              path: /var/lib/cloud
        "#};
        let s: Stage = serde_yaml::from_str(y).unwrap();
        assert_eq!(s.data_sources.providers, vec!["aws"]);
        assert_eq!(s.data_sources.path, "/var/lib/cloud");
    }

    #[test]
    fn roundtrip_stage() {
        let s = Stage {
            name: "x".into(),
            commands: vec!["echo".into()],
            only_if_os: "linux".into(),
            ..Default::default()
        };
        let txt = serde_yaml::to_string(&s).unwrap();
        let back: Stage = serde_yaml::from_str(&txt).unwrap();
        assert_eq!(back, s);
    }

    #[test]
    fn sysctl_accepts_bare_integer() {
        let y = indoc! {r#"
            sysctl:
              foo: 100
        "#};
        let s: Stage = serde_yaml::from_str(y).unwrap();
        assert_eq!(s.sysctl.get("foo").unwrap(), "100");
    }

    #[test]
    fn sysctl_accepts_bare_float() {
        let y = indoc! {r#"
            sysctl:
              foo: 1.5
        "#};
        let s: Stage = serde_yaml::from_str(y).unwrap();
        assert_eq!(s.sysctl.get("foo").unwrap(), "1.5");
    }

    #[test]
    fn sysctl_accepts_bool() {
        let y = indoc! {r#"
            sysctl:
              foo: true
        "#};
        let s: Stage = serde_yaml::from_str(y).unwrap();
        assert_eq!(s.sysctl.get("foo").unwrap(), "true");
    }

    #[test]
    fn sysctl_accepts_quoted_string() {
        let y = indoc! {r#"
            sysctl:
              foo: "100"
        "#};
        let s: Stage = serde_yaml::from_str(y).unwrap();
        assert_eq!(s.sysctl.get("foo").unwrap(), "100");
    }

    #[test]
    fn environment_accepts_bare_string() {
        let y = indoc! {r#"
            environment:
              PATH: /bin
        "#};
        let s: Stage = serde_yaml::from_str(y).unwrap();
        assert_eq!(s.environment.get("PATH").unwrap(), "/bin");
    }

    #[test]
    fn systemd_firstboot_accepts_bare_string() {
        let y = indoc! {r#"
            systemd_firstboot:
              hostname: kairos
        "#};
        let s: Stage = serde_yaml::from_str(y).unwrap();
        assert_eq!(s.systemd_firstboot.get("hostname").unwrap(), "kairos");
    }

    #[test]
    fn sysctl_mixed_types_in_same_map() {
        let y = indoc! {r#"
            sysctl:
              a: 100
              b: "200"
              c: true
        "#};
        let s: Stage = serde_yaml::from_str(y).unwrap();
        assert_eq!(s.sysctl.get("a").unwrap(), "100");
        assert_eq!(s.sysctl.get("b").unwrap(), "200");
        assert_eq!(s.sysctl.get("c").unwrap(), "true");
    }

    #[test]
    fn sysctl_real_world_kernel_keys() {
        let y = indoc! {r#"
            sysctl:
              net.core.rmem_max: 7500000
              net.ipv4.ip_forward: 1
        "#};
        let s: Stage = serde_yaml::from_str(y).unwrap();
        assert_eq!(s.sysctl.get("net.core.rmem_max").unwrap(), "7500000");
        assert_eq!(s.sysctl.get("net.ipv4.ip_forward").unwrap(), "1");
    }

    #[test]
    fn environment_accepts_integer() {
        let y = indoc! {r#"
            environment:
              MAX_THREADS: 8
        "#};
        let s: Stage = serde_yaml::from_str(y).unwrap();
        assert_eq!(s.environment.get("MAX_THREADS").unwrap(), "8");
    }

    #[test]
    fn package_pins_accepts_integer_version() {
        let y = indoc! {r#"
            package_pins:
              foo: 1
              bar: "1.2.3"
        "#};
        let s: Stage = serde_yaml::from_str(y).unwrap();
        assert_eq!(s.package_pins.get("foo").unwrap(), "1");
        assert_eq!(s.package_pins.get("bar").unwrap(), "1.2.3");
    }

    #[test]
    fn timesyncd_accepts_integer() {
        let y = indoc! {r#"
            timesyncd:
              PollIntervalMaxSec: 2048
              NTP: pool.ntp.org
        "#};
        let s: Stage = serde_yaml::from_str(y).unwrap();
        assert_eq!(s.timesyncd.get("PollIntervalMaxSec").unwrap(), "2048");
        assert_eq!(s.timesyncd.get("NTP").unwrap(), "pool.ntp.org");
    }
}
