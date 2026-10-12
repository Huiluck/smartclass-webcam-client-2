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
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::{Mutex, mpsc, watch};
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::Message;
use tracing::{debug, info, warn};

use crate::clock::now_rfc3339_nano;
use crate::command::SwitchParams;
use crate::events::{CoreEvent, Emit};
use crate::media::{
    self, DropStats, FrameSource, MediaFrame, StreamSpec, codec_name, default_bitrate_kbps,
};
use crate::params::{ParamStore, PersistedParams};

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

/// A parameter switch that arrived while a stream was running.
///
/// Only the latest command survives: it is applied once, after that stream is gone, so a
/// recording never changes resolution under the server's feet.
struct PendingSwitch {
    /// Camera the switch names; the camera the deferred set belongs to.
    camera_enum: u32,
    /// The set the source is reconfigured with when the stream ends.
    params: PersistedParams,
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
    events: Option<UnboundedSender<CoreEvent>>,
    params: Arc<ParamStore>,
    pending: Option<PendingSwitch>,
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
        events: Option<UnboundedSender<CoreEvent>>,
        params: Arc<ParamStore>,
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
            events,
            params,
            pending: None,
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

    /// Whether the pipeline is currently feeding a stream.
    ///
    /// A stream is what defers a parameter switch and holds a requested reconnect back, so the
    /// session can tell when either has come due.
    pub(crate) fn is_streaming(&self) -> bool {
        self.active.is_some()
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

    /// Handles `switch_camera`: moves the source onto the requested parameters.
    ///
    /// The command is only the server's way in; the request itself travels the one path a
    /// switch takes, shared with the ones an embedding application asks for.
    async fn switch_camera(&mut self, envelope: &Envelope) {
        let payload: SwitchCameraPayload = match envelope.payload_as() {
            Ok(payload) => payload,
            Err(error) => {
                self.reject(
                    envelope.id.as_deref(),
                    format!("invalid switch_camera payload: {error}"),
                );
                return;
            }
        };

        let result = self
            .switch(SwitchParams {
                camera_enum: payload.camera_enum,
                resolution: payload.resolution,
                fps: payload.fps,
                // The protocol's `switch_camera` names no codec and no bitrate: the camera keeps
                // the codec it runs, and the client derives the rate it can afford.
                codec: None,
                bitrate_kbps: None,
            })
            .await;
        self.ack(envelope.id.as_deref(), result);
    }

    /// Validates a parameter switch against the announcement and routes it.
    ///
    /// The one path a switch travels, whoever asked for it: applied at once while no stream
    /// runs, and remembered — latest wins — until the active one ends otherwise, so a recording
    /// keeps the parameters it started with. An applied set is persisted, so the next session
    /// opens with it. The caller decides how the outcome is reported: a server command through
    /// its `ack`, an application request through the event hook.
    pub(crate) async fn switch(&mut self, request: SwitchParams) -> Result<(), String> {
        let index = request.camera_enum as usize;
        let Some(camera) = self.cameras.get(index) else {
            return Err(format!("camera {} is not registered", request.camera_enum));
        };

        let resolution = request
            .resolution
            .clone()
            .unwrap_or_else(|| camera.resolution.clone());
        let fps = request.fps.unwrap_or(camera.fps);
        if !camera.supported_resolutions.contains(&resolution) {
            return Err(format!(
                "resolution {resolution} is not announced for camera {}",
                request.camera_enum
            ));
        }
        if !camera.supported_framerates.contains(&fps) {
            return Err(format!(
                "{fps} fps is not announced for camera {}",
                request.camera_enum
            ));
        }

        let codec = request.codec.unwrap_or(self.parameters[index].codec);
        let bitrate_kbps = request
            .bitrate_kbps
            .unwrap_or_else(|| default_bitrate_kbps(codec, &resolution, fps));
        let params = PersistedParams {
            resolution: resolution.clone(),
            fps,
            codec,
            bitrate_kbps,
        };

        if self.active.is_some() {
            info!(
                camera_enum = request.camera_enum,
                resolution = %resolution,
                fps,
                ?codec,
                bitrate_kbps,
                "a stream is live; applying the parameter switch when it ends"
            );
            self.events.emit(CoreEvent::Notice {
                message: format!(
                    "the parameter switch to {} waits for the end of the recording",
                    describe(&params)
                ),
            });
            self.pending = Some(PendingSwitch {
                camera_enum: request.camera_enum,
                params,
            });
            return Ok(());
        }

        info!(
            camera_enum = request.camera_enum,
            resolution = %resolution,
            fps,
            ?codec,
            bitrate_kbps,
            "camera switched"
        );
        self.apply(request.camera_enum, &params).await
    }

    /// Handles `start_recording`: takes over the single capture pipeline and starts feeding
    /// the named stream.
    async fn start_recording(&mut self, envelope: &Envelope) {
        let payload: StartRecordingPayload = match envelope.payload_as() {
            Ok(payload) => payload,
            Err(error) => {
                self.reject(
                    envelope.id.as_deref(),
                    format!("invalid start_recording payload: {error}"),
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
                let bitrate =
                    default_bitrate_kbps(codec, &parameters.resolution, parameters.fps);
                let params = PersistedParams {
                    resolution: parameters.resolution,
                    fps: parameters.fps,
                    codec,
                    bitrate_kbps: bitrate,
                };
                if let Err(error) = self.apply(payload.camera_enum, &params).await {
                    warn!(%error, "could not apply the requested codec");
                    self.ack(id.as_deref(), Err(error));
                    return;
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

    /// Answers a command that could not even be decoded.
    ///
    /// The server learns about it from the `ack`, the caller from the event hook.
    fn reject(&self, id: Option<&str>, message: String) {
        self.events.emit(CoreEvent::Warn {
            message: message.clone(),
        });
        self.ack(id, Err(message));
    }

    /// Moves the source onto `params`, remembers the set and persists it.
    ///
    /// The source skips a request that matches what it already runs, so calling this with
    /// the parameters the pipeline is on costs nothing. On failure the source and the
    /// server's view of the camera are left unchanged and the reason is returned.
    async fn apply(&mut self, camera_enum: u32, params: &PersistedParams) -> Result<(), String> {
        let result = {
            let mut source = self.source.lock().await;
            source
                .set_parameters(
                    &params.resolution,
                    params.fps,
                    params.codec,
                    params.bitrate_kbps,
                )
                .map_err(|error| error.message().to_owned())
        };

        if let Err(error) = result {
            self.events.emit(CoreEvent::Warn {
                message: format!("could not apply the requested capture parameters: {error}"),
            });
            return Err(error);
        }

        self.parameters[camera_enum as usize] = CameraParameters {
            resolution: params.resolution.clone(),
            fps: params.fps,
            codec: params.codec,
        };
        self.active_camera.store(camera_enum, Ordering::Relaxed);
        self.events.emit(CoreEvent::ParametersApplied {
            camera_enum,
            resolution: params.resolution.clone(),
            fps: params.fps,
            codec: codec_name(params.codec).to_owned(),
            bitrate_kbps: params.bitrate_kbps,
        });
        // The switch is deliberate, so the next session has to start from it instead of
        // from the parameters the registration announces.
        self.params.record_applied(params);
        Ok(())
    }

    /// Applies the parameter set a command deferred while a stream was running.
    ///
    /// The deferred command was acknowledged when it arrived; only the set has to follow,
    /// once, now that the pipeline is free.
    async fn apply_pending(&mut self) {
        let Some(pending) = self.pending.take() else {
            return;
        };

        info!(
            camera_enum = pending.camera_enum,
            resolution = %pending.params.resolution,
            fps = pending.params.fps,
            codec = ?pending.params.codec,
            "applying the parameter switch deferred while streaming"
        );
        if let Err(error) = self.apply(pending.camera_enum, &pending.params).await {
            warn!(%error, "could not apply the parameters deferred while streaming");
        }
    }

    /// Signals the capture task, waits briefly for it and aborts it if it does not wind down.
    ///
    /// Every stream teardown goes through here — an explicit `stop_recording`, a stream
    /// replaced by `start_recording` and the end of the session — so this is also where a
    /// parameter switch deferred during the stream is applied. A session that dies while
    /// recording therefore switches the pipeline as part of its own teardown, before the
    /// next session starts from the persisted set anyway.
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

        self.apply_pending().await;
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

/// Renders a parameter set for a message a user reads.
fn describe(params: &PersistedParams) -> String {
    format!(
        "{} at {} fps, {}, {} kbps",
        params.resolution,
        params.fps,
        codec_name(params.codec),
        params.bitrate_kbps
    )
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

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::atomic::AtomicU32;
    use std::sync::{Arc, Mutex as StdMutex};

    use scwc_protocol::{
        AckPayload, CameraRegistration, Codec, Envelope, StartRecordingPayload,
        StopRecordingPayload, SwitchCameraPayload,
    };
    use tokio::sync::{Mutex, mpsc};
    use tokio_tungstenite::tungstenite::Message;

    use super::ControlState;
    use crate::events::CoreEvent;
    use crate::media::test_support::{Applied, RecordingSource, TempDir};
    use crate::media::{DropStats, MEDIA_QUEUE_CAPACITY, MediaFrame};
    use crate::params::ParamStore;

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

    fn switch_camera(resolution: &str, fps: u32) -> Envelope {
        Envelope::typed(
            "switch_camera",
            &SwitchCameraPayload {
                camera_enum: 0,
                resolution: Some(resolution.to_owned()),
                fps: Some(fps),
            },
        )
        .expect("a switch_camera envelope")
        .with_id("switch")
    }

    fn start_recording(stream_id: &str) -> Envelope {
        Envelope::typed(
            "start_recording",
            &StartRecordingPayload {
                camera_enum: 0,
                stream_id: stream_id.to_owned(),
                codec: None,
            },
        )
        .expect("a start_recording envelope")
        .with_id("start")
    }

    fn stop_recording(stream_id: &str) -> Envelope {
        Envelope::typed(
            "stop_recording",
            &StopRecordingPayload {
                camera_enum: 0,
                stream_id: stream_id.to_owned(),
            },
        )
        .expect("a stop_recording envelope")
        .with_id("stop")
    }

    /// A control state over a [`RecordingSource`], with every queue it writes to kept open.
    struct Harness {
        control: ControlState<RecordingSource>,
        applied: Arc<StdMutex<Vec<Applied>>>,
        acks: mpsc::UnboundedReceiver<Message>,
        _media: mpsc::Receiver<MediaFrame>,
        _events: mpsc::UnboundedReceiver<CoreEvent>,
    }

    impl Harness {
        fn new(state_path: Option<PathBuf>) -> Self {
            let (source, applied) = RecordingSource::new(Codec::H264);
            let (control_tx, acks) = mpsc::unbounded_channel();
            let (media_tx, media) = mpsc::channel(MEDIA_QUEUE_CAPACITY);
            let (events_tx, events) = mpsc::unbounded_channel();
            let control = ControlState::new(
                Arc::new(Mutex::new(source)),
                cameras(),
                Codec::H264,
                control_tx,
                media_tx,
                Arc::new(DropStats::default()),
                Arc::new(AtomicU32::new(0)),
                Some(events_tx),
                Arc::new(ParamStore::new(state_path, None)),
            );

            Self {
                control,
                applied,
                acks,
                _media: media,
                _events: events,
            }
        }

        /// The reconfigurations that reached the source, in order.
        fn applied(&self) -> Vec<Applied> {
            self.applied
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .clone()
        }

        /// The `ack` payloads queued so far, in order.
        fn acks(&mut self) -> Vec<AckPayload> {
            let mut acks = Vec::new();
            while let Ok(message) = self.acks.try_recv() {
                let text = message.to_text().expect("an ack is text");
                let envelope: Envelope = serde_json::from_str(text).expect("a control envelope");
                acks.push(envelope.payload_as::<AckPayload>().expect("an ack payload"));
            }

            acks
        }
    }

    #[tokio::test]
    async fn a_switch_without_a_stream_reaches_the_pipeline_and_the_state_file_at_once() {
        let dir = TempDir::new("idle-switch");
        let path = dir.path().join("capture-params.json");
        let mut harness = Harness::new(Some(path.clone()));

        harness.control.handle(switch_camera("1280x720", 15)).await;

        assert_eq!(
            harness.applied(),
            vec![Applied {
                resolution: "1280x720".to_owned(),
                fps: 15,
                codec: Codec::H264,
                // 1280x720 at 15 fps is 13.8 megapixels per second at H.264's budget.
                bitrate_kbps: 1_480,
            }]
        );
        assert_eq!(harness.acks(), vec![AckPayload::ok()]);

        let persisted = ParamStore::new(Some(path), None)
            .persisted()
            .expect("the applied set is persisted");
        assert_eq!(persisted.resolution, "1280x720");
        assert_eq!(persisted.fps, 15);
    }

    #[tokio::test]
    async fn switching_while_recording_defers_only_the_latest_set_to_the_end() {
        let mut harness = Harness::new(None);

        harness.control.handle(start_recording("stream-1")).await;
        harness.control.handle(switch_camera("1280x720", 15)).await;
        harness.control.handle(switch_camera("1920x1080", 15)).await;
        assert!(
            harness.applied().is_empty(),
            "a live stream keeps the parameters it started with"
        );

        harness.control.handle(stop_recording("stream-1")).await;

        let applied = harness.applied();
        assert_eq!(applied.len(), 1, "the deferred set is applied exactly once");
        assert_eq!(applied[0].resolution, "1920x1080");
        assert_eq!(applied[0].fps, 15);
        assert_eq!(harness.acks(), vec![AckPayload::ok(); 4]);
    }
}
