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

    // -------------------------------------------------------------------
    // Ported from Go: every documented key, true→flag, empty value edge.
    // -------------------------------------------------------------------

    #[test]
    fn all_documented_keys_combine_into_sorted_single_call() {
        // locale, keymap, hostname, timezone, root-password, root-password-hashed
        // all collapse into ONE shell-out, alphabetically.
        let mut m = HashMap::new();
        m.insert("locale".into(), "en_US.UTF-8".into());
        m.insert("keymap".into(), "us".into());
        m.insert("hostname".into(), "host1".into());
        m.insert("timezone".into(), "UTC".into());
        m.insert("root-password".into(), "secret".into());
        m.insert("root-password-hashed".into(), "$6$hashed".into());

        let stage = Stage {
            systemd_firstboot: m,
            ..Default::default()
        };
        let fs = MemVfs::new();
        let console = RecordingConsole::default();
        run(&stage, &fs, &console).expect("ok");

        let cmds = console.commands();
        assert_eq!(cmds.len(), 1, "exactly one shell-out");
        // Alphabetically sorted by the rendered "--key" / "--key=val" form.
        assert_eq!(
            cmds[0],
            "systemd-firstboot --hostname=host1 \
             --keymap=us \
             --locale=en_US.UTF-8 \
             --root-password-hashed=$6$hashed \
             --root-password=secret \
             --timezone=UTC"
        );
    }

    #[test]
    fn value_true_emits_bare_flag_not_key_value() {
        // value=="true" → bare `--key` form (boolean), not `--key=true`.
        let mut m = HashMap::new();
        m.insert("force".into(), "true".into());
        m.insert("prompt-keymap".into(), "true".into());
        let stage = Stage {
            systemd_firstboot: m,
            ..Default::default()
        };
        let fs = MemVfs::new();
        let console = RecordingConsole::default();
        run(&stage, &fs, &console).expect("ok");
        let cmds = console.commands();
        assert_eq!(cmds.len(), 1);
        assert!(cmds[0].contains(" --force "));
        assert!(cmds[0].ends_with(" --prompt-keymap") || cmds[0].contains(" --prompt-keymap "));
        // Never the long form for these.
        assert!(!cmds[0].contains("--force=true"));
        assert!(!cmds[0].contains("--prompt-keymap=true"));
    }

    // -------------------------------------------------------------------
    // Direct port of the Go `It` block in pkg/plugins/systemd_firstboot_test.go.
    // -------------------------------------------------------------------

    /// Go: "sets first-boot configuration"
    #[test]
    fn go_port_sets_first_boot_configuration() {
        let fs = MemVfs::new();
        let console = RecordingConsole::new();

        let mut m = HashMap::new();
        m.insert("keymap".into(), "us".into());
        m.insert("LOCALE".into(), "en_US.UTF-8".into());
        m.insert("force".into(), "true".into());
        let stage = Stage {
            systemd_firstboot: m,
            ..Default::default()
        };
        run(&stage, &fs, &console).expect("err nil");

        let cmds = console.commands();
        // Go: ContainElements("systemd-firstboot --force --keymap=us --locale=en_US.UTF-8")
        // and len == 1.
        assert_eq!(cmds.len(), 1);
        assert!(
            cmds.contains(&"systemd-firstboot --force --keymap=us --locale=en_US.UTF-8".to_string()),
            "expected command in {cmds:?}"
        );
    }

    #[test]
    fn empty_value_emits_key_equals_empty() {
        // Edge: value is "" (not "true") — emits --key= with nothing on the RHS.
        let mut m = HashMap::new();
        m.insert("keymap".into(), String::new());
        let stage = Stage {
            systemd_firstboot: m,
            ..Default::default()
        };
        let fs = MemVfs::new();
        let console = RecordingConsole::default();
        run(&stage, &fs, &console).expect("ok");
        assert_eq!(
            console.commands(),
            vec!["systemd-firstboot --keymap=".to_string()]
        );
    }
}
