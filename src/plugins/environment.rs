//! `environment` plugin — port of `pkg/plugins/environment.go`.
//!
//! Maintains an `/etc/environment`-style KEY=VALUE file. Existing file
//! contents (if any) are parsed, the stage's `environment` map is merged
//! in (new values override existing keys), and the result is rewritten
//! sorted by key. Quoting matches `godotenv.Write`: a value is wrapped in
//! double quotes when it contains whitespace or shell-special characters,
//! otherwise emitted bare.
//!
//! Behaviour notes vs Go:
//!   - Go gates on `len(s.Environment) == 0` (i.e. an `environment_file`
//!     alone with no entries to merge is a no-op). We replicate that to
//!     keep dry-runs idempotent.
//!   - Go uses `joho/godotenv` for parse + serialise. We vendor a tiny
//!     parser here covering bare values, `K="quoted"`, `K='quoted'`,
//!     comments and blank lines — enough for `/etc/environment` and
//!     `cos-layout.env`.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use tracing::{debug, info};

use crate::console::Console;
use crate::error::Result;
use crate::executor::Plugin;
use crate::schema::Stage;
use crate::vfs::Vfs;

const DEFAULT_ENV_FILE: &str = "/etc/environment";

/// Build the plugin closure for registration with the executor.
pub fn build() -> Plugin {
    Arc::new(run)
}

/// Apply the environment plugin against `stage`.
pub fn run(stage: &Stage, fs: &dyn Vfs, _console: &dyn Console) -> Result<()> {
    // Match Go: nothing to merge => no write.
    if stage.environment.is_empty() {
        debug!("environment: empty map, skipping");
        return Ok(());
    }

    let path_str = if stage.environment_file.is_empty() {
        DEFAULT_ENV_FILE
    } else {
        stage.environment_file.as_str()
    };
    let path = Path::new(path_str);

    // Read existing file if present; otherwise start from empty.
    let mut env: HashMap<String, String> = if fs.exists(path) {
        match fs.read_to_string(path) {
            Ok(s) => parse_dotenv(&s),
            Err(e) => {
                debug!(error = %e, path = path_str, "could not read existing env file; starting empty");
                HashMap::new()
            }
        }
    } else {
        HashMap::new()
    };

    // Merge: stage entries win over file contents.
    for (k, v) in &stage.environment {
        env.insert(k.clone(), v.clone());
    }

    let rendered = render_dotenv(&env);
    fs.write(path, rendered.as_bytes())?;
    info!(path = path_str, entries = env.len(), "wrote environment file");
    Ok(())
}

/// Minimal `.env` parser. Supports:
///   - `KEY=value`
///   - `KEY="value with spaces"` / `KEY='value'` (matching quotes stripped)
///   - blank lines and `# comment` lines are ignored
///   - leading `export ` is stripped
///   - lines without `=` are skipped silently
fn parse_dotenv(input: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for raw in input.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line).trim_start();
        let Some(eq) = line.find('=') else {
            continue;
        };
        let key = line[..eq].trim();
        if key.is_empty() {
            continue;
        }
        let value = line[eq + 1..].trim();
        let value = unquote(value);
        out.insert(key.to_string(), value);
    }
    out
}

fn unquote(v: &str) -> String {
    if v.len() >= 2 {
        let bytes = v.as_bytes();
        let first = bytes[0];
        let last = bytes[v.len() - 1];
        if (first == b'"' && last == b'"') || (first == b'\'' && last == b'\'') {
            return v[1..v.len() - 1].to_string();
        }
    }
    v.to_string()
}

/// Serialise sorted by key. Values are quoted when they contain whitespace
/// or any shell-special char from `"'\\$ \t\n#`. Matches `godotenv.Write`
/// closely enough for the round-trip cases we care about.
fn render_dotenv(env: &HashMap<String, String>) -> String {
    let mut keys: Vec<&String> = env.keys().collect();
    keys.sort();
    let mut out = String::new();
    for k in keys {
        let v = &env[k];
        out.push_str(k);
        out.push('=');
        if needs_quoting(v) {
            out.push('"');
            for c in v.chars() {
                match c {
                    '"' | '\\' => {
                        out.push('\\');
                        out.push(c);
                    }
                    _ => out.push(c),
                }
            }
            out.push('"');
        } else {
            out.push_str(v);
        }
        out.push('\n');
    }
    out
}

