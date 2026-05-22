# Contributing to yip-rs

Thanks for picking this up. yip-rs is a port of [mudler/yip](https://github.com/mudler/yip),
so the design pressure is "match Go's observable behaviour first, then
clean up". When in doubt, check what Go does and copy it.

## Repo layout

```
.
├── Cargo.toml             # crate manifest + feature flags
├── rust-toolchain.toml    # pinned toolchain (stable + rustfmt + clippy)
├── build.rs               # injects YIP_VERSION / YIP_COMMIT env vars
├── src/
│   ├── lib.rs             # re-exports + version consts
│   ├── main.rs            # 2-line clap entrypoint
│   ├── cli/               # clap definitions
│   ├── executor/          # stage runner, DAG, plugin chain
│   ├── schema/            # YAML serde structs
│   ├── plugins/           # 22 action plugins
│   ├── conditionals/      # 7 gating plugins
│   ├── vfs/               # filesystem trait + 3 impls
│   ├── console/           # subprocess trait + 2 impls
│   ├── template/          # tera + sprig-subset
│   └── error.rs           # one Error enum, Result alias
├── tests/
│   ├── yaml_parse.rs      # schema fixture parity tests
│   ├── cli.rs             # black-box binary tests
│   └── fixtures/          # YAML fixtures used by integration tests
└── docs/                  # developer docs (this file + friends)
```

See `ARCHITECTURE.md` for what each module actually does.

## Dev setup

Toolchain is pinned by `rust-toolchain.toml`:

```toml
[toolchain]
channel = "stable"
components = ["rustfmt", "clippy"]
```

`rustup` will install the right toolchain automatically the first time
you `cargo` in this directory. No `nightly`, no `MSRV` magic — the
`rust-version = "1.85"` field in `Cargo.toml` is the minimum.

System deps you may need at runtime depending on feature flags:

- Default build (`cargo build`): self-contained, only needs `/bin/sh`.
- `--no-default-features`: needs `git`, `skopeo`, `parted`, `sgdisk`,
  `partprobe`, `udevadm` on `$PATH` when those plugins run. Tests don't
  exercise these paths by default.

Always-shelled-out (regardless of features): `mkfs.*`, `resize2fs`,
`xfs_growfs`, `btrfs filesystem resize`, `modprobe`, `systemctl`,
`hostnamectl`, plus most package managers.

## Running tests

```bash
cargo test --lib                            # unit tests only
cargo test --all-features                   # everything, all backends
cargo test --no-default-features            # shell-out backends only
cargo test -- --ignored                     # online-only tests
cargo test --lib plugins::files             # one module
cargo test --lib plugins::files::tests::writes_plain_text_file_with_perms
                                            # one test (full path)
```

What runs where:

- `src/**/tests` inline modules — unit tests. Mockito for HTTP, MemVfs
  for filesystem, RecordingConsole for shell-out. No network unless
  marked `#[ignore = "online"]`.
- `tests/yaml_parse.rs` — fixture parity with Go's
  `pkg/schema/schema_test.go`. Pure parsing.
- `tests/cli.rs` — black-box, shells out to the built binary via
  `assert_cmd`. Verifies exit codes, stdout, stderr.

See `docs/testing.md` for fixture conventions and how to write each test
flavour.

## Lint

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo clippy --all-targets --no-default-features -- -D warnings
```

Both `cargo fmt --check` and `cargo clippy -D warnings` must pass before
merge. CI runs them as gating jobs (`.github/workflows/ci.yml`).

Clippy is strict — `-D warnings` means any new warning fails the build.
If clippy fights you on a specific lint that's wrong for this codebase,
silence it locally with `#[allow(clippy::lint_name)]` and a one-line
comment, not a project-wide opt-out.

## Commit style

Look at `git log --oneline` to see the shape. Conventional Commits prefix
(`feat:`, `fix:`, `chore:`, `feat(wave-X):`) and a single-line subject ≤
~72 chars. Body is **expected** for anything non-trivial — describe
what changed, why it changed, and any deviation from the Go original.
Bullet points are fine; prose is fine; code excerpts in fenced blocks
are fine when they clarify the change.

Example (real, from this repo):

```
feat(wave-3): 12 simple plugins

Four parallel agents landed 12 plugins across file ops, config files,
systemd, and entity (passwd/shadow) editing.

## File operations (commands.rs, files.rs, directories.rs)

- **commands**: iterates `stage.commands`, calls `console.run` per entry,
  aggregates failures into `Error::Multi`. Empty list silent no-op.
...
```

Bad commit messages:

- `fix bug` — what bug? in what?
- `update plugin` — which plugin? what about it?
- `wip` — squash before merging, don't leave wip in the log.

If you're adding a plugin or conditional, mention the matching Go file
in `pkg/plugins/*.go` so reviewers can cross-check parity.

## PR checklist

Before opening:

- [ ] `cargo fmt --all -- --check` passes
- [ ] `cargo clippy --all-targets --all-features -- -D warnings` passes
- [ ] `cargo clippy --all-targets --no-default-features -- -D warnings` passes
- [ ] `cargo test --all-features` passes locally
- [ ] `cargo test --no-default-features` passes locally
- [ ] New plugins / conditionals have unit tests using MemVfs +
      RecordingConsole at minimum
- [ ] If behaviour diverges from Go yip, the source file has a comment
      block explaining the divergence
- [ ] If the change touches the YAML schema, `tests/yaml_parse.rs` has
      a parsing test for the new field
- [ ] Configuration changes are also documented in
      `docs/configuration-reference.md`
- [ ] `CHANGELOG.md` has an `[Unreleased]` entry under the right header

PR description should answer: what changed, why, what Go yip behaviour
this matches (or deliberately doesn't), how it was tested.

## Verifying against Go behaviour

When you change anything that could differ from Go:

1. Find the matching Go file. The schema submodules list it in their
   header comment; plugins typically map 1:1 (`src/plugins/files.rs` ↔
   `pkg/plugins/files.go`).
2. Read the Go source. Note edge cases, error handling, ordering.
3. If yip-rs deviates, decide:
   - Bug-for-bug compat → match Go exactly, even if Go is weird.
   - Deliberate improvement → document in the file's module header
     ("Differences from the Go version: ...").
4. If there's a Go test that exercises the case, copy the fixture into
   `tests/yaml_parse.rs` (for schema) or add an equivalent unit test
   (for plugins).

The `default.rs` and most plugin files already have a "Differences from
the Go version" comment block — extend it, don't shrink it.

## When to ask

- The thing you want to add doesn't exist in Go yip → open an issue
  before writing code. Schema extensions break compatibility.
- A test fails on a path that's clearly Go-derived but the assertion
  looks wrong → check `git log` for the original commit, often the Go
  behaviour was intentional.
- You're about to touch the DAG construction in
  `executor/default.rs::build_dag_for_config` → re-read the lexical-vs-
  explicit edge-wiring comments first. There's a reason `b after: [a]`
  followed by `a` doesn't cycle, and it's easy to break.

## What goes in CHANGELOG.md

`CHANGELOG.md` is the user-facing change log. Group entries under one of
`Added` / `Changed` / `Fixed` / `Removed` headers. One bullet per
user-visible change. Internal refactors that don't change behaviour
don't belong here (they belong in git log).
