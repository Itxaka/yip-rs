//! Systemd unit toggle configuration.

use serde::{Deserialize, Serialize};

#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SystemctlOverride {
    #[serde(default, rename = "service", skip_serializing_if = "String::is_empty")]
    pub service: String,
    #[serde(default, rename = "content", skip_serializing_if = "String::is_empty")]
    pub content: String,
    #[serde(default, rename = "name", skip_serializing_if = "String::is_empty")]
    pub name: String,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Systemctl {
    #[serde(default, rename = "enable", skip_serializing_if = "Vec::is_empty")]
    pub enable: Vec<String>,
    #[serde(default, rename = "disable", skip_serializing_if = "Vec::is_empty")]
    pub disable: Vec<String>,
    #[serde(default, rename = "start", skip_serializing_if = "Vec::is_empty")]
    pub start: Vec<String>,
    #[serde(default, rename = "mask", skip_serializing_if = "Vec::is_empty")]
    pub mask: Vec<String>,
    #[serde(default, rename = "overrides", skip_serializing_if = "Vec::is_empty")]
    pub overrides: Vec<SystemctlOverride>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use indoc::indoc;
    use pretty_assertions::assert_eq;

    #[test]
    fn parses() {
        let y = indoc! {r#"
            enable: [a.service, b.service]
            disable: [c.service]
            start: [d.service]
            mask: [e.service]
            overrides:
              - service: a.service
                name: override.conf
                content: |
                  [Service]
                  Restart=always
        "#};
        let s: Systemctl = serde_yaml::from_str(y).unwrap();
        assert_eq!(s.enable, vec!["a.service", "b.service"]);
        assert_eq!(s.disable, vec!["c.service"]);
        assert_eq!(s.start, vec!["d.service"]);
        assert_eq!(s.mask, vec!["e.service"]);
        assert_eq!(s.overrides.len(), 1);
        assert_eq!(s.overrides[0].service, "a.service");
        assert_eq!(s.overrides[0].name, "override.conf");
        assert!(s.overrides[0].content.contains("Restart=always"));
    }

    #[test]
    fn defaults() {
        let s: Systemctl = serde_yaml::from_str("{}").unwrap();
        assert_eq!(s, Systemctl::default());
    }

    #[test]
    fn roundtrip() {
        let s = Systemctl {
            enable: vec!["x.service".into()],
            ..Default::default()
        };
        let txt = serde_yaml::to_string(&s).unwrap();
        let back: Systemctl = serde_yaml::from_str(&txt).unwrap();
        assert_eq!(back, s);
    }

    #[test]
    fn minimal_yaml_enable_only() {
        let y = indoc! {r#"
            enable: [chronyd.service]
        "#};
        let s: Systemctl = serde_yaml::from_str(y).unwrap();
        assert_eq!(s.enable, vec!["chronyd.service"]);
        assert!(s.disable.is_empty());
        assert!(s.start.is_empty());
        assert!(s.mask.is_empty());
        assert!(s.overrides.is_empty());
    }

    #[test]
    fn default_serialises_empty() {
        let txt = serde_yaml::to_string(&Systemctl::default()).unwrap();
        assert!(!txt.contains("enable"));
        assert!(!txt.contains("disable"));
        assert!(!txt.contains("start"));
        assert!(!txt.contains("mask"));
        assert!(!txt.contains("overrides"));
    }

    #[test]
    fn maximal_roundtrip() {
        let s = Systemctl {
            enable: vec!["a.service".into(), "b.timer".into()],
            disable: vec!["c.service".into()],
            start: vec!["d.service".into()],
            mask: vec!["e.service".into(), "f.service".into()],
            overrides: vec![
                SystemctlOverride {
                    service: "a.service".into(),
                    name: "override.conf".into(),
                    content: "[Service]\nRestart=always\n".into(),
                },
                SystemctlOverride {
                    service: "b.timer".into(),
                    name: "tweak.conf".into(),
                    content: "[Timer]\nOnBootSec=10s\n".into(),
                },
            ],
        };
        let txt = serde_yaml::to_string(&s).unwrap();
        let back: Systemctl = serde_yaml::from_str(&txt).unwrap();
        assert_eq!(back, s);
    }

    #[test]
    fn systemctl_override_default() {
        let o: SystemctlOverride = serde_yaml::from_str("{}").unwrap();
        assert_eq!(o, SystemctlOverride::default());
    }

    #[test]
    fn yaml_keys_match_go_tags() {
        // enable / disable / start / mask / overrides + override's
        // service / content / name — all lowercase in Go.
        let y = indoc! {r#"
            overrides:
              - service: s.service
                content: "[Service]\nType=oneshot\n"
                name: my.conf
        "#};
        let s: Systemctl = serde_yaml::from_str(y).unwrap();
        assert_eq!(s.overrides.len(), 1);
        assert_eq!(s.overrides[0].service, "s.service");
        assert_eq!(s.overrides[0].name, "my.conf");
        assert!(s.overrides[0].content.contains("Type=oneshot"));
    }

    #[test]
    fn empty_overrides_vec_omits_key() {
        let s = Systemctl {
            enable: vec!["a".into()],
            overrides: Vec::new(),
            ..Default::default()
        };
        let txt = serde_yaml::to_string(&s).unwrap();
        assert!(!txt.contains("overrides"));
        assert!(txt.contains("enable"));
    }
}
