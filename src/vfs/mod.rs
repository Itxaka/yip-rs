//! Filesystem abstraction trait. Real + tempdir + in-memory impls.
//!
//! Ports the surface yip uses from `twpayne/go-vfs`. The Go interface is
//! large, but yip plugins only touch a small subset (read/write/mkdir/walk/
//! stat/chmod/chown/symlink/remove); that subset is what the [`Vfs`] trait
//! exposes.

mod mem;
mod real;
mod temp;
mod vfs;

#[cfg(test)]
mod tests_common;

pub use mem::MemVfs;
pub use real::RealVfs;
pub use temp::TempVfs;
pub use vfs::{Metadata, Vfs};
