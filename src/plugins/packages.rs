//! `packages` plugin — port of `pkg/plugins/packages.go`.
//!
//! Runs the host's package manager to refresh/upgrade/install/remove
//! packages. The package manager is detected from `/etc/os-release` (read
//! through the [`Vfs`]). Each action shells out via [`Console::run`].
//!
//! Order of operations matches Go: refresh -> upgrade -> install -> remove.
//! Errors from individual actions do not abort the chain; they accumulate
//! into [`Error::Multi`] so the executor's multierror semantics line up
//! with the rest of the plugin set.
//!
//! Detection table (per the Rust-side spec — narrower than the Go original):
//!
//!   - `ID=ubuntu`, `ID=debian`, or `ID_LIKE` contains `debian` -> apt
//!   - `ID=fedora|rhel|centos|rocky|alma|oracle`, or `ID_LIKE` contains
//!     `rhel` -> dnf
//!   - `ID=alpine` -> apk
//!   - `ID=opensuse-*` / `ID=sles` -> zypper
//!
//! Anything else returns [`Error::Other`] with `"no supported package
//! manager detected"`.

use std::path::Path;
use std::sync::Arc;

use tracing::{debug, info, warn};

use crate::console::Console;
use crate::error::{Error, Result};
use crate::executor::Plugin;
use crate::schema::Stage;
use crate::vfs::Vfs;

/// Supported package managers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PackageManager {
    Apt,
    Dnf,
    Apk,
    Zypper,
}

impl PackageManager {
    fn refresh_cmd(self) -> &'static str {
        match self {
            PackageManager::Apt => "apt-get update",
            PackageManager::Dnf => "dnf check-update",
            PackageManager::Apk => "apk update",
            PackageManager::Zypper => "zypper refresh",
        }
    }

    fn upgrade_cmd(self) -> &'static str {
        match self {
            PackageManager::Apt => "apt-get -y upgrade",
            PackageManager::Dnf => "dnf -y upgrade",
            PackageManager::Apk => "apk upgrade",
            PackageManager::Zypper => "zypper -n update",
        }
    }

    fn install_prefix(self) -> &'static str {
        match self {
            PackageManager::Apt => "apt-get -y install",
            PackageManager::Dnf => "dnf -y install",
            PackageManager::Apk => "apk add",
            PackageManager::Zypper => "zypper -n install",
        }
    }

    fn remove_prefix(self) -> &'static str {
        match self {
            PackageManager::Apt => "apt-get -y remove",
            PackageManager::Dnf => "dnf -y remove",
            PackageManager::Apk => "apk del",
            PackageManager::Zypper => "zypper -n remove",
        }
    }
}

/// Build the plugin closure for registration with the executor.
pub fn build() -> Plugin {
    Arc::new(run)
}

/// Apply the packages plugin against `stage`.
pub fn run(stage: &Stage, fs: &dyn Vfs, console: &dyn Console) -> Result<()> {
    let pkgs = &stage.packages;

    // No-op gate matches Go: nothing to do if every action is empty/false.
    if pkgs.install.is_empty()
        && pkgs.remove.is_empty()
        && !pkgs.refresh
        && !pkgs.upgrade
    {
        debug!("packages: no actions requested, skipping");
        return Ok(());
    }

    let pm = detect_package_manager(fs)?;
    info!(?pm, "packages: detected package manager");

    let mut errs: Vec<Error> = Vec::new();

    if pkgs.refresh {
        run_simple(console, pm.refresh_cmd(), "refresh", &mut errs);
    }
    if pkgs.upgrade {
        run_simple(console, pm.upgrade_cmd(), "upgrade", &mut errs);
    }
    if !pkgs.install.is_empty() {
        let cmd = format!("{} {}", pm.install_prefix(), pkgs.install.join(" "));
        run_simple(console, &cmd, "install", &mut errs);
    }
    if !pkgs.remove.is_empty() {
        let cmd = format!("{} {}", pm.remove_prefix(), pkgs.remove.join(" "));
        run_simple(console, &cmd, "remove", &mut errs);
    }

    if errs.is_empty() {
        Ok(())
    } else {
        Err(Error::Multi(errs))
    }
}

