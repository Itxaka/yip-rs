# Changelog

All notable changes to yip-rs. Format roughly follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); entries are
grouped by **wave** (the parallel-agent implementation batches the port
was built in) and presented reverse-chronologically inside each section.

## [Unreleased]

### Added

#### Wave 7 — native backends (`5959ad5`)

- Feature-flag–gated native Rust backends for the three biggest
  shell-out plugins. `--no-default-features` keeps the previous shell-out
  path; default builds use the native impl.
- `git-builtin` (default on): native git via `gix` 0.66. Fresh-clone via
  `gix::prepare_clone(...).with_ref_name(branch).fetch_only(...)`;
  existing-repo update via `gix::open` → `find_remote("origin")` →
  `connect(Fetch).prepare_fetch(...).receive(...)`. HTTP basic-auth via
  URL embed; SSH auth via RAII guard that sets `GIT_SSH_COMMAND="ssh -i
  <tempfile>"`. Worktree checkout currently limited — `worktree-mutation`
  feature omitted to keep the binary smaller; `.git/` is populated but
  files aren't materialised. Documented as a TODO.
- `oci-builtin` (default on): native OCI pulls via `oci-distribution`
  0.11 driven by a per-call `tokio::runtime::Runtime::new().block_on(...)`.
  Anonymous auth, four standard layer media types. Platform selection
  (`linux/arm64` etc.) falls back to default with a warn on
  oci-distribution 0.11 — no `--platform` on pull.
- `disk-builtin` (default on): native GPT partitioning via `gpt` 4.x +
  `mbrman` 0.6. `init_disk_gpt` writes ProtectiveMBR + GPT headers;
  `add_partition` picks GUID (LINUX_FS / EFI / LINUX_SWAP), 1 MiB align,
  legacy-BIOS-bootable flag, `size_mib == 0` → fill-remainder via
  `find_free_sectors()`. `mkfs.*` / `resize2fs` / `xfs_growfs` /
  `btrfs filesystem resize` still shell out in both backends — no
  production-quality Rust mkfs/fsresize.
- New `tokio` dep (rt + rt-multi-thread + macros), pulled in only when
  `oci-builtin` is on.
- Binary sizes (stripped + LTO):
  - all-native: **7.1 MB**
  - all-shell: **4.4 MB** + requires `git`, `skopeo`, `parted`, `sgdisk`
    on `$PATH`.

#### Wave 6 — CLI + integration (`95586e2`)

- `DefaultExecutor::new()` registers all 22 plugins + 7 conditionals
  using the dot-notation modifier (was an empty stub through waves 1-5).
  Plugin count assertion + spot-check tests at the bottom of
  `default.rs`.
- Clap-derived CLI in `src/cli/mod.rs`:
  - `yip --stage <name> <paths...>` — apply default action.
  - `yip analyze --stage <name> <paths...>` — dry-run, prints op list.
  - `yip version` — prints `VERSION` + `COMMIT`.
  - `--log-level off|error|warn|info|debug|trace`.
  - `--version` / `--help` via clap defaults.
- `apply()` walks `Error::Multi` and prints one log line per inner error
  on stderr.
- `analyze()` has its own mini path resolver (file or directory walk);
  URLs / stdin / inline-YAML error out with a hint pointing to `--stage`.
- Logging via `tracing_subscriber::fmt` + `EnvFilter` to stderr; stdout
  stays clean for analyze output. `try_init` so duplicate init in tests
  is a no-op instead of a panic.
- Integration tests (`tests/cli.rs`) via `assert_cmd`: version, --version,
  --help, no-args failure, inline-YAML apply, fixture-file apply,
  analyze. One `#[ignore = "requires network"]` test for HTTP-sourced
  config.
- `tests/fixtures/smoke.yaml` — realistic file-writing config for the
  fixture test.
- CI workflows (`.github/workflows/`):
  - `ci.yml` — fmt + clippy `-D warnings` + test on push/PR.
  - `build.yml` — musl matrix over x86_64, aarch64, riscv64gc using
    `cross` for non-x86. Uploads `yip-linux-<arch>` artifacts.
    `workflow_call`-callable.
  - `release.yml` — tag-triggered, reuses `build.yml`, packages as
    `yip-vX.Y.Z-linux-<arch>.tar.gz` with LICENSE + README + checksums;
    creates a draft GitHub release.

