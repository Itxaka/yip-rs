//! Port of `pkg/plugins/timesyncd.go`.
//!
//! Renders the stage's `timesyncd` map as an INI-format `[Time]` section
//! and writes it to `/etc/systemd/timesyncd.conf`. The Go plugin uses
//! `gopkg.in/ini.v1` to load → merge → save, which means any
//! pre-existing keys outside the touched ones are preserved.
//!
//! This port intentionally writes the file directly with a deterministic
//! layout (alphabetically sorted keys, `KEY=VALUE` with no surrounding
//! whitespace) and overwrites whatever is at the target path. yip's
//! schema always ships the full intended `[Time]` block, so merging on
//! top of a partial file isn't useful in practice, and a stable rendering
//! is easier to diff / audit / test against.
//!
//! Behaviour summary:
//!   - empty map → no write at all.
//!   - non-empty map → overwrite `/etc/systemd/timesyncd.conf` with
//!     `[Time]\n<key>=<value>\n...` using alphabetically-sorted keys.

use std::path::Path;
use std::sync::Arc;

use tracing::debug;

use crate::console::Console;
use crate::error::Result;
use crate::executor::Plugin;
use crate::schema::Stage;
use crate::vfs::Vfs;

const TIMESYNCD_CONF: &str = "/etc/systemd/timesyncd.conf";

pub fn build() -> Plugin {
    Arc::new(run)
}

/// Write `[Time]` config from `stage.timesyncd` to `/etc/systemd/timesyncd.conf`.
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

    debug!(path = TIMESYNCD_CONF, bytes = out.len(), "writing timesyncd config");
    fs.write(Path::new(TIMESYNCD_CONF), out.as_bytes())?;
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

        let content = fs.read_to_string(Path::new(TIMESYNCD_CONF)).unwrap();
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
        assert!(!fs.exists(Path::new(TIMESYNCD_CONF)));
    }

    #[test]
    fn existing_file_is_overwritten() {
        let fs = MemVfs::new();
        // Seed a pre-existing file with unrelated content.
        fs.write(
            Path::new(TIMESYNCD_CONF),
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

        let content = fs.read_to_string(Path::new(TIMESYNCD_CONF)).unwrap();
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
        let content = fs.read_to_string(Path::new(TIMESYNCD_CONF)).unwrap();
        let header_count = content.matches("[Time]").count();
        assert_eq!(header_count, 1);
        // 1 header line + 2 key=value lines + trailing newline.
        assert_eq!(content.lines().count(), 3);
    }
}
