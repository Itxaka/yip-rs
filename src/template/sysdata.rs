//! System fact gathering for the templating layer.
//!
//! Mirrors yip's `templateSysData` in `pkg/plugins/common.go`: build a
//! `Values.System.*` JSON tree from the host so kairos configs that
//! reference `{{ .Values.System.OS.Name }}` etc. resolve at render time.
//!
//! The Go side uses `github.com/zcalusic/sysinfo` plus
//! `github.com/denisbrodbeck/machineid`. We approximate the fields that
//! the most-common kairos templates touch:
//!
//! - OS name/version/id from `/etc/os-release`
//! - kernel release from `uname -r`
//! - hostname via libc `gethostname`
//! - machine-id from `/etc/machine-id` (also tried `/var/lib/dbus/machine-id`)
//! - random uuid as `Random` (matches `utils.RandomString(32)` slot)
//! - architecture from Rust's `std::env::consts::ARCH`
//!
//! This is intentionally narrower than `zcalusic/sysinfo`; the templates
//! that need richer info (CPU brand, memory size) can be added when a
//! kairos config actually depends on them.

use std::fs;

use serde_json::{json, Value};

use crate::error::{Error, Result};

/// Parsed view of /etc/os-release. Only the keys yip configs typically
/// reference are surfaced as named fields; everything else lands in `extra`.
#[derive(Debug, Default, Clone)]
pub struct OsRelease {
    pub name: String,
    pub version: String,
    pub id: String,
    pub pretty_name: String,
    pub extra: std::collections::BTreeMap<String, String>,
}

/// Parse an `/etc/os-release`-style key/value file. Handles double-quoted,
/// single-quoted, and bare values; skips blank lines and `#` comments.
/// Returns an empty `OsRelease` if the file is missing.
pub fn parse_os_release() -> Result<OsRelease> {
    parse_os_release_from("/etc/os-release")
}

fn parse_os_release_from(path: &str) -> Result<OsRelease> {
    let body = match fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(OsRelease::default()),
        Err(e) => return Err(Error::io_at(path, e)),
    };
    Ok(parse_os_release_string(&body))
}

fn parse_os_release_string(body: &str) -> OsRelease {
    let mut out = OsRelease::default();
    for raw in body.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((k, v_raw)) = line.split_once('=') else {
            continue;
        };
        let key = k.trim().to_string();
        let val = unquote(v_raw.trim());
        match key.as_str() {
            "NAME" => out.name = val.clone(),
            "VERSION" | "VERSION_ID" if out.version.is_empty() => out.version = val.clone(),
            "ID" => out.id = val.clone(),
            "PRETTY_NAME" => out.pretty_name = val.clone(),
            _ => {}
        }
        out.extra.insert(key, val);
    }
    out
}

fn unquote(v: &str) -> String {
    let v = v.trim();
    if (v.starts_with('"') && v.ends_with('"')) || (v.starts_with('\'') && v.ends_with('\'')) {
        if v.len() >= 2 {
            return v[1..v.len() - 1].to_string();
        }
    }
    v.to_string()
}

fn read_kernel_release() -> String {
    // Linux exposes the kernel release via /proc; this matches what `uname -r`
    // prints. We avoid `nix::sys::utsname` because it lives behind the
    // `feature` nix feature which yip-rs does not enable.
    if let Ok(s) = fs::read_to_string("/proc/sys/kernel/osrelease") {
        return s.trim().to_string();
    }
    String::new()
}

fn read_hostname() -> String {
    // We use libc::gethostname directly because nix 0.30 gates its wrapper
    // behind the `hostname` feature which yip-rs does not enable. The
    // /etc/hostname / /proc/sys/kernel/hostname files are also acceptable
    // fallbacks; we try /proc first because it does not require linking.
    if let Ok(s) = fs::read_to_string("/proc/sys/kernel/hostname") {
        return s.trim().to_string();
    }
    let mut buf = [0u8; 256];
    // SAFETY: libc::gethostname writes up to `buf.len()` bytes and
    // null-terminates on success. We treat any nonzero return as failure.
    let rc = unsafe { libc::gethostname(buf.as_mut_ptr() as *mut libc::c_char, buf.len()) };
    if rc != 0 {
        return String::new();
    }
    let nul = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    String::from_utf8_lossy(&buf[..nul]).into_owned()
}

