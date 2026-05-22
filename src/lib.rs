//! yip — cloud-init-style YAML stage executor (Rust port of mudler/yip).
//!
//! Library surface re-exports the public modules so binaries and tests can
//! share the same code path. `src/main.rs` is a thin clap shim around
//! [`cli::run`].

pub mod cli;
pub mod conditionals;
pub mod console;
pub mod error;
pub mod executor;
pub mod plugins;
pub mod schema;
pub mod template;
pub mod vfs;

pub const VERSION: &str = env!("YIP_VERSION");
pub const COMMIT: &str = env!("YIP_COMMIT");
