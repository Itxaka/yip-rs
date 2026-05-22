//! `Executor` trait + plugin/conditional type aliases.
//!
//! Mirrors `pkg/executor/executor.go`:
//!   - Go `Plugin` is a single func type used for both real plugins (side
//!     effects) and conditionals (gating). In Rust we split them: real
//!     plugins return `Result<()>` and conditionals return a tri-state
//!     [`ConditionalOutcome`] so we don't have to abuse error values to mean
//!     "skip this stage".
//!   - The Go `Executor` interface has six methods. We collapse `Plugins` /
//!     `Conditionals` / `Modifier` into builder methods on `DefaultExecutor`
//!     (they're construction concerns, not part of the runtime contract).
//!   - `Graph` is folded into [`Executor::analyze`] since petgraph already
//!     gives us a topological iterator.

use std::sync::Arc;

use crate::console::Console;
use crate::error::Result;
use crate::schema::{Config, Stage};
use crate::vfs::Vfs;

/// Result of evaluating a conditional against a stage. `Skip` means "this
/// stage should not run"; the executor short-circuits the plugin chain for
/// that stage. `Run` means "proceed". Errors from conditionals are logged
/// and treated as `Skip` (matches Go's `applyStage`: any conditional error
/// returns `nil` and skips the stage).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConditionalOutcome {
    Run,
    Skip,
}

/// A plugin is an action: given a stage + fs + console, perform side
/// effects. Errors are accumulated by the executor (multierror), they do
/// NOT abort the plugin chain for the current stage.
pub type Plugin =
    Arc<dyn Fn(&Stage, &dyn Vfs, &dyn Console) -> Result<()> + Send + Sync>;

/// A conditional decides whether a stage should run. Returning `Skip`
/// short-circuits the rest of the plugins for that stage.
pub type Conditional =
    Arc<dyn Fn(&Stage, &dyn Vfs, &dyn Console) -> Result<ConditionalOutcome> + Send + Sync>;

/// The thing that runs yip configs.
///
/// Implementations must be `Send + Sync` so a single executor can be shared
/// across threads (the DAG is single-threaded today, but plugins may spawn
/// their own work).
pub trait Executor: Send + Sync {
    /// Top-level entrypoint. `paths` may be:
    ///   - a file (`*.yaml` / `*.yml`)
    ///   - a directory (walked recursively, `.yaml`/`.yml` only, lexicographic)
    ///   - an `http://` / `https://` URL
    ///   - `-` → read from stdin
    ///   - raw inline YAML (contains `:` and `\n`)
    ///
    /// Mirrors Go `Executor.Run`.
    fn run(
        &self,
        stage: &str,
        fs: &dyn Vfs,
        console: &dyn Console,
        paths: &[String],
    ) -> Result<()>;

    /// Run a single pre-loaded `Config`. Mirrors Go `Apply`.
    fn apply(
        &self,
        stage: &str,
        cfg: &Config,
        fs: &dyn Vfs,
        console: &dyn Console,
    ) -> Result<()>;

    /// Dry-run: returns the ordered list of stage operation names that
    /// would execute for `stage` against `cfg`. No side effects, no
    /// conditionals evaluated. Mirrors Go `Analyze` + `Graph` (we flatten
    /// the layered DAG into a single ordered list — yip doesn't actually
    /// parallelise, so this loses no information).
    fn analyze(&self, stage: &str, cfg: &Config) -> Vec<String>;
}
