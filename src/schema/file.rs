//! File / Download / Directory schema entries.
//!
//! Go's `File` / `Download` / `Directory` structs have no `yaml:"..."` tags,
//! which means gopkg.in/yaml.v3 lowercases each field name to derive the key.
//! Concretely: `Path` → `path`, `Permissions` → `permissions`,
//! `Owner` → `owner`, `Group` → `group`, `Content` → `content`,
//! `Encoding` → `encoding`, `URL` → `url`, `Timeout` → `timeout`,
//! `OwnerString` → `ownerstring`.
//!
//! In Go `Owner` and `Group` are `int`, but cloud-init style YAML often
//! supplies a username string. yip handles that by populating a separate
//! `OwnerString` field from the cloud-init loader. In Rust we additionally
//! accept `owner: "alice"` directly via a tolerant enum-deserialization so
//! callers don't need to round-trip through the cloud-init loader.

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Numeric or string owner/group identifier. Mirrors Go's pair of
/// `Owner int` + `OwnerString string`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OwnerId {
    Numeric(i32),
    Name(String),
}

impl Default for OwnerId {
    fn default() -> Self {
        OwnerId::Numeric(0)
    }
}

impl OwnerId {
    /// Returns the numeric owner (0 if a name).
    pub fn as_int(&self) -> i32 {
        match self {
            OwnerId::Numeric(n) => *n,
            OwnerId::Name(_) => 0,
        }
    }

    /// Returns the string form if any.
    pub fn as_name(&self) -> Option<&str> {
        match self {
            OwnerId::Name(s) if !s.is_empty() => Some(s),
            _ => None,
        }
    }
}

impl Serialize for OwnerId {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            OwnerId::Numeric(n) => s.serialize_i32(*n),
            OwnerId::Name(name) => s.serialize_str(name),
        }
    }
}

impl<'de> Deserialize<'de> for OwnerId {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Helper {
            Int(i64),
            Str(String),
        }
        match Helper::deserialize(d)? {
            Helper::Int(n) => Ok(OwnerId::Numeric(n as i32)),
            Helper::Str(s) => {
                // Accept "1000" as numeric too.
                if let Ok(n) = s.parse::<i32>() {
                    Ok(OwnerId::Numeric(n))
                } else {
                    Ok(OwnerId::Name(s))
                }
            }
        }
    }
}

fn owner_is_default(o: &OwnerId) -> bool {
    matches!(o, OwnerId::Numeric(0))
}

fn is_zero_u32(v: &u32) -> bool {
    *v == 0
}

fn is_zero_i32(v: &i32) -> bool {
    *v == 0
}

