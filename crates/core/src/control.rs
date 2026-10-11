//! Control-channel dispatch: turns server envelopes into device state and `ack`s.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use scwc_protocol::{
    BinaryHeader, CameraRegistration, Codec, Envelope, PhotoPayload, PingPayload, ProtocolError,
    StartRecordingPayload, StatusPayload, StopRecordingPayload, SwitchCameraPayload,
    TakePhotoPayload, encode_frame,
};
use serde_json::json;
use tokio::sync::{Mutex, mpsc, watch};
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::Message;
use tracing::{debug, info, warn};

use crate::clock::now_rfc3339_nano;
use crate::media::{
    self, DropStats, FrameSource, MediaFrame, StreamSpec, default_bitrate_kbps,
};

/// How long a stopped capture task may take to wind down before it is aborted.
const CAPTURE_STOP_TIMEOUT: Duration = Duration::from_secs(1);

/// Parameters a camera is believed to be at; seeded from the registration and moved by
/// `switch_camera`.
#[derive(Debug, Clone)]
struct CameraParameters {
    resolution: String,
    fps: u32,
    codec: Codec,
}

/// A stream the core is currently feeding.
struct ActiveStream {
    camera_enum: u32,
    stream_id: String,
    started: Instant,
    stop: watch::Sender<bool>,
    task: JoinHandle<()>,
}

/// Per-session device state: the announced cameras, the active stream and the outbound
/// queues, plus the command handling that operates on them.
pub(crate) struct ControlState<F: FrameSource> {
    source: Arc<Mutex<F>>,
    cameras: Vec<CameraRegistration>,
    parameters: Vec<CameraParameters>,
    control_tx: mpsc::UnboundedSender<Message>,
    media_tx: mpsc::Sender<MediaFrame>,
    stats: Arc<DropStats>,
    active_camera: Arc<AtomicU32>,
    active: Option<ActiveStream>,
    started: Instant,
}

