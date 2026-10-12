//! WebSocket attach and the per-connection session loop.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use scwc_protocol::{CameraRegistration, Envelope, ProtocolError};
use tokio::net::TcpStream;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio::sync::{Mutex, mpsc};
use tokio::time::{MissedTickBehavior, interval};
use tokio_tungstenite::tungstenite::Error as WebSocketError;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};
use tracing::{debug, info, warn};

use crate::CoreError;
use crate::command::{SwitchParams, UiCommand};
use crate::config::ServerConfig;
use crate::control::ControlState;
use crate::events::{CoreEvent, Emit};
use crate::media::{
    DropStats, FrameSource, MediaFrame, MEDIA_QUEUE_CAPACITY, codec_name, default_bitrate_kbps,
};
use crate::params::{ParamStore, PersistedParams};

/// WebSocket stream as produced by [`connect_async`]: TCP, wrapped in TLS when the ticket
/// URL is `wss`.
pub(crate) type WsStream = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// How often the session asks whether it should remind the server it is alive.
const KEEPALIVE_CHECK: Duration = Duration::from_secs(5);

/// Outbound silence that triggers a `status` keepalive.
///
/// The server closes a connection after 60 seconds without traffic, so this stays far
/// enough below the deadline that one lost check cannot cost the ticket.
const KEEPALIVE_IDLE: Duration = Duration::from_secs(20);

/// Period of the periodic drop-counter report.
const DROP_REPORT: Duration = Duration::from_secs(30);

/// Why a session ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SessionOutcome {
    /// The connection is gone (close frame, transport error or the server replacing us).
    Closed,
    /// The application asked the client to register again, with these credentials.
    Reconnect(ServerConfig),
    /// The caller asked the core to stop.
    Shutdown,
}

/// Attaches to the ticket URL, upgrading the connection to a WebSocket.
pub(crate) async fn attach(url: &str) -> Result<WsStream, CoreError> {
    match connect_async(url).await {
        Ok((stream, response)) => {
            debug!(status = %response.status(), "websocket upgraded");
            Ok(stream)
        }
        Err(error) => Err(classify(error)),
    }
}

/// Maps a handshake failure to a typed error, keeping the ticket semantics.
fn classify(error: WebSocketError) -> CoreError {
    match &error {
        WebSocketError::Http(response) => {
            let status = response.status().as_u16();
            let detail = response
                .body()
                .as_deref()
                .map(|body| String::from_utf8_lossy(body).trim().to_owned())
                .filter(|body| !body.is_empty())
                .unwrap_or_else(|| format!("HTTP {status}"));

            CoreError::from_status(status, detail)
        }
        _ => CoreError::from(error),
    }
}

