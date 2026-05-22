//! End-to-end black-box tests against real-world Kairos yip configs.
//!
//! These fixtures are verbatim copies of cloud-config / yip YAML files that
//! ship in production Kairos:
//!
//! - `tests/fixtures/kairos/00_rootfs.yaml`,
//!   `01_extra_binds.yaml`, `08_efi_assessment.yaml`, `08_ssh.yaml`,
//!   `25_autologin.yaml`, `26_selinux.yaml`, `26_vm.yaml`, `30_ulimit.yaml`,
//!   `31_hosts.yaml` come from `kairos-init/pkg/bundled/cloudconfigs/`
//!   (the canonical bundled OEM configs that get baked into every Kairos
//!   image).
//! - `kairos_install_config.yaml`, `kairos_zfs.yaml` come from
//!   `kairos/tests/assets/` (test installer fixtures: `#cloud-config` /
//!   `#node-config` blobs with `install:` / `k3s:` extras and templated
//!   `{{ trunc 4 .Random }}` hostnames).
//!
//! For each fixture the test pipeline is the same:
//!
//! 1. Load via `Config::load_file` — asserts the YAML parses without error.
//! 2. Assert the expected top-level stages are present.
//! 3. Run `DefaultExecutor::new().analyze(stage, &cfg)` and assert the
//!    resulting op-name list is non-empty for at least one stage the file
//!    actually declares.
//!
//! Coverage rationale: we deliberately pick configs that exercise distinct
//! corners of the schema — `only_os` regexes, `environment_file`, layout
//! expansion, multi-line `if`, `systemctl` overrides, Go-template stage
//! strings, the cloud-init `#cloud-config` + extra top-level keys path,
//! and the `modules:` field — to maximise the surface area a regression
//! has to dodge.

use std::path::PathBuf;

use yip::executor::{DefaultExecutor, Executor};
use yip::schema::Config;

/// Resolve `tests/fixtures/kairos/<name>` against `CARGO_MANIFEST_DIR`.
///
/// Using `CARGO_MANIFEST_DIR` keeps these tests robust whether `cargo test`
/// is invoked from the workspace root or the crate dir.
fn fixture(name: &str) -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.push("tests/fixtures/kairos");
    p.push(name);
    p
}

/// Convenience: load a fixture and assert it parsed.
fn load(name: &str) -> Config {
    let p = fixture(name);
    Config::load_file(&p).unwrap_or_else(|e| panic!("failed to load {}: {e}", p.display()))
}

/// Convenience: assert `analyze(stage, &cfg)` returns at least one op name.
/// Returns the op-name list so individual tests can do extra checks.
fn analyze_nonempty(cfg: &Config, stage: &str) -> Vec<String> {
    let exec = DefaultExecutor::new();
    let names = exec.analyze(stage, cfg);
    assert!(
        !names.is_empty(),
        "analyze({stage}) yielded no op names for cfg name={:?}; stages keys={:?}",
        cfg.name,
        cfg.stages.keys().collect::<Vec<_>>()
    );
    names
}

// ---------------------------------------------------------------------------
// 00_rootfs.yaml — the Kairos rootfs config that drives immucore.
// ---------------------------------------------------------------------------

#[test]
fn kairos_00_rootfs_parses_and_analyzes() {
    let cfg = load("00_rootfs.yaml");
    assert_eq!(cfg.name, "Rootfs Layout Settings");

    // All expected top-level stage keys.
    for key in [
        "rootfs",
        "rootfs.after",
        "initramfs",
        "fs",
        "fs.after",
        "boot.before",
    ] {
        assert!(
            cfg.stages.contains_key(key),
            "expected stage {key} in 00_rootfs.yaml, got {:?}",
            cfg.stages.keys().collect::<Vec<_>>()
        );
    }

    // The interesting payload lives under `rootfs` itself.
    assert_eq!(cfg.stages["rootfs"].len(), 4);

    // Sanity on the environment_file / environment fields the rootfs
    // step uses to populate cos-layout.env.
    let first = &cfg.stages["rootfs"][0];
    assert_eq!(first.environment_file, "/run/cos/cos-layout.env");
    assert!(first.environment.contains_key("VOLUMES"));
    assert!(first.environment.contains_key("OVERLAY"));

    // analyze("rootfs") collapses .before + main + .after.
    let names = analyze_nonempty(&cfg, "rootfs");
    // op-name prefix must be the config `name:`.
    assert!(
        names.iter().all(|n| n.starts_with("Rootfs Layout Settings.")),
        "got {names:?}"
    );
}

// ---------------------------------------------------------------------------
// 01_extra_binds.yaml — distro-specific binds via `only_os` regex.
// ---------------------------------------------------------------------------

