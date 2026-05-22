//! The [`Executor`] trait and the plugin / conditional function aliases.
//!
//! This module defines the *contract* — the actual runtime is in
//! [`crate::executor::DefaultExecutor`]. An executor takes a stage key
//! (e.g. `"rootfs"`) and a set of sources (files, dirs, URLs, inline YAML),
//! parses each into a [`Config`], topologically orders the contained
//! stages, and runs the registered plugin chain against each one.
//!
//! ## Plugins vs conditionals
//!
//! The Go reference encodes both as the same `Plugin` function type and
//! abuses error values to mean "skip this stage". We split them:
//!
//! - A [`Plugin`] is an *action* — it returns `Result<()>`. Errors are
//!   accumulated by the executor; they do not abort the chain.
//! - A [`Conditional`] is a *gate* — it returns a tri-state
//!   [`ConditionalOutcome`]. Returning [`ConditionalOutcome::Skip`]
//!   short-circuits the plugin chain for the current stage. Errors are
//!   logged and also treated as `Skip` (matching Go's observable
//!   behaviour).
//!
//! Both are wrapped in [`Arc`] so a single executor instance is cheap to
//! clone and safe to share across threads.
//!
//! # Examples
//!
//! ```no_run
//! use std::sync::Arc;
//! use yip::executor::{ConditionalOutcome, Conditional, Plugin};
//! use yip::error::Result;
//!
//! let always_run: Conditional = Arc::new(|_st, _fs, _con| Ok(ConditionalOutcome::Run));
//! let noop: Plugin = Arc::new(|_st, _fs, _con| Ok(()));
//! ```
//!
//! # Stability
//!
//! Public API. The trait shape mirrors the Go `Executor` interface so the
//! port stays comparable; adding a method is a breaking change.

use std::sync::Arc;

use crate::console::Console;
use crate::error::Result;
use crate::schema::{Config, Stage};
use crate::vfs::Vfs;

/// Result of evaluating a conditional against a stage.
///
/// `Skip` means "this stage should not run"; the executor short-circuits
/// the plugin chain for that stage. `Run` means "proceed". Errors from
/// conditionals are logged and treated as `Skip` (matches Go's
/// `applyStage`: any conditional error returns `nil` and skips the stage).
///
/// # Examples
///
/// ```
/// use yip::executor::ConditionalOutcome;
///
/// let go = ConditionalOutcome::Run;
/// assert_eq!(go, ConditionalOutcome::Run);
/// assert_ne!(go, ConditionalOutcome::Skip);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConditionalOutcome {
    /// Proceed with the plugin chain for this stage.
    Run,
    /// Do not run any plugins for this stage; move on.
    Skip,
}

/// A plugin: given a stage, fs, and console, perform side effects.
///
/// Errors are accumulated by the executor (multierror); they do NOT abort
/// the plugin chain for the current stage. Wrapped in [`Arc`] so the same
/// plugin can be shared across executor instances.
///
/// # Examples
///
/// ```
/// use std::sync::Arc;
/// use yip::executor::Plugin;
///
/// let _noop: Plugin = Arc::new(|_st, _fs, _con| Ok(()));
/// ```
pub type Plugin =
    Arc<dyn Fn(&Stage, &dyn Vfs, &dyn Console) -> Result<()> + Send + Sync>;

/// A conditional: decides whether a stage should run.
///
/// Returning [`ConditionalOutcome::Skip`] short-circuits the rest of the
/// plugins for that stage. Returning an error is logged and treated as
/// `Skip`.
///
/// # Examples
///
/// ```
/// use std::sync::Arc;
/// use yip::executor::{Conditional, ConditionalOutcome};
///
/// let always: Conditional = Arc::new(|_st, _fs, _con| Ok(ConditionalOutcome::Run));
/// ```
pub type Conditional =
    Arc<dyn Fn(&Stage, &dyn Vfs, &dyn Console) -> Result<ConditionalOutcome> + Send + Sync>;

/// The thing that runs yip configs.
///
/// Implementations must be `Send + Sync` so a single executor can be
/// shared across threads (the DAG is single-threaded today, but plugins
/// may spawn their own work).
///
/// The canonical impl is [`crate::executor::DefaultExecutor`]. Custom
/// executors are useful for tests (inject specific plugins) and embedders
/// who want a reduced surface.
///
/// # Examples
///
/// ```no_run
/// use yip::console::StandardConsole;
/// use yip::executor::{DefaultExecutor, Executor};
/// use yip::vfs::RealVfs;
///
/// let exec = DefaultExecutor::new();
/// exec.run("rootfs", &RealVfs::new(), &StandardConsole::new(), &[]).unwrap();
/// ```
pub trait Executor: Send + Sync {
    /// Top-level entrypoint. `paths` may be:
    ///   - a file (`*.yaml` / `*.yml`)
    ///   - a directory (walked recursively, `.yaml`/`.yml` only,
    ///     lexicographic)
    ///   - an `http://` / `https://` URL
    ///   - `-` → read from stdin
    ///   - raw inline YAML (contains `:` and `\n`)
    ///
    /// Mirrors Go `Executor.Run`.
    ///
    /// # Errors
    ///
    /// Returns [`crate::error::Error::Multi`] if any plugin or source
    /// loader failed. A single failure surfaces as the inner error
    /// directly.
    fn run(
        &self,
        stage: &str,
        fs: &dyn Vfs,
        console: &dyn Console,
        paths: &[String],
    ) -> Result<()>;

    /// Run a single pre-loaded [`Config`]. Mirrors Go `Apply`.
    ///
    /// Unlike [`Executor::run`], this does *not* build a DAG — stages are
    /// applied in declaration order.
    ///
    /// # Errors
    ///
    /// Returns [`crate::error::Error::Multi`] if any plugin failed.
    fn apply(
        &self,
        stage: &str,
        cfg: &Config,
        fs: &dyn Vfs,
        console: &dyn Console,
    ) -> Result<()>;

    /// Dry-run: returns the ordered list of stage operation names that
    /// would execute for `stage` against `cfg`.
    ///
    /// No side effects, no conditionals evaluated. Mirrors Go `Analyze` +
    /// `Graph` (the layered DAG is flattened into a single ordered list
    /// — yip doesn't actually parallelise, so this loses no information).
    ///
    /// # Examples
    ///
    /// ```
    /// use yip::executor::{DefaultExecutor, Executor};
    /// use yip::schema::Config;
    ///
    /// let cfg = Config::load(b"name: t\nstages:\n  rootfs:\n    - name: one\n").unwrap();
    /// let names = DefaultExecutor::empty().analyze("rootfs", &cfg);
    /// assert!(names.iter().any(|n| n.ends_with(".one")));
    /// ```
    fn analyze(&self, stage: &str, cfg: &Config) -> Vec<String>;
}
