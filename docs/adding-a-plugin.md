# Adding a plugin

Walk through adding a hypothetical `foo` plugin end-to-end. The pattern
matches every plugin currently in `src/plugins/`; the canonical reference
is `src/plugins/files.rs`.

Goal: when a stage contains

```yaml
stages:
  rootfs:
    - name: hello-foo
      foo:
        - bar
        - baz
```

the `foo` plugin should run `bar` and `baz` through some side effect.

## Step 1 — Add the schema field

`src/schema/stage.rs`. Add a field with the right serde rename:

```rust
#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Stage {
    // ... existing fields ...

    /// Items to be foo'd. Matches Go's `Foo []string` yaml:"foo".
    #[serde(default, rename = "foo", skip_serializing_if = "Vec::is_empty")]
    pub foo: Vec<String>,
}
```

Rules:

- **`#[serde(default)]`** — the field must be optional in YAML. Every
  existing field defaults; yours does too.
- **`rename = "foo"`** — must match the Go YAML tag (or, if Go has no
  explicit tag, the lowercase field name).
- **`skip_serializing_if`** — keeps the field out of `cfg.to_yaml()`
  output when empty. Use the right helper: `Vec::is_empty`,
  `String::is_empty`, `HashMap::is_empty`, or a custom `xxx_is_default`
  function (see existing `dns_is_default` / `systemctl_is_default`).
- If the value is a struct, put the struct in its own submodule under
  `src/schema/` (look at `file.rs`, `git.rs`, `user.rs` for patterns)
  and re-export from `src/schema/mod.rs`.

If the YAML accepts more than one shape (e.g. `owner: 1000` *and*
`owner: "alice"`), use a custom enum + tolerant deserialize. See
`OwnerId` in `src/schema/file.rs`.

Add a parsing assertion in `tests/yaml_parse.rs` or a unit test inside
`stage.rs`:

```rust
#[test]
fn parses_foo_field() {
    let y = "foo: [a, b]";
    let s: Stage = serde_yaml::from_str(y).unwrap();
    assert_eq!(s.foo, vec!["a".to_string(), "b".to_string()]);
}
```

## Step 2 — Write the plugin module

Create `src/plugins/foo.rs`. Two public symbols: `build()` and `run()`.

```rust
//! `foo` plugin — does foo to each entry in `stage.foo`.
//!
//! Mirrors `pkg/plugins/foo.go::EnsureFoo` (or whatever the Go name is).
//! Per entry: <one-line summary of what happens>. All per-entry failures
//! aggregate into `Error::Multi`; the loop never aborts.

use std::sync::Arc;

use tracing::{debug, info, warn};

use crate::console::Console;
use crate::error::{Error, Result};
use crate::executor::Plugin;
use crate::schema::Stage;
use crate::vfs::Vfs;

/// Build a [`Plugin`] arc-closure. Wired into `DefaultExecutor::new()`.
pub fn build() -> Plugin {
    Arc::new(run)
}

/// Pure entry point — exposed so tests can call without going through `Arc`.
pub fn run(stage: &Stage, fs: &dyn Vfs, console: &dyn Console) -> Result<()> {
    if stage.foo.is_empty() {
        return Ok(());
    }

    info!(count = stage.foo.len(), "applying foo");

    let mut errs: Vec<Error> = Vec::new();
    for entry in &stage.foo {
        if let Err(e) = do_one(entry, fs, console) {
            warn!(entry = %entry, error = %e, "foo entry failed");
            errs.push(e);
        }
    }

    match errs.len() {
        0 => Ok(()),
        _ => Err(Error::Multi(errs)),
    }
}

fn do_one(entry: &str, _fs: &dyn Vfs, console: &dyn Console) -> Result<()> {
    if entry.is_empty() {
        return Err(Error::other("foo entry is empty"));
    }
    debug!(entry = %entry, "foo");
    console.run(&format!("foo {entry}"))?;
    Ok(())
}
```

Conventions to keep:

- **Empty-input short-circuit.** Plugins are run for every stage; most
  stages won't have your field. Return `Ok(())` immediately when
  `stage.foo.is_empty()` (or whatever the empty check is). Avoid even a
  `debug!` log for that path — it's the common case.
- **Per-entry loop with multierror.** Iterate, collect failures into a
  `Vec<Error>`, return `Error::Multi(errs)` at the end. **Never** abort
  on the first failure; Go's loop doesn't and we match.
- **Use `Vfs` and `Console`, not `std::fs` / `std::process`.** Tests
  rely on this. The only exceptions are syscalls that don't go through
  filesystem paths (e.g. `libc::sethostname` in `hostname.rs`,
  `libc::gethostname` in `conditionals/node.rs`).
- **Structured logging.** `tracing` macros with key-value fields, not
  formatted strings: `warn!(path = %p.display(), "failed to write")` not
  `warn!("failed to write {}", p.display())`.
- **Document deviations from Go.** If you deliberately differ, put a
  block comment at the top of the file explaining why.

## Step 3 — Register the module

`src/plugins/mod.rs`:

```rust
pub mod commands;
pub mod directories;
pub mod dns;
pub mod entities;
// ...
pub mod foo;          // <-- add this
// ...
```

Order in this file doesn't matter, but the existing list is loosely
grouped by wave. Drop yours wherever fits.

## Step 4 — Wire into `DefaultExecutor::new()`

`src/executor/default.rs`:

