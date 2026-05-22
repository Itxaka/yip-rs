//! # yip — cloud-init-style YAML stage executor
//!
//! `yip` is a Rust port of [mudler/yip], the declarative configuration tool
//! used by [Kairos] and related projects to drive system bring-up. It reads
//! one or more YAML "stages", topologically orders them, and runs a chain
//! of plugins against each one to mutate the host: write files, run shell
//! commands, create users, install packages, configure systemd units,
//! download artifacts, mount filesystems, and so on.
//!
//! This crate is both a library and a binary:
//!
//! - The **library** ([`executor`], [`schema`], [`plugins`], …) is what you
//!   reach for when you want to embed yip into another tool or to drive it
//!   from tests with mock backends.
//! - The **binary** (`src/main.rs`) is a thin [`clap`]-based wrapper that
//!   delegates to [`cli::run`]. The binary is what gets shipped to
//!   production hosts.
//!
//! [mudler/yip]: https://github.com/mudler/yip
//! [Kairos]: https://kairos.io
//!
//! ## Quickstart
//!
//! Apply a single inline YAML config against the real filesystem:
//!
//! ```no_run
//! use yip::console::StandardConsole;
//! use yip::executor::{DefaultExecutor, Executor};
//! use yip::vfs::RealVfs;
//!
//! let exec = DefaultExecutor::new();
//! let fs = RealVfs::new();
//! let console = StandardConsole::new();
//!
//! // Sources can be file paths, directory paths, http(s) URLs, "-" for
//! // stdin, or inline YAML.
//! exec.run(
//!     "rootfs",
//!     &fs,
//!     &console,
//!     &["/etc/yip/conf.d".to_string()],
//! ).expect("yip stage failed");
//! ```
//!
//! Inspect what would run without touching anything:
//!
//! ```no_run
//! use yip::executor::{DefaultExecutor, Executor};
//! use yip::schema::Config;
//!
//! let cfg = Config::load(b"name: demo\nstages:\n  rootfs:\n    - name: hi\n").unwrap();
//! let exec = DefaultExecutor::new();
//! let ops = exec.analyze("rootfs", &cfg);
//! assert!(ops.iter().any(|n| n.ends_with(".hi")));
//! ```
//!
//! ## Module map
//!
//! | Module | What it contains |
//! |---|---|
//! | [`cli`] | clap definitions, `run()` entrypoint used by the binary. |
//! | [`conditionals`] | The seven conditional plugins (`if`, `only_if_os`, `if_arch`, …). |
//! | [`console`] | [`console::Console`] trait + production / recording impls. |
//! | [`error`] | The crate-wide [`error::Error`] enum and [`error::Result`] alias. |
//! | [`executor`] | The DAG-driven stage runner: [`executor::Executor`] trait and [`executor::DefaultExecutor`]. |
//! | [`plugins`] | The 23 action plugins that mutate the system (files, users, packages, …). |
//! | [`schema`] | YAML types ([`schema::Config`], [`schema::Stage`], `User`, `Layout`, …) plus the `dot_notation_modifier`. |
//! | [`template`] | sprig-subset templating used to render configs before parsing. |
//! | [`vfs`] | [`vfs::Vfs`] trait + real / tempdir / in-memory impls. |
//!
//! ## Feature flags
//!
//! Features are toggled at build time in `Cargo.toml`. Defaults pull in
//! native backends; disable them to shrink the binary at the cost of
//! needing the corresponding system tool at runtime.
//!
//! | Feature | Default | Effect |
//! |---|---|---|
//! | `git-builtin` | on | Embed [`gix`](https://crates.io/crates/gix) for git operations. Disable to shell out to `git(1)`. |
//! | `oci-builtin` | on | Embed [`oci-distribution`](https://crates.io/crates/oci-distribution) for OCI registry pulls. Disable to shell out to `skopeo(1)`. |
//! | `disk-builtin` | on | Embed [`gpt`](https://crates.io/crates/gpt) / [`mbrman`](https://crates.io/crates/mbrman) for partitioning. |
//! | `nogit` | off | Drop the `git` plugin entirely. |
//! | `nounpack` | off | Drop the `unpack_images` plugin entirely. |
//!
//! ## Build-time constants
//!
//! [`VERSION`] and [`COMMIT`] are populated by `build.rs` from the
//! workspace `Cargo.toml` and the current git checkout, respectively.
//! They're surfaced through `--version` on the CLI and useful when
//! logging.
//!
//! ## Further reading
//!
//! - `README.md` — high-level overview, layout, license.
//! - `USAGE.md` (when present) — config schema reference.
//! - `ARCHITECTURE.md` (when present) — port design notes and Go ↔ Rust
//!   mappings.
//! - Upstream Go yip: <https://github.com/mudler/yip>.
//!
//! ## Stability
//!
//! Public API. Versioned via [semver](https://semver.org). The crate is
//! pre-1.0 — breaking changes can land on minor bumps until 1.0.

pub mod cli;
pub mod conditionals;
pub mod console;
pub mod error;
pub mod executor;
pub mod plugins;
pub mod schema;
pub mod template;
pub mod vfs;

/// Semver version string for this build. Populated at compile time by
/// `build.rs` from the workspace `Cargo.toml`.
///
/// # Examples
///
/// ```
/// // Always a non-empty string.
/// assert!(!yip::VERSION.is_empty());
/// ```
pub const VERSION: &str = env!("YIP_VERSION");

/// Short git commit hash this binary/library was built from. Populated at
/// compile time by `build.rs`. May be an empty string for source-tarball
/// builds where no `.git` directory is available.
///
/// # Examples
///
/// ```
/// // May be empty for tarball builds; never panics.
/// let _ = yip::COMMIT;
/// ```
pub const COMMIT: &str = env!("YIP_COMMIT");
