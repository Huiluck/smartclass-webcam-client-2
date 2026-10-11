//! The test environment: cached artifacts plus a live PostgreSQL and server.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use anyhow::{Context, Result};
use tracing::info;

use crate::api::{DeviceCredentials, ManagementApi};
use crate::artifacts::{CachePaths, PgBin, ensure_artifacts};
use crate::client::{ClientOptions, ClientProcess, spawn_cli};
use crate::postgres::Postgres;
use crate::server::ServerProcess;

/// Scratch port for PostgreSQL; an ephemeral port is picked when it is taken.
const PREFERRED_PG_PORT: u16 = 55432;
/// Scratch port for the server; an ephemeral port is picked when it is taken.
const PREFERRED_SERVER_PORT: u16 = 18080;
/// Bound on how long the server may take to answer `/healthz` and reach its database.
const STARTUP_TIMEOUT: Duration = Duration::from_secs(60);

static INSTANCES: AtomicUsize = AtomicUsize::new(0);

/// A fully booted end-to-end environment.
///
/// Dropping it stops the server (SIGTERM, then SIGKILL) and then the database, so nothing
/// outlives the test even when an assertion panics.
pub struct TestEnv {
    paths: CachePaths,
    postgres: Postgres,
    server: ServerProcess,
    api: ManagementApi,
    unique: String,
    db_url: String,
}

impl TestEnv {
    /// Ensure the cached server build exists, cloning and compiling only on a cold cache.
    pub async fn ensure() -> Result<CachePaths> {
        ensure_artifacts().await
    }

    /// Ensure the cached server build, then boot a scratch PostgreSQL cluster from the host's
    /// binaries and the server, waiting until both answer.
    pub async fn start() -> Result<Self> {
        let pg_bin = PgBin::resolve()?;
        let paths = Self::ensure().await?;
        let unique = unique_id();
        let postgres = Postgres::start(pg_bin, &paths.run_dir(), &unique, PREFERRED_PG_PORT)?;
        let db_url = postgres.dsn();
        let server = ServerProcess::spawn(
            &paths.server_bin(),
            &paths.run_dir(),
            &unique,
            &db_url,
            PREFERRED_SERVER_PORT,
        )?;

        let api = ManagementApi::new(server.base_url())?;
        api.wait_healthz(STARTUP_TIMEOUT)
            .await
            .context("waiting for the server's health endpoint")?;
        api.wait_database(STARTUP_TIMEOUT)
            .await
            .context("waiting for the server's database")?;

        info!(base_url = api.base_url(), "test environment is ready");
        Ok(Self {
            paths,
            postgres,
            server,
            api,
            unique,
            db_url,
        })
    }

    /// Management-plane client for the running server.
    pub fn api(&self) -> &ManagementApi {
        &self.api
    }

    /// The server child process.
    pub fn server(&self) -> &ServerProcess {
        &self.server
    }

    /// The PostgreSQL instance.
    pub fn postgres(&self) -> &Postgres {
        &self.postgres
    }

    /// Paths of the artifact cache this environment is built from.
    pub fn paths(&self) -> &CachePaths {
        &self.paths
    }

    /// Register a device named `e2e` and return its id and token.
    pub async fn provision_device(&self) -> Result<DeviceCredentials> {
        self.api.provision_device("e2e").await
    }

    /// Spawn the client under test with 4K60 defaults, logging to the run directory.
    pub fn spawn_client(
        &self,
        binary: &Path,
        credentials: &DeviceCredentials,
        resolution: &str,
        fps: u32,
        bitrate_kbps: u32,
        codec: Option<&str>,
    ) -> Result<ClientProcess> {
        let base_url = self.api.base_url().to_string();
        let options = ClientOptions {
            server_url: &base_url,
            device_id: &credentials.device_id,
            token: &credentials.token,
            resolution,
            fps,
            bitrate_kbps,
            codec,
            extra_args: Vec::new(),
        };
        let log = self.run_dir().join(format!("client-{}.log", self.unique));
        spawn_cli(binary, &options, &log)
    }

    /// Kill the server and start a fresh one on the same port and database.
    ///
    /// The client should notice the drop and re-register; the tests observe that through the
    /// management API's `online` flag.
    pub async fn restart_server(&mut self) -> Result<()> {
        let port = self.server.port();
        self.server.stop();

        info!(port, "restarting the server");
        self.server = ServerProcess::spawn_on(
            &self.paths.server_bin(),
            &self.paths.run_dir(),
            &format!("{}-restart", self.unique),
            &self.db_url,
            port,
        )?;
        self.api
            .wait_healthz(STARTUP_TIMEOUT)
            .await
            .context("waiting for the restarted server's health endpoint")?;
        self.api
            .wait_database(STARTUP_TIMEOUT)
            .await
            .context("waiting for the restarted server's database")?;
        Ok(())
    }

    fn run_dir(&self) -> PathBuf {
        self.paths.run_dir()
    }
}

impl Drop for TestEnv {
    fn drop(&mut self) {
        // Stop the server before the database goes away, so it never observes a dead DB as
        // part of an in-flight request. `Postgres::drop` stops the cluster right after.
        self.server.stop();
    }
}

/// Unique per environment, so concurrently running tests never share directories or logs.
fn unique_id() -> String {
    format!(
        "{}-{}",
        std::process::id(),
        INSTANCES.fetch_add(1, Ordering::Relaxed)
    )
}