impl<F: FrameSource + 'static> ControlState<F> {
    /// Builds the state for a fresh session, seeding every camera with the parameters the
    /// registration just announced.
    pub(crate) fn new(
        source: Arc<Mutex<F>>,
        cameras: Vec<CameraRegistration>,
        codec: Codec,
        control_tx: mpsc::UnboundedSender<Message>,
        media_tx: mpsc::Sender<MediaFrame>,
        stats: Arc<DropStats>,
        active_camera: Arc<AtomicU32>,
    ) -> Self {
        let parameters = cameras
            .iter()
            .map(|camera| CameraParameters {
                resolution: camera.resolution.clone(),
                fps: camera.fps,
                codec,
            })
            .collect();

        Self {
            source,
            cameras,
            parameters,
            control_tx,
            media_tx,
            stats,
            active_camera,
            active: None,
            started: Instant::now(),
        }
    }

    /// Handles one server envelope.
    ///
    /// Every command is answered with exactly one `ack` echoing its `id` — except when the
    /// command carries no `id`, which the protocol uses for messages that are not answered.
    /// Unknown message types are logged and ignored, as forward compatibility demands.
    pub(crate) async fn handle(&mut self, envelope: Envelope) {
        match envelope.msg_type.as_str() {
            "ping" => {
                let ts = envelope.payload_as::<PingPayload>().ok().map(|ping| ping.ts);
                self.send(Envelope::pong(ts.as_deref()));
                debug!(?ts, "answered ping with pong");
            }
            "switch_camera" => self.switch_camera(&envelope).await,
            "start_recording" => self.start_recording(&envelope).await,
            "stop_recording" => self.stop_recording(&envelope).await,
            "take_photo" => self.take_photo(&envelope),
            other => debug!(msg_type = other, "ignoring unknown control message"),
        }
    }

    /// Stream id the core may push frames for, if any.
    pub(crate) fn active_stream_id(&self) -> Option<&str> {
        self.active.as_ref().map(|active| active.stream_id.as_str())
    }

    /// A server-agnostic state report, used for keepalives and the periodic drop report.
    pub(crate) fn status(&self) -> StatusPayload {
        let mut payload = StatusPayload::default();
        let fields = &mut payload.fields;
        let state = if self.active.is_some() {
            "recording"
        } else {
            "idle"
        };

        fields.insert("state".to_owned(), json!(state));
        fields.insert(
            "uptime_s".to_owned(),
            json!(self.started.elapsed().as_secs()),
        );
        fields.insert("frames_sent".to_owned(), json!(self.stats.sent_frames()));
        fields.insert(
            "frames_dropped".to_owned(),
            json!(self.stats.dropped_frames()),
        );
        fields.insert("drop_bursts".to_owned(), json!(self.stats.drop_bursts()));
        fields.insert(
            "frames_discarded".to_owned(),
            json!(self.stats.mismatched_frames()),
        );

        if let Some(active) = &self.active {
            fields.insert("camera_enum".to_owned(), json!(active.camera_enum));
            fields.insert("stream_id".to_owned(), json!(active.stream_id));
            fields.insert(
                "stream_uptime_s".to_owned(),
                json!(active.started.elapsed().as_secs()),
            );
        }

        payload
    }

    /// Stops the capture task, if any; called when the session ends.
    pub(crate) async fn shutdown(&mut self) {
        self.stop_active().await;
    }

    /// Handles `switch_camera`: validates the target against the announcement, reconfigures
    /// the source and only then answers `ok`.
    async fn switch_camera(&mut self, envelope: &Envelope) {
        let payload: SwitchCameraPayload = match envelope.payload_as() {
            Ok(payload) => payload,
            Err(error) => {
                self.ack(
                    envelope.id.as_deref(),
                    Err(format!("invalid switch_camera payload: {error}")),
                );
                return;
            }
        };

        let index = payload.camera_enum as usize;
        let Some(camera) = self.cameras.get(index) else {
            self.ack(
                envelope.id.as_deref(),
                Err(format!("camera {} is not registered", payload.camera_enum)),
            );
            return;
        };

        let resolution = payload
            .resolution
            .clone()
            .unwrap_or_else(|| camera.resolution.clone());
        let fps = payload.fps.unwrap_or(camera.fps);
        if !camera.supported_resolutions.contains(&resolution) {
            self.ack(
                envelope.id.as_deref(),
                Err(format!(
                    "resolution {resolution} is not announced for camera {}",
                    payload.camera_enum
                )),
            );
            return;
        }
        if !camera.supported_framerates.contains(&fps) {
            self.ack(
                envelope.id.as_deref(),
                Err(format!(
                    "{fps} fps is not announced for camera {}",
                    payload.camera_enum
                )),
            );
            return;
        }

        let codec = self.parameters[index].codec;
        let bitrate = default_bitrate_kbps(codec);
        let result = {
            let mut source = self.source.lock().await;
            source
                .set_parameters(&resolution, fps, codec, bitrate)
                .map_err(|error| error.message().to_owned())
        };

        match result {
            Ok(()) => {
                self.parameters[index] = CameraParameters {
                    resolution: resolution.clone(),
                    fps,
                    codec,
                };
                self.active_camera.store(payload.camera_enum, Ordering::Relaxed);
                info!(
                    camera_enum = payload.camera_enum,
                    resolution = %resolution,
                    fps,
                    ?codec,
                    "camera switched"
                );
                self.ack(envelope.id.as_deref(), Ok(()));
            }
            Err(error) => self.ack(envelope.id.as_deref(), Err(error)),
        }
    }

    /// Handles `start_recording`: takes over the single capture pipeline and starts feeding
    /// the named stream.
    async fn start_recording(&mut self, envelope: &Envelope) {
        let payload: StartRecordingPayload = match envelope.payload_as() {
            Ok(payload) => payload,
            Err(error) => {
                self.ack(
                    envelope.id.as_deref(),
                    Err(format!("invalid start_recording payload: {error}")),
                );
                return;
            }
        };

        let index = payload.camera_enum as usize;
        if self.cameras.get(index).is_none() {
            self.ack(
                envelope.id.as_deref(),
                Err(format!("camera {} is not registered", payload.camera_enum)),
            );
            return;
        }

        if let Some(active) = &self.active {
            if active.stream_id == payload.stream_id {
                debug!(stream_id = %payload.stream_id, "start_recording repeated for the active stream");
                self.ack(envelope.id.as_deref(), Ok(()));
                return;
            }
            warn!(
                stream_id = %active.stream_id,
                replacement = %payload.stream_id,
                "replacing the active stream: only one capture runs at a time"
            );
        }

        let id = envelope.id.clone();
        self.stop_active().await;

        if let Some(codec) = payload.codec {
            let parameters = self.parameters[index].clone();
            if codec != parameters.codec {
                let bitrate = default_bitrate_kbps(codec);
                let result = {
                    let mut source = self.source.lock().await;
                    source
                        .set_parameters(&parameters.resolution, parameters.fps, codec, bitrate)
                        .map_err(|error| error.message().to_owned())
                };
                match result {
                    Ok(()) => self.parameters[index].codec = codec,
                    Err(error) => {
                        warn!(%error, "could not apply the requested codec");
                        self.ack(id.as_deref(), Err(error));
                        return;
                    }
                }
            }
        }

        let (stop, stop_rx) = watch::channel(false);
        let spec = StreamSpec {
            camera_enum: payload.camera_enum,
            stream_id: payload.stream_id.clone(),
        };
        let task = media::spawn(
            self.source.clone(),
            spec,
            self.media_tx.clone(),
            self.stats.clone(),
            stop_rx,
        );
        self.active = Some(ActiveStream {
            camera_enum: payload.camera_enum,
            stream_id: payload.stream_id.clone(),
            started: Instant::now(),
            stop,
            task,
        });
        self.active_camera.store(payload.camera_enum, Ordering::Relaxed);
        info!(
            camera_enum = payload.camera_enum,
            stream_id = %payload.stream_id,
            codec = ?payload.codec,
            "stream started"
        );
        self.ack(id.as_deref(), Ok(()));
    }

    /// Handles `stop_recording`: stops feeding the named stream.
    async fn stop_recording(&mut self, envelope: &Envelope) {
        let payload: StopRecordingPayload = match envelope.payload_as() {
            Ok(payload) => payload,
            Err(error) => {
                self.ack(
                    envelope.id.as_deref(),
                    Err(format!("invalid stop_recording payload: {error}")),
                );
                return;
            }
        };

        let id = envelope.id.clone();
        let matching = self.active.as_ref().is_some_and(|active| {
            active.camera_enum == payload.camera_enum && active.stream_id == payload.stream_id
        });

        if matching {
            self.stop_active().await;
            self.ack(id.as_deref(), Ok(()));
            return;
        }

        let error = match &self.active {
            Some(active) => format!(
                "stop_recording for stream {} (camera {}) but stream {} (camera {}) is active",
                payload.stream_id, payload.camera_enum, active.stream_id, active.camera_enum
            ),
            None => format!(
                "stop_recording for stream {} but no stream is active",
                payload.stream_id
            ),
        };
        warn!(%error, "ignoring stop_recording for an inactive stream");
        self.ack(id.as_deref(), Err(error));
    }

    /// Handles `take_photo`: accepts the request immediately and uploads the still from a
    /// detached task, so a slow capture never delays the control channel.
    fn take_photo(&mut self, envelope: &Envelope) {
        let payload: TakePhotoPayload = match envelope.payload_as() {
            Ok(payload) => payload,
            Err(error) => {
                self.ack(
                    envelope.id.as_deref(),
                    Err(format!("invalid take_photo payload: {error}")),
                );
                return;
            }
        };

        if self.cameras.get(payload.camera_enum as usize).is_none() {
            self.ack(
                envelope.id.as_deref(),
                Err(format!("camera {} is not registered", payload.camera_enum)),
            );
            return;
        }

        if let Some(active) = &self.active {
            if active.camera_enum != payload.camera_enum {
                warn!(
                    requested = payload.camera_enum,
                    active = active.camera_enum,
                    stream_id = %active.stream_id,
                    "take_photo targets a camera that is not feeding the active stream; the server may discard the image"
                );
            }
        }

        tokio::spawn(capture_photo(
            self.source.clone(),
            self.control_tx.clone(),
            payload.camera_enum,
            payload.request_id,
        ));
        self.ack(envelope.id.as_deref(), Ok(()));
    }

    /// Queues one control envelope for the socket writer.
    fn send(&self, envelope: Result<Envelope, ProtocolError>) {
        let message = match envelope {
            Ok(envelope) => match serde_json::to_string(&envelope) {
                Ok(json) => Message::text(json),
                Err(error) => {
                    warn!(%error, "could not serialize an outgoing control message");
                    return;
                }
            },
            Err(error) => {
                warn!(%error, "could not build an outgoing control message");
                return;
            }
        };

        if self.control_tx.send(message).is_err() {
            debug!("session ended before the control message could be queued");
        }
    }

    /// Answers a command, echoing its `id`; commands without one are not answered.
    fn ack(&self, id: Option<&str>, result: Result<(), String>) {
        let Some(id) = id else {
            debug!("command carried no id; handled without an ack");
            return;
        };

        match result {
            Ok(()) => self.send(Envelope::ack_ok(id)),
            Err(error) => {
                warn!(id, %error, "command failed");
                self.send(Envelope::ack_error(id, error));
            }
        }
    }

    /// Signals the capture task, waits briefly for it and aborts it if it does not wind down.
    async fn stop_active(&mut self) {
        let Some(mut active) = self.active.take() else {
            return;
        };

        debug!(stream_id = %active.stream_id, "stopping capture");
        let _ = active.stop.send(true);

        match tokio::time::timeout(CAPTURE_STOP_TIMEOUT, &mut active.task).await {
            Ok(Ok(())) => info!(stream_id = %active.stream_id, "stream stopped"),
            Ok(Err(error)) => warn!(stream_id = %active.stream_id, %error, "capture task failed"),
            Err(_) => {
                warn!(stream_id = %active.stream_id, "capture task did not stop in time; aborting");
                active.task.abort();
            }
        }
    }
}

