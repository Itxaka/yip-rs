//! `package_pins` plugin — port of `pkg/plugins/package_pins.go`.
//!
//! Best-effort version-pin policy applied before package installs. The
//! Rust port follows the simplified file-layout spec rather than the Go
//! original (which uses dnf-versionlock and zypper addlock):
//!
//!   - apt: per-package file at `/etc/apt/preferences.d/<pkg>.pref`
//!     containing `Package: <pkg>\nPin: version <ver>\nPin-Priority: 1001\n`
//!   - dnf: per-package file at `/etc/dnf/protected.d/<pkg>.conf`
//!     containing `<pkg>\n`, plus a `versionlock` line appended to
//!     `/etc/dnf/dnf.conf`
//!   - apk: rewrite `/etc/apk/world` with `<pkg>=<ver>` entries (preserving
//!     any pre-existing entries that aren't being pinned)
//!
//! Anything else (no os-release, unknown distro, zypper) is logged at warn
//! and skipped — pinning is opportunistic, never fatal. Per-pin errors
//! aggregate into [`Error::Multi`].

use std::path::{Path, PathBuf};
use std::sync::Arc;

use tracing::{debug, info, warn};

use crate::console::Console;
use crate::error::{Error, Result};
use crate::executor::Plugin;
use crate::plugins::packages::{detect_package_manager, PackageManager};
use crate::schema::Stage;
use crate::vfs::Vfs;

/// Build the plugin closure for registration with the executor.
pub fn build() -> Plugin {
    Arc::new(run)
}

/// Apply the package_pins plugin against `stage`.
pub fn run(stage: &Stage, fs: &dyn Vfs, _console: &dyn Console) -> Result<()> {
    if stage.package_pins.is_empty() {
        debug!("package_pins: empty map, skipping");
        return Ok(());
    }

    let pm = match detect_package_manager(fs) {
        Ok(pm) => pm,
        Err(e) => {
            warn!(error = %e, "package_pins: no supported package manager detected, skipping");
            return Ok(());
        }
    };

    // Deterministic order so output files are reproducible.
    let mut keys: Vec<&String> = stage.package_pins.keys().collect();
    keys.sort();

    let mut errs: Vec<Error> = Vec::new();

    match pm {
        PackageManager::Apt => apply_apt(fs, &keys, &stage.package_pins, &mut errs),
        PackageManager::Dnf => apply_dnf(fs, &keys, &stage.package_pins, &mut errs),
        PackageManager::Apk => apply_apk(fs, &keys, &stage.package_pins, &mut errs),
        PackageManager::Zypper => {
            warn!("package_pins: zypper pinning not supported by this port; skipping");
            return Ok(());
        }
    }

    if errs.is_empty() {
        info!(pinned = keys.len(), ?pm, "package_pins: applied");
        Ok(())
    } else {
        Err(Error::Multi(errs))
    }
}

// ---- apt ----

fn apply_apt(
    fs: &dyn Vfs,
    keys: &[&String],
    pins: &std::collections::HashMap<String, String>,
    errs: &mut Vec<Error>,
) {
    let dir = Path::new("/etc/apt/preferences.d");
    if let Err(e) = fs.mkdir_all(dir) {
        warn!(error = %e, "package_pins(apt): mkdir /etc/apt/preferences.d failed");
        errs.push(e);
        return;
    }

    for name in keys {
        let ver = pins[*name].trim();
        if ver.is_empty() {
            warn!(pkg = %name, "package_pins(apt): empty version, skipping");
            continue;
        }
        let path: PathBuf = dir.join(format!("{name}.pref"));
        let body = format!(
            "Package: {name}\nPin: version {ver}\nPin-Priority: 1001\n"
        );
        debug!(pkg = %name, ver = %ver, path = %path.display(), "package_pins(apt): writing pref");
        if let Err(e) = fs.write(&path, body.as_bytes()) {
            warn!(pkg = %name, error = %e, "package_pins(apt): write failed");
            errs.push(e);
        }
    }
}

// ---- dnf ----

