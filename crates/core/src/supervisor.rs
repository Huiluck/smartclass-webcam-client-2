//! The supervisor: register, attach, run a session, back off, repeat.

use std::any::Any;
use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::AtomicU32;
use std::time::{Duration, Instant};

use futures_util::FutureExt;
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
use crate::preview;
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
/// or protocol failure — and a panic anywhere in the cycle that reaches the server — is
/// retried forever, so the caller does not see it.
///
/// Commands that reach [`ClientConfig::commands`] move the running client: a switch is applied
/// by the session that is live (or the next one, when none is), and a reconnect registers again
/// with the credentials the command carries — immediately, unless a stream is live, in which case
/// it waits for that stream so a recording is never cut short.
///
/// The `source` is moved into capture tasks and therefore has to be `'static`; a
/// [`FrameSource`] that borrows its environment cannot be used here. The source belongs to this
/// call, not to a session: sessions come and go, retry and are replaced while the camera stays
/// open and keeps the parameters it runs, and only the end of this call drops it.
pub async fn run<F, S>(config: ClientConfig, source: F, shutdown: S) -> Result<(), CoreError>
where
    F: FrameSource + 'static,
    S: Future<Output = ()> + Send,
{
    let outcome = cycle(config, source, shutdown).await;

    // The run is over, so nothing captures any more: dropping the preview sink ends the
    // receiver the capture path was writing to, and with it whatever forwards its frames.
    preview::set_preview_sink(None);
    outcome
}

/// The register → attach → session → back off cycle, until the client stops; see [`run`].
async fn cycle<F, S>(mut config: ClientConfig, source: F, shutdown: S) -> Result<(), CoreError>
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

        // One attempt is one session's worth of work — register, attach, run the session — and
        // every failure inside it is that attempt's failure: it is retried from scratch while
        // the source above outlives it. A panic is caught here for the same reason: it is a bug
        // in one attempt, not a reason to stop capturing, and the source is not the attempt's
        // to lose.
        let attempt = match catch_panic(connect(
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
        ))
        .await
        {
            Ok(attempt) => attempt,
            Err(panic) => {
                let reason = format!("the client cycle panicked: {panic}");
                error!(%reason, "abandoning the attempt that panicked");
                config.events.emit(CoreEvent::Warn {
                    message: reason.clone(),
                });
                Attempt::Retry {
                    stable: false,
                    reason,
                }
            }
        };

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

/// Awaits `future`, reporting a panic inside it as its message instead of unwinding.
///
/// One attempt — a registration, an attach, a whole session — shares the client's fate for as
/// long as it runs, so a panic in it would otherwise take down the supervisor, and with it the
/// frame source the caller handed over. Catching it here turns a bug in one attempt into the
/// same thing every other failure of that attempt is: a reason to start the next one.
async fn catch_panic<Fut: Future>(future: Fut) -> Result<Fut::Output, String> {
    match AssertUnwindSafe(future).catch_unwind().await {
        Ok(output) => Ok(output),
        Err(payload) => Err(panic_message(payload.as_ref())),
    }
}

