//! Stub — wave-6 agent fills this in.
//! Clap CLI: subcommand `--stage <name> <paths…>` plus `validate`,
//! `apply`, etc. Mirrors Go `cmd/` layout.

use std::process::ExitCode;

use clap::Parser;

#[derive(Debug, Parser)]
#[command(name = "yip", version = crate::VERSION)]
pub struct Cli {
    /// Stage name to run (e.g. `rootfs`, `initramfs`).
    #[arg(short, long)]
    pub stage: Option<String>,
    /// One or more paths (files or directories) holding YAML config.
    pub paths: Vec<std::path::PathBuf>,
}

pub fn run(_cli: Cli) -> ExitCode {
    eprintln!("yip {} (commit {})", crate::VERSION, crate::COMMIT);
    eprintln!("CLI wiring not yet implemented — wave 6.");
    ExitCode::SUCCESS
}
