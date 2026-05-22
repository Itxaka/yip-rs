//! `unpack_image` plugin — pull an OCI image and extract its rootfs into a
//! target directory. Port of `pkg/plugins/unpack_image.go::UnpackImage`.
//!
//! ## Implementation strategy
//!
//! Two backends sit behind the same in-process contract:
//!
//! ```text
//! fn fetch_image_layers(image: &str, platform: &str) -> Result<Vec<Vec<u8>>>;
//! ```
//!
//! Both return layer blobs (gzipped-or-plain-tar bytes) in apply order. The
//! caller then feeds each into [`extract_layer`], which is backend-agnostic
//! and handles whiteouts + tar extraction.
//!
//! * **`backend_oci`** (default, `oci-builtin` feature) — uses the
//!   `oci-distribution` crate. Async-only, so we spin a per-call
//!   `tokio::runtime::Runtime` to bridge into our otherwise-sync world.
//!   Pure-Rust, talks straight to the registry over rustls — no skopeo
//!   needed at runtime.
//! * **`backend_skopeo`** (fallback, when `oci-builtin` is off or `nounpack`
//!   is on) — shells out to `skopeo copy docker://<image> dir:<tmpdir>`,
//!   then reads `manifest.json` and the on-disk blobs. This keeps a working
//!   path for size-constrained / FIPS-y builds that don't want the
//!   `oci-distribution`+`tokio` dependency footprint.
//!
//! In both cases the layer-extraction half (`extract_layer`, `sanitize_rel`,
//! whiteout vocabulary including opaque-dir `.wh..wh..opq`) is identical and
//! tested once.
//!
//! ## Differences from Go
//!
//! - Go's `containerd/archive.Apply` handles a richer whiteout vocabulary
//!   (e.g. `.wh..wh..opq` opaque dirs). We support that too.
//! - Go falls back to the local Docker daemon if a registry pull fails.
//!   We don't — `oci-distribution` only talks to registries, and skopeo's
//!   `containers-storage:` / `docker-daemon:` transports are a config
//!   concern we leave to the operator. v1 only does `docker://`.
//! - The `platform` field is honoured by the skopeo backend via
//!   `--override-os` / `--override-arch`. The native backend punts on
//!   per-platform manifest selection in v1 — see the TODO in
//!   `backend_oci::fetch_image_layers`.
//! - All per-image errors are aggregated into [`Error::Multi`], mirroring
//!   Go's `multierror.Append` loop.

use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use tracing::{debug, info, warn};

use crate::console::Console;
use crate::error::{Error, Result};
use crate::executor::Plugin;
use crate::schema::unpack::UnpackImageConf;
use crate::schema::Stage;
use crate::vfs::Vfs;

// ---------------------------------------------------------------------------
// Backend dispatch.
//
// Exactly one of these is compiled in. Both expose:
//     fn fetch_image_layers(image: &str, platform: &str) -> Result<Vec<Vec<u8>>>
//
// `nounpack` short-circuits the whole plugin to an error regardless, but we
// still pick a backend so the module compiles cleanly under any flag combo.
// ---------------------------------------------------------------------------

// Backends are defined inline further down (look for `mod backend_oci { ... }`
// and `mod backend_skopeo { ... }`). We just set up the dispatch aliases here.

#[cfg(all(feature = "oci-builtin", not(feature = "nounpack")))]
use backend_oci as backend;
#[cfg(any(not(feature = "oci-builtin"), feature = "nounpack"))]
use backend_skopeo as backend;

/// Build a [`Plugin`] arc-closure.
pub fn build() -> Plugin {
    Arc::new(run)
}

/// Pure entry point — exposed so tests don't have to go through `Arc`.
pub fn run(stage: &Stage, fs: &dyn Vfs, console: &dyn Console) -> Result<()> {
    if stage.unpack_images.is_empty() {
        return Ok(());
    }

    info!(count = stage.unpack_images.len(), "unpacking images");

    let mut errs: Vec<Error> = Vec::new();
    for conf in &stage.unpack_images {
        if let Err(e) = unpack_one(conf, fs, console) {
            warn!(
                source = %conf.source,
                target = %conf.target,
                error = %e,
                "unpack_image failed",
            );
            errs.push(e);
        }
    }

    match errs.len() {
        0 => Ok(()),
        _ => Err(Error::Multi(errs)),
    }
}

