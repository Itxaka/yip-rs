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
    use pretty_assertions::assert_eq;

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

    #[test]
    fn minimal_yaml_source_only() {
        let y = indoc! {r#"
            source: docker.io/library/alpine
        "#};
        let u: UnpackImageConf = serde_yaml::from_str(y).unwrap();
        assert_eq!(u.source, "docker.io/library/alpine");
        assert!(u.target.is_empty());
        assert!(u.platform.is_empty());
    }

    #[test]
    fn maximal_yaml_roundtrip() {
        let u = UnpackImageConf {
            source: "ghcr.io/org/img:tag".into(),
            target: "/var/lib/rootfs".into(),
            platform: "linux/arm64".into(),
        };
        let s = serde_yaml::to_string(&u).unwrap();
        let back: UnpackImageConf = serde_yaml::from_str(&s).unwrap();
        assert_eq!(back, u);
    }

    #[test]
    fn default_omits_keys_in_yaml() {
        let s = serde_yaml::to_string(&UnpackImageConf::default()).unwrap();
        assert!(!s.contains("source"));
        assert!(!s.contains("target"));
        assert!(!s.contains("platform"));
    }

    #[test]
    fn yaml_keys_match_go_tags() {
        // Go yaml tags: source, target, platform — all lowercase.
        let y = indoc! {r#"
            source: a
            target: b
            platform: c
        "#};
        let u: UnpackImageConf = serde_yaml::from_str(y).unwrap();
        assert_eq!(u.source, "a");
        assert_eq!(u.target, "b");
        assert_eq!(u.platform, "c");
        let s = serde_yaml::to_string(&u).unwrap();
        assert!(s.contains("source: a"));
        assert!(s.contains("target: b"));
        assert!(s.contains("platform: c"));
    }

    #[test]
    fn parses_image_with_digest() {
        // Edge case: source can reference an image by digest.
        let y = indoc! {r#"
            source: "registry.example/img@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
            target: /mnt
        "#};
        let u: UnpackImageConf = serde_yaml::from_str(y).unwrap();
        assert!(u.source.contains("@sha256:"));
        assert_eq!(u.target, "/mnt");
    }
}
