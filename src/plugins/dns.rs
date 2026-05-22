//! `dns` plugin — port of `pkg/plugins/dns.go`.
//!
//! Writes a `/etc/resolv.conf`-style file. Default output path is
//! `/etc/resolv.conf`, overridable via `stage.dns.path`.
//!
//! Format (matches Go `Build`):
//!   - One `search <space-joined-domains>` line if `dns_search` is set and
//!     joined value is not just ".".
//!   - One `nameserver X` line per entry in `nameservers`.
//!   - One `options <space-joined-options>` line if `dns_options` is set and
//!     joined value is non-empty (after trim).
//!
//! Go's `DNS` plugin actually short-circuits on `len(Nameservers) == 0`
//! (search/options alone do not trigger a write); we replicate that for
//! compatibility, even though the task description allows search/options
//! alone.

use std::path::Path;
use std::sync::Arc;

use tracing::{debug, info};

use crate::console::Console;
use crate::error::Result;
use crate::executor::Plugin;
use crate::schema::Stage;
use crate::vfs::Vfs;

const DEFAULT_RESOLV_CONF: &str = "/etc/resolv.conf";

/// Build the plugin closure for registration with the executor.
pub fn build() -> Plugin {
    Arc::new(run)
}

/// Apply the DNS plugin against `stage`.
pub fn run(stage: &Stage, fs: &dyn Vfs, _console: &dyn Console) -> Result<()> {
    let dns = &stage.dns;
    // Match Go: only nameservers gate the write.
    if dns.nameservers.is_empty() {
        debug!("dns: no nameservers, skipping");
        return Ok(());
    }

    let path = if dns.path.is_empty() {
        DEFAULT_RESOLV_CONF
    } else {
        dns.path.as_str()
    };

    let content = render_resolv_conf(&dns.nameservers, &dns.dns_search, &dns.dns_options);
    fs.write(Path::new(path), content.as_bytes())?;
    info!(path, "wrote resolv.conf");
    Ok(())
}

/// Pure renderer — same line ordering as Go's `Build`: search, nameservers, options.
fn render_resolv_conf(nameservers: &[String], search: &[String], options: &[String]) -> String {
    let mut buf = String::new();
    if !search.is_empty() {
        let joined = search.join(" ");
        if joined.trim() != "." {
            buf.push_str("search ");
            buf.push_str(&joined);
            buf.push('\n');
        }
    }
    for ns in nameservers {
        buf.push_str("nameserver ");
        buf.push_str(ns);
        buf.push('\n');
    }
    if !options.is_empty() {
        let joined = options.join(" ");
        if !joined.trim().is_empty() {
            buf.push_str("options ");
            buf.push_str(&joined);
            buf.push('\n');
        }
    }
    buf
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::console::RecordingConsole;
    use crate::schema::dns::DNS;
    use crate::vfs::MemVfs;

    fn stage_with_dns(dns: DNS) -> Stage {
        Stage {
            dns,
            ..Default::default()
        }
    }

    #[test]
    fn empty_dns_does_not_write() {
        let fs = MemVfs::new();
        let console = RecordingConsole::new();
        let stage = Stage::default();
        run(&stage, &fs, &console).unwrap();
        assert!(!fs.exists(Path::new(DEFAULT_RESOLV_CONF)));
    }

    #[test]
    fn search_only_does_not_write_matches_go() {
        // Go's DNS() gates on len(Nameservers) == 0; search-only is a no-op.
        let fs = MemVfs::new();
        let console = RecordingConsole::new();
        let stage = stage_with_dns(DNS {
            dns_search: vec!["example.com".into()],
            ..Default::default()
        });
        run(&stage, &fs, &console).unwrap();
        assert!(!fs.exists(Path::new(DEFAULT_RESOLV_CONF)));
    }

    #[test]
    fn writes_single_nameserver_default_path() {
        let fs = MemVfs::new();
        let console = RecordingConsole::new();
        let stage = stage_with_dns(DNS {
            nameservers: vec!["8.8.8.8".into()],
            ..Default::default()
        });
        run(&stage, &fs, &console).unwrap();
        let got = fs.read_to_string(Path::new(DEFAULT_RESOLV_CONF)).unwrap();
        assert_eq!(got, "nameserver 8.8.8.8\n");
    }

    #[test]
    fn writes_two_nameservers_produces_two_lines() {
        let fs = MemVfs::new();
        let console = RecordingConsole::new();
        let stage = stage_with_dns(DNS {
            nameservers: vec!["8.8.8.8".into(), "1.1.1.1".into()],
            ..Default::default()
        });
        run(&stage, &fs, &console).unwrap();
        let got = fs.read_to_string(Path::new(DEFAULT_RESOLV_CONF)).unwrap();
        let ns_lines: Vec<_> = got.lines().filter(|l| l.starts_with("nameserver ")).collect();
        assert_eq!(ns_lines, vec!["nameserver 8.8.8.8", "nameserver 1.1.1.1"]);
    }

    #[test]
    fn writes_full_search_and_options() {
        let fs = MemVfs::new();
        let console = RecordingConsole::new();
        let stage = stage_with_dns(DNS {
            nameservers: vec!["8.8.8.8".into()],
            dns_search: vec!["a.example".into(), "b.example".into()],
            dns_options: vec!["ndots:2".into(), "timeout:1".into()],
            ..Default::default()
        });
        run(&stage, &fs, &console).unwrap();
        let got = fs.read_to_string(Path::new(DEFAULT_RESOLV_CONF)).unwrap();
        assert_eq!(
            got,
            "search a.example b.example\nnameserver 8.8.8.8\noptions ndots:2 timeout:1\n"
        );
    }

    #[test]
    fn respects_custom_path() {
        let fs = MemVfs::new();
        let console = RecordingConsole::new();
        let stage = stage_with_dns(DNS {
            nameservers: vec!["1.1.1.1".into()],
            path: "/tmp/myresolv".into(),
            ..Default::default()
        });
        run(&stage, &fs, &console).unwrap();
        assert!(!fs.exists(Path::new(DEFAULT_RESOLV_CONF)));
        assert_eq!(
            fs.read_to_string(Path::new("/tmp/myresolv")).unwrap(),
            "nameserver 1.1.1.1\n"
        );
    }

    #[test]
    fn search_dot_is_dropped() {
        // Go's special case: joined search == "." is omitted.
        let out = render_resolv_conf(&["8.8.8.8".into()], &[".".into()], &[]);
        assert_eq!(out, "nameserver 8.8.8.8\n");
    }

    #[test]
    fn empty_option_string_is_dropped() {
        let out = render_resolv_conf(&["8.8.8.8".into()], &[], &["".into()]);
        assert_eq!(out, "nameserver 8.8.8.8\n");
    }

    #[test]
    fn build_returns_callable_plugin() {
        let plugin = build();
        let fs = MemVfs::new();
        let console = RecordingConsole::new();
        let stage = stage_with_dns(DNS {
            nameservers: vec!["9.9.9.9".into()],
            ..Default::default()
        });
        plugin(&stage, &fs, &console).unwrap();
        assert_eq!(
            fs.read_to_string(Path::new(DEFAULT_RESOLV_CONF)).unwrap(),
            "nameserver 9.9.9.9\n"
        );
    }
}
