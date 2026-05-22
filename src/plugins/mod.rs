//! Plugin set. Each module exposes `pub fn build() -> crate::executor::Plugin`
//! plus a `pub fn run(stage, fs, console) -> Result<()>` form for direct
//! testing.
//!
//! Mirrors the 23 plugins registered by Go's `NewExecutor()`. Waves 3/4/5
//! fill these in; until they land, each module is a stub.

pub mod commands;
pub mod directories;
pub mod dns;
pub mod entities;
pub mod environment;
pub mod files;
pub mod hostname;
pub mod modules;
pub mod sysctl;
pub mod systemctl;
pub mod systemd_firstboot;
pub mod timesyncd;
// wave 4
pub mod download;
pub mod package_pins;
pub mod packages;
pub mod ssh;
pub mod user;
pub mod datasource;
// wave 5
pub mod git;
pub mod layout;
pub mod unpack_image;
