# yip-rs Test Coverage vs Go yip

## Summary

- Rust tests: 467 total (`#[test]` + `#[tokio::test]` blocks under `src/` and `tests/`).
  - lib unit tests (`src/`): 451
  - integration `tests/cli.rs`: 8
  - integration `tests/yaml_parse.rs`: 8
- Go tests: 132 `It(...)` blocks across 28 Ginkgo files + 1 stdlib `TestPartitionDevicePath` table-driven test (~24 cases).
- Headline: Rust has roughly **3.5x** more test functions, but Go's coverage is heavier per test (each `It` exercises end-to-end plugin behavior, often via real shell). Several Go behaviors are not exercised by any Rust test.

## Per-module breakdown

### plugins/commands

| Go It block | Rust counterpart | Status |
| --- | --- | --- |
| `execute commands` | `commands::three_commands_all_succeed`, `matches_go_basic_test` | covered |
| `execute templated commands` | (none — templating inside command strings) | **MISSING** |
| - | `commands::empty_stage_is_ok` | extra |
| - | `commands::middle_command_fails_but_others_still_run` | extra |

Go: 2 It, Rust: 4. Coverage: **50%** of Go behaviors. Templated commands gap is real (Go test feeds `{{.Values.foo}}` style template into `commands:` and asserts substitution).

### plugins/dir (directories)

| Go It block | Rust counterpart |
| --- | --- |
| `Creates a /tmp/dir directory` | `directories::creates_one_dir_with_perm` |
| `Changes permissions of existing directory` | `directories::idempotent_on_existing_dir_updates_perm` |
| `Creates /tmp/dir/subdir1/subdir2 ... missing parent dirs` | `directories::creates_nested_dirs` |

Go: 3 It, Rust: 7. Coverage: **100%** + Rust extras (empty_stage, numeric_owner, name_owner_warn, aggregates_errors).

### plugins/dns

| Go It block | Rust counterpart |
| --- | --- |
| `sets dns` | `dns::writes_full_search_and_options` |

Go: 1 It, Rust: 9. Coverage: **100%** + Rust extras (search-only no-write, custom path, empty option drop, single+multi nameserver, search dot drop, callable plugin).

### plugins/download

| Go It block | Rust counterpart |
| --- | --- |
| `downloads correctly in the specified location` | `download::downloads_body_to_path` |
| `downloads correctly in the specified full path` | `download::downloads_body_to_path` (path arg variant) |

Go: 2 It, Rust: 6. Coverage: **100%** + Rust extras (empty list, 404 aggregation, timeout, empty-path error, numeric owner).

### plugins/environment

| Go It block | Rust counterpart |
| --- | --- |
| `configures a /etc/environment setting` | `environment::writes_single_kv_to_default_file` |
| `configures /run/cos/cos-layout.env and creates missing directories` | `environment::respects_custom_environment_file` (path arg) — but **missing-parent-dir creation is not explicitly asserted** |

Go: 2 It, Rust: 10. Coverage: **~90%**. Possibly-missing case: Rust does not assert that environment writer creates a missing parent dir for the env file path. Verify against `environment.rs` behavior.

### plugins/files

| Go It block | Rust counterpart |
| --- | --- |
| `creates a /tmp/test/foo file` | `files::writes_plain_text_file_with_perms` |
| `creates a /testarea/dir/subdir/foo file and its parent directories` | `files::creates_missing_parent_dir` |

Go: 2 It, Rust: 11. Coverage: **100%** + Rust extras (b64, b64+gzip, base64-long alias, multiple files, numeric owner, name owner warn, bad encoding aggregated error, string/text alias decoder, empty stage).

### plugins/git (git build)

| Go It block | Rust counterpart |
| --- | --- |
| `clones a public repo in a path that doesn't exist` | `git::fresh_clone_against_local_bare_repo_creates_git_dir`, `git::git_online_clone_gix` |
| `clones a public repo in a path that does exist but is not a git repo` | (not directly — Rust covers fresh + existing, but **does not cover "path exists but is not git" case explicitly**) | **MISSING** |
| `clones a public repo in a path that is already checked out` | `git::existing_repo_runs_fetch_and_reset` |
| `PIt: clones a private repo` (pending) | covered (basic_auth + private_key tests) |

