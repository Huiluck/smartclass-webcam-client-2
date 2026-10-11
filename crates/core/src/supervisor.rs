//! The supervisor: register, attach, run a session, back off, repeat.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::AtomicU32;
use std::time::{Duration, Instant};

use tokio::sync::Mutex;
use tokio::time::sleep;
use tracing::{debug, error, info, warn};

use crate::CameraRegistration;
use crate::config::ClientConfig;
use crate::error::CoreError;
use crate::media::{DropStats, FrameSource};
use crate::register;
use crate::session::{self, SessionOutcome};

/// First reconnect delay after a failed attempt or a short-lived session.
const BACKOFF_FIRST: Duration = Duration::from_secs(1);

/// Ceiling of the reconnect backoff.
const BACKOFF_MAX: Duration = Duration::from_secs(30);

/// A session that stayed up this long proves the link is healthy and resets the backoff.
const STABLE_SESSION: Duration = Duration::from_secs(60);

/// Runs the device client until `shutdown` resolves.
///
/// The loop is: register for a ticket, attach, run the session until the socket dies, then
/// restart from registration behind an exponential backoff (1s, doubling, capped at 30s,
/// reset once a session survives [`STABLE_SESSION`]). A ticket is single-use, and a newer
/// connection for the same device may replace this one without a close handshake, so no
/// session is ever resumed — only replaced.
///
/// Returns `Ok(())` once `shutdown` resolves (the connection is closed politely first), and
/// [`CoreError::Config`] when the configuration itself can never work. Any other transport
/// or protocol failure is retried forever, so the caller does not see it.
///
/// The `source` is moved into capture tasks and therefore has to be `'static`; a
/// [`FrameSource`] that borrows its environment cannot be used here.
pub async fn run<F, S>(config: ClientConfig, source: F, shutdown: S) -> Result<(), CoreError>
where
    F: FrameSource + 'static,
    S: Future<Output = ()> + Send,
{
    config.validate()?;

    let http = reqwest::Client::builder().build()?;
    let source = Arc::new(Mutex::new(source));
    let stats = Arc::new(DropStats::default());
    let cameras = config.cameras.clone();
    let active_camera = Arc::new(AtomicU32::new(0));
    let mut backoff = BACKOFF_FIRST;
    tokio::pin!(shutdown);

    info!(
        server = %config.server,
        device_id = %config.device_id,
        cameras = cameras.len(),
        "device client started"
    );

    loop {
        let attempt = connect(
            &http,
            &config,
            &source,
            &cameras,
            &stats,
            &active_camera,
            shutdown.as_mut(),
        )
        .await;

        match attempt {
            Attempt::Shutdown => {
                info!("device client stopped");
                return Ok(());
            }
            Attempt::Fatal(error) => {
                error!(%error, "device client cannot continue");
                return Err(error);
            }
            Attempt::Retry { stable } => {
                let delay = next_delay(&mut backoff, stable);
                warn!(?delay, "waiting before the next registration attempt");
                if !wait(delay, shutdown.as_mut()).await {
                    info!("device client stopped");
                    return Ok(());
                }
            }
        }
    }
}

/// How one register → attach → session cycle ended.
enum Attempt {
    /// The session is gone; start over after a backoff.
    Retry {
        /// `true` when the session outlived [`STABLE_SESSION`].
        stable: bool,
    },
    /// The caller asked the client to stop.
    Shutdown,
    /// The endpoint the server handed out cannot be used; retrying cannot help.
    Fatal(CoreError),
}

/// Runs one registration, attach and session cycle.
async fn connect<F, S>(
    http: &reqwest::Client,
    config: &ClientConfig,
    source: &Arc<Mutex<F>>,
    cameras: &[CameraRegistration],
    stats: &Arc<DropStats>,
    active_camera: &Arc<AtomicU32>,
    mut shutdown: Pin<&mut S>,
) -> Attempt
where
    F: FrameSource + 'static,
    S: Future<Output = ()> + Send,
{
    let registration = match register::register(http, config).await {
        Ok(registration) => registration,
        Err(error) => {
            warn!(%error, "registration failed");
            return Attempt::Retry { stable: false };
        }
    };
    debug!(
        expires_at = %registration.expires_at,
        "ticket issued"
    );

    // The server hands out a root-relative path; the WebSocket scheme and host come from
    // the configured base URL, which already carried the successful registration.
    let url = match register::ws_url(&config.server, &registration.websocket_path) {
        Ok(url) => url,
        Err(error) => return Attempt::Fatal(error),
    };

    let started = Instant::now();
    let outcome = match session::attach(&url).await {
        Ok(ws) => {
            info!("attached; starting session");
            session::run(
                ws,
                source.clone(),
                cameras.to_vec(),
                stats.clone(),
                active_camera.clone(),
                shutdown.as_mut(),
            )
            .await
        }
        Err(error) => {
            warn!(%error, "websocket attach failed");
            SessionOutcome::Closed
        }
    };

    match outcome {
        SessionOutcome::Shutdown => Attempt::Shutdown,
        SessionOutcome::Closed => {
            let uptime = started.elapsed();
            info!(?uptime, "session ended");
            Attempt::Retry {
                stable: uptime >= STABLE_SESSION,
            }
        }
    }
}

/// Next reconnect delay, doubling until [`BACKOFF_MAX`] and resetting after a stable session.
fn next_delay(backoff: &mut Duration, stable: bool) -> Duration {
    if stable {
        *backoff = BACKOFF_FIRST;
        return *backoff;
    }

    let delay = *backoff;
    *backoff = (*backoff * 2).min(BACKOFF_MAX);
    delay
}

/// Sleeps for `delay`, returning `false` when `shutdown` resolved first.
async fn wait<S: Future<Output = ()>>(delay: Duration, mut shutdown: Pin<&mut S>) -> bool {
    tokio::select! {
        _ = sleep(delay) => true,
        _ = shutdown.as_mut() => {
            info!("shutdown requested while waiting for the next attempt");
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{BACKOFF_FIRST, BACKOFF_MAX, next_delay};

    #[test]
    fn backoff_doubles_until_the_cap_and_resets_after_a_stable_session() {
        let mut backoff = BACKOFF_FIRST;
        let mut delays = Vec::new();
        for _ in 0..7 {
            delays.push(next_delay(&mut backoff, false));
        }

        assert_eq!(
            delays,
            vec![
                Duration::from_secs(1),
                Duration::from_secs(2),
                Duration::from_secs(4),
                Duration::from_secs(8),
                Duration::from_secs(16),
                BACKOFF_MAX,
                BACKOFF_MAX,
            ]
        );
        assert_eq!(next_delay(&mut backoff, true), BACKOFF_FIRST);
    }
}
