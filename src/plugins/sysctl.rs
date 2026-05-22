//! `sysctl` plugin — port of `pkg/plugins/sysctl.go`.
//!
//! For each `key=value` in `stage.sysctl`, writes `value` to
//! `/proc/sys/<key-with-dots-as-slashes>`. Errors are best-effort:
//! they're aggregated into [`Error::Multi`] but every key is still
//! attempted. Go's plugin does only the `/proc/sys` write; it does
//! NOT also persist to `/etc/sysctl.d/`, so neither do we.

use std::path::PathBuf;
use std::sync::Arc;

use tracing::{debug, info, warn};

use crate::console::Console;
use crate::error::{Error, Result};
use crate::executor::Plugin;
use crate::schema::Stage;
use crate::vfs::Vfs;

/// Build the plugin closure for registration with the executor.
pub fn build() -> Plugin {
    Arc::new(run)
}

/// Apply the sysctl plugin against `stage`.
pub fn run(stage: &Stage, fs: &dyn Vfs, _console: &dyn Console) -> Result<()> {
    if stage.sysctl.is_empty() {
        debug!("sysctl: empty map, skipping");
        return Ok(());
    }

    let mut errs: Vec<Error> = Vec::new();
    // Sort keys so output ordering / error ordering is deterministic.
    let mut keys: Vec<&String> = stage.sysctl.keys().collect();
    keys.sort();

    for k in keys {
        let v = &stage.sysctl[k];
        let path = sysctl_path(k);
        debug!(key = %k, path = %path.display(), "applying sysctl");
        if let Err(e) = fs.write(&path, v.as_bytes()) {
            warn!(key = %k, error = %e, "sysctl write failed");
            errs.push(e);
        }
    }

    match errs.len() {
        0 => {
            info!(count = stage.sysctl.len(), "sysctls applied");
            Ok(())
        }
        _ => Err(Error::Multi(errs)),
    }
}