#[test]
fn kairos_01_extra_binds_parses_and_analyzes() {
    let cfg = load("01_extra_binds.yaml");
    assert_eq!(cfg.name, "Extra binds for specific distros");
    assert!(cfg.stages.contains_key("rootfs"));
    assert_eq!(cfg.stages["rootfs"].len(), 2);

    // Both stages must carry an only_os regex.
    for st in &cfg.stages["rootfs"] {
        assert!(!st.only_if_os.is_empty(), "only_os missing on {:?}", st.name);
    }

    analyze_nonempty(&cfg, "rootfs");
}

// ---------------------------------------------------------------------------
// 08_efi_assessment.yaml — systemctl overrides + enable list + multiline content.
// ---------------------------------------------------------------------------

#[test]
fn kairos_08_efi_assessment_parses_and_analyzes() {
    let cfg = load("08_efi_assessment.yaml");
    assert_eq!(cfg.name, "Enable EFI assessment");
    assert!(cfg.stages.contains_key("initramfs"));
    assert_eq!(cfg.stages["initramfs"].len(), 2);

    // First stage writes two override files.
    let first = &cfg.stages["initramfs"][0];
    assert_eq!(first.files.len(), 2);
    assert!(first.files[0].content.contains("ExecStartPre=mount"));

    // Second stage uses systemctl.enable.
    let second = &cfg.stages["initramfs"][1];
    assert_eq!(second.systemctl.enable, vec!["systemd-bless-boot"]);

    analyze_nonempty(&cfg, "initramfs");
}

// ---------------------------------------------------------------------------
// 08_ssh.yaml — the smallest possible "real" config.
// ---------------------------------------------------------------------------

#[test]
fn kairos_08_ssh_parses_and_analyzes() {
    let cfg = load("08_ssh.yaml");
    assert_eq!(cfg.name, "Default config");
    assert!(cfg.stages.contains_key("initramfs"));
    assert_eq!(cfg.stages["initramfs"].len(), 1);
    let st = &cfg.stages["initramfs"][0];
    assert_eq!(st.name, "Generate host keys");
    assert_eq!(st.commands, vec!["ssh-keygen -A"]);

    analyze_nonempty(&cfg, "initramfs");
}

// ---------------------------------------------------------------------------
// 25_autologin.yaml — multi-line files + multi-line `if` + getty overrides.
// ---------------------------------------------------------------------------