Go: 3 active It + 1 pending, Rust: 17. Coverage: **80%** + many Rust extras (branch-only, parent-dir creation, fetch failure, basic-auth URL embedding, percent-encoding, GIT_SSH_COMMAND, insecure StrictHostKeyChecking=no, private_key overrides basic_auth). The "exists-but-not-git-repo" case is a real gap that Go's test catches (verifies Rust would `git init` or replace).

### plugins/git_nogit

| Go It | Rust |
| --- | --- |
| `returns a not supported error` | (Rust has no `nogit` build-feature equivalent; gix is always linked) |

Coverage: N/A — Rust never disables git, so this Go test has no semantic mirror. **Not a gap.**

### plugins/hostname

| Go It | Rust |
| --- | --- |
| `configures /etc/hostname` | `hostname::writes_hostname_with_trailing_newline` |

Go: 1, Rust: 6. Coverage: **100%** + Rust extras (machine_id 32-hex, sethostname failure non-fatal, whitespace-only no-op, empty no-op, build callable).

### plugins/if (conditionals umbrella)

| Go It | Rust counterpart |
| --- | --- |
| `IfConditional Executes` | `if_cond::true_command_runs_stage`, `false_command_skips_stage` |
| `IfOsConditional Executes` | `only_if_os::matching_literal_runs` etc. (9 tests) |
| `IfOsVersionConditional Executes` | `only_if_os_version::*` (10 tests) |
| `IfArchConditional Fails with no match` | `if_arch::nonsense_arch_skips` |
| `IfArchConditional Succeeds` | `if_arch::matching_arch_runs` |
| `IfServiceConditional Fails if not supported` | `if_service_manager::missing_proc_comm_skips` |
| `IfServiceConditional Fails if not matched` | `if_service_manager::systemd_does_not_match_openrc` |
| `IfServiceConditional Fails if it finds both` | (no explicit "both systemd+openrc present" case) | **MISSING** |
| `IfServiceConditional Succeeds to find systemctl` | `if_service_manager::systemd_matches_systemd` |
| `IfServiceConditional Succeeds to find openrc` | `if_service_manager::init_with_openrc_run_matches_openrc` |
| `IfFiles Fails with unknown check type` | (Rust enum makes invalid type unrepresentable, but **no serde-deny test for unknown string variant**) | partial |
| `IfFiles Succeeds when all files exist (IfCheckAll)` | `if_files::all_with_both_present_runs` |
| `IfFiles Fails when not all files exist (IfCheckAll)` | `if_files::all_with_one_missing_skips` |
| `IfFiles Succeeds when at least one file exists (IfCheckAny)` | `if_files::any_with_one_present_runs` |
| `IfFiles Fails when no files exist (IfCheckAny)` | `if_files::any_with_none_present_skips` |
| `IfFiles Succeeds when no files exist (IfCheckNone)` | `if_files::none_with_none_present_runs` |
| `IfFiles Fails when at least one file exists (IfCheckNone)` | `if_files::none_with_one_present_skips` |
| `IfFiles Succeeds with empty file list for all checks` | `if_files::empty_path_list_for_check_kind_is_no_op` |

Go: 18 It, Rust: 40 across all conditional files. Coverage: **~95%**. Gaps: "both managers found" race case and "unknown check type" deserialize error.

### plugins/layout

Go has **23 It** in `layout_test.go` plus `TestPartitionDevicePath` (~6 table cases). Rust has **45+** tests in `plugins/layout.rs`.