#[cfg(feature = "nounpack")]
fn unpack_one(_conf: &UnpackImageConf, _fs: &dyn Vfs, _console: &dyn Console) -> Result<()> {
    Err(Error::Other(
        "unpack_image disabled at build time".to_string(),
    ))
}

#[cfg(not(feature = "nounpack"))]
fn unpack_one(conf: &UnpackImageConf, fs: &dyn Vfs, console: &dyn Console) -> Result<()> {
    if conf.source.is_empty() {
        warn!("no source defined for unpack_image");
        return Ok(());
    }
    if conf.target.is_empty() {
        warn!("no target defined for unpack_image");
        return Ok(());
    }

    // 1. Make sure the target dir exists.
    let target = Path::new(&conf.target);
    fs.mkdir_all(target)?;

    // 2. Ask the active backend for the layer blobs, in order.
    let layers = backend::fetch_image_layers(&conf.source, &conf.platform, console)?;

    // 3. Extract each layer into the target. We pass an empty media_type so
    //    `extract_layer` falls back to gzip magic-byte sniffing — that works
    //    for both backends without us having to plumb the type through the
    //    fetch_image_layers contract.
    for (idx, data) in layers.iter().enumerate() {
        debug!(layer = idx, size = data.len(), "extracting layer");
        extract_layer(data, "", target, fs)?;
    }

    Ok(())
}

/// Decide whether the layer is gzipped from its `mediaType` (falling back to
/// the magic bytes if the media type is missing or unknown).
fn is_gzip_layer(media_type: &str, data: &[u8]) -> bool {
    let mt = media_type.to_ascii_lowercase();
    if mt.contains("gzip") || mt.ends_with("+gzip") {
        return true;
    }
    if mt.ends_with("tar") {
        return false;
    }
    // Unknown / empty media type — sniff magic.
    data.len() >= 2 && data[0] == 0x1f && data[1] == 0x8b
}

/// Extract a layer's tar.gz (or plain tar) blob into `target`, honouring
/// OCI whiteouts.
pub(crate) fn extract_layer(
    data: &[u8],
    media_type: &str,
    target: &Path,
    fs: &dyn Vfs,
) -> Result<()> {
    let raw: Vec<u8> = if is_gzip_layer(media_type, data) {
        use std::io::Read;
        let mut dec = flate2::read::GzDecoder::new(data);
        let mut out = Vec::new();
        dec.read_to_end(&mut out)
            .map_err(|e| Error::other(format!("gunzip layer: {e}")))?;
        out
    } else {
        data.to_vec()
    };

    let mut ar = tar::Archive::new(std::io::Cursor::new(raw));
    ar.set_preserve_permissions(true);
    ar.set_preserve_mtime(false);
    ar.set_unpack_xattrs(false);

    let entries = ar
        .entries()
        .map_err(|e| Error::other(format!("read tar entries: {e}")))?;

    for entry in entries {
        let mut entry = entry.map_err(|e| Error::other(format!("read tar entry: {e}")))?;
        let path = entry
            .path()
            .map_err(|e| Error::other(format!("decode tar entry path: {e}")))?
            .into_owned();

        // Sanitise: strip leading "/" and reject ".." traversal. Standard
        // OCI layers are always relative, but we don't want a malicious
        // image escaping `target`.
        let rel = sanitize_rel(&path)?;
        if rel.as_os_str().is_empty() {
            continue;
        }

        // Whiteout handling. OCI uses `.wh.<basename>` to remove a path
        // from a lower layer, and `.wh..wh..opq` inside a directory to
        // clear the directory's contents.
        if let Some(fname) = rel.file_name().and_then(|s| s.to_str()) {
            if fname == ".wh..wh..opq" {
                if let Some(parent) = rel.parent() {
                    let p = target.join(parent);
                    debug!(path = %p.display(), "opaque whiteout");
                    let _ = fs.remove_all(&p);
                    let _ = fs.mkdir_all(&p);
                }
                continue;
            }
            if let Some(real) = fname.strip_prefix(".wh.") {
                let mut victim = PathBuf::from(target);
                if let Some(parent) = rel.parent() {
                    victim.push(parent);
                }
                victim.push(real);
                debug!(path = %victim.display(), "whiteout");
                let _ = fs.remove_all(&victim);
                continue;
            }
        }

        let dest = target.join(&rel);
        // Pull every field off the header up-front so the immutable borrow
        // drops before we mutably borrow `entry` to read its body.
        let (etype, mode, uid, gid, size) = {
            let header = entry.header();
            (
                header.entry_type(),
                header.mode().unwrap_or(0o644) & 0o7777,
                header.uid().unwrap_or(0) as i32,
                header.gid().unwrap_or(0) as i32,
                header.size().unwrap_or(0) as usize,
            )
        };

        if etype.is_dir() {
            fs.mkdir_all(&dest)?;
            // Best-effort chmod; ignore failures on read-only Vfs.
            let _ = fs.chmod(&dest, mode | 0o40000);
            continue;
        }

        if etype.is_symlink() || etype.is_hard_link() {
            let link_target = entry
                .link_name()
                .map_err(|e| Error::other(format!("read link name: {e}")))?
                .ok_or_else(|| Error::other("symlink entry missing link name"))?
                .into_owned();
            if let Some(parent) = dest.parent() {
                fs.mkdir_all(parent)?;
            }
            // Replace any existing file/symlink at dest.
            let _ = fs.remove(&dest);
            fs.symlink(&link_target, &dest)?;
            continue;
        }

        if etype.is_file() {
            if let Some(parent) = dest.parent() {
                fs.mkdir_all(parent)?;
            }
            let mut buf = Vec::with_capacity(size);
            use std::io::Read;
            entry
                .read_to_end(&mut buf)
                .map_err(|e| Error::other(format!("read tar file body: {e}")))?;
            fs.write(&dest, &buf)?;
            if mode != 0 {
                let _ = fs.chmod(&dest, mode);
            }
            if uid != 0 || gid != 0 {
                let _ = fs.chown(&dest, uid, gid);
            }
            continue;
        }

        // Char/block devices, fifos, etc. — ignore. Container rootfs layers
        // for the workloads we care about don't typically contain them, and
        // creating them needs root + mknod which the Vfs doesn't expose.
        debug!(
            kind = ?etype,
            path = %rel.display(),
            "skipping non-regular tar entry",
        );
    }

    Ok(())
}

