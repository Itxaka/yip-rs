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
    use pretty_assertions::assert_eq;

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

    #[test]
    fn minimal_yaml_name_only() {
        let u: User = serde_yaml::from_str("name: bob\n").unwrap();
        assert_eq!(u.name, "bob");
        assert!(u.password_hash.is_empty());
        assert!(u.groups.is_empty());
        assert!(u.ssh_authorized_keys.is_empty());
        assert!(!u.lock_passwd);
        assert!(!u.system);
    }

    #[test]
    fn empty_groups_list_round_trips_as_default() {
        // Edge case: explicitly empty list should serialise as a default
        // (Vec::is_empty causes the key to be omitted).
        let u = User {
            name: "u".into(),
            groups: Vec::new(),
            ssh_authorized_keys: Vec::new(),
            ..Default::default()
        };
        let s = serde_yaml::to_string(&u).unwrap();
        assert!(!s.contains("groups"));
        assert!(!s.contains("ssh_authorized_keys"));
        let back: User = serde_yaml::from_str(&s).unwrap();
        assert_eq!(back, u);
    }

    #[test]
    fn yaml_aliases_match_go_tags() {
        // Verify YAML key spelling matches Go yaml tags: passwd, no_create_home,
        // primary_group, no_user_group, no_log_init, lock_passwd,
        // ssh_authorized_keys.
        let y = indoc! {r#"
            name: u
            passwd: HASH
            no_create_home: true
            primary_group: admin
            no_user_group: true
            no_log_init: true
            lock_passwd: true
            ssh_authorized_keys: [k1]
        "#};
        let u: User = serde_yaml::from_str(y).unwrap();
        assert_eq!(u.password_hash, "HASH");
        assert!(u.no_create_home);
        assert_eq!(u.primary_group, "admin");
        assert!(u.no_user_group);
        assert!(u.no_log_init);
        assert!(u.lock_passwd);
        assert_eq!(u.ssh_authorized_keys, vec!["k1"]);
    }

    #[test]
    fn maximal_user_roundtrip() {
        let u = User {
            name: "alice".into(),
            password_hash: "$6$abc".into(),
            ssh_authorized_keys: vec!["ssh-ed25519 AAAA".into(), "ssh-rsa BBBB".into()],
            gecos: "Alice".into(),
            homedir: "/home/alice".into(),
            no_create_home: true,
            primary_group: "alice".into(),
            groups: vec!["wheel".into(), "docker".into()],
            no_user_group: true,
            system: true,
            no_log_init: true,
            shell: "/bin/zsh".into(),
            lock_passwd: true,
            uid: "2000".into(),
        };
        let s = serde_yaml::to_string(&u).unwrap();
        let back: User = serde_yaml::from_str(&s).unwrap();
        assert_eq!(back, u);
    }

    #[test]
    fn empty_user_yaml_omits_everything() {
        let s = serde_yaml::to_string(&User::default()).unwrap();
        // Default user must serialise to an empty mapping / nothing recognisable.
        assert!(!s.contains("passwd"));
        assert!(!s.contains("groups"));
        assert!(!s.contains("ssh_authorized_keys"));
        assert!(!s.contains("uid"));
        assert!(!s.contains("lock_passwd"));
    }

    #[test]
    fn yip_entity_default_for_empty_yaml() {
        let e: YipEntity = serde_yaml::from_str("{}").unwrap();
        assert_eq!(e, YipEntity::default());
    }

    #[test]
    fn yip_entity_roundtrip() {
        let e = YipEntity {
            path: "/etc/passwd".into(),
            entity: "kind: user\nname: foo\n".into(),
        };
        let s = serde_yaml::to_string(&e).unwrap();
        let back: YipEntity = serde_yaml::from_str(&s).unwrap();
        assert_eq!(back, e);
    }
}