/// Translate a dotted sysctl key into its `/proc/sys/...` path.
/// e.g. `vm.swappiness` -> `/proc/sys/vm/swappiness`.
fn sysctl_path(key: &str) -> PathBuf {
    let mut p = PathBuf::from("/proc/sys");
    for seg in key.split('.') {
        if seg.is_empty() {
            continue;
        }
        p.push(seg);
    }
    p
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::path::Path;

    use crate::console::RecordingConsole;
    use crate::vfs::MemVfs;

    fn stage_sysctl(pairs: &[(&str, &str)]) -> Stage {
        let mut m = HashMap::new();
        for (k, v) in pairs {
            m.insert((*k).to_string(), (*v).to_string());
        }
        Stage {
            sysctl: m,
            ..Default::default()
        }
    }

    #[test]
    fn empty_sysctl_is_noop() {
        let fs = MemVfs::new();
        let console = RecordingConsole::new();
        run(&Stage::default(), &fs, &console).unwrap();
        // Trivially: nothing written. We can only assert via picking a
        // representative path.
        assert!(!fs.exists(Path::new("/proc/sys/vm/swappiness")));
    }

    #[test]
    fn writes_single_sysctl() {
        let fs = MemVfs::new();
        let console = RecordingConsole::new();
        let stage = stage_sysctl(&[("vm.swappiness", "10")]);
        run(&stage, &fs, &console).unwrap();
        let got = fs
            .read_to_string(Path::new("/proc/sys/vm/swappiness"))
            .unwrap();
        assert_eq!(got, "10");
    }

    #[test]
    fn writes_two_sysctls() {
        let fs = MemVfs::new();
        let console = RecordingConsole::new();
        let stage = stage_sysctl(&[
            ("debug.exception-trace", "0"),
            ("net.ipv4.ip_forward", "1"),
        ]);
        run(&stage, &fs, &console).unwrap();
        assert_eq!(
            fs.read_to_string(Path::new("/proc/sys/debug/exception-trace"))
                .unwrap(),
            "0"
        );
        assert_eq!(
            fs.read_to_string(Path::new("/proc/sys/net/ipv4/ip_forward"))
                .unwrap(),
            "1"
        );
    }

    #[test]
    fn dot_to_slash_translation() {
        assert_eq!(
            sysctl_path("vm.swappiness"),
            PathBuf::from("/proc/sys/vm/swappiness")
        );
        assert_eq!(
            sysctl_path("net.ipv4.conf.all.forwarding"),
            PathBuf::from("/proc/sys/net/ipv4/conf/all/forwarding")
        );
        // Single-segment key (no dot).
        assert_eq!(sysctl_path("kernel"), PathBuf::from("/proc/sys/kernel"));
        // Leading/trailing dot should not produce empty path segments.
        assert_eq!(
            sysctl_path(".vm.swappiness."),
            PathBuf::from("/proc/sys/vm/swappiness")
        );
    }

    /// A [`Vfs`] that fails `write` for a configured path prefix. Used to
    /// verify the multi-error aggregation path without needing /proc.
    struct PartialFailVfs {
        inner: MemVfs,
        fail_for: String,
    }

    impl Vfs for PartialFailVfs {
        fn read(&self, path: &Path) -> Result<Vec<u8>> {
            self.inner.read(path)
        }
        fn write(&self, path: &Path, bytes: &[u8]) -> Result<()> {
            if path.to_string_lossy().contains(&self.fail_for) {
                return Err(Error::other(format!("synthetic failure for {:?}", path)));
            }
            self.inner.write(path, bytes)
        }
        fn mkdir_all(&self, path: &Path) -> Result<()> {
            self.inner.mkdir_all(path)
        }
        fn read_dir(&self, path: &Path) -> Result<Vec<PathBuf>> {
            self.inner.read_dir(path)
        }
        fn metadata(&self, path: &Path) -> Result<crate::vfs::Metadata> {
            self.inner.metadata(path)
        }
        fn exists(&self, path: &Path) -> bool {
            self.inner.exists(path)
        }
        fn remove(&self, path: &Path) -> Result<()> {
            self.inner.remove(path)
        }
        fn remove_all(&self, path: &Path) -> Result<()> {
            self.inner.remove_all(path)
        }
        fn chmod(&self, path: &Path, mode: u32) -> Result<()> {
            self.inner.chmod(path, mode)
        }
        fn chown(&self, path: &Path, uid: i32, gid: i32) -> Result<()> {
            self.inner.chown(path, uid, gid)
        }
        fn symlink(&self, target: &Path, link: &Path) -> Result<()> {
            self.inner.symlink(target, link)
        }
        fn walk(&self, root: &Path) -> Result<Vec<PathBuf>> {
            self.inner.walk(root)
        }
    }

    #[test]
    fn aggregates_errors_but_continues() {
        let fs = PartialFailVfs {
            inner: MemVfs::new(),
            fail_for: "bad".to_string(),
        };
        let console = RecordingConsole::new();
        let stage = stage_sysctl(&[("vm.bad", "x"), ("vm.good", "y")]);
        let err = run(&stage, &fs, &console).unwrap_err();
        match err {
            Error::Multi(es) => {
                assert_eq!(es.len(), 1, "exactly one key should have failed");
            }
            other => panic!("expected Multi, got {other:?}"),
        }
        // The "good" key was still written despite the failure on "bad".
        assert_eq!(
            fs.inner
                .read_to_string(Path::new("/proc/sys/vm/good"))
                .unwrap(),
            "y"
        );
    }

    #[test]
    fn build_returns_callable_plugin() {
        let plugin = build();
        let fs = MemVfs::new();
        let console = RecordingConsole::new();
        let stage = stage_sysctl(&[("kernel.hostname", "ignored")]);
        plugin(&stage, &fs, &console).unwrap();
        assert_eq!(
            fs.read_to_string(Path::new("/proc/sys/kernel/hostname"))
                .unwrap(),
            "ignored"
        );
    }

    // -------------------------------------------------------------------
    // Ported from Go: deep keys, empty value, read-only failure.
    // -------------------------------------------------------------------

    #[test]
    fn deeply_nested_key_resolves_correctly() {
        let fs = MemVfs::new();
        let console = RecordingConsole::default();
        let stage = stage_sysctl(&[("net.ipv4.tcp_syncookies", "1")]);
        run(&stage, &fs, &console).unwrap();
        let got = fs
            .read_to_string(Path::new("/proc/sys/net/ipv4/tcp_syncookies"))
            .unwrap();
        assert_eq!(got, "1");
    }

    #[test]
    fn empty_value_writes_empty_file() {
        // Some sysctls accept the empty string to clear a value.
        let fs = MemVfs::new();
        let console = RecordingConsole::default();
        let stage = stage_sysctl(&[("net.ipv4.foo", "")]);
        run(&stage, &fs, &console).unwrap();
        let got = fs
            .read_to_string(Path::new("/proc/sys/net/ipv4/foo"))
            .unwrap();
        assert_eq!(got, "");
    }

    #[test]
    fn read_only_sysctl_path_failure_captured_in_multi() {
        // Mock a read-only target — the write call returns an error, and the
        // plugin should aggregate it into Error::Multi while still attempting
        // siblings.
        let fs = PartialFailVfs {
            inner: MemVfs::new(),
            fail_for: "readonly".to_string(),
        };
        let console = RecordingConsole::default();
        let stage = stage_sysctl(&[
            ("kernel.readonly_key", "x"),
            ("kernel.normal_key", "y"),
        ]);
        let err = run(&stage, &fs, &console).unwrap_err();
        match err {
            Error::Multi(es) => assert_eq!(es.len(), 1, "exactly one failure"),
            other => panic!("expected Multi, got {other:?}"),
        }
        // Sibling key still succeeded.
        assert_eq!(
            fs.inner
                .read_to_string(Path::new("/proc/sys/kernel/normal_key"))
                .unwrap(),
            "y"
        );
    }
}