/// Strip a leading "/" or "./", reject any ".." components.
fn sanitize_rel(p: &Path) -> Result<PathBuf> {
    let mut out = PathBuf::new();
    for comp in p.components() {
        match comp {
            Component::Prefix(_) | Component::RootDir | Component::CurDir => {}
            Component::ParentDir => {
                return Err(Error::other(format!(
                    "tar entry path contains ..: {}",
                    p.display()
                )));
            }
            Component::Normal(c) => out.push(c),
        }
    }
    Ok(out)
}

// =====================================================================
// backend_oci — native OCI pull via oci-distribution.
// =====================================================================
#[cfg(all(feature = "oci-builtin", not(feature = "nounpack")))]
mod backend_oci {
    use super::*;

    use oci_distribution::client::{Client, ClientConfig};
    use oci_distribution::manifest as oci_manifest;
    use oci_distribution::secrets::RegistryAuth;
    use oci_distribution::Reference;

    /// Pull `image` and return the layer blobs in apply order.
    ///
    /// `platform` is `linux/arm64`-style. Currently unused: `oci-distribution`
    /// 0.11's high-level `Client::pull` resolves manifests internally and
    /// doesn't take a platform selector. For multi-arch image indexes the
    /// crate picks one arbitrary entry. Supporting explicit platform
    /// selection would mean dropping to `pull_image_manifest` + `pull_blob`
    /// and walking a manifest index by hand — see TODO below.
    pub(super) fn fetch_image_layers(
        image: &str,
        platform: &str,
        _console: &dyn Console,
    ) -> Result<Vec<Vec<u8>>> {
        if !platform.is_empty() {
            // TODO: walk the manifest index manually via
            // Client::pull_image_manifest + pull_blob, matching `os/arch`
            // against entries. For v1 we log and let the registry pick.
            warn!(
                platform,
                "platform selection is not implemented for the native OCI backend; \
                 registry will pick the default manifest"
            );
        }

        let rt = tokio::runtime::Runtime::new()
            .map_err(|e| Error::other(format!("tokio runtime: {e}")))?;

        let image_owned = image.to_string();
        rt.block_on(async move {
            let reference: Reference = image_owned
                .parse()
                .map_err(|e| Error::other(format!("parse oci ref {image_owned:?}: {e}")))?;

            let client = Client::new(ClientConfig {
                // Defaults are fine; rustls TLS is already on via the
                // crate's `rustls-tls` feature in Cargo.toml.
                ..Default::default()
            });
            let auth = RegistryAuth::Anonymous;

            // Accept both Docker and OCI tar / tar+gzip layer media types.
            // `Client::pull` filters layers against this list, so we need to
            // enumerate every variant we want to receive.
            let accepted: Vec<&str> = vec![
                oci_manifest::IMAGE_DOCKER_LAYER_TAR_MEDIA_TYPE,
                oci_manifest::IMAGE_DOCKER_LAYER_GZIP_MEDIA_TYPE,
                oci_manifest::IMAGE_LAYER_GZIP_MEDIA_TYPE,
                oci_manifest::IMAGE_LAYER_MEDIA_TYPE,
            ];

            let image_data = client
                .pull(&reference, &auth, accepted)
                .await
                .map_err(|e| Error::other(format!("oci pull {image_owned:?}: {e}")))?;

            debug!(
                image = %image_owned,
                layers = image_data.layers.len(),
                "oci pull complete",
            );

            Ok(image_data.layers.into_iter().map(|l| l.data).collect())
        })
    }
}