/// The message a panic payload carries, as much of it as a log line can show.
fn panic_message(payload: &(dyn Any + Send)) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        (*message).to_owned()
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else {
        "a panic without a message".to_owned()
    }
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
///
/// The `source` is only *borrowed* here, as it is by the session and its capture tasks: the
/// attempt reads frames out of it and reconfigures it, and nothing on this path can drop it, so
/// a registration that fails, an attach that fails, an upload that fails or a session that dies
/// leaves the camera exactly as it was — open, on the parameters it runs. A parameter switch is
/// the one deliberate exception, and it travels the reset path a session starts with.
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
            // A session that ends takes its recording with it, and the camera must not keep
            // encoding frames for a stream that is gone: `stop_recording` has already disabled
            // it, and this covers the ways a session ends without one — a failure, an abort, a
            // replaced connection.
            source.lock().await.set_encoding(false);
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
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use scwc_protocol::Codec;
    use tokio::net::TcpListener;

    use super::{BACKOFF_FIRST, BACKOFF_MAX, catch_panic, next_delay};
    use crate::media::test_support::RecordingSource;
    use crate::media::{EncodedFrame, FrameSource};
    use crate::{CameraRegistration, ClientConfig, CoreEvent, SourceError};

    /// One camera, announced at 1920x1080@30 in H.264; a client has to announce at least one.
    fn cameras() -> Vec<CameraRegistration> {
        vec![CameraRegistration {
            camera_enum: 0,
            resolution: "1920x1080".to_owned(),
            fps: 30,
            supported_resolutions: vec!["1920x1080".to_owned(), "1280x720".to_owned()],
            supported_framerates: vec![30, 15],
            supported_codec: vec![Codec::H264, Codec::H265],
            attrs: Default::default(),
        }]
    }

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

    #[tokio::test]
    async fn an_attempt_that_panics_is_reported_as_its_message() {
        /// One attempt that fails the way a bug in the session path would.
        async fn explode() -> u32 {
            panic!("the session exploded")
        }

        // The panic a session would hit is caught where its attempt is awaited, and turned into
        // the reason that attempt is retried — never into an unwind that would end the client
        // (and take the frame source down with it).
        assert_eq!(
            catch_panic(explode()).await,
            Err("the session exploded".to_owned())
        );
        assert_eq!(catch_panic(async { 7 }).await, Ok(7));
    }

    /// A [`FrameSource`] that reports its own destruction.
    ///
    /// Dropping a real source is what closes a camera — `CaptureSource::drop` stops the backend
    /// — so counting the drops is how this test observes that a failing attempt did not take the
    /// camera down with it.
    struct Closing<F> {
        source: F,
        closes: Arc<AtomicUsize>,
    }

    impl<F> Drop for Closing<F> {
        fn drop(&mut self) {
            self.closes.fetch_add(1, Ordering::Relaxed);
        }
    }

    impl<F: FrameSource> FrameSource for Closing<F> {
        fn next_frame(&mut self) -> impl Future<Output = EncodedFrame> + Send {
            self.source.next_frame()
        }

        fn set_parameters(
            &mut self,
            resolution: &str,
            fps: u32,
            codec: Codec,
            bitrate_kbps: u32,
        ) -> Result<(), SourceError> {
            self.source
                .set_parameters(resolution, fps, codec, bitrate_kbps)
        }

        async fn take_photo(&mut self) -> Result<Vec<u8>, SourceError> {
            self.source.take_photo().await
        }

        fn set_encoding(&mut self, enabled: bool) {
            self.source.set_encoding(enabled);
        }

        fn preferred_codec(&self) -> Codec {
            self.source.preferred_codec()
        }
    }

    #[tokio::test]
    async fn the_frame_source_stays_open_across_attempts_that_fail() {
        // A listener that accepts and drops every connection: registration cannot succeed, so
        // the client fails the attempt and backs off — the failure this test is about.
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("a local listener");
        let port = listener.local_addr().expect("a bound address").port();
        tokio::spawn(async move {
            while let Ok((socket, _)) = listener.accept().await {
                drop(socket);
            }
        });

        let (source, _applied, _encoding) = RecordingSource::new(Codec::H264);
        let closes = Arc::new(AtomicUsize::new(0));
        let source = Closing {
            source,
            closes: closes.clone(),
        };
        let (events_tx, mut events_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut config = ClientConfig::new(
            format!("http://127.0.0.1:{port}"),
            "camera-01",
            "wdt_test_token",
            cameras(),
        );
        config.events = Some(events_tx);

        let (shutdown_tx, mut shutdown_rx) = tokio::sync::watch::channel(());
        let client = tokio::spawn(super::run(config, source, async move {
            let _ = shutdown_rx.changed().await;
        }));

        // Long enough for the first registration to fail and the backoff to start.
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(
            !client.is_finished(),
            "the client gave up on an attempt it should have retried"
        );
        // The attempt failed (that is what a retry is announced for) and the source is still
        // where it was: open, waiting for the next one.
        let mut retried = false;
        while let Ok(event) = events_rx.try_recv() {
            retried |= matches!(event, CoreEvent::Reconnecting { .. });
        }
        assert!(retried, "the failed attempt was not retried");
        assert_eq!(
            closes.load(Ordering::Relaxed),
            0,
            "a failed attempt must leave the camera open"
        );

        let _ = shutdown_tx.send(());
        client
            .await
            .expect("the client task")
            .expect("a shutdown is not a failure");
        assert_eq!(
            closes.load(Ordering::Relaxed),
            1,
            "the camera closes when the client stops, and only then"
        );
    }
}
