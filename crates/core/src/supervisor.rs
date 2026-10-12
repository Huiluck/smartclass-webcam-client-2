//! The supervisor: register, attach, run a session, back off, repeat.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::AtomicU32;
use std::time::{Duration, Instant};

use tokio::sync::Mutex;
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::time::sleep;
use tracing::{debug, error, info, warn};

use crate::CameraRegistration;
use crate::command::{SwitchParams, UiCommand};
use crate::config::{ClientConfig, ServerConfig};
use crate::error::CoreError;
use crate::events::{CoreEvent, Emit};
use crate::media::{DropStats, FrameSource};
use crate::params::ParamStore;
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
/// Commands that reach [`ClientConfig::commands`] move the running client: a switch is applied
/// by the session that is live (or the next one, when none is), and a reconnect registers again
/// with the credentials the command carries — immediately, unless a stream is live, in which case
/// it waits for that stream so a recording is never cut short.
///
/// The `source` is moved into capture tasks and therefore has to be `'static`; a
/// [`FrameSource`] that borrows its environment cannot be used here.
pub async fn run<F, S>(mut config: ClientConfig, source: F, shutdown: S) -> Result<(), CoreError>
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
    let params = Arc::new(ParamStore::new(
        config.state_path.clone(),
        config.events.clone(),
    ));
    let mut commands = config.commands.take();
    // A switch that arrived while no session was live. Only a session can check a switch against
    // the announcement and route it the way `switch_camera` is routed, so it waits for the next
    // one instead of being dropped.
    let mut queued: Option<SwitchParams> = None;
    let mut backoff = BACKOFF_FIRST;
    tokio::pin!(shutdown);

    info!(
        server = %config.server,
        device_id = %config.device_id,
        cameras = cameras.len(),
        "device client started"
    );

    loop {
        // Between sessions this loop is the consumer of the application's commands: this is how a
        // client that is retrying a server that went away learns about a new one.
        while let Some(command) = try_command(&mut commands) {
            match command {
                UiCommand::Reconnect(server_config) => {
                    replace_server_config(&mut config, server_config)
                }
                UiCommand::SwitchParams(request) => queued = Some(request),
            }
        }

        let attempt = connect(
            &http,
            &config,
            &source,
            &cameras,
            &stats,
            &active_camera,
            &params,
            queued.take(),
            commands.as_mut(),
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
            Attempt::Reconnect(server_config) => {
                // The application asked for it, so there is nothing to wait out.
                replace_server_config(&mut config, server_config);
                backoff = BACKOFF_FIRST;
            }
            Attempt::Retry { stable, reason } => {
                let delay = next_delay(&mut backoff, stable);
                warn!(?delay, %reason, "waiting before the next registration attempt");
                config.events.emit(CoreEvent::Reconnecting {
                    reason,
                    delay_ms: delay.as_millis() as u64,
                });
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
        /// Human-readable reason, reported to the caller as part of
        /// [`CoreEvent::Reconnecting`].
        reason: String,
    },
    /// The application asked the client to register again, with these credentials.
    Reconnect(ServerConfig),
    /// The caller asked the client to stop.
    Shutdown,
    /// The endpoint the server handed out cannot be used; retrying cannot help.
    Fatal(CoreError),
}

/// Takes the next command the application queued, without waiting.
///
/// Used where the client must not block on it: between sessions, before the next attempt.
fn try_command(commands: &mut Option<UnboundedReceiver<UiCommand>>) -> Option<UiCommand> {
    commands.as_mut()?.try_recv().ok()
}

/// Moves the client onto the credentials a [`UiCommand::Reconnect`] carried.
///
/// A request the client cannot use — an unparsable URL, an empty token — is reported and
/// otherwise ignored: the client keeps the credentials it has and carries on, because a bad
/// request is not a reason to stop capturing.
fn replace_server_config(config: &mut ClientConfig, server_config: ServerConfig) {
    if let Err(error) = config.set_server_config(server_config) {
        warn!(%error, "ignoring the server configuration the application asked for");
        config.events.emit(CoreEvent::Warn {
            message: format!("ignoring the requested server configuration: {error}"),
        });
    }
}

/// Runs one registration, attach and session cycle.
#[allow(clippy::too_many_arguments)]
async fn connect<F, S>(
    http: &reqwest::Client,
    config: &ClientConfig,
    source: &Arc<Mutex<F>>,
    cameras: &[CameraRegistration],
    stats: &Arc<DropStats>,
    active_camera: &Arc<AtomicU32>,
    params: &Arc<ParamStore>,
    queued: Option<SwitchParams>,
    commands: Option<&mut UnboundedReceiver<UiCommand>>,
    mut shutdown: Pin<&mut S>,
) -> Attempt
where
    F: FrameSource + 'static,
    S: Future<Output = ()> + Send,
{
    config.events.emit(CoreEvent::Registering);

    let registration = match register::register(http, config).await {
        Ok(registration) => registration,
        Err(error) => {
            warn!(%error, "registration failed");
            let reason = format!("registration failed: {error}");
            config.events.emit(CoreEvent::Warn {
                message: reason.clone(),
            });
            return Attempt::Retry {
                stable: false,
                reason,
            };
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
            config.events.emit(CoreEvent::Attached);
            config.events.emit(CoreEvent::SessionStarted);
            let outcome = session::run(
                ws,
                source.clone(),
                cameras.to_vec(),
                stats.clone(),
                active_camera.clone(),
                config.events.clone(),
                params.clone(),
                queued,
                commands,
                shutdown.as_mut(),
            )
            .await;
            config.events.emit(CoreEvent::SessionEnded {
                reason: format!("{outcome:?}"),
            });
            outcome
        }
        Err(error) => {
            warn!(%error, "websocket attach failed");
            let reason = format!("websocket attach failed: {error}");
            config.events.emit(CoreEvent::Warn {
                message: reason.clone(),
            });
            // No session ever ran on this ticket, so there is no started/ended pair to report.
            return Attempt::Retry {
                stable: false,
                reason,
            };
        }
    };

    match outcome {
        SessionOutcome::Shutdown => Attempt::Shutdown,
        SessionOutcome::Reconnect(server_config) => Attempt::Reconnect(server_config),
        SessionOutcome::Closed => {
            let uptime = started.elapsed();
            info!(?uptime, "session ended");
            Attempt::Retry {
                stable: uptime >= STABLE_SESSION,
                reason: format!("session ended after {uptime:?}"),
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