/// Run a single shell command, log its output (or empty marker), and
/// push any error onto `errs` rather than bubbling.
fn run_simple(console: &dyn Console, cmd: &str, action: &str, errs: &mut Vec<Error>) {
    debug!(action, cmd, "packages: running");
    match console.run(cmd) {
        Ok(out) => {
            let trimmed = out.trim();
            if trimmed.is_empty() {
                debug!(action, "packages: empty command output");
            } else {
                debug!(action, output = %trimmed, "packages: command output");
            }
        }
        Err(e) => {
            warn!(action, cmd, error = %e, "packages: command failed");
            errs.push(e);
        }
    }
}

/// Read `/etc/os-release` from `fs` and pick a package manager based on
/// the spec'd ID / ID_LIKE rules. Returns [`Error::Other`] when nothing
/// matches (or the file is missing / unparseable).
pub(crate) fn detect_package_manager(fs: &dyn Vfs) -> Result<PackageManager> {
    const OS_RELEASE: &str = "/etc/os-release";
    let content = fs
        .read_to_string(Path::new(OS_RELEASE))
        .map_err(|e| {
            debug!(error = %e, "packages: could not read /etc/os-release");
            Error::other("no supported package manager detected")
        })?;

    let (id, id_like) = parse_os_release(&content);

    if let Some(pm) = match_id(&id, &id_like) {
        return Ok(pm);
    }

    Err(Error::other("no supported package manager detected"))
}

fn match_id(id: &str, id_like: &str) -> Option<PackageManager> {
    let id_l = id.to_ascii_lowercase();
    let id_like_l = id_like.to_ascii_lowercase();

    // Debian/Ubuntu family.
    if id_l == "ubuntu" || id_l == "debian" {
        return Some(PackageManager::Apt);
    }
    if id_like_contains(&id_like_l, "debian") {
        return Some(PackageManager::Apt);
    }

    // RHEL family.
    if matches!(
        id_l.as_str(),
        "fedora" | "rhel" | "centos" | "rocky" | "alma" | "almalinux" | "oracle" | "ol"
    ) {
        return Some(PackageManager::Dnf);
    }
    if id_like_contains(&id_like_l, "rhel") || id_like_contains(&id_like_l, "fedora") {
        return Some(PackageManager::Dnf);
    }

    // Alpine.
    if id_l == "alpine" {
        return Some(PackageManager::Apk);
    }

    // SUSE family. `opensuse-leap`, `opensuse-tumbleweed`, `sles`, `sle-micro`.
    if id_l == "sles"
        || id_l == "sle-micro"
        || id_l == "suse"
        || id_l.starts_with("opensuse-")
        || id_l == "opensuse"
    {
        return Some(PackageManager::Zypper);
    }
    if id_like_contains(&id_like_l, "suse") {
        return Some(PackageManager::Zypper);
    }

    None
}

/// `ID_LIKE` is a space-separated list. Check membership tolerantly:
/// the field may be unquoted or wrapped in single/double quotes.
fn id_like_contains(id_like: &str, needle: &str) -> bool {
    id_like
        .split_whitespace()
        .any(|tok| tok.eq_ignore_ascii_case(needle))
}

/// Pull `ID=` and `ID_LIKE=` out of an os-release file. Anything else is
/// ignored. Values may be unquoted or quoted with `"` or `'`.
fn parse_os_release(content: &str) -> (String, String) {
    let mut id = String::new();
    let mut id_like = String::new();
    for raw in content.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some(eq) = line.find('=') else { continue };
        let key = line[..eq].trim();
        let val = strip_quotes(line[eq + 1..].trim());
        match key {
            "ID" => id = val.to_string(),
            "ID_LIKE" => id_like = val.to_string(),
            _ => {}
        }
    }
    (id, id_like)
}

