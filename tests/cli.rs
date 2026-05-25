//! Black-box integration tests for the `yip` binary.
//!
//! These tests shell out to the binary produced by `cargo build` (via
//! `assert_cmd::Command::cargo_bin`) and assert on its exit code / stdout /
//! stderr. They intentionally avoid linking to the library so the public
//! CLI surface is exercised the same way real users invoke it.
//!
//! Tests that would require network access or root privileges are marked
//! `#[ignore]` so they are skipped by default and can be opted into with
//! `cargo test -- --ignored`.

use std::io::Write;

use assert_cmd::Command;
use predicates::prelude::*;

/// Tiny inline yip config that only runs `true` — succeeds on any system,
/// touches no files, needs no privileges.
const MINIMAL_YAML: &str = "name: test
stages:
  rootfs:
    - name: smoke
      commands:
        - \"true\"
";

/// Write `contents` to a fresh temp file and return the handle. The caller
/// must keep the handle alive for the duration of the test so the file is
/// not deleted before `yip` reads it.
fn write_temp_yaml(contents: &str) -> tempfile::NamedTempFile {
    let mut f = tempfile::Builder::new()
        .prefix("yip-cli-test-")
        .suffix(".yaml")
        .tempfile()
        .expect("create temp file");
    f.write_all(contents.as_bytes()).expect("write yaml");
    f.flush().expect("flush yaml");
    f
}

#[test]
fn version_subcommand_prints_a_version() {
    Command::cargo_bin("yip")
        .expect("yip binary")
        .arg("version")
        .assert()
        .success()
        .stdout(predicate::str::contains("yip"));
}

#[test]
fn version_flag_prints_version() {
    // clap's auto-generated `--version` output is `<name> <version>`.
    let re = predicate::str::is_match(r"yip\s+\S+").expect("valid regex");
    Command::cargo_bin("yip")
        .expect("yip binary")
        .arg("--version")
        .assert()
        .success()
        .stdout(re);
}

#[test]
fn help_prints_usage() {
    Command::cargo_bin("yip")
        .expect("yip binary")
        .arg("--help")
        .assert()
        .success()
        .stdout(
            predicate::str::contains("Cloud-init")
                .or(predicate::str::contains("stage"))
                .or(predicate::str::contains("Usage")),
        );
}

#[test]
fn no_args_errors() {
    // With no stage and no paths, the CLI cannot do anything useful and
    // should exit non-zero with a usage hint on stderr.
    Command::cargo_bin("yip")
        .expect("yip binary")
        .assert()
        .failure()
        .stderr(
            predicate::str::contains("Usage")
                .or(predicate::str::contains("usage"))
                .or(predicate::str::contains("required")),
        );
}

#[test]
fn apply_minimal_config_via_inline_yaml() {
    let f = write_temp_yaml(MINIMAL_YAML);
    Command::cargo_bin("yip")
        .expect("yip binary")
        .arg("--stage")
        .arg("rootfs")
        .arg(f.path())
        .assert()
        .success();
}

#[test]
fn apply_fixture_smoke_yaml() {
    // Realistic fixture lives under tests/fixtures/. CARGO_MANIFEST_DIR is
    // set by cargo for integration tests so this resolves to the repo root.
    let manifest = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
    let fixture = std::path::Path::new(&manifest).join("tests/fixtures/smoke.yaml");
    assert!(fixture.exists(), "fixture missing at {}", fixture.display());

    // Note: the fixture writes to /tmp/yip-rs-smoke. We do not assert on the
    // file existing because some sandboxes block writes to /tmp; we only
    // require yip itself to exit successfully.
    Command::cargo_bin("yip")
        .expect("yip binary")
        .arg("--stage")
        .arg("rootfs")
        .arg(&fixture)
        .assert()
        .success();
}

#[test]
fn analyze_subcommand_lists_ops() {
    let f = write_temp_yaml(MINIMAL_YAML);
    Command::cargo_bin("yip")
        .expect("yip binary")
        .arg("analyze")
        .arg("--stage")
        .arg("rootfs")
        .arg(f.path())
        .assert()
        .success()
        .stdout(
            predicate::str::contains("test.smoke")
                .or(predicate::str::contains("smoke"))
                .or(predicate::str::contains("test")),
        );
}
