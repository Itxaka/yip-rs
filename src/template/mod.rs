//! Templating: sprig-subset funcmap layered on top of `tera`.
//!
//! Bridges Go `text/template` + sprig (as used by yip) onto tera. The yip
//! Go implementation registers `sprig.TxtFuncMap()` on a `text/template`
//! engine and renders config blobs before YAML parsing — we mimic the
//! same behaviour, sharing the `Values.System.*` JSON shape so existing
//! kairos cloud-init configs render unchanged.
//!
//! Two main differences from the Go side:
//!
//! - Go uses `{{ .Foo.Bar }}` for field access. Tera uses `{{ Foo.Bar }}`
//!   (no leading dot). [`preprocess`] rewrites leading dots inside
//!   `{{ ... }}` segments to bridge the two dialects.
//! - Sprig provides ~140 functions; we ship the subset yip configs
//!   actually use. Unimplemented funcs are documented in [`funcs`].

mod engine;
mod funcs;
mod sysdata;

pub use engine::{preprocess, render, render_with_sysdata};
pub use sysdata::{gather_sysdata, parse_os_release, OsRelease};
