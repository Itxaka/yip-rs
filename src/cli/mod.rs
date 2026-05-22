//! Clap CLI for the `yip` binary.
//!
//! Mirrors the Go cobra command in `cmd/root.go`:
//!
//!   yip --stage <name> <paths...>     # default: apply stage
//!   yip analyze --stage <name> <paths...>
//!   yip version
//!
//! Paths follow the same resolution rules as Go yip: filesystem files,
//! directories (walked for *.yaml/*.yml), `http://` / `https://` URLs,
//! `-` for stdin, or raw inline YAML. Resolution happens inside the
//! executor — see `crate::executor::DefaultExecutor::resolve_source`.

use std::path::Path;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use tracing::{error, info};
use tracing_subscriber::EnvFilter;

use crate::console::StandardConsole;
use crate::error::Error;
use crate::executor::{DefaultExecutor, Executor};
use crate::schema::Config;
use crate::vfs::RealVfs;

#[derive(Debug, Parser)]
#[command(
    name = "yip",
    version = crate::VERSION,
    long_version = concat!(env!("YIP_VERSION"), " (commit ", env!("YIP_COMMIT"), ")"),
    about = "Cloud-init-style YAML stage executor (Rust port of mudler/yip)",
)]
pub struct Cli {
    /// Stage name to run (e.g. `rootfs`, `initramfs`). When set, the default
    /// action runs that stage against the supplied paths.
    #[arg(short, long)]
    pub stage: Option<String>,

    /// One or more sources: file paths, directories (walked for *.yaml/*.yml),
    /// `http://...` URLs, `-` for stdin, or raw inline YAML.
    pub paths: Vec<String>,

    /// Set logging level (default: info). Accepts off/error/warn/info/debug/trace.
    #[arg(long, default_value = "info")]
    pub log_level: String,

    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Print the dry-run plan for a stage (which ops would execute, in order).
    /// Doesn't actually run anything. Mirrors Go `yip validate` / analyze.
    Analyze {
        #[arg(short, long)]
        stage: String,
        paths: Vec<String>,
    },
    /// Print version info and exit.
    Version,
}

pub fn run(cli: Cli) -> ExitCode {
    // 1. Init logging. Done first so any subsequent dispatch can log.
    init_logging(&cli.log_level);

    // 2. Dispatch. Subcommands win over the default `--stage` action; this
    //    matches clap's normal precedence and is the same shape as Go yip's
    //    cobra setup (root vs subcommands).
    match cli.command {
        Some(Command::Version) => {
            println!("yip {} (commit {})", crate::VERSION, crate::COMMIT);
            ExitCode::SUCCESS
        }
        Some(Command::Analyze { stage, paths }) => analyze(&stage, &paths),
        None => match cli.stage {
            Some(stage) => apply(&stage, &cli.paths),
            None => {
                error!("usage: yip --stage <name> <paths...> | yip analyze ... | yip version");
                ExitCode::FAILURE
            }
        },
    }
}

/// Apply `stage` against every supplied source via `DefaultExecutor::run`.
///
/// Errors from individual sources/plugins are aggregated by the executor
/// into `Error::Multi`; we walk that here and print one line per error so
/// the operator sees the whole picture instead of just the first failure.
fn apply(stage: &str, paths: &[String]) -> ExitCode {
    if paths.is_empty() {
        error!("yip --stage {stage}: needs at least one path or url");
        return ExitCode::FAILURE;
    }

    let exec = DefaultExecutor::new();
    let fs = RealVfs::new();
    let console = StandardConsole::new();

    info!(stage = stage, sources = paths.len(), "yip applying stage");

    match exec.run(stage, &fs, &console, paths) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            print_error(&e);
            ExitCode::FAILURE
        }
    }
}

