//! Port of `pkg/plugins/systemctl.go`.
//!
//! Drives `systemctl` for enable/disable/start/mask lists plus drop-in
//! override files. Each list is iterated and one shell-out per service is
//! recorded (mirrors Go's `console.RunTemplate`). Override files are
//! written under `/etc/systemd/system/<service>.d/<name>` — defaulting to
//! `override-yip.conf` when no name is given, matching Go's
//! `DefaultOverrideName`.
//!
//! Difference from Go: after writing overrides we additionally run
//! `systemctl daemon-reload` so the unit-d drop-ins take effect without
//! the caller needing to remember. The reload is fired exactly once,
//! even with multiple overrides, and only when at least one override was
//! actually written (skipped/invalid overrides don't trigger it).
//!
//! Per-service / per-override failures aggregate into `Error::Multi`.

use std::path::PathBuf;
use std::sync::Arc;

use tracing::{debug, warn};

use crate::console::Console;
use crate::error::{Error, Result};
use crate::executor::Plugin;
use crate::schema::Stage;
use crate::vfs::Vfs;

const DEFAULT_OVERRIDE_NAME: &str = "override-yip.conf";
const SERVICE_EXT: &str = ".service";
const CONF_EXT: &str = ".conf";

const ERR_EMPTY_OVERRIDE_SERVICE: &str = "Skipping empty override service";
const ERR_EMPTY_OVERRIDE_CONTENT: &str = "Empty override content";

pub fn build() -> Plugin {
    Arc::new(run)
}

/// Drive systemctl enable/disable/start/mask and write override drop-ins.
pub fn run(stage: &Stage, fs: &dyn Vfs, console: &dyn Console) -> Result<()> {
    let mut errs: Vec<Error> = Vec::new();

    run_list(console, &stage.systemctl.enable, "systemctl enable", &mut errs);
    run_list(console, &stage.systemctl.disable, "systemctl disable", &mut errs);
    run_list(console, &stage.systemctl.mask, "systemctl mask", &mut errs);
    run_list(console, &stage.systemctl.start, "systemctl start", &mut errs);

    let mut wrote_any_override = false;
    for ov in &stage.systemctl.overrides {
        if ov.service.is_empty() {
            warn!("{ERR_EMPTY_OVERRIDE_SERVICE}");
            continue;
        }
        if ov.content.is_empty() {
            warn!(service = %ov.service, "{ERR_EMPTY_OVERRIDE_CONTENT}");
            continue;
        }

        // Default the drop-in filename and ensure it has a .conf extension.
        let mut name = if ov.name.is_empty() {
            DEFAULT_OVERRIDE_NAME.to_string()
        } else {
            ov.name.clone()
        };
        if !name.ends_with(CONF_EXT) {
            name.push_str(CONF_EXT);
        }

        // Service entries can be supplied without the .service suffix.
        let mut service = ov.service.clone();
        if !service.ends_with(SERVICE_EXT) {
            service.push_str(SERVICE_EXT);
        }

        let override_dir = PathBuf::from(format!("/etc/systemd/system/{service}.d"));
        if let Err(e) = fs.mkdir_all(&override_dir) {
            warn!(dir = %override_dir.display(), error = %e, "failed to create override dir");
            errs.push(e);
            continue;
        }

        let override_path = override_dir.join(&name);
        debug!(path = %override_path.display(), "writing systemd override");
        if let Err(e) = fs.write(&override_path, ov.content.as_bytes()) {
            warn!(path = %override_path.display(), error = %e, "failed to write override");
            errs.push(e);
            continue;
        }
        wrote_any_override = true;
    }

    // Fire one daemon-reload covering every override we wrote.
    if wrote_any_override {
        if let Err(e) = console.run("systemctl daemon-reload") {
            warn!(error = %e, "systemctl daemon-reload failed");
            errs.push(e);
        }
    }

    if errs.is_empty() {
        Ok(())
    } else {
        Err(Error::Multi(errs))
    }
}

