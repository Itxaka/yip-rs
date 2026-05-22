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

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::schema::stage::Stage;

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
        let cfg: Config = serde_yaml::from_slice(bytes)?;
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
}
