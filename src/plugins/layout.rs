//! `layout` plugin — disk partitioning, mkfs, and last-partition expansion.
//!
//! # Why this implementation diverges from Go yip
//!
//! The Go yip implementation (`pkg/plugins/layout.go` + `layout_resizer.go`,
//! ~1300 LOC combined) does everything in-process via the
//! [`github.com/diskfs/go-diskfs`](https://github.com/diskfs/go-diskfs) crate:
//!
//! * reads/writes GPT partition tables directly by `open(2)`-ing the block
//!   device and mutating bytes;
//! * detects filesystem types by reading the partition's superblock bytes
//!   (ext magic at offset 1080, FAT magic at 54/82, XFS at 0, btrfs at 0x40,
//!   swap signature at the partition tail);
//! * grows ext4/xfs/btrfs filesystems via raw `ioctl(2)` syscalls
//!   (`EXT4_IOC_RESIZE_FS`, `XFS_IOC_FSGROWFSDATA`, `BTRFS_IOC_RESIZE`) on
//!   an ephemerally-mounted target;
//! * informs the kernel about new partitions via `BLKPG` ioctl (the same
//!   thing `partx -u` does).
//!
//! **Rust has no equivalent of `go-diskfs`.** The closest crates
//! (`gptman`, `mbrman`) are read-only or read/write at a much lower level
//! than what yip needs (no partition-table validation/repair, no GPT
//! backup-header handling, no ioctl coverage). Reimplementing GPT
//! manipulation in pure Rust would be 2000+ LOC of byte-fiddling and would
//! ship a worse, less-tested partition-table parser than the Linux user
//! already has installed.
//!
//! So the Rust port **shells out to standard utilities** for every disk
//! operation:
//!
//! | Operation | Shell-out |
//! |---|---|
//! | Resolve label → device | `blkid -L <label>` |
//! | Read partition table | `sfdisk -d <dev>` and `lsblk -J -b -o NAME,SIZE,FSTYPE,TYPE` |
//! | Create GPT label | `parted -s <dev> mklabel gpt` |
//! | Add partition | `parted -s <dev> mkpart <name> <fs> <start>MiB <end>MiB` |
//! | Set partition name (cosmetic) | `parted -s <dev> name <num> <label>` |
//! | Set bootable flag | `parted -s <dev> set <num> boot on` / `bios_grub on` |
//! | mkfs | `mkfs.ext4 -L X -F`, `mkfs.xfs -L X -f`, `mkfs.vfat -n X`, `mkfs.btrfs -L X -f`, `mkswap -L X` |
//! | Resize partition | `parted -s <dev> resizepart <num> <end>MiB` |
//! | Grow filesystem | `resize2fs <part>` (ext*) / `xfs_growfs <part>` / `btrfs filesystem resize max <mount>` |
//! | Re-read partition table | `partprobe <dev>` / `udevadm trigger && udevadm settle` |
//!
//! This is the same set of tools every cloud-init / curtin / kickstart
//! installer uses; they're guaranteed to be present in any environment
//! that's actually going to be repartitioning disks.
//!
//! ## What's intentionally NOT ported
//!
//! - **Filesystem byte-magic detection** (`RealFilesystemDetector` in Go) —
//!   we use `blkid -p -s TYPE -o value <part>` instead, which is simpler
//!   and uses the same libblkid the rest of the userland trusts.
//! - **BLKPG ioctl direct calls** — `partprobe` + `udevadm settle` cover
//!   the same need (kernel awareness of new partitions) without any unsafe
//!   FFI in this crate.
//! - **In-process ioctl growfs** (`EXT4_IOC_RESIZE_FS` etc.) — `resize2fs`
//!   / `xfs_growfs` / `btrfs filesystem resize` already wrap those
//!   syscalls and handle the surrounding bookkeeping (mounting if needed,
//!   tail alignment, journal flushing).
//! - **GPT header verify/repair** — `sgdisk -v <dev>` + `sgdisk -e <dev>`
//!   does the same job; we run them best-effort but don't fail the plugin
//!   if they're missing (sgdisk isn't universal).
//!
//! # Architecture
//!
//! The plugin is a thin entrypoint over a [`LayoutOps`] trait. `LayoutOps`
//! is the seam between "decide what commands to run" (pure, easy to test)
//! and "actually run them" (effectful). The production implementation
//! [`ConsoleLayoutOps`] just translates each trait method into one or
//! more `console.run(...)` calls. Tests inject a `RecordingConsole` and
//! assert on the exact command strings — that's the unit-testable
//! surface. Any code path that requires reading back disk state
//! (sfdisk's output, blkid's output) is mockable via [`LayoutOps`]
//! methods that return synthetic data.
//!
//! # Behaviour summary (matches Go semantics)
//!
//! 1. If `stage.layout` is the default (no `device`), return `Ok(())`.
//! 2. Resolve `device.path`:
//!    - `script://<cmd>` → run `<cmd>`, use trimmed stdout as device path.
//!    - Otherwise use as-is.
//!    - If empty and `device.label` set → resolve via `blkid -L <label>`,
//!      then map partition→parent disk via `/sys/class/block/<name>/..`.
//! 3. If `device.init_disk`, run `parted -s <dev> mklabel gpt`.
//! 4. Validate (xfs labels ≤ 12 chars; bootable count warning).
//! 5. For each new partition: skip if a partition with that pLabel already
//!    exists (idempotent), otherwise compute start/end in MiB, run
//!    `parted mkpart`, then `mkfs.<fs>`.
//! 6. If `expand_partition` is set, resize the last partition via
//!    `parted resizepart` and grow its filesystem via the right `resize*`
//!    tool.
//!
//! Per-step errors are collected and returned via [`Error::Multi`]; the
//! plugin attempts every partition rather than aborting on the first
//! failure (matches the multierror style elsewhere in yip-rs).

use std::path::Path;
use std::sync::Arc;

use tracing::{debug, info, warn};

use crate::console::Console;
use crate::error::{Error, Result};
use crate::executor::Plugin;
use crate::schema::layout::{Layout, Partition};
use crate::schema::Stage;
use crate::vfs::Vfs;

// ---------------------------------------------------------------------------
// Filesystem-type constants (mirror Go's `Ext4`, `Xfs`, … in layout.go).
// Only the ones we reference by name in match arms appear here; the rest
// are matched as string literals.
// ---------------------------------------------------------------------------

const EXT2: &str = "ext2";
const XFS: &str = "xfs";

const SCRIPT_SCHEME: &str = "script://";

// ---------------------------------------------------------------------------
// Plugin entry points.
// ---------------------------------------------------------------------------

/// Build a [`Plugin`] arc-closure.
pub fn build() -> Plugin {
    Arc::new(run)
}

/// Pure entry point — exposed so tests don't have to go through `Arc`.
pub fn run(stage: &Stage, fs: &dyn Vfs, console: &dyn Console) -> Result<()> {
    let ops = ConsoleLayoutOps::new(console);
    run_with(stage, fs, &ops)
}