/// Iterate `services`, shell out `<prefix> <svc>` for each, collect errors.
fn run_list(console: &dyn Console, services: &[String], prefix: &str, errs: &mut Vec<Error>) {
    for svc in services {
        let cmd = format!("{prefix} {svc}");
        if let Err(e) = console.run(&cmd) {
            warn!(cmd = %cmd, error = %e, "systemctl call failed");
            errs.push(e);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::console::RecordingConsole;
    use crate::schema::systemctl::{Systemctl, SystemctlOverride};
    use crate::vfs::MemVfs;
    use std::path::Path;

    #[test]
    fn enable_list_emits_two_calls() {
        let stage = Stage {
            systemctl: Systemctl {
                enable: vec!["a".into(), "b".into()],
                ..Default::default()
            },
            ..Default::default()
        };
        let fs = MemVfs::new();
        let console = RecordingConsole::new();
        run(&stage, &fs, &console).expect("all ok");
        assert_eq!(
            console.commands(),
            vec![
                "systemctl enable a".to_string(),
                "systemctl enable b".to_string(),
            ]
        );
    }

    #[test]
    fn all_four_lists_emit_calls_in_documented_order() {
        let stage = Stage {
            systemctl: Systemctl {
                enable: vec!["e1".into()],
                disable: vec!["d1".into()],
                mask: vec!["m1".into()],
                start: vec!["s1".into()],
                ..Default::default()
            },
            ..Default::default()
        };
        let fs = MemVfs::new();
        let console = RecordingConsole::new();
        run(&stage, &fs, &console).expect("ok");
        // Order: enable, disable, mask, start (matches Go).
        assert_eq!(
            console.commands(),
            vec![
                "systemctl enable e1".to_string(),
                "systemctl disable d1".to_string(),
                "systemctl mask m1".to_string(),
                "systemctl start s1".to_string(),
            ]
        );
    }

    #[test]
    fn override_writes_file_at_expected_drop_in_path() {
        let stage = Stage {
            systemctl: Systemctl {
                overrides: vec![SystemctlOverride {
                    service: "foo.service".into(),
                    content: "[Service]\nRestart=always".into(),
                    name: String::new(),
                }],
                ..Default::default()
            },
            ..Default::default()
        };
        let fs = MemVfs::new();
        let console = RecordingConsole::new();
        run(&stage, &fs, &console).expect("ok");

        let path = Path::new("/etc/systemd/system/foo.service.d/override-yip.conf");
        assert!(fs.exists(path));
        assert_eq!(
            fs.read_to_string(path).unwrap(),
            "[Service]\nRestart=always"
        );
    }

    #[test]
    fn override_auto_appends_service_ext() {
        let stage = Stage {
            systemctl: Systemctl {
                overrides: vec![SystemctlOverride {
                    service: "foo".into(),
                    content: "x".into(),
                    name: String::new(),
                }],
                ..Default::default()
            },
            ..Default::default()
        };
        let fs = MemVfs::new();
        let console = RecordingConsole::new();
        run(&stage, &fs, &console).expect("ok");
        assert!(fs.exists(Path::new("/etc/systemd/system/foo.service.d/override-yip.conf")));
    }

    #[test]
    fn override_custom_name_without_ext_gets_conf_appended() {
        let stage = Stage {
            systemctl: Systemctl {
                overrides: vec![SystemctlOverride {
                    service: "foo.service".into(),
                    content: "x".into(),
                    name: "custom".into(),
                }],
                ..Default::default()
            },
            ..Default::default()
        };
        let fs = MemVfs::new();
        let console = RecordingConsole::new();
        run(&stage, &fs, &console).expect("ok");
        assert!(fs.exists(Path::new("/etc/systemd/system/foo.service.d/custom.conf")));
    }

    #[test]
    fn override_empty_service_is_skipped() {
        let stage = Stage {
            systemctl: Systemctl {
                overrides: vec![SystemctlOverride {
                    service: String::new(),
                    content: "x".into(),
                    name: String::new(),
                }],
                ..Default::default()
            },
            ..Default::default()
        };
        let fs = MemVfs::new();
        let console = RecordingConsole::new();
        run(&stage, &fs, &console).expect("ok — empty service is a warn, not an err");
        // No daemon-reload either: nothing was written.
        assert!(console.commands().is_empty());
        assert!(!fs.exists(Path::new("/etc/systemd/system")));
    }

    #[test]
    fn override_empty_content_is_skipped() {
        let stage = Stage {
            systemctl: Systemctl {
                overrides: vec![SystemctlOverride {
                    service: "foo.service".into(),
                    content: String::new(),
                    name: String::new(),
                }],
                ..Default::default()
            },
            ..Default::default()
        };
        let fs = MemVfs::new();
        let console = RecordingConsole::new();
        run(&stage, &fs, &console).expect("ok");
        assert!(console.commands().is_empty());
        assert!(!fs.exists(Path::new("/etc/systemd/system/foo.service.d/override-yip.conf")));
    }

    #[test]
    fn daemon_reload_fires_exactly_once_with_multiple_overrides() {
        let stage = Stage {
            systemctl: Systemctl {
                overrides: vec![
                    SystemctlOverride {
                        service: "a.service".into(),
                        content: "x".into(),
                        name: String::new(),
                    },
                    SystemctlOverride {
                        service: "b.service".into(),
                        content: "y".into(),
                        name: String::new(),
                    },
                ],
                ..Default::default()
            },
            ..Default::default()
        };
        let fs = MemVfs::new();
        let console = RecordingConsole::new();
        run(&stage, &fs, &console).expect("ok");
        let reloads = console
            .commands()
            .into_iter()
            .filter(|c| c == "systemctl daemon-reload")
            .count();
        assert_eq!(reloads, 1);
    }

    #[test]
    fn daemon_reload_skipped_when_no_overrides() {
        let stage = Stage {
            systemctl: Systemctl {
                enable: vec!["x".into()],
                ..Default::default()
            },
            ..Default::default()
        };
        let fs = MemVfs::new();
        let console = RecordingConsole::new();
        run(&stage, &fs, &console).expect("ok");
        assert!(!console
            .commands()
            .iter()
            .any(|c| c == "systemctl daemon-reload"));
    }

    #[test]
    fn enable_failure_aggregates_into_multi() {
        let stage = Stage {
            systemctl: Systemctl {
                enable: vec!["good".into(), "bad".into()],
                ..Default::default()
            },
            ..Default::default()
        };
        let fs = MemVfs::new();
        let console = RecordingConsole::new();
        console.expect("systemctl enable bad", Err("nope".to_string()));
        let err = run(&stage, &fs, &console).expect_err("bad fails");
        match err {
            Error::Multi(errs) => assert_eq!(errs.len(), 1),
            other => panic!("expected Multi, got {other:?}"),
        }
        // Both attempted.
        assert_eq!(console.commands().len(), 2);
    }
}