fn apply_dnf(
    fs: &dyn Vfs,
    keys: &[&String],
    pins: &std::collections::HashMap<String, String>,
    errs: &mut Vec<Error>,
) {
    let protected_dir = Path::new("/etc/dnf/protected.d");
    if let Err(e) = fs.mkdir_all(protected_dir) {
        warn!(error = %e, "package_pins(dnf): mkdir /etc/dnf/protected.d failed");
        errs.push(e);
        return;
    }

    for name in keys {
        let ver = pins[*name].trim();
        if ver.is_empty() {
            warn!(pkg = %name, "package_pins(dnf): empty version, skipping");
            continue;
        }
        let path = protected_dir.join(format!("{name}.conf"));
        let body = format!("{name}\n");
        debug!(pkg = %name, ver = %ver, path = %path.display(), "package_pins(dnf): writing protected file");
        if let Err(e) = fs.write(&path, body.as_bytes()) {
            warn!(pkg = %name, error = %e, "package_pins(dnf): write failed");
            errs.push(e);
        }
    }

    // Ensure dnf.conf has a `versionlock` plugin entry. We add it
    // idempotently: read existing content, append if not already present.
    let dnf_conf = Path::new("/etc/dnf/dnf.conf");
    if let Err(e) = update_dnf_conf_versionlock(fs, dnf_conf) {
        warn!(error = %e, "package_pins(dnf): updating dnf.conf failed");
        errs.push(e);
    }
}

/// Add a `versionlock` marker to `/etc/dnf/dnf.conf`. If the file does
/// not exist, create a minimal `[main]` section. If the marker is
/// already present, do nothing.
fn update_dnf_conf_versionlock(fs: &dyn Vfs, path: &Path) -> Result<()> {
    // Make sure the parent dir exists.
    if let Some(parent) = path.parent() {
        fs.mkdir_all(parent)?;
    }

    let mut existing = if fs.exists(path) {
        fs.read_to_string(path).unwrap_or_default()
    } else {
        String::new()
    };

    // Idempotent: don't re-add if the file already mentions versionlock.
    if existing
        .lines()
        .any(|l| l.trim().eq_ignore_ascii_case("versionlock"))
    {
        debug!("package_pins(dnf): dnf.conf already contains versionlock");
        return Ok(());
    }

    if existing.is_empty() {
        existing.push_str("[main]\n");
    } else if !existing.ends_with('\n') {
        existing.push('\n');
    }
    existing.push_str("versionlock\n");

    fs.write(path, existing.as_bytes())
}

// ---- apk ----