// =====================================================================
// backend_skopeo — `skopeo copy ... dir:` shell-out fallback.
// =====================================================================
#[cfg(any(not(feature = "oci-builtin"), feature = "nounpack"))]
mod backend_skopeo {
    use super::*;

    /// Pull `image` by invoking `skopeo copy docker://<image> dir:<tmpdir>`
    /// and reading the resulting on-disk layout. Layer ordering follows the
    /// manifest's `layers` array (which is already apply-order).
    pub(super) fn fetch_image_layers(
        image: &str,
        platform: &str,
        console: &dyn Console,
    ) -> Result<Vec<Vec<u8>>> {
        let tmp = tempfile::tempdir()
            .map_err(|e| Error::other(format!("create tempdir for unpack_image: {e}")))?;
        let tmp_path = tmp.path();

        let cmd = build_skopeo_cmd(image, platform, tmp_path);
        debug!(cmd = %cmd, "running skopeo");
        console.run(&cmd)?;

        // Read manifest.json and iterate layers in order.
        let manifest_path = tmp_path.join("manifest.json");
        let manifest_bytes = std::fs::read(&manifest_path)
            .map_err(|e| Error::io_at(manifest_path.clone(), e))?;
        let manifest: Manifest = serde_json::from_slice(&manifest_bytes)
            .map_err(|e| Error::other(format!("parse {manifest_path:?}: {e}")))?;

        let mut out: Vec<Vec<u8>> = Vec::with_capacity(manifest.layers.len());
        for layer in &manifest.layers {
            let blob = blob_path(tmp_path, &layer.digest)?;
            debug!(digest = %layer.digest, media = %layer.media_type, "reading layer blob");
            let data = std::fs::read(&blob).map_err(|e| Error::io_at(blob.clone(), e))?;
            out.push(data);
        }
        Ok(out)
    }

    /// Build the skopeo command line. `platform` is `linux/amd64`-style.
    pub(super) fn build_skopeo_cmd(source: &str, platform: &str, tmp: &Path) -> String {
        // skopeo copy [--override-os OS] [--override-arch ARCH] docker://<src> dir:<tmp>
        let mut s = String::from("skopeo copy");
        if !platform.is_empty() {
            if let Some((os, arch)) = platform.split_once('/') {
                s.push_str(&format!(
                    " --override-os {} --override-arch {}",
                    shell_escape(os),
                    shell_escape(arch)
                ));
            } else {
                // Single token — treat as arch only.
                s.push_str(&format!(" --override-arch {}", shell_escape(platform)));
            }
        }
        s.push_str(&format!(
            " docker://{} dir:{}",
            shell_escape(source),
            shell_escape(&tmp.display().to_string())
        ));
        s
    }

    /// Minimal shell-escape: wrap in single quotes and escape embedded ones.
    /// Sufficient for image refs and paths (no shell metacharacters expected
    /// in well-formed inputs, but we still defend against funny chars).
    pub(super) fn shell_escape(s: &str) -> String {
        if s.is_empty() {
            return "''".to_string();
        }
        if s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '.' | '_' | '-' | ':' | '@'))
        {
            return s.to_string();
        }
        let mut out = String::with_capacity(s.len() + 2);
        out.push('\'');
        for c in s.chars() {
            if c == '\'' {
                out.push_str("'\\''");
            } else {
                out.push(c);
            }
        }
        out.push('\'');
        out
    }

    /// `sha256:abcd...` → `<tmp>/abcd...`. Skopeo's dir transport stores
    /// blobs keyed by the digest with the algorithm prefix stripped.
    pub(super) fn blob_path(tmp: &Path, digest: &str) -> Result<PathBuf> {
        let stripped = digest
            .split_once(':')
            .map(|(_, rest)| rest)
            .unwrap_or(digest);
        if stripped.is_empty() {
            return Err(Error::other(format!("invalid blob digest: {digest}")));
        }
        Ok(tmp.join(stripped))
    }

    // ---------- skopeo `manifest.json` shape ----------

    #[derive(Debug, serde::Deserialize)]
    struct Manifest {
        #[serde(default)]
        layers: Vec<ManifestLayer>,
    }

    #[derive(Debug, serde::Deserialize)]
    struct ManifestLayer {
        #[serde(default)]
        digest: String,
        #[serde(rename = "mediaType", default)]
        media_type: String,
    }
}

