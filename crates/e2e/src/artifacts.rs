//! Cached artifact setup: the PostgreSQL distribution and the server source/binary.
//!
//! Every network fetch lands in `.cache/downloads/<sha256-of-url>` first; extraction and the
//! Go build are guarded by stamp files so a second test run does no work at all (and no
//! network I/O).

use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};
use tracing::info;

/// Upstream repository of the server under test.
pub const SERVER_REPO_URL: &str = "https://github.com/crazy4chicken/smartclass-webcam-server";

/// Server tag the harness builds and runs.
pub const SERVER_TAG: &str = "v0.3.1";

/// PostgreSQL version used as the server's database.
pub const PG_VERSION: &str = "18.6.0";

/// Target triple of the PostgreSQL distribution; the harness currently ships darwin/arm64 only.
pub const PG_TARGET: &str = "aarch64-apple-darwin";

/// Name of the marker file that keys extraction/build artifacts to their source version.
const STAMP_FILE: &str = ".scwc-e2e-stamp";

/// URL of the PostgreSQL archive for the harness target.
pub fn pg_archive_url() -> String {
    format!(
        "https://github.com/theseus-rs/postgresql-binaries/releases/download/{PG_VERSION}/postgresql-{PG_VERSION}-{PG_TARGET}.tar.gz"
    )
}

/// Directory name the PostgreSQL archive extracts to.
fn pg_dir_name() -> String {
    format!("postgresql-{PG_VERSION}-{PG_TARGET}")
}

/// Absolute path of the workspace root (the directory holding `.cache`).
pub fn workspace_root() -> Result<PathBuf> {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let root = manifest.join("..").join("..");
    fs::canonicalize(&root).with_context(|| format!("resolving workspace root {}", root.display()))
}

/// Paths under `<workspace>/.cache` the harness reads and writes.
#[derive(Debug, Clone)]
pub struct CachePaths {
    root: PathBuf,
}

impl CachePaths {
    /// Locate the cache relative to this crate's manifest directory.
    pub fn discover() -> Result<Self> {
        Ok(Self {
            root: workspace_root()?.join(".cache"),
        })
    }

    /// The `.cache` directory itself.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Directory holding every downloaded artifact, keyed by URL hash.
    pub fn downloads_dir(&self) -> PathBuf {
        self.root.join("downloads")
    }

    /// Directory holding extracted service distributions.
    pub fn services_dir(&self) -> PathBuf {
        self.root.join("services")
    }

    /// Directory holding cloned source checkouts.
    pub fn src_dir(&self) -> PathBuf {
        self.root.join("src")
    }

    /// Directory holding locally built binaries.
    pub fn bin_dir(&self) -> PathBuf {
        self.root.join("bin")
    }

    /// Directory holding per-test run state (data directories, logs).
    pub fn run_dir(&self) -> PathBuf {
        self.root.join("run")
    }

    /// The compiled server binary.
    pub fn server_bin(&self) -> PathBuf {
        self.bin_dir().join("smartclass-webcam-server")
    }

    /// The `bin/` directory of the extracted PostgreSQL distribution.
    pub fn pg_bin_dir(&self) -> PathBuf {
        self.services_dir().join(pg_dir_name()).join("bin")
    }

    /// Create every cache directory the harness uses.
    pub fn ensure_dirs(&self) -> Result<()> {
        for dir in [
            self.downloads_dir(),
            self.services_dir(),
            self.src_dir(),
            self.bin_dir(),
            self.run_dir(),
        ] {
            fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        Ok(())
    }
}

/// Make every cached artifact available and return the resolved cache paths.
///
/// Safe to call repeatedly: the second call is a few file-existence checks.
pub async fn ensure_artifacts() -> Result<CachePaths> {
    if !cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        bail!(
            "the e2e harness ships a darwin/arm64 PostgreSQL build only; \
             host is {}-{}",
            std::env::consts::OS,
            std::env::consts::ARCH
        );
    }

    let paths = CachePaths::discover()?;
    paths.ensure_dirs()?;

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(600))
        .build()
        .context("building the artifact download client")?;

    ensure_postgres(&paths, &client).await?;
    ensure_server(&paths)?;
    Ok(paths)
}

