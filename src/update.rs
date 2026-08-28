//! Update checker and in-app self-updater driven by GitHub Releases.
//!
//! [`check`] asks the GitHub API for the latest release and compares its tag
//! with the running version; [`download_and_install`] streams the archive for
//! this platform next to the executable, verifies the SHA-256 digest GitHub
//! publishes for the asset, extracts the binary and swaps it into place with
//! `self-replace`, after which [`relaunch`] starts the new build. Everything
//! here blocks and is meant to run on a background thread; the UI polls the
//! results. No egui in this module.
//!
//! Release assets are named `openclip-<version>-<target>.<zip|tar.gz>` and hold
//! a folder of the same stem with the binary, LICENSE and README; only the
//! binary is taken out of the archive.

use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Context, anyhow, bail};
use semver::Version;
use serde::Deserialize;
use sha2::{Digest, Sha256};

pub const REPO: &str = "catalingrigoriev285/openclip";
/// The human-facing page for the newest release (fallback when in-app install is not possible).
pub const RELEASES_URL: &str = "https://github.com/catalingrigoriev285/openclip/releases/latest";
const API_URL: &str = "https://api.github.com/repos/catalingrigoriev285/openclip/releases/latest";
/// File name of the binary inside the release archive.
pub const BIN_NAME: &str = if cfg!(windows) { "openclip.exe" } else { "openclip" };
/// Environment variable that overrides the running version (manual testing of
/// the update flow against a real release).
pub const PRETEND_VERSION_ENV: &str = "OPENCLIP_UPDATE_PRETEND_VERSION";

/// Asset name suffix built by the release workflow for this platform; `None`
/// when no release archive exists for the target (e.g. macOS x86_64).
#[cfg(all(windows, target_arch = "x86_64"))]
pub const TARGET_SUFFIX: Option<&str> = Some("windows-x86_64.zip");
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
pub const TARGET_SUFFIX: Option<&str> = Some("linux-x86_64.tar.gz");
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
pub const TARGET_SUFFIX: Option<&str> = Some("macos-arm64.tar.gz");
#[cfg(not(any(
    all(windows, target_arch = "x86_64"),
    all(target_os = "linux", target_arch = "x86_64"),
    all(target_os = "macos", target_arch = "aarch64")
)))]
pub const TARGET_SUFFIX: Option<&str> = None;

const DOWNLOAD_BLOCK: usize = 64 * 1024;

/// A downloadable release archive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Asset {
    pub name: String,
    pub url: String,
    /// Size announced by the API (0 when unknown).
    pub size: u64,
    /// SHA-256 published by GitHub for the asset, when present.
    pub sha256: Option<[u8; 32]>,
}

/// A published release that is newer than the running build.
#[derive(Debug, Clone)]
pub struct Release {
    pub version: Version,
    pub tag: String,
    pub name: String,
    pub html_url: String,
    /// Release notes (GitHub-flavoured markdown, shown as plain text).
    pub body: String,
    /// The archive for this platform, if the release has one.
    pub asset: Option<Asset>,
}

/// Download state shared with the UI.
#[derive(Debug, Default)]
pub struct Progress {
    pub downloaded: AtomicU64,
    pub cancel: AtomicBool,
}

/// The version this process reports, honouring [`PRETEND_VERSION_ENV`].
pub fn local_version() -> Version {
    if let Ok(v) = std::env::var(PRETEND_VERSION_ENV)
        && let Ok(v) = Version::parse(v.trim())
    {
        return v;
    }
    Version::parse(env!("CARGO_PKG_VERSION")).expect("crate version is valid semver")
}

/// Whether `remote` should be offered as an update over `local`.
pub fn is_newer(remote: &Version, local: &Version) -> bool {
    remote > local
}

/// Queries GitHub for the latest release. `Ok(None)` means the running build is
/// up to date (`/releases/latest` never returns drafts or pre-releases).
pub fn check() -> anyhow::Result<Option<Release>> {
    let local = local_version();
    let json = fetch_latest_json()?;
    let release = parse_release(&json)?;
    log::info!("update check: latest release is {} (running {local})", release.version);
    Ok(is_newer(&release.version, &local).then_some(release))
}

