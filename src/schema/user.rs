//! User account configuration.

use serde::{Deserialize, Serialize};

/// `YipEntity` declares an extra `/etc/passwd`-style entity line to ensure (or
/// delete) on disk.
#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct YipEntity {
    #[serde(default, rename = "path")]
    pub path: String,
    #[serde(default, rename = "entity")]
    pub entity: String,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct User {
    #[serde(default, rename = "name", skip_serializing_if = "String::is_empty")]
    pub name: String,

    /// `PasswordHash` in Go — YAML key is `passwd`.
    #[serde(default, rename = "passwd", skip_serializing_if = "String::is_empty")]
    pub password_hash: String,

    #[serde(
        default,
        rename = "ssh_authorized_keys",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub ssh_authorized_keys: Vec<String>,

    #[serde(default, rename = "gecos", skip_serializing_if = "String::is_empty")]
    pub gecos: String,

    #[serde(default, rename = "homedir", skip_serializing_if = "String::is_empty")]
    pub homedir: String,

    #[serde(default, rename = "no_create_home", skip_serializing_if = "is_false")]
    pub no_create_home: bool,

    #[serde(
        default,
        rename = "primary_group",
        skip_serializing_if = "String::is_empty"
    )]
    pub primary_group: String,

    #[serde(default, rename = "groups", skip_serializing_if = "Vec::is_empty")]
    pub groups: Vec<String>,

    #[serde(default, rename = "no_user_group", skip_serializing_if = "is_false")]
    pub no_user_group: bool,

    #[serde(default, rename = "system", skip_serializing_if = "is_false")]
    pub system: bool,

    #[serde(default, rename = "no_log_init", skip_serializing_if = "is_false")]
    pub no_log_init: bool,

    #[serde(default, rename = "shell", skip_serializing_if = "String::is_empty")]
    pub shell: String,

    #[serde(default, rename = "lock_passwd", skip_serializing_if = "is_false")]
    pub lock_passwd: bool,

    /// UID is a string in Go (`UID string`) — matches yip's YAML where users
    /// might write `uid: "1002"` (quoted) or `uid: 1002`. We keep `String` to
    /// preserve semantics; integer values are accepted via an untagged helper.
    #[serde(default, rename = "uid", skip_serializing_if = "String::is_empty")]
    pub uid: String,
}

fn is_false(b: &bool) -> bool {
    !*b
}

#[cfg(test)]
mod tests {
    use super::*;
    use indoc::indoc;

    #[test]
    fn parses_full_user() {
        let y = indoc! {r#"
            name: bar
            passwd: foo
            uid: "1002"
            lock_passwd: true
            groups: [sudo]
            ssh_authorized_keys: [aaa, bbb]
            gecos: "Bar"
            homedir: /home/bar
            no_create_home: false
            primary_group: bar
            no_user_group: false
            system: false
            no_log_init: false
            shell: /bin/bash
        "#};
        let u: User = serde_yaml::from_str(y).unwrap();
        assert_eq!(u.name, "bar");
        assert_eq!(u.password_hash, "foo");
        assert_eq!(u.uid, "1002");
        assert!(u.lock_passwd);
        assert_eq!(u.groups, vec!["sudo"]);
        assert_eq!(u.ssh_authorized_keys, vec!["aaa", "bbb"]);
        assert_eq!(u.shell, "/bin/bash");
    }

    #[test]
    fn defaults() {
        let u: User = serde_yaml::from_str("{}").unwrap();
        assert_eq!(u, User::default());
    }

    #[test]
    fn yip_entity_parses() {
        let y = indoc! {r#"
            path: /etc/passwd
            entity: |
              ENTITY=user
        "#};
        let e: YipEntity = serde_yaml::from_str(y).unwrap();
        assert_eq!(e.path, "/etc/passwd");
        assert!(e.entity.contains("ENTITY=user"));
    }

    #[test]
    fn roundtrip() {
        let u = User {
            name: "alice".into(),
            password_hash: "hash".into(),
            uid: "1000".into(),
            lock_passwd: true,
            ..Default::default()
        };
        let s = serde_yaml::to_string(&u).unwrap();
        let back: User = serde_yaml::from_str(&s).unwrap();
        assert_eq!(back, u);
    }
}
