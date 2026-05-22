//! `if_files` stage field — map of check kind → list of paths.
//!
//! Go: `type IfCheckType string` with constants `"any"`, `"all"`, `"none"`,
//! used as keys in `map[IfCheckType][]string`. We model the key as an enum so
//! callers can pattern-match and serde validates the spelling.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "lowercase")]
pub enum IfCheckType {
    Any,
    All,
    None,
}

/// Equivalent of Go's `map[IfCheckType][]string`.
pub type IfFiles = HashMap<IfCheckType, Vec<String>>;

/// Sometimes more convenient struct form.
#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct IfFile {
    #[serde(default, rename = "any", skip_serializing_if = "Vec::is_empty")]
    pub any: Vec<String>,
    #[serde(default, rename = "all", skip_serializing_if = "Vec::is_empty")]
    pub all: Vec<String>,
    #[serde(default, rename = "none", skip_serializing_if = "Vec::is_empty")]
    pub none: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use indoc::indoc;
    use pretty_assertions::assert_eq;

    #[test]
    fn map_form_parses() {
        let y = indoc! {r#"
            any:
              - /etc/foo
            all:
              - /etc/bar
              - /etc/baz
            none:
              - /etc/qux
        "#};
        let m: IfFiles = serde_yaml::from_str(y).unwrap();
        assert_eq!(m.get(&IfCheckType::Any).unwrap(), &vec!["/etc/foo"]);
        assert_eq!(
            m.get(&IfCheckType::All).unwrap(),
            &vec!["/etc/bar", "/etc/baz"]
        );
        assert_eq!(m.get(&IfCheckType::None).unwrap(), &vec!["/etc/qux"]);
    }

    #[test]
    fn struct_form_parses() {
        let y = indoc! {r#"
            any: [/etc/foo]
            all: [/etc/bar]
        "#};
        let i: IfFile = serde_yaml::from_str(y).unwrap();
        assert_eq!(i.any, vec!["/etc/foo"]);
        assert_eq!(i.all, vec!["/etc/bar"]);
        assert!(i.none.is_empty());
    }

    #[test]
    fn check_type_serialization() {
        assert_eq!(
            serde_yaml::to_string(&IfCheckType::Any).unwrap().trim(),
            "any"
        );
        assert_eq!(
            serde_yaml::to_string(&IfCheckType::All).unwrap().trim(),
            "all"
        );
        assert_eq!(
            serde_yaml::to_string(&IfCheckType::None).unwrap().trim(),
            "none"
        );
    }

    #[test]
    fn map_form_minimal_any_only() {
        let y = indoc! {r#"
            any: [/etc/foo]
        "#};
        let m: IfFiles = serde_yaml::from_str(y).unwrap();
        assert_eq!(m.len(), 1);
        assert_eq!(m.get(&IfCheckType::Any).unwrap(), &vec!["/etc/foo"]);
        assert!(m.get(&IfCheckType::All).is_none());
        assert!(m.get(&IfCheckType::None).is_none());
    }

    #[test]
    fn check_type_deserialises_lowercase_only() {
        // Capitalised variants must fail (rename_all = "lowercase").
        let r: Result<IfCheckType, _> = serde_yaml::from_str("Any");
        assert!(r.is_err());
        let r: Result<IfCheckType, _> = serde_yaml::from_str("ALL");
        assert!(r.is_err());
    }

    #[test]
    fn check_type_deserialises_each_lowercase_variant() {
        let a: IfCheckType = serde_yaml::from_str("any").unwrap();
        assert_eq!(a, IfCheckType::Any);
        let b: IfCheckType = serde_yaml::from_str("all").unwrap();
        assert_eq!(b, IfCheckType::All);
        let c: IfCheckType = serde_yaml::from_str("none").unwrap();
        assert_eq!(c, IfCheckType::None);
    }

    #[test]
    fn struct_form_default_for_empty_yaml() {
        let i: IfFile = serde_yaml::from_str("{}").unwrap();
        assert_eq!(i, IfFile::default());
    }

    #[test]
    fn struct_form_roundtrip_maximal() {
        let i = IfFile {
            any: vec!["/a".into(), "/b".into()],
            all: vec!["/c".into()],
            none: vec!["/d".into(), "/e".into(), "/f".into()],
        };
        let s = serde_yaml::to_string(&i).unwrap();
        let back: IfFile = serde_yaml::from_str(&s).unwrap();
        assert_eq!(back, i);
    }

    #[test]
    fn struct_form_default_serialisation_omits_keys() {
        let s = serde_yaml::to_string(&IfFile::default()).unwrap();
        assert!(!s.contains("any"));
        assert!(!s.contains("all"));
        assert!(!s.contains("none"));
    }

    #[test]
    fn map_form_roundtrip_all_keys() {
        let mut m: IfFiles = HashMap::new();
        m.insert(IfCheckType::Any, vec!["/p1".into()]);
        m.insert(IfCheckType::All, vec!["/p2".into(), "/p3".into()]);
        m.insert(IfCheckType::None, vec!["/p4".into()]);
        let s = serde_yaml::to_string(&m).unwrap();
        let back: IfFiles = serde_yaml::from_str(&s).unwrap();
        assert_eq!(back, m);
    }
}