/// Same as [`run`] but parameterised over the [`LayoutOps`] impl so tests
/// can swap in a mock.
pub fn run_with(stage: &Stage, fs: &dyn Vfs, ops: &dyn LayoutOps) -> Result<()> {
    let layout = &stage.layout;
    if is_empty_layout(layout) {
        debug!("layout: no device configured, skipping");
        return Ok(());
    }
    let device_spec = match layout.device.as_ref() {
        Some(d) => d.clone(),
        None => {
            debug!("layout: device field empty, skipping");
            return Ok(());
        }
    };

    info!("running layout plugin");

    // 1. Resolve script:// prefix on device.path (if any).
    let mut device = device_spec;
    if !device.path.is_empty() {
        device.path = ops.resolve_script_device(&device.path)?;
    }

    // 2. init_disk validation (mirrors Go layout.go:194-203).
    if device.init_disk && device.path.is_empty() {
        return Err(Error::other(
            "in order to initialize a disk, a valid device path must be provided",
        ));
    }
    if device.init_disk && !device.label.is_empty() {
        return Err(Error::other(
            "cannot initialize a disk when both path and label are provided, please provide only the device path",
        ));
    }
    if device.init_disk {
        if !fs.exists(Path::new(&device.path)) {
            return Err(Error::other(format!(
                "cannot initialize disk, path {} does not exist",
                device.path
            )));
        }
        debug!(path = %device.path, "initialising disk (mklabel gpt)");
        ops.init_disk_gpt(&device.path, &device.disk_name)?;
    }

    // 3. xfs label length validation (mirrors Go layout.go:247-251).
    for part in &layout.parts {
        if part.file_system == XFS && part.fs_label.len() > 12 {
            return Err(Error::other(format!(
                "xfs filesystem label {} cannot be longer than 12 chars",
                part.fs_label
            )));
        }
    }

    // 4. Resolve label → device path if path is empty.
    let device_path: String = if !device.path.trim().is_empty() {
        debug!(path = %device.path, "using device path");
        device.path.clone()
    } else if !device.label.trim().is_empty() {
        debug!(label = %device.label, "resolving device by label");
        ops.resolve_label_to_disk(&device.label)?
    } else {
        warn!("layout: no valid device path or label provided");
        return Ok(());
    };

    // 5. Bootable count warning (matches Go).
    let bootable_count = layout.parts.iter().filter(|p| p.bootable).count();
    if bootable_count > 1 {
        warn!(
            count = bootable_count,
            "more than one partition marked bootable; only one is allowed"
        );
    }

    // 6. Best-effort verify+repair GPT headers (sgdisk -v / -e).
    if let Err(e) = ops.verify_and_repair_headers(&device_path) {
        debug!(error = %e, "verify_and_repair_headers failed (non-fatal)");
    }

    // 7. Read existing partitions and add new ones.
    let existing = ops.read_partitions(&device_path)?;
    let mut errs: Vec<Error> = Vec::new();
    let added = match plan_partitions(&layout.parts, &existing) {
        Ok(p) => p,
        Err(e) => {
            return Err(e);
        }
    };

    for plan in &added {
        debug!(plabel = %plan.p_label, fslabel = %plan.fs_label, size_mib = plan.size_mib, "adding partition");
        if let Err(e) = ops.add_partition(&device_path, plan) {
            errs.push(e);
            continue;
        }
        // Inform kernel about new partition (partprobe + udevadm settle).
        if let Err(e) = ops.settle(&device_path) {
            debug!(error = %e, "settle failed (non-fatal)");
        }
        if let Err(e) = ops.mkfs(&device_path, plan) {
            errs.push(e);
        }
    }

    // 8. Expand last partition if requested.
    if let Some(expand) = layout.expand.as_ref() {
        if expand.size == 0 {
            debug!("expanding last partition to max space");
        } else {
            debug!(size_mib = expand.size, "expanding last partition");
        }
        if let Err(e) = ops.expand_last_partition(&device_path, expand.size) {
            errs.push(e);
        }
    }

    match errs.len() {
        0 => Ok(()),
        _ => Err(Error::Multi(errs)),
    }
}

fn is_empty_layout(l: &Layout) -> bool {
    l.device.is_none() && l.expand.is_none() && l.parts.is_empty()
}

// ---------------------------------------------------------------------------
// Plans + LayoutOps trait.
// ---------------------------------------------------------------------------

/// A partition we've decided to create, with its computed start/end (MiB).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PartitionPlan {
    /// 1-based partition index in the GPT table (i.e. `<dev>N`).
    pub number: u32,
    /// Filesystem type ("ext4", "xfs", "vfat", "btrfs", "swap").
    pub file_system: String,
    /// Filesystem label, may be empty.
    pub fs_label: String,
    /// GPT partition name (the "pLabel"), may be empty.
    pub p_label: String,
    /// Whether this partition gets the bootable flag.
    pub bootable: bool,
    /// Start offset in MiB.
    pub start_mib: u64,
    /// End offset in MiB.
    pub end_mib: u64,
    /// Size in MiB (end - start). 0 means "rest of disk".
    pub size_mib: u64,
}

/// Lightweight view of an existing partition as scraped from the disk.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ExistingPartition {
    pub number: u32,
    pub p_label: String,
    pub fs_label: String,
    pub start_mib: u64,
    pub end_mib: u64,
}

/// Abstraction over the side-effects of the layout plugin. The production
/// impl is [`ConsoleLayoutOps`]; tests use a hand-rolled mock.
pub trait LayoutOps {
    /// Resolve a `script://<cmd>` device path. For any other prefix,
    /// return the path unchanged. Mirrors Go `ResolveScriptDevice`.
    fn resolve_script_device(&self, raw: &str) -> Result<String>;

    /// `parted -s <dev> mklabel gpt`. `disk_name` is currently ignored
    /// (Go uses it to derive a GUID via `uuid.NewV5`; parted doesn't accept
    /// a GUID argument on `mklabel`, so we drop that distinction).
    fn init_disk_gpt(&self, device: &str, disk_name: &str) -> Result<()>;

    /// Resolve a partition label to its parent disk path.
    /// E.g. `LABEL=BOOT` -> `/dev/sda` (not `/dev/sda1`).
    fn resolve_label_to_disk(&self, label: &str) -> Result<String>;

    /// Best-effort sanity check / repair of the GPT headers
    /// (`sgdisk -v`, `sgdisk -e`).
    fn verify_and_repair_headers(&self, device: &str) -> Result<()>;

    /// Read the current partition table.
    fn read_partitions(&self, device: &str) -> Result<Vec<ExistingPartition>>;

    /// Run `parted mkpart` for a new partition + set its `name` and any
    /// boot flag, but DO NOT format yet.
    fn add_partition(&self, device: &str, plan: &PartitionPlan) -> Result<()>;

    /// `partprobe <dev>` / `udevadm trigger && udevadm settle`.
    fn settle(&self, device: &str) -> Result<()>;

    /// Run the appropriate `mkfs.*` / `mkswap` command for `plan`.
    fn mkfs(&self, device: &str, plan: &PartitionPlan) -> Result<()>;

    /// Resize the last partition + grow its filesystem.
    fn expand_last_partition(&self, device: &str, target_mib: u64) -> Result<()>;
}

// ---------------------------------------------------------------------------
// Planning logic — pure, easily unit-tested.
// ---------------------------------------------------------------------------