/// Fetch a URL into the cache, downloading only on a miss, and return the cached path.
pub async fn fetch_cached(client: &reqwest::Client, dir: &Path, url: &str) -> Result<PathBuf> {
    fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;

    let key = url_key(url);
    let dest = dir.join(&key);
    if fs::metadata(&dest).map(|meta| meta.len() > 0).unwrap_or(false) {
        return Ok(dest);
    }

    info!(url, "downloading artifact (cache miss)");
    let response = client
        .get(url)
        .send()
        .await
        .with_context(|| format!("downloading {url}"))?;
    if !response.status().is_success() {
        bail!("downloading {url} failed with HTTP {}", response.status());
    }
    let bytes = response
        .bytes()
        .await
        .with_context(|| format!("reading the body of {url}"))?;
    if bytes.is_empty() {
        bail!("downloading {url} returned an empty body");
    }

    let tmp = dir.join(format!("{key}.part"));
    fs::write(&tmp, &bytes).with_context(|| format!("writing {}", tmp.display()))?;
    fs::rename(&tmp, &dest).with_context(|| format!("publishing {}", dest.display()))?;
    info!(bytes = bytes.len(), path = %dest.display(), "artifact cached");
    Ok(dest)
}

/// Ensure the PostgreSQL distribution is extracted and return its `bin/` directory.
async fn ensure_postgres(paths: &CachePaths, client: &reqwest::Client) -> Result<PathBuf> {
    let dir = paths.services_dir().join(pg_dir_name());
    let initdb = dir.join("bin").join("initdb");
    if initdb.exists() && stamp_matches(&dir, &pg_dir_name()) {
        return Ok(dir.join("bin"));
    }

    let archive_url = pg_archive_url();
    let archive = fetch_cached(client, &paths.downloads_dir(), &archive_url).await?;
    let sidecar_url = format!("{archive_url}.sha256");
    let sidecar = fetch_cached(client, &paths.downloads_dir(), &sidecar_url).await?;

    let sidecar_text = fs::read_to_string(&sidecar)
        .with_context(|| format!("reading {}", sidecar.display()))?;
    let expected = sidecar_text
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    let actual = sha256_file(&archive)?;
    if expected.is_empty() || expected != actual {
        bail!("PostgreSQL archive checksum mismatch: expected {expected:?}, got {actual}");
    }

    if dir.exists() {
        fs::remove_dir_all(&dir).with_context(|| format!("removing {}", dir.display()))?;
    }
    info!(archive = %archive.display(), "extracting PostgreSQL");
    let output = Command::new("tar")
        .arg("-xzf")
        .arg(&archive)
        .arg("-C")
        .arg(paths.services_dir())
        .output()
        .context("running tar (is it on PATH?)")?;
    if !output.status.success() {
        bail!(
            "tar failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    if !initdb.exists() {
        bail!(
            "the PostgreSQL archive did not produce {}",
            initdb.display()
        );
    }

    write_stamp(&dir, &pg_dir_name())?;
    Ok(dir.join("bin"))
}

/// Ensure the server source is cloned and built at [`SERVER_TAG`], returning the binary path.
fn ensure_server(paths: &CachePaths) -> Result<PathBuf> {
    let bin = paths.server_bin();
    let src = paths.src_dir().join("smartclass-webcam-server");
    if bin.exists() && stamp_matches(&src, SERVER_TAG) {
        return Ok(bin);
    }

    if src.exists() {
        fs::remove_dir_all(&src).with_context(|| format!("removing {}", src.display()))?;
    }
    info!(tag = SERVER_TAG, "cloning the server source");
    let output = Command::new("git")
        .args(["clone", "--depth", "1", "--branch", SERVER_TAG])
        .arg(SERVER_REPO_URL)
        .arg(&src)
        .output()
        .context("running git (is it on PATH?)")?;
    if !output.status.success() {
        bail!(
            "cloning {SERVER_REPO_URL} at {SERVER_TAG} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    info!("building the server binary");
    let output = Command::new("go")
        .arg("build")
        .arg("-o")
        .arg(&bin)
        .arg("./cmd/server")
        .current_dir(&src)
        .output()
        .context("running go (is it on PATH?)")?;
    if !output.status.success() {
        bail!(
            "go build failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    if !bin.exists() {
        bail!("go build did not produce {}", bin.display());
    }

    write_stamp(&src, SERVER_TAG)?;
    Ok(bin)
}

fn stamp_matches(dir: &Path, expected: &str) -> bool {
    fs::read_to_string(dir.join(STAMP_FILE))
        .map(|stamp| stamp.trim() == expected)
        .unwrap_or(false)
}

fn write_stamp(dir: &Path, value: &str) -> Result<()> {
    let stamp = dir.join(STAMP_FILE);
    fs::write(&stamp, value).with_context(|| format!("writing {}", stamp.display()))
}

/// Hex-encoded SHA-256 of a URL string, used as the download cache key.
fn url_key(url: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(url.as_bytes());
    to_hex(hasher.finalize().as_slice())
}

/// Hex-encoded SHA-256 of a file's contents.
fn sha256_file(path: &Path) -> Result<String> {
    let mut file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 64 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .with_context(|| format!("reading {}", path.display()))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(to_hex(hasher.finalize().as_slice()))
}

fn to_hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;

    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(out, "{byte:02x}");
    }
    out
}
