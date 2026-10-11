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

/// Target triple of the PostgreSQL distribution to fetch for this host, when one is published.
///
/// The binaries are single-platform builds published per Rust target triple, so the harness has
/// to ask for the one it is running on: a host without a published build is refused up front,
/// rather than by a download that 404s halfway through bring-up.
pub fn pg_target() -> Option<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => Some("aarch64-apple-darwin"),
        ("macos", "x86_64") => Some("x86_64-apple-darwin"),
        ("linux", "x86_64") => Some("x86_64-unknown-linux-gnu"),
        ("linux", "aarch64") => Some("aarch64-unknown-linux-gnu"),
        ("windows", "x86_64") => Some("x86_64-pc-windows-msvc"),
        _ => None,
    }
}

/// Why this host cannot run the harness, for the one place that reports it.
fn unsupported_host() -> anyhow::Error {
    anyhow::anyhow!(
        "no PostgreSQL build is published for {}/{}; \
         the harness can drive the server on macOS, Linux and Windows x86_64",
        std::env::consts::OS,
        std::env::consts::ARCH
    )
}

/// URL of the PostgreSQL archive for the harness target.
pub fn pg_archive_url() -> Result<String> {
    let target = pg_target().ok_or_else(unsupported_host)?;
    Ok(format!(
        "https://github.com/theseus-rs/postgresql-binaries/releases/download/{PG_VERSION}/postgresql-{PG_VERSION}-{target}.tar.gz"
    ))
}

/// Directory name the PostgreSQL archive extracts to.
fn pg_dir_name() -> Result<String> {
    Ok(format!(
        "postgresql-{PG_VERSION}-{}",
        pg_target().ok_or_else(unsupported_host)?
    ))
}

/// Name of the marker file that keys extraction/build artifacts to their source version.
const STAMP_FILE: &str = ".scwc-e2e-stamp";

/// The file name a program has on this host.
///
/// Windows appends `.exe` to the binaries this workspace builds and to the PostgreSQL tools
/// alike; naming them without a suffix everywhere and adding it here keeps that one fact in one
/// place instead of in every path the harness builds.
pub fn program_name(name: &str) -> String {
    if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_owned()
    }
}

/// The file name of `initdb` on this host.
fn initdb_binary() -> String {
    program_name("initdb")
}

/// Absolute path of the workspace root (the directory holding `.cache`).
///
/// The path is canonicalized and then stripped of the Windows verbatim prefix, because every
/// path the harness hands to an external program — `git`, `cargo`, `tar`, the PostgreSQL tools —
/// is derived from this one, and those programs reject the verbatim form.
pub fn workspace_root() -> Result<PathBuf> {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let root = manifest.join("..").join("..");
    let root = fs::canonicalize(&root)
        .with_context(|| format!("resolving workspace root {}", root.display()))?;
    Ok(strip_verbatim(root))
}

/// Removes the `\\?\` prefix `fs::canonicalize` puts on Windows paths.
///
/// The prefix is what makes a canonical path immune to `MAX_PATH` and to further normalization;
/// it is also what tools outside Rust refuse to open. `\\?\UNC\server\share` becomes the plain
/// `\\server\share` it stands for.
fn strip_verbatim(path: PathBuf) -> PathBuf {
    let text = path.to_string_lossy();
    if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
        return PathBuf::from(format!(r"\\{rest}"));
    }
    match text.strip_prefix(r"\\?\") {
        Some(rest) => PathBuf::from(rest),
        None => path,
    }
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
        self.bin_dir().join(program_name("smartclass-webcam-server"))
    }

    /// The `bin/` directory of the extracted PostgreSQL distribution.
    pub fn pg_bin_dir(&self) -> Result<PathBuf> {
        Ok(self.services_dir().join(pg_dir_name()?).join("bin"))
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
    // Refuses a host with no published PostgreSQL build before anything is downloaded.
    if pg_target().is_none() {
        return Err(unsupported_host());
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

/// Pulls the SHA-256 digest out of a checksum sidecar.
///
/// The sidecars are generated per platform, so their shape differs: the Unix builds publish
/// `<digest>  <file>`, and the Windows one is the output of `certutil -hashfile`, which prints a
/// sentence, the digest, and another sentence. What they have in common is exactly one
/// 64-character hexadecimal token, which is what this looks for rather than trusting a position
/// in the text.
fn checksum_digest(text: &str) -> Option<String> {
    text.split_whitespace()
        .map(|token| token.trim_end_matches(':'))
        .find(|token| token.len() == 64 && token.chars().all(|c| c.is_ascii_hexdigit()))
        .map(str::to_ascii_lowercase)
}

/// Ensure the PostgreSQL distribution is extracted and return its `bin/` directory.
async fn ensure_postgres(paths: &CachePaths, client: &reqwest::Client) -> Result<PathBuf> {
    let dir_name = pg_dir_name()?;
    let dir = paths.services_dir().join(&dir_name);
    let initdb = dir.join("bin").join(initdb_binary());
    if initdb.exists() && stamp_matches(&dir, &dir_name) {
        return Ok(dir.join("bin"));
    }

    let archive_url = pg_archive_url()?;
    let archive = fetch_cached(client, &paths.downloads_dir(), &archive_url).await?;
    let sidecar_url = format!("{archive_url}.sha256");
    let sidecar = fetch_cached(client, &paths.downloads_dir(), &sidecar_url).await?;

    let sidecar_text = fs::read_to_string(&sidecar)
        .with_context(|| format!("reading {}", sidecar.display()))?;
    let expected = checksum_digest(&sidecar_text)
        .ok_or_else(|| anyhow::anyhow!("no SHA-256 digest in {}", sidecar.display()))?;
    let actual = sha256_file(&archive)?;
    if expected != actual {
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

    write_stamp(&dir, &dir_name)?;
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
    // The module and build caches are pointed into the harness cache rather than left at the
    // toolchain's default under the user's home: that directory is not necessarily writable by
    // the process running the tests, and a cache inside `.cache` is removed with the rest of the
    // harness state.
    let go_cache = paths.root().join("go");
    fs::create_dir_all(&go_cache)
        .with_context(|| format!("creating {}", go_cache.display()))?;
    let output = Command::new("go")
        .arg("build")
        .arg("-o")
        .arg(&bin)
        .arg("./cmd/server")
        .current_dir(&src)
        .env("GOMODCACHE", go_cache.join("mod"))
        .env("GOCACHE", go_cache.join("build"))
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