// =====================================================================
// Tests — extraction logic (backend-agnostic).
// =====================================================================

#[cfg(all(test, not(feature = "nounpack")))]
mod tests {
    use super::*;
    use flate2::write::GzEncoder;
    use flate2::Compression;

    /// Build a tar.gz blob in memory from `(path, mode, contents)` triples.
    /// Paths ending in `/` are written as Directory entries (matching what
    /// real OCI layers contain); everything else is a regular file.
    fn build_tar_gz(entries: &[(&str, u32, &[u8])]) -> Vec<u8> {
        let buf: Vec<u8> = Vec::new();
        let enc = GzEncoder::new(buf, Compression::default());
        let mut builder = tar::Builder::new(enc);
        for (path, mode, contents) in entries {
            let mut header = tar::Header::new_gnu();
            header.set_path(path).expect("set_path");
            header.set_mode(*mode);
            if path.ends_with('/') {
                header.set_entry_type(tar::EntryType::Directory);
                header.set_size(0);
                header.set_cksum();
                builder
                    .append(&header, std::io::empty())
                    .expect("append dir");
            } else {
                header.set_size(contents.len() as u64);
                header.set_cksum();
                builder
                    .append(&header, *contents)
                    .expect("append tar entry");
            }
        }
        let enc = builder.into_inner().expect("into_inner");
        enc.finish().expect("gz finish")
    }

    /// Build a plain (non-gz) tar blob.
    fn build_tar(entries: &[(&str, u32, &[u8])]) -> Vec<u8> {
        let buf: Vec<u8> = Vec::new();
        let mut builder = tar::Builder::new(buf);
        for (path, mode, contents) in entries {
            let mut header = tar::Header::new_gnu();
            header.set_path(path).expect("set_path");
            header.set_mode(*mode);
            if path.ends_with('/') {
                header.set_entry_type(tar::EntryType::Directory);
                header.set_size(0);
                header.set_cksum();
                builder
                    .append(&header, std::io::empty())
                    .expect("append dir");
            } else {
                header.set_size(contents.len() as u64);
                header.set_cksum();
                builder
                    .append(&header, *contents)
                    .expect("append tar entry");
            }
        }
        builder.into_inner().expect("into_inner")
    }

    /// Build a tar.gz containing a single whiteout entry.
    fn build_whiteout_tar_gz(path_with_wh: &str) -> Vec<u8> {
        let buf: Vec<u8> = Vec::new();
        let enc = GzEncoder::new(buf, Compression::default());
        let mut builder = tar::Builder::new(enc);
        let mut header = tar::Header::new_gnu();
        header.set_path(path_with_wh).expect("set_path");
        header.set_size(0);
        header.set_mode(0o644);
        header.set_cksum();
        builder
            .append(&header, std::io::empty())
            .expect("append whiteout");
        let enc = builder.into_inner().expect("into_inner");
        enc.finish().expect("gz finish")
    }

    #[test]
    fn empty_unpack_images_is_ok() {
        use crate::console::RecordingConsole;
        use crate::vfs::MemVfs;
        let stage = Stage::default();
        let fs = MemVfs::new();
        let console = RecordingConsole::new();
        run(&stage, &fs, &console).expect("empty -> Ok");
        assert!(console.commands().is_empty(), "no skopeo invocations");
    }