fn fetch_latest_json() -> anyhow::Result<String> {
    let result = agent(Duration::from_secs(10))
        .get(API_URL)
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .call();
    let mut response = match result {
        Ok(r) => r,
        Err(ureq::Error::StatusCode(403 | 429)) => bail!("GitHub rate limit reached, try again later"),
        Err(ureq::Error::StatusCode(404)) => bail!("no release has been published yet"),
        Err(e) => return Err(anyhow!(e).context("contacting GitHub")),
    };
    response.body_mut().read_to_string().context("reading the GitHub response")
}

/// HTTP client: OS certificate store, sensible timeouts. `global` bounds the
/// whole call (`None` for downloads, which are bounded per phase instead).
fn agent(global: Duration) -> ureq::Agent {
    let tls = ureq::tls::TlsConfig::builder().root_certs(ureq::tls::RootCerts::PlatformVerifier).build();
    ureq::Agent::config_builder()
        .timeout_global(Some(global))
        .user_agent(format!("openclip/{}", env!("CARGO_PKG_VERSION")))
        .http_status_as_error(true)
        .tls_config(tls)
        .build()
        .new_agent()
}

fn download_agent() -> ureq::Agent {
    let tls = ureq::tls::TlsConfig::builder().root_certs(ureq::tls::RootCerts::PlatformVerifier).build();
    ureq::Agent::config_builder()
        .timeout_connect(Some(Duration::from_secs(15)))
        .timeout_recv_response(Some(Duration::from_secs(30)))
        .timeout_recv_body(Some(Duration::from_secs(15 * 60)))
        .user_agent(format!("openclip/{}", env!("CARGO_PKG_VERSION")))
        .http_status_as_error(true)
        .tls_config(tls)
        .build()
        .new_agent()
}

#[derive(Deserialize)]
struct ApiRelease {
    tag_name: String,
    #[serde(default)]
    name: Option<String>,
    html_url: String,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    assets: Vec<ApiAsset>,
}

#[derive(Deserialize)]
struct ApiAsset {
    name: String,
    #[serde(default)]
    size: u64,
    browser_download_url: String,
    #[serde(default)]
    digest: Option<String>,
}

/// Parses a GitHub "get a release" JSON document and picks this platform's asset.
pub fn parse_release(json: &str) -> anyhow::Result<Release> {
    let api: ApiRelease = serde_json::from_str(json).context("unexpected GitHub response")?;
    let version = Version::parse(api.tag_name.trim_start_matches(['v', 'V']))
        .with_context(|| format!("release tag {:?} is not a version", api.tag_name))?;
    let assets: Vec<Asset> = api
        .assets
        .into_iter()
        .map(|a| Asset {
            name: a.name,
            url: a.browser_download_url,
            size: a.size,
            sha256: a.digest.as_deref().and_then(parse_digest),
        })
        .collect();
    Ok(Release {
        asset: pick_asset(&assets, TARGET_SUFFIX),
        version,
        name: api.name.filter(|n| !n.trim().is_empty()).unwrap_or_else(|| api.tag_name.clone()),
        tag: api.tag_name,
        html_url: api.html_url,
        body: api.body.unwrap_or_default(),
    })
}

/// The release archive for a platform (`openclip-<ver>-<suffix>`).
pub fn pick_asset(assets: &[Asset], suffix: Option<&str>) -> Option<Asset> {
    let suffix = suffix?;
    assets.iter().find(|a| a.name.starts_with("openclip-") && a.name.ends_with(suffix)).cloned()
}

/// `"sha256:<64 hex digits>"` → bytes.
fn parse_digest(s: &str) -> Option<[u8; 32]> {
    let hex = s.trim().strip_prefix("sha256:")?;
    if hex.len() != 64 || !hex.is_ascii() {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[2 * i..2 * i + 2], 16).ok()?;
    }
    Some(out)
}

/// Whether the executable can be replaced in place (its folder is writable).
/// Probed once per process; the UI asks every frame while the dialog is open.
pub fn install_dir_writable() -> bool {
    static WRITABLE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *WRITABLE.get_or_init(|| {
        std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(crate::settings::dir_is_writable))
            .unwrap_or(false)
    })
}

/// Removes leftover files on every exit path.
struct Cleanup(Vec<PathBuf>);

impl Drop for Cleanup {
    fn drop(&mut self) {
        for p in &self.0 {
            if p.exists()
                && let Err(e) = fs::remove_file(p)
            {
                log::warn!("update: could not remove {}: {e}", p.display());
            }
        }
    }
}