fn needs_quoting(v: &str) -> bool {
    if v.is_empty() {
        return false;
    }
    v.chars()
        .any(|c| c.is_whitespace() || matches!(c, '"' | '\'' | '\\' | '$' | '#' | '`'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::console::RecordingConsole;
    use crate::vfs::MemVfs;

    fn stage_env(env: &[(&str, &str)], file: Option<&str>) -> Stage {
        Stage {
            environment: env.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
            environment_file: file.unwrap_or("").to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn empty_environment_is_noop() {
        let fs = MemVfs::new();
        let console = RecordingConsole::new();
        run(&Stage::default(), &fs, &console).unwrap();
        assert!(!fs.exists(Path::new(DEFAULT_ENV_FILE)));
    }

    #[test]
    fn writes_single_kv_to_default_file() {
        let fs = MemVfs::new();
        let console = RecordingConsole::new();
        let stage = stage_env(&[("foo", "0")], None);
        run(&stage, &fs, &console).unwrap();
        let got = fs.read_to_string(Path::new(DEFAULT_ENV_FILE)).unwrap();
        assert_eq!(got, "foo=0\n");
    }

    #[test]
    fn appends_new_key_preserves_existing() {
        let fs = MemVfs::new();
        let console = RecordingConsole::new();
        fs.write(Path::new(DEFAULT_ENV_FILE), b"PATH=/usr/bin\n").unwrap();
        let stage = stage_env(&[("FOO", "bar")], None);
        run(&stage, &fs, &console).unwrap();
        let got = fs.read_to_string(Path::new(DEFAULT_ENV_FILE)).unwrap();
        // Sorted by key.
        assert_eq!(got, "FOO=bar\nPATH=/usr/bin\n");
    }

    #[test]
    fn overrides_existing_key_no_duplicate() {
        let fs = MemVfs::new();
        let console = RecordingConsole::new();
        fs.write(Path::new(DEFAULT_ENV_FILE), b"FOO=old\nBAR=keep\n")
            .unwrap();
        let stage = stage_env(&[("FOO", "new")], None);
        run(&stage, &fs, &console).unwrap();
        let got = fs.read_to_string(Path::new(DEFAULT_ENV_FILE)).unwrap();
        assert_eq!(got, "BAR=keep\nFOO=new\n");
    }

    #[test]
    fn quotes_value_with_spaces() {
        let fs = MemVfs::new();
        let console = RecordingConsole::new();
        let stage = stage_env(&[("MSG", "hello world")], None);
        run(&stage, &fs, &console).unwrap();
        let got = fs.read_to_string(Path::new(DEFAULT_ENV_FILE)).unwrap();
        assert_eq!(got, "MSG=\"hello world\"\n");
    }

    #[test]
    fn respects_custom_environment_file() {
        let fs = MemVfs::new();
        let console = RecordingConsole::new();
        let stage = stage_env(&[("foo", "0")], Some("/run/cos/cos-layout.env"));
        run(&stage, &fs, &console).unwrap();
        assert!(!fs.exists(Path::new(DEFAULT_ENV_FILE)));
        let got = fs
            .read_to_string(Path::new("/run/cos/cos-layout.env"))
            .unwrap();
        assert_eq!(got, "foo=0\n");
    }

    #[test]
    fn parses_quoted_existing_value() {
        let fs = MemVfs::new();
        let console = RecordingConsole::new();
        fs.write(Path::new(DEFAULT_ENV_FILE), b"MSG=\"hello world\"\n")
            .unwrap();
        // Add a second var so the write actually fires (gated on stage env len).
        let stage = stage_env(&[("OTHER", "v")], None);
        run(&stage, &fs, &console).unwrap();
        let got = fs.read_to_string(Path::new(DEFAULT_ENV_FILE)).unwrap();
        // Old key preserved + still quoted on re-render, new key added.
        assert!(got.contains("MSG=\"hello world\"\n"));
        assert!(got.contains("OTHER=v\n"));
    }

    #[test]
    fn parse_dotenv_handles_comments_and_export() {
        let src = "# top comment\nexport FOO=bar\nBAZ='qux'\n\nINVALID\n";
        let parsed = parse_dotenv(src);
        assert_eq!(parsed.get("FOO").map(|s| s.as_str()), Some("bar"));
        assert_eq!(parsed.get("BAZ").map(|s| s.as_str()), Some("qux"));
        assert!(!parsed.contains_key("INVALID"));
    }

    #[test]
    fn needs_quoting_truth_table() {
        assert!(!needs_quoting(""));
        assert!(!needs_quoting("simple"));
        assert!(!needs_quoting("a=b")); // '=' alone isn't special
        assert!(needs_quoting("with space"));
        assert!(needs_quoting("tab\there"));
        assert!(needs_quoting("has\"quote"));
        assert!(needs_quoting("has$var"));
        assert!(needs_quoting("hash#here"));
    }

    #[test]
    fn build_returns_callable_plugin() {
        let plugin = build();
        let fs = MemVfs::new();
        let console = RecordingConsole::new();
        let stage = stage_env(&[("A", "1")], None);
        plugin(&stage, &fs, &console).unwrap();
        assert_eq!(
            fs.read_to_string(Path::new(DEFAULT_ENV_FILE)).unwrap(),
            "A=1\n"
        );
    }
}
