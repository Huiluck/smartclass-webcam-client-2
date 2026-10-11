//! End-to-end test harness for the SmartClass Webcam client.
//!
//! The harness brings up the *real* [`smartclass-webcam-server`] together with a scratch
//! PostgreSQL cluster on the host, and exposes small helpers that integration tests use to
//! provision a device and drive the management API.
//!
//! Host prerequisites:
//!
//! - **PostgreSQL 16 or newer** (`initdb`, `pg_ctl`, `createdb`, `postgres`). The harness only
//!   *locates* these, through `SCWC_PGBIN` when set and otherwise on `PATH`; it never
//!   downloads a database distribution. On macOS, `brew install postgresql@18` is enough.
//! - **git** and **go** for the server build.
//!
//! Everything the harness builds for itself is cached under `<workspace>/.cache`:
//!
//! - `src/` — the cloned server source, stamped with the tag it was cloned at,
//! - `bin/` — the compiled `smartclass-webcam-server`, rebuilt only when the tag changes,
//! - `run/` — per-test PostgreSQL data directories, socket directories and logs.
//!
//! ```no_run
//! # async fn demo() -> anyhow::Result<()> {
//! use scwc_e2e::TestEnv;
//! let env = TestEnv::start().await?;
//! let device = env.provision_device().await?;
//! println!("device {} token {}", device.device_id, device.token);
//! # Ok(())
//! # }
//! ```
//!
//! [`smartclass-webcam-server`]: https://github.com/crazy4chicken/smartclass-webcam-server

#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod api;
mod artifacts;
mod client;
mod env;
mod postgres;
mod process;
mod server;

pub use api::{Device, DeviceCredentials, ManagementApi, Segment, Stream};
pub use artifacts::{CachePaths, PgBin, SERVER_TAG, ensure_artifacts, workspace_root};
pub use client::{ClientOptions, ClientProcess, build_cli, spawn_cli};
pub use env::TestEnv;
pub use postgres::Postgres;
pub use process::{ChildGuard, pick_port};
pub use server::ServerProcess;

use std::sync::LazyLock;

static TRACING: LazyLock<()> = LazyLock::new(|| {
    let filter = std::env::var("RUST_LOG").unwrap_or_else(|_| "info,scwc_e2e=debug".to_string());
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::new(filter))
        .with_test_writer()
        .try_init();
});

/// Install a `tracing-subscriber` logger once for the whole test binary.
///
/// The filter defaults to `info,scwc_e2e=debug` and can be overridden with `RUST_LOG`.
pub fn init_tracing() {
    LazyLock::force(&TRACING);
}
