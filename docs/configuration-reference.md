# Configuration reference

Full YAML schema for yip-rs. Every key, every type, every default. Mirrors Go yip 1:1 unless flagged otherwise.

For CLI flags see [`USAGE.md`](../USAGE.md). For diffs from Go yip see [`migrating-from-go-yip.md`](migrating-from-go-yip.md).

## Top-level shape

A config file is one YAML document with at most two top-level keys:

```yaml
name: my-config              # optional, free-form label
stages:                      # map of stage-name -> list-of-step
  <stage-name>:
    - <step>
    - <step>
  <other-stage>:
    - <step>
```

| Key | Type | Required | Default | Notes |
|---|---|---|---|---|
| `name` | string | no | `""` | Used in op-name prefix (`<name>.<stage-name>`). Falls back to source path/url if empty. |
| `stages` | map<string, list<Stage>> | no | `{}` | Empty map = no-op config. |

Stage keys are arbitrary strings. Convention is `rootfs`, `rootfs.before`, `rootfs.after`, `initramfs`, `boot`, `network`, `reconcile` — see [`USAGE.md`](../USAGE.md). The `--stage <X>` flag auto-runs `X.before`, `X`, `X.after`.

Cloud-init `#cloud-config` parsing is **not yet implemented** in yip-rs. Pass yip-native YAML only for now.

## Stage struct

Every entry in a `stages: <name>: [...]` list is a `Stage`. All 32 fields are optional; missing fields parse to their default.

Quick map of stage fields, grouped:

| Category | Fields |
|---|---|
| Identity / wiring | `name`, `after` |
| Conditionals | `if`, `only_os`, `only_os_version`, `only_arch`, `only_service_manager`, `if_files`, `node` |
| File-system actions | `files`, `downloads`, `directories` |
| Commands | `commands`, `modules` |
| Identity / accounts | `users`, `ensure_entities`, `delete_entities`, `authorized_keys` |
| Networking | `dns`, `hostname` |
| System knobs | `sysctl`, `environment`, `environment_file`, `timesyncd`, `systemd_firstboot` |
| Systemd | `systemctl` |
| Packages | `packages`, `package_pins` |
| Disks | `layout` |
| Images | `unpack_images` |
| Git | `git` |
| Cloud-init | `datasource` |

Detail per field below.

---

### `name`

| | |
|---|---|
| YAML key | `name` |
| Type | string |
| Required | no |
| Default | `""` |
| Go parity | identical |

Free-form stage label. Used in op-name prefix and `analyze` output. Duplicate names within one stage list trigger automatic suffixing (`<name>.0`, `<name>.1`).

```yaml
stages:
  rootfs:
    - name: bootstrap
      commands: [echo hi]
```

---

### `after`

| | |
|---|---|
| YAML key | `after` |
| Type | list of `{name: string}` |
| Required | no |
| Default | `[]` |
| Go parity | identical |

Declares dependencies on other stages in the same stage list. Stages with `after` are topologically ordered (after-deps satisfied before they run). Stages without `after` form an implicit lexical chain.

```yaml
stages:
  rootfs:
    - name: b
      after:
        - name: a
      commands: [echo b]
    - name: a
      commands: [echo a]
    # `a` runs before `b` even though `b` appears first in the YAML.
```

Multiple matches: if more than one stage shares `name: a`, `b` waits for all of them. Cycles → executor error.

---

### `if`

| | |
|---|---|
| YAML key | `if` |
| Type | string (shell expression) |
| Required | no |
| Default | `""` |
| Go parity | identical |

Shell condition. Non-empty → executed via the configured console; non-zero exit → stage skipped. Empty → unconditional.

```yaml
stages:
  rootfs:
    - if: "[ ! -e /tmp/already-done ]"
      commands: [touch /tmp/already-done]
```

---

### `only_os`

| | |
|---|---|
| YAML key | `only_os` |
| Type | string |
| Required | no |
| Default | `""` |
| Go parity | identical |

Matched against `ID=` from `/etc/os-release`. Substring match, case-sensitive. Empty → no constraint.

```yaml
stages:
  rootfs:
    - only_os: ubuntu
      commands: [apt-get update]
```

---

### `only_os_version`

| | |
|---|---|
| YAML key | `only_os_version` |
| Type | string |
| Required | no |
| Default | `""` |
| Go parity | identical |

