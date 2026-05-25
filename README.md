# yip-rs

Rust port of [yip](https://github.com/mudler/yip) — the cloud-init-style YAML stage executor used by Kairos and friends.

Status: **early bootstrap**. Not production-ready. The Go binary at `mudler/yip` is still the canonical implementation.

## What it does

Applies declarative YAML configuration to a system: writes files, runs commands, creates users, installs packages, configures systemd units, mounts filesystems, sets sysctls, downloads artifacts, clones git repos, unpacks OCI images, etc. Same input format as Go yip — the Rust port aims for byte-for-byte schema parity so existing config consumes without modification.

## Quickstart

Three commands, soup-to-nuts:

```bash
# 1. build
cargo build --release

# 2. write a config
cat > /tmp/hello.yaml <<'EOF'
name: hello
stages:
  default:
    - name: greet
      commands:
        - echo "hello from yip-rs"
      files:
        - path: /tmp/yip-hello.txt
          content: "hi\n"
          permissions: 0644
EOF

# 3. apply
./target/release/yip --stage default /tmp/hello.yaml
```

More CLI detail in [USAGE.md](USAGE.md). Full schema in [docs/configuration-reference.md](docs/configuration-reference.md). Coming from Go yip? Read [docs/migrating-from-go-yip.md](docs/migrating-from-go-yip.md) first.

## Library + binary

```rust
use yip::executor::{DefaultExecutor, Executor};
use yip::console::StandardConsole;
use yip::vfs::RealVfs;

let exec = DefaultExecutor::new();
let fs = RealVfs::new();
let console = StandardConsole::new();
exec.run("rootfs", &fs, &console, &["/etc/yip/conf.d".to_string()])?;
```

```bash
yip --stage rootfs /etc/yip/conf.d
```

## Go yip vs yip-rs

High-level differences. Schema is identical; runtime and dependency footprint are not.

| Aspect | Go yip | yip-rs |
|---|---|---|
| Binary size (stripped, default features) | ~25-30 MB | ~12-18 MB |
| Binary size (no `oci-builtin`, no `git-builtin`) | n/a | ~4-6 MB |
| Runtime deps (default build) | none (statically linked) | none (statically linked) |
| Runtime deps (minimal build) | none | `git`, `skopeo`, `parted`/`sgdisk` (when those features off) |
| DAG engine | `spectrocloud-labs/herd` | hand-rolled topo sort over `petgraph` |
| Conditional skip semantics | error-as-skip | tri-state (`Run` / `Skip`) — error treated as `Skip` |
| Templating | `text/template` + full sprig (~140 funcs) | `tera` + sprig subset (~60 funcs) |
| OCI pull | `go-containerregistry` | `oci-distribution` (rustls) or shell-out to `skopeo` |
| Git | `go-git` / `git` binary | `gix` (gitoxide) or shell-out to `git` |
| Disk partitioning | `diskfs/go-diskfs` (in-process) | `gpt`/`mbrman` (in-process); `mkfs` shells out |
| Cloud-init `#cloud-config` parser | yes (custom loader) | not yet (uses raw yip YAML) |
| Plugin count | 23 | 22 (entities + delete_entities share build; unpack_image is one) |
| Conditional count | 7 | 7 |
| Tests | go test | ginkgo-free; pure `cargo test` |
| License | Apache-2.0 | Apache-2.0 |

## Feature flag matrix

Each flag toggles an embedded backend. Disable to shrink the binary at the cost of needing the matching system binary on `$PATH`.

| Feature | Default | Plugin | Native impl | Fallback when off | Approx size delta |
|---|---|---|---|---|---|
| `git-builtin` | on | `git` | `gix` (gitoxide) | shell out to `git` | ~+3-4 MB |
| `oci-builtin` | on | `unpack_image` | `oci-distribution` + `tokio` | shell out to `skopeo` | ~+6-8 MB |
| `disk-builtin` | on | `layout` | `gpt` + `mbrman` (mkfs still shells out) | shell out to `parted` / `sgdisk` | ~+1-2 MB |
| `nogit` | off | `git` | — | plugin compiled out entirely | ~-3-4 MB |
| `nounpack` | off | `unpack_image` | — | plugin compiled out entirely | ~-6-8 MB |

Minimum-size build:

```bash
cargo build --release --no-default-features --features "" \
  -- # nothing
# add `nogit nounpack` to also drop the plugins themselves.
```

Default build:

```bash
cargo build --release
# == --features "git-builtin oci-builtin disk-builtin"
```

## Plugin status

Mirrors the 23 Go plugins. `Done` = full behavioural port. `Partial` = works for the common case but Go has paths we don't. `Stubbed` = compiles, does nothing useful yet. `TODO` = not present.

| Plugin | Status | Notes |
|---|---|---|
| `commands` | Done | Shells out per entry; templating deferred to higher layer. |
| `files` | Done | Encodings: raw / b64 / gzip / gz+b64. Name-based chown not yet wired. |
| `directories` | Done | Same chown caveat as `files`. |
| `downloads` | Done | `reqwest` blocking client; honours `timeout`. |
| `dns` | Done | Writes resolv.conf; no-op when nameservers empty (matches Go). |
| `hostname` | Done | Writes /etc/hostname + machine-id, calls `sethostname(2)`. |
| `sysctl` | Done | `/proc/sys` writes only — no `/etc/sysctl.d` (matches Go). |
| `ssh` | Done | Resolves `github:`, `gitlab:`, http(s), raw; dedupes; appends. |
| `modules` | Done | Shells out to `modprobe` (Go uses syscalls; behaviour equivalent). |
| `environment` | Done | Merges `/etc/environment`; godotenv-flavour quoting. |
| `entities` / `delete_entities` | Done | Native parser; supports user / group / shadow / gshadow. |
| `systemctl` | Done | enable/disable/start/mask + drop-in overrides; auto `daemon-reload`. |
| `systemd_firstboot` | Done | Single shell-out with sorted flags. |
| `timesyncd` | Done | Overwrites `/etc/systemd/timesyncd.conf` deterministically. |
| `users` | Done | Native passwd/shadow/group writer; sha512crypt hashing. |
| `packages` | Partial | apt / dnf / apk / zypper detection. No `versionlock` etc. |
| `package_pins` | Partial | apt + dnf + apk file forms. No `zypper`. |
| `git` | Done | `gix` native or shell-out fallback. |
| `layout` | Partial | gpt/mbr native; ext/xfs/btrfs grow via `resize2fs`/`xfs_growfs`/`btrfs` shell-out. |
| `unpack_image` | Done | `oci-distribution` native or `skopeo` fallback; whiteouts supported. |
| `datasource` | Done | All 14 Go providers: aws, nocloud, azure, gcp, openstack, digitalocean, scaleway, hetzner, packet, vultr, metaldata, vmware, cdrom, config-drive, file. |

| Conditional | Status | Notes |
|---|---|---|
| `node` | Done | Hostname match. |
| `if` | Done | Shell-out boolean; non-zero exit = skip. |
| `only_if_os` | Done | Parses /etc/os-release ID. |
| `only_if_os_version` | Done | semver-ish comparison on os-release VERSION_ID. |
| `if_arch` | Done | Aliases Rust target arch to Go names (`x86_64`→`amd64`, `aarch64`→`arm64`, …) so Go configs work unchanged. |
| `if_service_manager` | Done | Detects systemd / openrc / runit. |
| `if_files` | Done | `any` / `all` / `none` semantics. |

## Layout

```
src/
├── lib.rs            # library root
├── main.rs           # binary entrypoint
├── cli/              # clap definitions, run() entry
├── schema/           # YAML schema: Stage, User, File, Layout, ...
├── executor/         # stage runner, DAG, plugin chain
├── console/          # console abstraction (shell-out vs mock)
├── vfs/              # filesystem abstraction (real, tempdir, in-memory)
├── conditionals/     # 7 conditional plugins (if, only_os, only_arch, …)
├── plugins/          # 22 action plugins (files, commands, users, …)
├── template/         # sprig-subset template engine
└── error.rs          # error types
```

## Feature flags

| Feature | Default | Effect |
|---|---|---|
| `git-builtin` | on | Embed `gix` for git operations. Disable to shrink binary and shell out to `git`. |
| `oci-builtin` | on | Embed `oci-distribution` for OCI registry pulls. Disable to shell out to `skopeo`. |
| `disk-builtin` | on | Embed `gpt`/`mbrman` for partitioning. Disable to shell out to `parted`/`sgdisk`. |
| `nogit` | off | Drop the `git` plugin entirely. |
| `nounpack` | off | Drop the `unpack_images` plugin entirely. |

## License

Apache-2.0 (same as upstream yip). See `LICENSE`.
