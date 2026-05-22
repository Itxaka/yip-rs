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
}