/// Runs one connected session until the socket dies, `shutdown` resolves, or the application
/// asks for a reconnect.
///
/// Owns both halves of the socket: incoming frames are dispatched to [`ControlState`],
/// while outgoing messages arrive on two queues — an unbounded one for control traffic and
/// photos, which must never be dropped, and the bounded media queue, whose overflow policy
/// lives in the capture task.
///
/// `queued` is a switch the embedding application asked for while no session was live, and
/// `commands` is where the ones that arrive while this session runs come from. Both travel the
/// path a `switch_camera` command takes.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run<F, S>(
    ws: WsStream,
    source: Arc<Mutex<F>>,
    cameras: Vec<CameraRegistration>,
    stats: Arc<DropStats>,
    active_camera: Arc<AtomicU32>,
    events: Option<UnboundedSender<CoreEvent>>,
    params: Arc<ParamStore>,
    queued: Option<SwitchParams>,
    mut commands: Option<&mut UnboundedReceiver<UiCommand>>,
    mut shutdown: Pin<&mut S>,
) -> SessionOutcome
where
    F: FrameSource + 'static,
    S: Future<Output = ()>,
{
    let (mut sink, mut stream) = ws.split();
    let (control_tx, mut control_rx) = mpsc::unbounded_channel::<Message>();
    let (media_tx, mut media_rx) = mpsc::channel::<MediaFrame>(MEDIA_QUEUE_CAPACITY);

    reset_source(&source, &cameras, &active_camera, &events, &params).await;
    let codec = {
        let source = source.lock().await;
        source.preferred_codec()
    };
    let mut control = ControlState::new(
        source,
        cameras,
        codec,
        control_tx.clone(),
        media_tx,
        stats.clone(),
        active_camera,
        events.clone(),
        params,
    );

    // A switch that arrived while no session was live takes effect here, through the one path a
    // switch travels.
    if let Some(request) = queued {
        request_switch(&mut control, &events, request).await;
    }

    let mut keepalive = interval(KEEPALIVE_CHECK);
    keepalive.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut report = interval(DROP_REPORT);
    report.set_missed_tick_behavior(MissedTickBehavior::Delay);
    report.tick().await;

    // A reconnect the application asked for while a stream was live, waiting for that stream.
    let mut reconnect: Option<ServerConfig> = None;
    let mut last_outbound = Instant::now();
    info!("session started");

    let outcome = loop {
        tokio::select! {
            _ = shutdown.as_mut() => {
                info!("shutdown requested; closing the connection");
                break SessionOutcome::Shutdown;
            }
            incoming = stream.next() => {
                match incoming {
                    Some(Ok(Message::Text(text))) => {
                        match serde_json::from_str::<Envelope>(text.as_str()) {
                            Ok(envelope) => {
                                debug!(
                                    msg_type = %envelope.msg_type,
                                    id = ?envelope.id,
                                    "control message received"
                                );
                                control.handle(envelope).await;
                            }
                            Err(error) => warn!(%error, "ignoring an undecodable control frame"),
                        }
                    }
                    Some(Ok(Message::Close(frame))) => {
                        info!(?frame, "server closed the connection");
                        break SessionOutcome::Closed;
                    }
                    Some(Ok(Message::Binary(_))) => {
                        debug!("ignoring an unexpected binary frame from the server")
                    }
                    Some(Ok(Message::Frame(_))) => debug!("ignoring a raw frame message"),
                    Some(Ok(Message::Ping(_) | Message::Pong(_))) => {
                        debug!("websocket keepalive handled by the transport")
                    }
                    Some(Err(error)) => {
                        warn!(%error, "websocket read failed");
                        events.emit(CoreEvent::Warn {
                            message: format!("websocket read failed: {error}"),
                        });
                        break SessionOutcome::Closed;
                    }
                    None => {
                        info!("websocket stream ended");
                        break SessionOutcome::Closed;
                    }
                }
            }
            Some(message) = control_rx.recv() => {
                if let Err(error) = sink.send(message).await {
                    warn!(%error, "websocket write failed");
                    events.emit(CoreEvent::Warn {
                        message: format!("websocket write failed: {error}"),
                    });
                    break SessionOutcome::Closed;
                }
                last_outbound = Instant::now();
            }
            Some(frame) = media_rx.recv() => {
                if control.active_stream_id() == Some(frame.stream_id.as_ref()) {
                    if let Err(error) = sink.send(frame.message).await {
                        warn!(%error, "websocket write failed");
                        events.emit(CoreEvent::Warn {
                            message: format!("websocket write failed: {error}"),
                        });
                        break SessionOutcome::Closed;
                    }
                    stats.record_sent();
                    last_outbound = Instant::now();
                } else {
                    // The server discards frames for a stream it does not know, without
                    // telling anyone, so the client has to notice and report it here.
                    stats.record_mismatch();
                    warn!(
                        stream_id = %frame.stream_id,
                        "discarded a queued frame whose stream is no longer active"
                    );
                }
            }
            _ = keepalive.tick() => {
                let idle = last_outbound.elapsed();
                if idle >= KEEPALIVE_IDLE {
                    debug!(?idle, "keepalive status");
                    send_status(&control, &control_tx);
                }
            }
            _ = report.tick() => {
                info!(
                    stream_id = ?control.active_stream_id(),
                    frames_sent = stats.sent_frames(),
                    frames_dropped = stats.dropped_frames(),
                    drop_bursts = stats.drop_bursts(),
                    frames_discarded = stats.mismatched_frames(),
                    "media report"
                );
                send_status(&control, &control_tx);
            }
            command = next_command(&mut commands) => {
                match command {
                    UiCommand::SwitchParams(request) => {
                        request_switch(&mut control, &events, request).await;
                    }
                    UiCommand::Reconnect(server_config) => {
                        if control.is_streaming() {
                            info!("a stream is live; reconnecting as the application asked when it ends");
                            events.emit(CoreEvent::Notice {
                                message: "the reconnect waits for the end of the recording"
                                    .to_owned(),
                            });
                            reconnect = Some(server_config);
                        } else {
                            info!("reconnecting as the application asked");
                            break SessionOutcome::Reconnect(server_config);
                        }
                    }
                }
            }
        }

        // A reconnect that waited for the stream is due as soon as that stream is gone, whatever
        // ended it — the server's `stop_recording`, a stream replaced by the next one, or the end
        // of the session itself.
        if !control.is_streaming() {
            if let Some(server_config) = reconnect.take() {
                info!("the stream is gone; reconnecting as the application asked");
                break SessionOutcome::Reconnect(server_config);
            }
        }
    };

    control.shutdown().await;

    // Whatever is still queued — the `ack` of the command that ended the session, a parameter
    // switch that never reached a stream — goes out before the socket is closed.
    while let Ok(message) = control_rx.try_recv() {
        if sink.send(message).await.is_err() {
            break;
        }
    }

    // A socket that died while a reconnect was waiting for a stream cannot serve the application
    // either, so the credentials it asked for are used for the next registration instead.
    let outcome = match (outcome, reconnect) {
        (SessionOutcome::Closed, Some(server_config)) => SessionOutcome::Reconnect(server_config),
        (outcome, _) => outcome,
    };
    if matches!(
        outcome,
        SessionOutcome::Shutdown | SessionOutcome::Reconnect(_)
    ) {
        let _ = sink.close().await;
    }

    info!(?outcome, "session ended");
    outcome
}