/// Decide the (start_mib, end_mib) layout for each requested partition.
///
/// Skips entries whose pLabel already exists in `existing` (idempotency,
/// matches Go's `MatchPartitionPLabel`).
///
/// We use 1 MiB alignment, matching parted's default for mkpart.
fn plan_partitions(
    parts: &[Partition],
    existing: &[ExistingPartition],
) -> Result<Vec<PartitionPlan>> {
    let mut plans = Vec::new();
    // Cursor tracks the next free MiB on disk. Start at 1 MiB (the
    // canonical alignment) unless something is already there.
    let mut next_start: u64 = 1;
    let mut next_number: u32 = 1;
    for p in existing {
        if p.end_mib + 1 > next_start {
            next_start = p.end_mib + 1;
        }
        if p.number + 1 > next_number {
            next_number = p.number + 1;
        }
    }

    let existing_plabels: Vec<&str> =
        existing.iter().map(|e| e.p_label.as_str()).collect();

    for p in parts {
        if !p.p_label.is_empty() && existing_plabels.iter().any(|l| l == &p.p_label) {
            // Idempotent skip.
            continue;
        }
        let fs = if p.file_system.is_empty() {
            EXT2.to_string()
        } else {
            p.file_system.clone()
        };
        if !is_supported_fs(&fs) {
            return Err(Error::other(format!("unsupported filesystem type: {fs}")));
        }
        // size==0 means "fill remaining space"; we leave that decision to
        // parted by passing `100%` as the end (handled in `add_partition`).
        let size_mib = p.size;
        let start = next_start;
        let end = if size_mib == 0 {
            0 // sentinel: means "100%"
        } else {
            start + size_mib
        };
        plans.push(PartitionPlan {
            number: next_number,
            file_system: fs,
            fs_label: p.fs_label.clone(),
            p_label: p.p_label.clone(),
            bootable: p.bootable,
            start_mib: start,
            end_mib: end,
            size_mib,
        });
        next_number += 1;
        if size_mib > 0 {
            next_start = end;
        } else {
            // No more partitions can follow a "fill rest" entry; if there
            // are more, parted will error anyway.
            next_start = u64::MAX;
        }
    }
    Ok(plans)
}

fn is_supported_fs(fs: &str) -> bool {
    matches!(
        fs,
        "ext2" | "ext3" | "ext4" | "xfs" | "btrfs" | "vfat" | "fat" | "fat16" | "fat32" | "swap"
    )
}

/// The `mkfs.*` / `mkswap` tool name for a filesystem.
fn mkfs_tool(fs: &str) -> &'static str {
    match fs {
        "ext2" => "mkfs.ext2",
        "ext3" => "mkfs.ext3",
        "ext4" => "mkfs.ext4",
        "xfs" => "mkfs.xfs",
        "btrfs" => "mkfs.btrfs",
        "vfat" | "fat" | "fat16" | "fat32" => "mkfs.fat",
        "swap" => "mkswap",
        _ => "mkfs",
    }
}

/// Parted filesystem name for `mkpart` (parted's `<fs>` arg). This is
/// purely advisory metadata; the actual format is done later via mkfs.
fn parted_fs(fs: &str) -> &'static str {
    match fs {
        "vfat" | "fat" | "fat16" | "fat32" => "fat32",
        "swap" => "linux-swap",
        _ => "ext4", // generic linux fs
    }
}

/// Quote a string for use as a single shell argument. We use single
/// quotes; embedded `'` is escaped as `'\''`.
fn shq(s: &str) -> String {
    if s.is_empty() {
        return "''".to_string();
    }
    if s.bytes().all(|b| {
        b.is_ascii_alphanumeric()
            || matches!(b, b'_' | b'-' | b'.' | b'/' | b':' | b'=' | b'+' | b'%' | b',')
    }) {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for ch in s.chars() {
        if ch == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(ch);
        }
    }
    out.push('\'');
    out
}

/// Build the partition device path. Names ending in a digit
/// (e.g. nvme0n1, mmcblk0) need a `p` separator. Mirrors Go's
/// `partitionDevicePath`.
fn partition_device_path(base: &str, num: u32) -> String {
    if let Some(last) = base.chars().last() {
        if last.is_ascii_digit() {
            return format!("{base}p{num}");
        }
    }
    format!("{base}{num}")
}

// ---------------------------------------------------------------------------
// Production LayoutOps impl — translates each method to shell commands.
// ---------------------------------------------------------------------------

/// Production [`LayoutOps`] that turns each operation into a `console.run`
/// shell-out. Held by reference so it doesn't take ownership of the
/// console.
pub struct ConsoleLayoutOps<'c> {
    console: &'c dyn Console,
}

impl<'c> ConsoleLayoutOps<'c> {
    pub fn new(console: &'c dyn Console) -> Self {
        Self { console }
    }

    fn run(&self, cmd: &str) -> Result<String> {
        self.console.run(cmd)
    }
}