```rust
pub fn new() -> Self {
    Self::empty()
        .with_modifier(Arc::new(|bytes: &[u8]| {
            crate::schema::dot_notation_modifier(bytes)
        }))
        // ... conditionals ...
        .with_plugin("dns", crate::plugins::dns::build())
        .with_plugin("download", crate::plugins::download::build())
        // ...
        .with_plugin("foo", crate::plugins::foo::build())   // <-- add
        // ...
}
```

**Order matters.** Plugins run in registration order for every stage.
Match Go's `NewExecutor()` order if the matching Go plugin exists; for
new plugins, put yours near related ones (e.g. file-touching plugins
near `files` / `directories`).

There's a test at the bottom of `default.rs` that asserts the total
plugin count:

```rust
#[test]
fn default_executor_registers_all_plugins() {
    let exec = DefaultExecutor::new();
    assert_eq!(exec.plugins.len(), 22, ...);
    // ... spot checks ...
}
```

Bump the count and add a `names.contains(&"foo")` assertion.

## Step 5 — Tests

At minimum, every plugin should have unit tests for: empty input,
happy path, one error case. Use `MemVfs` and `RecordingConsole` so the
tests don't depend on the host.

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::console::RecordingConsole;
    use crate::vfs::MemVfs;

    #[test]
    fn empty_stage_is_ok() {
        let stage = Stage::default();
        let fs = MemVfs::new();
        let console = RecordingConsole::new();
        run(&stage, &fs, &console).expect("empty -> Ok");
    }

    #[test]
    fn runs_each_entry_through_console() {
        let stage = Stage {
            foo: vec!["a".into(), "b".into()],
            ..Default::default()
        };
        let fs = MemVfs::new();
        let console = RecordingConsole::new();
        run(&stage, &fs, &console).expect("should succeed");

        assert_eq!(
            console.commands(),
            vec!["foo a".to_string(), "foo b".to_string()],
        );
    }

    #[test]
    fn empty_entry_aggregates_error_without_aborting() {
        let stage = Stage {
            foo: vec!["a".into(), "".into(), "c".into()],
            ..Default::default()
        };
        let fs = MemVfs::new();
        let console = RecordingConsole::new();
        let err = run(&stage, &fs, &console).expect_err("empty entry should fail");
        match err {
            Error::Multi(errs) => assert_eq!(errs.len(), 1),
            other => panic!("expected Multi, got {other:?}"),
        }
        // The good entries still ran.
        assert!(console.commands().contains(&"foo a".to_string()));
        assert!(console.commands().contains(&"foo c".to_string()));
    }

    #[test]
    fn console_error_propagates_as_one_entry() {
        let stage = Stage {
            foo: vec!["bad".into()],
            ..Default::default()
        };
        let fs = MemVfs::new();
        let console = RecordingConsole::new();
        console.expect("foo bad", Err("boom".to_string()));
        let err = run(&stage, &fs, &console).expect_err("should fail");
        match err {
            Error::Multi(errs) => assert_eq!(errs.len(), 1),
            other => panic!("expected Multi, got {other:?}"),
        }
    }
}
```

What to cover beyond the minimum:

- Each branch in `do_one` (encoding variants, owner types, etc.).
- Ordering: if your plugin runs multiple sub-actions, assert the order
  via `console.commands()` or fixture state.
- Idempotency: if the plugin can run twice, assert the second run is a
  no-op (look at `entities.rs` tests for the pattern).
- Error aggregation: at least one test that proves a mid-loop failure
  doesn't stop subsequent entries from running.

For plugins that need real filesystem semantics (e.g. you call
`std::fs::canonicalize`), use `TempVfs::new()` instead of `MemVfs`.

For plugins that shell out and you want to assert command shape — that's
exactly what `RecordingConsole::expect()` and `console.commands()` are
for. See `src/plugins/git.rs` for a heavier example.

## Step 6 — Document the YAML

`docs/configuration-reference.md` (create if missing). Add a short
section keyed by your field name:

```markdown
## `foo`

List of strings. Each entry is fed to `foo <entry>` via the configured
Console. Empty list is a no-op.

**Example**

```yaml
stages:
  rootfs:
    - foo:
        - bar
        - baz
```

**Errors**

Empty entries and non-zero `foo` exits are aggregated into a multi-error.
The whole list is processed even if some entries fail.
```

Cross-link from any related sections (e.g. if `foo` only makes sense
after `files`, mention it).

## Cross-check: Go parity

After steps 1-6:

- Open `pkg/plugins/foo.go` (the Go original).
- Re-read your `do_one`. Does it match Go's order of operations? Edge
  cases? Default values?
- If you diverged deliberately, write a comment block at the top of
  `foo.rs`:
  ```rust
  //! Differences from the Go version, with justification:
  //!
  //! 1. <thing>: <why>
  //! 2. ...
  ```
- If Go has a test that asserts a behaviour you didn't replicate,
  either replicate it or document why it's irrelevant.

## Final checklist

- [ ] Field added to `Stage` with `#[serde(default, rename = ...)]`
- [ ] New module `src/plugins/foo.rs` with `build()` + `run()`
- [ ] `pub mod foo;` in `src/plugins/mod.rs`
- [ ] Wired into `DefaultExecutor::new()` in `src/executor/default.rs`
- [ ] Plugin count test updated
- [ ] Unit tests cover empty / happy / error-aggregation paths
- [ ] `docs/configuration-reference.md` entry added
- [ ] `CHANGELOG.md` `[Unreleased] → Added` entry
- [ ] `cargo fmt` / `cargo clippy -D warnings` clean
- [ ] If Go has a matching plugin, behavioural parity confirmed or
      divergence documented
