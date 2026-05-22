# yip-rs

Rust port of [yip](https://github.com/mudler/yip) — the cloud-init-style YAML stage executor used by Kairos and friends.

Status: **early bootstrap**. Not production-ready. The Go binary at `mudler/yip` is still the canonical implementation.

## What it does

Applies declarative YAML configuration to a system: writes files, runs commands, creates users, installs packages, configures systemd units, mounts filesystems, sets sysctls, downloads artifacts, clones git repos, unpacks OCI images, etc. Same input format as Go yip — the Rust port aims for byte-for-byte schema parity so existing config consumes without modification.

## Library + binary

```rust
use yip::executor::Executor;

let exec = Executor::default();
exec.run("rootfs", &["/etc/yip/conf.d"])?;
```

```bash
yip --stage rootfs /etc/yip/conf.d
```

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
├── plugins/          # 23 action plugins (files, commands, users, …)
├── template/         # sprig-subset template engine
└── error.rs          # error types
```

## Feature flags

| Feature | Default | Effect |
|---|---|---|
| `git-builtin` | on | Embed `gix` for git operations. Disable to shrink binary and shell out to `git`. |
| `oci-builtin` | on | Embed `oci-distribution` for OCI registry pulls. Disable to shell out to `skopeo`. |
| `nogit` | off | Drop the `git` plugin entirely. |
| `nounpack` | off | Drop the `unpack_images` plugin entirely. |

## License

Apache-2.0 (same as upstream yip). See `LICENSE`.