Matched against `VERSION_ID=` from `/etc/os-release`. Semver-ish comparison: leading operator (`>=`, `<=`, `>`, `<`, `=`) optional, default is equality.

```yaml
stages:
  rootfs:
    - only_os: ubuntu
      only_os_version: ">=22.04"
      commands: [snap version]
```

---

### `only_arch`

| | |
|---|---|
| YAML key | `only_arch` |
| Type | string |
| Required | no |
| Default | `""` |
| Go parity | **different naming** — see below |

Matched against the Rust target arch (`std::env::consts::ARCH`). **Important**: Rust uses `x86_64` where Go uses `amd64`. Use `x86_64` for Intel/AMD 64-bit, `aarch64` for ARM 64-bit, `arm` for 32-bit ARM.

```yaml
stages:
  rootfs:
    - only_arch: x86_64
      commands: [echo "running on x86_64"]
    - only_arch: aarch64
      commands: [echo "running on arm64"]
```

Multiple arches: comma-separated.

```yaml
- only_arch: "x86_64,aarch64"
```

---

### `only_service_manager`

| | |
|---|---|
| YAML key | `only_service_manager` |
| Type | string |
| Required | no |
| Default | `""` |
| Go parity | identical |

Matched against the detected init/service manager: `systemd`, `openrc`, or `runit`. Detection is by presence of canonical files (`/run/systemd/system`, `/run/openrc/softlevel`, etc).

```yaml
- only_service_manager: systemd
  systemctl:
    enable: [foo.service]
```

---

### `if_files`

| | |
|---|---|
| YAML key | `if_files` |
| Type | map<`any`\|`all`\|`none`, list<string>> |
| Required | no |
| Default | `{}` |
| Go parity | identical |

File-existence gate. Three modes:

- `any`: at least one of the listed paths must exist.
- `all`: every listed path must exist.
- `none`: no listed path may exist.

Multiple modes can be combined; all combined checks must pass.

```yaml
- if_files:
    any:
      - /etc/foo
      - /etc/bar
    none:
      - /etc/disable-me
  commands: [echo "passed"]
```

---

### `node`

| | |
|---|---|
| YAML key | `node` |
| Type | string |
| Required | no |
| Default | `""` |
| Go parity | identical |

Hostname gate. Stage runs only when the system's hostname matches. Empty → no constraint.

```yaml
- node: my-build-host
  commands: [echo "only on my build host"]
```

---

### `files`

| | |
|---|---|
| YAML key | `files` |
| Type | list<File> |
| Required | no |
| Default | `[]` |
| Go parity | identical |

