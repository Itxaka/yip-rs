//! `hostname` plugin — port of `pkg/plugins/hostname.go`.
//!
//! Sets the system hostname:
//!   1. Writes the hostname to `/etc/hostname` (with trailing newline, like Go).
//!   2. Writes a 32-char hex machine-id to `/etc/machine-id` (Go uses
//!      `denisbrodbeck/machineid` which reads `/etc/machine-id`/D-Bus; in
//!      Rust we synthesise one from a v4 UUID since this plugin only runs
//!      in early-boot where the file may not yet exist).
//!   3. Calls `sethostname(2)` via `nix::unistd::sethostname`. Failure
//!      (typically EPERM as non-root in tests) is logged and swallowed —
//!      the on-disk write is what matters; the live kernel hostname will
//!      be picked up on next boot.
//!
//! Empty `stage.hostname` short-circuits to `Ok(())`.

use std::path::Path;
use std::sync::Arc;

use tracing::{debug, warn};
use uuid::Uuid;

use crate::console::Console;
use crate::error::Result;
use crate::executor::Plugin;
use crate::schema::Stage;
use crate::vfs::Vfs;

const HOSTNAME_PATH: &str = "/etc/hostname";
const MACHINE_ID_PATH: &str = "/etc/machine-id";

/// Build the plugin closure for registration with the executor.
pub fn build() -> Plugin {
    Arc::new(run)
}

