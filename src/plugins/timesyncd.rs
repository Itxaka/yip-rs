//! Port of `pkg/plugins/timesyncd.go`.
//!
//! Renders the stage's `timesyncd` map as an INI-format `[Time]` section
//! and writes it to `/etc/systemd/timesyncd.conf.d/10-yip.conf` (a drop-in
//! file that systemd-timesyncd merges on top of the distro-shipped
//! `/etc/systemd/timesyncd.conf`). The Go plugin writes to the same
//! drop-in path via `gopkg.in/ini.v1` (load → merge → save), so the
//! pre-existing keys outside the touched ones are preserved.
//!
//! This Rust port intentionally writes the file directly with a
//! deterministic layout (alphabetically sorted keys, `KEY=VALUE` with no
//! surrounding whitespace) and overwrites whatever is at the target path.
//! yip's schema always ships the full intended `[Time]` block, so merging
//! on top of a partial file isn't useful in practice, and a stable
//! rendering is easier to diff / audit / test against.
//!
//! Behaviour summary:
//!   - empty map → no write at all.
//!   - non-empty map → ensure `/etc/systemd/timesyncd.conf.d/` exists,
//!     then write `/etc/systemd/timesyncd.conf.d/10-yip.conf` with
//!     `[Time]\n<key>=<value>\n...` using alphabetically-sorted keys.

use std::path::Path;
use std::sync::Arc;

use tracing::debug;

use crate::console::Console;
use crate::error::Result;
use crate::executor::Plugin;
use crate::schema::Stage;
use crate::vfs::Vfs;

/// Target drop-in path. Matches the Go plugin which writes to the
/// `.conf.d/` directory rather than the top-level config — this preserves
/// distro-shipped defaults that systemd-timesyncd reads via its standard
/// drop-in mechanism.
const TIMESYNCD_PATH: &str = "/etc/systemd/timesyncd.conf.d/10-yip.conf";

pub fn build() -> Plugin {
    Arc::new(run)
}

