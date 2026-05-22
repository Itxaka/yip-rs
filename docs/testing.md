# Testing

Three layers, two filesystem mocks, one HTTP mock, one shell mock. No
test should depend on the host's state — if you find yourself reaching
for `std::fs::read("/etc/passwd")` in a test, switch to a Vfs impl.

## Test layers

### Unit tests — `#[cfg(test)] mod tests` inline

Lives at the bottom of every source file. Imports `super::*` + the
module's mock-friendly deps (`MemVfs`, `RecordingConsole`, `mockito`).
Runs under `cargo test --lib`.

Most plugin / conditional / schema logic is tested here. The hot loop:

```bash
cargo test --lib plugins::files          # one module
cargo test --lib plugins::files::tests::writes_plain_text_file_with_perms
                                         # one test
```

Fast (no I/O unless the test opts in via `TempVfs`), self-contained,
should always be green.

### Integration tests — `tests/`

Top-level files in `tests/` compile as separate test binaries that link
to the `yip` library. They exercise crate-public surfaces.

- **`tests/yaml_parse.rs`** — Schema parity. Reproduces selected
  fixtures from yip's Go test suite (`pkg/schema/schema_test.go` and
  `pkg/executor/default_test.go`) byte-for-byte. Pure parsing, no I/O.
- **`tests/cli.rs`** — Black-box. Shells out to the binary built by
  `assert_cmd::Command::cargo_bin("yip")` and asserts on exit code,
  stdout, stderr.

```bash
cargo test --test yaml_parse
cargo test --test cli
```

### Online-only tests — `#[ignore = "online"]`

Anything that needs the public internet (cloning a real GitHub gist,
pulling from docker.io) is marked `#[ignore = "online"]`. Default
`cargo test` skips them; opt-in with:

```bash
cargo test -- --ignored
```

CI does **not** run these. Use them locally when you're touching the
network code path.

## Vfs impls — when to use which

`src/vfs/{mem,temp,real}.rs`. All implement the same `Vfs` trait so the
plugin code under test doesn't know which one it has.

| Impl | When |
|------|------|
| **`MemVfs`** | Default for unit tests. No syscalls, no tempdir cleanup. Use unless you have a reason. Inspect via `fs.read(Path::new("/x"))`, `fs.exists(...)`, `fs.metadata(...)`. |
| **`TempVfs`** | Use when the plugin shells out (so the shelled command needs to see real files), or when the plugin calls something that doesn't go through `Vfs` (e.g. `std::fs::canonicalize`). Every guest path is rebased under a tempdir; the host filesystem is untouched. `td.path()` is the tempdir root. |
| **`RealVfs`** | Production. In tests, use only when you specifically need to assert against real-filesystem semantics that neither mock captures (e.g. real ownership). Some executor tests use it because they exercise the directory-walk code path and need real `walkdir` entries. |

Example: `MemVfs` test from `src/plugins/files.rs`:

```rust
#[test]
fn writes_plain_text_file_with_perms() {
    let stage = Stage {
        files: vec![File {
            path: "/tmp/test/foo".to_string(),
            content: "Test".to_string(),
            permissions: 0o644,
            ..Default::default()
        }],
        ..Default::default()
    };
    let fs = MemVfs::new();
    let console = RecordingConsole::new();
    run(&stage, &fs, &console).expect("write should succeed");

    let got = fs.read_to_string(Path::new("/tmp/test/foo"))
        .expect("read written file");
    assert_eq!(got, "Test");

    let m = fs.metadata(Path::new("/tmp/test/foo")).expect("metadata");
    assert!(m.is_file);
    assert_eq!(m.mode, 0o644);
}
```

## `RecordingConsole` — shell-out assertion

`src/console/console.rs`. Captures every `run` / `run_in` call without
executing anything. Default response is `Ok("")`; install canned
responses per-command via `expect`.

```rust
let console = RecordingConsole::new();

// Canned response for a specific command:
console.expect("ip addr", Ok("eth0: ...".to_string()));
console.expect("git clone bad-url /tmp/x", Err("nope".to_string()));

// Run the plugin.
run(&stage, &fs, &console).expect("ok");

// Assert what was recorded.
assert_eq!(
    console.commands(),
    vec![
        "systemctl daemon-reload".to_string(),
        "systemctl enable foo.service".to_string(),
    ],
);

// Or inspect cwd:
for call in console.calls() {
    println!("{} (cwd: {:?})", call.cmd, call.cwd);
}
```

Use `expect(...)` for both Ok and Err outcomes. The error variant
returns an `Error::Cmd` matching what `StandardConsole` would produce on
a non-zero exit.

## Mockito — HTTP mocking

For plugins that `reqwest::blocking::get` (`download.rs`, `ssh.rs`,
`datasource.rs`), use `mockito` to spin up an in-process HTTP server
and override the URL the plugin hits.

