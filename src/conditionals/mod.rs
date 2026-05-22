//! Conditional plugins. Each one returns a [`crate::executor::ConditionalOutcome`]
//! deciding whether a stage should run.
//!
//! Mirrors the 7 conditionals registered in Go's `NewExecutor()`:
//! `NodeConditional`, `IfConditional`, `OnlyIfOS`, `OnlyIfOSVersion`,
//! `IfArch`, `IfServiceManager`, `IfFiles`.
//!
//! Wave-2 agents fill in each submodule with a `pub fn build() ->
//! crate::executor::Conditional` constructor; this module re-exports
//! them once they land.

pub mod node;
pub mod if_cond;
pub mod only_if_os;
pub mod only_if_os_version;
pub mod if_arch;
pub mod if_service_manager;
pub mod if_files;