| Go It block | Rust counterpart |
| --- | --- |
| `computeFreeSpace ... one partition` | (no direct test of compute_free_space helper) | **MISSING** |
| `computeFreeSpace ... multiple partitions` | (none) | **MISSING** |
| `CheckDiskFreeSpaceMiB returns false when nearly full` | (none — no dedicated free-space check test) | **MISSING** |
| `Fails to find device by path` | `init_disk_without_path_errors` (partial) |
| `Fails to find device by label` | `label_only_resolves_via_blkid_to_parent_disk` (covers happy path, not failure) | partial |
| `Adds a new partition by path` | `add_one_ext4_partition_by_path` |
| `Adds a new partition by path with fsLabel` | covered via fs_type_dispatch_* tests |
| `Adds a new partition by label` | `label_only_resolves_via_blkid_to_parent_disk` |
| `Adds a new partition by label with fsLabel` | partial via fs_type_dispatch_* |
| `Fails to add a partition of 1025MiB, only 1024 available` | (no out-of-space failure test) | **MISSING** |
| `Ignores an already existing partition` | `idempotent_skip_when_plabel_exists` |
| `Fails to expand last partition, can't shrink` | (no shrink-rejection test) | **MISSING** |
| `Expands last partition` | `expand_last_partition_emits_resizepart_and_resize2fs` |
| `Expands last partition to take all space` | `expand_with_size_zero_uses_100_percent` |
| `Expands last partition after creating the partitions` | (no combined add+expand test) | **MISSING** |
| `Expands last partition with XFS fs` | (only ext4 expand tested) | **MISSING** |
| `Fails to expand last partition, not enough space` | (none) | **MISSING** |
| `Fails on an xfs fs with a label longer than 12 chars` | `xfs_label_longer_than_12_chars_fails` |
| `Works on an non-xfs fs with a label longer than 12 chars` | (no positive-case mirror) | partial |
| `Adds a swap partition and fails expanding it` | `fs_type_dispatch_swap_uses_mkswap` (covers add, not "fail to expand swap") | partial |
| `Resolves device path via script:// and adds a partition` | `script_prefix_resolved_before_use` |
| `Returns error when script:// script exits non-zero` | `console_ops_resolve_script_device_runs_and_trims` doesn't cover non-zero | **MISSING** (assertion-wise) |
| `Returns error when script:// script produces empty output` | `console_ops_resolve_script_device_empty_output_errors` |
| `TestPartitionDevicePath` (table: sda/vda/mmcblk/nvme/loop/edge cases) | `partition_device_path_handles_digit_devices` (single case) | partial |

Go: 23 It + ~6 table cases, Rust: 45. Coverage of Go behaviors: **~65%**. Biggest gap: free-space planning, capacity-exhaustion errors, and the matrix of "expand+filesystem-type+failure modes".

### plugins/packages

| Go It | Rust counterpart |
| --- | --- |
| `execute proper install commands` | `install_on_ubuntu_fires_apt_get_install`, `full_action_order_on_apt` |
| `execute proper install commands for different OS` | apt/dnf/alpine/zypper full_action_order tests |
| `fails if it cant identify the systems package manager` | `unknown_os_returns_error_other`, `missing_os_release_returns_error_other` |

Go: 3, Rust: 19. Coverage: **100%** + huge Rust extras (per-OS detection, ID_LIKE inheritance, quoted ID, multi error aggregation).

### plugins/script_device

| Go It | Rust counterpart |
| --- | --- |
| `returns a plain path unchanged` | `console_ops_resolve_script_device_passthrough` |
| `executes the script and returns the trimmed stdout` | `console_ops_resolve_script_device_runs_and_trims` |
| `trims leading and trailing whitespace from stdout` | (same as above, partial) | partial |
| `returns an error when the script exits with a non-zero code` | (no non-zero exit assertion) | **MISSING** |
| `returns an error when the script produces no output` | `console_ops_resolve_script_device_empty_output_errors` |
| `returns an error when the script path does not exist` | (none) | **MISSING** |
| `passes arguments to the script` | (none) | **MISSING** |

Go: 7, Rust: 4 (script-device subset under layout). Coverage: **~57%**. Concrete gaps: arg-passing, missing-file, non-zero exit.

### plugins/ssh

| Go It | Rust counterpart |
| --- | --- |
| `configures a user authorized_key` | `ssh::plain_key_written_verbatim`, multi-key, dedupe, github-prefix tests |