    #[test]
    fn extracts_files_into_target() {
        use crate::vfs::MemVfs;
        let blob = build_tar_gz(&[
            ("etc/", 0o755, b""),
            ("etc/hello", 0o600, b"hi there"),
            ("usr/bin/", 0o755, b""),
            ("usr/bin/yip", 0o755, b"\x7fELF-fake"),
        ]);
        let fs = MemVfs::new();
        let target = Path::new("/out");
        fs.mkdir_all(target).unwrap();

        extract_layer(
            &blob,
            "application/vnd.oci.image.layer.v1.tar+gzip",
            target,
            &fs,
        )
        .expect("extract should succeed");

        assert_eq!(fs.read(Path::new("/out/etc/hello")).unwrap(), b"hi there");
        assert_eq!(
            fs.read(Path::new("/out/usr/bin/yip")).unwrap(),
            b"\x7fELF-fake"
        );
        let m = fs.metadata(Path::new("/out/etc/hello")).unwrap();
        assert_eq!(m.mode & 0o7777, 0o600);
        let m = fs.metadata(Path::new("/out/usr/bin/yip")).unwrap();
        assert_eq!(m.mode & 0o7777, 0o755);
    }

    #[test]
    fn extracts_plain_tar_without_gzip() {
        use crate::vfs::MemVfs;
        let blob = build_tar(&[("a.txt", 0o644, b"abc")]);
        let fs = MemVfs::new();
        let target = Path::new("/t");
        fs.mkdir_all(target).unwrap();
        extract_layer(&blob, "application/vnd.oci.image.layer.v1.tar", target, &fs)
            .expect("plain tar extracts");
        assert_eq!(fs.read(Path::new("/t/a.txt")).unwrap(), b"abc");
    }

    #[test]
    fn auto_detects_gzip_from_magic_when_mediatype_empty() {
        use crate::vfs::MemVfs;
        let blob = build_tar_gz(&[("x", 0o644, b"y")]);
        let fs = MemVfs::new();
        let target = Path::new("/m");
        fs.mkdir_all(target).unwrap();
        extract_layer(&blob, "", target, &fs).expect("magic-byte detection");
        assert_eq!(fs.read(Path::new("/m/x")).unwrap(), b"y");
    }

    #[test]
    fn whiteout_removes_prior_layer_file() {
        use crate::vfs::MemVfs;
        let fs = MemVfs::new();
        let target = Path::new("/r");
        fs.mkdir_all(target).unwrap();

        // First layer: writes foo.
        let l1 = build_tar_gz(&[("foo", 0o644, b"original")]);
        extract_layer(&l1, "application/vnd.oci.image.layer.v1.tar+gzip", target, &fs).unwrap();
        assert!(fs.exists(Path::new("/r/foo")));

        // Second layer: a whiteout entry .wh.foo at the same level.
        let l2 = build_whiteout_tar_gz(".wh.foo");
        extract_layer(&l2, "application/vnd.oci.image.layer.v1.tar+gzip", target, &fs).unwrap();
        assert!(
            !fs.exists(Path::new("/r/foo")),
            "whiteout should have removed /r/foo"
        );
    }

    #[test]
    fn whiteout_in_subdir_removes_correct_file() {
        use crate::vfs::MemVfs;
        let fs = MemVfs::new();
        let target = Path::new("/r");
        fs.mkdir_all(target).unwrap();

        let l1 = build_tar_gz(&[
            ("sub/", 0o755, b""),
            ("sub/keep", 0o644, b"k"),
            ("sub/gone", 0o644, b"g"),
        ]);
        extract_layer(&l1, "application/vnd.oci.image.layer.v1.tar+gzip", target, &fs).unwrap();

        let l2 = build_whiteout_tar_gz("sub/.wh.gone");
        extract_layer(&l2, "application/vnd.oci.image.layer.v1.tar+gzip", target, &fs).unwrap();

        assert!(fs.exists(Path::new("/r/sub/keep")));
        assert!(!fs.exists(Path::new("/r/sub/gone")));
    }

    #[test]
    fn opaque_whiteout_clears_directory() {
        use crate::vfs::MemVfs;
        let fs = MemVfs::new();
        let target = Path::new("/r");
        fs.mkdir_all(target).unwrap();

        let l1 = build_tar_gz(&[
            ("d/", 0o755, b""),
            ("d/a", 0o644, b"a"),
            ("d/b", 0o644, b"b"),
        ]);
        extract_layer(&l1, "application/vnd.oci.image.layer.v1.tar+gzip", target, &fs).unwrap();
        assert!(fs.exists(Path::new("/r/d/a")));

        let l2 = build_whiteout_tar_gz("d/.wh..wh..opq");
        extract_layer(&l2, "application/vnd.oci.image.layer.v1.tar+gzip", target, &fs).unwrap();
        assert!(!fs.exists(Path::new("/r/d/a")));
        assert!(!fs.exists(Path::new("/r/d/b")));
        // Directory itself is recreated empty.
        assert!(fs.exists(Path::new("/r/d")));
    }