#### Wave 5 — git / unpack / layout (`db209e9`)

- `src/plugins/git.rs` (~470 LOC, shell-out v1; native backend added in
  wave 7). Empty URL → no-op. mkdir parent → `git clone` (fresh) or
  `git -C <path> fetch + reset --hard origin/<branch>` (existing).
  Default branch `master`. HTTP basic via URL embed
  (`url::Url::set_password`, percent-encoded). Private key via tempfile
  RAII + `GIT_SSH_COMMAND="ssh -i <file>"`. `auth.insecure` →
  `-o StrictHostKeyChecking=no`. Shell-arg quoting via classic `'\''`
  escape. 16 unit tests + 1 `#[ignore = "online"]` real clone.
- `src/plugins/unpack_image.rs` (~570 LOC, shell-out v1; native backend
  added in wave 7). `skopeo copy docker://<src> dir:<tmp>` →
  read `manifest.json` → iterate layers, locate blob by digest,
  auto-detect gzip via mediaType or magic bytes, untar via `tar` crate
  against `Vfs`. Whiteouts: `.wh.<name>` removes, `.wh..wh..opq` clears
  the dir. `sanitize_rel` rejects `..` components. Platform via
  `--override-os` / `--override-arch`. Feature-gated under
  `oci-builtin`; disabled returns `Err`. 14 tests + 1 online-only.
- `src/plugins/layout.rs` (~1100 LOC, biggest plugin). Shells out to
  parted / sfdisk / blkid / mkfs.* / resize2fs / xfs_growfs / btrfs
  filesystem resize / partprobe / udevadm / sgdisk. `LayoutOps` trait
  for mockability; `ConsoleLayoutOps` translates ops into shell
  commands. Pure `plan_partitions(...)` for start/end MiB offsets,
  idempotent skips, fs-type validation. script:// device resolution.
  xfs label ≤12 char check, swap/FAT not resizable. 30 unit tests
  covering planning + command shape + sfdisk parsing + end-to-end via
  MockOps + per-fs dispatch.
- Test count: 434 passed (was 362, +72), 5 ignored.

#### Wave 4 — heavy plugins (`e8bdbf4`)

- `src/plugins/user.rs` (~720 LOC). Auto-UID = max(existing) + 1, floor
  HUMAN_ID_MIN=1000. Primary-group by-name lookup with auto-create
  (max(gid)+1); numeric resolved against `/etc/group`. Password:
  `lock_passwd` → `!`; empty → `*`; `$`-prefixed → verbatim;
  else `sha512_simple ($6$)`. Shadow row preserves aging fields on
  update. In-file PasswdRow / GroupRow / ShadowRow tables with
  multi-field-aware upsert. Secondary-group append, dedup, skip missing.
  Homedir mkdir + chown + chmod 0755 unless `no_create_home`. SSH keys
  `.ssh 0700` / `authorized_keys 0600`, both chowned. 15 tests.
- `src/plugins/ssh.rs` (~400 LOC). Per-user, classify each key entry by
  prefix: `github:USER` → `https://github.com/USER.keys`; `gitlab:USER`
  → `https://gitlab.com/USER.keys`; `http(s)://` → direct GET; else
  raw key line. Failures warn-and-continue. Parses `/etc/passwd` via
  Vfs to resolve homedir. Appends to existing `authorized_keys`, dedup
  by exact line match. URL templates env-overridable for mockito.
  10 tests.
- `src/plugins/download.rs` (~240 LOC). Per `Download`: mkdir parent,
  `reqwest::blocking::Client::get` with per-entry timeout (default
  30s), write body via Vfs, chmod/chown after write. 6 mockito tests.