Materialise files on disk. See [File](#file) below.

```yaml
files:
  - path: /etc/foo.conf
    content: |
      key=value
    permissions: 0644
    owner: 0
    group: 0
```

---

### `downloads`

| | |
|---|---|
| YAML key | `downloads` |
| Type | list<Download> |
| Required | no |
| Default | `[]` |
| Go parity | identical |

HTTP-fetch files. See [Download](#download) below.

```yaml
downloads:
  - url: https://example.com/blob.bin
    path: /var/lib/blob.bin
    permissions: 0644
    timeout: 60
```

---

### `directories`

| | |
|---|---|
| YAML key | `directories` |
| Type | list<Directory> |
| Required | no |
| Default | `[]` |
| Go parity | identical (intermediate-dir chown not replicated — see plugin notes) |

Ensure directories exist with given perms/ownership. See [Directory](#directory) below.

```yaml
directories:
  - path: /var/lib/foo
    permissions: 0755
    owner: 1000
```

---

### `commands`

| | |
|---|---|
| YAML key | `commands` |
| Type | list<string> |
| Required | no |
| Default | `[]` |
| Go parity | identical (templating deferred to higher layer) |

Shell-out per entry. Errors do not abort the loop — every command runs, errors accumulate into a multi-error.

```yaml
commands:
  - id
  - touch /tmp/x
  - "[ -e /tmp/x ] && echo ok"
```

Each entry is passed to the system shell verbatim.

---

### `modules`

| | |
|---|---|
| YAML key | `modules` |
| Type | list<string> |
| Required | no |
| Default | `[]` |
| Go parity | identical (Go uses syscalls; yip-rs shells out to `modprobe(8)`) |

Kernel modules to load. Already-loaded modules are a no-op (`modprobe`'s behaviour).

```yaml
modules:
  - nvidia
  - kvm_intel
```

---

### `users`

| | |
|---|---|
| YAML key | `users` |
| Type | map<string, User> |
| Required | no |
| Default | `{}` |
| Go parity | identical (native passwd/shadow/group writer; not entities-library based) |

Create/update Unix users. Key is the username (overrides `User.name` if both set). See [User](#user) below.

```yaml
users:
  alice:
    passwd: "$6$..."
    shell: /bin/bash
    uid: "1001"
    groups: [sudo, docker]
    ssh_authorized_keys:
      - ssh-ed25519 AAAA...
```

---

### `ensure_entities`

| | |
|---|---|
| YAML key | `ensure_entities` |
| Type | list<YipEntity> |
| Required | no |
| Default | `[]` |
| Go parity | identical |

Raw passwd-style line manipulation. Lower level than `users` — use this when you need group/shadow/gshadow direct control. See [YipEntity](#yipentity) below.

```yaml
ensure_entities:
  - path: /etc/passwd
    entity: |
      kind: "user"
      username: "foo"
      uid: 1000
      gid: 1000
      homedir: /home/foo
      shell: /bin/bash
```

---

### `delete_entities`

Same shape as `ensure_entities`, but deletes lines instead. See [YipEntity](#yipentity).

```yaml
delete_entities:
  - path: /etc/passwd
    entity: |
      kind: user
      username: oldbob
```

---

### `authorized_keys`

| | |
|---|---|
| YAML key | `authorized_keys` |
| Type | map<string, list<string>> |
| Required | no |
| Default | `{}` |
| Go parity | identical |

Per-user SSH `authorized_keys` material. Map key is the username. Values are key specs; each spec is one of:

- `github:USERNAME` → fetch `https://github.com/USERNAME.keys`
- `gitlab:USERNAME` → fetch `https://gitlab.com/USERNAME.keys`
- `http(s)://...` → GET that URL
- anything else → treated as a raw `ssh-...` line verbatim

Resolved keys land in `<home>/.ssh/authorized_keys`. Existing keys are preserved; new keys are appended dedup-on-exact-match. `.ssh` dir is created with mode `0700`, file with `0600`, owned by the user.

```yaml
authorized_keys:
  alice:
    - github:alice
    - https://example.com/alice-laptop.pub
    - "ssh-ed25519 AAAA... alice-yubikey"
```

---

### `dns`

| | |
|---|---|
| YAML key | `dns` |
| Type | DNS |
| Required | no |
| Default | empty DNS struct |
| Go parity | identical (writes only when `nameservers` non-empty) |

Renders an `/etc/resolv.conf`-style file. See [DNS](#dns-struct) below.

```yaml
dns:
  path: /etc/resolv.conf
  nameservers:
    - 8.8.8.8
    - 1.1.1.1
  search:
    - example.com
  options:
    - timeout:2
```

---

### `hostname`

| | |
|---|---|
| YAML key | `hostname` |
| Type | string |
| Required | no |
| Default | `""` |
| Go parity | identical (machine-id source differs — see plugin notes) |

Sets hostname. Writes `/etc/hostname` + `/etc/machine-id` + calls `sethostname(2)`. Empty → no-op.

```yaml
hostname: my-box
```

---

### `sysctl`

| | |
|---|---|
| YAML key | `sysctl` |
| Type | map<string, string> |
| Required | no |
| Default | `{}` |
| Go parity | identical (writes only `/proc/sys`, not `/etc/sysctl.d`) |

Runtime sysctl values. Keys use dot notation (translated to `/proc/sys/<dotted/with/slashes>`).

```yaml
sysctl:
  net.ipv4.ip_forward: "1"
  vm.swappiness: "10"
```

Values must be strings (YAML quoting matters for numbers).

---

### `environment`

| | |
|---|---|
| YAML key | `environment` |
| Type | map<string, string> |
| Required | no |
| Default | `{}` |
| Go parity | identical |

Merge into `/etc/environment` (or `environment_file`). Existing file contents are parsed, new keys merged in (override on conflict), result re-emitted sorted by key. Quoting follows godotenv: bare value unless it contains whitespace or shell-special chars.

```yaml
environment:
  HTTP_PROXY: http://proxy:3128
  HTTPS_PROXY: http://proxy:3128
  NO_PROXY: "localhost,127.0.0.1"
```

---

### `environment_file`

| | |
|---|---|
| YAML key | `environment_file` |
| Type | string |
| Required | no |
| Default | `/etc/environment` |
| Go parity | identical |

Override the file the `environment` plugin merges into. Useful for shell profile fragments.

```yaml
environment_file: /etc/profile.d/proxy.sh
environment:
  HTTP_PROXY: http://proxy:3128
```

---

### `timesyncd`

| | |
|---|---|
| YAML key | `timesyncd` |
| Type | map<string, string> |
| Required | no |
| Default | `{}` |
| Go parity | **slightly different** — yip-rs overwrites the file deterministically; Go merges. See plugin notes. |

Writes `/etc/systemd/timesyncd.conf` as `[Time]` + alphabetically-sorted `KEY=VALUE` lines.

```yaml
timesyncd:
  NTP: "0.pool.ntp.org 1.pool.ntp.org"
  FallbackNTP: "ntp.example.com"
```

---

### `systemd_firstboot`

| | |
|---|---|
| YAML key | `systemd_firstboot` |
| Type | map<string, string> |
| Required | no |
| Default | `{}` |
| Go parity | identical |

Drives `systemd-firstboot(1)`. Keys are lowercased into `--key=value` flags; `value == "true"` becomes a bare `--key`. Flags are sorted alphabetically for stable output.

```yaml
systemd_firstboot:
  keymap: us
  locale: en_US.UTF-8
  timezone: Europe/Berlin
```

---

### `systemctl`

| | |
|---|---|
| YAML key | `systemctl` |
| Type | Systemctl |
| Required | no |
| Default | empty struct |
| Go parity | identical + auto `daemon-reload` after overrides |

Enable / disable / start / mask units + optional drop-in override files. See [Systemctl](#systemctl-struct) below.

```yaml
systemctl:
  enable:
    - sshd.service
    - cron.service
  disable:
    - bluetooth.service
  start:
    - sshd.service
  mask:
    - apport.service
  overrides:
    - service: sshd.service
      content: |
        [Service]
        Restart=always
```

---

### `packages`

| | |
|---|---|
| YAML key | `packages` |
| Type | Packages |
| Required | no |
| Default | empty struct |
| Go parity | partial — apt / dnf / apk / zypper only |

Package install/remove/refresh/upgrade. See [Packages](#packages-struct) below.

```yaml
packages:
  refresh: true
  upgrade: false
  install:
    - vim
    - curl
  remove:
    - nano
```

Distro is detected from `/etc/os-release`. Unsupported distro → plugin returns `Error::Other`.

---

### `package_pins`

| | |
|---|---|
| YAML key | `package_pins` |
| Type | map<string, string> |
| Required | no |
| Default | `{}` |
| Go parity | partial — apt / dnf / apk only |

Best-effort version pinning. Backend depends on detected distro:

- apt: `/etc/apt/preferences.d/<pkg>.pref` with `Pin-Priority: 1001`.
- dnf: `/etc/dnf/protected.d/<pkg>.conf` + `versionlock` line in `dnf.conf`.
- apk: rewrites `/etc/apk/world` with `pkg=ver` entries.
- zypper / unknown: warn + skip.

```yaml
package_pins:
  curl: "7.81.0-1ubuntu1.15"
  vim: "2:8.2.3458-2ubuntu2"
```

---

### `layout`

| | |
|---|---|
| YAML key | `layout` |
| Type | Layout |
| Required | no |
| Default | empty struct |
| Go parity | partial — gpt/mbr partitioning native; mkfs and fs-grow shell out |

Disk partitioning and last-partition expansion. See [Layout](#layout-struct) below.

```yaml
layout:
  device:
    path: /dev/sda
    label: gpt
  expand_partition:
    size: 0          # 0 = fill remainder
  add_partitions:
    - fsLabel: COS_PERSISTENT
      pLabel: persistent
      size: 4096
      filesystem: ext4
```

---

### `unpack_images`

| | |
|---|---|
| YAML key | `unpack_images` |
| Type | list<UnpackImageConf> |
| Required | no |
| Default | `[]` |
| Go parity | identical (native via `oci-distribution` or `skopeo` fallback) |

Pull OCI images and extract their rootfs. See [UnpackImageConf](#unpackimageconf) below.

```yaml
unpack_images:
  - source: quay.io/kairos/core:latest
    target: /var/lib/kairos/core
    platform: linux/amd64
```

---

### `git`

| | |
|---|---|
| YAML key | `git` |
| Type | Git |
| Required | no |
| Default | empty struct |
| Go parity | identical (native via `gix` or `git` binary fallback) |

Clone or update a git repo. See [Git](#git-struct) below.

```yaml
git:
  url: https://github.com/foo/bar.git
  path: /opt/bar
  branch: main
  branch_only: true
  auth:
    username: alice
    password: hunter2
```

---

### `datasource`

| | |
|---|---|
| YAML key | `datasource` |
| Type | DataSource |
| Required | no |
| Default | empty struct |
| Go parity | partial — aws + nocloud only |

Cloud-init datasource fetch. See [DataSource](#datasource-struct) below.

```yaml
datasource:
  providers:
    - aws
    - nocloud
  path: /run/config
  userdata_name: user-data
```

---

## Nested structs

### File

| Key | Type | Required | Default | Notes |
|---|---|---|---|---|
| `path` | string | yes | `""` | Absolute path of the file to write. |
| `content` | string | no | `""` | File body. See `encoding`. |
| `encoding` | string | no | `""` (raw) | One of `""`, `string`, `b64`, `base64`, `gzip`, `gz+b64`, `b64+gz`. |
| `permissions` | int (octal) | no | `0` | If `0`, no chmod. Use YAML octal: `0644`. |
| `owner` | int \| string | no | `0` | Numeric uid OR username. Accepts both; cf. `OwnerId`. |
| `group` | int | no | `0` | Numeric gid. |
| `ownerstring` | string | no | `""` | Cloud-init-style separate name field. Rarely set explicitly. |

```yaml
files:
  - path: /etc/foo.conf
    content: |
      hello
    permissions: 0644
    owner: 0
    group: 0
  - path: /usr/local/bin/script
    content: "IyEvYmluL3NoCmVjaG8gaGV5Cg=="
    encoding: b64
    permissions: 0755
```

Name-based chown (e.g. `owner: alice`) is parsed but not yet applied; the plugin warns and skips chown when given a name (TODO).

---

### Download

| Key | Type | Required | Default | Notes |
|---|---|---|---|---|
| `path` | string | yes | `""` | Local destination path. |
| `url` | string | yes | `""` | HTTP(S) URL. |
| `permissions` | int (octal) | no | `0` | If `0`, no chmod. |
| `owner` | int \| string | no | `0` | Numeric uid OR username. |
| `group` | int | no | `0` | Numeric gid. |
| `timeout` | int | no | `30` (when `0`) | Seconds. `0` is treated as the 30s default. |
| `ownerstring` | string | no | `""` | Same as `File.ownerstring`. |

```yaml
downloads:
  - url: https://example.com/blob.bin
    path: /var/lib/blob.bin
    permissions: 0644
    timeout: 60
```

---

### Directory

| Key | Type | Required | Default | Notes |
|---|---|---|---|---|
| `path` | string | yes | `""` | Absolute path. |
| `permissions` | int (octal) | no | `0` | If `0`, no chmod. Applied to the final directory only. |
| `owner` | int \| string | no | `0` | Numeric uid OR username. |
| `group` | int | no | `0` | Numeric gid. |

```yaml
directories:
  - path: /var/lib/foo
    permissions: 0755
    owner: 1000
    group: 1000
```

Intermediate (parent) directories are created via `mkdir_all` but only the final path receives chmod/chown. Go yip applies perms to each newly-created intermediate; yip-rs does not (the gap is the depends-on-pre-existing-state branch which is hard to replicate consistently across VFS impls).

---

### User

| Key | Type | Required | Default | Notes |
|---|---|---|---|---|
| `name` | string | no | (map key) | If absent, the `users:` map key is used. |
| `passwd` | string | no | `""` | Password hash (`$6$...`) OR plaintext. Plaintext is sha512crypt-hashed before write. |
| `lock_passwd` | bool | no | `false` | If `true`, password line is `!`. |
| `uid` | string | no | `""` | Quoted because Go uses string. Empty → reuse or allocate. |
| `gecos` | string | no | `""` | GECOS field. |
| `homedir` | string | no | `""` | Defaults to `/home/<name>` when unset. |
| `no_create_home` | bool | no | `false` | Skip homedir creation. |
| `primary_group` | string | no | `""` | Group name or numeric gid. Empty → group named after user. |
| `groups` | list<string> | no | `[]` | Supplementary groups. Auto-created if missing. |
| `no_user_group` | bool | no | `false` | Don't create a per-user group. |
| `system` | bool | no | `false` | System account (uid < 1000). |
| `no_log_init` | bool | no | `false` | Compat flag — currently no-op in yip-rs. |
| `shell` | string | no | `""` | Defaults to `/bin/bash` when unset. |
| `ssh_authorized_keys` | list<string> | no | `[]` | Same format as the top-level `authorized_keys` value list. |

```yaml
users:
  alice:
    passwd: "$6$rounds=4096$saltsalt$..."
    shell: /bin/bash
    uid: "1001"
    primary_group: alice
    groups: [sudo, docker]
    ssh_authorized_keys:
      - github:alice
```

UID allocation: explicit `uid` wins; else reuse existing `/etc/passwd` entry's uid; else `max(existing_uids)+1` floored at 1000.

---

### YipEntity

| Key | Type | Required | Default | Notes |
|---|---|---|---|---|
| `path` | string | no | (kind default) | Target file. Empty → defaulted from `kind:` in entity body. |
| `entity` | string | yes | `""` | YAML doc describing the entity. See below. |

Entity body fields (parsed from the `entity:` string):

| Field | Applies to | Notes |
|---|---|---|
| `kind` | all | `user`, `group`, `shadow`, `gshadow`. |
| `username` | user, shadow | |
| `password` | user, shadow | Pre-hashed; not auto-hashed. |
| `uid` / `gid` | user | int. |
| `info` | user | GECOS. |
| `homedir` | user | |
| `shell` | user | |
| `group_name` | group | |
| `users` | group | comma-joined member list. |
| `name` | gshadow | |
| `administrators` / `members` | gshadow | |

```yaml
ensure_entities:
  - path: /etc/passwd
    entity: |
      kind: user
      username: foo
      password: "x"
      uid: 1500
      gid: 1500
      info: "Foo Service"
      homedir: /var/lib/foo
      shell: /usr/sbin/nologin
```

Path defaults: user → `/etc/passwd`, group → `/etc/group`, shadow → `/etc/shadow`, gshadow → `/etc/gshadow`.

---

### DNS struct

| Key | Type | Required | Default | Notes |
|---|---|---|---|---|
| `path` | string | no | `/etc/resolv.conf` | Target file. |
| `nameservers` | list<string> | no | `[]` | One `nameserver X` line per entry. **Plugin no-ops when empty** (matches Go). |
| `search` | list<string> | no | `[]` | Joined into one `search a b c` line. Skipped if joined value is `.`. |
| `options` | list<string> | no | `[]` | Joined into one `options a b c` line. Skipped if joined trims to empty. |

```yaml
dns:
  path: /etc/resolv.conf
  nameservers: [8.8.8.8, 1.1.1.1]
  search: [example.com]
  options: ["timeout:2", "attempts:3"]
```

---

### Systemctl struct

| Key | Type | Required | Default | Notes |
|---|---|---|---|---|
| `enable` | list<string> | no | `[]` | `systemctl enable <unit>`. |
| `disable` | list<string> | no | `[]` | `systemctl disable <unit>`. |
| `start` | list<string> | no | `[]` | `systemctl start <unit>`. |
| `mask` | list<string> | no | `[]` | `systemctl mask <unit>`. |
| `overrides` | list<SystemctlOverride> | no | `[]` | Drop-in conf files. Triggers an automatic `systemctl daemon-reload`. |

SystemctlOverride:

| Key | Type | Required | Default | Notes |
|---|---|---|---|---|
| `service` | string | yes | `""` | Target unit name. |
| `content` | string | yes | `""` | File body. |
| `name` | string | no | `override-yip.conf` | Drop-in file name under `/etc/systemd/system/<service>.d/`. |

```yaml
systemctl:
  enable: [sshd.service]
  start: [sshd.service]
  mask: [apport.service]
  overrides:
    - service: sshd.service
      name: 10-restart.conf
      content: |
        [Service]
        Restart=always
        RestartSec=5
```

---

### Packages struct

| Key | Type | Required | Default | Notes |
|---|---|---|---|---|
| `install` | list<string> | no | `[]` | Packages to install. |
| `remove` | list<string> | no | `[]` | Packages to remove. |
| `refresh` | bool | no | `false` | Refresh package index first. |
| `upgrade` | bool | no | `false` | Upgrade all packages. |

Order of operations: `refresh` → `upgrade` → `install` → `remove`. Errors do not short-circuit; each phase runs.

```yaml
packages:
  refresh: true
  install: [vim, htop]
  remove: [nano]
```

Detection (per `/etc/os-release`):

| ID / ID_LIKE | Backend |
|---|---|
| `ubuntu`, `debian`, or `ID_LIKE` contains `debian` | apt |
| `fedora`, `rhel`, `centos`, `rocky`, `alma`, `oracle`, or `ID_LIKE` contains `rhel` | dnf |
| `alpine` | apk |
| `opensuse-*`, `sles` | zypper |

---

### Layout struct

| Key | Type | Required | Default | Notes |
|---|---|---|---|---|
| `device` | Device | no | absent | Target disk + label config. |
| `expand_partition` | ExpandPartition | no | absent | Grow last partition. |
| `add_partitions` | list<Partition> | no | `[]` | New partitions to append. |

Device:

| Key | Type | Required | Default | Notes |
|---|---|---|---|---|
| `path` | string | yes | `""` | Block device. `script://<cmd>` runs `cmd` and uses its trimmed stdout. |
| `label` | string | no | `""` | `gpt` or `mbr`. |
| `init_disk` | bool | no | `false` | If true, wipe + create fresh label. |
| `disk_name` | string | no | `""` | Optional GPT disk name. |

ExpandPartition:

| Key | Type | Required | Default | Notes |
|---|---|---|---|---|
| `size` | u64 (MiB) | no | `0` | `0` = fill all remaining space. |

Partition:

| Key | Type | Required | Default | Notes |
|---|---|---|---|---|
| `fsLabel` | string | no | `""` | Filesystem label (passed to `mkfs.*`). |
| `pLabel` | string | no | `""` | GPT partition label. |
| `size` | u64 (MiB) | no | `0` | `0` for the last partition = fill rest. |
| `filesystem` | string | no | `""` | One of `ext2`/`ext3`/`ext4`/`xfs`/`btrfs`/`vfat`/`fat32`/`swap`. mkfs shells out. |
| `bootable` | bool | no | `false` | MBR/GPT bootable flag. |

```yaml
layout:
  device:
    path: /dev/sda
    label: gpt
    init_disk: false
  add_partitions:
    - fsLabel: COS_PERSISTENT
      pLabel: persistent
      size: 0
      filesystem: ext4
  expand_partition:
    size: 0
```

Filesystem grow (`expand_partition`) uses `resize2fs` for ext*, `xfs_growfs` for xfs, `btrfs filesystem resize` for btrfs. These shell out via the configured `Console`.

---

### UnpackImageConf

| Key | Type | Required | Default | Notes |
|---|---|---|---|---|
| `source` | string | yes | `""` | OCI reference (e.g. `quay.io/foo:tag` or `docker://...`). |
| `target` | string | yes | `""` | Filesystem path where layers are extracted. |
| `platform` | string | no | host platform | e.g. `linux/amd64`, `linux/arm64`. |

```yaml
unpack_images:
  - source: quay.io/kairos/core:latest
    target: /var/lib/kairos/core
    platform: linux/amd64
```

Backend depends on build features (see README): `oci-distribution` native pull (default) or `skopeo` shell-out. Whiteouts (`.wh.*`) and opaque dirs (`.wh..wh..opq`) are honoured.

---

### Git struct

| Key | Type | Required | Default | Notes |
|---|---|---|---|---|
| `url` | string | yes | `""` | Repository URL. Empty → plugin is a no-op. |
| `path` | string | yes | `""` | Local checkout path. |
| `branch` | string | no | `master` | Branch to track. **Default is `master`, not `main`** (matches Go). |
| `branch_only` | bool | no | `false` | `--single-branch` on clone; explicit checkout on update. |
| `auth` | Auth | no | empty | Credentials block. |

Auth:

| Key | Type | Required | Default | Notes |
|---|---|---|---|---|
| `username` | string | no | `""` | HTTP basic-auth username. |
| `password` | string | no | `""` | HTTP basic-auth password / token. |
| `private_key` | string | no | `""` | SSH private key (PEM). |
| `public_key` | string | no | `""` | SSH public key. |
| `insecure` | bool | no | `false` | Skip TLS verification. |

```yaml
git:
  url: https://github.com/foo/bar.git
  path: /opt/bar
  branch: main
  branch_only: true
  auth:
    username: bot
    password: ghp_xxxxx
```

---

### DataSource struct

| Key | Type | Required | Default | Notes |
|---|---|---|---|---|
| `providers` | list<string> | no | `[]` | Ordered list. First provider returning userdata wins. |
| `path` | string | no | `/run/config` | Where fetched metadata + userdata land. |
| `userdata_name` | string | no | `user-data` | Filename for the userdata blob. |

Implemented providers in yip-rs:

| Name | Status | Notes |
|---|---|---|
| `aws` | Done | IMDS at 169.254.169.254. `YIP_AWS_BASE_URL` env var overrides for tests. |
| `nocloud` | Partial | Reads `/var/lib/cloud/seed/nocloud/{user-data,meta-data}` from the VFS. Go also probes block devices; yip-rs does not. |
| `azure` / `gcp` / `openstack` / `digitalocean` / `scaleway` / `hetzner` / `packet` / `vultr` / `metaldata` / `vmware` / `cdrom` / `config-drive` / `file` | Stubbed | Provider lookup returns `Error::Other("provider X not yet ported")`. |

```yaml
datasource:
  providers: [aws, nocloud]
  path: /run/config
  userdata_name: user-data
```

---

## OwnerId helper

`owner` fields on `File` / `Download` / `Directory` accept either form:

```yaml
- path: /tmp/foo
  owner: 1000          # numeric uid

- path: /tmp/bar
  owner: alice         # username string

- path: /tmp/baz
  owner: "1000"        # quoted numeric — still parsed as numeric
```

In Go, the loader populates a separate `OwnerString` field when a name is supplied. yip-rs accepts either form via a tolerant enum (`OwnerId::Numeric(i32)` or `OwnerId::Name(String)`). Name-based chown is parsed but not yet applied — plugins warn and skip when given a name.

---

## Templating

YAML config goes through a templating pass before parse. yip-rs uses [tera](https://keats.github.io/tera/) with a sprig-subset funcmap. The system facts available under `.Values.System.*` (or `.Values.node.*` for hostname etc) match the Go sysinfo / sprig shape.

```yaml
stages:
  rootfs:
    - name: echo
      commands:
        - echo "{{ .Values.node.hostname }}"
        - echo "{{ .Values.system.os }}"
```

Differences vs Go templating:

- yip-rs supports `{{ .Foo.Bar }}` and `{{ Foo.Bar }}` (the leading dot is rewritten on the way in).
- ~60 sprig functions implemented (lower/upper/trim/split/join/replace/quote/squote/cat/indent/nindent/default/empty/coalesce/first/last/len/append/prepend/concat/uniq/keys/values/hasKey/pluck/pick/omit/dict/set/unset/add/sub/mul/div/mod/min/max/int/int64/float/toString/toBool/b64enc/b64dec/sha256sum/sha512sum/md5sum/...). Unimplemented sprig funcs error with a clear "not implemented" message at render time.

---

## DAG ordering, in 30 seconds

For each `--stage X`, yip-rs runs:

1. `X.before` substage
2. `X` substage
3. `X.after` substage

Within one substage, stages run in topological order based on `after:`. Stages without `after:` form an implicit lexical chain (declared order). Cycles are an error.

Errors from individual plugins do not short-circuit. The entire stage list runs; errors accumulate; the executor returns a multi-error at the end.

---

## What's NOT in yip-rs yet

| Feature | Status |
|---|---|
| `#cloud-config` parser | Not implemented. Pass yip-native YAML. |
| Name-based `chown` | Parsed, but plugin warns and skips. |
| All datasource providers except aws + nocloud | Stubbed. |
| `versionlock` for zypper | Skipped. |
| go-diskfs-style in-process fs-grow | Shells out to `resize2fs` / `xfs_growfs` / `btrfs`. |
| systemctl `try-restart` / `reload` | Not exposed (use `commands:`). |
| Full sprig funcmap | ~60/140 funcs ported. |
| Per-intermediate-dir chmod on `directories` | Only final dir gets perms. |

See `docs/migrating-from-go-yip.md` for the diff for Go users.
