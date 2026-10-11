//! WebSocket attach and the per-connection session loop.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use scwc_protocol::{CameraRegistration, Envelope, ProtocolError};
use tokio::net::TcpStream;
use tokio::sync::{Mutex, mpsc};
use tokio::time::{MissedTickBehavior, interval};
use tokio_tungstenite::tungstenite::Error as WebSocketError;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};
use tracing::{debug, info, warn};

use crate::CoreError;
use crate::control::ControlState;
use crate::media::{
    DropStats, FrameSource, MediaFrame, MEDIA_QUEUE_CAPACITY, default_bitrate_kbps,
};

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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SessionOutcome {
    /// The connection is gone (close frame, transport error or the server replacing us).
    Closed,
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

/// Runs one connected session until the socket dies or `shutdown` resolves.
///
/// Owns both halves of the socket: incoming frames are dispatched to [`ControlState`],
/// while outgoing messages arrive on two queues — an unbounded one for control traffic and
/// photos, which must never be dropped, and the bounded media queue, whose overflow policy
/// lives in the capture task.
pub(crate) async fn run<F, S>(
    ws: WsStream,
    source: Arc<Mutex<F>>,
    cameras: Vec<CameraRegistration>,
    stats: Arc<DropStats>,
    active_camera: Arc<AtomicU32>,
    mut shutdown: Pin<&mut S>,
) -> SessionOutcome
where
    F: FrameSource + 'static,
    S: Future<Output = ()>,
{
    let (mut sink, mut stream) = ws.split();
    let (control_tx, mut control_rx) = mpsc::unbounded_channel::<Message>();
    let (media_tx, mut media_rx) = mpsc::channel::<MediaFrame>(MEDIA_QUEUE_CAPACITY);

    reset_source(&source, &cameras, &active_camera).await;
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
    );

    let mut keepalive = interval(KEEPALIVE_CHECK);
    keepalive.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut report = interval(DROP_REPORT);
    report.set_missed_tick_behavior(MissedTickBehavior::Delay);
    report.tick().await;

    let mut last_outbound = Instant::now();
    info!("session started");

    let outcome = loop {
        tokio::select! {
            _ = shutdown.as_mut() => {
                info!("shutdown requested; closing the connection");
                let _ = sink.close().await;
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
                    break SessionOutcome::Closed;
                }
                last_outbound = Instant::now();
            }
            Some(frame) = media_rx.recv() => {
                if control.active_stream_id() == Some(frame.stream_id.as_ref()) {
                    if let Err(error) = sink.send(frame.message).await {
                        warn!(%error, "websocket write failed");
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
        }
    };

    control.shutdown().await;
    info!(?outcome, "session ended");
    outcome
}

/// Puts the source back on the parameters the new registration announces.
///
/// `switch_camera` state belongs to the connection that carried the command; once that
/// connection is gone the server believes the camera sits at the configured values again,
/// so the pipeline has to follow.
async fn reset_source<F: FrameSource>(
    source: &Arc<Mutex<F>>,
    cameras: &[CameraRegistration],
    active_camera: &AtomicU32,
) {
    let index = active_camera.load(Ordering::Relaxed) as usize;
    let Some(camera) = cameras.get(index) else {
        return;
    };

    let bitrate;
    let codec;
    let result = {
        let mut source = source.lock().await;
        codec = camera
            .supported_codec
            .first()
            .copied()
            .unwrap_or_else(|| source.preferred_codec());
        bitrate = default_bitrate_kbps(codec);
        source
            .set_parameters(&camera.resolution, camera.fps, codec, bitrate)
            .map_err(|error| error.message().to_owned())
    };

    match result {
        Ok(()) => info!(
            camera_enum = camera.camera_enum,
            resolution = %camera.resolution,
            fps = camera.fps,
            ?codec,
            bitrate_kbps = bitrate,
            "frame source reset to the announced parameters"
        ),
        Err(error) => warn!(
            %error,
            camera_enum = camera.camera_enum,
            "could not reset the frame source to the announced parameters"
        ),
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
