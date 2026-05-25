//! `datasource` plugin — port of `pkg/plugins/datasource.go`.
//!
//! Iterates over the providers listed in `stage.data_sources.providers`, asks
//! each one to fetch cloud-init style metadata + userdata, and writes the
//! result under `/run/config` (the Go [`prv.ConfigPath`] constant). The first
//! provider to return userdata wins; subsequent providers are skipped.
//!
//! Providers implemented:
//!   * `aws` — HTTP fetch from EC2 IMDS at `169.254.169.254`. Override via
//!     `YIP_AWS_BASE_URL`.
//!   * `azure` — HTTP fetch from Azure IMDS, base64-decoded. Override via
//!     `YIP_AZURE_BASE_URL`.
//!   * `gcp` — HTTP fetch with `Metadata-Flavor: Google`. Override via
//!     `YIP_GCP_BASE_URL`.
//!   * `openstack`, `digitalocean`, `scaleway`, `hetzner`, `packet`, `vultr`,
//!     `metaldata` — plain HTTP fetches at vendor-specific URLs. Each has
//!     its own `YIP_<PROVIDER>_URL` env override for tests.
//!   * `vmware` — shells out to `vmtoolsd --cmd "info-get …"`; decodes
//!     `base64` and `gzip+base64` encodings.
//!   * `cdrom` — scans well-known cdrom mountpoints for `user-data`.
//!   * `config-drive` — scans well-known config-2 mountpoints for the
//!     openstack-layout `openstack/latest/user_data` file.
//!   * `file` — reads `stage.data_sources.path` (default
//!     `/etc/yip/user-data`) from the Vfs.
//!   * `nocloud` — reads `user-data` (+ optional `meta-data`) from the
//!     local nocloud seed dir on the Vfs.
//!
//! Behaviour preserved from the Go side:
//!   * Empty providers list → no-op `Ok(())`.
//!   * The provider list is de-duplicated (Go's `unique()`).
//!   * On success, the raw bytes go to `/run/config/userdata`. If the bytes
//!     parse as a yip [`Config`], they're additionally written to
//!     `/run/config/<userdata_name>` (default `userdata.yaml`).
//!   * If `data_sources.path` is non-empty, it overrides `/run/config` as the
//!     output base directory — unless the winning provider is `file` (whose
//!     `path` IS the userdata file path, not an output directory).
//!   * If no provider returns data, the plugin returns
//!     `Error::Other("no metadata/userdata found")`.
//!
//! NOT YET PORTED (vs Go):
//!   * VMware multipart/mixed decoding (`DecodeMultipartVmware`).
//!   * Post-extract hostname / authorized_keys side-effects
//!     (`processHostnameFile` / `processSSHFile`).
//!   * Shebang userdata execution via [`Console::run`].

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use base64::Engine;
use tracing::{debug, info, warn};

use crate::console::Console;
use crate::error::{Error, Result};
use crate::executor::Plugin;
use crate::schema::{Config, Stage};
use crate::vfs::Vfs;

/// Output base dir — matches Go `providers.ConfigPath`.
pub const CONFIG_PATH: &str = "/run/config";

/// Default seed dir for the `nocloud` provider. The Go side mounts a CDROM
/// and reads from its mountpoint; we shortcut to the canonical local path
/// that cloud-init itself accepts when a CDROM is not available.
const NOCLOUD_SEED_DIR: &str = "/var/lib/cloud/seed/nocloud";

// ---- HTTP provider env overrides + default URLs ----------------------------

const AWS_BASE_URL_ENV: &str = "YIP_AWS_BASE_URL";
const AWS_DEFAULT_BASE: &str = "http://169.254.169.254/latest/";

const YIP_AZURE_BASE_URL: &str = "YIP_AZURE_BASE_URL";
const AZURE_DEFAULT_URL: &str =
    "http://169.254.169.254/metadata/instance/compute/customData?api-version=2021-01-01&format=text";

const YIP_GCP_BASE_URL: &str = "YIP_GCP_BASE_URL";
const GCP_DEFAULT_URL: &str =
    "http://metadata.google.internal/computeMetadata/v1/instance/attributes/user-data";

const YIP_OPENSTACK_URL: &str = "YIP_OPENSTACK_URL";
const OPENSTACK_DEFAULT_URL: &str = "http://169.254.169.254/openstack/latest/user_data";

const YIP_DIGITALOCEAN_URL: &str = "YIP_DIGITALOCEAN_URL";
const DIGITALOCEAN_DEFAULT_URL: &str = "http://169.254.169.254/metadata/v1/user-data";

const YIP_SCALEWAY_URL: &str = "YIP_SCALEWAY_URL";
const SCALEWAY_DEFAULT_URL: &str = "http://169.254.42.42/user_data/cloud-init";

const YIP_HETZNER_URL: &str = "YIP_HETZNER_URL";
const HETZNER_DEFAULT_URL: &str = "http://169.254.169.254/hetzner/v1/userdata";

const YIP_PACKET_URL: &str = "YIP_PACKET_URL";
const PACKET_DEFAULT_URL: &str = "http://metadata.packet.net/userdata";

const YIP_VULTR_URL: &str = "YIP_VULTR_URL";
const VULTR_DEFAULT_URL: &str = "http://169.254.169.254/v1/user-data";

const YIP_METALDATA_URL: &str = "YIP_METALDATA_URL";
const METALDATA_DEFAULT_URL: &str = "http://169.254.169.254/2009-04-04/user-data";

const FILE_DEFAULT_PATH: &str = "/etc/yip/user-data";

/// Build the plugin closure for executor registration.
pub fn build() -> Plugin {
    Arc::new(run)
}

/// Top-level entry point. Mirrors Go `DataSources(...)`.
pub fn run(stage: &Stage, fs: &dyn Vfs, console: &dyn Console) -> Result<()> {
    let ds = &stage.data_sources;
    if ds.providers.is_empty() {
        debug!("datasource: empty providers list, skipping");
        return Ok(());
    }

    // Dedup while preserving order (Go's `unique()`).
    let providers = dedup(&ds.providers);
    info!(providers = ?providers, "datasource: starting probe");

    // Always ensure the output dir exists, even if we end up returning an
    // error. The Go test "Runs datasources and fails to acquire any
    // metadata" asserts this.
    fs.mkdir_all(Path::new(CONFIG_PATH))?;

    let mut userdata: Option<Vec<u8>> = None;
    let mut winning_provider: Option<&str> = None;

    for name in &providers {
        debug!(provider = %name, "datasource: probing");
        match probe_and_extract(name, stage, fs, console) {
            Ok(Some(bytes)) => {
                info!(provider = %name, bytes = bytes.len(), "datasource: got userdata");
                userdata = Some(bytes);
                winning_provider = Some(name.as_str());
                break;
            }
            Ok(None) => {
                debug!(provider = %name, "datasource: probe negative, skipping");
            }
            Err(e) => {
                // Match Go: per-provider failures are warn-logged, not fatal.
                warn!(provider = %name, error = %e, "datasource: extract failed");
            }
        }
    }

    let userdata = match userdata {
        Some(b) => b,
        None => {
            return Err(Error::other("no metadata/userdata found"));
        }
    };

    // Pick output dir. Go uses `s.DataSources.Path` IFF it's non-empty AND
    // it doesn't match the provider name (the `file` provider's path IS the
    // provider name in Go). We approximate the same guard by comparing
    // against the winning provider name.
    let base_path: PathBuf = if !ds.path.is_empty()
        && winning_provider.map(|p| p != ds.path && p != "file").unwrap_or(true)
    {
        PathBuf::from(&ds.path)
    } else {
        PathBuf::from(CONFIG_PATH)
    };
    fs.mkdir_all(&base_path)?;

    let userdata_name = if ds.userdata_name.is_empty() {
        "userdata.yaml"
    } else {
        ds.userdata_name.as_str()
    };

    process_userdata(fs, &base_path, &userdata, userdata_name)?;
    Ok(())
}

