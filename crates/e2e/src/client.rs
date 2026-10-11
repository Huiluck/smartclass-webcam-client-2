//! The client under test: building `scwc-cli` and running it as a child process.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use tracing::info;

use crate::artifacts::workspace_root;
use crate::process::ChildGuard;

/// Arguments passed to `scwc-cli` for one test run.
#[derive(Debug, Clone)]
pub struct ClientOptions<'a> {
    /// Base URL of the server under test.
    pub server_url: &'a str,
    /// Provisioned device id.
    pub device_id: &'a str,
    /// Provisioned device token.
    pub token: &'a str,
    /// Resolution announced at registration.
    pub resolution: &'a str,
    /// Frame rate announced at registration.
    pub fps: u32,
    /// Encoder bitrate in kbps.
    pub bitrate_kbps: u32,
    /// Optional codec override.
    pub codec: Option<&'a str>,
    /// Additional CLI flags appended verbatim (e.g. `--source camera`).
    pub extra_args: Vec<String>,
}

/// Build `scwc-cli` in the workspace and return the path of the produced binary.
///
/// The binary is located the way the cargo bookkeeping dictates: `CARGO_TARGET_DIR` when
/// set (resolved relative to the workspace root), otherwise `<workspace>/target`.
pub fn build_cli() -> Result<PathBuf> {
    let root = workspace_root()?;
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
    let target_dir = match std::env::var_os("CARGO_TARGET_DIR") {
        Some(dir) => {
            let dir = PathBuf::from(dir);
            if dir.is_absolute() { dir } else { root.join(dir) }
        }
        None => root.join("target"),
    };

    info!("building scwc-cli");
    let mut command = Command::new(&cargo);
    command.current_dir(&root).args(["build", "-p", "scwc-cli"]);
    // `rustup run` puts the toolchain's `cargo` on `CARGO` but not necessarily on `PATH`, so
    // the nested cargo may fail to locate `rustc`. Point it at the sibling binary explicitly.
    if let Some(toolchain_bin) = Path::new(&cargo).parent() {
        let rustc = toolchain_bin.join("rustc");
        if rustc.exists() {
            command.env("RUSTC", rustc);
            if let Ok(joined) = std::env::join_paths(
                std::iter::once(toolchain_bin.to_path_buf())
                    .chain(std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())),
            ) {
                command.env("PATH", joined);
            }
        }
    }
    let output = command
        .output()
        .with_context(|| format!("running {cargo} build -p scwc-cli"))?;
    if !output.status.success() {
        bail!(
            "cargo build -p scwc-cli failed:\n{}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    let binary = target_dir.join("debug").join("scwc-cli");
    if !binary.exists() {
        bail!(
            "cargo build -p scwc-cli succeeded but no binary is at {}",
            binary.display()
        );
    }
    Ok(binary)
}

/// Spawn `scwc-cli` from `build_cli` with the given options, capturing its output to `log`.
pub fn spawn_cli(binary: &Path, options: &ClientOptions<'_>, log: &Path) -> Result<ClientProcess> {
    let stdout = std::fs::File::create(log).with_context(|| format!("creating {}", log.display()))?;
    let stderr = stdout
        .try_clone()
        .with_context(|| format!("duplicating the handle for {}", log.display()))?;

    let mut command = Command::new(binary);
    command
        .arg("--server")
        .arg(options.server_url)
        .arg("--device-id")
        .arg(options.device_id)
        .arg("--token")
        .arg(options.token)
        .arg("--resolution")
        .arg(options.resolution)
        .arg("--fps")
        .arg(options.fps.to_string())
        .arg("--bitrate-kbps")
        .arg(options.bitrate_kbps.to_string());
    if let Some(codec) = options.codec {
        command.arg("--codec").arg(codec);
    }
    command.args(&options.extra_args);
    command
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr));

    info!(binary = %binary.display(), log = %log.display(), "starting scwc-cli");
    let child = command
        .spawn()
        .with_context(|| format!("spawning {}", binary.display()))?;
    Ok(ClientProcess {
        guard: ChildGuard::new("scwc-cli", child),
        log: log.to_path_buf(),
    })
}

/// A running `scwc-cli` child; dropped (and thereby killed) with the test.
pub struct ClientProcess {
    guard: ChildGuard,
    log: PathBuf,
}

impl ClientProcess {
    /// Process id of the client.
    pub fn pid(&self) -> u32 {
        self.guard.id()
    }

    /// Path of the captured client output.
    pub fn log_path(&self) -> &Path {
        &self.log
    }

    /// Stop the client.
    pub fn stop(&mut self) {
        self.guard.stop();
    }
}
