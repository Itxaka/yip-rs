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
}
