//! `OnlyIfOS` conditional — regex-match `stage.only_if_os` against the
//! `NAME=` field of `/etc/os-release`.
//!
//! Mirrors `OnlyIfOS` in Go yip (`pkg/plugins/if_os.go`):
//!   - empty `only_os` → run.
//!   - non-empty → compile as regex, read NAME from /etc/os-release, run
//!     if it matches.
//!   - any failure on the read path or regex side is logged and treated
//!     as Skip. The Go version surfaces these as errors but the executor
//!     coerces conditional errors to "skip"; we collapse that here so the
//!     control flow is explicit.
//!
//! We deliberately read `/etc/os-release` through the [`Vfs`] (rather than
//! reusing `crate::template::sysdata::parse_os_release`, which goes
//! straight to `std::fs`) so unit tests can stub the file with `MemVfs`.

use std::path::Path;
use std::sync::Arc;

use tracing::warn;

use crate::console::Console;
use crate::error::Result;
use crate::executor::{Conditional, ConditionalOutcome};
use crate::schema::Stage;
use crate::vfs::Vfs;

const OS_RELEASE_PATH: &str = "/etc/os-release";

/// Build the conditional fn pointer registered by the executor.
pub fn build() -> Conditional {
    Arc::new(check)
}

/// The actual decision function. Pulled out so it can be unit-tested
/// against a `MemVfs` without going through `Arc<dyn Fn>`.
pub fn check(
    stage: &Stage,
    fs: &dyn Vfs,
    _console: &dyn Console,
) -> Result<ConditionalOutcome> {
    // Empty `only_os` is the "no filter, always run" case (Go: outer `if
    // s.OnlyIfOs != ""` returns nil).
    if stage.only_if_os.is_empty() {
        return Ok(ConditionalOutcome::Run);
    }

    // Compile the regex first; a bad pattern in the YAML is a user error
    // and should not be silently treated as "match nothing". Go returns
    // the regex error; the executor turns it into Skip. We log + Skip
    // directly so the warning is surfaced once at the source.
    let re = match regex::Regex::new(&stage.only_if_os) {
        Ok(r) => r,
        Err(e) => {
            warn!(
                only_os = stage.only_if_os.as_str(),
                error = %e,
                "OnlyIfOS: invalid regex, skipping stage",
            );
            return Ok(ConditionalOutcome::Skip);
        }
    };

    // Read /etc/os-release via the Vfs so tests can mock it. Missing or
    // unreadable file → no match → Skip (matches Go behaviour where an
    // empty os name causes the function to return an error which the
    // executor coerces to Skip).
    let body = match fs.read_to_string(Path::new(OS_RELEASE_PATH)) {
        Ok(s) => s,
        Err(e) => {
            warn!(
                path = OS_RELEASE_PATH,
                error = %e,
                "OnlyIfOS: cannot read os-release, skipping stage",
            );
            return Ok(ConditionalOutcome::Skip);
        }
    };

    let name = parse_name(&body);
    if name.is_empty() {
        warn!(
            only_os = stage.only_if_os.as_str(),
            "OnlyIfOS: NAME field missing from os-release, skipping stage",
        );
        return Ok(ConditionalOutcome::Skip);
    }

    if re.is_match(&name) {
        Ok(ConditionalOutcome::Run)
    } else {
        Ok(ConditionalOutcome::Skip)
    }
}

/// Pull the `NAME=` value out of an `/etc/os-release`-style body. Handles
/// double-quoted, single-quoted, and bare values. Returns an empty string
/// if the key is absent.
///
/// Kept local rather than reused from `template::sysdata` because that
/// module's parser only reads `/etc/os-release` directly via `std::fs`
/// and we want the value to come from the `Vfs` for testability.
fn parse_name(body: &str) -> String {
    for raw in body.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((k, v_raw)) = line.split_once('=') else {
            continue;
        };
        if k.trim() == "NAME" {
            return unquote(v_raw.trim());
        }
    }
    String::new()
}