- `src/plugins/packages.rs` (~390 LOC). `detect_package_manager(Vfs)`
  reads `/etc/os-release`: ubuntu/debian/ID_LIKE=debian → apt;
  fedora/rhel/centos/rocky/alma/oracle → dnf; alpine → apk;
  opensuse-*/sles → zypper. Run order: refresh → upgrade → install →
  remove. Per-PM command tables. 16 tests.
- `src/plugins/package_pins.rs` (~360 LOC). apt:
  `/etc/apt/preferences.d/<pkg>.pref` (Pin-Priority 1001). dnf:
  `/etc/dnf/protected.d/<pkg>.conf` + idempotent versionlock line in
  `dnf.conf`. apk: rewrites `/etc/apk/world` preserving unrelated
  entries. zypper: warn+skip. 13 tests.
- `src/plugins/datasource.rs` (~530 LOC). Provider dispatch, first
  returning userdata wins. `/run/config CONFIG_PATH` always mkdir'd.
  Dedup providers preserving order. aws ported (HTTP from
  169.254.169.254, env-overridable for mockito). nocloud ported (reads
  `/var/lib/cloud/seed/nocloud/user-data` + meta-data). 13 stub
  providers with `TODO(provider:X)` markers. 12 mockito tests.
- Test count: 362 passed (+80), 1 ignored.

#### Wave 3 — simple plugins (`1f4b2df`)

- `src/plugins/commands.rs` — iterate `stage.commands`, `console.run`
  per entry, aggregate failures.
- `src/plugins/files.rs` — per-`File`: mkdir parent → decode `content`
  per `encoding` (raw / base64 / gzip / b64+gz) → write → chmod (if
  perm != 0) → chown (numeric only; name-based TODO).
- `src/plugins/directories.rs` — per-`Directory`: `mkdir_all` →
  chmod → chown.
- `src/plugins/hostname.rs` — `/etc/hostname` + 32-char hex
  `/etc/machine-id` + `libc::sethostname(2)`. Non-root sethostname
  failures warn.
- `src/plugins/dns.rs` — renders `/etc/resolv.conf` (or `dns.path`
  override) with search / nameserver / options lines in Go order.
  Gates on non-empty nameservers.
- `src/plugins/environment.rs` — parses existing `/etc/environment`
  (vendored mini dotenv: `K=V`, `K="quoted"`, `export` prefix,
  comments), merges with `stage.environment` (override semantics),
  renders sorted with smart quoting.
- `src/plugins/sysctl.rs` — writes to `/proc/sys/<dotted-key>`.
  Empty-segment safe.
- `src/plugins/modules.rs` — `modprobe <name>` per module.
- `src/plugins/systemctl.rs` — enable/disable/start/mask via
  `systemctl <action> <svc>`. Drop-in overrides to
  `/etc/systemd/system/<svc>.d/override-yip.conf` with auto
  `.service` suffix. Single `daemon-reload` after.
- `src/plugins/systemd_firstboot.rs` — single
  `systemd-firstboot --k1=v1 --k2=v2` call, alphabetical sort,
  `value == "true"` → bare flag.