    #[test]
    #[ignore = "tar crate's set_path rejects `..` upfront, so we can't \
                construct a malicious tar to feed extract_layer. \
                sanitize_rel still rejects `..` at extract time — \
                covered via direct unit test below."]
    fn rejects_path_traversal() {
        use crate::vfs::MemVfs;
        let blob = build_tar_gz(&[("../escape", 0o644, b"nope")]);
        let fs = MemVfs::new();
        let target = Path::new("/safe");
        fs.mkdir_all(target).unwrap();
        let err = extract_layer(
            &blob,
            "application/vnd.oci.image.layer.v1.tar+gzip",
            target,
            &fs,
        )
        .expect_err("must reject ..");
        match err {
            Error::Other(msg) => assert!(msg.contains(".."), "msg: {msg}"),
            other => panic!("expected Other, got {other:?}"),
        }
    }

    #[test]
    fn sanitize_rel_rejects_dotdot() {
        let err = sanitize_rel(Path::new("a/../b")).expect_err("must reject ..");
        match err {
            Error::Other(msg) => assert!(msg.contains(".."), "msg: {msg}"),
            other => panic!("expected Other, got {other:?}"),
        }
    }

    #[test]
    fn symlink_entry_extracted() {
        use crate::vfs::MemVfs;
        // Build a tar with a single symlink "link" -> "target".
        let buf: Vec<u8> = Vec::new();
        let enc = GzEncoder::new(buf, Compression::default());
        let mut builder = tar::Builder::new(enc);
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(tar::EntryType::Symlink);
        header.set_size(0);
        header.set_mode(0o777);
        builder
            .append_link(&mut header, "link", "target")
            .expect("append_link");
        let enc = builder.into_inner().expect("into_inner");
        let blob = enc.finish().expect("gz finish");

        let fs = MemVfs::new();
        let target = Path::new("/s");
        fs.mkdir_all(target).unwrap();
        extract_layer(&blob, "application/vnd.oci.image.layer.v1.tar+gzip", target, &fs)
            .expect("symlink ok");
        let m = fs.metadata(Path::new("/s/link")).expect("link exists");
        assert!(m.is_symlink);
    }

    #[test]
    fn empty_source_or_target_is_warned_not_errored() {
        use crate::console::RecordingConsole;
        use crate::vfs::MemVfs;
        let mut stage = Stage::default();
        stage.unpack_images.push(UnpackImageConf {
            source: "".into(),
            target: "/foo".into(),
            platform: "".into(),
        });
        stage.unpack_images.push(UnpackImageConf {
            source: "alpine".into(),
            target: "".into(),
            platform: "".into(),
        });
        let fs = MemVfs::new();
        let console = RecordingConsole::new();
        run(&stage, &fs, &console).expect("missing fields are skipped, not errors");
        assert!(console.commands().is_empty(), "skopeo not invoked");
    }
}

// =====================================================================
// Tests — skopeo command construction (only when that backend is active).
// =====================================================================

#[cfg(all(test, any(not(feature = "oci-builtin"), feature = "nounpack")))]
mod tests_skopeo {
    use super::backend_skopeo::{blob_path, build_skopeo_cmd, shell_escape};
    use std::path::{Path, PathBuf};

    #[test]
    fn skopeo_cmd_includes_platform_overrides() {
        let tmp = PathBuf::from("/tmp/x");
        let cmd = build_skopeo_cmd("quay.io/kairos/core:latest", "linux/arm64", &tmp);
        assert!(cmd.contains("--override-os linux"), "got: {cmd}");
        assert!(cmd.contains("--override-arch arm64"), "got: {cmd}");
        assert!(cmd.contains("docker://quay.io/kairos/core:latest"), "got: {cmd}");
        assert!(cmd.contains("dir:/tmp/x"), "got: {cmd}");
    }

    #[test]
    fn skopeo_cmd_without_platform_omits_overrides() {
        let tmp = PathBuf::from("/tmp/y");
        let cmd = build_skopeo_cmd("alpine:latest", "", &tmp);
        assert!(!cmd.contains("--override-os"));
        assert!(!cmd.contains("--override-arch"));
        assert!(cmd.contains("docker://alpine:latest"));
        assert!(cmd.contains("dir:/tmp/y"));
    }