Go: 1, Rust: 10. Coverage: **100%** + Rust extras (github: prefix, raw http url, dedupe, idempotency, user-without-passwd skip, http failure non-fatal, empty no-op).

### plugins/sysctl

| Go It | Rust |
| --- | --- |
| `configures a /sys/proc setting` | `sysctl::writes_single_sysctl`, `dot_to_slash_translation`, etc. |

Go: 1, Rust: 6. Coverage: **100%**.

### plugins/systemctl

| Go It | Rust counterpart |
| --- | --- |
| `starts and enables services` | `enable_list_emits_two_calls`, `all_four_lists_emit_calls_in_documented_order` |
| `creates override files` | `override_writes_file_at_expected_drop_in_path` |
| `creates override files if service is given without extension` | `override_auto_appends_service_ext` |
| `creates override files with custom override file name` | (Rust has `override_custom_name_without_ext_gets_conf_appended` — covers the without-ext form; **with-ext custom name not explicit**) | partial |
| `creates override files with custom override file name missing the extension` | `override_custom_name_without_ext_gets_conf_appended` |
| `doesn't do anything if service name is missing` | `override_empty_service_is_skipped` |
| `doesn't do anything if content is missing` | `override_empty_content_is_skipped` |

Go: 7, Rust: 10. Coverage: **~90%** + Rust extras (daemon-reload once, daemon-reload skipped when no overrides, enable failure aggregation).

### plugins/systemd_firstboot

| Go It | Rust |
| --- | --- |
| `sets first-boot configuration` | `keys_concatenated_into_one_call_alphabetical` |

Go: 1, Rust: 4. Coverage: **100%** + extras (empty no-op, two keys one call, shellout error propagation).

### plugins/timesyncd

| Go It | Rust |
| --- | --- |
| `configures timesyncd` | `two_keys_render_alphabetically` |

Go: 1, Rust: 4. Coverage: **100%**.

### plugins/unpack_image

| Go It | Rust counterpart |
| --- | --- |
| `Extracts` | `extracts_files_into_target`, `extracts_plain_tar_without_gzip`, `auto_detects_gzip_from_magic_when_mediatype_empty`, `native_pull_and_extract_alpine` |
| `Extracts for a different platform` | `skopeo_cmd_includes_platform_overrides`, `skopeo_pull_and_extract_alpine` |

Go: 2, Rust: 20. Coverage: **100%** + huge Rust extras (whiteouts, opaque whiteouts, dotdot rejection, symlink, shell escape, blob path, feature-disabled).

### plugins/user

| Go It | Rust counterpart |
| --- | --- |
| `change user password` | `plain_password_gets_sha512crypted`, `opaque_dollar_password_passes_through_unchanged` |
| `set UID and Lockpasswd` | `creates_alice_with_explicit_uid_and_ssh_key`, `lock_passwd_writes_bang` |
| `edits already existing user password` | `applying_twice_does_not_duplicate` |
| `preserves password aging fields when editing an existing user password` | `preserves_aging_fields_on_password_update` |
| `adds users to group` | `adds_user_to_existing_group` |
| `Recreates users with the same UID() and in order` | (Rust has auto-UID tests, but **not "recreate same user keeps UID"**) | partial |
| `Creates the user multiple times, keeping the same UID()` | `applying_twice_does_not_duplicate` (related but not the same assertion) | partial |
| `Creates the user multiple times, keeping the same UID(), even if a new users is added` | (none — multi-user UID stability under additions) | **MISSING** |

Go: 8 It (large, multi-step), Rust: 22 (smaller, focused). Coverage: **~75%**. Critical gap: idempotency-across-multiple-applies with intervening user additions.

### plugins/datasource