fn read_machine_id() -> String {
    for p in ["/etc/machine-id", "/var/lib/dbus/machine-id"] {
        if let Ok(s) = fs::read_to_string(p) {
            let t = s.trim();
            if !t.is_empty() {
                return t.to_string();
            }
        }
    }
    String::new()
}

/// Build the JSON object templates consume as `.Values.System.*`. Mirrors
/// the shape of `templateSysData` in yip's `pkg/plugins/common.go`.
pub fn gather_sysdata() -> Result<Value> {
    let os = parse_os_release().unwrap_or_default();
    let hostname = read_hostname();
    let kernel = read_kernel_release();
    let arch = std::env::consts::ARCH.to_string();
    let machine_id = read_machine_id();
    let random = uuid::Uuid::new_v4().to_string();

    Ok(json!({
        "Values": {
            "System": {
                "OS": {
                    "Name": os.name,
                    "Version": os.version,
                    "Id": os.id,
                    "PrettyName": os.pretty_name,
                },
                "Arch": arch,
                "Hostname": hostname,
                "Kernel": kernel,
                "MachineId": machine_id,
                "Random": random,
                // Mirror the legacy slots yip injects directly into Values:
                "Random32": random_string(32),
            },
            // Top-level `Random` / `ProtectedID` mirror yip's
            // `interpolateOpts["Random"]` placement so configs written
            // against the Go impl keep working.
            "Random": random_string(32),
            "ProtectedID": machine_id,
        }
    }))
}

fn random_string(n: usize) -> String {
    use rand::Rng;
    const CHARS: &[u8] = b"1234567890abcdefghijklmnopqrstuvwxyz";
    let mut rng = rand::thread_rng();
    (0..n)
        .map(|_| CHARS[rng.gen_range(0..CHARS.len())] as char)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_handles_quotes_and_comments() {
        let body = r#"
# a comment

NAME="Kairos"
VERSION='1.2.3'
ID=kairos
PRETTY_NAME="Kairos 1.2.3 (Foo)"
HOME_URL=https://example.com
"#;
        let r = parse_os_release_string(body);
        assert_eq!(r.name, "Kairos");
        assert_eq!(r.version, "1.2.3");
        assert_eq!(r.id, "kairos");
        assert_eq!(r.pretty_name, "Kairos 1.2.3 (Foo)");
        assert_eq!(r.extra.get("HOME_URL").map(String::as_str), Some("https://example.com"));
    }

    #[test]
    fn parse_blank_input_is_default() {
        let r = parse_os_release_string("");
        assert!(r.name.is_empty());
        assert!(r.version.is_empty());
    }

    #[test]
    fn parse_missing_file_is_default() {
        let r = parse_os_release_from("/no/such/file/yip-rs-test").unwrap();
        assert!(r.name.is_empty());
    }

    #[test]
    fn gather_sysdata_shape() {
        let v = gather_sysdata().unwrap();
        // Must have Values.System.OS.Name (may be empty on systems without
        // /etc/os-release, but the key must exist).
        assert!(v.pointer("/Values/System/OS/Name").is_some());
        assert!(v.pointer("/Values/System/Arch").is_some());
        assert!(v.pointer("/Values/System/Hostname").is_some());
        assert!(v.pointer("/Values/System/Random").is_some());
        assert!(v.pointer("/Values/Random").is_some());
        // Arch is always populated by Rust.
        let arch = v.pointer("/Values/System/Arch").and_then(Value::as_str).unwrap();
        assert!(!arch.is_empty());
    }

    #[test]
    fn gather_then_render() {
        let data = gather_sysdata().unwrap();
        let out = crate::template::render("{{ .Values.System.Arch }}", &data).unwrap();
        assert!(!out.is_empty());
        assert_eq!(out, std::env::consts::ARCH);
    }

    #[test]
    fn gather_os_name_non_empty_when_host_has_release_file() {
        // This is a soft check — only asserts non-empty if the host
        // actually has /etc/os-release. On CI containers it usually does.
        if std::path::Path::new("/etc/os-release").exists() {
            let v = gather_sysdata().unwrap();
            let name = v
                .pointer("/Values/System/OS/Name")
                .and_then(Value::as_str)
                .unwrap_or("");
            assert!(!name.is_empty(), "expected NAME from /etc/os-release");
        }
    }
}
