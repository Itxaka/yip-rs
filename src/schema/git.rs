//! Git repository config (used by the `git` plugin).

use serde::{Deserialize, Serialize};

#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Auth {
    #[serde(default, rename = "username", skip_serializing_if = "String::is_empty")]
    pub username: String,
    #[serde(default, rename = "password", skip_serializing_if = "String::is_empty")]
    pub password: String,
    #[serde(default, rename = "private_key", skip_serializing_if = "String::is_empty")]
    pub private_key: String,
    #[serde(default, rename = "insecure", skip_serializing_if = "is_false")]
    pub insecure: bool,
    #[serde(default, rename = "public_key", skip_serializing_if = "String::is_empty")]
    pub public_key: String,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Git {
    #[serde(default, rename = "auth", skip_serializing_if = "auth_is_default")]
    pub auth: Auth,
    #[serde(default, rename = "url", skip_serializing_if = "String::is_empty")]
    pub url: String,
    #[serde(default, rename = "path", skip_serializing_if = "String::is_empty")]
    pub path: String,
    #[serde(default, rename = "branch", skip_serializing_if = "String::is_empty")]
    pub branch: String,
    #[serde(default, rename = "branch_only", skip_serializing_if = "is_false")]
    pub branch_only: bool,
}

fn is_false(b: &bool) -> bool {
    !*b
}

fn auth_is_default(a: &Auth) -> bool {
    a == &Auth::default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use indoc::indoc;
    use pretty_assertions::assert_eq;

    #[test]
    fn parses_full_git() {
        let y = indoc! {r#"
            url: https://example.com/foo.git
            path: /opt/foo
            branch: main
            branch_only: true
            auth:
              username: alice
              password: hunter2
              private_key: KEY
              insecure: true
              public_key: PUB
        "#};
        let g: Git = serde_yaml::from_str(y).unwrap();
        assert_eq!(g.url, "https://example.com/foo.git");
        assert_eq!(g.path, "/opt/foo");
        assert_eq!(g.branch, "main");
        assert!(g.branch_only);
        assert_eq!(g.auth.username, "alice");
        assert_eq!(g.auth.password, "hunter2");
        assert_eq!(g.auth.private_key, "KEY");
        assert!(g.auth.insecure);
        assert_eq!(g.auth.public_key, "PUB");
    }

    #[test]
    fn defaults_ok() {
        let g: Git = serde_yaml::from_str("{}").unwrap();
        assert_eq!(g, Git::default());
    }

    #[test]
    fn roundtrip() {
        let g = Git {
            auth: Auth {
                username: "u".into(),
                password: "p".into(),
                ..Default::default()
            },
            url: "https://x".into(),
            branch: "main".into(),
            ..Default::default()
        };
        let s = serde_yaml::to_string(&g).unwrap();
        let back: Git = serde_yaml::from_str(&s).unwrap();
        assert_eq!(back, g);
    }

    #[test]
    fn minimal_yaml_url_and_path() {
        let y = indoc! {r#"
            url: https://example.com/r.git
            path: /opt/r
        "#};
        let g: Git = serde_yaml::from_str(y).unwrap();
        assert_eq!(g.url, "https://example.com/r.git");
        assert_eq!(g.path, "/opt/r");
        assert!(g.branch.is_empty());
        assert!(!g.branch_only);
        assert_eq!(g.auth, Auth::default());
    }

    #[test]
    fn branch_only_yaml_alias_matches_go_tag() {
        // YAML must be `branch_only`, not `BranchOnly`.
        let y = indoc! {r#"
            url: u
            branch_only: true
        "#};
        let g: Git = serde_yaml::from_str(y).unwrap();
        assert!(g.branch_only);
        // Round-trip should also use `branch_only`.
        let s = serde_yaml::to_string(&g).unwrap();
        assert!(s.contains("branch_only"));
        assert!(!s.contains("BranchOnly"));
    }

    #[test]
    fn default_git_yaml_omits_keys() {
        let s = serde_yaml::to_string(&Git::default()).unwrap();
        assert!(!s.contains("auth"));
        assert!(!s.contains("url"));
        assert!(!s.contains("path"));
        assert!(!s.contains("branch"));
        assert!(!s.contains("branch_only"));
    }

    #[test]
    fn auth_yaml_keys_match_go_tags() {
        // private_key / public_key are snake_case in Go yaml tags.
        let y = indoc! {r#"
            url: u
            auth:
              username: bob
              password: pw
              private_key: PRIV
              public_key: PUB
              insecure: true
        "#};
        let g: Git = serde_yaml::from_str(y).unwrap();
        assert_eq!(g.auth.username, "bob");
        assert_eq!(g.auth.password, "pw");
        assert_eq!(g.auth.private_key, "PRIV");
        assert_eq!(g.auth.public_key, "PUB");
        assert!(g.auth.insecure);
    }

    #[test]
    fn auth_default_for_empty_yaml() {
        let a: Auth = serde_yaml::from_str("{}").unwrap();
        assert_eq!(a, Auth::default());
    }

    #[test]
    fn maximal_git_roundtrip() {
        let g = Git {
            auth: Auth {
                username: "u".into(),
                password: "p".into(),
                private_key: "PRIV".into(),
                insecure: true,
                public_key: "PUB".into(),
            },
            url: "git@host:org/repo.git".into(),
            path: "/srv/repo".into(),
            branch: "release/1.0".into(),
            branch_only: true,
        };
        let s = serde_yaml::to_string(&g).unwrap();
        let back: Git = serde_yaml::from_str(&s).unwrap();
        assert_eq!(back, g);
    }

    #[test]
    fn auth_skipped_when_default_even_with_other_fields() {
        // If only Git.url is set, the auth section should not appear in YAML.
        let g = Git {
            url: "u".into(),
            ..Default::default()
        };
        let s = serde_yaml::to_string(&g).unwrap();
        assert!(!s.contains("auth"));
        assert!(s.contains("url"));
    }
}
