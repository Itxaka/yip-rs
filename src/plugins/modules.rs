//! Port of `pkg/plugins/modules.go`.
//!
//! The Go plugin uses `pault.ag/go/modprobe` (a CGO-free reimplementation of
//! the modprobe machinery) to load kernel modules directly via syscalls. In
//! Rust we shell out to the `modprobe(8)` binary instead — simpler, no
//! kernel-module-loading dependency, and matches what Kairos's initramfs
//! actually has on `$PATH`. Per-module failures are aggregated; one bad
//! module does not abort the rest. Matches Go's `multierror.Append` flow.
//!
//! Go also tries to skip modules already listed in `/proc/modules` so it
//! doesn't issue a redundant load. We let `modprobe` handle that — it's a
//! no-op for already-loaded modules, and skipping the check keeps the
//! shell-out form trivial.

use std::sync::Arc;

use tracing::{debug, warn};

use crate::console::Console;
use crate::error::{Error, Result};
use crate::executor::Plugin;
use crate::schema::Stage;
use crate::vfs::Vfs;

/// Plugin factory. Mirrors the `build()` pattern used by every other plugin.
pub fn build() -> Plugin {
    Arc::new(run)
}

/// Load each module in `stage.modules` by shelling out to `modprobe`.
///
/// Per-module failures are collected into `Error::Multi`; a single failure
/// does not abort subsequent loads (matches Go's `multierror`).
pub fn run(stage: &Stage, _fs: &dyn Vfs, console: &dyn Console) -> Result<()> {
    if stage.modules.is_empty() {
        return Ok(());
    }

    let mut errs: Vec<Error> = Vec::new();
    for m in &stage.modules {
        let cmd = format!("modprobe {m}");
        debug!(module = %m, "loading kernel module");
        match console.run(&cmd) {
            Ok(_) => debug!(module = %m, "module loaded"),
            Err(e) => {
                warn!(module = %m, error = %e, "failed to load module");
                errs.push(e);
            }
        }
    }

    if errs.is_empty() {
        Ok(())
    } else {
        Err(Error::Multi(errs))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::console::RecordingConsole;
    use crate::vfs::MemVfs;

    #[test]
    fn three_modules_three_calls() {
        let stage = Stage {
            modules: vec!["foo".into(), "bar".into(), "baz".into()],
            ..Default::default()
        };
        let fs = MemVfs::new();
        let console = RecordingConsole::new();
        run(&stage, &fs, &console).expect("all default-ok");
        assert_eq!(
            console.commands(),
            vec![
                "modprobe foo".to_string(),
                "modprobe bar".to_string(),
                "modprobe baz".to_string(),
            ]
        );
    }

    #[test]
    fn empty_modules_no_calls() {
        let stage = Stage::default();
        let fs = MemVfs::new();
        let console = RecordingConsole::new();
        run(&stage, &fs, &console).expect("empty is ok");
        assert!(console.commands().is_empty());
    }

    #[test]
    fn one_failure_others_still_run_and_multi_returned() {
        let stage = Stage {
            modules: vec!["a".into(), "b".into(), "c".into()],
            ..Default::default()
        };
        let fs = MemVfs::new();
        let console = RecordingConsole::new();
        console.expect("modprobe b", Err("boom".to_string()));

        let err = run(&stage, &fs, &console).expect_err("b fails");
        match err {
            Error::Multi(errs) => assert_eq!(errs.len(), 1),
            other => panic!("expected Error::Multi, got {other:?}"),
        }
        // All three still attempted.
        assert_eq!(
            console.commands(),
            vec![
                "modprobe a".to_string(),
                "modprobe b".to_string(),
                "modprobe c".to_string(),
            ]
        );
    }
}