| Go It | Rust counterpart |
| --- | --- |
| `Runs datasources and fails to adquire any metadata` | `stub_provider_errors_are_aggregated_into_no_data`, `nocloud_missing_seed_dir_returns_no_metadata_error` |
| `Runs each datasource just once` | `dedup_preserves_order_and_uniqueness` |
| `Properly finds a datasource and transforms it into a userdata file` | `yip_config_userdata_is_mirrored_to_default_name`, `nocloud_provider_reads_seed_userdata` |
| `Properly finds a datasource ... with custom name` | `custom_userdata_name_is_honoured` |
| `Properly decodes VMWARE datasource` | (no VMware provider tests) | **MISSING** |

Go: 5, Rust: 12. Coverage: **80%**. Real gap: VMware datasource provider has no Rust counterpart at all (check if even ported).

### plugins/dot_notation parsing

Go: covered indirectly via `pkg/schema/schema_test.go` `Loading from dot notation` (5 It).
Rust: `schema::dot_notation::*` (6 tests) + `tests/yaml_parse.rs::dot_notation_*` (4 tests). Coverage: **100%**.

### executor

| Go It (16 total) | Rust counterpart |
| --- | --- |
| `Interpolates sys info` | `default_executor_registers_all_plugins` (registration only — **no end-to-end sysinfo interpolation assertion**) | **MISSING** |
| `Filter command node execution` | `conditional_skip_prevents_plugins` |
| `Creates dirs` | `single_plugin_runs_once_per_stage` (related) |
| `Run commands` | `single_plugin_runs_once_per_stage` |
| `Run yip files in sequence` | `directory_walk_picks_up_yaml_and_yml_only` |
| `Run yip files in sequence with after` | `after_dependency_runs_in_topological_order` |
| `Execute single yip files` | `tests/cli.rs::apply_minimal_config_via_inline_yaml`, `apply_fixture_smoke_yaml` |
| `Reports error, and executes all yip files` | `plugin_errors_are_aggregated_not_aborted` |
| `Get Users` | (executor-level user creation — not directly mirrored; covered piecemeal by plugin tests) | partial |
| `Deletes Users` | (no executor-level delete-user test; plugin-level `delete_removes_matching_line` covers it) | partial |
| `Skip with if conditionals` | `conditional_skip_prevents_plugins` |
| `Unnamed steps are run in sequence` | (no explicit "unnamed steps preserve order" test) | **MISSING** |
| `Does not try to merge steps as dependencies based on their name` | (none) | **MISSING** |
| `has multiple instructions` | `tests/yaml_parse::yip_native_multi_stage_full_fixture` |
| `has multiple instructions in different files` | `directory_walk_picks_up_yaml_and_yml_only` |
| `same instructions in different cloud-config files` | (no fixture for merging same-stage across files) | **MISSING** |

Go: 16, Rust executor unit + cli integration: 18. Coverage: **~70%**. Three real gaps above.

### schema

| Go It (9) | Rust counterpart |
| --- | --- |
| 5x `Reads yip file correctly` (dot notation) | `schema::dot_notation::*` + `tests/yaml_parse::dot_notation_*` |
| `Reads cloudconfig to boot stage` | `schema::config::parses_multi_stage_config` |
| `Reads sshkeys to network stage if they require network` | (no test for **automatic ssh-keys → network stage promotion**) | **MISSING** |
| `Reads cloudconfig with a jinja header` | (no jinja-header-stripping test) | **MISSING** |
| `Dumps YipConfig to string and loads it with no issues` | `schema::config::roundtrip` and many per-struct `roundtrip` tests |

Go: 9, Rust: 8 + tons of per-struct schema roundtrip tests (file, dns, git, unpack, layout, packages, stage, systemctl, user, if_files). Coverage: **~80%**. Real gaps: ssh-keys auto-staging, jinja header (`## template: jinja\n`) stripping.

### utils/http, utils/text

| Go It (3) | Rust counterpart |
| --- | --- |
| `detect urls` | (handled inline by `download.rs` URL parse; no dedicated test) | partial |
| `correctly templates input` | `template::engine::*` (11 tests) |
| `Generates strings of the correct length` | (no random string helper test) | **MISSING** (low value) |

Coverage: **~70%**. Random string gap is low priority.

### vfs / console (no Go equivalent)

