//! Port of `pkg/plugins/systemd_firstboot.go`.
//!
//! Drives `systemd-firstboot(1)` so cloud-init-style configs can seed
//! locale / keymap / hostname / timezone / etc on first boot. The Go
//! plugin issues a single shell invocation with every flag concatenated
//! and alphabetically sorted; we match that exactly so test fixtures and
//! audit logs stay stable.
//!
//! Encoding rules (mirroring Go):
//!   - keys are lowercased.
//!   - `value == "true"` produces a bare `--key` flag (boolean form).
//!   - everything else produces `--key=value`.
//!   - sort flags alphabetically before joining, so map-iteration order
//!     doesn't leak into the recorded command.
//!   - empty map → no shell-out at all.

use std::sync::Arc;

use tracing::debug;

use crate::console::Console;
use crate::error::Result;
use crate::executor::Plugin;
use crate::schema::Stage;
use crate::vfs::Vfs;

pub fn build() -> Plugin {
    Arc::new(run)
}

/// Render the `systemd_firstboot` map into a single `systemd-firstboot <args>`
/// shell command and run it. Empty map is a no-op.
pub fn run(stage: &Stage, _fs: &dyn Vfs, console: &dyn Console) -> Result<()> {
    if stage.systemd_firstboot.is_empty() {
        return Ok(());
    }

    let mut args: Vec<String> = stage
        .systemd_firstboot
        .iter()
        .map(|(k, v)| {
            let key = k.to_lowercase();
            if v == "true" {
                format!("--{key}")
            } else {
                format!("--{key}={v}")
            }
        })
        .collect();
    args.sort();
    let cmd = format!("systemd-firstboot {}", args.join(" "));
    debug!(cmd = %cmd, "running systemd-firstboot");

    let out = console.run(&cmd)?;
    debug!(output = %out, "systemd-firstboot output");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::console::RecordingConsole;
    use crate::vfs::MemVfs;
    use std::collections::HashMap;

    #[test]
    fn empty_map_no_calls() {
        let stage = Stage::default();
        let fs = MemVfs::new();
        let console = RecordingConsole::new();
        run(&stage, &fs, &console).expect("ok");
        assert!(console.commands().is_empty());
    }

    #[test]
    fn keys_concatenated_into_one_call_alphabetical() {
        let mut m = HashMap::new();
        m.insert("keymap".into(), "us".into());
        m.insert("LOCALE".into(), "en_US.UTF-8".into());
        m.insert("force".into(), "true".into());
        let stage = Stage {
            systemd_firstboot: m,
            ..Default::default()
        };
        let fs = MemVfs::new();
        let console = RecordingConsole::new();
        run(&stage, &fs, &console).expect("ok");

        let cmds = console.commands();
        assert_eq!(cmds.len(), 1);
        // --force is the bool form; --locale and --keymap take values; sorted.
        assert_eq!(
            cmds[0],
            "systemd-firstboot --force --keymap=us --locale=en_US.UTF-8"
        );
    }

    #[test]
    fn two_keys_one_call() {
        let mut m = HashMap::new();
        m.insert("hostname".into(), "host1".into());
        m.insert("timezone".into(), "UTC".into());
        let stage = Stage {
            systemd_firstboot: m,
            ..Default::default()
        };
        let fs = MemVfs::new();
        let console = RecordingConsole::new();
        run(&stage, &fs, &console).expect("ok");
        assert_eq!(
            console.commands(),
            vec!["systemd-firstboot --hostname=host1 --timezone=UTC".to_string()]
        );
    }

    #[test]
    fn shellout_error_propagates() {
        let mut m = HashMap::new();
        m.insert("keymap".into(), "us".into());
        let stage = Stage {
            systemd_firstboot: m,
            ..Default::default()
        };
        let fs = MemVfs::new();
        let console = RecordingConsole::new();
        console.expect("systemd-firstboot --keymap=us", Err("nope".into()));
        let err = run(&stage, &fs, &console).expect_err("propagates");
        assert!(matches!(err, crate::error::Error::Cmd { .. }));
    }
}