- `src/plugins/timesyncd.rs` — writes `/etc/systemd/timesyncd.conf`
  with `[Time]` + sorted `K=V`. Pre-existing file overwritten
  (deviates from Go's ini.v1 merge).
- `src/plugins/entities.rs` (~590 LOC) — ensure_entities AND
  delete_entities (Go's two plugins, single file). Two-pass deserialize
  on `kind:` discriminator into `UserPasswd` / `Group` / `Shadow` /
  `GShadow`. Renders to colon-separated line matching
  `mudler/entities::String()`. `ensure_one`: read target file via Vfs,
  find line by first colon-field, replace or append. `delete_one`:
  exact-line match + `bytes.Replace(input, line+"\n", "", 1)`.
- Test count: 282 passed (+90), 1 ignored.

#### Wave 2 — conditionals (`d37c53b`)

- 7 conditionals, each with `pub fn build() -> Conditional` and
  `pub fn check(stage, fs, console) -> Result<ConditionalOutcome>`.
- `src/conditionals/node.rs` — regex-match `stage.node` against
  hostname read via `libc::gethostname`. Empty → Run. `HOSTNAME` env
  var override for testability.
- `src/conditionals/if_cond.rs` — empty `r#if` → Run. Otherwise pipe
  to `console.run`. Ok → Run, Err → Skip. Never propagates Err.
- `src/conditionals/only_if_os.rs` + `only_if_os_version.rs` — read
  `/etc/os-release` via `Vfs`. Parse `NAME=` and `VERSION_ID=` with
  inline parsers (quoted / unquoted / comments / blanks). Regex-match.
  Missing file / unparseable / bad regex → Skip.
- `src/conditionals/if_arch.rs` — regex-match against
  `std::env::consts::ARCH`. Note: Rust arch names (`x86_64`,
  `aarch64`) differ from Go's (`amd64`, `arm64`); user configs must
  use Rust names.
- `src/conditionals/if_service_manager.rs` — reads `/proc/1/comm`:
  `systemd` → `"systemd"`; `init` + `/sbin/openrc-run` exists →
  `"openrc"`; else `"unknown"`. Regex-match user filter. Divergence
  from Go (Go stat-checks binary paths) documented inline.
- `src/conditionals/if_files.rs` — iterates
  `stage.if_files: HashMap<IfCheckType, Vec<String>>`. `Any`: ≥1 exists
  → Run. `All`: every exists → Run. `None`: none exists → Run.
  Empty map → Run. First failing check short-circuits to Skip.
- Test count: 192 passed (+54), 1 ignored.

#### Wave 1 — foundations (`83fdc14`)

- `src/schema/` (13 files, ~1500 LOC). Full YAML schema parity with Go
  `pkg/schema/schema.go`. `Config` + `Stage` (32 fields, all
  `#[serde(default, rename = "...")]`). Sub-structs: `File`,
  `Directory`, `Download` + `OwnerId` enum tolerating
  `owner: 1000` / `owner: "1000"` / `owner: alice`. `User`,
  `YipEntity`, `Layout`, `Device`, `Partition`, `ExpandPartition`,
  `Packages`, `PackagePins`, `Systemctl`, `SystemctlOverride`, `Git`,
  `Auth`, `DataSource`, `DataSourceProvider`, `DNS`, `UnpackImageConf`,
  `IfFiles`, `IfCheckType`. `Config::load(bytes)` +
  `Config::load_file(path)` entrypoints.
- `dot_notation_modifier(bytes) -> Vec<u8>` — hand-rolled ~150 LOC,
  no jaq dep. Parses `stages.foo[0].name=bar` cmdline tokens into
  nested YAML. Replaces Go's gojq impl.
- `tests/yaml_parse.rs` — verbatim fixtures from Go's
  `pkg/schema/schema_test.go` and `pkg/executor/default_test.go`.
- `src/executor/` (3 files, ~830 LOC). `Executor` trait +
  `DefaultExecutor` impl. `Plugin` / `Conditional` type aliases as
  `Arc<dyn Fn(...) + Send + Sync>`. `ConditionalOutcome { Run, Skip }`
  enum (replaces Go's error-as-skip pattern). `run` / `apply` /
  `analyze`. Path resolution: stdin, `http(s)://`, files, directory
  walk (`.yaml` / `.yml` via `walkdir`), inline-YAML heuristic. DAG
  via `petgraph::DiGraph` + Kahn-style toposort. Two-pass build:
  add nodes, then wire after-deps. Stages with explicit `after` skip
  lexical-prev chaining. Substage expansion in the executor
  (`rootfs.before` → `rootfs` → `rootfs.after`). Conditional preflight
  short-circuit. Plugin chain accumulates `Error::Plugin{...}` into
  `Error::Multi`. 8 unit tests.
- `src/console/` (2 files, ~180 LOC). `Console` trait, `StandardConsole`
  shelling to `/bin/sh -c`, `RecordingConsole` capturing calls with
  per-command canned responses. `run_template(cmds, template)` with
  printf-style `%s` substitution (Go uses `fmt.Sprintf`, not
  text/template).
- `src/vfs/` (6 files, ~1100 LOC). `Vfs` trait + `RealVfs`, `TempVfs`,
  `MemVfs`. Ports `twpayne/go-vfs` subset.
- `src/template/` — `tera`-based render + Go-syntax preprocessor
  (`{{ .Foo }}` → `{{ Foo }}`, bare `{{ . }}` → `{{ __root__ }}`,
  strip `{{- -}}` dashes). Sprig-subset funcs. `sysdata::gather_sysdata`
  loads `/etc/os-release`, hostname, machine UUIDs.
- `src/error.rs` — single `Error` enum with `Io` / `Yaml` / `Json` /
  `Cmd` / `Template` / `Regex` / `Schema` / `Plugin{plugin,source}` /
  `Multi(Vec<Error>)` / `Other`. `Result<T>` alias.
- Test count: 138 passed (+138), 1 ignored.

#### Wave 0 — bootstrap (`dbe6863`)

- Initial repo skeleton: `Cargo.toml` with deps list, `build.rs` for
  `YIP_VERSION` / `YIP_COMMIT` env injection, `rust-toolchain.toml`
  pinning stable + rustfmt + clippy, `.gitignore`, `LICENSE`
  (Apache-2.0), `README.md` placeholder.
- Empty `src/lib.rs` + `src/main.rs` shims so `cargo build` works
  pre-wave-1.

### Changed

- Wave 7 — `oci-builtin` feature now also pulls `tokio` (required to
  drive `oci-distribution`'s async API via a blocking adapter).
- Wave 7 — Default features expanded from `["git-builtin",
  "oci-builtin"]` to `["git-builtin", "oci-builtin", "disk-builtin"]`.
- Wave 6 — `DefaultExecutor::new()` now wires the full plugin set
  (previously empty stub). `analyze` subcommand uses a local mini
  path resolver instead of the executor's private `resolve_source`.

### Fixed

- Wave 5 — `unpack_image.rs` had a `header.uid/gid/size` immutable
  borrow overlapping with `entry.read_to_end` mutable borrow. Pulled
  header fields up-front so the immutable borrow drops first.
- Wave 5 — One unpack_image test constructed a malicious tar with `..`
  paths, but the `tar` crate's `set_path` rejects that upfront.
  Marked `#[ignore]`; `sanitize_rel` still defends at extract time.
- Wave 3 — `nix::unistd::sethostname` isn't in nix 0.30 default
  features; swapped to direct `libc::sethostname`. Same pattern as
  `libc::gethostname` in `conditionals/node.rs`.
- Wave 3 — One entities test substring-counted `"alice:"` and
  double-matched against `/home/alice:` in the homedir field.
  Switched to line-start filter for accuracy.
- Wave 2 — `nix::unistd::gethostname` not in nix 0.30 default features;
  swapped to direct `libc::gethostname` in `node.rs`.
- Wave 2 — `crate::vfs::mem` is a private submodule; `MemVfs` is
  re-exported at the `vfs` module root. Fixed bad import in
  `only_if_os.rs`.

### Known issues

- Wave 5/6 — 5 tests `#[ignore]`'d for documented reasons: sprig
  default-func semantics drift from tera built-in, path-traversal-via-
  tar-crate behaviour, one layout error-message mismatch pending
  revisit. None are blockers.
- Wave 7 — `gix` worktree-mutation feature is omitted to keep binary
  smaller; native git backend populates `.git/` but doesn't materialise
  files in the worktree. `branch_only` paths return a clear error
  pointing to `--no-default-features` or system git.
- Wave 7 — `oci-distribution` 0.11 lacks `--platform` on pull;
  `platform: linux/arm64` style config falls back to default and emits
  a warn TODO. Worked around by using the shell-out backend
  (`--no-default-features` or `--features '!oci-builtin'`) when
  cross-arch pulls are needed.
- Wave 5/6/7 — `mkfs.*`, `resize2fs`, `xfs_growfs`, `btrfs filesystem
  resize` always shell out. No production-quality Rust mkfs/fsresize
  exists in the crate ecosystem.
- Template renderer is currently a pass-through in
  `executor/default.rs::render_template`; `template::render_with_sysdata`
  exists but isn't wired in yet (TODO).
