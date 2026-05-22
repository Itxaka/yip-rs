//! End-to-end YAML parsing tests for `crate::schema`.
//!
//! These mirror selected fixtures from yip's Go test suite
//! (`pkg/schema/schema_test.go` and `pkg/executor/default_test.go`) so we can
//! demonstrate parity with the original implementation.

use indoc::indoc;
use std::collections::HashMap;

use yip::schema::{
    dot_notation_modifier, Config, Directory, File, IfCheckType, OwnerId, Stage,
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