/// Process extracted userdata: always write raw to `<base>/userdata`, and if
/// it parses as a yip [`Config`] also mirror it to `<base>/<userdata_name>`.
/// Shebang execution is intentionally deferred (see module docs).
fn process_userdata(fs: &dyn Vfs, base: &Path, data: &[u8], userdata_name: &str) -> Result<()> {
    // 1. Raw dump — always happens.
    let raw_path = base.join("userdata");
    fs.write(&raw_path, data)?;
    fs.chmod(&raw_path, 0o644)?;

    // 2. If it parses as a yip Config, mirror under the canonical name.
    match Config::load(data) {
        Ok(_) => {
            let named = base.join(userdata_name);
            fs.write(&named, data)?;
            fs.chmod(&named, 0o644)?;
            debug!(path = %named.display(), "datasource: wrote yip-config userdata");
        }
        Err(_) => {
            // Could be a shebang script (#!/...) — log + leave the raw copy.
            // TODO: shell-out via Console::run once we wire that through.
            if data.starts_with(b"#!") {
                info!("datasource: userdata is a shebang script; execution not yet implemented");
            } else {
                debug!("datasource: userdata is neither yip-config nor shebang; leaving raw only");
            }
        }
    }
    Ok(())
}

/// Dispatch one provider name to its implementation. Returns
///   * `Ok(Some(bytes))` — provider probed and extracted successfully
///   * `Ok(None)` — provider did not detect (Probe == false)
///   * `Err(_)` — provider matched but extraction failed
fn probe_and_extract(
    name: &str,
    stage: &Stage,
    fs: &dyn Vfs,
    console: &dyn Console,
) -> Result<Option<Vec<u8>>> {
    match name {
        "aws" => aws_provider(),
        "nocloud" => nocloud_provider(fs),
        "azure" => azure_provider(),
        "gcp" => gcp_provider(),
        "openstack" => openstack_provider(),
        "digitalocean" => digitalocean_provider(),
        "scaleway" => scaleway_provider(),
        "hetzner" => hetzner_provider(),
        "packet" => packet_provider(),
        "vultr" => vultr_provider(),
        "metaldata" => metaldata_provider(),
        "vmware" => vmware_provider(console),
        "cdrom" => cdrom_provider(fs),
        "config-drive" => config_drive_provider(fs),
        "file" => file_provider(stage, fs),

        other => {
            warn!(provider = %other, "datasource: unknown provider, skipping");
            Ok(None)
        }
    }
}

// ---------------------------------------------------------------------------
// AWS
// ---------------------------------------------------------------------------

/// Resolve the AWS IMDS base URL. Defaults to the real endpoint; the
/// `YIP_AWS_BASE_URL` env var overrides for tests / non-prod.
fn aws_base_url() -> String {
    std::env::var(AWS_BASE_URL_ENV).unwrap_or_else(|_| AWS_DEFAULT_BASE.to_string())
}

/// AWS provider — Probe + Extract collapsed since `Probe` in Go just hits
/// `meta-data/hostname` and we want to do that anyway for the actual extract.
fn aws_provider() -> Result<Option<Vec<u8>>> {
    let base = aws_base_url();
    let client = http_client()?;

    // Probe = "can we read the hostname". Maps Go's `Probe()`.
    let hostname_url = format!("{}meta-data/hostname", trailing_slash(&base));
    let hostname = match http_get(&client, &hostname_url) {
        Ok(b) => b,
        Err(e) => {
            debug!(error = %e, "aws: probe (hostname) failed; not on AWS");
            return Ok(None);
        }
    };

    // Probe succeeded — fetch the rest. None of these are fatal; matches
    // Go where `awsMetaGet` only logs on failure.
    debug!("aws: probe succeeded, hostname={}", String::from_utf8_lossy(&hostname));

    // userdata is the only thing we actually return upward.
    let userdata_url = format!("{}user-data", trailing_slash(&base));
    match http_get(&client, &userdata_url) {
        Ok(b) => Ok(Some(b)),
        Err(e) => {
            warn!(error = %e, "aws: failed to fetch user-data");
            // Go returns (nil, nil) in this case, meaning "matched but no
            // userdata" — we model that as Ok(None) so the executor moves on.
            Ok(None)
        }
    }
}

fn trailing_slash(s: &str) -> String {
    if s.ends_with('/') {
        s.to_string()
    } else {
        format!("{s}/")
    }
}

fn http_client() -> Result<reqwest::blocking::Client> {
    reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
        .map_err(|e| Error::other(format!("aws: build http client: {e}")))
}

fn http_get(client: &reqwest::blocking::Client, url: &str) -> Result<Vec<u8>> {
    let resp = client
        .get(url)
        .send()
        .map_err(|e| Error::other(format!("aws: GET {url}: {e}")))?;
    if !resp.status().is_success() {
        return Err(Error::other(format!(
            "aws: GET {url}: status {}",
            resp.status()
        )));
    }
    let bytes = resp
        .bytes()
        .map_err(|e| Error::other(format!("aws: body {url}: {e}")))?;
    Ok(bytes.to_vec())
}

// ---------------------------------------------------------------------------
// Shared HTTP helper for "single-URL userdata" providers
// ---------------------------------------------------------------------------

/// Single-shot HTTP GET with a short timeout. Returns:
///   * `Ok(Some(bytes))` on 2xx,
///   * `Ok(None)` on 404 or unreachable host (provider not present),
///   * `Err(_)` on any other non-2xx (true extract failure).
fn simple_userdata_get(
    provider: &str,
    url: &str,
    headers: &[(&str, &str)],
) -> Result<Option<Vec<u8>>> {
    let client = match reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            warn!(provider, error = %e, "build http client");
            return Ok(None);
        }
    };
    let mut req = client.get(url);
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    let resp = match req.send() {
        Ok(r) => r,
        Err(e) => {
            debug!(provider, error = %e, "transport error, skipping");
            return Ok(None);
        }
    };
    let status = resp.status();
    if status == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }
    if !status.is_success() {
        return Err(Error::other(format!("{provider}: HTTP {status}")));
    }
    match resp.bytes() {
        Ok(b) => Ok(Some(b.to_vec())),
        Err(e) => Err(Error::other(format!("{provider}: read body: {e}"))),
    }
}

// ---------------------------------------------------------------------------
// azure
// ---------------------------------------------------------------------------

fn azure_url() -> String {
    std::env::var(YIP_AZURE_BASE_URL).unwrap_or_else(|_| AZURE_DEFAULT_URL.to_string())
}

fn azure_provider() -> Result<Option<Vec<u8>>> {
    let body = match simple_userdata_get("azure", &azure_url(), &[("Metadata", "true")])? {
        Some(b) => b,
        None => return Ok(None),
    };
    // Azure returns base64-encoded customData. Trim ASCII whitespace
    // (newlines, spaces, tabs) before decoding.
    let trimmed: Vec<u8> = body
        .iter()
        .copied()
        .filter(|b| !b.is_ascii_whitespace())
        .collect();
    if trimmed.is_empty() {
        // Empty body == "no userdata configured" on Azure.
        return Ok(None);
    }
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(&trimmed)
        .map_err(|e| Error::other(format!("azure: base64 decode: {e}")))?;
    Ok(Some(decoded))
}

// ---------------------------------------------------------------------------
// gcp
// ---------------------------------------------------------------------------

fn gcp_url() -> String {
    std::env::var(YIP_GCP_BASE_URL).unwrap_or_else(|_| GCP_DEFAULT_URL.to_string())
}

fn gcp_provider() -> Result<Option<Vec<u8>>> {
    simple_userdata_get("gcp", &gcp_url(), &[("Metadata-Flavor", "Google")])
}

// ---------------------------------------------------------------------------
// openstack (HTTP metadata service, NOT config-drive)
// ---------------------------------------------------------------------------

fn openstack_url() -> String {
    std::env::var(YIP_OPENSTACK_URL).unwrap_or_else(|_| OPENSTACK_DEFAULT_URL.to_string())
}

fn openstack_provider() -> Result<Option<Vec<u8>>> {
    simple_userdata_get("openstack", &openstack_url(), &[])
}

// ---------------------------------------------------------------------------
// digitalocean
// ---------------------------------------------------------------------------

fn digitalocean_url() -> String {
    std::env::var(YIP_DIGITALOCEAN_URL).unwrap_or_else(|_| DIGITALOCEAN_DEFAULT_URL.to_string())
}