Rust-only: `vfs::mem` (10), `vfs::temp` (11), `vfs::real` (9), `console::console` (12). Total 42 tests with no Go counterpart — Go uses `afero` with vendored tests. **Already-strong area.**

### CLI (`tests/cli.rs`)

Rust-only end-to-end: version flag, help, no-args, inline yaml apply, fixture apply, analyze, remote http apply (8 tests). Go has no equivalent — `pkg/executor` tests stop at the library API. **Rust ahead.**

## High-priority gaps (10–15 most important missing Rust tests)

1. **Commands plugin: templated command strings**
   Go test feeds `commands: ["echo {{.Values.foo}}"]` with templating context and asserts substitution. Rust does not exercise templating inside `commands:` payloads.
   File: `src/plugins/commands.rs`. Effort: small.

2. **Datasource plugin: VMware provider**
   Go's "Properly decodes VMWARE datasource" parses a base64+gzip blob from a VMware-style payload. Rust has zero VMware coverage (likely the provider isn't ported, or is silently unimplemented).
   File: `src/plugins/datasource.rs`. Effort: medium (may require porting the provider).

3. **Schema: ssh_authorized_keys auto-promotion to `network` stage**
   Go schema test asserts that an empty top-level `ssh_authorized_keys:` block is moved into the `network` stage at parse time. Rust schema has no such test → behavior likely missing.
   File: `src/schema/config.rs` or `src/schema/stage.rs`. Effort: small (test) + possibly medium (behavior).

4. **Schema: jinja header stripping**
   Go test loads a config that begins with `## template: jinja` and asserts the header is removed before YAML parse. Rust schema has no test, and `tests/yaml_parse.rs` does not exercise headers.
   File: `src/schema/config.rs`. Effort: small.

5. **Layout: free-space planner**
   Go has three tests for `computeFreeSpace` and `CheckDiskFreeSpaceMiB` (one partition, many partitions, near-full). Rust's `plan_*` tests cover plan output but not the underlying free-space arithmetic, and there is no "fails to add 1025MiB when 1024 available" test.
   File: `src/plugins/layout.rs`. Effort: small.

6. **Layout: shrink rejection and capacity exhaustion when expanding**
   Go: "Fails to expand last partition, it can't shrink" and "Fails to expand last partition, if there is not enough space left". Rust expand tests only cover happy path + `size: 0`.
   File: `src/plugins/layout.rs`. Effort: small.

7. **Layout: expand last partition with XFS / combined-add-then-expand**
   Go covers expand on XFS specifically and a flow that adds new partitions then expands the last. Rust only covers ext4 expand and only as a stand-alone op.
   File: `src/plugins/layout.rs`. Effort: small.

8. **Layout: swap "fails to expand swap" path**
   Go asserts that attempting to expand a swap partition errors. Rust covers `mkswap` dispatch but not the expand-rejection rule.
   File: `src/plugins/layout.rs`. Effort: small.

9. **script_device: arg passing, missing-file, non-zero-exit**
   Go has 3 dedicated Its that Rust does not mirror in its layout console-ops tests. Worth porting verbatim because `script://` is part of immucore's `rd.cos.disk.layout` pipeline.
   File: `src/plugins/layout.rs` (console_ops_resolve_script_device_*). Effort: small.

10. **Executor: sysinfo interpolation end-to-end**
    Go's first executor test renders `{{.Values.hostname}}` and friends against gathered sysdata. Rust has `template::sysdata::gather_then_render` (unit) but no executor-level integration that proves the executor wires sysdata into the template engine for arbitrary plugins.
    File: `src/executor/default.rs` (new) or `tests/yaml_parse.rs`. Effort: medium.

11. **Executor: unnamed steps preserve declaration order**
    Go: "Unnamed steps are run in sequence". This catches a real class of regressions in the herd/topological sort when steps lack `name:`.
    File: `src/executor/default.rs`. Effort: small.

12. **Executor: same-stage instructions across multiple cloud-config files merge correctly**
    Go: "same instructions in different cloud-config files". Asserts merge semantics when two YAML files both define `stages.boot:` with different content.
    File: `tests/yaml_parse.rs` or `src/executor/default.rs`. Effort: small.

