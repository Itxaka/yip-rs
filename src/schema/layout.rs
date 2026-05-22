//! Disk layout configuration (used by the `layout` plugin).

use serde::{Deserialize, Serialize};

#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Device {
    #[serde(default, rename = "init_disk", skip_serializing_if = "is_false")]
    pub init_disk: bool,
    #[serde(default, rename = "disk_name", skip_serializing_if = "String::is_empty")]
    pub disk_name: String,
    #[serde(default, rename = "label", skip_serializing_if = "String::is_empty")]
    pub label: String,
    /// The block device to operate on (e.g. `/dev/sda`). Also accepts
    /// `script://<command>` — the command is executed and its stdout
    /// (trimmed) is used as the device path.
    #[serde(default, rename = "path", skip_serializing_if = "String::is_empty")]
    pub path: String,
}

/// Expand the last partition on the disk to fill (up to) `size` MiB.
/// In Go this is named `Expand` and renamed via the `expand_partition`
/// YAML tag on `Layout`. We name the type `ExpandPartition` here for
/// clarity at the type level — the YAML key stays `expand_partition`.
#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ExpandPartition {
    #[serde(default, rename = "size", skip_serializing_if = "is_zero_u64")]
    pub size: u64,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Partition {
    #[serde(default, rename = "fsLabel", skip_serializing_if = "String::is_empty")]
    pub fs_label: String,
    #[serde(default, rename = "size", skip_serializing_if = "is_zero_u64")]
    pub size: u64,
    #[serde(default, rename = "pLabel", skip_serializing_if = "String::is_empty")]
    pub p_label: String,
    #[serde(default, rename = "filesystem", skip_serializing_if = "String::is_empty")]
    pub file_system: String,
    #[serde(default, rename = "bootable", skip_serializing_if = "is_false")]
    pub bootable: bool,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Layout {
    #[serde(default, rename = "device", skip_serializing_if = "Option::is_none")]
    pub device: Option<Device>,
    #[serde(default, rename = "expand_partition", skip_serializing_if = "Option::is_none")]
    pub expand: Option<ExpandPartition>,
    /// Go field name is `Parts`, YAML key is `add_partitions`.
    #[serde(default, rename = "add_partitions", skip_serializing_if = "Vec::is_empty")]
    pub parts: Vec<Partition>,
}

fn is_false(b: &bool) -> bool {
    !*b
}

fn is_zero_u64(v: &u64) -> bool {
    *v == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use indoc::indoc;

    #[test]
    fn parses_full_layout() {
        let y = indoc! {r#"
            device:
              path: /dev/sda
              label: gpt
              init_disk: true
              disk_name: my-disk
            expand_partition:
              size: 1024
            add_partitions:
              - fsLabel: COS_PERSISTENT
                pLabel: persistent
                size: 4096
                filesystem: ext4
                bootable: false
        "#};
        let l: Layout = serde_yaml::from_str(y).unwrap();
        let d = l.device.as_ref().unwrap();
        assert_eq!(d.path, "/dev/sda");
        assert_eq!(d.label, "gpt");
        assert!(d.init_disk);
        assert_eq!(d.disk_name, "my-disk");

        assert_eq!(l.expand.as_ref().unwrap().size, 1024);

        assert_eq!(l.parts.len(), 1);
        assert_eq!(l.parts[0].fs_label, "COS_PERSISTENT");
        assert_eq!(l.parts[0].p_label, "persistent");
        assert_eq!(l.parts[0].size, 4096);
        assert_eq!(l.parts[0].file_system, "ext4");
        assert!(!l.parts[0].bootable);
    }

    #[test]
    fn empty_yaml_default() {
        let l: Layout = serde_yaml::from_str("{}").unwrap();
        assert!(l.device.is_none());
        assert!(l.expand.is_none());
        assert!(l.parts.is_empty());
    }

    #[test]
    fn roundtrip() {
        let l = Layout {
            device: Some(Device {
                path: "/".into(),
                ..Default::default()
            }),
            expand: Some(ExpandPartition { size: 0 }),
            parts: vec![Partition {
                fs_label: "X".into(),
                size: 100,
                ..Default::default()
            }],
        };
        let s = serde_yaml::to_string(&l).unwrap();
        let back: Layout = serde_yaml::from_str(&s).unwrap();
        assert_eq!(back, l);
    }
}
