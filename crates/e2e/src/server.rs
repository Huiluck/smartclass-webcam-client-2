//! The server child process: environment, stdout capture, RAII teardown.

use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result};
use tracing::info;

use crate::process::{ChildGuard, pick_port};

/// A spawned `smartclass-webcam-server` instance.
///
/// The server runs in development mode (`WEBCAM_DEV=true`, auth disabled), listens on
/// loopback and uses the discarding no-op storage backend, which is exactly what the M1
/// milestone needs: stream and segment rows are written, media bytes are discarded.
pub struct ServerProcess {
    guard: ChildGuard,
    port: u16,
    log: PathBuf,
}

impl ServerProcess {
    /// Spawn the server on `preferred_port` (falling back to an ephemeral port when taken).
    pub fn spawn(
        binary: &Path,
        run_dir: &Path,
        unique: &str,
        db_url: &str,
        preferred_port: u16,
    ) -> Result<Self> {
        let port = pick_port(preferred_port)?;
        Self::spawn_on(binary, run_dir, unique, db_url, port)
    }

    /// Spawn the server on an exact port, used when restarting onto the same address.
    pub fn spawn_on(
        binary: &Path,
        run_dir: &Path,
        unique: &str,
        db_url: &str,
        port: u16,
    ) -> Result<Self> {
        let log = run_dir.join(format!("server-{unique}.log"));
        let stdout = File::create(&log).with_context(|| format!("creating {}", log.display()))?;
        let stderr = stdout
            .try_clone()
            .with_context(|| format!("duplicating the handle for {}", log.display()))?;

        info!(port, log = %log.display(), "starting smartclass-webcam-server");
        let child = Command::new(binary)
            .env("WEBCAM_DB_URL", db_url)
            .env("WEBCAM_DEV", "true")
            .env("WEBCAM_LISTEN_ADDR", format!("127.0.0.1:{port}"))
            .stdout(Stdio::from(stdout))
            .stderr(Stdio::from(stderr))
            .spawn()
            .with_context(|| format!("spawning {}", binary.display()))?;

        Ok(Self {
            guard: ChildGuard::new("smartclass-webcam-server", child),
            port,
            log,
        })
    }

    /// Base URL of the running server, without a trailing slash.
    pub fn base_url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    /// Port the server listens on.
    pub fn port(&self) -> u16 {
        self.port
    }

    /// Path of the captured stdout/stderr log.
    pub fn log_path(&self) -> &Path {
        &self.log
    }

    /// Stop the server if it is still running.
    pub fn stop(&mut self) {
        self.guard.stop();
    }
}
