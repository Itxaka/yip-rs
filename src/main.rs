//! Binary entrypoint. All real logic lives in [`yip::cli::run`] so tests
//! and integration callers can drive the same code path without forking
//! a process.

use std::process::ExitCode;

use clap::Parser;

fn main() -> ExitCode {
    let cli = yip::cli::Cli::parse();
    yip::cli::run(cli)
}