fn digitalocean_provider() -> Result<Option<Vec<u8>>> {
    simple_userdata_get("digitalocean", &digitalocean_url(), &[])
}

// ---------------------------------------------------------------------------
// scaleway
// ---------------------------------------------------------------------------

fn scaleway_url() -> String {
    std::env::var(YIP_SCALEWAY_URL).unwrap_or_else(|_| SCALEWAY_DEFAULT_URL.to_string())
}

fn scaleway_provider() -> Result<Option<Vec<u8>>> {
    simple_userdata_get("scaleway", &scaleway_url(), &[])
}

// ---------------------------------------------------------------------------
// hetzner
// ---------------------------------------------------------------------------

fn hetzner_url() -> String {
    std::env::var(YIP_HETZNER_URL).unwrap_or_else(|_| HETZNER_DEFAULT_URL.to_string())
}

fn hetzner_provider() -> Result<Option<Vec<u8>>> {
    simple_userdata_get("hetzner", &hetzner_url(), &[])
}

// ---------------------------------------------------------------------------
// packet
// ---------------------------------------------------------------------------

fn packet_url() -> String {
    std::env::var(YIP_PACKET_URL).unwrap_or_else(|_| PACKET_DEFAULT_URL.to_string())
}

fn packet_provider() -> Result<Option<Vec<u8>>> {
    simple_userdata_get("packet", &packet_url(), &[])
}

// ---------------------------------------------------------------------------
// vultr
// ---------------------------------------------------------------------------

fn vultr_url() -> String {
    std::env::var(YIP_VULTR_URL).unwrap_or_else(|_| VULTR_DEFAULT_URL.to_string())
}

fn vultr_provider() -> Result<Option<Vec<u8>>> {
    simple_userdata_get("vultr", &vultr_url(), &[])
}

// ---------------------------------------------------------------------------
// metaldata
// ---------------------------------------------------------------------------

fn metaldata_url() -> String {
    std::env::var(YIP_METALDATA_URL).unwrap_or_else(|_| METALDATA_DEFAULT_URL.to_string())
}

fn metaldata_provider() -> Result<Option<Vec<u8>>> {
    simple_userdata_get("metaldata", &metaldata_url(), &[])
}

// ---------------------------------------------------------------------------
// vmware
// ---------------------------------------------------------------------------

fn vmware_provider(console: &dyn Console) -> Result<Option<Vec<u8>>> {
    // Probe for vmtoolsd existence first.
    if console.run("command -v vmtoolsd").is_err() {
        return Ok(None);
    }
    let raw = match vmware_get(console, "guestinfo.userdata") {
        Ok(s) if !s.is_empty() && s != "---" => s,
        _ => return Ok(None),
    };
    let encoding = console
        .run("vmtoolsd --cmd \"info-get guestinfo.userdata.encoding\"")
        .unwrap_or_default();
    let decoded = decode_vmware(raw.as_bytes(), encoding.trim())?;
    Ok(Some(decoded))
}

fn vmware_get(console: &dyn Console, key: &str) -> Result<String> {
    let out = console.run(&format!("vmtoolsd --cmd \"info-get {key}\""))?;
    Ok(out.trim_end_matches('\n').to_string())
}

fn decode_vmware(data: &[u8], encoding: &str) -> Result<Vec<u8>> {
    match encoding.trim() {
        "" => Ok(data.to_vec()),
        "base64" => base64::engine::general_purpose::STANDARD
            .decode(data)
            .map_err(|e| Error::other(format!("vmware base64: {e}"))),
        "gzip+base64" | "gz+base64" => {
            let b = base64::engine::general_purpose::STANDARD
                .decode(data)
                .map_err(|e| Error::other(format!("vmware base64: {e}")))?;
            use std::io::Read;
            let mut dec = flate2::read::GzDecoder::new(&b[..]);
            let mut out = Vec::new();
            dec.read_to_end(&mut out)
                .map_err(|e| Error::other(format!("vmware gunzip: {e}")))?;
            Ok(out)
        }
        other => Err(Error::other(format!("vmware: unknown encoding {other:?}"))),
    }
}

// ---------------------------------------------------------------------------
// cdrom / config-drive / file (Vfs-based)
// ---------------------------------------------------------------------------

fn cdrom_provider(fs: &dyn Vfs) -> Result<Option<Vec<u8>>> {
    for mp in &["/media/cdrom", "/mnt/cdrom", "/run/media/cdrom"] {
        let p = Path::new(mp).join("user-data");
        if fs.exists(&p) {
            return Ok(Some(fs.read(&p)?));
        }
    }
    Ok(None)
}

fn config_drive_provider(fs: &dyn Vfs) -> Result<Option<Vec<u8>>> {
    for mp in &["/media/config-2", "/mnt/config-2"] {
        let p = Path::new(mp).join("openstack/latest/user_data");
        if fs.exists(&p) {
            return Ok(Some(fs.read(&p)?));
        }
    }
    Ok(None)
}

fn file_provider(stage: &Stage, fs: &dyn Vfs) -> Result<Option<Vec<u8>>> {
    let p = if stage.data_sources.path.is_empty() {
        FILE_DEFAULT_PATH.to_string()
    } else {
        stage.data_sources.path.clone()
    };
    let path = Path::new(&p);
    if !fs.exists(path) {
        return Ok(None);
    }
    Ok(Some(fs.read(path)?))
}

// ---------------------------------------------------------------------------
// nocloud
// ---------------------------------------------------------------------------

/// Read `user-data` (+ optional `meta-data`) from the nocloud seed dir. The
/// Vfs is used so tests can prime MemVfs with the seed dir contents.
fn nocloud_provider(fs: &dyn Vfs) -> Result<Option<Vec<u8>>> {
    let seed = Path::new(NOCLOUD_SEED_DIR);
    let user_data = seed.join("user-data");
    if !fs.exists(&user_data) {
        debug!(path = %user_data.display(), "nocloud: seed user-data missing, skipping");
        return Ok(None);
    }

    // Best-effort copy of meta-data if present. Matches the Go CDROM
    // provider's behaviour of stashing per-provider metadata under
    // ConfigPath without bubbling failures.
    let meta_data = seed.join("meta-data");
    if fs.exists(&meta_data) {
        match fs.read(&meta_data) {
            Ok(bytes) => {
                let out = Path::new(CONFIG_PATH).join("meta-data");
                if let Err(e) = fs.write(&out, &bytes) {
                    warn!(error = %e, "nocloud: failed to mirror meta-data");
                }
            }
            Err(e) => warn!(error = %e, "nocloud: failed to read meta-data"),
        }
    }

    let userdata = fs.read(&user_data)?;
    Ok(Some(userdata))
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

/// Stable, order-preserving dedup. Matches Go `unique()` semantics.
fn dedup(input: &[String]) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::with_capacity(input.len());
    for s in input {
        if seen.insert(s.clone()) {
            out.push(s.clone());
        }
    }
    out
}

