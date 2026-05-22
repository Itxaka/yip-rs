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
}
