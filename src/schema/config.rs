//! Top-level config types — `Config` is the Rust port of Go's `YipConfig`.
//!
//! Go has `Source` as a non-yaml metadata field (set after parsing by the
//! loader). We expose it the same way: present on the struct but
//! `#[serde(skip)]` so it does not round-trip through YAML.

use std::collections::HashMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::schema::stage::Stage;

#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Config {
    /// Path or URL this config was loaded from. Not stored in YAML.
    #[serde(skip)]
    pub source: String,

    #[serde(default, rename = "name", skip_serializing_if = "String::is_empty")]
    pub name: String,

    #[serde(default, rename = "stages", skip_serializing_if = "HashMap::is_empty")]
    pub stages: HashMap<String, Vec<Stage>>,
}

/// Go-flavoured alias so a porting reader can still type `YipConfig`.
pub type YipConfig = Config;

impl Config {
    /// Parse a YAML byte slice into a Config. Mirrors `schema.Load` minus
    /// the cloud-init detection branch — this is the yip-native YAML path.
    pub fn load(bytes: &[u8]) -> Result<Self> {
        let cfg: Config = serde_yaml::from_slice(bytes)?;
        Ok(cfg)
    }

    /// Convenience: load from a file path, attaching the path to `source`.
    pub fn load_file(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let bytes = std::fs::read(path).map_err(|e| Error::io_at(path, e))?;
        let mut cfg = Self::load(&bytes)?;
        cfg.source = path.display().to_string();
        Ok(cfg)
    }

    /// Serialize to YAML. Mirrors Go's `YipConfig.ToString`.
    pub fn to_string_yaml(&self) -> Result<String> {
        Ok(serde_yaml::to_string(self)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use indoc::indoc;

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
}
