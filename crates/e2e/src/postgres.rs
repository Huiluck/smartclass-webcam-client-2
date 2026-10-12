//! PostgreSQL bring-up: `initdb`, `pg_ctl start`, `createdb`, and the mirror teardown.
//!
//! The binaries come from the host (see [`PgBin`]); the *cluster* is scratch state owned by
//! one test environment.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use tracing::info;

<<<<<<< HEAD
use crate::artifacts::program_name;
=======
use crate::artifacts::PgBin;
>>>>>>> 44b0fe99a9f7450302d170d22d2b7c100a9e9fdd

/// A running PostgreSQL instance owned by one test environment.
///
/// Dropping it runs `pg_ctl -m immediate stop` and removes its directories, so no postmaster
/// outlives the test. The data directory is wiped and re-initialised on the next start, which
/// keeps runs reproducible.
pub struct Postgres {
    bin: PgBin,
    data_dir: PathBuf,
    socket_dir: PathBuf,
    port: u16,
    log: PathBuf,
}

impl Postgres {
    /// Initialise a fresh data directory under `run_dir` and start a server on a scratch port.
    ///
    /// `unique` keeps two concurrently running environments from sharing a data directory.
    pub fn start(pg_bin: PgBin, run_dir: &Path, unique: &str, preferred_port: u16) -> Result<Self> {
        let data_dir = run_dir.join(format!("pgdata-{unique}"));
        let socket_dir = run_dir.join(format!("pgsock-{unique}"));
        let log = run_dir.join(format!("postgres-{unique}.log"));

        // A previous run may have crashed before its Drop ran; stop that postmaster so the
        // data directory can be wiped.
        if data_dir.join("postmaster.pid").exists() {
            let _ = pg_ctl(pg_bin.pg_ctl(), &data_dir, &log, &["-m", "immediate", "stop"]);
        }
        if data_dir.exists() {
            std::fs::remove_dir_all(&data_dir)
                .with_context(|| format!("removing {}", data_dir.display()))?;
        }
        std::fs::create_dir_all(&data_dir)
            .with_context(|| format!("creating {}", data_dir.display()))?;
        std::fs::create_dir_all(&socket_dir)
            .with_context(|| format!("creating {}", socket_dir.display()))?;

        info!(data = %data_dir.display(), "initialising PostgreSQL");
<<<<<<< HEAD
        let output = Command::new(pg_bin.join(program_name("initdb")))
=======
        let output = Command::new(pg_bin.initdb())
>>>>>>> 44b0fe99a9f7450302d170d22d2b7c100a9e9fdd
            .arg("-D")
            .arg(&data_dir)
            .arg("-U")
            .arg("postgres")
            .arg("-A")
            .arg("trust")
            .arg("-E")
            .arg("UTF8")
            .arg("--no-sync")
            .output()
            .context("running initdb")?;
        if !output.status.success() {
            bail!("initdb failed: {}", stderr(&output));
        }

        let port = crate::process::pick_port(preferred_port)?;
        let options = format!(
            "-p {port} -k {} -c listen_addresses=127.0.0.1",
            socket_dir.display()
        );
        info!(port, "starting PostgreSQL");
<<<<<<< HEAD
        let output = Command::new(pg_bin.join(program_name("pg_ctl")))
=======
        let output = Command::new(pg_bin.pg_ctl())
>>>>>>> 44b0fe99a9f7450302d170d22d2b7c100a9e9fdd
            .arg("start")
            .arg("-D")
            .arg(&data_dir)
            .arg("-l")
            .arg(&log)
            .arg("-o")
            .arg(&options)
            .arg("-w")
            .arg("-t")
            .arg("60")
            .output()
            .context("running pg_ctl start")?;
        if !output.status.success() {
            bail!(
                "pg_ctl start failed: {}\n--- {} ---\n{}",
                stderr(&output),
                log.display(),
                std::fs::read_to_string(&log).unwrap_or_default()
            );
        }

        info!("creating the webcam database");
<<<<<<< HEAD
        let output = Command::new(pg_bin.join(program_name("createdb")))
=======
        let output = Command::new(pg_bin.createdb())
>>>>>>> 44b0fe99a9f7450302d170d22d2b7c100a9e9fdd
            .arg("-h")
            .arg("127.0.0.1")
            .arg("-p")
            .arg(port.to_string())
            .arg("-U")
            .arg("postgres")
            .arg("webcam")
            .output()
            .context("running createdb")?;
        if !output.status.success() {
            bail!("createdb failed: {}", stderr(&output));
        }

        Ok(Self {
            bin: pg_bin,
            data_dir,
            socket_dir,
            port,
            log,
        })
    }

    /// TCP port the instance listens on.
    pub fn port(&self) -> u16 {
        self.port
    }

    /// Connection string for `WEBCAM_DB_URL`; local trust auth needs no password.
    pub fn dsn(&self) -> String {
        format!(
            "postgres://postgres@127.0.0.1:{}/webcam?sslmode=disable",
            self.port
        )
    }

    /// Path of the postmaster log, useful when startup assertions fail.
    pub fn log_path(&self) -> &Path {
        &self.log
    }
}

impl Drop for Postgres {
    fn drop(&mut self) {
        let _ = pg_ctl(
            self.bin.pg_ctl(),
            &self.data_dir,
            &self.log,
            &["-m", "immediate", "stop"],
        );
        // `pg_ctl -w` has waited for the shutdown, so the directories are free to remove.
        let _ = std::fs::remove_dir_all(&self.data_dir);
        let _ = std::fs::remove_dir_all(&self.socket_dir);
    }
}

<<<<<<< HEAD
fn pg_ctl(bin: &Path, data_dir: &Path, log: &Path, action: &[&str]) -> Result<()> {
    let output = Command::new(bin.join(program_name("pg_ctl")))
=======
fn pg_ctl(pg_ctl: &Path, data_dir: &Path, log: &Path, action: &[&str]) -> Result<()> {
    let output = Command::new(pg_ctl)
>>>>>>> 44b0fe99a9f7450302d170d22d2b7c100a9e9fdd
        .arg("-D")
        .arg(data_dir)
        .arg("-l")
        .arg(log)
        .arg("-w")
        .arg("-t")
        .arg("20")
        .args(action)
        .output()
        .context("running pg_ctl")?;
    if !output.status.success() {
        bail!("pg_ctl {action:?} failed: {}", stderr(&output));
    }
    Ok(())
}

fn stderr(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stderr).trim().to_string()
}
