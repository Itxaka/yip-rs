//! End-to-end YAML parsing tests for `crate::schema`.
//!
//! These mirror selected fixtures from yip's Go test suite
//! (`pkg/schema/schema_test.go` and `pkg/executor/default_test.go`) so we can
//! demonstrate parity with the original implementation.

use indoc::indoc;
use pretty_assertions::assert_eq;
use std::collections::HashMap;

use yip::schema::{
    dot_notation_modifier, Auth, Config, DataSource, Dependency, Device, Directory, ExpandPartition,
    File, Git, IfCheckType, Layout, OwnerId, PackagePins, Packages, Partition, Stage, User,
    YipEntity,
};

/// Reproduces the dot-notation cases from `schema_test.go`:
///     stages.foo[0].name=bar boo.baz
///     stages.foo[0].name=bar   stages.foo[0].commands[0]=baz
///     ip=dhcp test="echo ping_test_host=127.0.0.1  > /tmp/jojo"
///     stages.foo[0].name=bar ip=dhcp test="echo ping_test_host=127.0.0.1  > /tmp/dio"
#[test]
fn dot_notation_one_config_with_garbage() {
    let yaml = dot_notation_modifier(b"stages.foo[0].name=bar boo.baz").unwrap();
    let cfg = Config::load(&yaml).unwrap();
    assert_eq!(cfg.stages["foo"][0].name, "bar");
}

#[test]
fn dot_notation_two_configs() {
    let yaml =
        dot_notation_modifier(b"stages.foo[0].name=bar   stages.foo[0].commands[0]=baz").unwrap();
    let cfg = Config::load(&yaml).unwrap();
    assert_eq!(cfg.stages["foo"][0].name, "bar");
    assert_eq!(cfg.stages["foo"][0].commands[0], "baz");
}

#[test]
fn dot_notation_invalid_yields_empty_stages() {
    // Mirrors Go's `threeConfigInvalid`: no `stages.*` token at all, so the
    // resulting Config should have an empty stages map and no name.
    let yaml = dot_notation_modifier(
        br#"ip=dhcp test="echo ping_test_host=127.0.0.1  > /tmp/jojo""#,
    )
    .unwrap();
    let cfg = Config::load(&yaml).unwrap();
    assert!(cfg.stages.is_empty());
    assert!(cfg.name.is_empty());
}

#[test]
fn dot_notation_half_invalid_still_loads_valid_part() {
    let yaml = dot_notation_modifier(
        br#"stages.foo[0].name=bar ip=dhcp test="echo ping_test_host=127.0.0.1  > /tmp/dio""#,
    )
    .unwrap();
    let cfg = Config::load(&yaml).unwrap();
    assert!(cfg.name.is_empty());
    assert_eq!(cfg.stages["foo"][0].name, "bar");
}