/// Captures one still and uploads it on the photo channel.
///
/// Runs detached from the session task: a camera that takes a while to settle must not
/// stall control handling, and the server matches the image by `request_id` whenever it
/// arrives.
async fn capture_photo<F: FrameSource + 'static>(
    source: Arc<Mutex<F>>,
    out: mpsc::UnboundedSender<Message>,
    camera_enum: u32,
    request_id: String,
) {
    let photo = {
        let mut source = source.lock().await;
        source.take_photo().await
    };

    let jpeg = match photo {
        Ok(jpeg) => jpeg,
        Err(error) => {
            warn!(%error, camera_enum, "photo capture failed");
            send_error(
                &out,
                Some(request_id),
                format!("photo capture failed: {error}"),
            );
            return;
        }
    };

    let header = BinaryHeader::photo(PhotoPayload {
        camera_enum,
        request_id: Some(request_id.clone()),
        content_type: None,
        ts: Some(now_rfc3339_nano()),
    });

    match encode_frame(&header, &jpeg) {
        Ok(frame) => {
            if out.send(Message::binary(frame)).is_err() {
                warn!(request_id = %request_id, "session ended before the photo could be queued");
            } else {
                info!(camera_enum, request_id = %request_id, bytes = jpeg.len(), "photo uploaded");
            }
        }
        Err(error) => {
            warn!(%error, %request_id, "photo framing failed");
            send_error(
                &out,
                Some(request_id),
                format!("photo framing failed: {error}"),
            );
        }
    }
}

/// Reports a problem the server cannot learn from an `ack`, correlated to a command when
/// one is known.
fn send_error(out: &mpsc::UnboundedSender<Message>, id: Option<String>, message: String) {
    let Ok(envelope) = Envelope::error(message) else {
        warn!("could not build an error report");
        return;
    };
    let envelope = match id {
        Some(id) => envelope.with_id(id),
        None => envelope,
    };

    match serde_json::to_string(&envelope) {
        Ok(json) => {
            if out.send(Message::text(json)).is_err() {
                debug!("session ended before the error report could be queued");
            }
        }
        Err(error) => warn!(%error, "could not serialize an error report"),
    }
}