/// Downloads the release archive next to the executable, verifies it, extracts
/// the binary and replaces the running executable. Returns the executable path
/// to hand to [`relaunch`]. Cancellation (via [`Progress::cancel`]) surfaces as
/// an error; the caller can tell it apart by reading the flag.
pub fn download_and_install(release: &Release, progress: &Progress) -> anyhow::Result<PathBuf> {
    let asset = release.asset.as_ref().ok_or_else(|| anyhow!("no download for this platform"))?;
    // Resolve the path before anything moves (on Linux /proc/self/exe changes after the swap).
    let exe = std::env::current_exe().context("locating the executable")?;
    let dir = exe.parent().ok_or_else(|| anyhow!("executable has no parent folder"))?.to_path_buf();
    let exe_name = exe.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| BIN_NAME.into());
    let ext = if asset.name.ends_with(".zip") { "zip" } else { "tar.gz" };
    // Same folder (and volume) as the executable so the final rename is atomic.
    let archive = dir.join(format!(".openclip-update-{}.{ext}", std::process::id()));
    let new_exe = dir.join(format!("{exe_name}.new"));
    let _cleanup = Cleanup(vec![archive.clone(), new_exe.clone()]);

    download_to(asset, &archive, progress)?;
    if progress.cancel.load(Ordering::Relaxed) {
        bail!("cancelled");
    }
    extract_binary(&archive, &new_exe)?;
    self_replace::self_replace(&new_exe).context("replacing the executable")?;
    log::info!("update: installed {} over {}", release.version, exe.display());
    Ok(exe)
}

fn download_to(asset: &Asset, dest: &Path, progress: &Progress) -> anyhow::Result<()> {
    log::info!("update: downloading {} ({} bytes)", asset.url, asset.size);
    let mut response =
        download_agent().get(&asset.url).call().with_context(|| format!("downloading {}", asset.name))?;
    let mut reader = response.body_mut().as_reader();
    let mut file = File::create(dest).with_context(|| format!("creating {}", dest.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; DOWNLOAD_BLOCK];
    let mut total = 0u64;
    loop {
        if progress.cancel.load(Ordering::Relaxed) {
            bail!("cancelled");
        }
        let n = reader.read(&mut buf).context("download interrupted")?;
        if n == 0 {
            break;
        }
        file.write_all(&buf[..n]).context("writing the download")?;
        hasher.update(&buf[..n]);
        total += n as u64;
        progress.downloaded.store(total, Ordering::Relaxed);
        if asset.size > 0 && total > asset.size {
            bail!("download is larger than announced ({} bytes)", asset.size);
        }
    }
    file.flush()?;
    drop(file);
    if asset.size > 0 && total != asset.size {
        bail!("incomplete download: {total} of {} bytes", asset.size);
    }
    let digest: [u8; 32] = hasher.finalize().into();
    match asset.sha256 {
        Some(expected) if expected != digest => bail!("checksum mismatch — the download is corrupt"),
        Some(_) => log::info!("update: sha256 verified"),
        None => log::warn!("update: GitHub published no digest for {}; checksum not verified", asset.name),
    }
    Ok(())
}

/// Pulls [`BIN_NAME`] out of the release archive into `dest` (Windows: zip).
#[cfg(windows)]
fn extract_binary(archive: &Path, dest: &Path) -> anyhow::Result<()> {
    let file = File::open(archive)?;
    let mut zip = zip::ZipArchive::new(file).context("reading the zip archive")?;
    for i in 0..zip.len() {
        let mut entry = zip.by_index(i)?;
        if !entry.is_file() {
            continue;
        }
        let Some(path) = entry.enclosed_name() else { continue };
        if path.file_name().and_then(|n| n.to_str()) != Some(BIN_NAME) {
            continue;
        }
        let mut out = File::create(dest).with_context(|| format!("creating {}", dest.display()))?;
        io::copy(&mut entry, &mut out).context("extracting the binary")?;
        out.flush()?;
        return Ok(());
    }
    bail!("{BIN_NAME} not found in {}", archive.display())
}

/// Pulls [`BIN_NAME`] out of the release archive into `dest` (Unix: tar.gz).
#[cfg(unix)]
fn extract_binary(archive: &Path, dest: &Path) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let file = File::open(archive)?;
    let mut tar = tar::Archive::new(flate2::read::GzDecoder::new(file));
    for entry in tar.entries().context("reading the tar archive")? {
        let mut entry = entry?;
        if !entry.header().entry_type().is_file() {
            continue;
        }
        let is_bin = entry.path()?.file_name().and_then(|n| n.to_str()) == Some(BIN_NAME);
        if !is_bin {
            continue;
        }
        let mut out = File::create(dest).with_context(|| format!("creating {}", dest.display()))?;
        io::copy(&mut entry, &mut out).context("extracting the binary")?;
        out.flush()?;
        drop(out);
        fs::set_permissions(dest, fs::Permissions::from_mode(0o755))?;
        return Ok(());
    }
    bail!("{BIN_NAME} not found in {}", archive.display())
}

