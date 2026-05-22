//! `datasource` plugin — port of `pkg/plugins/datasource.go`.
//!
//! Iterates over the providers listed in `stage.data_sources.providers`, asks
//! each one to fetch cloud-init style metadata + userdata, and writes the
//! result under `/run/config` (the Go [`prv.ConfigPath`] constant). The first
//! provider to return userdata wins; subsequent providers are skipped.
//!
//! Wave-4 ships TWO real providers as proof-of-concept:
//!   * `aws` — HTTP fetch from the EC2 IMDS at `169.254.169.254`. The base
//!     URL can be overridden through the `YIP_AWS_BASE_URL` env var so tests
//!     can point it at a [`mockito`] server.
//!   * `nocloud` — read from the local seed dir on the [`Vfs`]
//!     (`/var/lib/cloud/seed/nocloud/user-data` + `meta-data`). NB: the Go
//!     code spells this provider `cdrom` / `config-drive` and probes block
//!     devices; we shortcut to the seed-dir form for the initial port.
//!
//! Every other Go provider (azure / gcp / openstack / digitalocean /
//! scaleway / hetzner / packet / vultr / metaldata / vmware / cdrom /
//! config-drive / file) is stubbed: the registry returns
//! `Err(Error::Other("provider X not yet ported"))` and the plugin logs a
//! `warn!` then moves on, exactly like the Go code does for an
//! `Extract`-error. See the `TODO(provider:X)` markers inline.
//!
//! Behaviour preserved from the Go side:
//!   * Empty providers list → no-op `Ok(())`.
//!   * The provider list is de-duplicated (Go's `unique()`).
//!   * On success, the raw bytes go to `/run/config/userdata`. If the bytes
//!     parse as a yip [`Config`], they're additionally written to
//!     `/run/config/<userdata_name>` (default `userdata.yaml`).
//!   * If `data_sources.path` is non-empty, it overrides `/run/config` as the
//!     output base directory.
//!   * If no provider returns data, the plugin returns
//!     `Error::Other("no metadata/userdata found")` — matching the Go error
//!     message used by `datasource_test.go`.
//!
//! NOT YET PORTED (vs Go):
//!   * VMware multipart/mixed decoding (`DecodeMultipartVmware`). Trivial to
//!     add — handled inline once the VMware provider lands.
//!   * Post-extract hostname / authorized_keys side-effects
//!     (`processHostnameFile` / `processSSHFile`). These call into the
//!     `Hostname` / `SSH` plugins and require wiring through the executor;
//!     deferred until those plugins land in wave 4.
//!   * Shebang userdata execution via [`Console::run`]. Deferred for the
//!     same reason — the Go code just runs `sh` on the extracted file; we
//!     can add it once a provider actually returns shebang userdata in CI.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

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

/// Env var for overriding the AWS IMDS base URL (used by tests + dev).
/// When unset, defaults to the real EC2 metadata endpoint.
const AWS_BASE_URL_ENV: &str = "YIP_AWS_BASE_URL";
const AWS_DEFAULT_BASE: &str = "http://169.254.169.254/latest/";

/// Build the plugin closure for executor registration.
pub fn build() -> Plugin {
    Arc::new(run)
}

/// Top-level entry point. Mirrors Go `DataSources(...)`.
pub fn run(stage: &Stage, fs: &dyn Vfs, _console: &dyn Console) -> Result<()> {
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
        match probe_and_extract(name, fs) {
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
        && winning_provider.map(|p| p != ds.path).unwrap_or(true)
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
fn probe_and_extract(name: &str, fs: &dyn Vfs) -> Result<Option<Vec<u8>>> {
    match name {
        "aws" => aws_provider(),
        "nocloud" => nocloud_provider(fs),

        // ---- Stubs ----------------------------------------------------------
        // Each is logged as `warn!` by the caller and counted as a provider
        // miss; adding the real impl is incremental work.

        // TODO(provider:azure): port pkg/plugins/datasourceProviders/provider_azure.go.
        "azure" => Err(Error::other("provider azure not yet ported")),
        // TODO(provider:gcp): port provider_gcp.go.
        "gcp" => Err(Error::other("provider gcp not yet ported")),
        // TODO(provider:openstack): port provider_openstack.go.
        "openstack" => Err(Error::other("provider openstack not yet ported")),
        // TODO(provider:digitalocean): port provider_digitalocean.go.
        "digitalocean" => Err(Error::other("provider digitalocean not yet ported")),
        // TODO(provider:scaleway): port provider_scaleway.go.
        "scaleway" => Err(Error::other("provider scaleway not yet ported")),
        // TODO(provider:hetzner): port provider_hetzner.go.
        "hetzner" => Err(Error::other("provider hetzner not yet ported")),
        // TODO(provider:packet): port provider_packet.go.
        "packet" => Err(Error::other("provider packet not yet ported")),
        // TODO(provider:vultr): port provider_vultr.go.
        "vultr" => Err(Error::other("provider vultr not yet ported")),
        // TODO(provider:metaldata): port provider_metaldata.go.
        "metaldata" => Err(Error::other("provider metaldata not yet ported")),
        // TODO(provider:vmware): port provider_vmware.go (+ multipart decode).
        "vmware" => Err(Error::other("provider vmware not yet ported")),
        // TODO(provider:cdrom): port cdrom_shared.go + provider_cdrom.go.
        "cdrom" => Err(Error::other("provider cdrom not yet ported")),
        // TODO(provider:config-drive): port provider_openstack_config_drive.go.
        "config-drive" => Err(Error::other("provider config-drive not yet ported")),
        // TODO(provider:file): port provider_file.go (host-fs path read).
        "file" => Err(Error::other("provider file not yet ported")),

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

    /// Serialize tests that mutate `YIP_AWS_BASE_URL` so they don't race.
    static AWS_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

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

    #[test]
    fn stub_provider_errors_are_aggregated_into_no_data() {
        let fs = MemVfs::new();
        let console = RecordingConsole::new();
        // All stubs return Err(); plugin should move on and end up with
        // "no metadata/userdata found".
        let stage = stage_with(vec!["azure", "gcp", "openstack"], "", "");
        let err = run(&stage, &fs, &console).expect_err("stubs return err");
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
}
