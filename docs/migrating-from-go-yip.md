# Migrating from Go yip

For people coming from `mudler/yip` (Go) wanting to swap in `kairos-io/yip-rs` (Rust). Read top to bottom; the gotchas in the middle bite.

## TL;DR

- YAML schema is the same. Your configs parse unchanged.
- CLI shape is the same: `yip --stage <name> <paths...>`.
- Arch names are **different** (`x86_64` not `amd64`).
- Templating is sprig-subset (~60/140 funcs). Most yip configs use ~5.
- A few providers / pinning backends are stubbed. Check the status table.
- Binary is smaller and shells out for git / OCI / disk *only if* you turn the corresponding feature off.

## What's identical

| Area | Notes |
|---|---|
| Top-level YAML shape | `name:` + `stages: {<name>: [<step>]}`. |
| Stage struct | Same 32 fields, same YAML keys (modulo doc'd field renames already present in Go). |
| Stage substage expansion | `--stage rootfs` runs `rootfs.before` → `rootfs` → `rootfs.after`. |
| `after:` dependency wiring | Same DAG semantics; topological order; cycle = error. |
| Conditional names | `if`, `only_os`, `only_os_version`, `only_arch`, `only_service_manager`, `if_files`, `node`. |
| File encodings | `""` / `string` / `b64` / `base64` / `gzip` / `gz+b64` / `b64+gz`. |
| Auth shape for git | `username` / `password` / `private_key` / `public_key` / `insecure`. |
| Multi-error semantics | Plugin errors accumulate; the chain never short-circuits. |
| `--stage <X>` precedence | `X.before` < `X` < `X.after`; missing substages silent no-op. |
| Lexical chain for stages without `after:` | Implicit; matches Go's `prev` wiring. |
| Stage duplicate-name handling | Suffixed with `.<index>` (matches Go `checkDuplicates`). |
| Path resolution order | stdin (`-`) → URL → file → dir → inline YAML heuristic. |
| Directory walk | Recursive; `*.yaml` + `*.yml`; sorted lexicographically. |

## What's different

### Arch names

| Go yip | yip-rs |
|---|---|
| `amd64` | `x86_64` |
| `arm64` | `aarch64` |
| `arm` | `arm` |
| `386` | `x86` |
| `riscv64` | `riscv64` |
| `ppc64le` | `powerpc64` |
| `s390x` | `s390x` |

Anything that says `only_arch: amd64` needs to become `only_arch: x86_64`. yip-rs uses Rust's `std::env::consts::ARCH` directly — there's no Go-style normalisation layer.

### Templating

Go yip uses `text/template` + the full sprig funcmap (~140 functions). yip-rs uses `tera` + a sprig-subset funcmap (~60 functions).

Two real concerns:

1. **Field access**: Go's `{{ .Foo.Bar }}` and tera's `{{ Foo.Bar }}` are both accepted (leading dots are rewritten by a pre-processor).
2. **Missing funcs**: render-time error with a clear message. The common ones (`lower`, `upper`, `trim`, `replace`, `split`, `join`, `quote`, `default`, `len`, `b64enc`, `b64dec`, `sha256sum`, etc) are all present. The exotic ones (date arithmetic, semver comparisons, cryptography beyond hashing, `regexReplaceAll`, etc) are not.

If your config uses `regexReplaceAll`, `dateModify`, `semverCompare`, `htpasswd`, `genCA`, etc — those will error. Open an issue with the function name; most are mechanical to add.

### Git

| | Go yip | yip-rs |
|---|---|---|
| Default backend | `go-git` (pure Go) | `gix` / gitoxide (pure Rust) |
| Fallback backend | shell-out to `git` (`gitbinary` build tag) | shell-out to `git` (when `git-builtin` is off) |
| Default branch | `master` | `master` (kept for parity) |
| Auth modes | basic, SSH key, insecure-TLS | basic, SSH key, insecure-TLS |

Both default backends are pure-language clients. Behaviour is observably identical for the operations yip does (clone, fetch + reset --hard, checkout); shell-out is a feature-flag away if you ship without the embedded backend.

### OCI

| | Go yip | yip-rs |
|---|---|---|
| Default backend | `go-containerregistry` | `oci-distribution` + tokio (rustls) |
| Fallback | none (always in-process) | shell out to `skopeo` (when `oci-builtin` is off) |
| Layer extract | in-process | in-process, identical whiteout rules |

yip-rs's `unpack_image` runs a tiny per-call tokio runtime to bridge `oci-distribution`'s async API into the otherwise-sync executor. There is no global runtime.

### Disk layout

This is the biggest divergence.

| | Go yip | yip-rs |
|---|---|---|
| Partition table | `diskfs/go-diskfs` (in-process) | `gpt` + `mbrman` (in-process) |
| `partprobe`/`BLKPG` | in-process ioctls | shell-out to `partprobe` |
| ext fs grow | in-process ioctl (`EXT4_IOC_RESIZE_FS`) | shell-out to `resize2fs` |
| xfs grow | in-process ioctl (`XFS_IOC_FSGROWFSDATA`) | shell-out to `xfs_growfs` |
| btrfs grow | in-process ioctl (`BTRFS_IOC_RESIZE`) | shell-out to `btrfs filesystem resize` |
| FS type detection | superblock magic bytes | shell-out to `blkid` |
| mkfs | shell-out (both) | shell-out (both) |

There is no production-quality Rust equivalent of `go-diskfs` yet. We do what we can in-process (gpt/mbr table edits) and shell out for the rest. Net effect: yip-rs needs `partprobe`, `blkid`, `resize2fs`, `xfs_growfs`, `btrfs`, and the relevant `mkfs.*` binaries on `$PATH` when `layout:` is used. Go yip needs only `mkfs.*`.

### `chroot` and process model

Go yip uses `syscall.Chroot` via its console wrapper. yip-rs's `StandardConsole` is a plain `std::process::Command` runner — no chroot helper. If you previously relied on a chroot-aware executor, do it explicitly in your wrapper:

```rust
nix::unistd::chroot("/sysroot")?;
// ... then run the executor ...
```

The `Console` trait is small enough that you can implement a chrooting version yourself in tens of lines.

### `hostname` machine-id source

Go yip reads `/etc/machine-id` via `denisbrodbeck/machineid` (also tries D-Bus). yip-rs generates a fresh v4-UUID-derived machine-id when the file doesn't exist. Only matters if you're running the `hostname` plugin in a context where the existing machine-id is significant.

### `sysctl`

Both implementations write only to `/proc/sys`. Neither persists to `/etc/sysctl.d/`. If you need persistence, use a `files:` entry.

### `timesyncd`

| | Go yip | yip-rs |
|---|---|---|
| Strategy | Load + merge + write via `gopkg.in/ini.v1` | Overwrite deterministically (sorted keys) |
| Preserves untouched keys | yes | **no** |

yip-rs always overwrites `/etc/systemd/timesyncd.conf` with `[Time]` + sorted `KEY=VALUE`. Go yip merges, preserving pre-existing keys you didn't touch.

If you rely on partial-update merge semantics — don't.

### `directories` chmod on intermediates

Go yip applies perms to each freshly-created intermediate directory. yip-rs uses `mkdir_all` and applies perms only to the final path. Intermediates get the umask default.

### Logging

| | Go yip | yip-rs |
|---|---|---|
| Library | `logrus` | `tracing` + `tracing-subscriber` |
| Default level | `info` | `info` |
| Env override | `LOGRUS_LEVEL` (sometimes) | `RUST_LOG` (standard) |
| Format | logrus pretty | tracing pretty single-line |
| JSON mode | yes | not yet |

### Plugin count

Go yip registers 23 plugins; yip-rs registers 22. The difference: Go has two build variants of `unpack_image` (enabled/disabled-by-tag) that yip-rs collapses behind a feature flag. Behaviour is the same; counting differs.

### `#cloud-config` parsing

Go yip detects `#cloud-config` headers and routes them through a cloud-init-compat loader that maps `runcmd:` → commands, `write_files:` → files, etc. **yip-rs does not.** Pass yip-native YAML only.

If you have cloud-init YAML to feed in, pre-translate it. The mapping is straightforward but not yet ported.

## Plugin status diff

Full list. `Done` = full port. `Partial` = common path works, edge cases not yet. `Stubbed` = compiles, does nothing useful yet. `TODO` = not present.

| Plugin | Status | Diff vs Go |
|---|---|---|
| `commands` | Done | None functionally. Templating deferred to executor pre-render. |
| `files` | Done | Name-based chown skipped with warn (TODO). |
| `directories` | Done | Intermediate chmod not replicated. Name-based chown skipped. |
| `downloads` | Done | `reqwest` blocking. Honours `timeout`. |
| `dns` | Done | Identical. No-op on empty nameservers (matches Go). |
| `hostname` | Done | machine-id source differs (v4 UUID vs reading existing). |
| `sysctl` | Done | Identical. |
| `ssh` | Done | Identical. |
| `modules` | Done | Shells out to `modprobe` (Go uses syscalls). |
| `environment` | Done | Vendored godotenv-flavour parser. |
| `entities` / `delete_entities` | Done | Native parser; same kinds. |
| `systemctl` | Done | Adds an automatic `daemon-reload` after writing overrides. |
| `systemd_firstboot` | Done | Identical. |
| `timesyncd` | Done* | Overwrites file instead of merging. |
| `users` | Done | Native passwd/shadow/group writer; sha512crypt. |
| `packages` | Partial | apt/dnf/apk/zypper detection; no exotic flags. |
| `package_pins` | Partial | apt + dnf + apk forms; zypper skipped. |
| `git` | Done | Native via `gix` or shell-out. |
| `layout` | Partial | gpt/mbr in-process; fs-grow and partprobe shell out. |
| `unpack_image` | Done | Native `oci-distribution` or `skopeo` fallback. |
| `datasource` | Partial | aws + nocloud only. Others stubbed. |

| Conditional | Status | Diff vs Go |
|---|---|---|
| `node` | Done | hostname match. |
| `if` | Done | shell-out boolean. |
| `only_if_os` | Done | parses /etc/os-release. |
| `only_if_os_version` | Done | semver-ish comparison. |
| `if_arch` | Done | **uses Rust arch names (`x86_64` not `amd64`)**. |
| `if_service_manager` | Done | systemd / openrc / runit. |
| `if_files` | Done | any / all / none. |

## Migration checklist

Five things to verify before swapping the binary on a live system:

- [ ] **Arch names.** Grep your configs for `only_arch: amd64` / `only_arch: arm64` and rename to `x86_64` / `aarch64`. There is no compatibility shim.
- [ ] **Templating functions.** Run `yip analyze --stage <X> /etc/yip/conf.d` and skim — if any stage uses templating, eyeball the function names. Anything beyond the common sprig set (`lower`, `upper`, `trim`, `replace`, `split`, `join`, `quote`, `default`, `len`, `b64enc`, `b64dec`, `sha256sum`) might error at render time. Open an issue for missing funcs.
- [ ] **Datasource providers.** If you use `datasource:` with anything other than `aws` or `nocloud`, you'll get `Error::Other("provider X not yet ported")`. Either gate the stage with `only_if_os` until the provider lands, or carry Go yip for that one stage.
- [ ] **Disk layout deps.** If your config has a `layout:` section, ensure `partprobe`, `blkid`, `resize2fs` / `xfs_growfs` / `btrfs`, and `mkfs.*` are in the binary's `PATH`. Go yip needed only mkfs.
- [ ] **`#cloud-config` files.** If you pass any cloud-init style files (`#cloud-config` header), pre-translate them to yip-native YAML. yip-rs has no cloud-init detection branch yet.

Once those clear: build, swap, run with `--log-level debug` for the first boot or two, watch for `not yet ported` / `not implemented` warnings.

## When to NOT migrate yet

- You ship `#cloud-config` files unmodified.
- You depend on `datasource: { providers: [azure] }` (or gcp / openstack / digitalocean / etc).
- You depend on date-arithmetic / semver / regex sprig functions.
- You ship in a context where shelling out to `partprobe` / `resize2fs` is not possible (initramfs with no FS tools — though yip-rs's plugin will degrade with a warn rather than crash).

For everything else: the porting work is in flight, the schema is stable, and `yip analyze` will tell you what would run before anything touches the disk.