13. **Executor: stage names are not treated as implicit dependencies**
    Go: "Does not try to merge steps as dependencies based on their name". Negative test that two stages sharing a `name:` substring don't accidentally chain via `after:`.
    File: `src/executor/default.rs`. Effort: small.

14. **User plugin: UID stability across multiple applies with intervening new users**
    Go: "Creates the user multiple times, keeping the same UID(), even if a new users is added". Tests that the auto-UID allocator never recycles a UID after a delete/re-add cycle.
    File: `src/plugins/user.rs`. Effort: small.

15. **Git: clone into an existing non-git directory**
    Go: "clones a public repo in a path that does exist but is not a git repo". Rust covers fresh + existing-repo but not the in-between case (path present, not a git repo) — which is exactly the recovery case immucore relies on after partial failures.
    File: `src/plugins/git.rs`. Effort: small.

## Already-strong areas

- `vfs::{mem, temp, real}` — Rust has 30+ tests with cross-impl roundtrip; Go uses afero with no project-local tests.
- `console::console` recording — Rust's `Recording` console + 12 tests of its expect/expect_err/cmds API give richer mocking than Go offers.
- Schema deser/roundtrip — every yaml struct (file, dns, git, layout, packages, stage, systemctl, unpack, user, if_files) has explicit `parses`, `defaults`, `roundtrip` tests. Go schema tests are much sparser.
- Plugin "empty stage is noop" + "build returns callable plugin" pattern is uniform in Rust, missing in Go.
- CLI integration (`tests/cli.rs`) — 8 process-level tests including a real HTTP fetch. Go has no CLI tests.
- Conditionals — Rust splits `if_arch`, `if_files`, `if_cond`, `if_service_manager`, `node`, `only_if_os`, `only_if_os_version` into 40+ focused tests vs Go's 18.
- Layout console-ops shape tests — Rust asserts exact argv for mkpart/mkfs across ext4/xfs/vfat/btrfs/swap/nvme. Go only checks end results.

## Recommendations

**Port these 11 Go test cases verbatim** (with translation):

1. `commands_test.go::execute templated commands`
2. `datasource_test.go::Properly decodes VMWARE datasource`
3. `schema_test.go::Reads sshkeys to network stage if they require network`
4. `schema_test.go::Reads cloudconfig with a jinja header`
5. `layout_test.go::computeFreeSpace and CheckDiskFreeSpaceMiB` (3 cases)
6. `layout_test.go::Fails to add a partition of 1025MiB`
7. `layout_test.go::Fails to expand last partition, it can't shrink`
8. `layout_test.go::Fails to expand last partition, if there is not enough space left`
9. `layout_test.go::Expands last partition after creating the partitions`
10. `layout_test.go::Expands last partition with XFS fs`
11. `layout_test.go::Adds a swap partition and fails expanding it`
12. `script_device_test.go::{passes arguments, script path does not exist, non-zero exit}`
13. `default_test.go::{Unnamed steps in sequence, Does not merge by name, same instructions in different files, Interpolates sys info}`
14. `user_test.go::Creates the user multiple times, keeping the same UID(), even if a new users is added`
15. `git_test.go::clones a public repo in a path that does exist but is not a git repo`

**Add property tests for:**

- `template::engine` — pre-process must be idempotent on any input that contains no `{{`/`}}`.
- `entities` — `merge_or_append` over any list of well-formed passwd/group lines preserves count == input_count + new_count.
- `layout::plan` — for any partition list with disjoint sectors, planner output sectors are also disjoint and ascending.

**Add integration tests for:**

- A full immucore-shaped fixture: cmdline parse → layout → mount → user → systemctl, asserted via console recording.
- HTTP-served yip config with embedded templated commands (end-to-end equivalent of Go's `Execute single yip files`).
- Multi-file directory apply where one file fails: assert remaining files still run and stdout aggregates errors (mirror of `Reports error, and executes all yip files`).