/// Write `[Time]` config from `stage.timesyncd` to the timesyncd drop-in
/// file. Creates the parent directory if it doesn't already exist.
pub fn run(stage: &Stage, fs: &dyn Vfs, _console: &dyn Console) -> Result<()> {
    if stage.timesyncd.is_empty() {
        return Ok(());
    }

    // Collect, sort, render alphabetically for determinism.
    let mut keys: Vec<&String> = stage.timesyncd.keys().collect();
    keys.sort();

    let mut out = String::from("[Time]\n");
    for k in keys {
        let v = &stage.timesyncd[k];
        out.push_str(k);
        out.push('=');
        out.push_str(v);
        out.push('\n');
    }

    // Make sure the .conf.d/ directory exists before writing.
    let target = Path::new(TIMESYNCD_PATH);
    if let Some(parent) = target.parent() {
        if !parent.as_os_str().is_empty() {
            debug!(parent = %parent.display(), "ensuring timesyncd drop-in dir");
            fs.mkdir_all(parent)?;
        }
    }

    debug!(path = TIMESYNCD_PATH, bytes = out.len(), "writing timesyncd config");
    fs.write(target, out.as_bytes())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::console::RecordingConsole;
    use crate::vfs::MemVfs;
    use std::collections::HashMap;

    #[test]
    fn two_keys_render_alphabetically() {
        let mut m = HashMap::new();
        m.insert("NTP".into(), "time.example.com".into());
        m.insert("FallbackNTP".into(), "fallback.example.com".into());
        let stage = Stage {
            timesyncd: m,
            ..Default::default()
        };
        let fs = MemVfs::new();
        let console = RecordingConsole::new();
        run(&stage, &fs, &console).expect("ok");

        let content = fs.read_to_string(Path::new(TIMESYNCD_PATH)).unwrap();
        // Alphabetical: FallbackNTP < NTP (uppercase < lowercase in ASCII, both
        // upper here so simple lexicographic order applies).
        assert_eq!(
            content,
            "[Time]\nFallbackNTP=fallback.example.com\nNTP=time.example.com\n"
        );
    }

    #[test]
    fn empty_map_no_write() {
        let stage = Stage::default();
        let fs = MemVfs::new();
        let console = RecordingConsole::new();
        run(&stage, &fs, &console).expect("ok");
        assert!(!fs.exists(Path::new(TIMESYNCD_PATH)));
    }

    #[test]
    fn existing_file_is_overwritten() {
        let fs = MemVfs::new();
        // Seed a pre-existing file with unrelated content at the drop-in path.
        fs.mkdir_all(Path::new("/etc/systemd/timesyncd.conf.d"))
            .unwrap();
        fs.write(
            Path::new(TIMESYNCD_PATH),
            b"[Time]\nNTP=old.example.com\nLeftover=keep_me\n",
        )
        .unwrap();

        let mut m = HashMap::new();
        m.insert("NTP".into(), "new.example.com".into());
        let stage = Stage {
            timesyncd: m,
            ..Default::default()
        };
        let console = RecordingConsole::new();
        run(&stage, &fs, &console).expect("ok");

        let content = fs.read_to_string(Path::new(TIMESYNCD_PATH)).unwrap();
        // Old `Leftover` key is gone — file was overwritten, not merged.
        assert_eq!(content, "[Time]\nNTP=new.example.com\n");
        assert!(!content.contains("Leftover"));
    }

    #[test]
    fn renders_time_header_exactly_once() {
        let mut m = HashMap::new();
        m.insert("NTP".into(), "x".into());
        m.insert("RootDistanceMaxSec".into(), "5".into());
        let stage = Stage {
            timesyncd: m,
            ..Default::default()
        };
        let fs = MemVfs::new();
        let console = RecordingConsole::new();
        run(&stage, &fs, &console).expect("ok");
        let content = fs.read_to_string(Path::new(TIMESYNCD_PATH)).unwrap();
        let header_count = content.matches("[Time]").count();
        assert_eq!(header_count, 1);
        // 1 header line + 2 key=value lines + trailing newline.
        assert_eq!(content.lines().count(), 3);
    }

    // -------------------------------------------------------------------
    // Ported from Go: multi-line config rendering, empty-section no-write.
    // -------------------------------------------------------------------

    #[test]
    fn multi_line_config_renders_each_key_on_its_own_line() {
        let mut m = HashMap::new();
        m.insert("NTP".into(), "0.pool.example".into());
        m.insert("FallbackNTP".into(), "1.pool.example 2.pool.example".into());
        m.insert("PollIntervalMaxSec".into(), "2048".into());
        m.insert("PollIntervalMinSec".into(), "32".into());
        m.insert("RootDistanceMaxSec".into(), "5".into());
        let stage = Stage {
            timesyncd: m,
            ..Default::default()
        };
        let fs = MemVfs::new();
        let console = RecordingConsole::default();
        run(&stage, &fs, &console).expect("ok");
        let content = fs.read_to_string(Path::new(TIMESYNCD_PATH)).unwrap();
        // 1 header + 5 keys + trailing newline -> 6 lines.
        assert_eq!(content.lines().count(), 6);
        // Alphabetical key order.
        let expected = "[Time]\n\
                        FallbackNTP=1.pool.example 2.pool.example\n\
                        NTP=0.pool.example\n\
                        PollIntervalMaxSec=2048\n\
                        PollIntervalMinSec=32\n\
                        RootDistanceMaxSec=5\n";
        assert_eq!(content, expected);
    }

    #[test]
    fn empty_time_section_does_not_create_file_even_if_dir_exists() {
        let fs = MemVfs::new();
        let console = RecordingConsole::default();
        // Pre-create the parent dir to make sure absence of file is purely
        // due to empty map, not an mkdir failure.
        fs.mkdir_all(Path::new("/etc/systemd/timesyncd.conf.d"))
            .unwrap();
        let stage = Stage {
            timesyncd: HashMap::new(),
            ..Default::default()
        };
        run(&stage, &fs, &console).expect("ok");
        assert!(!fs.exists(Path::new(TIMESYNCD_PATH)));
    }

    #[test]
    fn single_ntp_entry_matches_go_default_test() {
        // Mirrors the Go timesyncd_test "configures timesyncd" case (a single
        // NTP pool entry). Sanity-check that the simplest config produces
        // exactly the [Time] header plus one key=value line.
        let mut m = HashMap::new();
        m.insert("NTP".into(), "0.pool".into());
        let stage = Stage {
            timesyncd: m,
            ..Default::default()
        };
        let fs = MemVfs::new();
        let console = RecordingConsole::default();
        run(&stage, &fs, &console).expect("ok");
        let content = fs.read_to_string(Path::new(TIMESYNCD_PATH)).unwrap();
        assert_eq!(content, "[Time]\nNTP=0.pool\n");
    }

    // -------------------------------------------------------------------
    // New tests asserting the drop-in path / mkdir_all behaviour.
    // -------------------------------------------------------------------

    #[test]
    fn writes_to_conf_d_drop_in_path_not_top_level_conf() {
        // Regression guard: the top-level /etc/systemd/timesyncd.conf must
        // be left alone. Only the .conf.d/10-yip.conf drop-in should land.
        let mut m = HashMap::new();
        m.insert("NTP".into(), "x.example".into());
        let stage = Stage {
            timesyncd: m,
            ..Default::default()
        };
        let fs = MemVfs::new();
        let console = RecordingConsole::default();
        run(&stage, &fs, &console).expect("ok");
        assert!(fs.exists(Path::new(
            "/etc/systemd/timesyncd.conf.d/10-yip.conf"
        )));
        assert!(!fs.exists(Path::new("/etc/systemd/timesyncd.conf")));
    }

    #[test]
    fn parent_conf_d_dir_is_created_when_missing() {
        // The plugin must mkdir_all the .conf.d/ parent — even on a host
        // that doesn't already have it. MemVfs starts empty.
        let mut m = HashMap::new();
        m.insert("NTP".into(), "a".into());
        let stage = Stage {
            timesyncd: m,
            ..Default::default()
        };
        let fs = MemVfs::new();
        let console = RecordingConsole::default();
        assert!(!fs.exists(Path::new("/etc/systemd/timesyncd.conf.d")));
        run(&stage, &fs, &console).expect("ok");
        assert!(fs.exists(Path::new("/etc/systemd/timesyncd.conf.d")));
        assert!(fs.exists(Path::new(TIMESYNCD_PATH)));
    }
}