fn apply_apk(
    fs: &dyn Vfs,
    keys: &[&String],
    pins: &std::collections::HashMap<String, String>,
    errs: &mut Vec<Error>,
) {
    let dir = Path::new("/etc/apk");
    if let Err(e) = fs.mkdir_all(dir) {
        warn!(error = %e, "package_pins(apk): mkdir /etc/apk failed");
        errs.push(e);
        return;
    }

    let world_path = dir.join("world");
    // Parse any existing entries so we don't drop unrelated packages.
    let existing = if fs.exists(&world_path) {
        fs.read_to_string(&world_path).unwrap_or_default()
    } else {
        String::new()
    };

    let mut entries: Vec<String> = Vec::new();
    for raw in existing.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        // Drop any pre-existing entry for a package we're about to pin —
        // the new pinned entry replaces it.
        let pkg_base = line
            .split(|c: char| c == '=' || c == '<' || c == '>' || c == '~')
            .next()
            .unwrap_or(line)
            .trim();
        if keys.iter().any(|k| k.as_str() == pkg_base) {
            continue;
        }
        entries.push(line.to_string());
    }

    for name in keys {
        let ver = pins[*name].trim();
        if ver.is_empty() {
            warn!(pkg = %name, "package_pins(apk): empty version, skipping");
            continue;
        }
        entries.push(format!("{name}={ver}"));
    }

    // Deterministic ordering of the final file: lexicographic.
    entries.sort();
    entries.dedup();

    let mut body = String::new();
    for e in &entries {
        body.push_str(e);
        body.push('\n');
    }

    debug!(path = %world_path.display(), "package_pins(apk): rewriting world");
    if let Err(e) = fs.write(&world_path, body.as_bytes()) {
        warn!(error = %e, "package_pins(apk): write failed");
        errs.push(e);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::console::RecordingConsole;
    use crate::schema::packages::PackagePins;
    use crate::vfs::MemVfs;

    fn write_os_release(fs: &MemVfs, body: &str) {
        fs.write(Path::new("/etc/os-release"), body.as_bytes())
            .unwrap();
    }

    fn stage_pins(entries: &[(&str, &str)]) -> Stage {
        let mut m: PackagePins = std::collections::HashMap::new();
        for (k, v) in entries {
            m.insert((*k).to_string(), (*v).to_string());
        }
        Stage {
            package_pins: m,
            ..Default::default()
        }
    }

    #[test]
    fn empty_pins_is_noop() {
        let fs = MemVfs::new();
        write_os_release(&fs, "ID=ubuntu\n");
        let console = RecordingConsole::new();
        run(&Stage::default(), &fs, &console).unwrap();
        assert!(!fs.exists(Path::new("/etc/apt/preferences.d/foo.pref")));
    }

    #[test]
    fn unknown_os_warns_and_skips() {
        let fs = MemVfs::new();
        write_os_release(&fs, "ID=plan9\n");
        let console = RecordingConsole::new();
        run(&stage_pins(&[("foo", "1.0")]), &fs, &console).expect("skip is ok");
        // Nothing written under any of our managed dirs.
        assert!(!fs.exists(Path::new("/etc/apt/preferences.d/foo.pref")));
        assert!(!fs.exists(Path::new("/etc/dnf/protected.d/foo.conf")));
        assert!(!fs.exists(Path::new("/etc/apk/world")));
    }

    // ---- apt ----

    #[test]
    fn apt_writes_per_pkg_pref_file() {
        let fs = MemVfs::new();
        write_os_release(&fs, "ID=debian\n");
        let console = RecordingConsole::new();
        run(&stage_pins(&[("foo", "1.2.3")]), &fs, &console).unwrap();

        let path = Path::new("/etc/apt/preferences.d/foo.pref");
        assert!(fs.exists(path));
        let got = fs.read_to_string(path).unwrap();
        assert_eq!(got, "Package: foo\nPin: version 1.2.3\nPin-Priority: 1001\n");
    }

    #[test]
    fn apt_multiple_pins_write_separate_files() {
        let fs = MemVfs::new();
        write_os_release(&fs, "ID=ubuntu\n");
        let console = RecordingConsole::new();
        run(
            &stage_pins(&[("foo", "1.0"), ("bar", "2.0")]),
            &fs,
            &console,
        )
        .unwrap();
        assert!(fs.exists(Path::new("/etc/apt/preferences.d/foo.pref")));
        assert!(fs.exists(Path::new("/etc/apt/preferences.d/bar.pref")));
        assert_eq!(
            fs.read_to_string(Path::new("/etc/apt/preferences.d/bar.pref"))
                .unwrap(),
            "Package: bar\nPin: version 2.0\nPin-Priority: 1001\n"
        );
    }

    #[test]
    fn apt_empty_version_skipped() {
        let fs = MemVfs::new();
        write_os_release(&fs, "ID=ubuntu\n");
        let console = RecordingConsole::new();
        run(&stage_pins(&[("foo", "")]), &fs, &console).unwrap();
        assert!(!fs.exists(Path::new("/etc/apt/preferences.d/foo.pref")));
    }

    // ---- dnf ----

    #[test]
    fn dnf_writes_protected_conf_and_dnf_conf_versionlock() {
        let fs = MemVfs::new();
        write_os_release(&fs, "ID=fedora\n");
        let console = RecordingConsole::new();
        run(&stage_pins(&[("foo", "1.2.3")]), &fs, &console).unwrap();

        let protected = Path::new("/etc/dnf/protected.d/foo.conf");
        assert!(fs.exists(protected));
        assert_eq!(fs.read_to_string(protected).unwrap(), "foo\n");

        let conf = fs.read_to_string(Path::new("/etc/dnf/dnf.conf")).unwrap();
        assert!(
            conf.contains("versionlock"),
            "expected versionlock in dnf.conf, got: {conf}"
        );
    }

    #[test]
    fn dnf_dnf_conf_versionlock_is_idempotent() {
        let fs = MemVfs::new();
        write_os_release(&fs, "ID=rocky\n");
        // pre-existing dnf.conf already configured.
        fs.write(
            Path::new("/etc/dnf/dnf.conf"),
            b"[main]\ngpgcheck=1\nversionlock\n",
        )
        .unwrap();
        let console = RecordingConsole::new();
        run(&stage_pins(&[("foo", "1.0")]), &fs, &console).unwrap();
        let conf = fs.read_to_string(Path::new("/etc/dnf/dnf.conf")).unwrap();
        // Exactly one versionlock line.
        let count = conf.lines().filter(|l| l.trim() == "versionlock").count();
        assert_eq!(count, 1, "versionlock should not be duplicated, got: {conf}");
    }

    #[test]
    fn dnf_creates_dnf_conf_when_missing() {
        let fs = MemVfs::new();
        write_os_release(&fs, "ID=rhel\n");
        let console = RecordingConsole::new();
        run(&stage_pins(&[("kernel", "5.14")]), &fs, &console).unwrap();
        let conf = fs.read_to_string(Path::new("/etc/dnf/dnf.conf")).unwrap();
        assert!(conf.contains("[main]"));
        assert!(conf.contains("versionlock"));
    }

    // ---- apk ----

    #[test]
    fn apk_writes_world_with_version_suffix() {
        let fs = MemVfs::new();
        write_os_release(&fs, "ID=alpine\n");
        let console = RecordingConsole::new();
        run(&stage_pins(&[("foo", "1.2.3")]), &fs, &console).unwrap();
        let got = fs.read_to_string(Path::new("/etc/apk/world")).unwrap();
        assert_eq!(got, "foo=1.2.3\n");
    }

    #[test]
    fn apk_preserves_other_world_entries() {
        let fs = MemVfs::new();
        write_os_release(&fs, "ID=alpine\n");
        fs.write(Path::new("/etc/apk/world"), b"keep-me\nold-foo\n")
            .unwrap();
        let console = RecordingConsole::new();
        run(&stage_pins(&[("foo", "1.0")]), &fs, &console).unwrap();
        let got = fs.read_to_string(Path::new("/etc/apk/world")).unwrap();
        // foo gets a pinned entry; keep-me and old-foo (unrelated) remain.
        assert!(got.contains("foo=1.0\n"));
        assert!(got.contains("keep-me\n"));
        assert!(got.contains("old-foo\n"));
    }

    #[test]
    fn apk_replaces_existing_entry_for_same_pkg() {
        let fs = MemVfs::new();
        write_os_release(&fs, "ID=alpine\n");
        fs.write(Path::new("/etc/apk/world"), b"foo\nbar\n").unwrap();
        let console = RecordingConsole::new();
        run(&stage_pins(&[("foo", "2.0")]), &fs, &console).unwrap();
        let got = fs.read_to_string(Path::new("/etc/apk/world")).unwrap();
        // The unpinned `foo` was dropped, the `foo=2.0` replaced it.
        assert!(got.contains("foo=2.0\n"));
        assert!(!got.lines().any(|l| l == "foo"));
        assert!(got.contains("bar\n"));
    }

    #[test]
    fn apk_empty_version_skipped() {
        let fs = MemVfs::new();
        write_os_release(&fs, "ID=alpine\n");
        let console = RecordingConsole::new();
        run(&stage_pins(&[("foo", "")]), &fs, &console).unwrap();
        let got = fs.read_to_string(Path::new("/etc/apk/world")).unwrap();
        assert_eq!(got, "");
    }

    #[test]
    fn build_returns_callable_plugin() {
        let fs = MemVfs::new();
        write_os_release(&fs, "ID=ubuntu\n");
        let console = RecordingConsole::new();
        let plugin = build();
        plugin(&stage_pins(&[("foo", "1.0")]), &fs, &console).unwrap();
        assert!(fs.exists(Path::new("/etc/apt/preferences.d/foo.pref")));
    }
}
