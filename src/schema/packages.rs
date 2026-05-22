//! Package manager directives.
//!
//! Note: in Go, `PackagePins` lives directly on `Stage` as a
//! `map[string]string` (yaml: `package_pins`). We expose it via a type alias
//! so `Stage` can use the same name.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// `map[string]string` under YAML key `package_pins` on Stage.
pub type PackagePins = HashMap<String, String>;

#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Packages {
    #[serde(default, rename = "install", skip_serializing_if = "Vec::is_empty")]
    pub install: Vec<String>,
    #[serde(default, rename = "remove", skip_serializing_if = "Vec::is_empty")]
    pub remove: Vec<String>,
    #[serde(default, rename = "refresh", skip_serializing_if = "is_false")]
    pub refresh: bool,
    #[serde(default, rename = "upgrade", skip_serializing_if = "is_false")]
    pub upgrade: bool,
}

fn is_false(b: &bool) -> bool {
    !*b
}

#[cfg(test)]
mod tests {
    use super::*;
    use indoc::indoc;
    use pretty_assertions::assert_eq;

    #[test]
    fn parses_full() {
        let y = indoc! {r#"
            install: [vim, curl]
            remove: [nano]
            refresh: true
            upgrade: false
        "#};
        let p: Packages = serde_yaml::from_str(y).unwrap();
        assert_eq!(p.install, vec!["vim", "curl"]);
        assert_eq!(p.remove, vec!["nano"]);
        assert!(p.refresh);
        assert!(!p.upgrade);
    }

    #[test]
    fn defaults() {
        let p: Packages = serde_yaml::from_str("{}").unwrap();
        assert_eq!(p, Packages::default());
    }

    #[test]
    fn roundtrip() {
        let p = Packages {
            install: vec!["a".into()],
            upgrade: true,
            ..Default::default()
        };
        let s = serde_yaml::to_string(&p).unwrap();
        let back: Packages = serde_yaml::from_str(&s).unwrap();
        assert_eq!(back, p);
    }

    #[test]
    fn minimal_yaml_install_only() {
        let y = indoc! {r#"
            install:
              - htop
        "#};
        let p: Packages = serde_yaml::from_str(y).unwrap();
        assert_eq!(p.install, vec!["htop"]);
        assert!(p.remove.is_empty());
        assert!(!p.refresh);
        assert!(!p.upgrade);
    }

    #[test]
    fn empty_default_yaml_skips_keys() {
        let s = serde_yaml::to_string(&Packages::default()).unwrap();
        assert!(!s.contains("install"));
        assert!(!s.contains("remove"));
        assert!(!s.contains("refresh"));
        assert!(!s.contains("upgrade"));
    }

    #[test]
    fn maximal_roundtrip_all_flags_set() {
        let p = Packages {
            install: vec!["vim".into(), "git".into(), "curl".into()],
            remove: vec!["nano".into(), "vi".into()],
            refresh: true,
            upgrade: true,
        };
        let s = serde_yaml::to_string(&p).unwrap();
        let back: Packages = serde_yaml::from_str(&s).unwrap();
        assert_eq!(back, p);
    }

    #[test]
    fn package_pins_type_alias_parses() {
        // PackagePins is a HashMap<String, String>; verify the alias
        // behaves identically to that map.
        let y = indoc! {r#"
            foo: "1.2.3"
            bar: "2.0"
        "#};
        let pins: PackagePins = serde_yaml::from_str(y).unwrap();
        assert_eq!(pins.get("foo").unwrap(), "1.2.3");
        assert_eq!(pins.get("bar").unwrap(), "2.0");
    }

    #[test]
    fn empty_install_vec_omits_key() {
        let p = Packages {
            install: Vec::new(),
            remove: vec!["x".into()],
            ..Default::default()
        };
        let s = serde_yaml::to_string(&p).unwrap();
        assert!(!s.contains("install"));
        assert!(s.contains("remove"));
    }
}