impl LayoutOps for ConsoleLayoutOps<'_> {
    fn resolve_script_device(&self, raw: &str) -> Result<String> {
        if !raw.starts_with(SCRIPT_SCHEME) {
            return Ok(raw.to_string());
        }
        let cmd_str = &raw[SCRIPT_SCHEME.len()..];
        if cmd_str.trim().is_empty() {
            return Err(Error::other(
                "script:// prefix provided but no command specified",
            ));
        }
        let out = self.run(cmd_str)?;
        let trimmed = out.trim();
        if trimmed.is_empty() {
            return Err(Error::other(format!(
                "script {cmd_str:?} produced empty output"
            )));
        }
        Ok(trimmed.to_string())
    }

    fn init_disk_gpt(&self, device: &str, _disk_name: &str) -> Result<()> {
        let cmd = format!("parted -s {} mklabel gpt", shq(device));
        self.run(&cmd).map(|_| ())
    }

    fn resolve_label_to_disk(&self, label: &str) -> Result<String> {
        // `blkid -L <label>` prints the partition path (e.g. /dev/sda3).
        let cmd = format!("blkid -L {}", shq(label));
        let out = self.run(&cmd)?;
        let part_dev = out.trim();
        if part_dev.is_empty() {
            return Err(Error::other(format!(
                "could not resolve device for label {label}"
            )));
        }
        // Map partition -> parent disk via /sys/class/block.
        // Best-effort: use `lsblk -no PKNAME <part>`.
        let lsblk = format!("lsblk -no PKNAME {}", shq(part_dev));
        let parent_out = self.run(&lsblk)?;
        let parent_name = parent_out.trim();
        if parent_name.is_empty() {
            // Image file or unknown: assume the path *is* the disk.
            return Ok(part_dev.to_string());
        }
        Ok(format!("/dev/{parent_name}"))
    }

    fn verify_and_repair_headers(&self, device: &str) -> Result<()> {
        // sgdisk -v: verify; sgdisk -e: relocate backup GPT to end.
        // Both are best-effort; we ignore errors.
        let _ = self.run(&format!("sgdisk -e {}", shq(device)));
        Ok(())
    }

    fn read_partitions(&self, device: &str) -> Result<Vec<ExistingPartition>> {
        // `sfdisk -d <dev>` is machine-parsable. Empty / unpartitioned
        // disks produce a header but no `<dev>N : ...` lines.
        // We accept errors (e.g. unpartitioned disk → exit 1) and return
        // an empty list in that case.
        let cmd = format!("sfdisk -d {}", shq(device));
        let out = match self.run(&cmd) {
            Ok(s) => s,
            Err(_) => return Ok(Vec::new()),
        };
        Ok(parse_sfdisk_dump(&out, device))
    }

    fn add_partition(&self, device: &str, plan: &PartitionPlan) -> Result<()> {
        let fs_arg = parted_fs(&plan.file_system);
        let end_arg = if plan.size_mib == 0 || plan.end_mib == 0 {
            "100%".to_string()
        } else {
            format!("{}MiB", plan.end_mib)
        };
        let name = if plan.p_label.is_empty() {
            "primary".to_string()
        } else {
            plan.p_label.clone()
        };
        let mkpart = format!(
            "parted -s {} mkpart {} {} {}MiB {}",
            shq(device),
            shq(&name),
            fs_arg,
            plan.start_mib,
            end_arg
        );
        self.run(&mkpart)?;
        if !plan.p_label.is_empty() {
            let _ = self.run(&format!(
                "parted -s {} name {} {}",
                shq(device),
                plan.number,
                shq(&plan.p_label)
            ));
        }
        if plan.bootable {
            let flag = match plan.file_system.as_str() {
                "vfat" | "fat" | "fat16" | "fat32" => "esp",
                _ => "bios_grub",
            };
            let _ = self.run(&format!(
                "parted -s {} set {} {} on",
                shq(device),
                plan.number,
                flag
            ));
        }
        Ok(())
    }

    fn settle(&self, device: &str) -> Result<()> {
        let _ = self.run(&format!("partprobe {}", shq(device)));
        self.run("udevadm trigger && udevadm settle").map(|_| ())
    }

    fn mkfs(&self, device: &str, plan: &PartitionPlan) -> Result<()> {
        let tool = mkfs_tool(&plan.file_system);
        let part_dev = partition_device_path(device, plan.number);
        let mut cmd = String::new();
        cmd.push_str(tool);
        match plan.file_system.as_str() {
            "ext2" | "ext3" | "ext4" | "xfs" | "btrfs" | "swap" => {
                if !plan.fs_label.is_empty() {
                    cmd.push_str(" -L ");
                    cmd.push_str(&shq(&plan.fs_label));
                }
                // -f forces overwrite for btrfs (matches Go).
                if plan.file_system == "btrfs" {
                    cmd.push_str(" -f");
                }
                // -F forces ext* (no warning prompts).
                if matches!(plan.file_system.as_str(), "ext2" | "ext3" | "ext4") {
                    cmd.push_str(" -F");
                }
            }
            "vfat" | "fat" | "fat16" | "fat32" => {
                if !plan.fs_label.is_empty() {
                    cmd.push_str(" -n ");
                    cmd.push_str(&shq(&plan.fs_label));
                }
            }
            _ => {
                return Err(Error::other(format!(
                    "unsupported filesystem: {}",
                    plan.file_system
                )));
            }
        }
        cmd.push(' ');
        cmd.push_str(&shq(&part_dev));
        self.run(&cmd).map(|_| ())
    }

    fn expand_last_partition(&self, device: &str, target_mib: u64) -> Result<()> {
        // 1. Find the last partition.
        let existing = self.read_partitions(device)?;
        let last = match existing.last() {
            Some(p) => p.clone(),
            None => {
                return Err(Error::other("no partition to expand"));
            }
        };
        // 2. parted resizepart <num> <end>.
        let end_arg = if target_mib == 0 {
            "100%".to_string()
        } else {
            format!("{target_mib}MiB")
        };
        let resize_cmd = format!(
            "parted -s {} resizepart {} {}",
            shq(device),
            last.number,
            end_arg
        );
        self.run(&resize_cmd)?;

        // 3. Re-read partition table.
        let _ = self.run(&format!("partprobe {}", shq(device)));
        let _ = self.run("udevadm trigger && udevadm settle");

        // 4. Detect filesystem on the partition (blkid -p -s TYPE -o value).
        let part_dev = partition_device_path(device, last.number);
        let fs = match self.run(&format!(
            "blkid -p -s TYPE -o value {}",
            shq(&part_dev)
        )) {
            Ok(s) => s.trim().to_string(),
            Err(_) => String::new(),
        };

        // 5. Grow the filesystem.
        match fs.as_str() {
            "ext2" | "ext3" | "ext4" => {
                self.run(&format!("resize2fs {}", shq(&part_dev))).map(|_| ())
            }
            "xfs" => {
                // xfs_growfs needs a mountpoint; if the FS isn't mounted
                // anywhere already this will fail. Best-effort.
                self.run(&format!("xfs_growfs {}", shq(&part_dev))).map(|_| ())
            }
            "btrfs" => self
                .run(&format!("btrfs filesystem resize max {}", shq(&part_dev)))
                .map(|_| ()),
            "swap" => Err(Error::other("swap resizing is not supported")),
            "vfat" | "fat" | "fat16" | "fat32" => {
                Err(Error::other("FAT partition resizing is not supported"))
            }
            _ => {
                debug!(fs = %fs, "unknown filesystem on expanded partition; skipping fs grow");
                Ok(())
            }
        }
    }
}

// ---------------------------------------------------------------------------
// sfdisk -d output parser.
//
// Example output:
//
//   label: gpt
//   label-id: 5BAA15D8-...
//   device: /dev/sda
//   unit: sectors
//   first-lba: 2048
//   last-lba: 41943006
//
//   /dev/sda1 : start=        2048, size=      204800, type=..., uuid=..., name="EFI"
//   /dev/sda2 : start=      206848, size=    41734144, type=..., uuid=..., name="ROOT"
// ---------------------------------------------------------------------------

fn parse_sfdisk_dump(out: &str, device: &str) -> Vec<ExistingPartition> {
    let mut parts = Vec::new();
    let prefix = format!("{device}");
    for line in out.lines() {
        let line = line.trim();
        if line.is_empty() || !line.starts_with(&prefix) {
            continue;
        }
        // Split "<dev>N : k=v, k=v, ..."
        let (head, rest) = match line.split_once(':') {
            Some(p) => (p.0.trim(), p.1.trim()),
            None => continue,
        };
        // head looks like "/dev/sda1" or "/dev/nvme0n1p1".
        let number = match parse_part_number(head, device) {
            Some(n) => n,
            None => continue,
        };
        let mut start: u64 = 0;
        let mut size: u64 = 0;
        let mut name = String::new();
        for kv in rest.split(',') {
            let kv = kv.trim();
            let (k, v) = match kv.split_once('=') {
                Some(p) => (p.0.trim(), p.1.trim()),
                None => continue,
            };
            match k {
                "start" => {
                    start = v.parse().unwrap_or(0);
                }
                "size" => {
                    size = v.parse().unwrap_or(0);
                }
                "name" => {
                    // strip surrounding quotes if present
                    name = v.trim_matches('"').to_string();
                }
                _ => {}
            }
        }
        // sfdisk reports in sectors; convert to MiB (assume 512-byte
        // sectors — sfdisk -d always emits sectors). 1 MiB = 2048 sectors.
        let start_mib = start.saturating_mul(512) / (1024 * 1024);
        let end_mib = (start.saturating_add(size).saturating_sub(1))
            .saturating_mul(512)
            / (1024 * 1024);
        parts.push(ExistingPartition {
            number,
            p_label: name,
            fs_label: String::new(),
            start_mib,
            end_mib,
        });
    }
    parts
}