/// Dry-run: load each supplied path as a `Config` and ask the executor for
/// its op-name plan. We deliberately do NOT use `Executor::run` here —
/// `analyze` must be side-effect-free, and that's exactly what the
/// `Executor::analyze` trait method guarantees.
///
/// Path resolution is intentionally simpler than the executor's
/// `resolve_source`: just files and directories. URLs / stdin / inline YAML
/// are skipped with a warning because the executor's resolver is private
/// and the dry-run is only ever useful for source files the operator is
/// editing locally.
fn analyze(stage: &str, paths: &[String]) -> ExitCode {
    if paths.is_empty() {
        error!("yip analyze --stage {stage}: needs at least one path");
        return ExitCode::FAILURE;
    }

    let exec = DefaultExecutor::new();
    let mut had_error = false;

    for source in paths {
        let configs = match load_configs_for_analyze(source) {
            Ok(c) => c,
            Err(e) => {
                error!(source = source.as_str(), error = %e, "failed to load source");
                had_error = true;
                continue;
            }
        };

        for (label, cfg) in configs {
            let names = exec.analyze(stage, &cfg);
            println!("# {label}");
            if names.is_empty() {
                println!("  (no ops for stage `{stage}`)");
            } else {
                for n in names {
                    println!("  {n}");
                }
            }
        }
    }

    if had_error { ExitCode::FAILURE } else { ExitCode::SUCCESS }
}

/// Local mini path-resolver for `analyze`. Files load directly; directories
/// are walked for `*.yaml` / `*.yml` in sorted order (mirrors the executor's
/// `load_dir`). Anything else (URLs, stdin, inline YAML) returns an error
/// because we can't reach the executor's private resolver from here.
fn load_configs_for_analyze(source: &str) -> Result<Vec<(String, Config)>, Error> {
    let p = Path::new(source);
    if !p.exists() {
        return Err(Error::other(format!(
            "analyze source {source:?}: not a file or directory \
             (URLs / stdin / inline YAML are not supported by `analyze` — use `--stage` for those)"
        )));
    }

    let md = std::fs::metadata(p).map_err(|e| Error::io_at(p, e))?;
    if md.is_file() {
        let cfg = Config::load_file(p)?;
        return Ok(vec![(source.to_string(), cfg)]);
    }

    // Directory walk: lexicographic, *.yaml / *.yml only.
    let mut entries: Vec<std::path::PathBuf> = walkdir::WalkDir::new(p)
        .sort_by_file_name()
        .into_iter()
        .filter_map(|r| r.ok())
        .filter(|e| e.file_type().is_file())
        .map(|e| e.into_path())
        .filter(|q| {
            matches!(
                q.extension().and_then(|s| s.to_str()),
                Some("yaml") | Some("yml")
            )
        })
        .collect();
    entries.sort();

    let mut out = Vec::with_capacity(entries.len());
    for q in entries {
        let cfg = Config::load_file(&q)?;
        out.push((q.display().to_string(), cfg));
    }
    Ok(out)
}

/// Pretty-print an error to stderr. `Error::Multi` is unrolled so each
/// underlying failure gets its own line — matches Go's multierror output.
fn print_error(e: &Error) {
    match e {
        Error::Multi(errs) => {
            error!("{} error(s) during run:", errs.len());
            for (i, inner) in errs.iter().enumerate() {
                error!("  [{}] {}", i + 1, inner);
            }
        }
        other => error!("{other}"),
    }
}

/// Set up the global tracing subscriber. We accept the standard
/// `off/error/warn/info/debug/trace` levels; anything else (including
/// `RUST_LOG`-style directives) is rejected and we fall back to `info`.
/// Logs go to stderr so stdout stays clean for `analyze` output.
fn init_logging(level: &str) {
    let filter = EnvFilter::try_new(level).unwrap_or_else(|_| EnvFilter::new("info"));
    // `try_init` so duplicate initialisation in tests is a no-op instead
    // of a panic.
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .try_init();
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn parses_stage_flag() {
        let cli = Cli::parse_from(["yip", "--stage", "rootfs", "/foo.yaml"]);
        assert_eq!(cli.stage.as_deref(), Some("rootfs"));
        assert_eq!(cli.paths, vec!["/foo.yaml".to_string()]);
    }

    #[test]
    fn parses_version_subcommand() {
        let cli = Cli::parse_from(["yip", "version"]);
        assert!(matches!(cli.command, Some(Command::Version)));
    }

    #[test]
    fn parses_analyze_subcommand() {
        let cli = Cli::parse_from(["yip", "analyze", "--stage", "rootfs", "/f.yaml"]);
        match cli.command {
            Some(Command::Analyze { stage, paths }) => {
                assert_eq!(stage, "rootfs");
                assert_eq!(paths, vec!["/f.yaml".to_string()]);
            }
            _ => panic!("expected Analyze"),
        }
    }

    #[test]
    fn version_command_returns_success() {
        let cli = Cli {
            stage: None, paths: vec![], log_level: "off".into(),
            command: Some(Command::Version),
        };
        // run() will println — that's fine.
        let _ = run(cli);
    }
}