fn unquote(v: &str) -> String {
    let v = v.trim();
    if v.len() >= 2
        && ((v.starts_with('"') && v.ends_with('"'))
            || (v.starts_with('\'') && v.ends_with('\'')))
    {
        return v[1..v.len() - 1].to_string();
    }
    v.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::console::RecordingConsole;
    use crate::schema::Stage;
    use crate::vfs::MemVfs;

    fn write_os_release(vfs: &MemVfs, body: &str) {
        vfs.write(Path::new(OS_RELEASE_PATH), body.as_bytes()).unwrap();
    }

    fn run(stage: &Stage, vfs: &MemVfs) -> ConditionalOutcome {
        // `check` never touches the console, but the signature requires
        // one. `RecordingConsole` is the standard test mock.
        let console = RecordingConsole::new();
        let outcome = check(stage, vfs, &console).expect("check must not return Err");
        assert!(
            console.commands().is_empty(),
            "OnlyIfOS must not invoke the console",
        );
        outcome
    }

    #[test]
    fn empty_only_os_runs() {
        let vfs = MemVfs::new();
        // No /etc/os-release written: empty filter must not even try to read.
        let stage = Stage::default();
        assert_eq!(run(&stage, &vfs), ConditionalOutcome::Run);
    }

    #[test]
    fn matching_literal_runs() {
        let vfs = MemVfs::new();
        write_os_release(&vfs, "NAME=\"Ubuntu\"\nID=ubuntu\n");
        let stage = Stage {
            only_if_os: "Ubuntu".into(),
            ..Default::default()
        };
        assert_eq!(run(&stage, &vfs), ConditionalOutcome::Run);
    }

    #[test]
    fn non_matching_alternation_skips() {
        let vfs = MemVfs::new();
        write_os_release(&vfs, "NAME=\"Hadron Linux\"\nID=hadron\n");
        let stage = Stage {
            only_if_os: "(Ubuntu|Debian).*".into(),
            ..Default::default()
        };
        assert_eq!(run(&stage, &vfs), ConditionalOutcome::Skip);
    }

    #[test]
    fn matching_prefix_regex_runs() {
        let vfs = MemVfs::new();
        write_os_release(&vfs, "NAME=\"Hadron Linux\"\nID=hadron\n");
        let stage = Stage {
            only_if_os: "Hadron.*".into(),
            ..Default::default()
        };
        assert_eq!(run(&stage, &vfs), ConditionalOutcome::Run);
    }

    #[test]
    fn missing_os_release_skips_without_panic() {
        let vfs = MemVfs::new();
        // intentionally no os-release file
        let stage = Stage {
            only_if_os: "Ubuntu".into(),
            ..Default::default()
        };
        assert_eq!(run(&stage, &vfs), ConditionalOutcome::Skip);
    }

    #[test]
    fn bad_regex_skips_without_panic() {
        let vfs = MemVfs::new();
        write_os_release(&vfs, "NAME=\"Ubuntu\"\n");
        let stage = Stage {
            // unclosed group → regex compile error
            only_if_os: "(unterminated".into(),
            ..Default::default()
        };
        assert_eq!(run(&stage, &vfs), ConditionalOutcome::Skip);
    }

    #[test]
    fn os_release_without_name_field_skips() {
        let vfs = MemVfs::new();
        write_os_release(&vfs, "ID=ubuntu\nVERSION=22.04\n");
        let stage = Stage {
            only_if_os: ".*".into(),
            ..Default::default()
        };
        assert_eq!(run(&stage, &vfs), ConditionalOutcome::Skip);
    }

    #[test]
    fn single_quoted_name_is_unquoted() {
        let vfs = MemVfs::new();
        write_os_release(&vfs, "NAME='Arch Linux'\n");
        let stage = Stage {
            only_if_os: "^Arch Linux$".into(),
            ..Default::default()
        };
        assert_eq!(run(&stage, &vfs), ConditionalOutcome::Run);
    }

    #[test]
    fn bare_name_value_works() {
        let vfs = MemVfs::new();
        write_os_release(&vfs, "NAME=Fedora\n");
        let stage = Stage {
            only_if_os: "Fedora".into(),
            ..Default::default()
        };
        assert_eq!(run(&stage, &vfs), ConditionalOutcome::Run);
    }
}
