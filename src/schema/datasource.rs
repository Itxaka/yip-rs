//! Cloud-init-style datasource configuration.

use serde::{Deserialize, Serialize};

/// Identifies the known set of cloud-init datasource providers. Stored as a
/// plain string (yip itself does no validation here) — alias kept for clarity.
pub type DataSourceProvider = String;

#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DataSource {
    #[serde(default, rename = "providers", skip_serializing_if = "Vec::is_empty")]
    pub providers: Vec<DataSourceProvider>,
    #[serde(default, rename = "path", skip_serializing_if = "String::is_empty")]
    pub path: String,
    #[serde(default, rename = "userdata_name", skip_serializing_if = "String::is_empty")]
    pub userdata_name: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use indoc::indoc;
    use pretty_assertions::assert_eq;

    #[test]
    fn parses() {
        let y = indoc! {r#"
            providers:
              - ec2
              - gce
            path: /var/lib/cloud
            userdata_name: user-data
        "#};
        let ds: DataSource = serde_yaml::from_str(y).unwrap();
        assert_eq!(ds.providers, vec!["ec2", "gce"]);
        assert_eq!(ds.path, "/var/lib/cloud");
        assert_eq!(ds.userdata_name, "user-data");
    }

    #[test]
    fn defaults_for_empty_yaml() {
        let ds: DataSource = serde_yaml::from_str("{}").unwrap();
        assert_eq!(ds, DataSource::default());
    }

    #[test]
    fn roundtrip() {
        let ds = DataSource {
            providers: vec!["aws".into()],
            path: "/p".into(),
            userdata_name: "u".into(),
        };
        let s = serde_yaml::to_string(&ds).unwrap();
        let back: DataSource = serde_yaml::from_str(&s).unwrap();
        assert_eq!(back, ds);
    }

    #[test]
    fn minimal_yaml_providers_only() {
        let y = indoc! {r#"
            providers:
              - aws
        "#};
        let ds: DataSource = serde_yaml::from_str(y).unwrap();
        assert_eq!(ds.providers, vec!["aws"]);
        assert!(ds.path.is_empty());
        assert!(ds.userdata_name.is_empty());
    }

    #[test]
    fn maximal_yaml_all_fields() {
        let y = indoc! {r#"
            providers:
              - aws
              - gce
              - azure
              - digitalocean
            path: /var/lib/cloud/instance
            userdata_name: user-data.yaml
        "#};
        let ds: DataSource = serde_yaml::from_str(y).unwrap();
        assert_eq!(ds.providers, vec!["aws", "gce", "azure", "digitalocean"]);
        assert_eq!(ds.path, "/var/lib/cloud/instance");
        assert_eq!(ds.userdata_name, "user-data.yaml");
    }

    #[test]
    fn default_skips_empty_keys() {
        let s = serde_yaml::to_string(&DataSource::default()).unwrap();
        assert!(!s.contains("providers"));
        assert!(!s.contains("path"));
        assert!(!s.contains("userdata_name"));
    }

    #[test]
    fn yaml_key_userdata_name_matches_go_tag() {
        // Go uses `userdata_name` (snake_case) in the yaml tag; ensure no
        // unintended camelCase variant works.
        let y = indoc! {r#"
            userdata_name: u
        "#};
        let ds: DataSource = serde_yaml::from_str(y).unwrap();
        assert_eq!(ds.userdata_name, "u");
        // Round-trip with that exact key.
        let s = serde_yaml::to_string(&ds).unwrap();
        assert!(s.contains("userdata_name"));
        assert!(!s.contains("userdataName"));
    }

    #[test]
    fn empty_providers_omitted_in_yaml() {
        let ds = DataSource {
            providers: Vec::new(),
            path: "/p".into(),
            userdata_name: String::new(),
        };
        let s = serde_yaml::to_string(&ds).unwrap();
        assert!(!s.contains("providers"));
        assert!(s.contains("path"));
        // Round-trip preserves equality.
        let back: DataSource = serde_yaml::from_str(&s).unwrap();
        assert_eq!(back, ds);
    }

    #[test]
    fn datasource_provider_is_string_alias() {
        // DataSourceProvider should be a plain String alias.
        let p: DataSourceProvider = "noop".to_string();
        assert_eq!(p, "noop");
        let ds = DataSource {
            providers: vec![p.clone()],
            ..Default::default()
        };
        assert_eq!(ds.providers[0], "noop");
    }
}
