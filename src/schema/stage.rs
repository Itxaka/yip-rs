//! `Stage` is the unit of work executed by yip's plugins. One stage in YAML
//! maps to one `Stage` struct. All 24 fields are optional — every missing
//! YAML key parses to its default value.

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

#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Dependency {
    #[serde(default, rename = "name", skip_serializing_if = "String::is_empty")]
    pub name: String,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Stage {
    // --- core file/command actions ---
    #[serde(default, rename = "commands", skip_serializing_if = "Vec::is_empty")]
    pub commands: Vec<String>,
    #[serde(default, rename = "files", skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<File>,
    #[serde(default, rename = "downloads", skip_serializing_if = "Vec::is_empty")]
    pub downloads: Vec<Download>,
    #[serde(default, rename = "directories", skip_serializing_if = "Vec::is_empty")]
    pub directories: Vec<Directory>,
    #[serde(default, rename = "if", skip_serializing_if = "String::is_empty")]
    pub r#if: String,

    // --- entity / passwd-style ---
    #[serde(
        default,
        rename = "ensure_entities",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub ensure_entities: Vec<YipEntity>,
    #[serde(
        default,
        rename = "delete_entities",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub delete_entities: Vec<YipEntity>,

    // --- networking / identity ---
    #[serde(default, rename = "dns", skip_serializing_if = "dns_is_default")]
    pub dns: DNS,
    #[serde(default, rename = "hostname", skip_serializing_if = "String::is_empty")]
    pub hostname: String,
    #[serde(default, rename = "name", skip_serializing_if = "String::is_empty")]
    pub name: String,
    #[serde(default, rename = "sysctl", skip_serializing_if = "HashMap::is_empty")]
    pub sysctl: HashMap<String, String>,

    /// `SSHKeys` in Go — YAML key `authorized_keys`.
    #[serde(
        default,
        rename = "authorized_keys",
        skip_serializing_if = "HashMap::is_empty"
    )]
    pub ssh_keys: HashMap<String, Vec<String>>,

    #[serde(default, rename = "node", skip_serializing_if = "String::is_empty")]
    pub node: String,
    #[serde(default, rename = "users", skip_serializing_if = "HashMap::is_empty")]
    pub users: HashMap<String, User>,
    #[serde(default, rename = "modules", skip_serializing_if = "Vec::is_empty")]
    pub modules: Vec<String>,

    #[serde(default, rename = "systemctl", skip_serializing_if = "systemctl_is_default")]
    pub systemctl: Systemctl,

    #[serde(
        default,
        rename = "environment",
        skip_serializing_if = "HashMap::is_empty"
    )]
    pub environment: HashMap<String, String>,
    #[serde(
        default,
        rename = "environment_file",
        skip_serializing_if = "String::is_empty"
    )]
    pub environment_file: String,

    #[serde(
        default,
        rename = "package_pins",
        skip_serializing_if = "HashMap::is_empty"
    )]
    pub package_pins: PackagePins,

    #[serde(default, rename = "packages", skip_serializing_if = "packages_is_default")]
    pub packages: Packages,

    #[serde(
        default,
        rename = "unpack_images",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub unpack_images: Vec<UnpackImageConf>,

    // --- dependency wiring ---
    #[serde(default, rename = "after", skip_serializing_if = "Vec::is_empty")]
    pub after: Vec<Dependency>,

    /// `DataSources` in Go — YAML key `datasource`.
    #[serde(default, rename = "datasource", skip_serializing_if = "datasource_is_default")]
    pub data_sources: DataSource,

    #[serde(default, rename = "layout", skip_serializing_if = "layout_is_default")]
    pub layout: Layout,

    #[serde(
        default,
        rename = "systemd_firstboot",
        skip_serializing_if = "HashMap::is_empty"
    )]
    pub systemd_firstboot: HashMap<String, String>,

    #[serde(
        default,
        rename = "timesyncd",
        skip_serializing_if = "HashMap::is_empty"
    )]
    pub timesyncd: HashMap<String, String>,

    #[serde(default, rename = "git", skip_serializing_if = "git_is_default")]
    pub git: Git,

    // --- conditionals ---
    #[serde(default, rename = "only_os", skip_serializing_if = "String::is_empty")]
    pub only_if_os: String,
    #[serde(
        default,
        rename = "only_os_version",
        skip_serializing_if = "String::is_empty"
    )]
    pub only_if_os_version: String,
    #[serde(default, rename = "only_arch", skip_serializing_if = "String::is_empty")]
    pub only_if_arch: String,
    #[serde(
        default,
        rename = "only_service_manager",
        skip_serializing_if = "String::is_empty"
    )]
    pub only_if_service_manager: String,
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
}
