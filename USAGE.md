# yip-rs CLI usage

Detailed reference for the `yip` binary. For the YAML schema see [`docs/configuration-reference.md`](docs/configuration-reference.md). Coming from Go yip? See [`docs/migrating-from-go-yip.md`](docs/migrating-from-go-yip.md).

## Synopsis

```
yip [--stage <name>] [--log-level <lvl>] <path|url|->...
yip analyze --stage <name> <path>...
yip version
```

## Top-level flags

| Flag | Short | Default | Description |
|---|---|---|---|
| `--stage <name>` | `-s` | (none) | Stage name to apply (e.g. `rootfs`, `initramfs`, `boot`). When set without a subcommand, runs that stage against `<path>...`. |
| `--log-level <lvl>` | — | `info` | One of `off`, `error`, `warn`, `info`, `debug`, `trace`. |
| `--version` / `-V` | — | — | Print version, exit. |
| `--help` / `-h` | — | — | Print clap-generated help. |

`--stage` without paths is an error. `--stage` with a path or URL is the common case.

## Subcommands

### `yip analyze --stage <name> <path>...`

Dry-run. Loads each path, builds the DAG, prints the ordered op-name list, exits. No side effects.

```bash
$ yip analyze --stage rootfs /etc/yip/conf.d/10-base.yaml
# /etc/yip/conf.d/10-base.yaml
  10-base.before.pre
  10-base.main
  10-base.after.post
```

