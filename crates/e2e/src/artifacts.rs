//! Host prerequisites and the cached server build.
//!
//! PostgreSQL is a *host* prerequisite: the harness locates the host's `initdb`/`pg_ctl`/
//! `createdb`/`postgres` (see [`PgBin::resolve`]) and never downloads a database
//! distribution. Only the server under test is fetched: its source is cloned and built into
//! `.cache`, keyed by a stamp file so a second test run does nothing.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use tracing::info;

/// Upstream repository of the server under test.
pub const SERVER_REPO_URL: &str = "https://github.com/crazy4chicken/smartclass-webcam-server";

/// Server tag the harness builds and runs.
pub const SERVER_TAG: &str = "v0.3.1";

/// Name of the marker file that keys the server build to its source tag.
const STAMP_FILE: &str = ".scwc-e2e-stamp";

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

    /// Create every cache directory the harness uses.
    pub fn ensure_dirs(&self) -> Result<()> {
        for dir in [self.src_dir(), self.bin_dir(), self.run_dir()] {
            fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        Ok(())
    }
}

/// The host PostgreSQL tools the harness drives.
///
/// Resolved from `SCWC_PGBIN` (a directory holding the four binaries) when set, otherwise
/// from `PATH`. PostgreSQL is not downloaded: install it on the host, for example with
/// `brew install postgresql@18`.
#[derive(Debug, Clone)]
pub struct PgBin {
    initdb: PathBuf,
    pg_ctl: PathBuf,
    createdb: PathBuf,
    postgres: PathBuf,
}

impl PgBin {
    /// Locate the host's PostgreSQL binaries and check that they run.
    pub fn resolve() -> Result<Self> {
        let dir = std::env::var_os("SCWC_PGBIN").map(PathBuf::from);

        let find = |name: &str| -> Result<PathBuf> {
            match &dir {
                Some(dir) => {
                    let candidate = dir.join(name);
                    if candidate.is_file() {
                        Ok(candidate)
                    } else {
                        bail!("SCWC_PGBIN={} does not contain {name}", dir.display())
                    }
                }
                None => which(name).with_context(|| {
                    format!(
                        "locating {name}: set SCWC_PGBIN to the PostgreSQL bin directory \
                         (for example /opt/homebrew/opt/postgresql@18/bin) or put it on PATH"
                    )
                }),
            }
        };

        let pg = Self {
            initdb: find("initdb")?,
            pg_ctl: find("pg_ctl")?,
            createdb: find("createdb")?,
            postgres: find("postgres")?,
        };

        let output = Command::new(pg.postgres())
            .arg("--version")
            .output()
            .with_context(|| format!("running {} --version", pg.postgres.display()))?;
        if !output.status.success() {
            bail!(
                "{} --version failed: {}",
                pg.postgres.display(),
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        info!(
            version = %String::from_utf8_lossy(&output.stdout).trim(),
            "using the host PostgreSQL"
        );
        Ok(pg)
    }

    /// `initdb`, which creates the scratch data directory.
    pub fn initdb(&self) -> &Path {
        &self.initdb
    }

    /// `pg_ctl`, which starts and stops the scratch cluster.
    pub fn pg_ctl(&self) -> &Path {
        &self.pg_ctl
    }

    /// `createdb`, which creates the `webcam` database.
    pub fn createdb(&self) -> &Path {
        &self.createdb
    }

    /// `postgres`, the server executable itself.
    pub fn postgres(&self) -> &Path {
        &self.postgres
    }
}

/// Make the cached server build available and return the resolved cache paths.
///
/// Safe to call repeatedly: a warm cache is two file-existence checks.
pub async fn ensure_artifacts() -> Result<CachePaths> {
    let paths = CachePaths::discover()?;
    paths.ensure_dirs()?;
    ensure_server(&paths)?;
    Ok(paths)
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

/// First executable file named `name` on `PATH`.
fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
}
