//! Container image unpack configuration (`unpack_images` stage field).

use serde::{Deserialize, Serialize};

#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct UnpackImageConf {
    #[serde(default, rename = "source", skip_serializing_if = "String::is_empty")]
    pub source: String,
    #[serde(default, rename = "target", skip_serializing_if = "String::is_empty")]
    pub target: String,
    #[serde(default, rename = "platform", skip_serializing_if = "String::is_empty")]
    pub platform: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use indoc::indoc;

    #[test]
    fn parses() {
        let y = indoc! {r#"
            source: quay.io/foo/bar:latest
            target: /var/lib/foo
            platform: linux/amd64
        "#};
        let u: UnpackImageConf = serde_yaml::from_str(y).unwrap();
        assert_eq!(u.source, "quay.io/foo/bar:latest");
        assert_eq!(u.target, "/var/lib/foo");
        assert_eq!(u.platform, "linux/amd64");
    }

    #[test]
    fn defaults() {
        let u: UnpackImageConf = serde_yaml::from_str("{}").unwrap();
        assert_eq!(u, UnpackImageConf::default());
    }

    #[test]
    fn roundtrip() {
        let u = UnpackImageConf {
            source: "x".into(),
            target: "y".into(),
            platform: "z".into(),
        };
        let s = serde_yaml::to_string(&u).unwrap();
        let back: UnpackImageConf = serde_yaml::from_str(&s).unwrap();
        assert_eq!(back, u);
    }
}