    #[test]
    fn shell_escape_preserves_safe_chars() {
        assert_eq!(shell_escape("quay.io/foo/bar:latest"), "quay.io/foo/bar:latest");
        assert_eq!(shell_escape("linux"), "linux");
    }

    #[test]
    fn shell_escape_quotes_funky_input() {
        let got = shell_escape("hello world");
        assert_eq!(got, "'hello world'");
        let got = shell_escape("it's");
        assert_eq!(got, "'it'\\''s'");
    }

    #[test]
    fn blob_path_strips_algorithm_prefix() {
        let p = blob_path(Path::new("/tmp"), "sha256:abcdef").unwrap();
        assert_eq!(p, PathBuf::from("/tmp/abcdef"));
        // No prefix is still accepted.
        let p = blob_path(Path::new("/tmp"), "abcdef").unwrap();
        assert_eq!(p, PathBuf::from("/tmp/abcdef"));
    }
}

// =====================================================================
// Tests — online smoke tests (per backend, gated #[ignore]).
// =====================================================================

/// Online smoke test for the native OCI backend. Pulls a small image and
/// asserts that at least one file landed in the target dir.
#[cfg(all(test, feature = "oci-builtin", not(feature = "nounpack")))]
mod tests_online_oci {
    use super::*;

    #[test]
    #[ignore = "online: pulls alpine:latest via oci-distribution"]
    fn native_pull_and_extract_alpine() {
        use crate::console::StandardConsole;
        use crate::vfs::TempVfs;

        let fs = TempVfs::new().expect("tempvfs");
        let target = fs.root.join("rootfs");
        let stage = Stage {
            unpack_images: vec![UnpackImageConf {
                source: "docker.io/library/alpine:latest".into(),
                target: target.display().to_string(),
                platform: "".into(),
            }],
            ..Default::default()
        };
        let console = StandardConsole::new();
        run(&stage, &fs, &console).expect("alpine native pull should succeed");
        assert!(
            target.join("bin/busybox").exists() || target.join("bin/sh").exists(),
            "expected /bin/busybox or /bin/sh in extracted alpine rootfs",
        );
    }
}

/// Online smoke test for the skopeo backend. Requires `skopeo` on $PATH.
/// Only meaningful when the skopeo backend is the active one AND the
/// `nounpack` kill-switch is off (otherwise `run()` short-circuits).
#[cfg(all(test, not(feature = "oci-builtin"), not(feature = "nounpack")))]
mod tests_online_skopeo {
    use super::*;

    #[test]
    #[ignore = "online: requires skopeo + network"]
    fn skopeo_pull_and_extract_alpine() {
        use crate::console::StandardConsole;
        use crate::vfs::TempVfs;

        let fs = TempVfs::new().expect("tempvfs");
        let target = fs.root.join("rootfs");
        let stage = Stage {
            unpack_images: vec![UnpackImageConf {
                source: "docker.io/library/alpine:latest".into(),
                target: target.display().to_string(),
                platform: "linux/amd64".into(),
            }],
            ..Default::default()
        };
        let console = StandardConsole::new();
        run(&stage, &fs, &console).expect("alpine skopeo pull should succeed");
        assert!(target.join("bin/busybox").exists() || target.join("bin/sh").exists());
    }
}

// =====================================================================
// Tests — nounpack disabled-feature behaviour.
// =====================================================================

#[cfg(all(test, feature = "nounpack"))]
mod tests_disabled {
    use super::*;
    use crate::console::RecordingConsole;
    use crate::vfs::MemVfs;

    #[test]
    fn returns_error_when_feature_disabled() {
        let stage = Stage {
            unpack_images: vec![UnpackImageConf {
                source: "alpine".into(),
                target: "/t".into(),
                platform: "".into(),
            }],
            ..Default::default()
        };
        let fs = MemVfs::new();
        let console = RecordingConsole::new();
        let err = run(&stage, &fs, &console).expect_err("disabled -> Err");
        match err {
            Error::Multi(errs) => {
                assert_eq!(errs.len(), 1);
                match &errs[0] {
                    Error::Other(msg) => assert!(msg.contains("disabled at build time")),
                    other => panic!("expected Other, got {other:?}"),
                }
            }
            other => panic!("expected Multi, got {other:?}"),
        }
    }

    #[test]
    fn empty_unpack_images_is_ok_even_when_disabled() {
        let stage = Stage::default();
        let fs = MemVfs::new();
        let console = RecordingConsole::new();
        run(&stage, &fs, &console).expect("no-op");
    }
}