#[test]
fn kairos_25_autologin_parses_and_analyzes() {
    let cfg = load("25_autologin.yaml");
    assert_eq!(cfg.name, "Root autologin");
    assert!(cfg.stages.contains_key("initramfs"));
    assert_eq!(cfg.stages["initramfs"].len(), 2);

    // First stage has a multi-line `if` joined with `\` continuations.
    let first = &cfg.stages["initramfs"][0];
    assert!(first.r#if.contains("interactive-install"));
    assert!(first.r#if.contains("live_mode"));
    // Two getty override files.
    assert_eq!(first.files.len(), 2);
    assert!(first.files[0].path.starts_with("/etc/systemd/system/"));

    analyze_nonempty(&cfg, "initramfs");
}

// ---------------------------------------------------------------------------
// 26_selinux.yaml — single tiny stage with a multi-line `if`.
// ---------------------------------------------------------------------------

#[test]
fn kairos_26_selinux_parses_and_analyzes() {
    let cfg = load("26_selinux.yaml");
    assert_eq!(cfg.name, "SELinux");
    assert!(cfg.stages.contains_key("initramfs"));
    assert_eq!(cfg.stages["initramfs"].len(), 1);
    let st = &cfg.stages["initramfs"][0];
    assert_eq!(st.name, "Relabelling");
    assert!(st.r#if.contains("selinux=1"));
    assert_eq!(st.commands.len(), 1);

    analyze_nonempty(&cfg, "initramfs");
}

// ---------------------------------------------------------------------------
// 26_vm.yaml — four boot stages gated on dmi product_name + only_service_manager.
// ---------------------------------------------------------------------------

#[test]
fn kairos_26_vm_parses_and_analyzes() {
    let cfg = load("26_vm.yaml");
    assert_eq!(cfg.name, "Enable QEMU tools");
    assert!(cfg.stages.contains_key("boot"));
    assert_eq!(cfg.stages["boot"].len(), 4);

    // Verify the service-manager gates are populated and balanced
    // (two openrc + two systemd).
    let sm_counts = cfg.stages["boot"]
        .iter()
        .fold((0usize, 0usize), |(o, s), st| match st.only_if_service_manager.as_str() {
            "openrc" => (o + 1, s),
            "systemd" => (o, s + 1),
            _ => (o, s),
        });
    assert_eq!(sm_counts, (2, 2), "expected 2 openrc + 2 systemd stages");

    analyze_nonempty(&cfg, "boot");
}

// ---------------------------------------------------------------------------
// 30_ulimit.yaml — minimal `boot.before` config (one openrc command).
// ---------------------------------------------------------------------------

#[test]
fn kairos_30_ulimit_parses_and_analyzes() {
    let cfg = load("30_ulimit.yaml");
    // This config has no top-level `name:`.
    assert!(cfg.name.is_empty());
    assert!(cfg.stages.contains_key("boot.before"));
    assert_eq!(cfg.stages["boot.before"].len(), 1);

    let st = &cfg.stages["boot.before"][0];
    assert_eq!(st.only_if_service_manager, "openrc");
    assert!(st.commands[0].contains("rc_ulimit"));

    // Analyse the `boot` parent — yip's analyze expands to
    // {boot.before, boot, boot.after} and the only populated key here is
    // boot.before, so we expect at least one op name.
    analyze_nonempty(&cfg, "boot");
}

// ---------------------------------------------------------------------------
// 31_hosts.yaml — directories + files + commands + templated hostname.
// ---------------------------------------------------------------------------

#[test]
fn kairos_31_hosts_parses_and_analyzes() {
    // No `name:` field on this one.
    let cfg = load("31_hosts.yaml");
    assert!(cfg.name.is_empty());
    assert!(cfg.stages.contains_key("initramfs"));
    assert_eq!(cfg.stages["initramfs"].len(), 3);

    // The third stage carries a Go-template hostname. It must round-trip
    // as a literal string (we don't render templates at parse time).
    let third = &cfg.stages["initramfs"][2];
    assert!(
        third.hostname.contains("{{ trunc 4 .MachineID }}"),
        "expected templated hostname, got {:?}",
        third.hostname
    );

    // Directories + files + commands all populated on the first stage.
    let first = &cfg.stages["initramfs"][0];
    assert!(!first.directories.is_empty());
    assert!(!first.files.is_empty());
    assert!(!first.commands.is_empty());

    analyze_nonempty(&cfg, "initramfs");
}

// ---------------------------------------------------------------------------
// kairos_install_config.yaml — #cloud-config with extra top-level `install:`.
// ---------------------------------------------------------------------------

#[test]
fn kairos_install_config_parses_and_analyzes() {
    // This is a real Kairos installer config with a `#cloud-config` shebang
    // and an extra `install:` block that yip ignores (extra top-level keys
    // are not part of the yip schema but are accepted silently).
    let cfg = load("kairos_install_config.yaml");
    assert!(cfg.name.is_empty());
    assert!(cfg.stages.contains_key("initramfs"));
    assert_eq!(cfg.stages["initramfs"].len(), 2);

    // User block: kairos / passwd / admin group.
    let first = &cfg.stages["initramfs"][0];
    assert!(first.users.contains_key("kairos"));
    let kairos = &first.users["kairos"];
    assert_eq!(kairos.password_hash, "kairos");
    assert_eq!(kairos.groups, vec!["admin"]);

    // Templated hostname survives as literal text.
    let second = &cfg.stages["initramfs"][1];
    assert!(second.hostname.contains("{{ trunc 4 .Random }}"));

    analyze_nonempty(&cfg, "initramfs");
}

// ---------------------------------------------------------------------------
// kairos_zfs.yaml — modules field + cross-stage config.
// ---------------------------------------------------------------------------

#[test]
fn kairos_zfs_parses_and_analyzes() {
    let cfg = load("kairos_zfs.yaml");
    assert!(cfg.name.is_empty());
    assert!(cfg.stages.contains_key("initramfs"));
    assert!(cfg.stages.contains_key("rootfs"));

    // rootfs stage loads the zfs kernel module.
    let rootfs0 = &cfg.stages["rootfs"][0];
    assert_eq!(rootfs0.modules, vec!["zfs"]);

    // initramfs stage has both users and hostname on the same Stage.
    let init0 = &cfg.stages["initramfs"][0];
    assert!(init0.users.contains_key("kairos"));
    assert!(init0.hostname.contains("{{ trunc 4 .Random }}"));

    // Both stages should produce op names.
    analyze_nonempty(&cfg, "initramfs");
    analyze_nonempty(&cfg, "rootfs");
}

// ---------------------------------------------------------------------------
// Cross-fixture sanity: every fixture in tests/fixtures/kairos/ must parse.
//
// This catches the case where someone drops a new fixture into the dir and
// forgets to add an explicit per-file test — at minimum we still verify the
// parse step works.
// ---------------------------------------------------------------------------

#[test]
fn all_kairos_fixtures_parse_without_error() {
    let dir = {
        let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        p.push("tests/fixtures/kairos");
        p
    };
    let mut count = 0usize;
    for entry in std::fs::read_dir(&dir).unwrap() {
        let entry = entry.unwrap();
        let path = entry.path();
        let ext = path.extension().and_then(|s| s.to_str()).unwrap_or("");
        if ext != "yaml" && ext != "yml" {
            continue;
        }
        Config::load_file(&path)
            .unwrap_or_else(|e| panic!("failed to load {}: {e}", path.display()));
        count += 1;
    }
    // At least the explicit set we ported.
    assert!(count >= 10, "expected ≥10 fixtures, found {count}");
}