Restriction vs `--stage`: `analyze` only accepts file/dir paths. URLs, stdin, and inline YAML are rejected (the executor's resolver is private; analyze uses a simpler walker).

### `yip version`

Prints `yip <semver> (commit <sha>)` and exits 0. Long form (`--version`) prints the same.

## Path source types

`<path>` arguments resolve in this order (first match wins):

| Form | Example | Notes |
|---|---|---|
| stdin sentinel | `-` | Reads stdin to EOF, parses as one config. Use with `cat foo.yaml \| yip --stage rootfs -`. |
| URL | `https://example.com/conf.yaml` | HTTP(S) GET. Non-2xx is an error. No retry. |
| File | `/etc/yip/foo.yaml` | Single file, must exist, parsed as one config. |
| Directory | `/etc/yip/conf.d` | Walked recursively. Only `*.yaml` / `*.yml` are picked up; sorted lexicographically. |
| Inline YAML | `name: x\nstages: {default: [{commands: [id]}]}` | Heuristic: contains `:` and `\n`. Useful for one-liners. |

You can mix sources in one call:

```bash
yip --stage rootfs /etc/yip/base.yaml https://example.com/cloud-init.yaml -
```

Each source is resolved independently. Errors from one source do not abort the rest; the executor aggregates them into a single multi-error printed at the end.

## Stage naming conventions

When you pass `--stage <X>`, yip-rs runs three stages in order against every source:

1. `<X>.before`
2. `<X>`
3. `<X>.after`

Missing substages are silent no-ops. This matches Go yip's call-site loop.

If you pass an explicit substage (`--stage rootfs.before`), yip-rs skips the expansion and runs only that one. The check is suffix-based: `.before` or `.after`.

Canonical stage names used by Kairos:

| Stage | When |
|---|---|
| `rootfs` | Inside initramfs, before switch_root. |
| `initramfs` | After switch_root, inside the new root. |
| `boot` | systemd / openrc first-up after `initramfs`. |
| `network` | After network is online. |
| `reconcile` | Periodic / on-demand drift correction. |

The names are conventions, not magic — yip-rs only cares that the key under `stages:` in the YAML matches whatever you pass to `--stage`.

## Examples

Apply one file:

```bash
yip --stage rootfs /etc/yip/conf.d/00-bootstrap.yaml
```

Apply a whole directory (sorted load order):

```bash
yip --stage rootfs /etc/yip/conf.d
```

Apply from a URL:

```bash
yip --stage boot https://example.com/cloud-init.yaml
```

From stdin via shell pipe:

```bash
cat my.yaml | yip --stage default -
```

Inline YAML, no file:

```bash
yip --stage default $'name: x\nstages:\n  default:\n    - commands: [id]\n'
```

Dry-run plan:

```bash
yip analyze --stage rootfs /etc/yip/conf.d
```

## Logging

Two independent knobs:

- `--log-level <lvl>` — clap flag, accepts a single tracing level token.
- `RUST_LOG` — standard `tracing` env-filter. Wins over `--log-level` if set to a valid filter expression. Use it for per-module filtering:

```bash
RUST_LOG=yip=debug,yip::plugins::files=trace yip --stage rootfs /etc/yip/conf.d
```

Logs go to stderr. stdout is reserved for `analyze` output and the `version` line. This means you can pipe `yip analyze ... | grep` without log noise.

Default format is `tracing-subscriber`'s pretty single-line. There is no JSON output mode yet.

## Exit codes

| Code | Meaning |
|---|---|
| `0` | Success — every source applied without plugin errors. |
| `1` | At least one source / stage / plugin failed; or bad CLI args. |

yip-rs does not split error vs failure exit codes the way some tools do. A single failed plugin in a single source returns `1` and the multi-error is printed to stderr. The DAG still runs every other op — failures are accumulated, not short-circuited.

## Embedding as a library

Add the crate:

```toml
[dependencies]
yip = { git = "https://github.com/kairos-io/yip-rs", rev = "..." }
```

Minimal embedding:

```rust
use yip::executor::{DefaultExecutor, Executor};
use yip::console::StandardConsole;
use yip::vfs::RealVfs;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let exec = DefaultExecutor::new();
    let fs = RealVfs::new();
    let console = StandardConsole::new();

    // Apply a stage from a path.
    exec.run("rootfs", &fs, &console, &["/etc/yip/conf.d".to_string()])?;

    Ok(())
}
```

Apply a pre-parsed `Config` (skips path resolution):

```rust
use yip::schema::Config;
use yip::executor::{DefaultExecutor, Executor};
use yip::console::StandardConsole;
use yip::vfs::RealVfs;

let cfg = Config::load_file("/etc/yip/foo.yaml")?;
let exec = DefaultExecutor::new();
exec.apply("rootfs", &cfg, &RealVfs::new(), &StandardConsole::new())?;
```

Inject a recording `Console` for tests:

```rust
use yip::console::RecordingConsole;
use yip::vfs::MemVfs;

let console = RecordingConsole::new();
let fs = MemVfs::new();
// ... run executor ...
for call in console.calls() {
    println!("{}", call.cmd);
}
```

Custom plugin chain (drop the default set, add your own):

```rust
use yip::executor::DefaultExecutor;

let exec = DefaultExecutor::empty()
    .with_plugin("files", yip::plugins::files::build())
    .with_plugin("commands", yip::plugins::commands::build());
```

`Executor` is `Send + Sync`. The DAG itself is single-threaded today; plugins may spawn their own work.

## Running from a systemd unit

Drop-in unit, runs the `boot` stage after multi-user is up:

```ini
[Unit]
Description=Apply yip boot stage
After=network-online.target
Wants=network-online.target
ConditionPathExists=/etc/yip/conf.d

[Service]
Type=oneshot
Environment=RUST_LOG=info
ExecStart=/usr/bin/yip --stage boot /etc/yip/conf.d
RemainAfterExit=yes
StandardOutput=journal
StandardError=journal

[Install]
WantedBy=multi-user.target
```

Notes:

- `Type=oneshot` matches yip's "run to completion, no daemon" model.
- `RemainAfterExit=yes` so dependent units can `After=yip-boot.service`.
- Stage failure → unit fails. Combine with `OnFailure=` for alerting.
- Use `ConditionPathExists=` to skip when the conf.d directory is absent (otherwise yip exits 1 on "no paths supplied").

For early-boot use (initramfs), build with `--features ""` (no defaults) to keep the binary small. See README for the size matrix.