fn strip_quotes(v: &str) -> &str {
    if v.len() >= 2 {
        let bytes = v.as_bytes();
        let first = bytes[0];
        let last = bytes[v.len() - 1];
        if (first == b'"' && last == b'"') || (first == b'\'' && last == b'\'') {
            return &v[1..v.len() - 1];
        }
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::console::RecordingConsole;
    use crate::schema::packages::Packages;
    use crate::vfs::MemVfs;

    fn write_os_release(fs: &MemVfs, body: &str) {
        fs.write(Path::new("/etc/os-release"), body.as_bytes())
            .unwrap();
    }

    fn stage_install(pkgs: &[&str]) -> Stage {
        Stage {
            packages: Packages {
                install: pkgs.iter().map(|s| s.to_string()).collect(),
                ..Default::default()
            },
            ..Default::default()
        }
    }

    // ----- detection -----

    #[test]
    fn detects_ubuntu_as_apt() {
        let fs = MemVfs::new();
        write_os_release(&fs, "ID=ubuntu\nVERSION=22.04\n");
        assert_eq!(detect_package_manager(&fs).unwrap(), PackageManager::Apt);
    }

    #[test]
    fn detects_debian_via_id_like() {
        let fs = MemVfs::new();
        write_os_release(&fs, "ID=raspbian\nID_LIKE=debian\n");
        assert_eq!(detect_package_manager(&fs).unwrap(), PackageManager::Apt);
    }

    #[test]
    fn detects_fedora_as_dnf() {
        let fs = MemVfs::new();
        write_os_release(&fs, "ID=fedora\nVERSION=39\n");
        assert_eq!(detect_package_manager(&fs).unwrap(), PackageManager::Dnf);
    }

    #[test]
    fn detects_rocky_centos_alma_oracle_as_dnf() {
        for id in ["rocky", "centos", "almalinux", "oracle", "rhel"] {
            let fs = MemVfs::new();
            write_os_release(&fs, &format!("ID={id}\n"));
            assert_eq!(detect_package_manager(&fs).unwrap(), PackageManager::Dnf);
        }
    }

    #[test]
    fn detects_rhel_via_id_like() {
        let fs = MemVfs::new();
        write_os_release(&fs, "ID=customrhelish\nID_LIKE=\"rhel fedora\"\n");
        assert_eq!(detect_package_manager(&fs).unwrap(), PackageManager::Dnf);
    }

    #[test]
    fn detects_alpine_as_apk() {
        let fs = MemVfs::new();
        write_os_release(&fs, "ID=alpine\n");
        assert_eq!(detect_package_manager(&fs).unwrap(), PackageManager::Apk);
    }

    #[test]
    fn detects_opensuse_as_zypper() {
        let fs = MemVfs::new();
        write_os_release(&fs, "ID=opensuse-leap\n");
        assert_eq!(detect_package_manager(&fs).unwrap(), PackageManager::Zypper);
        let fs = MemVfs::new();
        write_os_release(&fs, "ID=sles\n");
        assert_eq!(detect_package_manager(&fs).unwrap(), PackageManager::Zypper);
    }

    #[test]
    fn unknown_os_returns_error_other() {
        let fs = MemVfs::new();
        write_os_release(&fs, "ID=plan9\n");
        let err = detect_package_manager(&fs).unwrap_err();
        match err {
            Error::Other(msg) => assert!(msg.contains("no supported package manager")),
            other => panic!("expected Error::Other, got {other:?}"),
        }
    }

    #[test]
    fn missing_os_release_returns_error_other() {
        let fs = MemVfs::new();
        let err = detect_package_manager(&fs).unwrap_err();
        match err {
            Error::Other(msg) => assert!(msg.contains("no supported package manager")),
            other => panic!("expected Error::Other, got {other:?}"),
        }
    }

    #[test]
    fn parses_quoted_id() {
        let (id, like) = parse_os_release("ID=\"openEuler\"\nID_LIKE='rhel'\n");
        assert_eq!(id, "openEuler");
        assert_eq!(like, "rhel");
    }

    // ----- run() behaviour -----

    #[test]
    fn empty_stage_is_noop_even_without_os_release() {
        let fs = MemVfs::new();
        let console = RecordingConsole::new();
        run(&Stage::default(), &fs, &console).expect("noop");
        assert!(console.commands().is_empty());
    }

    #[test]
    fn install_on_ubuntu_fires_apt_get_install() {
        let fs = MemVfs::new();
        write_os_release(&fs, "ID=ubuntu\n");
        let console = RecordingConsole::new();
        run(&stage_install(&["foo"]), &fs, &console).expect("ok");
        assert_eq!(console.commands(), vec!["apt-get -y install foo".to_string()]);
    }

    #[test]
    fn install_on_fedora_fires_dnf_install() {
        let fs = MemVfs::new();
        write_os_release(&fs, "ID=fedora\n");
        let console = RecordingConsole::new();
        run(&stage_install(&["foo", "bar"]), &fs, &console).expect("ok");
        assert_eq!(
            console.commands(),
            vec!["dnf -y install foo bar".to_string()]
        );
    }

    #[test]
    fn full_action_order_on_apt() {
        let fs = MemVfs::new();
        write_os_release(&fs, "ID=debian\n");
        let console = RecordingConsole::new();
        let stage = Stage {
            packages: Packages {
                install: vec!["foo".into(), "bar".into()],
                remove: vec!["baz".into()],
                refresh: true,
                upgrade: true,
            },
            ..Default::default()
        };
        run(&stage, &fs, &console).expect("ok");
        assert_eq!(
            console.commands(),
            vec![
                "apt-get update".to_string(),
                "apt-get -y upgrade".to_string(),
                "apt-get -y install foo bar".to_string(),
                "apt-get -y remove baz".to_string(),
            ]
        );
    }

    #[test]
    fn full_action_order_on_alpine() {
        let fs = MemVfs::new();
        write_os_release(&fs, "ID=alpine\n");
        let console = RecordingConsole::new();
        let stage = Stage {
            packages: Packages {
                install: vec!["foo".into()],
                remove: vec!["bar".into()],
                refresh: true,
                upgrade: true,
            },
            ..Default::default()
        };
        run(&stage, &fs, &console).expect("ok");
        assert_eq!(
            console.commands(),
            vec![
                "apk update".to_string(),
                "apk upgrade".to_string(),
                "apk add foo".to_string(),
                "apk del bar".to_string(),
            ]
        );
    }

    #[test]
    fn full_action_order_on_zypper() {
        let fs = MemVfs::new();
        write_os_release(&fs, "ID=opensuse-tumbleweed\n");
        let console = RecordingConsole::new();
        let stage = Stage {
            packages: Packages {
                install: vec!["foo".into()],
                remove: vec!["bar".into()],
                refresh: true,
                upgrade: true,
            },
            ..Default::default()
        };
        run(&stage, &fs, &console).expect("ok");
        assert_eq!(
            console.commands(),
            vec![
                "zypper refresh".to_string(),
                "zypper -n update".to_string(),
                "zypper -n install foo".to_string(),
                "zypper -n remove bar".to_string(),
            ]
        );
    }

    #[test]
    fn unknown_os_with_actions_errors() {
        let fs = MemVfs::new();
        write_os_release(&fs, "ID=plan9\n");
        let console = RecordingConsole::new();
        let err = run(&stage_install(&["foo"]), &fs, &console).unwrap_err();
        match err {
            Error::Other(msg) => assert!(msg.contains("no supported package manager")),
            other => panic!("expected Error::Other, got {other:?}"),
        }
        assert!(console.commands().is_empty());
    }

    #[test]
    fn install_failure_aggregates_into_multi() {
        let fs = MemVfs::new();
        write_os_release(&fs, "ID=ubuntu\n");
        let console = RecordingConsole::new();
        console.expect("apt-get -y install foo", Err("locked".to_string()));
        let stage = Stage {
            packages: Packages {
                install: vec!["foo".into()],
                remove: vec!["bar".into()],
                ..Default::default()
            },
            ..Default::default()
        };
        let err = run(&stage, &fs, &console).unwrap_err();
        match err {
            Error::Multi(errs) => assert_eq!(errs.len(), 1),
            other => panic!("expected Multi, got {other:?}"),
        }
        // remove still ran despite the install failure.
        assert!(console
            .commands()
            .iter()
            .any(|c| c == "apt-get -y remove bar"));
    }

    #[test]
    fn build_returns_callable_plugin() {
        let fs = MemVfs::new();
        write_os_release(&fs, "ID=ubuntu\n");
        let console = RecordingConsole::new();
        let plugin = build();
        plugin(&stage_install(&["foo"]), &fs, &console).unwrap();
        assert_eq!(console.commands(), vec!["apt-get -y install foo".to_string()]);
    }

    // --- Additional tests ported from Go behaviour expectations ---

    #[test]
    fn apt_refresh_then_install_in_correct_order() {
        // Refresh + install in same stage: refresh must run before install.
        let fs = MemVfs::new();
        write_os_release(&fs, "ID=ubuntu\n");
        let console = RecordingConsole::new();
        let stage = Stage {
            packages: Packages {
                install: vec!["vim".into()],
                refresh: true,
                ..Default::default()
            },
            ..Default::default()
        };
        run(&stage, &fs, &console).expect("ok");
        assert_eq!(
            console.commands(),
            vec![
                "apt-get update".to_string(),
                "apt-get -y install vim".to_string(),
            ]
        );
    }

    #[test]
    fn dnf_upgrade_only() {
        // Only upgrade set — no install/remove/refresh.
        let fs = MemVfs::new();
        write_os_release(&fs, "ID=fedora\n");
        let console = RecordingConsole::new();
        let stage = Stage {
            packages: Packages {
                upgrade: true,
                ..Default::default()
            },
            ..Default::default()
        };
        run(&stage, &fs, &console).expect("ok");
        assert_eq!(
            console.commands(),
            vec!["dnf -y upgrade".to_string()]
        );
    }

    #[test]
    fn apk_install_with_version_pin_in_name() {
        // The `install` list can contain entries like `pkg=1.2.3` which apk
        // accepts natively. We pass these through unchanged.
        let fs = MemVfs::new();
        write_os_release(&fs, "ID=alpine\n");
        let console = RecordingConsole::new();
        let stage = Stage {
            packages: Packages {
                install: vec!["foo=1.2.3".into(), "bar".into()],
                ..Default::default()
            },
            ..Default::default()
        };
        run(&stage, &fs, &console).expect("ok");
        assert_eq!(
            console.commands(),
            vec!["apk add foo=1.2.3 bar".to_string()]
        );
    }

    #[test]
    fn refresh_only_no_install_runs_just_refresh() {
        // Empty install list + refresh: just the refresh command should
        // fire.
        let fs = MemVfs::new();
        write_os_release(&fs, "ID=ubuntu\n");
        let console = RecordingConsole::new();
        let stage = Stage {
            packages: Packages {
                refresh: true,
                ..Default::default()
            },
            ..Default::default()
        };
        run(&stage, &fs, &console).expect("ok");
        assert_eq!(console.commands(), vec!["apt-get update".to_string()]);
    }

    #[test]
    fn combined_refresh_upgrade_install_remove_apt_order() {
        // All four actions: refresh -> upgrade -> install -> remove.
        // (`full_action_order_on_apt` already covers this for debian; we
        // re-check for ubuntu to be explicit and to differ in package list
        // contents.)
        let fs = MemVfs::new();
        write_os_release(&fs, "ID=ubuntu\n");
        let console = RecordingConsole::new();
        let stage = Stage {
            packages: Packages {
                install: vec!["vim".into()],
                remove: vec!["nano".into()],
                refresh: true,
                upgrade: true,
            },
            ..Default::default()
        };
        run(&stage, &fs, &console).expect("ok");
        assert_eq!(
            console.commands(),
            vec![
                "apt-get update".to_string(),
                "apt-get -y upgrade".to_string(),
                "apt-get -y install vim".to_string(),
                "apt-get -y remove nano".to_string(),
            ]
        );
    }

    #[test]
    fn combined_refresh_upgrade_install_remove_dnf_order() {
        // Same scenario, dnf side.
        let fs = MemVfs::new();
        write_os_release(&fs, "ID=fedora\n");
        let console = RecordingConsole::new();
        let stage = Stage {
            packages: Packages {
                install: vec!["foo".into(), "bar".into()],
                remove: vec!["baz".into()],
                refresh: true,
                upgrade: true,
            },
            ..Default::default()
        };
        run(&stage, &fs, &console).expect("ok");
        assert_eq!(
            console.commands(),
            vec![
                "dnf check-update".to_string(),
                "dnf -y upgrade".to_string(),
                "dnf -y install foo bar".to_string(),
                "dnf -y remove baz".to_string(),
            ]
        );
    }

    #[test]
    fn upgrade_after_failed_refresh_still_runs() {
        // Per multierror semantics: a failing refresh accumulates an error
        // but the rest of the chain proceeds (upgrade then install).
        let fs = MemVfs::new();
        write_os_release(&fs, "ID=ubuntu\n");
        let console = RecordingConsole::new();
        console.expect("apt-get update", Err("net down".to_string()));
        let stage = Stage {
            packages: Packages {
                install: vec!["foo".into()],
                refresh: true,
                upgrade: true,
                ..Default::default()
            },
            ..Default::default()
        };
        let err = run(&stage, &fs, &console).expect_err("refresh fails");
        match err {
            Error::Multi(errs) => assert_eq!(errs.len(), 1),
            other => panic!("expected Multi, got {other:?}"),
        }
        // Subsequent stages still ran.
        assert!(console
            .commands()
            .iter()
            .any(|c| c == "apt-get -y upgrade"));
        assert!(console
            .commands()
            .iter()
            .any(|c| c == "apt-get -y install foo"));
    }
}
