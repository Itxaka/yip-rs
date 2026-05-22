//! Stage runner + plugin chain. Ports `pkg/executor/default.go`.
//!
//! Public surface:
//! - [`Executor`] trait — `run`, `apply`, `analyze`
//! - [`Plugin`], [`Conditional`] type aliases (Arc'd closures)
//! - [`ConditionalOutcome`] — `Run` / `Skip`
//! - [`DefaultExecutor`] — concrete impl with registered plugins + conditionals

mod default;
mod executor;

pub use default::DefaultExecutor;
pub use executor::{Conditional, ConditionalOutcome, Executor, Plugin};