/// Waits for the next command from the embedding application.
///
/// A caller that sends none — a headless run — waits forever here, which leaves the branch out
/// of the session loop without disabling it.
async fn next_command(commands: &mut Option<&mut UnboundedReceiver<UiCommand>>) -> UiCommand {
    match commands {
        Some(commands) => match commands.recv().await {
            Some(command) => command,
            // The application dropped its sender: nothing can arrive any more.
            None => std::future::pending().await,
        },
        None => std::future::pending().await,
    }
}

/// Applies a switch the embedding application asked for, reporting a refusal to the event hook.
///
/// A server-issued switch reports itself through its `ack`; an application request has no
/// command to answer, so the reason travels as a warning.
async fn request_switch<F: FrameSource + 'static>(
    control: &mut ControlState<F>,
    events: &Option<UnboundedSender<CoreEvent>>,
    request: SwitchParams,
) {
    if let Err(error) = control.switch(request).await {
        warn!(%error, "could not apply the requested capture parameters");
        events.emit(CoreEvent::Warn {
            message: format!("could not switch the capture parameters: {error}"),
        });
    }
}

/// Puts the source on the parameters a session starts from.
///
/// The order is: the set the pipeline was last deliberately switched to, when the state
/// file has one, then the values the new registration announces, and otherwise the source
/// keeps the parameters it was opened with.
///
/// `switch_camera` state belongs to the connection that carried the command; once that
/// connection is gone the server believes the camera sits at the configured values again —
/// except for a switch, which outlives the connection and is remembered on disk.
async fn reset_source<F: FrameSource>(
    source: &Arc<Mutex<F>>,
    cameras: &[CameraRegistration],
    active_camera: &AtomicU32,
    events: &Option<UnboundedSender<CoreEvent>>,
    params: &ParamStore,
) {
    let index = active_camera.load(Ordering::Relaxed) as usize;
    let Some(camera) = cameras.get(index) else {
        debug!(
            camera_enum = index,
            "no announced camera to reset the frame source for; leaving it as it was opened"
        );
        return;
    };

    let persisted = params.persisted();
    let origin = if persisted.is_some() {
        "persisted"
    } else {
        "announced"
    };

    let (target, unchanged, result) = {
        let mut source = source.lock().await;
        let target = match persisted {
            Some(persisted) => persisted,
            None => {
                let codec = camera
                    .supported_codec
                    .first()
                    .copied()
                    .unwrap_or_else(|| source.preferred_codec());
                PersistedParams {
                    resolution: camera.resolution.clone(),
                    fps: camera.fps,
                    codec,
                    bitrate_kbps: default_bitrate_kbps(codec, &camera.resolution, camera.fps),
                }
            }
        };

        // A request that repeats what the pipeline already runs is skipped inside the
        // source; reporting it as a reset would only add noise on every reconnect.
        let unchanged = params.applied().as_ref() == Some(&target);
        let result = source
            .set_parameters(
                &target.resolution,
                target.fps,
                target.codec,
                target.bitrate_kbps,
            )
            .map_err(|error| error.message().to_owned());

        (target, unchanged, result)
    };

    match result {
        Ok(()) => {
            // Only a command-driven switch is written to disk: persisting the announced
            // defaults would freeze them and hide a later change on the server.
            params.note_applied(&target);

            if unchanged {
                debug!(
                    origin = %origin,
                    camera_enum = camera.camera_enum,
                    resolution = %target.resolution,
                    fps = target.fps,
                    codec = ?target.codec,
                    "frame source already runs the session parameters"
                );
            } else {
                info!(
                    origin = %origin,
                    camera_enum = camera.camera_enum,
                    resolution = %target.resolution,
                    fps = target.fps,
                    codec = ?target.codec,
                    bitrate_kbps = target.bitrate_kbps,
                    "frame source reset to the session parameters"
                );
                // The session opens on a set nobody may have asked for in this process — the
                // persisted one, or the announced defaults — so the caller learns it here.
                events.emit(CoreEvent::ParametersApplied {
                    camera_enum: camera.camera_enum,
                    resolution: target.resolution.clone(),
                    fps: target.fps,
                    codec: codec_name(target.codec).to_owned(),
                    bitrate_kbps: target.bitrate_kbps,
                });
            }
        }
        Err(error) => {
            warn!(
                origin = %origin,
                %error,
                camera_enum = camera.camera_enum,
                "could not reset the frame source"
            );
            events.emit(CoreEvent::Warn {
                message: format!("could not reset the frame source: {error}"),
            });
        }
    }
}

