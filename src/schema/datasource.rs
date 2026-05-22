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
}
