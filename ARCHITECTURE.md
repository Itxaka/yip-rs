# Architecture

yip-rs is a Rust port of [mudler/yip](https://github.com/mudler/yip): a
declarative YAML stage executor for system bootstrap (cloud-init-ish, but
much smaller and not bound to cloud metadata). One process, no daemon, no
state besides whatever the YAML config tells it to write.

This doc is for people changing the code, not configuring the tool. See
`docs/configuration-reference.md` for the YAML surface.

## 1. High-level overview

```
            +------------+
   argv --> |  CLI       |     src/cli/mod.rs
            |  (clap)    |     parses --stage / paths / subcommands
            +-----+------+
                  |
                  v
            +------------+
            | Executor   |     src/executor/default.rs
            | (resolve   |     resolves paths -> Configs,
            |  + run)    |     dispatches per stage
            +-----+------+
                  |
                  | for each (source, config):
                  v
            +------------+
            | build DAG  |     petgraph::DiGraph<(name, Stage)>
            | toposort   |     Kahn-style; cycles -> error
            +-----+------+
                  |
                  | for each Stage in topo order:
                  v
            +------------+     +-----------------+
            | conditionals|--->| Skip? short-    |
            | preflight   |    | circuit stage   |
            +-----+-------+    +-----------------+
                  |
                  | Run
                  v
            +------------+
            | plugin chain
            | (22 builtin)
            +-----+------+
                 / \
                v   v
          +------+  +------+
          | Vfs  |  | Console
          +------+  +------+
          file ops   shell-out
          (Real /    (Standard /
           Temp /     Recording)
           Mem)
```

Everything below the executor talks to the world through two traits: `Vfs`
(filesystem) and `Console` (subprocess). Tests swap both for in-memory /
recording impls; production wiring uses `RealVfs` + `StandardConsole`.

## 2. Module map

| Module | Purpose |
|--------|---------|
| `src/cli/` | Clap CLI shim. `run(Cli) -> ExitCode`. Dispatches `--stage`, `analyze`, `version`. Owns logging init. |
| `src/executor/` | `Executor` trait + `DefaultExecutor`. Path resolve -> parse -> DAG -> conditionals -> plugins. Multierror aggregation. |
| `src/schema/` | Serde structs for the YAML surface. One submodule per group (`stage`, `file`, `user`, `layout`, ...). `dot_notation_modifier` lives here. |
| `src/plugins/` | 22 action plugins. Each is `pub fn build() -> Plugin` + `pub fn run(stage, fs, console) -> Result<()>`. |
| `src/conditionals/` | 7 gating plugins. Same shape but return `ConditionalOutcome::{Run,Skip}`. |
| `src/vfs/` | `Vfs` trait + `RealVfs` / `TempVfs` / `MemVfs`. |
| `src/console/` | `Console` trait + `StandardConsole` / `RecordingConsole`. |
| `src/template/` | Tera-based renderer with Go-template preprocessor + sprig-subset funcs + sysdata gather. |
| `src/error.rs` | One `Error` enum, `Result<T> = std::result::Result<T, Error>`. `Error::Multi(Vec<Error>)` is the multierror carrier. |
| `src/lib.rs` | Re-exports + `VERSION` / `COMMIT` env consts (set by `build.rs`). |
| `src/main.rs` | Two lines: parse clap, call `cli::run`, exit. |

## 3. Key abstractions

### 3.1 `Vfs` trait

`src/vfs/vfs.rs`. Small surface — just what yip's plugins actually call:

```rust
pub trait Vfs: Send + Sync {
    fn read(&self, path: &Path) -> Result<Vec<u8>>;
    fn write(&self, path: &Path, bytes: &[u8]) -> Result<()>;
    fn mkdir_all(&self, path: &Path) -> Result<()>;
    fn read_dir(&self, path: &Path) -> Result<Vec<PathBuf>>;
    fn metadata(&self, path: &Path) -> Result<Metadata>;
    fn exists(&self, path: &Path) -> bool;
    fn remove(&self, path: &Path) -> Result<()>;
    fn remove_all(&self, path: &Path) -> Result<()>;
    fn chmod(&self, path: &Path, mode: u32) -> Result<()>;
    fn chown(&self, path: &Path, uid: i32, gid: i32) -> Result<()>;
    fn symlink(&self, target: &Path, link: &Path) -> Result<()>;
    fn walk(&self, root: &Path) -> Result<Vec<PathBuf>>;
}
```

Three impls:

- **`RealVfs`** — production. Thin wrapper over `std::fs` + `nix`. No
  remapping; `/etc/passwd` writes to the actual `/etc/passwd`.
- **`TempVfs`** — every guest path is rebased under a tempdir root. Real
  syscalls, but the host filesystem is untouched. Use this when a plugin
  shells out and the shelled command needs to see real files.
- **`MemVfs`** — `HashMap` backed. Fastest, no syscalls. Use this for
  plugins that talk to `Vfs` only and never shell out for filesystem work.

Why a trait at all? Two reasons:

1. Testability — see `src/plugins/files.rs` tests; they build a
   `MemVfs`, run the plugin, and assert on `fs.read(Path::new("/foo"))`
   without ever touching the host.
2. Chroot-ish remapping — `TempVfs::host(guest)` is the same pattern
   immucore-rs uses (`State::path`). Plugins compose against `&dyn Vfs`
   so a caller embedding yip-rs can scope writes under any root.

### 3.2 `Console` trait

`src/console/console.rs`. Subprocess execution.

```rust
pub trait Console: Send + Sync {
    fn run(&self, cmd: &str) -> Result<String>;
    fn run_in(&self, cwd: &Path, cmd: &str) -> Result<String>;
    fn run_with_output(&self, cmd: &str) -> Result<Output> { /* default */ }
    fn run_template(&self, cmds: &[String], template: &str) -> Result<()> { /* default */ }
}
```

`run` spawns `/bin/sh -c <cmd>` and returns combined stdout+stderr. Non-zero
exit → `Error::Cmd { cmd, status, stderr, stdout }`.

- **`StandardConsole`** — production. Calls `Command::new("/bin/sh")`.
- **`RecordingConsole`** — tests. Captures every call into a `Vec` and
  returns either an empty success or a per-command canned response set via
  `expect()`. Lets you assert on command shape without running anything.

`run_template` is a printf-style helper: `run_template(["sshd","cron"],
"systemctl enable %s")` runs `systemctl enable sshd` then `systemctl enable
cron`, aggregating failures.

### 3.3 `Plugin` + `Conditional` type aliases

`src/executor/executor.rs`:

```rust
pub type Plugin =
    Arc<dyn Fn(&Stage, &dyn Vfs, &dyn Console) -> Result<()> + Send + Sync>;

pub type Conditional =
    Arc<dyn Fn(&Stage, &dyn Vfs, &dyn Console) -> Result<ConditionalOutcome>
        + Send + Sync>;

pub enum ConditionalOutcome { Run, Skip }
```

Plugins are arbitrary closures. The registration ceremony is `pub fn build()
-> Plugin { Arc::new(run) }` where `run` is a free function — the closure
type collapses to a thin function pointer because nothing is captured.

Why split conditionals from plugins? In Go yip, conditionals are plugins
that abuse `error != nil` to mean "skip". We have a real tri-state outcome
so a real error from a conditional doesn't get conflated with "this stage
shouldn't run". Errors from conditionals are logged and treated as Skip,
matching Go's observable behaviour.

### 3.4 `Stage` + `Config`

`src/schema/stage.rs` + `src/schema/config.rs`. Pure serde structs.

```rust
pub struct Config {
    pub source: String,                          // #[serde(skip)]
    pub name: String,
    pub stages: HashMap<String, Vec<Stage>>,     // keyed by stage name
}

pub struct Stage {
    pub name: String,
    pub commands: Vec<String>,
    pub files: Vec<File>,
    pub directories: Vec<Directory>,
    // ... 28 more fields ...
    pub after: Vec<Dependency>,
    pub only_if_os: String,
    pub only_if_os_version: String,
    pub only_if_arch: String,
    pub only_if_service_manager: String,
    pub if_files: IfFiles,
}
```

Every field is `#[serde(default, rename = "...")]` to match Go's YAML tags
exactly. Missing keys deserialize to the field's `Default`. Renames are
load-bearing: Go uses `only_os` but the struct field is `OnlyIfOS` →
Rust uses `only_if_os` and renames to `only_os`. Don't drop the rename.

### 3.5 DAG via petgraph + Kahn-style toposort

`src/executor/default.rs::build_dag_for_config`. Two-pass build:

1. Add all nodes (one per stage in the YAML's `stages[key]` array).
2. Wire edges:
   - For each stage with `after: [{name: a}, ...]`, add `a -> me` for
     every matching node (multiple matches → multiple edges).
   - For stages **without** an explicit `after`, also chain to the
     previous lexically-declared stage (yip's implicit ordering). Stages
     that did declare `after` do **not** participate in the lexical chain
     in either direction — otherwise `b after: [a]` followed by `a` would
     produce edges in both directions and cycle.

Then `petgraph::algo::toposort`. Cycles surface as `Error::other("cycle
detected in stage dependencies at node ...")`.

Why not use Go yip's `herd` DAG? `herd` has weak/strong dep edges and
parallel execution. yip uses neither: every stage op registers with
`WeakDeps` identically and execution is serial. A plain topo sort over
petgraph is equivalent and ~150 LOC instead of a vendored DAG library.

## 4. Stage lifecycle

For a single `yip --stage rootfs <paths>` invocation:

```
for source in paths:
    cfgs = resolve_source(source)                # 1
    for (label, cfg) in cfgs:
        for substage in [rootfs.before, rootfs, rootfs.after]:    # 2
            ordered_ops = build_dag_for_config(label, substage, cfg)
            for (op_name, stage) in ordered_ops:                  # 3
                if not check_conditionals(stage):                 # 4
                    continue
                run_plugins(stage, errs)                          # 5
return finish(errs)                                               # 6
```

1. **Resolve.** stdin (`-`), URL (`http(s)://`), file, directory walk
   (`*.yaml` / `*.yml` lex sorted), inline YAML (heuristic: contains `:`
   and `\n`).
2. **Substage expansion.** `rootfs` automatically expands to
   `[rootfs.before, rootfs, rootfs.after]`. Empty substages no-op.
   Stages already suffixed `.before` / `.after` don't re-expand. This
   matches Go's CLI-layer loop but we pulled it into the executor because
   every caller did it identically.
3. **DAG**, see §3.5.
4. **Conditionals preflight.** Runs every registered conditional in
   registration order. First `Skip` short-circuits the plugin chain for
   this stage. Conditional errors are warned and treated as Skip.
5. **Plugin chain.** All plugins run in registration order regardless of
   individual failures. Each failure becomes `Error::Plugin{ plugin,
   source }` and gets pushed onto a `Vec<Error>` per stage. Plugins do
   not abort each other.
6. **Multierror.** All errors from all stages from all sources land in one
   `Vec<Error>`. `finish(errs)`:
   - empty → `Ok(())`
   - one → that error verbatim
   - many → `Error::Multi(errs)`

   CLI's `print_error` unrolls `Error::Multi` into one log line per inner
   error, matching Go's `multierror` output.

## 5. Templating

`src/template/`. Configs are template-rendered **before** YAML parse, so
the rendered text must still be valid YAML.

When fires:
- `DefaultExecutor::parse_bytes` calls `render_template(bytes)` first, then
  applies the configured modifier (`dot_notation_modifier` by default),
  then `Config::load`.
- Currently `render_template` in `default.rs` is a pass-through stub —
  `template::render_with_sysdata` exists but isn't wired in yet (TODO in
  the source).

Sprig subset. Go yip uses `text/template` + `sprig.TxtFuncMap()` (~140
funcs). We layer a hand-picked subset on top of `tera`:

- Go `{{ .Foo.Bar }}` is rewritten to tera `{{ Foo.Bar }}` by `preprocess`.
- `{{ . }}` → `{{ __root__ }}` (a synthetic var holding the root JSON value).
- Go whitespace trim `{{- ... -}}` → dashes stripped (tera's `{%- ... -%}`
  applies only to statements, not expressions).
- Sysdata (`gather_sysdata`) loads `/etc/os-release`, hostname, machine
  UUIDs into a JSON blob exposed to the template root.

Funcs we ship live in `src/template/funcs.rs`. Unimplemented sprig funcs
are documented inline as TODOs.

## 6. Path resolution

CLI gives the executor a list of strings. Each string is one of:

```
stdin?        --- "-" ----------------> read all of stdin
URL?          --- http:// or https:// -> reqwest::blocking::get
exists?       --- file ----------------> fs::read
              \-- directory -----------> walk *.yaml/*.yml sorted
inline YAML?  --- contains ':' and '\n'-> parse the string itself
                                          (label = "<INLINE>")
otherwise     --> Error::other
```

`resolve_source` returns `Vec<(label, Config)>`. The label is what shows up
in op names and error messages — file path, URL, `<STDIN>`, or `<INLINE>`.

Directory walk uses `walkdir::WalkDir::sort_by_file_name` so iteration is
deterministic. Non-`.yaml`/`.yml` files are silently skipped.

## 7. Feature flags

Defined in `Cargo.toml`:

| Flag | Default | Effect |
|------|---------|--------|
| `git-builtin` | on | Native git via `gix`. Off → shell out to `git`. |
| `oci-builtin` | on | Native OCI via `oci-distribution` + per-call tokio runtime. Off → shell out to `skopeo`. |
| `disk-builtin` | on | Native GPT via `gpt` + `mbrman`. Off → shell out to `parted` / `sgdisk`. |
| `nogit` | off | Drops the `git` plugin entirely (overrides `git-builtin`). |
| `nounpack` | off | Drops `unpack_image` entirely (overrides `oci-builtin`). |

Trade-offs:

- **All defaults on**: ~7.1 MB binary stripped+LTO, fully self-contained.
- **`--no-default-features`**: ~4.4 MB binary, but needs `git`, `skopeo`,
  `parted`, `sgdisk`, `partprobe`, `udevadm` on `$PATH` at runtime.
- The `mkfs.*` and `resize2fs` / `xfs_growfs` / `btrfs filesystem resize`
  calls always shell out — no production-quality Rust mkfs/fsresize
  exists. Both backend variants share that code path.

Internal layout: each big plugin (`git.rs`, `unpack_image.rs`, `layout.rs`)
has `backend_native` and `backend_shell` modules behind `cfg(feature = ...)`
flags, with a `make_ops(console)` dispatcher picking the impl. The pure
planning code (e.g. `plan_partitions` in `layout.rs`) lives outside both
backends.

## 8. Comparison to Go yip

| Concept | Go yip | yip-rs |
|---------|--------|--------|
| DAG | spectrocloud-labs/herd | petgraph + Kahn toposort |
| Multierror | `hashicorp/go-multierror` | `Error::Multi(Vec<Error>)` |
| Conditionals | Plugins that return `error != nil` to skip | Real `ConditionalOutcome::{Run,Skip}` enum |
| Substage loop | At CLI call site (`cmd/yip/main.go`) | In executor (`run` / `apply`) |
| stillAlive ticker | 10s goroutine per plugin | Dropped — `tracing` spans give equivalent observability |
| `Plugin` type | Single Go func type for both action + gating | Two type aliases: `Plugin` and `Conditional` |
| `Executor` interface | 6 methods | 3 methods (`run`, `apply`, `analyze`); the construction-time setters are builder methods on `DefaultExecutor` |
| Templating | `text/template` + sprig | `tera` + sprig-subset + Go-syntax preprocessor |
| Vfs | `twpayne/go-vfs` | Hand-rolled trait, same surface |
| Git | shells out to `git` | native `gix` (default) or shells out |
| OCI pull | shells out to `skopeo` | native `oci-distribution` (default) or shells out |
| GPT | shells out to `parted` / `sgdisk` | native `gpt` crate (default) or shells out |
| Dot-notation parser | gojq | Hand-rolled ~150 LOC, no jq dep |
| Owner field | `Owner int` + parallel `OwnerString string` | Single `OwnerId::{Numeric, Name}` enum + tolerant deserialize |

Behavioural parity is the target. Where we deviate, the source file has a
module-level comment explaining why (search for `Differences from the Go
version`). The reference fixture suite in `tests/yaml_parse.rs` reproduces
selected cases from Go's `pkg/schema/schema_test.go` and
`pkg/executor/default_test.go` to keep us honest.

## Where to look next

- `src/executor/default.rs` — start here if you want to understand any
  runtime behaviour. The whole control flow is in one file.
- `src/plugins/files.rs` — canonical plugin template.
- `src/conditionals/if_arch.rs` — canonical conditional template.
- `tests/yaml_parse.rs` — fixture-driven schema parity tests.
- `tests/cli.rs` — black-box CLI tests.
- `docs/adding-a-plugin.md` — step-by-step for adding to the plugin set.
- `docs/testing.md` — how tests are structured, what to use when.