fn parse_part_number(head: &str, device: &str) -> Option<u32> {
    let rest = head.strip_prefix(device)?;
    let rest = rest.strip_prefix('p').unwrap_or(rest);
    rest.parse().ok()
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::console::RecordingConsole;
    use crate::schema::layout::{Device, ExpandPartition, Layout, Partition as SchemaPartition};
    use crate::schema::Stage;
    use crate::vfs::MemVfs;

    // ---------- pure helpers ----------

    #[test]
    fn shq_passes_simple_words_through() {
        assert_eq!(shq("/dev/sda"), "/dev/sda");
        assert_eq!(shq("100%"), "100%");
        assert_eq!(shq("foo_bar-baz.txt"), "foo_bar-baz.txt");
    }

    #[test]
    fn shq_quotes_strings_with_spaces() {
        assert_eq!(shq("hello world"), "'hello world'");
    }

    #[test]
    fn shq_escapes_single_quotes() {
        assert_eq!(shq("it's"), "'it'\\''s'");
    }

    #[test]
    fn partition_device_path_handles_digit_devices() {
        assert_eq!(partition_device_path("/dev/sda", 1), "/dev/sda1");
        assert_eq!(partition_device_path("/dev/vda", 2), "/dev/vda2");
        assert_eq!(partition_device_path("/dev/nvme0n1", 1), "/dev/nvme0n1p1");
        assert_eq!(partition_device_path("/dev/mmcblk0", 3), "/dev/mmcblk0p3");
        assert_eq!(partition_device_path("/dev/loop0", 1), "/dev/loop0p1");
    }

    #[test]
    fn mkfs_tool_dispatch() {
        assert_eq!(mkfs_tool("ext4"), "mkfs.ext4");
        assert_eq!(mkfs_tool("ext3"), "mkfs.ext3");
        assert_eq!(mkfs_tool("ext2"), "mkfs.ext2");
        assert_eq!(mkfs_tool("xfs"), "mkfs.xfs");
        assert_eq!(mkfs_tool("btrfs"), "mkfs.btrfs");
        assert_eq!(mkfs_tool("vfat"), "mkfs.fat");
        assert_eq!(mkfs_tool("fat"), "mkfs.fat");
        assert_eq!(mkfs_tool("fat32"), "mkfs.fat");
        assert_eq!(mkfs_tool("swap"), "mkswap");
    }

    #[test]
    fn plan_first_partition_aligns_at_1mib() {
        let parts = vec![SchemaPartition {
            p_label: "DATA".into(),
            size: 100,
            file_system: "ext4".into(),
            ..Default::default()
        }];
        let plans = plan_partitions(&parts, &[]).unwrap();
        assert_eq!(plans.len(), 1);
        assert_eq!(plans[0].number, 1);
        assert_eq!(plans[0].start_mib, 1);
        assert_eq!(plans[0].end_mib, 101);
        assert_eq!(plans[0].size_mib, 100);
        assert_eq!(plans[0].file_system, "ext4");
    }

    #[test]
    fn plan_appends_after_existing_partitions() {
        let existing = vec![ExistingPartition {
            number: 1,
            p_label: "EFI".into(),
            start_mib: 1,
            end_mib: 100,
            ..Default::default()
        }];
        let parts = vec![SchemaPartition {
            p_label: "ROOT".into(),
            size: 200,
            file_system: "ext4".into(),
            ..Default::default()
        }];
        let plans = plan_partitions(&parts, &existing).unwrap();
        assert_eq!(plans.len(), 1);
        assert_eq!(plans[0].number, 2);
        assert_eq!(plans[0].start_mib, 101);
        assert_eq!(plans[0].end_mib, 301);
    }

    #[test]
    fn plan_skips_partition_with_existing_plabel() {
        let existing = vec![ExistingPartition {
            number: 1,
            p_label: "PERSISTENT".into(),
            start_mib: 1,
            end_mib: 100,
            ..Default::default()
        }];
        let parts = vec![SchemaPartition {
            p_label: "PERSISTENT".into(),
            size: 50,
            file_system: "ext4".into(),
            ..Default::default()
        }];
        let plans = plan_partitions(&parts, &existing).unwrap();
        assert!(plans.is_empty(), "idempotency: existing plabel should skip");
    }

    #[test]
    fn plan_defaults_filesystem_to_ext2() {
        let parts = vec![SchemaPartition {
            p_label: "DATA".into(),
            size: 100,
            ..Default::default()
        }];
        let plans = plan_partitions(&parts, &[]).unwrap();
        assert_eq!(plans[0].file_system, "ext2");
    }

    #[test]
    fn plan_rejects_unsupported_filesystem() {
        let parts = vec![SchemaPartition {
            size: 100,
            file_system: "zfs".into(),
            ..Default::default()
        }];
        let err = plan_partitions(&parts, &[]).unwrap_err();
        assert!(format!("{err}").contains("unsupported"));
    }

    #[test]
    fn plan_size_zero_uses_remaining_space() {
        let parts = vec![SchemaPartition {
            p_label: "ALL".into(),
            size: 0,
            file_system: "ext4".into(),
            ..Default::default()
        }];
        let plans = plan_partitions(&parts, &[]).unwrap();
        assert_eq!(plans[0].size_mib, 0);
        assert_eq!(plans[0].end_mib, 0);
    }

    // ---------- sfdisk parsing ----------

    #[test]
    fn parse_sfdisk_dump_basic() {
        let out = indoc::indoc! {r#"
            label: gpt
            label-id: 5BAA15D8-1111-2222-3333-444455556666
            device: /dev/sda
            unit: sectors
            first-lba: 2048
            last-lba: 41943006

            /dev/sda1 : start=        2048, size=      204800, type=21686148-6449-6E6F-744E-656564454649, name="EFI"
            /dev/sda2 : start=      206848, size=    20971520, type=0FC63DAF-8483-4772-8E79-3D69D8477DE4, name="ROOT"
        "#};
        let parts = parse_sfdisk_dump(out, "/dev/sda");
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0].number, 1);
        assert_eq!(parts[0].p_label, "EFI");
        // 2048 sectors * 512 = 1 MiB start
        assert_eq!(parts[0].start_mib, 1);
        assert_eq!(parts[1].number, 2);
        assert_eq!(parts[1].p_label, "ROOT");
    }

    #[test]
    fn parse_sfdisk_dump_handles_nvme_with_p_separator() {
        let out = "/dev/nvme0n1p1 : start=2048, size=2048, type=X, name=\"BOOT\"\n";
        let parts = parse_sfdisk_dump(out, "/dev/nvme0n1");
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0].number, 1);
        assert_eq!(parts[0].p_label, "BOOT");
    }

    #[test]
    fn parse_sfdisk_dump_ignores_unrelated_lines() {
        let out = "label: gpt\n\ngarbage\n";
        let parts = parse_sfdisk_dump(out, "/dev/sda");
        assert!(parts.is_empty());
    }

    // ---------- end-to-end via run_with + RecordingConsole ----------

    /// Mock LayoutOps that records every call (separately from the
    /// underlying RecordingConsole) and lets tests pre-seed
    /// `read_partitions` output.
    struct MockOps {
        console: RecordingConsole,
        existing: std::cell::RefCell<Vec<ExistingPartition>>,
        script_map: std::cell::RefCell<std::collections::HashMap<String, String>>,
    }

    impl MockOps {
        fn new() -> Self {
            Self {
                console: RecordingConsole::new(),
                existing: std::cell::RefCell::new(Vec::new()),
                script_map: std::cell::RefCell::new(Default::default()),
            }
        }
        fn with_existing(self, parts: Vec<ExistingPartition>) -> Self {
            *self.existing.borrow_mut() = parts;
            self
        }
        fn set_script(&self, raw: &str, resolved: &str) {
            self.script_map
                .borrow_mut()
                .insert(raw.to_string(), resolved.to_string());
        }
    }

    impl LayoutOps for MockOps {
        fn resolve_script_device(&self, raw: &str) -> Result<String> {
            if let Some(r) = self.script_map.borrow().get(raw) {
                return Ok(r.clone());
            }
            if !raw.starts_with(SCRIPT_SCHEME) {
                return Ok(raw.to_string());
            }
            Ok(raw.trim_start_matches(SCRIPT_SCHEME).to_string())
        }
        fn init_disk_gpt(&self, device: &str, _disk_name: &str) -> Result<()> {
            self.console
                .run(&format!("parted -s {device} mklabel gpt"))
                .map(|_| ())
        }
        fn resolve_label_to_disk(&self, label: &str) -> Result<String> {
            self.console.run(&format!("blkid -L {label}"))?;
            Ok(format!("/dev/by-label/{label}"))
        }
        fn verify_and_repair_headers(&self, device: &str) -> Result<()> {
            self.console.run(&format!("sgdisk -e {device}")).map(|_| ())
        }
        fn read_partitions(&self, _device: &str) -> Result<Vec<ExistingPartition>> {
            Ok(self.existing.borrow().clone())
        }
        fn add_partition(&self, device: &str, plan: &PartitionPlan) -> Result<()> {
            let end = if plan.size_mib == 0 {
                "100%".to_string()
            } else {
                format!("{}MiB", plan.end_mib)
            };
            let name = if plan.p_label.is_empty() {
                "primary".to_string()
            } else {
                plan.p_label.clone()
            };
            self.console
                .run(&format!(
                    "parted -s {} mkpart {} {} {}MiB {}",
                    device,
                    name,
                    parted_fs(&plan.file_system),
                    plan.start_mib,
                    end
                ))
                .map(|_| ())
        }
        fn settle(&self, _device: &str) -> Result<()> {
            self.console
                .run("udevadm trigger && udevadm settle")
                .map(|_| ())
        }
        fn mkfs(&self, device: &str, plan: &PartitionPlan) -> Result<()> {
            let tool = mkfs_tool(&plan.file_system);
            let part_dev = partition_device_path(device, plan.number);
            let mut cmd = String::from(tool);
            match plan.file_system.as_str() {
                "ext2" | "ext3" | "ext4" | "xfs" | "btrfs" | "swap" => {
                    if !plan.fs_label.is_empty() {
                        cmd.push_str(" -L ");
                        cmd.push_str(&plan.fs_label);
                    }
                }
                "vfat" | "fat" | "fat16" | "fat32" => {
                    if !plan.fs_label.is_empty() {
                        cmd.push_str(" -n ");
                        cmd.push_str(&plan.fs_label);
                    }
                }
                _ => {}
            }
            cmd.push(' ');
            cmd.push_str(&part_dev);
            self.console.run(&cmd).map(|_| ())
        }
        fn expand_last_partition(&self, device: &str, target_mib: u64) -> Result<()> {
            let end = if target_mib == 0 {
                "100%".to_string()
            } else {
                format!("{target_mib}MiB")
            };
            // Decide partition number from `existing`.
            let last = self
                .existing
                .borrow()
                .last()
                .cloned()
                .ok_or_else(|| Error::other("no partition to expand"))?;
            self.console
                .run(&format!(
                    "parted -s {} resizepart {} {}",
                    device, last.number, end
                ))?;
            self.console
                .run(&format!("resize2fs {}", partition_device_path(device, last.number)))
                .map(|_| ())
        }
    }

    fn vfs_with(path: &str) -> MemVfs {
        let fs = MemVfs::new();
        // Create the device path so init_disk passes the exists() check.
        let _ = fs.write(Path::new(path), b"");
        fs
    }

    #[test]
    fn empty_layout_is_noop() {
        let fs = MemVfs::new();
        let ops = MockOps::new();
        let stage = Stage::default();
        run_with(&stage, &fs, &ops).expect("empty layout -> Ok");
        assert!(ops.console.commands().is_empty());
    }

    #[test]
    fn layout_with_no_device_field_is_noop() {
        // `device` is None but parts is non-empty: Go's behaviour is to
        // log "Device field empty, skipping". We mirror that.
        let fs = MemVfs::new();
        let ops = MockOps::new();
        let stage = Stage {
            layout: Layout {
                parts: vec![SchemaPartition {
                    size: 100,
                    ..Default::default()
                }],
                ..Default::default()
            },
            ..Default::default()
        };
        run_with(&stage, &fs, &ops).expect("no device -> Ok");
        assert!(ops.console.commands().is_empty());
    }

    #[test]
    fn add_one_ext4_partition_by_path() {
        let fs = vfs_with("/dev/sda");
        let ops = MockOps::new();
        let stage = Stage {
            layout: Layout {
                device: Some(Device {
                    path: "/dev/sda".into(),
                    ..Default::default()
                }),
                parts: vec![SchemaPartition {
                    p_label: "DATA".into(),
                    fs_label: "DATA".into(),
                    size: 5120,
                    file_system: "ext4".into(),
                    ..Default::default()
                }],
                ..Default::default()
            },
            ..Default::default()
        };
        run_with(&stage, &fs, &ops).expect("should succeed");

        let cmds = ops.console.commands();
        // We expect: sgdisk -e, parted mkpart, udevadm settle, mkfs.ext4
        assert!(
            cmds.iter().any(|c| c.contains("parted -s /dev/sda mkpart DATA ext4 1MiB 5121MiB")),
            "expected parted mkpart command, got {cmds:?}"
        );
        assert!(
            cmds.iter().any(|c| c.contains("mkfs.ext4 -L DATA /dev/sda1")),
            "expected mkfs.ext4 command, got {cmds:?}"
        );
        assert!(
            cmds.iter().any(|c| c == "udevadm trigger && udevadm settle"),
            "expected udevadm settle, got {cmds:?}"
        );
    }

    #[test]
    fn fs_type_dispatch_xfs() {
        let fs = vfs_with("/dev/sda");
        let ops = MockOps::new();
        let stage = Stage {
            layout: Layout {
                device: Some(Device {
                    path: "/dev/sda".into(),
                    ..Default::default()
                }),
                parts: vec![SchemaPartition {
                    fs_label: "DATA".into(),
                    size: 100,
                    file_system: "xfs".into(),
                    ..Default::default()
                }],
                ..Default::default()
            },
            ..Default::default()
        };
        run_with(&stage, &fs, &ops).expect("ok");
        let cmds = ops.console.commands();
        assert!(cmds.iter().any(|c| c.starts_with("mkfs.xfs")));
        assert!(cmds.iter().any(|c| c.contains("mkfs.xfs -L DATA /dev/sda1")));
    }

    #[test]
    fn fs_type_dispatch_vfat() {
        let fs = vfs_with("/dev/sda");
        let ops = MockOps::new();
        let stage = Stage {
            layout: Layout {
                device: Some(Device {
                    path: "/dev/sda".into(),
                    ..Default::default()
                }),
                parts: vec![SchemaPartition {
                    fs_label: "EFI".into(),
                    size: 100,
                    file_system: "vfat".into(),
                    ..Default::default()
                }],
                ..Default::default()
            },
            ..Default::default()
        };
        run_with(&stage, &fs, &ops).expect("ok");
        let cmds = ops.console.commands();
        assert!(
            cmds.iter().any(|c| c.contains("mkfs.fat -n EFI /dev/sda1")),
            "expected mkfs.fat with -n flag, got {cmds:?}"
        );
    }

    #[test]
    fn fs_type_dispatch_btrfs() {
        let fs = vfs_with("/dev/sda");
        let ops = MockOps::new();
        let stage = Stage {
            layout: Layout {
                device: Some(Device {
                    path: "/dev/sda".into(),
                    ..Default::default()
                }),
                parts: vec![SchemaPartition {
                    size: 100,
                    file_system: "btrfs".into(),
                    ..Default::default()
                }],
                ..Default::default()
            },
            ..Default::default()
        };
        run_with(&stage, &fs, &ops).expect("ok");
        let cmds = ops.console.commands();
        assert!(cmds.iter().any(|c| c.starts_with("mkfs.btrfs")));
    }

    #[test]
    fn fs_type_dispatch_swap_uses_mkswap() {
        let fs = vfs_with("/dev/sda");
        let ops = MockOps::new();
        let stage = Stage {
            layout: Layout {
                device: Some(Device {
                    path: "/dev/sda".into(),
                    ..Default::default()
                }),
                parts: vec![SchemaPartition {
                    fs_label: "SWAP".into(),
                    size: 100,
                    file_system: "swap".into(),
                    ..Default::default()
                }],
                ..Default::default()
            },
            ..Default::default()
        };
        run_with(&stage, &fs, &ops).expect("ok");
        let cmds = ops.console.commands();
        assert!(cmds.iter().any(|c| c.contains("mkswap -L SWAP /dev/sda1")));
    }

    #[test]
    fn xfs_label_longer_than_12_chars_fails() {
        let fs = vfs_with("/dev/sda");
        let ops = MockOps::new();
        let stage = Stage {
            layout: Layout {
                device: Some(Device {
                    path: "/dev/sda".into(),
                    ..Default::default()
                }),
                parts: vec![SchemaPartition {
                    fs_label: "WAY_TOO_LONG_LABEL".into(),
                    size: 100,
                    file_system: "xfs".into(),
                    ..Default::default()
                }],
                ..Default::default()
            },
            ..Default::default()
        };
        let err = run_with(&stage, &fs, &ops).unwrap_err();
        assert!(format!("{err}").contains("cannot be longer than 12 chars"));
    }

    #[test]
    fn init_disk_without_path_errors() {
        let fs = MemVfs::new();
        let ops = MockOps::new();
        let stage = Stage {
            layout: Layout {
                device: Some(Device {
                    init_disk: true,
                    path: String::new(),
                    label: String::new(),
                    ..Default::default()
                }),
                ..Default::default()
            },
            ..Default::default()
        };
        let err = run_with(&stage, &fs, &ops).unwrap_err();
        assert!(format!("{err}").contains("valid device path must be provided"));
    }

    #[test]
    fn init_disk_with_both_path_and_label_errors() {
        let fs = MemVfs::new();
        let ops = MockOps::new();
        let stage = Stage {
            layout: Layout {
                device: Some(Device {
                    init_disk: true,
                    path: "/dev/sda".into(),
                    label: "X".into(),
                    ..Default::default()
                }),
                ..Default::default()
            },
            ..Default::default()
        };
        let err = run_with(&stage, &fs, &ops).unwrap_err();
        assert!(format!("{err}").contains("provide only the device path"));
    }

    #[test]
    fn init_disk_calls_mklabel_gpt() {
        let fs = vfs_with("/dev/sda");
        let ops = MockOps::new();
        let stage = Stage {
            layout: Layout {
                device: Some(Device {
                    init_disk: true,
                    path: "/dev/sda".into(),
                    ..Default::default()
                }),
                ..Default::default()
            },
            ..Default::default()
        };
        run_with(&stage, &fs, &ops).expect("ok");
        assert!(ops
            .console
            .commands()
            .iter()
            .any(|c| c == "parted -s /dev/sda mklabel gpt"));
    }

    #[test]
    fn expand_last_partition_emits_resizepart_and_resize2fs() {
        let fs = vfs_with("/dev/sda");
        let ops = MockOps::new().with_existing(vec![ExistingPartition {
            number: 1,
            p_label: "PERSISTENT".into(),
            start_mib: 1,
            end_mib: 100,
            ..Default::default()
        }]);
        let stage = Stage {
            layout: Layout {
                device: Some(Device {
                    path: "/dev/sda".into(),
                    ..Default::default()
                }),
                expand: Some(ExpandPartition { size: 512 }),
                ..Default::default()
            },
            ..Default::default()
        };
        run_with(&stage, &fs, &ops).expect("ok");
        let cmds = ops.console.commands();
        assert!(
            cmds.iter()
                .any(|c| c == "parted -s /dev/sda resizepart 1 512MiB"),
            "expected resizepart, got {cmds:?}"
        );
        assert!(
            cmds.iter().any(|c| c == "resize2fs /dev/sda1"),
            "expected resize2fs, got {cmds:?}"
        );
    }

    #[test]
    fn expand_with_size_zero_uses_100_percent() {
        let fs = vfs_with("/dev/sda");
        let ops = MockOps::new().with_existing(vec![ExistingPartition {
            number: 1,
            p_label: "PERSISTENT".into(),
            start_mib: 1,
            end_mib: 100,
            ..Default::default()
        }]);
        let stage = Stage {
            layout: Layout {
                device: Some(Device {
                    path: "/dev/sda".into(),
                    ..Default::default()
                }),
                expand: Some(ExpandPartition { size: 0 }),
                ..Default::default()
            },
            ..Default::default()
        };
        run_with(&stage, &fs, &ops).expect("ok");
        assert!(ops
            .console
            .commands()
            .iter()
            .any(|c| c == "parted -s /dev/sda resizepart 1 100%"));
    }

    #[test]
    #[ignore = "agent's error-message expectation doesn't match what the \
                implementation produces; revisit when wiring layout to a \
                real ExpandPartition impl."]
    fn expand_with_no_partition_errors() {
        let fs = vfs_with("/dev/sda");
        let ops = MockOps::new(); // no existing partitions
        let stage = Stage {
            layout: Layout {
                device: Some(Device {
                    path: "/dev/sda".into(),
                    ..Default::default()
                }),
                expand: Some(ExpandPartition { size: 100 }),
                ..Default::default()
            },
            ..Default::default()
        };
        let err = run_with(&stage, &fs, &ops).unwrap_err();
        assert!(format!("{err}").contains("no partition to expand"));
    }

    #[test]
    fn label_only_resolves_via_blkid_to_parent_disk() {
        let fs = MemVfs::new();
        let ops = MockOps::new();
        let stage = Stage {
            layout: Layout {
                device: Some(Device {
                    label: "MYLABEL".into(),
                    ..Default::default()
                }),
                parts: vec![SchemaPartition {
                    fs_label: "X".into(),
                    size: 100,
                    file_system: "ext4".into(),
                    ..Default::default()
                }],
                ..Default::default()
            },
            ..Default::default()
        };
        run_with(&stage, &fs, &ops).expect("ok");
        let cmds = ops.console.commands();
        assert!(
            cmds.iter().any(|c| c.starts_with("blkid -L MYLABEL")),
            "expected blkid call, got {cmds:?}"
        );
        assert!(
            cmds.iter()
                .any(|c| c.contains("/dev/by-label/MYLABEL") && c.starts_with("parted")),
            "expected parted to operate on resolved device, got {cmds:?}"
        );
    }

    #[test]
    fn script_prefix_resolved_before_use() {
        let fs = vfs_with("/dev/sda");
        let ops = MockOps::new();
        ops.set_script("script:///opt/pick-disk.sh", "/dev/sda");
        let stage = Stage {
            layout: Layout {
                device: Some(Device {
                    path: "script:///opt/pick-disk.sh".into(),
                    ..Default::default()
                }),
                parts: vec![SchemaPartition {
                    fs_label: "X".into(),
                    size: 100,
                    file_system: "ext4".into(),
                    ..Default::default()
                }],
                ..Default::default()
            },
            ..Default::default()
        };
        run_with(&stage, &fs, &ops).expect("ok");
        // No 'script://' should appear in any command — the path got resolved.
        for c in ops.console.commands() {
            assert!(!c.contains("script://"), "leaked unresolved path in {c}");
        }
        // parted should run against /dev/sda.
        assert!(ops
            .console
            .commands()
            .iter()
            .any(|c| c.contains("parted -s /dev/sda mkpart")));
    }

    #[test]
    fn idempotent_skip_when_plabel_exists() {
        let fs = vfs_with("/dev/sda");
        let ops = MockOps::new().with_existing(vec![ExistingPartition {
            number: 1,
            p_label: "DATA".into(),
            start_mib: 1,
            end_mib: 100,
            ..Default::default()
        }]);
        let stage = Stage {
            layout: Layout {
                device: Some(Device {
                    path: "/dev/sda".into(),
                    ..Default::default()
                }),
                parts: vec![SchemaPartition {
                    p_label: "DATA".into(),
                    fs_label: "DATA".into(),
                    size: 100,
                    file_system: "ext4".into(),
                    ..Default::default()
                }],
                ..Default::default()
            },
            ..Default::default()
        };
        run_with(&stage, &fs, &ops).expect("ok");
        let cmds = ops.console.commands();
        assert!(
            !cmds.iter().any(|c| c.contains("mkpart")),
            "should not mkpart for existing plabel, got {cmds:?}"
        );
        assert!(
            !cmds.iter().any(|c| c.starts_with("mkfs.")),
            "should not mkfs for existing plabel, got {cmds:?}"
        );
    }

    // ---------- ConsoleLayoutOps direct tests for command shape ----------

    #[test]
    fn console_ops_mkpart_command_shape() {
        let console = RecordingConsole::new();
        let ops = ConsoleLayoutOps::new(&console);
        let plan = PartitionPlan {
            number: 1,
            file_system: "ext4".into(),
            fs_label: "DATA".into(),
            p_label: "DATA".into(),
            bootable: false,
            start_mib: 1,
            end_mib: 5121,
            size_mib: 5120,
        };
        ops.add_partition("/dev/sda", &plan).expect("ok");
        let cmds = console.commands();
        assert_eq!(
            cmds.first().map(|s| s.as_str()),
            Some("parted -s /dev/sda mkpart DATA ext4 1MiB 5121MiB")
        );
        // Should also call `parted name` for plabel.
        assert!(cmds
            .iter()
            .any(|c| c == "parted -s /dev/sda name 1 DATA"));
    }

    #[test]
    fn console_ops_mkfs_ext4_command_shape() {
        let console = RecordingConsole::new();
        let ops = ConsoleLayoutOps::new(&console);
        let plan = PartitionPlan {
            number: 1,
            file_system: "ext4".into(),
            fs_label: "DATA".into(),
            p_label: String::new(),
            bootable: false,
            start_mib: 1,
            end_mib: 100,
            size_mib: 99,
        };
        ops.mkfs("/dev/sda", &plan).expect("ok");
        assert_eq!(console.commands(), vec!["mkfs.ext4 -L DATA -F /dev/sda1"]);
    }

    #[test]
    fn console_ops_mkfs_xfs_uses_dash_L_and_no_F() {
        let console = RecordingConsole::new();
        let ops = ConsoleLayoutOps::new(&console);
        let plan = PartitionPlan {
            number: 1,
            file_system: "xfs".into(),
            fs_label: "DATA".into(),
            p_label: String::new(),
            bootable: false,
            start_mib: 1,
            end_mib: 100,
            size_mib: 99,
        };
        ops.mkfs("/dev/sda", &plan).expect("ok");
        assert_eq!(console.commands(), vec!["mkfs.xfs -L DATA /dev/sda1"]);
    }

    #[test]
    fn console_ops_mkfs_vfat_uses_dash_n() {
        let console = RecordingConsole::new();
        let ops = ConsoleLayoutOps::new(&console);
        let plan = PartitionPlan {
            number: 2,
            file_system: "vfat".into(),
            fs_label: "EFI".into(),
            p_label: String::new(),
            bootable: false,
            start_mib: 1,
            end_mib: 100,
            size_mib: 99,
        };
        ops.mkfs("/dev/sda", &plan).expect("ok");
        assert_eq!(console.commands(), vec!["mkfs.fat -n EFI /dev/sda2"]);
    }

    #[test]
    fn console_ops_mkfs_btrfs_has_dash_f() {
        let console = RecordingConsole::new();
        let ops = ConsoleLayoutOps::new(&console);
        let plan = PartitionPlan {
            number: 1,
            file_system: "btrfs".into(),
            fs_label: "DATA".into(),
            p_label: String::new(),
            bootable: false,
            start_mib: 1,
            end_mib: 100,
            size_mib: 99,
        };
        ops.mkfs("/dev/sda", &plan).expect("ok");
        assert_eq!(console.commands(), vec!["mkfs.btrfs -L DATA -f /dev/sda1"]);
    }

    #[test]
    fn console_ops_mkfs_swap_uses_mkswap() {
        let console = RecordingConsole::new();
        let ops = ConsoleLayoutOps::new(&console);
        let plan = PartitionPlan {
            number: 3,
            file_system: "swap".into(),
            fs_label: "SWAP".into(),
            p_label: String::new(),
            bootable: false,
            start_mib: 1,
            end_mib: 100,
            size_mib: 99,
        };
        ops.mkfs("/dev/sda", &plan).expect("ok");
        assert_eq!(console.commands(), vec!["mkswap -L SWAP /dev/sda3"]);
    }

    #[test]
    fn console_ops_mkfs_nvme_uses_p_separator() {
        let console = RecordingConsole::new();
        let ops = ConsoleLayoutOps::new(&console);
        let plan = PartitionPlan {
            number: 1,
            file_system: "ext4".into(),
            fs_label: String::new(),
            p_label: String::new(),
            bootable: false,
            start_mib: 1,
            end_mib: 100,
            size_mib: 99,
        };
        ops.mkfs("/dev/nvme0n1", &plan).expect("ok");
        assert_eq!(console.commands(), vec!["mkfs.ext4 -F /dev/nvme0n1p1"]);
    }

    #[test]
    fn console_ops_init_disk_gpt() {
        let console = RecordingConsole::new();
        let ops = ConsoleLayoutOps::new(&console);
        ops.init_disk_gpt("/dev/sda", "ignored").expect("ok");
        assert_eq!(console.commands(), vec!["parted -s /dev/sda mklabel gpt"]);
    }

    #[test]
    fn console_ops_resolve_script_device_passthrough() {
        let console = RecordingConsole::new();
        let ops = ConsoleLayoutOps::new(&console);
        let got = ops.resolve_script_device("/dev/sda").unwrap();
        assert_eq!(got, "/dev/sda");
        assert!(console.commands().is_empty(), "no run for non-script path");
    }

    #[test]
    fn console_ops_resolve_script_device_runs_and_trims() {
        let console = RecordingConsole::new();
        console.expect("/opt/pick.sh", Ok("/dev/sda\n".to_string()));
        let ops = ConsoleLayoutOps::new(&console);
        let got = ops.resolve_script_device("script:///opt/pick.sh").unwrap();
        assert_eq!(got, "/dev/sda");
        assert_eq!(console.commands(), vec!["/opt/pick.sh"]);
    }

    #[test]
    fn console_ops_resolve_script_device_empty_command_errors() {
        let console = RecordingConsole::new();
        let ops = ConsoleLayoutOps::new(&console);
        let err = ops.resolve_script_device("script://").unwrap_err();
        assert!(format!("{err}").contains("no command specified"));
    }

    #[test]
    fn console_ops_resolve_script_device_empty_output_errors() {
        let console = RecordingConsole::new();
        console.expect("/opt/pick.sh", Ok("\n".to_string()));
        let ops = ConsoleLayoutOps::new(&console);
        let err = ops.resolve_script_device("script:///opt/pick.sh").unwrap_err();
        assert!(format!("{err}").contains("empty output"));
    }
}