Pattern (from `src/plugins/download.rs` tests):

```rust
#[test]
fn downloads_and_writes_file() {
    let mut server = mockito::Server::new();
    let m = server.mock("GET", "/some/file")
        .with_status(200)
        .with_body(b"hello")
        .create();

    let stage = Stage {
        downloads: vec![Download {
            url: format!("{}/some/file", server.url()),
            path: "/tmp/d".to_string(),
            ..Default::default()
        }],
        ..Default::default()
    };
    let fs = MemVfs::new();
    let console = RecordingConsole::new();
    run(&stage, &fs, &console).expect("download ok");

    assert_eq!(fs.read(Path::new("/tmp/d")).unwrap(), b"hello");
    m.assert();
}
```

For plugins that hit hardcoded URLs (AWS metadata `169.254.169.254`,
GitHub `github.com/<user>.keys`), the source has an env-var override —
look for `*_BASE_URL` in the plugin source. Set that to `server.url()`
in tests.

## Running tests

```bash
# All unit tests, default features:
cargo test --lib

# All unit tests, all backends:
cargo test --lib --all-features

# All unit tests, shell-out backends:
cargo test --lib --no-default-features

# Integration tests too:
cargo test --all-features

# One module:
cargo test --lib plugins::files

# One specific test:
cargo test --lib plugins::files::tests::writes_plain_text_file_with_perms

# Include `#[ignore]`'d tests:
cargo test -- --ignored

# Just the ignored ones:
cargo test -- --ignored --skip-default

# With output (println!/dbg! visible):
cargo test --lib plugins::files -- --nocapture
```

## Writing a fixture-driven test

`tests/yaml_parse.rs` is the reference. Pattern:

1. Write a heredoc YAML fixture mirroring a Go test's fixture verbatim.
2. Use `indoc!` to keep indentation readable.
3. Parse via `Config::load(y.as_bytes())` or `dot_notation_modifier`
   then `Config::load`.
4. Assert field-by-field. Don't roundtrip-through-string — assert the
   structured result directly.

```rust
use indoc::indoc;
use yip::schema::{Config, File, Stage};

#[test]
fn parses_files_with_b64_content() {
    let y = indoc! {r#"
        stages:
          boot:
            - files:
                - path: /foo/bar
                  permissions: 420
                  encoding: b64
                  content: CmZvbw==
    "#};
    let cfg = Config::load(y.as_bytes()).unwrap();
    let f = &cfg.stages["boot"][0].files[0];
    assert_eq!(f.path, "/foo/bar");
    assert_eq!(f.permissions, 420);
    assert_eq!(f.encoding, "b64");
    assert_eq!(f.content, "CmZvbw==");
}
```

If you're adding a new YAML field, add a fixture test for it here even
if the field is already covered by the plugin's unit tests. The point
of `yaml_parse.rs` is to lock the wire format.

## Marking online-only tests

Anything that needs network access:

```rust
#[test]
#[ignore = "online"]
fn clones_real_repo() {
    let console = StandardConsole::new();
    // ... actually clones from github ...
}
```

The string after `=` is the human-readable reason. `cargo test` skips
these by default; CI doesn't run them. Local devs opt in via
`cargo test -- --ignored`.

For tests that need root (e.g. layout.rs with real partitioning),
use `#[ignore = "root"]`. There's no automatic root-detect skip yet;
the comment is informational.

## Common gotchas

- **`MemVfs` doesn't track perms on parent dirs.** If a test depends on
  parent-directory mode being a specific value, use `TempVfs`.
- **`RecordingConsole` is order-sensitive.** `console.commands()` is in
  call order, not registration order. If the plugin registers a closure
  that runs later (e.g. via `run_template`), the recorded order reflects
  actual execution.
- **`tests/cli.rs` rebuilds the binary.** First `cargo test --test cli`
  in a clean checkout takes a minute. Subsequent runs are fast.
- **`indoc!` strips the common leading indent.** If your fixture looks
  wrong, check that all lines share the same leading whitespace.
- **Don't `assert!(matches!(err, Error::Multi(_)))` without checking the
  inner count.** Wrong counts hide bugs. Pattern-match and assert
  `errs.len() == N`.
- **`#[cfg(test)]` tests can't see `pub(crate)` items unless they're in
  the same crate.** Inline `mod tests` is in the same crate, so this
  isn't usually a problem — but if you split a test out to `tests/`,
  the item has to be `pub`.

## Code coverage

Not part of CI yet. To check locally:

```bash
cargo install cargo-llvm-cov
cargo llvm-cov --lib --all-features --html
# open target/llvm-cov/html/index.html
```

Aim for high coverage on plugin `do_one` branches and conditional
`check` branches. Schema files are mostly tested via `tests/yaml_parse.rs`.