/// Reproduces the spirit of the cloud-init suite's first multi-stage test by
/// asserting that a yip-native YAML with `users`, `files`, `commands`,
/// `environment`, and `layout` parses end-to-end with all expected fields.
#[test]
fn yip_native_multi_stage_full_fixture() {
    let y = indoc! {r#"
        stages:
          boot:
            - users:
                bar:
                  name: bar
                  passwd: foo
                  uid: "1002"
                  lock_passwd: true
                  groups: [sudo]
                  ssh_authorized_keys: [faaapploo]
              authorized_keys:
                bar: [asdd]
              files:
                - path: /foo/bar
                  permissions: 420
                  ownerstring: bar
                  encoding: b64
                  content: CiMgVGhpcyBmaWxlIGNvbnRyb2xzIHRoZSBzdGF0ZSBvZiBTRUxpbnV4
              commands: [foo]
            - layout:
                expand_partition:
                  size: 0
                device:
                  path: /
          initramfs:
            - hostname: bar
          test:
            - environment:
                foo: bar
    "#};
    let cfg = Config::load(y.as_bytes()).unwrap();
    assert_eq!(cfg.stages.len(), 3);

    let boot0 = &cfg.stages["boot"][0];
    assert_eq!(boot0.users["bar"].uid, "1002");
    assert_eq!(boot0.users["bar"].password_hash, "foo");
    assert!(boot0.users["bar"].lock_passwd);
    assert_eq!(boot0.users["bar"].groups, vec!["sudo"]);
    assert_eq!(
        boot0.users["bar"].ssh_authorized_keys,
        vec!["faaapploo".to_string()]
    );
    assert_eq!(
        boot0.ssh_keys.get("bar").unwrap(),
        &vec!["asdd".to_string()]
    );

    assert_eq!(boot0.files[0].path, "/foo/bar");
    assert_eq!(boot0.files[0].permissions, 0o644);
    assert_eq!(boot0.files[0].encoding, "b64");
    assert_eq!(boot0.files[0].owner_string, "bar");
    assert_eq!(boot0.commands, vec!["foo".to_string()]);

    let boot1 = &cfg.stages["boot"][1];
    assert_eq!(boot1.layout.expand.as_ref().unwrap().size, 0);
    assert_eq!(boot1.layout.device.as_ref().unwrap().path, "/");

    assert_eq!(cfg.stages["initramfs"][0].hostname, "bar");
    assert_eq!(
        cfg.stages["test"][0].environment.get("foo").unwrap(),
        "bar"
    );
}

/// Reproduces the executor `Interpolates sys info` / `Creates dirs` fixtures —
/// the actual Go assertions are about runtime behaviour, but the YAML they
/// build via struct literals must round-trip through our schema cleanly.
#[test]
fn executor_test_fixtures_roundtrip() {
    let mut stages: HashMap<String, Vec<Stage>> = HashMap::new();
    stages.insert(
        "foo".into(),
        vec![Stage {
            files: vec![File {
                path: "/tmp/test/foo".into(),
                content: "{{.Values.node.hostname}}".into(),
                permissions: 0o777,
                ..Default::default()
            }],
            ..Default::default()
        }],
    );
    let cfg = Config {
        stages,
        ..Default::default()
    };
    let yaml = cfg.to_string_yaml().unwrap();
    let back = Config::load(yaml.as_bytes()).unwrap();
    assert_eq!(back, cfg);
    assert_eq!(back.stages["foo"][0].files[0].permissions, 0o777);

    // Directory-only stage from the executor "Creates dirs" test.
    let mut stages2: HashMap<String, Vec<Stage>> = HashMap::new();
    stages2.insert(
        "foo".into(),
        vec![Stage {
            directories: vec![Directory {
                path: "/tmp/boo".into(),
                permissions: 0o777,
                ..Default::default()
            }],
            ..Default::default()
        }],
    );
    let cfg2 = Config {
        stages: stages2,
        ..Default::default()
    };
    let yaml2 = cfg2.to_string_yaml().unwrap();
    let back2 = Config::load(yaml2.as_bytes()).unwrap();
    assert_eq!(back2, cfg2);
}

/// Files / Owner / Group: cover both the numeric and the username form.
#[test]
fn file_owner_either_form() {
    let yaml = indoc! {r#"
        stages:
          test:
            - files:
                - path: /etc/a
                  owner: 1000
                  group: 1000
                - path: /etc/b
                  owner: alice
    "#};
    let cfg = Config::load(yaml.as_bytes()).unwrap();
    let files = &cfg.stages["test"][0].files;
    assert_eq!(files[0].owner, OwnerId::Numeric(1000));
    assert_eq!(files[0].group, 1000);
    assert_eq!(files[1].owner, OwnerId::Name("alice".into()));
}

/// Conditional + if_files combo.
#[test]
fn conditionals_and_if_files() {
    let yaml = indoc! {r#"
        stages:
          test:
            - name: cond
              only_os: linux
              only_arch: amd64
              only_os_version: "22.04"
              only_service_manager: systemd
              if_files:
                any: [/etc/foo]
                all: [/etc/bar, /etc/baz]
    "#};
    let cfg = Config::load(yaml.as_bytes()).unwrap();
    let s = &cfg.stages["test"][0];
    assert_eq!(s.only_if_os, "linux");
    assert_eq!(s.only_if_arch, "amd64");
    assert_eq!(s.only_if_os_version, "22.04");
    assert_eq!(s.only_if_service_manager, "systemd");
    assert_eq!(
        s.if_files.get(&IfCheckType::Any).unwrap(),
        &vec!["/etc/foo".to_string()]
    );
    assert_eq!(
        s.if_files.get(&IfCheckType::All).unwrap(),
        &vec!["/etc/bar".to_string(), "/etc/baz".to_string()]
    );
}

// ===========================================================================
// Dot-notation: extra cases (porting the spirit of Go's table-driven tests).
// ===========================================================================

#[test]
fn dot_notation_bare_token_becomes_true_value() {
    // Mirrors Go where any token without `=` defaults to "true".
    // We can verify via the Config path: a bare token isn't a stages.* path,
    // so loading produces a default Config; that's enough to know the modifier
    // didn't crash and yielded valid YAML.
    let yaml = dot_notation_modifier(b"some.flag").unwrap();
    let s = std::str::from_utf8(&yaml).unwrap();
    assert!(s.contains("some"));
    assert!(s.contains("flag"));
    assert!(s.contains("true"));
}

#[test]
fn dot_notation_array_then_object_path() {
    let yaml =
        dot_notation_modifier(b"stages.foo[0].commands[0]=cmd stages.foo[0].name=n").unwrap();
    let cfg = Config::load(&yaml).unwrap();
    assert_eq!(cfg.stages["foo"][0].name, "n");
    assert_eq!(cfg.stages["foo"][0].commands, vec!["cmd"]);
}

#[test]
fn dot_notation_three_commands_in_one_stage() {
    let yaml = dot_notation_modifier(
        b"stages.x[0].name=t stages.x[0].commands[0]=a stages.x[0].commands[1]=b stages.x[0].commands[2]=c",
    )
    .unwrap();
    let cfg = Config::load(&yaml).unwrap();
    assert_eq!(cfg.stages["x"][0].commands, vec!["a", "b", "c"]);
}

#[test]
fn dot_notation_two_stages_in_one_token_string() {
    let yaml = dot_notation_modifier(
        b"stages.x[0].name=n0 stages.x[1].name=n1 stages.x[2].name=n2",
    )
    .unwrap();
    let cfg = Config::load(&yaml).unwrap();
    assert_eq!(cfg.stages["x"].len(), 3);
    assert_eq!(cfg.stages["x"][0].name, "n0");
    assert_eq!(cfg.stages["x"][1].name, "n1");
    assert_eq!(cfg.stages["x"][2].name, "n2");
}

#[test]
fn dot_notation_value_with_quotes_preserved_after_outer_quotes_stripped() {
    let yaml = dot_notation_modifier(br#"stages.x[0].name="hello world""#).unwrap();
    let cfg = Config::load(&yaml).unwrap();
    assert_eq!(cfg.stages["x"][0].name, "hello world");
}

#[test]
fn dot_notation_multiple_spaces_between_tokens() {
    let yaml = dot_notation_modifier(b"stages.x[0].name=a    stages.x[0].commands[0]=b").unwrap();
    let cfg = Config::load(&yaml).unwrap();
    assert_eq!(cfg.stages["x"][0].name, "a");
    assert_eq!(cfg.stages["x"][0].commands[0], "b");
}

// ===========================================================================
// Multi-stage YAML fixtures from Go default_test.go.
// ===========================================================================

/// Port of Go: "01_first.yaml" — a single command stage.
#[test]
fn parses_single_command_stage_yaml() {
    let y = indoc! {r#"
        stages:
          test:
            - commands:
                - sed -i 's/boo/bar/g' /tmp/test/bar
    "#};
    let cfg = Config::load(y.as_bytes()).unwrap();
    assert_eq!(cfg.stages["test"].len(), 1);
    assert_eq!(cfg.stages["test"][0].commands.len(), 1);
    assert!(cfg.stages["test"][0].commands[0].contains("sed -i"));
}

/// Port of Go: "after" deps in YAML.
#[test]
fn parses_after_dependency_stage() {
    let y = indoc! {r#"
        stages:
          test:
            - after:
                - name: "test.test"
              commands:
                - echo hi
    "#};
    let cfg = Config::load(y.as_bytes()).unwrap();
    let s = &cfg.stages["test"][0];
    assert_eq!(s.after.len(), 1);
    assert_eq!(s.after[0].name, "test.test");
}

/// Port of Go: 4 user stages in sequence.
#[test]
fn parses_repeated_user_stages() {
    let y = indoc! {r#"
        stages:
          initramfs:
            - users:
                kairos:
                  groups:
                    - sudo
                  passwd: kairos
            - users:
                kairos:
                  groups:
                    - sudo
                  passwd: kairos
            - users:
                kairos:
                  groups:
                    - sudo
                  passwd: kairos
    "#};
    let cfg = Config::load(y.as_bytes()).unwrap();
    assert_eq!(cfg.stages["initramfs"].len(), 3);
    for s in &cfg.stages["initramfs"] {
        assert_eq!(s.users["kairos"].password_hash, "kairos");
        assert_eq!(s.users["kairos"].groups, vec!["sudo"]);
    }
}

/// Port of Go: rootfs / rootfs.before / rootfs.after combo.
#[test]
fn parses_rootfs_substage_combo() {
    let y = indoc! {r#"
        name: "Rootfs Layout Settings"
        stages:
          rootfs.before:
            - name: "before rootfs"
              commands:
                - echo before
          rootfs:
            - name: "rootfs"
              commands:
                - echo main
            - name: "rootfs 2"
              commands:
                - echo "2"
          initramfs:
            - name: "initramfs"
              commands:
                - echo initramfs
    "#};
    let cfg = Config::load(y.as_bytes()).unwrap();
    assert_eq!(cfg.name, "Rootfs Layout Settings");
    assert_eq!(cfg.stages["rootfs.before"][0].name, "before rootfs");
    assert_eq!(cfg.stages["rootfs"].len(), 2);
    assert_eq!(cfg.stages["rootfs"][0].name, "rootfs");
    assert_eq!(cfg.stages["rootfs"][1].name, "rootfs 2");
    assert_eq!(cfg.stages["initramfs"][0].name, "initramfs");
}

/// Port of Go: conditional via top-level `if` field.
#[test]
fn parses_if_conditional_string() {
    let y = indoc! {r#"
        stages:
          test:
            - if: "cat /tmp/test/bar | grep bar"
              commands:
                - echo "baz" > /tmp/test/baz
    "#};
    let cfg = Config::load(y.as_bytes()).unwrap();
    let s = &cfg.stages["test"][0];
    assert!(s.r#if.contains("grep bar"));
    assert_eq!(s.commands.len(), 1);
}

/// Port of Go: "Get Users" — ensure_entities populated.
#[test]
fn parses_ensure_entities() {
    let y = indoc! {r#"
        stages:
          foo:
            - ensure_entities:
                - path: /tmp/foo
                  entity: |
                    kind: "group"
                    group_name: "foo"
                    password: "xx"
                    gid: 1
                    users: "one,two,tree"
    "#};
    let cfg = Config::load(y.as_bytes()).unwrap();
    let s = &cfg.stages["foo"][0];
    assert_eq!(s.ensure_entities.len(), 1);
    assert_eq!(s.ensure_entities[0].path, "/tmp/foo");
    assert!(s.ensure_entities[0].entity.contains("group_name"));
}

/// Port of Go: "Deletes Users" — delete_entities populated.
#[test]
fn parses_delete_entities() {
    let y = indoc! {r#"
        stages:
          foo:
            - delete_entities:
                - path: /tmp/foo
                  entity: |
                    kind: "group"
                    group_name: "foo"
    "#};
    let cfg = Config::load(y.as_bytes()).unwrap();
    let s = &cfg.stages["foo"][0];
    assert_eq!(s.delete_entities.len(), 1);
    assert_eq!(s.delete_entities[0].path, "/tmp/foo");
}

// ===========================================================================
// Round-trip tests: parse → render → parse → equal.
// ===========================================================================

#[test]
fn roundtrip_minimal_config() {
    let mut stages = HashMap::new();
    stages.insert(
        "rootfs".to_string(),
        vec![Stage {
            name: "n".into(),
            commands: vec!["true".into()],
            ..Default::default()
        }],
    );
    let c = Config {
        name: "minimal".into(),
        stages,
        ..Default::default()
    };
    let s1 = c.to_string_yaml().unwrap();
    let parsed = Config::load(s1.as_bytes()).unwrap();
    let s2 = parsed.to_string_yaml().unwrap();
    let parsed2 = Config::load(s2.as_bytes()).unwrap();
    assert_eq!(parsed, parsed2);
}

#[test]
fn roundtrip_complex_config() {
    let mut users = HashMap::new();
    users.insert(
        "alice".to_string(),
        User {
            name: "alice".into(),
            uid: "1001".into(),
            password_hash: "hash".into(),
            groups: vec!["sudo".into()],
            lock_passwd: true,
            ssh_authorized_keys: vec!["key1".into(), "key2".into()],
            ..Default::default()
        },
    );

    let mut stages = HashMap::new();
    stages.insert(
        "boot".to_string(),
        vec![Stage {
            name: "complex".into(),
            users,
            files: vec![File {
                path: "/etc/foo".into(),
                content: "x".into(),
                permissions: 0o644,
                owner: OwnerId::Numeric(1000),
                group: 1000,
                ..Default::default()
            }],
            commands: vec!["true".into()],
            ..Default::default()
        }],
    );

    let c = Config {
        name: "cfg".into(),
        stages,
        ..Default::default()
    };
    let s = c.to_string_yaml().unwrap();
    let back = Config::load(s.as_bytes()).unwrap();
    assert_eq!(back, c);
    // Second round.
    let s2 = back.to_string_yaml().unwrap();
    let back2 = Config::load(s2.as_bytes()).unwrap();
    assert_eq!(back, back2);
}

#[test]
fn roundtrip_after_dependency() {
    let mut stages = HashMap::new();
    stages.insert(
        "rootfs".to_string(),
        vec![Stage {
            name: "b".into(),
            after: vec![Dependency { name: "a".into() }],
            ..Default::default()
        }],
    );
    let c = Config {
        stages,
        ..Default::default()
    };
    let s = c.to_string_yaml().unwrap();
    let back = Config::load(s.as_bytes()).unwrap();
    assert_eq!(back, c);
}

#[test]
fn roundtrip_layout_full() {
    let mut stages = HashMap::new();
    stages.insert(
        "boot".to_string(),
        vec![Stage {
            layout: Layout {
                device: Some(Device {
                    path: "/dev/sda".into(),
                    label: "gpt".into(),
                    init_disk: true,
                    disk_name: "main".into(),
                }),
                expand: Some(ExpandPartition { size: 2048 }),
                parts: vec![Partition {
                    fs_label: "ROOT".into(),
                    size: 4096,
                    p_label: "root".into(),
                    file_system: "ext4".into(),
                    bootable: true,
                }],
            },
            ..Default::default()
        }],
    );
    let c = Config {
        stages,
        ..Default::default()
    };
    let s = c.to_string_yaml().unwrap();
    let back = Config::load(s.as_bytes()).unwrap();
    assert_eq!(back, c);
}

#[test]
fn roundtrip_packages_and_pins() {
    let mut pins: PackagePins = HashMap::new();
    pins.insert("foo".into(), "1.2.3".into());
    pins.insert("bar".into(), "4.5.6".into());

    let mut stages = HashMap::new();
    stages.insert(
        "test".to_string(),
        vec![Stage {
            packages: Packages {
                install: vec!["vim".into(), "curl".into()],
                remove: vec!["nano".into()],
                refresh: true,
                upgrade: true,
            },
            package_pins: pins,
            ..Default::default()
        }],
    );
    let c = Config {
        stages,
        ..Default::default()
    };
    let s = c.to_string_yaml().unwrap();
    let back = Config::load(s.as_bytes()).unwrap();
    assert_eq!(back, c);
}

// ===========================================================================
// File / User / Layout / Packages: ≥3 fixtures each.
// ===========================================================================

#[test]
fn file_fixture_with_string_owner() {
    let y = indoc! {r#"
        stages:
          test:
            - files:
                - path: /a
                  owner: alice
                  content: hello
    "#};
    let cfg = Config::load(y.as_bytes()).unwrap();
    let f = &cfg.stages["test"][0].files[0];
    assert_eq!(f.path, "/a");
    assert_eq!(f.owner, OwnerId::Name("alice".into()));
    assert_eq!(f.content, "hello");
}

#[test]
fn file_fixture_minimal() {
    let y = indoc! {r#"
        stages:
          test:
            - files:
                - path: /b
    "#};
    let cfg = Config::load(y.as_bytes()).unwrap();
    assert_eq!(cfg.stages["test"][0].files[0].path, "/b");
    assert_eq!(cfg.stages["test"][0].files[0].permissions, 0);
}

#[test]
fn file_fixture_full() {
    // `permissions: 384` is the decimal value of 0o600.
    let y = indoc! {r#"
        stages:
          test:
            - files:
                - path: /c
                  permissions: 384
                  owner: 0
                  group: 0
                  content: secret
                  encoding: b64
    "#};
    let cfg = Config::load(y.as_bytes()).unwrap();
    let f = &cfg.stages["test"][0].files[0];
    assert_eq!(f.path, "/c");
    assert_eq!(f.permissions, 0o600);
    assert_eq!(f.content, "secret");
    assert_eq!(f.encoding, "b64");
}

#[test]
fn file_fixture_with_directories_too() {
    let y = indoc! {r#"
        stages:
          test:
            - files:
                - path: /a
              directories:
                - path: /dir
                  permissions: 493
    "#};
    let cfg = Config::load(y.as_bytes()).unwrap();
    let s = &cfg.stages["test"][0];
    assert_eq!(s.files.len(), 1);
    assert_eq!(s.directories.len(), 1);
    assert_eq!(s.directories[0].path, "/dir");
    assert_eq!(s.directories[0].permissions, 493);
}

#[test]
fn user_fixture_minimal() {
    let y = indoc! {r#"
        stages:
          test:
            - users:
                alice:
                  passwd: x
    "#};
    let cfg = Config::load(y.as_bytes()).unwrap();
    assert_eq!(cfg.stages["test"][0].users["alice"].password_hash, "x");
}

#[test]
fn user_fixture_full() {
    let y = indoc! {r#"
        stages:
          test:
            - users:
                bob:
                  name: bob
                  passwd: hash
                  uid: "1500"
                  shell: /bin/sh
                  homedir: /home/bob
                  primary_group: bob
                  groups: [docker, wheel]
                  ssh_authorized_keys: [k1, k2, k3]
                  lock_passwd: true
                  no_create_home: false
                  no_user_group: true
                  system: false
    "#};
    let cfg = Config::load(y.as_bytes()).unwrap();
    let u = &cfg.stages["test"][0].users["bob"];
    assert_eq!(u.uid, "1500");
    assert_eq!(u.shell, "/bin/sh");
    assert_eq!(u.homedir, "/home/bob");
    assert_eq!(u.primary_group, "bob");
    assert_eq!(u.groups, vec!["docker", "wheel"]);
    assert_eq!(u.ssh_authorized_keys, vec!["k1", "k2", "k3"]);
    assert!(u.lock_passwd);
    assert!(u.no_user_group);
}

#[test]
fn user_fixture_multiple_users() {
    let y = indoc! {r#"
        stages:
          test:
            - users:
                alice:
                  passwd: a
                bob:
                  passwd: b
                charlie:
                  passwd: c
    "#};
    let cfg = Config::load(y.as_bytes()).unwrap();
    let users = &cfg.stages["test"][0].users;
    assert_eq!(users.len(), 3);
    assert!(users.contains_key("alice"));
    assert!(users.contains_key("bob"));
    assert!(users.contains_key("charlie"));
}

#[test]
fn layout_fixture_device_only() {
    let y = indoc! {r#"
        stages:
          test:
            - layout:
                device:
                  path: /dev/sda
    "#};
    let cfg = Config::load(y.as_bytes()).unwrap();
    let l = &cfg.stages["test"][0].layout;
    assert_eq!(l.device.as_ref().unwrap().path, "/dev/sda");
    assert!(l.expand.is_none());
}

#[test]
fn layout_fixture_expand_only() {
    let y = indoc! {r#"
        stages:
          test:
            - layout:
                expand_partition:
                  size: 8192
    "#};
    let cfg = Config::load(y.as_bytes()).unwrap();
    let l = &cfg.stages["test"][0].layout;
    assert!(l.device.is_none());
    assert_eq!(l.expand.as_ref().unwrap().size, 8192);
}

#[test]
fn layout_fixture_multiple_partitions() {
    let y = indoc! {r#"
        stages:
          test:
            - layout:
                add_partitions:
                  - fsLabel: A
                    size: 100
                  - fsLabel: B
                    size: 200
                  - fsLabel: C
                    size: 300
    "#};
    let cfg = Config::load(y.as_bytes()).unwrap();
    let parts = &cfg.stages["test"][0].layout.parts;
    assert_eq!(parts.len(), 3);
    assert_eq!(parts[0].fs_label, "A");
    assert_eq!(parts[1].size, 200);
    assert_eq!(parts[2].fs_label, "C");
}

#[test]
fn packages_fixture_install_only() {
    let y = indoc! {r#"
        stages:
          test:
            - packages:
                install: [vim]
    "#};
    let cfg = Config::load(y.as_bytes()).unwrap();
    assert_eq!(cfg.stages["test"][0].packages.install, vec!["vim"]);
    assert!(cfg.stages["test"][0].packages.remove.is_empty());
}

#[test]
fn packages_fixture_remove_only() {
    let y = indoc! {r#"
        stages:
          test:
            - packages:
                remove: [nano, emacs]
    "#};
    let cfg = Config::load(y.as_bytes()).unwrap();
    assert_eq!(cfg.stages["test"][0].packages.remove, vec!["nano", "emacs"]);
}

#[test]
fn packages_fixture_full() {
    let y = indoc! {r#"
        stages:
          test:
            - packages:
                install: [a, b, c]
                remove: [d]
                refresh: true
                upgrade: true
    "#};
    let cfg = Config::load(y.as_bytes()).unwrap();
    let p = &cfg.stages["test"][0].packages;
    assert_eq!(p.install, vec!["a", "b", "c"]);
    assert_eq!(p.remove, vec!["d"]);
    assert!(p.refresh);
    assert!(p.upgrade);
}

// ---------------------------------------------------------------------------
// Git / DataSource / DNS / Systemctl: smaller fixture groups.
// ---------------------------------------------------------------------------

#[test]
fn git_fixture_https_no_auth() {
    let y = indoc! {r#"
        stages:
          test:
            - git:
                url: https://example.com/repo.git
                path: /opt/repo
    "#};
    let cfg = Config::load(y.as_bytes()).unwrap();
    let g = &cfg.stages["test"][0].git;
    assert_eq!(g.url, "https://example.com/repo.git");
    assert_eq!(g.path, "/opt/repo");
    assert_eq!(g.auth, Auth::default());
}

#[test]
fn git_fixture_with_branch_only() {
    let y = indoc! {r#"
        stages:
          test:
            - git:
                url: https://example.com/x.git
                path: /opt/x
                branch: develop
                branch_only: true
    "#};
    let cfg = Config::load(y.as_bytes()).unwrap();
    let g = &cfg.stages["test"][0].git;
    assert_eq!(g.branch, "develop");
    assert!(g.branch_only);
}

#[test]
fn git_fixture_with_password_auth() {
    let y = indoc! {r#"
        stages:
          test:
            - git:
                url: https://x/y.git
                path: /tmp
                auth:
                  username: alice
                  password: hunter2
    "#};
    let cfg = Config::load(y.as_bytes()).unwrap();
    let g = &cfg.stages["test"][0].git;
    assert_eq!(g.auth.username, "alice");
    assert_eq!(g.auth.password, "hunter2");
}

#[test]
fn datasource_fixture_full() {
    let y = indoc! {r#"
        stages:
          test:
            - datasource:
                providers: [ec2, gce, azure]
                path: /var/lib/cloud
                userdata_name: user-data
    "#};
    let cfg = Config::load(y.as_bytes()).unwrap();
    let d: &DataSource = &cfg.stages["test"][0].data_sources;
    assert_eq!(d.providers, vec!["ec2", "gce", "azure"]);
    assert_eq!(d.path, "/var/lib/cloud");
    assert_eq!(d.userdata_name, "user-data");
}

#[test]
fn datasource_fixture_providers_only() {
    let y = indoc! {r#"
        stages:
          test:
            - datasource:
                providers: [aws]
    "#};
    let cfg = Config::load(y.as_bytes()).unwrap();
    assert_eq!(cfg.stages["test"][0].data_sources.providers, vec!["aws"]);
}

#[test]
fn datasource_fixture_empty_is_default() {
    let y = indoc! {r#"
        stages:
          test:
            - name: n
    "#};
    let cfg = Config::load(y.as_bytes()).unwrap();
    assert_eq!(cfg.stages["test"][0].data_sources, DataSource::default());
}

// ---------------------------------------------------------------------------
// Stage-level dependency wiring fixtures.
// ---------------------------------------------------------------------------

#[test]
fn dependency_single_name() {
    let y = indoc! {r#"
        stages:
          test:
            - name: b
              after:
                - name: a
    "#};
    let cfg = Config::load(y.as_bytes()).unwrap();
    let s = &cfg.stages["test"][0];
    assert_eq!(s.after, vec![Dependency { name: "a".into() }]);
}

#[test]
fn dependency_multiple_names() {
    let y = indoc! {r#"
        stages:
          test:
            - name: d
              after:
                - name: a
                - name: b
                - name: c
    "#};
    let cfg = Config::load(y.as_bytes()).unwrap();
    let s = &cfg.stages["test"][0];
    assert_eq!(s.after.len(), 3);
    assert_eq!(s.after[0].name, "a");
    assert_eq!(s.after[2].name, "c");
}

// ---------------------------------------------------------------------------
// YipEntity fixtures.
// ---------------------------------------------------------------------------

#[test]
fn yip_entity_inline_struct() {
    let e = YipEntity {
        path: "/etc/passwd".into(),
        entity: "ENTITY=user".into(),
    };
    let s = serde_yaml::to_string(&e).unwrap();
    let back: YipEntity = serde_yaml::from_str(&s).unwrap();
    assert_eq!(back, e);
}

// ---------------------------------------------------------------------------
// Edge cases: blank lines, comments, empty strings.
// ---------------------------------------------------------------------------

#[test]
fn yaml_with_comments_parses() {
    let y = indoc! {r#"
        # top-level comment
        name: cfg # trailing
        stages:
          # within stages
          test:
            - name: x   # within stage
              commands:
                # before commands
                - echo hi
    "#};
    let cfg = Config::load(y.as_bytes()).unwrap();
    assert_eq!(cfg.name, "cfg");
    assert_eq!(cfg.stages["test"][0].name, "x");
}

#[test]
fn yaml_with_blank_lines_parses() {
    let y = "
name: spaced

stages:

  test:

    - name: hello

      commands:

        - echo hi
";
    let cfg = Config::load(y.as_bytes()).unwrap();
    assert_eq!(cfg.name, "spaced");
    assert_eq!(cfg.stages["test"][0].name, "hello");
}

#[test]
fn empty_yaml_object_is_default() {
    let cfg = Config::load(b"{}").unwrap();
    assert_eq!(cfg, Config::default());
}

#[test]
fn yaml_only_name_keeps_stages_empty() {
    let cfg = Config::load(b"name: only-a-name").unwrap();
    assert_eq!(cfg.name, "only-a-name");
    assert!(cfg.stages.is_empty());
}

#[test]
fn yaml_string_with_backslashes_in_command_preserved() {
    let y = indoc! {r#"
        stages:
          test:
            - commands:
                - "echo \\n hello"
    "#};
    let cfg = Config::load(y.as_bytes()).unwrap();
    let cmd = &cfg.stages["test"][0].commands[0];
    assert!(cmd.contains("hello"));
}

#[test]
fn yaml_very_long_stage_name_parses() {
    let long_name: String = "x".repeat(1000);
    let y = format!(
        "stages:\n  test:\n    - name: \"{long_name}\"\n      commands: [echo hi]\n"
    );
    let cfg = Config::load(y.as_bytes()).unwrap();
    assert_eq!(cfg.stages["test"][0].name.len(), 1000);
}

#[test]
fn yaml_explicitly_empty_string_for_name() {
    let y = indoc! {r#"
        stages:
          test:
            - name: ""
              commands: [echo hi]
    "#};
    let cfg = Config::load(y.as_bytes()).unwrap();
    assert_eq!(cfg.stages["test"][0].name, "");
    assert_eq!(cfg.stages["test"][0].commands, vec!["echo hi"]);
}

#[test]
fn yaml_with_value_containing_quotes_in_content() {
    let y = indoc! {r#"
        stages:
          test:
            - files:
                - path: /tmp/x
                  content: 'this has "double" and ''single'' quotes'
    "#};
    let cfg = Config::load(y.as_bytes()).unwrap();
    let c = &cfg.stages["test"][0].files[0].content;
    assert!(c.contains("\"double\""));
    assert!(c.contains("'single'"));
}

// ---------------------------------------------------------------------------
// Conditionals: only_os / only_arch / only_os_version / if_files combinations.
// ---------------------------------------------------------------------------

#[test]
fn conditional_only_if_only_os() {
    let y = indoc! {r#"
        stages:
          test:
            - only_os: linux
    "#};
    let cfg = Config::load(y.as_bytes()).unwrap();
    assert_eq!(cfg.stages["test"][0].only_if_os, "linux");
}

#[test]
fn conditional_only_arch_amd64() {
    let y = indoc! {r#"
        stages:
          test:
            - only_arch: amd64
    "#};
    let cfg = Config::load(y.as_bytes()).unwrap();
    assert_eq!(cfg.stages["test"][0].only_if_arch, "amd64");
}

#[test]
fn conditional_if_files_none_check() {
    let y = indoc! {r#"
        stages:
          test:
            - if_files:
                none: [/etc/forbidden]
    "#};
    let cfg = Config::load(y.as_bytes()).unwrap();
    let s = &cfg.stages["test"][0];
    assert_eq!(
        s.if_files.get(&IfCheckType::None).unwrap(),
        &vec!["/etc/forbidden".to_string()]
    );
}

#[test]
fn conditional_if_files_all_three_kinds() {
    let y = indoc! {r#"
        stages:
          test:
            - if_files:
                any: [a]
                all: [b, c]
                none: [d]
    "#};
    let cfg = Config::load(y.as_bytes()).unwrap();
    let s = &cfg.stages["test"][0];
    assert_eq!(s.if_files.len(), 3);
    assert_eq!(s.if_files.get(&IfCheckType::Any).unwrap(), &vec!["a".to_string()]);
    assert_eq!(
        s.if_files.get(&IfCheckType::All).unwrap(),
        &vec!["b".to_string(), "c".to_string()]
    );
    assert_eq!(s.if_files.get(&IfCheckType::None).unwrap(), &vec!["d".to_string()]);
}

#[test]
fn conditional_service_manager() {
    let y = indoc! {r#"
        stages:
          test:
            - only_service_manager: openrc
    "#};
    let cfg = Config::load(y.as_bytes()).unwrap();
    assert_eq!(cfg.stages["test"][0].only_if_service_manager, "openrc");
}

// ---------------------------------------------------------------------------
// Stage field coverage: sysctl / hostname / dns / modules / environment.
// ---------------------------------------------------------------------------

#[test]
fn stage_sysctl_parses() {
    let y = indoc! {r#"
        stages:
          test:
            - sysctl:
                net.ipv4.ip_forward: "1"
                vm.swappiness: "10"
    "#};
    let cfg = Config::load(y.as_bytes()).unwrap();
    let s = &cfg.stages["test"][0];
    assert_eq!(s.sysctl.get("net.ipv4.ip_forward").unwrap(), "1");
    assert_eq!(s.sysctl.get("vm.swappiness").unwrap(), "10");
}

#[test]
fn stage_hostname_parses() {
    let y = indoc! {r#"
        stages:
          test:
            - hostname: my-host
    "#};
    let cfg = Config::load(y.as_bytes()).unwrap();
    assert_eq!(cfg.stages["test"][0].hostname, "my-host");
}

#[test]
fn stage_modules_parses() {
    let y = indoc! {r#"
        stages:
          test:
            - modules: [nvme, ext4, btrfs]
    "#};
    let cfg = Config::load(y.as_bytes()).unwrap();
    assert_eq!(cfg.stages["test"][0].modules, vec!["nvme", "ext4", "btrfs"]);
}

#[test]
fn stage_environment_parses() {
    let y = indoc! {r#"
        stages:
          test:
            - environment:
                FOO: bar
                BAZ: qux
              environment_file: /etc/environment
    "#};
    let cfg = Config::load(y.as_bytes()).unwrap();
    let s = &cfg.stages["test"][0];
    assert_eq!(s.environment.get("FOO").unwrap(), "bar");
    assert_eq!(s.environment.get("BAZ").unwrap(), "qux");
    assert_eq!(s.environment_file, "/etc/environment");
}

#[test]
fn stage_node_filter_parses() {
    let y = indoc! {r#"
        stages:
          test:
            - node: specific-host
    "#};
    let cfg = Config::load(y.as_bytes()).unwrap();
    assert_eq!(cfg.stages["test"][0].node, "specific-host");
}

#[test]
fn stage_authorized_keys_global() {
    let y = indoc! {r#"
        stages:
          test:
            - authorized_keys:
                root: [k1, k2]
                alice: [k3]
    "#};
    let cfg = Config::load(y.as_bytes()).unwrap();
    let s = &cfg.stages["test"][0];
    assert_eq!(s.ssh_keys.get("root").unwrap(), &vec!["k1", "k2"]);
    assert_eq!(s.ssh_keys.get("alice").unwrap(), &vec!["k3"]);
}

#[test]
fn stage_systemd_firstboot_parses() {
    let y = indoc! {r#"
        stages:
          test:
            - systemd_firstboot:
                keymap: us
                timezone: UTC
    "#};
    let cfg = Config::load(y.as_bytes()).unwrap();
    let m = &cfg.stages["test"][0].systemd_firstboot;
    assert_eq!(m.get("keymap").unwrap(), "us");
    assert_eq!(m.get("timezone").unwrap(), "UTC");
}

#[test]
fn stage_timesyncd_parses() {
    let y = indoc! {r#"
        stages:
          test:
            - timesyncd:
                NTP: pool.ntp.org
    "#};
    let cfg = Config::load(y.as_bytes()).unwrap();
    assert_eq!(
        cfg.stages["test"][0].timesyncd.get("NTP").unwrap(),
        "pool.ntp.org"
    );
}

#[test]
fn stage_downloads_parses() {
    let y = indoc! {r#"
        stages:
          test:
            - downloads:
                - path: /tmp/x
                  url: https://example.com/x
                  permissions: 420
                  timeout: 60
    "#};
    let cfg = Config::load(y.as_bytes()).unwrap();
    let d = &cfg.stages["test"][0].downloads[0];
    assert_eq!(d.path, "/tmp/x");
    assert_eq!(d.url, "https://example.com/x");
    assert_eq!(d.timeout, 60);
}

#[test]
fn stage_unpack_images_parses() {
    let y = indoc! {r#"
        stages:
          test:
            - unpack_images:
                - source: x
                  target: y
                - source: a
                  target: b
    "#};
    let cfg = Config::load(y.as_bytes()).unwrap();
    let imgs = &cfg.stages["test"][0].unpack_images;
    assert_eq!(imgs.len(), 2);
    assert_eq!(imgs[0].source, "x");
    assert_eq!(imgs[1].target, "b");
}