/// Apply the hostname plugin against `stage`.
pub fn run(stage: &Stage, fs: &dyn Vfs, _console: &dyn Console) -> Result<()> {
    let hostname = stage.hostname.trim();
    if hostname.is_empty() {
        debug!("hostname: empty, skipping");
        return Ok(());
    }

    // 1. Write /etc/hostname with trailing newline (Go appends "\n").
    let mut bytes = hostname.as_bytes().to_vec();
    bytes.push(b'\n');
    fs.write(Path::new(HOSTNAME_PATH), &bytes)?;
    debug!(hostname, "wrote /etc/hostname");

    // 2. Write a 32-char hex machine-id. Go pulls it from
    //    denisbrodbeck/machineid which reads /etc/machine-id or D-Bus;
    //    we just synthesise a fresh one. The format is exactly 32 hex
    //    chars, lowercased.
    let machine_id = Uuid::new_v4().simple().to_string();
    fs.write(Path::new(MACHINE_ID_PATH), machine_id.as_bytes())?;
    debug!(machine_id, "wrote /etc/machine-id");

    // 3. Try to apply live via sethostname(2). Non-root will get EPERM —
    //    log + continue so unit tests + non-privileged dry-runs don't fail.
    //    Use libc directly to avoid needing the `nix` "hostname" feature.
    // SAFETY: `hostname` is a borrowed &str; we pass its bytes and length.
    let rc = unsafe {
        libc::sethostname(hostname.as_ptr() as *const libc::c_char, hostname.len())
    };
    if rc == 0 {
        debug!(hostname, "sethostname succeeded");
    } else {
        let err = std::io::Error::last_os_error();
        warn!(error = %err, hostname, "sethostname failed (non-root?), continuing");
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::console::RecordingConsole;
    use crate::vfs::MemVfs;

    #[test]
    fn empty_hostname_is_noop() {
        let fs = MemVfs::new();
        let console = RecordingConsole::new();
        let stage = Stage::default();
        run(&stage, &fs, &console).expect("noop ok");
        assert!(!fs.exists(Path::new(HOSTNAME_PATH)));
        assert!(!fs.exists(Path::new(MACHINE_ID_PATH)));
    }

    #[test]
    fn writes_hostname_with_trailing_newline() {
        let fs = MemVfs::new();
        let console = RecordingConsole::new();
        let stage = Stage {
            hostname: "myhost".into(),
            ..Default::default()
        };
        // sethostname will fail as non-root, but run() swallows it.
        run(&stage, &fs, &console).expect("run ok despite sethostname failure");
        let got = fs.read_to_string(Path::new(HOSTNAME_PATH)).unwrap();
        assert_eq!(got, "myhost\n");
    }

    #[test]
    fn writes_machine_id_in_32_hex_format() {
        let fs = MemVfs::new();
        let console = RecordingConsole::new();
        let stage = Stage {
            hostname: "x".into(),
            ..Default::default()
        };
        run(&stage, &fs, &console).expect("run ok");
        let mid = fs.read_to_string(Path::new(MACHINE_ID_PATH)).unwrap();
        assert_eq!(mid.len(), 32, "machine-id must be 32 chars, got {mid:?}");
        assert!(
            mid.chars().all(|c| c.is_ascii_hexdigit()),
            "machine-id must be hex, got {mid:?}"
        );
    }

    #[test]
    fn sethostname_failure_does_not_abort_run() {
        // We assert this by asserting that the file write still happens even
        // though sethostname returns EPERM in unit-test context.
        let fs = MemVfs::new();
        let console = RecordingConsole::new();
        let stage = Stage {
            hostname: "another".into(),
            ..Default::default()
        };
        let res = run(&stage, &fs, &console);
        assert!(res.is_ok(), "expected Ok, got {res:?}");
        assert_eq!(
            fs.read_to_string(Path::new(HOSTNAME_PATH)).unwrap(),
            "another\n"
        );
    }

    #[test]
    fn whitespace_only_hostname_is_noop() {
        let fs = MemVfs::new();
        let console = RecordingConsole::new();
        let stage = Stage {
            hostname: "   ".into(),
            ..Default::default()
        };
        run(&stage, &fs, &console).expect("noop ok");
        assert!(!fs.exists(Path::new(HOSTNAME_PATH)));
    }

    #[test]
    fn build_returns_callable_plugin() {
        let plugin = build();
        let fs = MemVfs::new();
        let console = RecordingConsole::new();
        let stage = Stage {
            hostname: "viabuild".into(),
            ..Default::default()
        };
        plugin(&stage, &fs, &console).expect("plugin closure ok");
        assert_eq!(
            fs.read_to_string(Path::new(HOSTNAME_PATH)).unwrap(),
            "viabuild\n"
        );
    }

    // -------------------------------------------------------------------
    // Ported from Go: FQDN, overwrites of pre-existing machine-id /
    // hostname files.
    // -------------------------------------------------------------------

    #[test]
    fn fqdn_hostname_with_dots_is_written_verbatim() {
        let fs = MemVfs::new();
        let console = RecordingConsole::default();
        let stage = Stage {
            hostname: "host1.example.com".into(),
            ..Default::default()
        };
        run(&stage, &fs, &console).expect("ok");
        assert_eq!(
            fs.read_to_string(Path::new(HOSTNAME_PATH)).unwrap(),
            "host1.example.com\n"
        );
    }

    #[test]
    fn empty_pre_existing_machine_id_is_overwritten() {
        let fs = MemVfs::new();
        let console = RecordingConsole::default();
        // Seed an empty machine-id file (matches the Go default_test fixture).
        fs.write(Path::new(MACHINE_ID_PATH), b"").unwrap();
        let stage = Stage {
            hostname: "h".into(),
            ..Default::default()
        };
        run(&stage, &fs, &console).expect("ok");
        let got = fs.read_to_string(Path::new(MACHINE_ID_PATH)).unwrap();
        // No longer empty; 32 hex chars.
        assert_eq!(got.len(), 32);
        assert!(got.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn pre_existing_hostname_file_is_overwritten() {
        let fs = MemVfs::new();
        let console = RecordingConsole::default();
        // Pre-existing file contents must be replaced, not appended to.
        fs.write(Path::new(HOSTNAME_PATH), b"oldhost\n").unwrap();
        let stage = Stage {
            hostname: "newhost".into(),
            ..Default::default()
        };
        run(&stage, &fs, &console).expect("ok");
        let got = fs.read_to_string(Path::new(HOSTNAME_PATH)).unwrap();
        assert_eq!(got, "newhost\n");
        assert!(!got.contains("oldhost"));
    }

    #[test]
    fn two_runs_produce_different_machine_ids() {
        // Machine-id is regenerated each run from a fresh v4 UUID; two
        // consecutive plugin invocations against fresh filesystems should
        // not collide. (Probability of UUID-v4 collision is astronomical.)
        let stage = Stage {
            hostname: "x".into(),
            ..Default::default()
        };
        let fs1 = MemVfs::new();
        let fs2 = MemVfs::new();
        let con = RecordingConsole::default();
        run(&stage, &fs1, &con).unwrap();
        run(&stage, &fs2, &con).unwrap();
        let a = fs1.read_to_string(Path::new(MACHINE_ID_PATH)).unwrap();
        let b = fs2.read_to_string(Path::new(MACHINE_ID_PATH)).unwrap();
        assert_ne!(a, b);
    }
}