/// Queues a `status` report, carrying the drop counters.
fn send_status<F: FrameSource + 'static>(control: &ControlState<F>, out: &mpsc::UnboundedSender<Message>) {
    let payload = control.status();
    let message = match Envelope::status(&payload)
        .and_then(|envelope| serde_json::to_string(&envelope).map_err(ProtocolError::from))
    {
        Ok(json) => Message::text(json),
        Err(error) => {
            warn!(%error, "could not build a status report");
            return;
        }
    };

    if out.send(message).is_err() {
        debug!("session ended before the status report could be queued");
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::AtomicU32;

    use scwc_protocol::{CameraRegistration, Codec};
    use tokio::sync::Mutex;

    use super::reset_source;
    use crate::media::default_bitrate_kbps;
    use crate::media::test_support::{Applied, RecordingSource, TempDir};
    use crate::params::{ParamStore, PersistedParams};

    /// One camera, announced at 1920x1080@30 in H.264.
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

    #[tokio::test]
    async fn a_persisted_switch_beats_the_announced_parameters() {
        let dir = TempDir::new("session-persisted");
        let path = dir.path().join("capture-params.json");
        let switched = PersistedParams {
            resolution: "1280x720".to_owned(),
            fps: 15,
            codec: Codec::H265,
            bitrate_kbps: default_bitrate_kbps(Codec::H265, "1280x720", 15),
        };
        ParamStore::new(Some(path.clone()), None).record_applied(&switched);

        let (source, applied, _encoding) = RecordingSource::new(Codec::H264);
        let source = Arc::new(Mutex::new(source));
        let store = ParamStore::new(Some(path), None);

        reset_source(&source, &cameras(), &AtomicU32::new(0), &None, &store).await;

        assert_eq!(
            applied
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .clone(),
            vec![Applied {
                resolution: switched.resolution.clone(),
                fps: switched.fps,
                codec: switched.codec,
                bitrate_kbps: switched.bitrate_kbps,
            }]
        );
        assert_eq!(
            store.applied(),
            Some(switched),
            "the set the session starts from is remembered as the pipeline state"
        );

        // A later session with the same state file resets to the same set, which the store
        // already knows is what the pipeline runs.
        reset_source(&source, &cameras(), &AtomicU32::new(0), &None, &store).await;
        assert_eq!(
            applied
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .len(),
            2
        );
    }

    #[tokio::test]
    async fn without_a_state_file_the_announced_parameters_win() {
        let (source, applied, _encoding) = RecordingSource::new(Codec::H264);
        let source = Arc::new(Mutex::new(source));

        reset_source(
            &source,
            &cameras(),
            &AtomicU32::new(0),
            &None,
            &ParamStore::new(None, None),
        )
        .await;

        assert_eq!(
            applied
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .clone(),
            vec![Applied {
                resolution: "1920x1080".to_owned(),
                fps: 30,
                codec: Codec::H264,
                bitrate_kbps: default_bitrate_kbps(Codec::H264, "1920x1080", 30),
            }]
        );
    }
}