/// Starts the (freshly installed) executable detached; the caller then exits.
/// Spawned directly, never through a shell, so no console window flashes.
pub fn relaunch(exe: &Path) -> io::Result<()> {
    let mut cmd = std::process::Command::new(exe);
    if let Some(dir) = exe.parent() {
        cmd.current_dir(dir);
    }
    cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    cmd.spawn().map(drop)
}

#[cfg(test)]
mod tests {
    use super::*;

    const DIGEST: &str = "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    fn fixture() -> String {
        format!(
            r#"{{
  "tag_name": "v0.1.1", "name": "v0.1.1", "draft": false, "prerelease": false,
  "html_url": "https://github.com/catalingrigoriev285/openclip/releases/tag/v0.1.1",
  "body": "**Full Changelog**: https://github.com/catalingrigoriev285/openclip/compare/v0.1.0...v0.1.1",
  "assets": [
    {{ "name": "openclip-0.1.1-macos-arm64.tar.gz", "size": 6913527, "digest": "{DIGEST}",
      "browser_download_url": "https://github.com/catalingrigoriev285/openclip/releases/download/v0.1.1/openclip-0.1.1-macos-arm64.tar.gz" }},
    {{ "name": "openclip-0.1.1-windows-x86_64.zip", "size": 8122290,
      "browser_download_url": "https://github.com/catalingrigoriev285/openclip/releases/download/v0.1.1/openclip-0.1.1-windows-x86_64.zip" }}
  ]
}}"#
        )
    }

    fn v(s: &str) -> Version {
        Version::parse(s).unwrap()
    }

    #[test]
    fn newer_means_strictly_greater() {
        assert!(is_newer(&v("0.1.3"), &v("0.1.2")));
        assert!(is_newer(&v("1.0.0"), &v("0.9.9")));
        assert!(!is_newer(&v("0.1.2"), &v("0.1.2")));
        // The published release can lag behind a development build.
        assert!(!is_newer(&v("0.1.1"), &v("0.1.2")));
        // Pre-releases sort below the final version.
        assert!(!is_newer(&v("0.2.0-rc.1"), &v("0.2.0")));
        assert!(is_newer(&v("0.2.0"), &v("0.2.0-rc.1")));
    }

    #[test]
    fn local_version_is_the_crate_version() {
        assert_eq!(local_version(), v(env!("CARGO_PKG_VERSION")));
    }

    #[test]
    fn parses_the_github_release_document() {
        let rel = parse_release(&fixture()).unwrap();
        assert_eq!(rel.version, v("0.1.1"));
        assert_eq!(rel.tag, "v0.1.1");
        assert_eq!(rel.name, "v0.1.1");
        assert!(rel.html_url.ends_with("/tag/v0.1.1"));
        assert!(rel.body.starts_with("**Full Changelog**"));
        #[cfg(all(windows, target_arch = "x86_64"))]
        {
            let asset = rel.asset.expect("windows asset");
            assert_eq!(asset.name, "openclip-0.1.1-windows-x86_64.zip");
            assert_eq!(asset.size, 8_122_290);
            assert_eq!(asset.sha256, None);
        }
        #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
        assert!(rel.asset.is_none(), "v0.1.1 has no Linux asset");
    }

    #[test]
    fn tag_without_v_prefix_and_missing_fields_are_tolerated() {
        let rel = parse_release(r#"{"tag_name":"1.2.3","html_url":"u"}"#).unwrap();
        assert_eq!(rel.version, v("1.2.3"));
        assert_eq!(rel.name, "1.2.3");
        assert_eq!(rel.body, "");
        assert!(rel.asset.is_none());
        assert!(parse_release(r#"{"tag_name":"nightly","html_url":"u"}"#).is_err());
        assert!(parse_release("not json").is_err());
    }

    #[test]
    fn picks_the_asset_for_a_platform() {
        let assets = [
            Asset { name: "openclip-0.1.1-macos-arm64.tar.gz".into(), url: "m".into(), size: 1, sha256: None },
            Asset { name: "openclip-0.1.1-windows-x86_64.zip".into(), url: "w".into(), size: 2, sha256: None },
            Asset { name: "other-0.1.1-linux-x86_64.tar.gz".into(), url: "x".into(), size: 3, sha256: None },
        ];
        assert_eq!(pick_asset(&assets, Some("windows-x86_64.zip")).unwrap().url, "w");
        assert_eq!(pick_asset(&assets, Some("macos-arm64.tar.gz")).unwrap().url, "m");
        assert!(pick_asset(&assets, Some("linux-x86_64.tar.gz")).is_none(), "wrong prefix must not match");
        assert!(pick_asset(&assets, None).is_none());
    }

    #[test]
    fn parses_sha256_digests() {
        let d = parse_digest(DIGEST).unwrap();
        assert_eq!(&d[..4], &[0x01, 0x23, 0x45, 0x67]);
        assert_eq!(d[31], 0xef);
        assert!(parse_digest("md5:0123").is_none());
        assert!(parse_digest("sha256:0123").is_none());
        assert!(parse_digest(&format!("sha256:{}", "zz".repeat(32))).is_none());
        assert!(parse_digest(&format!("sha256:{}", "é".repeat(32))).is_none());
    }

    fn scratch_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("openclip-update-{name}-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[cfg(windows)]
    #[test]
    fn extracts_the_binary_from_a_release_zip() {
        use zip::write::SimpleFileOptions;
        let dir = scratch_dir("zip");
        let archive = dir.join("release.zip");
        let mut w = zip::ZipWriter::new(File::create(&archive).unwrap());
        let opts = SimpleFileOptions::default();
        w.add_directory("openclip-9.9.9-windows-x86_64/", opts).unwrap();
        w.start_file("openclip-9.9.9-windows-x86_64/README.md", opts).unwrap();
        w.write_all(b"readme").unwrap();
        w.start_file("openclip-9.9.9-windows-x86_64/openclip.exe", opts).unwrap();
        w.write_all(b"MZ new binary").unwrap();
        w.finish().unwrap();

        let dest = dir.join("openclip.exe.new");
        extract_binary(&archive, &dest).unwrap();
        assert_eq!(fs::read(&dest).unwrap(), b"MZ new binary");

        let mut w = zip::ZipWriter::new(File::create(&archive).unwrap());
        w.start_file("openclip-9.9.9-windows-x86_64/README.md", opts).unwrap();
        w.write_all(b"readme").unwrap();
        w.finish().unwrap();
        assert!(extract_binary(&archive, &dest).is_err(), "archive without the binary is rejected");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn extracts_the_binary_from_a_release_tarball() {
        use std::os::unix::fs::PermissionsExt;
        let dir = scratch_dir("tar");
        let archive = dir.join("release.tar.gz");
        let gz = flate2::write::GzEncoder::new(File::create(&archive).unwrap(), flate2::Compression::fast());
        let mut tar = tar::Builder::new(gz);
        for (name, data) in [("README.md", &b"readme"[..]), ("openclip", &b"\x7fELF new binary"[..])] {
            let mut header = tar::Header::new_gnu();
            header.set_size(data.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            tar.append_data(&mut header, format!("openclip-9.9.9-linux-x86_64/{name}"), data).unwrap();
        }
        tar.into_inner().unwrap().finish().unwrap();

        let dest = dir.join("openclip.new");
        extract_binary(&archive, &dest).unwrap();
        assert_eq!(fs::read(&dest).unwrap(), b"\x7fELF new binary");
        assert_eq!(fs::metadata(&dest).unwrap().permissions().mode() & 0o777, 0o755);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn cleanup_removes_leftovers() {
        let dir = scratch_dir("cleanup");
        let junk = dir.join("junk");
        fs::write(&junk, b"x").unwrap();
        drop(Cleanup(vec![junk.clone(), dir.join("never-created")]));
        assert!(!junk.exists());
        fs::remove_dir_all(&dir).unwrap();
    }
}