#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct File {
    #[serde(default, rename = "path", skip_serializing_if = "String::is_empty")]
    pub path: String,
    #[serde(default, rename = "permissions", skip_serializing_if = "is_zero_u32")]
    pub permissions: u32,
    #[serde(default, rename = "owner", skip_serializing_if = "owner_is_default")]
    pub owner: OwnerId,
    #[serde(default, rename = "group", skip_serializing_if = "is_zero_i32")]
    pub group: i32,
    #[serde(default, rename = "content", skip_serializing_if = "String::is_empty")]
    pub content: String,
    #[serde(default, rename = "encoding", skip_serializing_if = "String::is_empty")]
    pub encoding: String,
    /// Distinct field from `owner` — populated when the source YAML supplied a
    /// username string and was processed by the cloud-init loader. Native yip
    /// YAML may still set it explicitly via `ownerstring: alice`.
    #[serde(default, rename = "ownerstring", skip_serializing_if = "String::is_empty")]
    pub owner_string: String,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Download {
    #[serde(default, rename = "path", skip_serializing_if = "String::is_empty")]
    pub path: String,
    #[serde(default, rename = "url", skip_serializing_if = "String::is_empty")]
    pub url: String,
    #[serde(default, rename = "permissions", skip_serializing_if = "is_zero_u32")]
    pub permissions: u32,
    #[serde(default, rename = "owner", skip_serializing_if = "owner_is_default")]
    pub owner: OwnerId,
    #[serde(default, rename = "group", skip_serializing_if = "is_zero_i32")]
    pub group: i32,
    #[serde(default, rename = "timeout", skip_serializing_if = "is_zero_i32")]
    pub timeout: i32,
    #[serde(default, rename = "ownerstring", skip_serializing_if = "String::is_empty")]
    pub owner_string: String,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Directory {
    #[serde(default, rename = "path", skip_serializing_if = "String::is_empty")]
    pub path: String,
    #[serde(default, rename = "permissions", skip_serializing_if = "is_zero_u32")]
    pub permissions: u32,
    #[serde(default, rename = "owner", skip_serializing_if = "owner_is_default")]
    pub owner: OwnerId,
    #[serde(default, rename = "group", skip_serializing_if = "is_zero_i32")]
    pub group: i32,
}

#[cfg(test)]
mod tests {
    use super::*;
    use indoc::indoc;

    #[test]
    fn file_parses_numeric_owner() {
        let y = indoc! {r#"
            path: /etc/foo
            permissions: 420
            owner: 1000
            group: 1000
            content: hi
            encoding: b64
        "#};
        let f: File = serde_yaml::from_str(y).unwrap();
        assert_eq!(f.path, "/etc/foo");
        assert_eq!(f.permissions, 420);
        assert_eq!(f.owner, OwnerId::Numeric(1000));
        assert_eq!(f.group, 1000);
        assert_eq!(f.content, "hi");
        assert_eq!(f.encoding, "b64");
    }

    #[test]
    fn file_accepts_string_owner() {
        let y = indoc! {r#"
            path: /etc/foo
            owner: alice
        "#};
        let f: File = serde_yaml::from_str(y).unwrap();
        assert_eq!(f.owner, OwnerId::Name("alice".into()));
        assert_eq!(f.owner.as_name(), Some("alice"));
    }

    #[test]
    fn file_quoted_numeric_string_owner_is_numeric() {
        let y = indoc! {r#"
            path: /etc/foo
            owner: "1000"
        "#};
        let f: File = serde_yaml::from_str(y).unwrap();
        assert_eq!(f.owner, OwnerId::Numeric(1000));
    }

    #[test]
    fn file_owner_string_field_separate() {
        // Cloud-init style — the loader normally fills this. We accept it
        // explicitly too. Note the unusual key: yaml.v3 default is lowercase
        // concatenation, so OwnerString → ownerstring.
        let y = indoc! {r#"
            path: /etc/foo
            ownerstring: alice
        "#};
        let f: File = serde_yaml::from_str(y).unwrap();
        assert_eq!(f.owner_string, "alice");
    }

    #[test]
    fn file_default() {
        let f: File = serde_yaml::from_str("{}").unwrap();
        assert_eq!(f, File::default());
    }

    #[test]
    fn download_parses() {
        let y = indoc! {r#"
            path: /tmp/x
            url: https://example.com/x
            permissions: 420
            owner: 0
            group: 0
            timeout: 30
        "#};
        let d: Download = serde_yaml::from_str(y).unwrap();
        assert_eq!(d.path, "/tmp/x");
        assert_eq!(d.url, "https://example.com/x");
        assert_eq!(d.timeout, 30);
    }

    #[test]
    fn directory_parses() {
        let y = indoc! {r#"
            path: /tmp/d
            permissions: 511
            owner: 0
        "#};
        let d: Directory = serde_yaml::from_str(y).unwrap();
        assert_eq!(d.path, "/tmp/d");
        assert_eq!(d.permissions, 511);
    }

    #[test]
    fn file_roundtrip_numeric() {
        let f = File {
            path: "/p".into(),
            permissions: 0o644,
            owner: OwnerId::Numeric(1000),
            group: 1000,
            content: "abc".into(),
            encoding: "".into(),
            owner_string: "".into(),
        };
        let s = serde_yaml::to_string(&f).unwrap();
        let back: File = serde_yaml::from_str(&s).unwrap();
        assert_eq!(back, f);
    }

    #[test]
    fn file_roundtrip_name_owner() {
        let f = File {
            path: "/p".into(),
            owner: OwnerId::Name("alice".into()),
            ..Default::default()
        };
        let s = serde_yaml::to_string(&f).unwrap();
        let back: File = serde_yaml::from_str(&s).unwrap();
        assert_eq!(back, f);
    }
}