// ===========================================================================
// tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::console::RecordingConsole;
    use crate::schema::DataSource;
    use crate::vfs::MemVfs;

    /// Serialize tests that mutate provider env vars so they don't race.
    static AWS_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    static AZURE_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    static GCP_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    static OPENSTACK_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    static DIGITALOCEAN_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    static SCALEWAY_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    static HETZNER_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    static PACKET_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    static VULTR_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    static METALDATA_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn stage_with(providers: Vec<&str>, path: &str, userdata_name: &str) -> Stage {
        Stage {
            data_sources: DataSource {
                providers: providers.into_iter().map(String::from).collect(),
                path: path.to_string(),
                userdata_name: userdata_name.to_string(),
            },
            ..Default::default()
        }
    }

    // ---- empty providers list --------------------------------------------

    #[test]
    fn empty_providers_is_noop() {
        let fs = MemVfs::new();
        let console = RecordingConsole::new();
        let stage = Stage::default();
        run(&stage, &fs, &console).expect("empty -> Ok");
        // CONFIG_PATH should NOT have been created — Go also only mkdirs
        // when there is at least one provider to consider.
        assert!(!fs.exists(Path::new(CONFIG_PATH)));
    }

    // ---- nocloud -----------------------------------------------------------

    #[test]
    fn nocloud_provider_reads_seed_userdata() {
        let fs = MemVfs::new();
        let console = RecordingConsole::new();

        let user_data = b"#cloud-config\nhostname: test\n";
        fs.mkdir_all(Path::new(NOCLOUD_SEED_DIR)).unwrap();
        fs.write(
            &Path::new(NOCLOUD_SEED_DIR).join("user-data"),
            user_data,
        )
        .unwrap();

        let stage = stage_with(vec!["nocloud"], "", "");
        run(&stage, &fs, &console).expect("nocloud should succeed");

        let raw = fs
            .read(&Path::new(CONFIG_PATH).join("userdata"))
            .expect("raw userdata written");
        assert_eq!(raw, user_data);
    }

    #[test]
    fn nocloud_also_mirrors_meta_data_when_present() {
        let fs = MemVfs::new();
        let console = RecordingConsole::new();

        fs.mkdir_all(Path::new(NOCLOUD_SEED_DIR)).unwrap();
        fs.write(
            &Path::new(NOCLOUD_SEED_DIR).join("user-data"),
            b"#cloud-config\n",
        )
        .unwrap();
        fs.write(
            &Path::new(NOCLOUD_SEED_DIR).join("meta-data"),
            b"instance-id: iid-local01\n",
        )
        .unwrap();

        let stage = stage_with(vec!["nocloud"], "", "");
        run(&stage, &fs, &console).expect("ok");

        let md = fs
            .read(&Path::new(CONFIG_PATH).join("meta-data"))
            .expect("meta-data mirrored");
        assert_eq!(md, b"instance-id: iid-local01\n");
    }

    #[test]
    fn nocloud_missing_seed_dir_returns_no_metadata_error() {
        let fs = MemVfs::new();
        let console = RecordingConsole::new();
        let stage = stage_with(vec!["nocloud"], "", "");
        let err = run(&stage, &fs, &console).expect_err("no seed dir -> err");
        assert!(
            err.to_string().to_lowercase().contains("no metadata/userdata"),
            "unexpected error: {err}"
        );
        // The directory still gets created (matches Go test).
        assert!(fs.exists(Path::new(CONFIG_PATH)));
    }

    // ---- valid-yip-config copy under userdata_name -----------------------

    #[test]
    fn yip_config_userdata_is_mirrored_to_default_name() {
        let fs = MemVfs::new();
        let console = RecordingConsole::new();

        // Minimal valid yip Config — top-level `name` parses.
        let cfg = b"name: from-cloud\nstages:\n  test:\n    - name: noop\n";
        fs.mkdir_all(Path::new(NOCLOUD_SEED_DIR)).unwrap();
        fs.write(&Path::new(NOCLOUD_SEED_DIR).join("user-data"), cfg)
            .unwrap();

        let stage = stage_with(vec!["nocloud"], "", "");
        run(&stage, &fs, &console).unwrap();

        let mirrored = fs
            .read(&Path::new(CONFIG_PATH).join("userdata.yaml"))
            .expect("yip-config mirror written");
        assert_eq!(mirrored, cfg);
    }

    #[test]
    fn custom_userdata_name_is_honoured() {
        let fs = MemVfs::new();
        let console = RecordingConsole::new();

        let cfg = b"name: x\n";
        fs.mkdir_all(Path::new(NOCLOUD_SEED_DIR)).unwrap();
        fs.write(&Path::new(NOCLOUD_SEED_DIR).join("user-data"), cfg)
            .unwrap();

        let stage = stage_with(vec!["nocloud"], "", "01_userdata.yaml");
        run(&stage, &fs, &console).unwrap();

        assert!(fs.exists(&Path::new(CONFIG_PATH).join("01_userdata.yaml")));
    }

    // ---- unknown / stub providers ----------------------------------------

    #[test]
    fn unknown_provider_is_skipped_with_warn() {
        let fs = MemVfs::new();
        let console = RecordingConsole::new();
        let stage = stage_with(vec!["definitely-not-a-real-provider"], "", "");
        // No data → "no metadata/userdata found".
        let err = run(&stage, &fs, &console).expect_err("unknown -> no data");
        assert!(err.to_string().to_lowercase().contains("no metadata/userdata"));
    }

    // ---- dedup -----------------------------------------------------------

    #[test]
    fn dedup_preserves_order_and_uniqueness() {
        let got = dedup(&[
            "aws".to_string(),
            "aws".to_string(),
            "gcp".to_string(),
            "aws".to_string(),
            "nocloud".to_string(),
        ]);
        assert_eq!(got, vec!["aws", "gcp", "nocloud"]);
    }

    // ---- aws (mockito) ---------------------------------------------------

    #[test]
    fn aws_provider_fetches_userdata_from_mocked_imds() {
        let _g = AWS_ENV_LOCK.lock().unwrap();
        let mut server = mockito::Server::new();
        let base = format!("{}/latest/", server.url());

        let _m1 = server
            .mock("GET", "/latest/meta-data/hostname")
            .with_status(200)
            .with_body("ip-10-0-0-1")
            .create();
        let _m2 = server
            .mock("GET", "/latest/user-data")
            .with_status(200)
            .with_body("#cloud-config\nhostname: from-aws\n")
            .create();

        // env mutation guarded by AWS_ENV_LOCK above.
        std::env::set_var(AWS_BASE_URL_ENV, &base);

        let fs = MemVfs::new();
        let console = RecordingConsole::new();
        let stage = stage_with(vec!["aws"], "", "");
        let res = run(&stage, &fs, &console);
        std::env::remove_var(AWS_BASE_URL_ENV);

        res.expect("aws fetch should succeed");
        let raw = fs
            .read(&Path::new(CONFIG_PATH).join("userdata"))
            .expect("raw written");
        assert_eq!(raw, b"#cloud-config\nhostname: from-aws\n");
    }

    #[test]
    fn aws_probe_failure_falls_through_to_no_data() {
        let _g = AWS_ENV_LOCK.lock().unwrap();
        let mut server = mockito::Server::new();
        let base = format!("{}/latest/", server.url());

        // hostname returns 404 -> Probe fails.
        let _m1 = server
            .mock("GET", "/latest/meta-data/hostname")
            .with_status(404)
            .create();

        std::env::set_var(AWS_BASE_URL_ENV, &base);
        let fs = MemVfs::new();
        let console = RecordingConsole::new();
        let stage = stage_with(vec!["aws"], "", "");
        let res = run(&stage, &fs, &console);
        std::env::remove_var(AWS_BASE_URL_ENV);

        let err = res.expect_err("probe failed -> no data");
        assert!(err.to_string().to_lowercase().contains("no metadata/userdata"));
    }

    // ---- plugin closure --------------------------------------------------

    #[test]
    fn build_returns_callable_plugin() {
        let plugin = build();
        let fs = MemVfs::new();
        let console = RecordingConsole::new();
        // Empty providers — just verifies the closure invokes without panic.
        plugin(&Stage::default(), &fs, &console).expect("closure ok");
    }

    // --- Additional tests ported from Go behaviour expectations ---

    #[test]
    fn multiple_providers_first_success_wins() {
        // Order matters: first provider that returns data is used; the rest
        // are skipped. We exercise this by listing an http provider
        // first (no server -> Ok(None)), then nocloud (succeeds).
        let _g = HETZNER_ENV_LOCK.lock().unwrap();
        let fs = MemVfs::new();
        let console = RecordingConsole::new();

        fs.mkdir_all(Path::new(NOCLOUD_SEED_DIR)).unwrap();
        let body = b"#cloud-config\nhostname: from-nocloud\n";
        fs.write(&Path::new(NOCLOUD_SEED_DIR).join("user-data"), body)
            .unwrap();

        // Make hetzner unreachable, then nocloud wins.
        std::env::set_var(YIP_HETZNER_URL, "http://127.0.0.1:1/userdata");
        let stage = stage_with(vec!["hetzner", "nocloud"], "", "");
        let res = run(&stage, &fs, &console);
        std::env::remove_var(YIP_HETZNER_URL);
        res.expect("ok");

        let got = fs
            .read(&Path::new(CONFIG_PATH).join("userdata"))
            .expect("written");
        assert_eq!(got, body);
    }

    #[test]
    fn nocloud_meta_data_without_user_data_skips_provider() {
        // If user-data is missing but meta-data is present, nocloud reports
        // a probe miss (Ok(None)).
        let fs = MemVfs::new();
        let console = RecordingConsole::new();
        fs.mkdir_all(Path::new(NOCLOUD_SEED_DIR)).unwrap();
        fs.write(
            &Path::new(NOCLOUD_SEED_DIR).join("meta-data"),
            b"instance-id: iid-meta-only\n",
        )
        .unwrap();
        // intentionally NO user-data.

        let stage = stage_with(vec!["nocloud"], "", "");
        let err = run(&stage, &fs, &console).expect_err("no user-data -> err");
        assert!(err.to_string().to_lowercase().contains("no metadata/userdata"));
    }

    #[test]
    fn invalid_yaml_userdata_falls_back_to_raw_only() {
        let fs = MemVfs::new();
        let console = RecordingConsole::new();
        let body = b"#cloud-config\n  invalid: [unbalanced\n";
        fs.mkdir_all(Path::new(NOCLOUD_SEED_DIR)).unwrap();
        fs.write(&Path::new(NOCLOUD_SEED_DIR).join("user-data"), body)
            .unwrap();

        let stage = stage_with(vec!["nocloud"], "", "");
        run(&stage, &fs, &console).expect("ok");

        let raw = fs
            .read(&Path::new(CONFIG_PATH).join("userdata"))
            .expect("raw present");
        assert_eq!(raw, body);

        let named = Path::new(CONFIG_PATH).join("userdata.yaml");
        assert!(
            !fs.exists(&named),
            "invalid-yaml userdata must not mirror under default name",
        );
    }

    #[test]
    fn custom_userdata_name_01_userdata_yaml() {
        let fs = MemVfs::new();
        let console = RecordingConsole::new();

        let cfg = b"name: from-custom\n";
        fs.mkdir_all(Path::new(NOCLOUD_SEED_DIR)).unwrap();
        fs.write(&Path::new(NOCLOUD_SEED_DIR).join("user-data"), cfg)
            .unwrap();

        let stage = stage_with(vec!["nocloud"], "", "01_userdata.yaml");
        run(&stage, &fs, &console).unwrap();

        let named = Path::new(CONFIG_PATH).join("01_userdata.yaml");
        assert!(fs.exists(&named), "custom userdata_name should be written");
        let got = fs.read(&named).expect("custom userdata mirror");
        assert_eq!(got, cfg);

        let default = Path::new(CONFIG_PATH).join("userdata.yaml");
        assert!(
            !fs.exists(&default),
            "default-name file should not be written when custom name given",
        );
    }

    #[test]
    fn aws_userdata_endpoint_404_falls_back_to_no_data() {
        let _g = AWS_ENV_LOCK.lock().unwrap();
        let mut server = mockito::Server::new();
        let base = format!("{}/latest/", server.url());

        let _m_host = server
            .mock("GET", "/latest/meta-data/hostname")
            .with_status(200)
            .with_body("host")
            .create();
        let _m_ud = server
            .mock("GET", "/latest/user-data")
            .with_status(404)
            .create();

        std::env::set_var(AWS_BASE_URL_ENV, &base);
        let fs = MemVfs::new();
        let console = RecordingConsole::new();
        let stage = stage_with(vec!["aws"], "", "");
        let res = run(&stage, &fs, &console);
        std::env::remove_var(AWS_BASE_URL_ENV);

        let err = res.expect_err("user-data 404 -> no data");
        assert!(err.to_string().to_lowercase().contains("no metadata/userdata"));
    }

    #[test]
    fn provider_list_deduplicates_in_run_path() {
        let _g = AWS_ENV_LOCK.lock().unwrap();
        let mut server = mockito::Server::new();
        let base = format!("{}/latest/", server.url());

        let _m_host = server
            .mock("GET", "/latest/meta-data/hostname")
            .with_status(200)
            .with_body("dedup-host")
            .expect(1)
            .create();
        let _m_ud = server
            .mock("GET", "/latest/user-data")
            .with_status(200)
            .with_body("#cloud-config\nx: 1\n")
            .expect(1)
            .create();

        std::env::set_var(AWS_BASE_URL_ENV, &base);
        let fs = MemVfs::new();
        let console = RecordingConsole::new();
        let stage = stage_with(
            vec!["aws", "aws", "aws", "aws"],
            "",
            "",
        );
        let res = run(&stage, &fs, &console);
        std::env::remove_var(AWS_BASE_URL_ENV);

        res.expect("ok");
        let got = fs
            .read(&Path::new(CONFIG_PATH).join("userdata"))
            .expect("written");
        assert_eq!(got, b"#cloud-config\nx: 1\n");
    }

    // =======================================================================
    // azure
    // =======================================================================

    #[test]
    fn azure_provider_decodes_base64_body() {
        let _g = AZURE_ENV_LOCK.lock().unwrap();
        let mut server = mockito::Server::new();
        // Azure customData is base64-encoded plaintext.
        let plaintext = b"#cloud-config\nhostname: from-azure\n";
        let encoded = base64::engine::general_purpose::STANDARD.encode(plaintext);
        let url = format!("{}/metadata", server.url());

        let _m = server
            .mock("GET", "/metadata")
            .match_header("metadata", "true")
            .with_status(200)
            .with_body(&encoded)
            .create();

        std::env::set_var(YIP_AZURE_BASE_URL, &url);
        let fs = MemVfs::new();
        let console = RecordingConsole::new();
        let stage = stage_with(vec!["azure"], "", "");
        let res = run(&stage, &fs, &console);
        std::env::remove_var(YIP_AZURE_BASE_URL);
        res.expect("azure ok");

        let raw = fs
            .read(&Path::new(CONFIG_PATH).join("userdata"))
            .expect("raw written");
        assert_eq!(raw, plaintext);
    }

    #[test]
    fn azure_404_returns_no_data() {
        let _g = AZURE_ENV_LOCK.lock().unwrap();
        let mut server = mockito::Server::new();
        let url = format!("{}/metadata", server.url());
        let _m = server.mock("GET", "/metadata").with_status(404).create();

        std::env::set_var(YIP_AZURE_BASE_URL, &url);
        let fs = MemVfs::new();
        let console = RecordingConsole::new();
        let stage = stage_with(vec!["azure"], "", "");
        let res = run(&stage, &fs, &console);
        std::env::remove_var(YIP_AZURE_BASE_URL);
        let err = res.expect_err("404 -> no data");
        assert!(err.to_string().to_lowercase().contains("no metadata/userdata"));
    }

    #[test]
    fn azure_unreachable_host_returns_no_data() {
        let _g = AZURE_ENV_LOCK.lock().unwrap();
        std::env::set_var(YIP_AZURE_BASE_URL, "http://127.0.0.1:1/metadata");
        let fs = MemVfs::new();
        let console = RecordingConsole::new();
        let stage = stage_with(vec!["azure"], "", "");
        let res = run(&stage, &fs, &console);
        std::env::remove_var(YIP_AZURE_BASE_URL);
        let err = res.expect_err("unreachable -> no data");
        assert!(err.to_string().to_lowercase().contains("no metadata/userdata"));
    }

    // =======================================================================
    // gcp
    // =======================================================================

    #[test]
    fn gcp_provider_fetches_userdata() {
        let _g = GCP_ENV_LOCK.lock().unwrap();
        let mut server = mockito::Server::new();
        let url = format!("{}/user-data", server.url());
        let _m = server
            .mock("GET", "/user-data")
            .match_header("metadata-flavor", "Google")
            .with_status(200)
            .with_body("#cloud-config\nhostname: gcp\n")
            .create();

        std::env::set_var(YIP_GCP_BASE_URL, &url);
        let fs = MemVfs::new();
        let console = RecordingConsole::new();
        let stage = stage_with(vec!["gcp"], "", "");
        let res = run(&stage, &fs, &console);
        std::env::remove_var(YIP_GCP_BASE_URL);
        res.expect("gcp ok");

        let raw = fs
            .read(&Path::new(CONFIG_PATH).join("userdata"))
            .expect("raw written");
        assert_eq!(raw, b"#cloud-config\nhostname: gcp\n");
    }

    #[test]
    fn gcp_404_returns_no_data() {
        let _g = GCP_ENV_LOCK.lock().unwrap();
        let mut server = mockito::Server::new();
        let url = format!("{}/user-data", server.url());
        let _m = server.mock("GET", "/user-data").with_status(404).create();
        std::env::set_var(YIP_GCP_BASE_URL, &url);
        let fs = MemVfs::new();
        let console = RecordingConsole::new();
        let stage = stage_with(vec!["gcp"], "", "");
        let res = run(&stage, &fs, &console);
        std::env::remove_var(YIP_GCP_BASE_URL);
        assert!(res.is_err());
    }

    #[test]
    fn gcp_unreachable_returns_no_data() {
        let _g = GCP_ENV_LOCK.lock().unwrap();
        std::env::set_var(YIP_GCP_BASE_URL, "http://127.0.0.1:1/user-data");
        let fs = MemVfs::new();
        let console = RecordingConsole::new();
        let stage = stage_with(vec!["gcp"], "", "");
        let res = run(&stage, &fs, &console);
        std::env::remove_var(YIP_GCP_BASE_URL);
        assert!(res.is_err());
    }

    // =======================================================================
    // openstack
    // =======================================================================

    #[test]
    fn openstack_provider_fetches_userdata() {
        let _g = OPENSTACK_ENV_LOCK.lock().unwrap();
        let mut server = mockito::Server::new();
        let url = format!("{}/user_data", server.url());
        let _m = server
            .mock("GET", "/user_data")
            .with_status(200)
            .with_body("openstack-body")
            .create();
        std::env::set_var(YIP_OPENSTACK_URL, &url);
        let fs = MemVfs::new();
        let console = RecordingConsole::new();
        let stage = stage_with(vec!["openstack"], "", "");
        let res = run(&stage, &fs, &console);
        std::env::remove_var(YIP_OPENSTACK_URL);
        res.expect("ok");
        assert_eq!(
            fs.read(&Path::new(CONFIG_PATH).join("userdata")).unwrap(),
            b"openstack-body"
        );
    }

    #[test]
    fn openstack_404_returns_no_data() {
        let _g = OPENSTACK_ENV_LOCK.lock().unwrap();
        let mut server = mockito::Server::new();
        let url = format!("{}/user_data", server.url());
        let _m = server.mock("GET", "/user_data").with_status(404).create();
        std::env::set_var(YIP_OPENSTACK_URL, &url);
        let fs = MemVfs::new();
        let console = RecordingConsole::new();
        let stage = stage_with(vec!["openstack"], "", "");
        let res = run(&stage, &fs, &console);
        std::env::remove_var(YIP_OPENSTACK_URL);
        assert!(res.is_err());
    }

    #[test]
    fn openstack_unreachable_returns_no_data() {
        let _g = OPENSTACK_ENV_LOCK.lock().unwrap();
        std::env::set_var(YIP_OPENSTACK_URL, "http://127.0.0.1:1/user_data");
        let fs = MemVfs::new();
        let console = RecordingConsole::new();
        let stage = stage_with(vec!["openstack"], "", "");
        let res = run(&stage, &fs, &console);
        std::env::remove_var(YIP_OPENSTACK_URL);
        assert!(res.is_err());
    }

    // =======================================================================
    // digitalocean
    // =======================================================================

    #[test]
    fn digitalocean_provider_fetches_userdata() {
        let _g = DIGITALOCEAN_ENV_LOCK.lock().unwrap();
        let mut server = mockito::Server::new();
        let url = format!("{}/user-data", server.url());
        let _m = server
            .mock("GET", "/user-data")
            .with_status(200)
            .with_body("do-body")
            .create();
        std::env::set_var(YIP_DIGITALOCEAN_URL, &url);
        let fs = MemVfs::new();
        let console = RecordingConsole::new();
        let stage = stage_with(vec!["digitalocean"], "", "");
        let res = run(&stage, &fs, &console);
        std::env::remove_var(YIP_DIGITALOCEAN_URL);
        res.expect("ok");
        assert_eq!(
            fs.read(&Path::new(CONFIG_PATH).join("userdata")).unwrap(),
            b"do-body"
        );
    }

    #[test]
    fn digitalocean_404_returns_no_data() {
        let _g = DIGITALOCEAN_ENV_LOCK.lock().unwrap();
        let mut server = mockito::Server::new();
        let url = format!("{}/user-data", server.url());
        let _m = server.mock("GET", "/user-data").with_status(404).create();
        std::env::set_var(YIP_DIGITALOCEAN_URL, &url);
        let fs = MemVfs::new();
        let console = RecordingConsole::new();
        let stage = stage_with(vec!["digitalocean"], "", "");
        let res = run(&stage, &fs, &console);
        std::env::remove_var(YIP_DIGITALOCEAN_URL);
        assert!(res.is_err());
    }

    #[test]
    fn digitalocean_unreachable_returns_no_data() {
        let _g = DIGITALOCEAN_ENV_LOCK.lock().unwrap();
        std::env::set_var(YIP_DIGITALOCEAN_URL, "http://127.0.0.1:1/user-data");
        let fs = MemVfs::new();
        let console = RecordingConsole::new();
        let stage = stage_with(vec!["digitalocean"], "", "");
        let res = run(&stage, &fs, &console);
        std::env::remove_var(YIP_DIGITALOCEAN_URL);
        assert!(res.is_err());
    }

    // =======================================================================
    // scaleway
    // =======================================================================

    #[test]
    fn scaleway_provider_fetches_userdata() {
        let _g = SCALEWAY_ENV_LOCK.lock().unwrap();
        let mut server = mockito::Server::new();
        let url = format!("{}/cloud-init", server.url());
        let _m = server
            .mock("GET", "/cloud-init")
            .with_status(200)
            .with_body("scale-body")
            .create();
        std::env::set_var(YIP_SCALEWAY_URL, &url);
        let fs = MemVfs::new();
        let console = RecordingConsole::new();
        let stage = stage_with(vec!["scaleway"], "", "");
        let res = run(&stage, &fs, &console);
        std::env::remove_var(YIP_SCALEWAY_URL);
        res.expect("ok");
        assert_eq!(
            fs.read(&Path::new(CONFIG_PATH).join("userdata")).unwrap(),
            b"scale-body"
        );
    }

    #[test]
    fn scaleway_404_returns_no_data() {
        let _g = SCALEWAY_ENV_LOCK.lock().unwrap();
        let mut server = mockito::Server::new();
        let url = format!("{}/cloud-init", server.url());
        let _m = server.mock("GET", "/cloud-init").with_status(404).create();
        std::env::set_var(YIP_SCALEWAY_URL, &url);
        let fs = MemVfs::new();
        let console = RecordingConsole::new();
        let stage = stage_with(vec!["scaleway"], "", "");
        let res = run(&stage, &fs, &console);
        std::env::remove_var(YIP_SCALEWAY_URL);
        assert!(res.is_err());
    }

    #[test]
    fn scaleway_unreachable_returns_no_data() {
        let _g = SCALEWAY_ENV_LOCK.lock().unwrap();
        std::env::set_var(YIP_SCALEWAY_URL, "http://127.0.0.1:1/cloud-init");
        let fs = MemVfs::new();
        let console = RecordingConsole::new();
        let stage = stage_with(vec!["scaleway"], "", "");
        let res = run(&stage, &fs, &console);
        std::env::remove_var(YIP_SCALEWAY_URL);
        assert!(res.is_err());
    }

    // =======================================================================
    // hetzner
    // =======================================================================

    #[test]
    fn hetzner_provider_fetches_userdata() {
        let _g = HETZNER_ENV_LOCK.lock().unwrap();
        let mut server = mockito::Server::new();
        let url = format!("{}/userdata", server.url());
        let _m = server
            .mock("GET", "/userdata")
            .with_status(200)
            .with_body("hetzner-body")
            .create();
        std::env::set_var(YIP_HETZNER_URL, &url);
        let fs = MemVfs::new();
        let console = RecordingConsole::new();
        let stage = stage_with(vec!["hetzner"], "", "");
        let res = run(&stage, &fs, &console);
        std::env::remove_var(YIP_HETZNER_URL);
        res.expect("ok");
        assert_eq!(
            fs.read(&Path::new(CONFIG_PATH).join("userdata")).unwrap(),
            b"hetzner-body"
        );
    }

    #[test]
    fn hetzner_404_returns_no_data() {
        let _g = HETZNER_ENV_LOCK.lock().unwrap();
        let mut server = mockito::Server::new();
        let url = format!("{}/userdata", server.url());
        let _m = server.mock("GET", "/userdata").with_status(404).create();
        std::env::set_var(YIP_HETZNER_URL, &url);
        let fs = MemVfs::new();
        let console = RecordingConsole::new();
        let stage = stage_with(vec!["hetzner"], "", "");
        let res = run(&stage, &fs, &console);
        std::env::remove_var(YIP_HETZNER_URL);
        assert!(res.is_err());
    }

    #[test]
    fn hetzner_unreachable_returns_no_data() {
        let _g = HETZNER_ENV_LOCK.lock().unwrap();
        std::env::set_var(YIP_HETZNER_URL, "http://127.0.0.1:1/userdata");
        let fs = MemVfs::new();
        let console = RecordingConsole::new();
        let stage = stage_with(vec!["hetzner"], "", "");
        let res = run(&stage, &fs, &console);
        std::env::remove_var(YIP_HETZNER_URL);
        assert!(res.is_err());
    }

    // =======================================================================
    // packet
    // =======================================================================

    #[test]
    fn packet_provider_fetches_userdata() {
        let _g = PACKET_ENV_LOCK.lock().unwrap();
        let mut server = mockito::Server::new();
        let url = format!("{}/userdata", server.url());
        let _m = server
            .mock("GET", "/userdata")
            .with_status(200)
            .with_body("packet-body")
            .create();
        std::env::set_var(YIP_PACKET_URL, &url);
        let fs = MemVfs::new();
        let console = RecordingConsole::new();
        let stage = stage_with(vec!["packet"], "", "");
        let res = run(&stage, &fs, &console);
        std::env::remove_var(YIP_PACKET_URL);
        res.expect("ok");
        assert_eq!(
            fs.read(&Path::new(CONFIG_PATH).join("userdata")).unwrap(),
            b"packet-body"
        );
    }

    #[test]
    fn packet_404_returns_no_data() {
        let _g = PACKET_ENV_LOCK.lock().unwrap();
        let mut server = mockito::Server::new();
        let url = format!("{}/userdata", server.url());
        let _m = server.mock("GET", "/userdata").with_status(404).create();
        std::env::set_var(YIP_PACKET_URL, &url);
        let fs = MemVfs::new();
        let console = RecordingConsole::new();
        let stage = stage_with(vec!["packet"], "", "");
        let res = run(&stage, &fs, &console);
        std::env::remove_var(YIP_PACKET_URL);
        assert!(res.is_err());
    }

    #[test]
    fn packet_unreachable_returns_no_data() {
        let _g = PACKET_ENV_LOCK.lock().unwrap();
        std::env::set_var(YIP_PACKET_URL, "http://127.0.0.1:1/userdata");
        let fs = MemVfs::new();
        let console = RecordingConsole::new();
        let stage = stage_with(vec!["packet"], "", "");
        let res = run(&stage, &fs, &console);
        std::env::remove_var(YIP_PACKET_URL);
        assert!(res.is_err());
    }

    // =======================================================================
    // vultr
    // =======================================================================

    #[test]
    fn vultr_provider_fetches_userdata() {
        let _g = VULTR_ENV_LOCK.lock().unwrap();
        let mut server = mockito::Server::new();
        let url = format!("{}/user-data", server.url());
        let _m = server
            .mock("GET", "/user-data")
            .with_status(200)
            .with_body("vultr-body")
            .create();
        std::env::set_var(YIP_VULTR_URL, &url);
        let fs = MemVfs::new();
        let console = RecordingConsole::new();
        let stage = stage_with(vec!["vultr"], "", "");
        let res = run(&stage, &fs, &console);
        std::env::remove_var(YIP_VULTR_URL);
        res.expect("ok");
        assert_eq!(
            fs.read(&Path::new(CONFIG_PATH).join("userdata")).unwrap(),
            b"vultr-body"
        );
    }

    #[test]
    fn vultr_404_returns_no_data() {
        let _g = VULTR_ENV_LOCK.lock().unwrap();
        let mut server = mockito::Server::new();
        let url = format!("{}/user-data", server.url());
        let _m = server.mock("GET", "/user-data").with_status(404).create();
        std::env::set_var(YIP_VULTR_URL, &url);
        let fs = MemVfs::new();
        let console = RecordingConsole::new();
        let stage = stage_with(vec!["vultr"], "", "");
        let res = run(&stage, &fs, &console);
        std::env::remove_var(YIP_VULTR_URL);
        assert!(res.is_err());
    }

    #[test]
    fn vultr_unreachable_returns_no_data() {
        let _g = VULTR_ENV_LOCK.lock().unwrap();
        std::env::set_var(YIP_VULTR_URL, "http://127.0.0.1:1/user-data");
        let fs = MemVfs::new();
        let console = RecordingConsole::new();
        let stage = stage_with(vec!["vultr"], "", "");
        let res = run(&stage, &fs, &console);
        std::env::remove_var(YIP_VULTR_URL);
        assert!(res.is_err());
    }

    // =======================================================================
    // metaldata
    // =======================================================================

    #[test]
    fn metaldata_provider_fetches_userdata() {
        let _g = METALDATA_ENV_LOCK.lock().unwrap();
        let mut server = mockito::Server::new();
        let url = format!("{}/user-data", server.url());
        let _m = server
            .mock("GET", "/user-data")
            .with_status(200)
            .with_body("metaldata-body")
            .create();
        std::env::set_var(YIP_METALDATA_URL, &url);
        let fs = MemVfs::new();
        let console = RecordingConsole::new();
        let stage = stage_with(vec!["metaldata"], "", "");
        let res = run(&stage, &fs, &console);
        std::env::remove_var(YIP_METALDATA_URL);
        res.expect("ok");
        assert_eq!(
            fs.read(&Path::new(CONFIG_PATH).join("userdata")).unwrap(),
            b"metaldata-body"
        );
    }

    #[test]
    fn metaldata_404_returns_no_data() {
        let _g = METALDATA_ENV_LOCK.lock().unwrap();
        let mut server = mockito::Server::new();
        let url = format!("{}/user-data", server.url());
        let _m = server.mock("GET", "/user-data").with_status(404).create();
        std::env::set_var(YIP_METALDATA_URL, &url);
        let fs = MemVfs::new();
        let console = RecordingConsole::new();
        let stage = stage_with(vec!["metaldata"], "", "");
        let res = run(&stage, &fs, &console);
        std::env::remove_var(YIP_METALDATA_URL);
        assert!(res.is_err());
    }

    #[test]
    fn metaldata_unreachable_returns_no_data() {
        let _g = METALDATA_ENV_LOCK.lock().unwrap();
        std::env::set_var(YIP_METALDATA_URL, "http://127.0.0.1:1/user-data");
        let fs = MemVfs::new();
        let console = RecordingConsole::new();
        let stage = stage_with(vec!["metaldata"], "", "");
        let res = run(&stage, &fs, &console);
        std::env::remove_var(YIP_METALDATA_URL);
        assert!(res.is_err());
    }

    // =======================================================================
    // vmware
    // =======================================================================

    #[test]
    fn vmware_returns_plaintext_when_no_encoding() {
        let fs = MemVfs::new();
        let console = RecordingConsole::new();
        console.expect("command -v vmtoolsd", Ok("/usr/bin/vmtoolsd".into()));
        console.expect(
            "vmtoolsd --cmd \"info-get guestinfo.userdata\"",
            Ok("#cloud-config\nhostname: vmware\n".into()),
        );
        // Empty encoding response (the default) -> plaintext.
        let stage = stage_with(vec!["vmware"], "", "");
        run(&stage, &fs, &console).expect("ok");
        let raw = fs
            .read(&Path::new(CONFIG_PATH).join("userdata"))
            .expect("written");
        // vmware_get trims a trailing newline (matches Go strip behaviour).
        assert_eq!(raw, b"#cloud-config\nhostname: vmware");
    }

    #[test]
    fn vmware_decodes_base64_encoding() {
        let fs = MemVfs::new();
        let console = RecordingConsole::new();
        let plaintext = b"hello-vmware-base64";
        let encoded = base64::engine::general_purpose::STANDARD.encode(plaintext);
        console.expect("command -v vmtoolsd", Ok("/usr/bin/vmtoolsd".into()));
        console.expect(
            "vmtoolsd --cmd \"info-get guestinfo.userdata\"",
            Ok(encoded),
        );
        console.expect(
            "vmtoolsd --cmd \"info-get guestinfo.userdata.encoding\"",
            Ok("base64".into()),
        );
        let stage = stage_with(vec!["vmware"], "", "");
        run(&stage, &fs, &console).expect("ok");
        let raw = fs
            .read(&Path::new(CONFIG_PATH).join("userdata"))
            .expect("written");
        assert_eq!(raw, plaintext);
    }

    #[test]
    fn vmware_decodes_gzip_base64_encoding() {
        use std::io::Write;
        let fs = MemVfs::new();
        let console = RecordingConsole::new();
        let plaintext = b"hello-vmware-gzip-base64";
        // gzip-compress
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        enc.write_all(plaintext).unwrap();
        let gz = enc.finish().unwrap();
        let encoded = base64::engine::general_purpose::STANDARD.encode(&gz);

        console.expect("command -v vmtoolsd", Ok("/usr/bin/vmtoolsd".into()));
        console.expect(
            "vmtoolsd --cmd \"info-get guestinfo.userdata\"",
            Ok(encoded),
        );
        console.expect(
            "vmtoolsd --cmd \"info-get guestinfo.userdata.encoding\"",
            Ok("gzip+base64".into()),
        );
        let stage = stage_with(vec!["vmware"], "", "");
        run(&stage, &fs, &console).expect("ok");
        let raw = fs
            .read(&Path::new(CONFIG_PATH).join("userdata"))
            .expect("written");
        assert_eq!(raw, plaintext);
    }

    #[test]
    fn vmware_missing_vmtoolsd_returns_no_data() {
        let fs = MemVfs::new();
        let console = RecordingConsole::new();
        // `command -v vmtoolsd` errors -> probe fails.
        console.expect("command -v vmtoolsd", Err("not found".into()));
        let stage = stage_with(vec!["vmware"], "", "");
        let err = run(&stage, &fs, &console).expect_err("no vmtoolsd -> no data");
        assert!(err.to_string().to_lowercase().contains("no metadata/userdata"));
    }

    // =======================================================================
    // cdrom
    // =======================================================================

    #[test]
    fn cdrom_finds_user_data_in_media_cdrom() {
        let fs = MemVfs::new();
        let console = RecordingConsole::new();
        fs.mkdir_all(Path::new("/media/cdrom")).unwrap();
        fs.write(Path::new("/media/cdrom/user-data"), b"cdrom-body")
            .unwrap();
        let stage = stage_with(vec!["cdrom"], "", "");
        run(&stage, &fs, &console).expect("ok");
        assert_eq!(
            fs.read(&Path::new(CONFIG_PATH).join("userdata")).unwrap(),
            b"cdrom-body"
        );
    }

    #[test]
    fn cdrom_falls_back_to_run_media_cdrom() {
        let fs = MemVfs::new();
        let console = RecordingConsole::new();
        fs.mkdir_all(Path::new("/run/media/cdrom")).unwrap();
        fs.write(Path::new("/run/media/cdrom/user-data"), b"runmedia-body")
            .unwrap();
        let stage = stage_with(vec!["cdrom"], "", "");
        run(&stage, &fs, &console).expect("ok");
        assert_eq!(
            fs.read(&Path::new(CONFIG_PATH).join("userdata")).unwrap(),
            b"runmedia-body"
        );
    }

    #[test]
    fn cdrom_missing_everywhere_returns_no_data() {
        let fs = MemVfs::new();
        let console = RecordingConsole::new();
        let stage = stage_with(vec!["cdrom"], "", "");
        let err = run(&stage, &fs, &console).expect_err("no mountpoints -> err");
        assert!(err.to_string().to_lowercase().contains("no metadata/userdata"));
    }

    // =======================================================================
    // config-drive
    // =======================================================================

    #[test]
    fn config_drive_finds_user_data_in_media_config_2() {
        let fs = MemVfs::new();
        let console = RecordingConsole::new();
        fs.mkdir_all(Path::new("/media/config-2/openstack/latest"))
            .unwrap();
        fs.write(
            Path::new("/media/config-2/openstack/latest/user_data"),
            b"cd-body",
        )
        .unwrap();
        let stage = stage_with(vec!["config-drive"], "", "");
        run(&stage, &fs, &console).expect("ok");
        assert_eq!(
            fs.read(&Path::new(CONFIG_PATH).join("userdata")).unwrap(),
            b"cd-body"
        );
    }

    #[test]
    fn config_drive_falls_back_to_mnt_config_2() {
        let fs = MemVfs::new();
        let console = RecordingConsole::new();
        fs.mkdir_all(Path::new("/mnt/config-2/openstack/latest"))
            .unwrap();
        fs.write(
            Path::new("/mnt/config-2/openstack/latest/user_data"),
            b"mnt-body",
        )
        .unwrap();
        let stage = stage_with(vec!["config-drive"], "", "");
        run(&stage, &fs, &console).expect("ok");
        assert_eq!(
            fs.read(&Path::new(CONFIG_PATH).join("userdata")).unwrap(),
            b"mnt-body"
        );
    }

    #[test]
    fn config_drive_missing_everywhere_returns_no_data() {
        let fs = MemVfs::new();
        let console = RecordingConsole::new();
        let stage = stage_with(vec!["config-drive"], "", "");
        let err = run(&stage, &fs, &console).expect_err("no config-drive -> err");
        assert!(err.to_string().to_lowercase().contains("no metadata/userdata"));
    }

    // =======================================================================
    // file
    // =======================================================================

    #[test]
    fn file_provider_reads_default_path() {
        let fs = MemVfs::new();
        let console = RecordingConsole::new();
        fs.mkdir_all(Path::new("/etc/yip")).unwrap();
        fs.write(Path::new("/etc/yip/user-data"), b"file-default-body")
            .unwrap();
        // path empty -> default /etc/yip/user-data is used. When the winning
        // provider is `file`, the output base stays at CONFIG_PATH (we
        // explicitly avoid using ds.path as output dir for `file`).
        let stage = stage_with(vec!["file"], "", "");
        run(&stage, &fs, &console).expect("ok");
        assert_eq!(
            fs.read(&Path::new(CONFIG_PATH).join("userdata")).unwrap(),
            b"file-default-body"
        );
    }

    #[test]
    fn file_provider_reads_overridden_path() {
        let fs = MemVfs::new();
        let console = RecordingConsole::new();
        fs.mkdir_all(Path::new("/srv/custom")).unwrap();
        fs.write(Path::new("/srv/custom/userdata.txt"), b"file-custom-body")
            .unwrap();
        // path is the userdata file when the `file` provider wins — output
        // base stays at CONFIG_PATH.
        let stage = stage_with(vec!["file"], "/srv/custom/userdata.txt", "");
        run(&stage, &fs, &console).expect("ok");
        assert_eq!(
            fs.read(&Path::new(CONFIG_PATH).join("userdata")).unwrap(),
            b"file-custom-body"
        );
    }

    #[test]
    fn file_provider_missing_path_returns_no_data() {
        let fs = MemVfs::new();
        let console = RecordingConsole::new();
        // No file at the default location, no override -> Ok(None) -> "no
        // metadata/userdata found".
        let stage = stage_with(vec!["file"], "", "");
        let err = run(&stage, &fs, &console).expect_err("no file -> err");
        assert!(err.to_string().to_lowercase().contains("no metadata/userdata"));
    }

    // =======================================================================
    // decode_vmware unit tests
    // =======================================================================

    #[test]
    fn decode_vmware_empty_encoding_is_passthrough() {
        let out = decode_vmware(b"hello", "").unwrap();
        assert_eq!(out, b"hello");
    }

    #[test]
    fn decode_vmware_unknown_encoding_errors() {
        let err = decode_vmware(b"hello", "rot13").unwrap_err();
        assert!(err.to_string().contains("unknown encoding"));
    }
}
