//! Child-process handling shared by the PostgreSQL, server and client helpers.

use std::process::{Child, Command, ExitStatus};
use std::thread::sleep;
use std::time::{Duration, Instant};

use anyhow::Result;
use tracing::{debug, warn};

/// RAII guard that terminates a child process when dropped.
///
/// The guard is what keeps the test environment clean: Rust runs `Drop` during a panic
/// unwind too, so a failing assertion never leaves a server or a client behind. Termination
/// is SIGTERM first, a bounded grace period, then SIGKILL.
pub struct ChildGuard {
    name: String,
    child: Option<Child>,
}

impl ChildGuard {
    /// Wrap an already spawned child.
    pub fn new(name: impl Into<String>, child: Child) -> Self {
        Self {
            name: name.into(),
            child: Some(child),
        }
    }

    /// Process id of the child, or `0` once the child has been reaped.
    pub fn id(&self) -> u32 {
        self.child.as_ref().map(Child::id).unwrap_or(0)
    }

    /// Non-blocking check for termination.
    pub fn try_wait(&mut self) -> Result<Option<ExitStatus>> {
        match self.child.as_mut() {
            Some(child) => Ok(child.try_wait()?),
            None => Ok(None),
        }
    }

    /// Terminate the child: SIGTERM, wait up to five seconds, then SIGKILL.
    pub fn stop(&mut self) {
        let Some(mut child) = self.child.take() else {
            return;
        };

        let pid = child.id();
        let _ = Command::new("kill")
            .arg("-TERM")
            .arg(pid.to_string())
            .status();

        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match child.try_wait() {
                Ok(Some(status)) => {
                    debug!(process = %self.name, pid, %status, "child exited after SIGTERM");
                    return;
                }
                Ok(None) if Instant::now() < deadline => sleep(Duration::from_millis(50)),
                _ => break,
            }
        }

        warn!(process = %self.name, pid, "child ignored SIGTERM; sending SIGKILL");
        let _ = child.kill();
        let _ = child.wait();
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Return `preferred` when it is free on loopback, otherwise an ephemeral port.
///
/// Keeps two concurrently running environments from colliding on a fixed scratch port.
pub fn pick_port(preferred: u16) -> Result<u16> {
    if std::net::TcpListener::bind(("127.0.0.1", preferred)).is_ok() {
        return Ok(preferred);
    }
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0))?;
    Ok(listener.local_addr()?.port())
}
