//! YAML schema types — Rust port of `pkg/schema/schema.go` from
//! [mudler/yip](https://github.com/mudler/yip).
//!
//! Each submodule mirrors a section of the original Go file:
//!
//! | submodule        | Go types                                  |
//! |------------------|-------------------------------------------|
//! | [`config`]       | `Config` / `YipConfig`, `Config::load*`   |
//! | [`stage`]        | `Stage`, `Dependency`                     |
//! | [`file`]         | `File`, `Download`, `Directory`, `OwnerId` |
//! | [`user`]         | `User`, `YipEntity`                       |
//! | [`layout`]       | `Layout`, `Device`, `Partition`, `ExpandPartition` |
//! | [`packages`]     | `Packages`, `PackagePins`                 |
//! | [`systemctl`]    | `Systemctl`, `SystemctlOverride`          |
//! | [`git`]          | `Git`, `Auth`                             |
//! | [`datasource`]   | `DataSource`, `DataSourceProvider`        |
//! | [`dns`]          | `DNS`                                     |
//! | [`unpack`]       | `UnpackImageConf`                         |
//! | [`if_files`]     | `IfCheckType`, `IfFiles`, `IfFile`        |
//! | [`dot_notation`] | `dot_notation_modifier`                   |

pub mod config;
pub mod datasource;
pub mod dns;
pub mod dot_notation;
pub mod file;
pub mod git;
pub mod if_files;
pub mod layout;
pub mod packages;
pub mod stage;
pub mod systemctl;
pub mod unpack;
pub mod user;

// Re-exports — flat surface so callers can `use crate::schema::{Config, Stage, ...}`.
pub use config::{Config, YipConfig};
pub use datasource::{DataSource, DataSourceProvider};
pub use dns::DNS;
pub use dot_notation::dot_notation_modifier;
pub use file::{Directory, Download, File, OwnerId};
pub use git::{Auth, Git};
pub use if_files::{IfCheckType, IfFile, IfFiles};
pub use layout::{Device, ExpandPartition, Layout, Partition};
pub use packages::{PackagePins, Packages};
pub use stage::{Dependency, Stage};
pub use systemctl::{Systemctl, SystemctlOverride};
pub use unpack::UnpackImageConf;
pub use user::{User, YipEntity};
