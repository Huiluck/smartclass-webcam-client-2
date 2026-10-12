//! Flutter-facing API of the device client.
//!
//! Every entry point here is a thin bridge over [`scwc_core`] and [`scwc_capture`]: the core
//! owns registration, the session, control and the media path, so this module only translates
//! Dart values into core values, owns the runtime the agent runs on, and streams progress back.
//!
//! # Events
//!
//! The core exposes no hooks, so [`AgentEvent`] is derived from what is observable through its
//! public API alone:
//!
//! - [`AgentEvent::Connecting`] — emitted when a run starts.
//! - [`AgentEvent::Online`] — emitted when the first encoded frame is pulled through the
//!   counting source. The core pulls frames only for a stream the server announced, so that
//!   first frame is the earliest point at which the registration was accepted, the attach
//!   succeeded and a stream is live.
//! - [`AgentEvent::Stats`] — once a second while the agent runs.
//! - [`AgentEvent::Error`] — [`run`] returning an error, which means the client cannot go on
//!   (a configuration that can never work); transport failures are retried inside the core.
//!
//! `Recording`, `RecordingStopped` and `Reconnecting` are part of the API but are not emitted by
//! this milestone: stream ids are assigned inside the core's session and reconnects happen
//! inside its supervisor, and neither is observable through the core's public API. Emitting
//! them would mean inventing ids or attempts. They stay in the enum for the milestone that adds
//! the hooks.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

use flutter_rust_bridge::frb;
use scwc_capture::{CaptureConfig, enumerate_cameras, open};
use scwc_core::{ClientConfig, EncodedFrame, FrameSource, run};
use scwc_protocol::{CameraRegistration, Codec};
use tokio::runtime::Runtime;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

use crate::frb_generated::StreamSink;

/// One camera of the host, as the operator picks it in the UI.
#[derive(Debug)]
pub struct CameraInfoDto {
    /// Stable device identifier, handed back to [`AgentConfig::camera_id`].
    pub id: String,
    /// Human-readable device name.
    pub name: String,
    /// Announced `(resolution, frame rates)` pairs, largest resolution first.
    pub presets: Vec<(String, Vec<u32>)>,
}

/// Everything the UI has to know to start the agent.
#[derive(Debug)]
pub struct AgentConfig {
    /// Server base URL, for example `https://smartclass.example.edu`.
    pub server: String,
    /// Device id the token was issued for.
    pub device_id: String,
    /// Device bearer token (`wdt_...`).
    pub token: String,
    /// Camera to capture from, from [`CameraInfoDto::id`].
    pub camera_id: String,
    /// Capture width in pixels.
    pub width: u32,
    /// Capture height in pixels.
    pub height: u32,
    /// Capture frame rate.
    pub fps: u32,
    /// `h264` or `h265`, exactly as the protocol spells them.
    pub codec: String,
    /// Encoder bitrate in kilobits per second.
    pub bitrate_kbps: u32,
}

/// Progress of the background agent.
#[derive(Debug, Clone)]
pub enum AgentEvent {
    /// A run started and is registering with the server.
    Connecting,
    /// The first frame of a live stream reached the core.
    Online,
    /// The server started a stream over this connection.
    Recording {
        /// Stream the server announced.
        stream_id: String,
    },
    /// The server's stream ended.
    RecordingStopped,
    /// The connection died and the core is registering again.
    Reconnecting {
        /// Reconnect attempt, counting from one.
        attempt: u32,
    },
    /// Frame counters, reported once a second while the agent runs.
    Stats {
        /// Encoded frames handed to the core since the agent started.
        sent_frames: u64,
        /// Frames dropped, which happens inside the core's own media queue.
        dropped_frames: u64,
    },
    /// The agent stopped because the client cannot go on.
    Error {
        /// Human-readable reason.
        message: String,
    },
}

/// Lists the cameras of the host.
///
/// Synchronous and cheap: nothing here opens a capture session. Fails with the platform's own
/// message when the host has no camera or the process may not use one.
#[frb(sync)]
pub fn list_cameras() -> Result<Vec<CameraInfoDto>, String> {
    enumerate_cameras()
        .map(|cameras| {
            cameras
                .into_iter()
                .map(|camera| CameraInfoDto {
                    id: camera.id,
                    name: camera.name,
                    presets: camera.presets,
                })
                .collect()
        })
        .map_err(|error| error.to_string())
}

/// Starts the device client in the background.
///
/// Returns as soon as the agent is scheduled. Fails when an agent already runs, or when the
/// configuration can never work: an empty device id or token, an unparsable server URL, an
/// unsupported codec name, or no camera at all. Everything that can fail later — the camera not
/// opening, the server rejecting the token — arrives as [`AgentEvent::Error`].
pub async fn start_agent(config: AgentConfig) -> Result<(), String> {
    crate::init_logging();
    let codec = parse_codec(&config.codec)?;
    let resolution = format!("{}x{}", config.width, config.height);
    let (supported_resolutions, supported_framerates) =
        capabilities(&config.camera_id, &resolution, config.fps);
    // VideoToolbox, Media Foundation and VAAPI encode both; the configured codec stays first as
    // the preferred one.
    let supported_codec = match codec {
        Codec::H265 => vec![Codec::H265, Codec::H264],
        _ => vec![Codec::H264, Codec::H265],
    };
    let client_config = ClientConfig::new(
        config.server,
        config.device_id,
        config.token,
        vec![CameraRegistration {
            camera_enum: 0,
            resolution,
            fps: config.fps,
            supported_resolutions,
            supported_framerates,
            supported_codec,
            attrs: Default::default(),
        }],
    );
    client_config.validate().map_err(|error| error.to_string())?;

    let capture_config = CaptureConfig {
        camera_id: Some(config.camera_id),
        width: config.width,
        height: config.height,
        fps: config.fps,
        codec,
        bitrate_kbps: config.bitrate_kbps,
    };

    // Started before the slot is reserved, so a runtime that cannot start never leaves the slot
    // looking busy.
    let runtime = runtime()?;

    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let (generation, previous) = {
        let mut slot = AGENT.lock().unwrap_or_else(|error| error.into_inner());
        if slot.shutdown.is_some() {
            return Err("an agent is already running".to_owned());
        }
        slot.generation += 1;
        slot.shutdown = Some(shutdown_tx);
        (slot.generation, slot.task.take())
    };

    // A previous agent that is still winding down may still hold the camera, so wait for it
    // before opening the device again.
    if let Some(task) = previous {
        let _ = task.await;
    }

    let frames = Arc::new(AtomicU64::new(0));
    let task = runtime.spawn(async move {
        emit(AgentEvent::Connecting);
        let stats = tokio::spawn(report_stats(Arc::clone(&frames)));

        // `open` starts the capture session, which can block for as long as the platform takes
        // to bring a camera up (and, on macOS, to ask for camera access).
        let result = match open(capture_config) {
            Ok(source) => {
                let source = CountingSource {
                    inner: source,
                    frames,
                };
                let shutdown = async move {
                    let _ = shutdown_rx.await;
                };
                run(client_config, source, shutdown)
                    .await
                    .map_err(|error| error.to_string())
            }
            Err(error) => Err(format!("could not open the camera: {error}")),
        };

        stats.abort();
        if let Err(message) = result {
            emit(AgentEvent::Error { message });
        }

        let mut slot = AGENT.lock().unwrap_or_else(|error| error.into_inner());
        if slot.generation == generation {
            slot.shutdown = None;
        }
    });

    AGENT
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .task = Some(task);
    Ok(())
}

/// Stops the background agent, if one runs.
///
/// Returns without waiting: the loop notices the signal at its next await point, and a later
/// [`start_agent`] waits for it to wind down before touching the camera again.
pub fn stop_agent() {
    let mut slot = AGENT.lock().unwrap_or_else(|error| error.into_inner());
    if let Some(shutdown) = slot.shutdown.take() {
        let _ = shutdown.send(());
    }
}

/// Subscribes to [`AgentEvent`]s; the returned Dart stream is the agent's progress.
///
/// Only events emitted after the call are delivered, and a subscription that Dart cancelled is
/// dropped at the next event.
pub fn agent_events(sink: StreamSink<AgentEvent>) {
    SUBSCRIBERS
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .push(sink);
}

/// The template's demo API, kept while the UI lane replaces the demo screen.
pub mod simple;

/// The one background agent of this process.
struct AgentSlot {
    /// Set while a run is live; sending on it asks the loop to stop.
    shutdown: Option<oneshot::Sender<()>>,
    /// The most recent run task, so the next start can wait for it to release the camera.
    task: Option<JoinHandle<()>>,
    /// Identifies the run a task belongs to, so a task cannot clear a newer agent's slot.
    generation: u64,
}

static AGENT: Mutex<AgentSlot> = Mutex::new(AgentSlot {
    shutdown: None,
    task: None,
    generation: 0,
});

/// Runtime the agent runs on, shared by every run of this process.
static RUNTIME: LazyLock<Result<Runtime, String>> =
    LazyLock::new(|| Runtime::new().map_err(|error| error.to_string()));

/// Dart streams subscribed to [`agent_events`].
static SUBSCRIBERS: Mutex<Vec<StreamSink<AgentEvent>>> = Mutex::new(Vec::new());

/// Hands `event` to every live subscriber.
///
/// A sink whose Dart subscription is gone rejects the event; that is how dead subscribers are
/// found and dropped.
fn emit(event: AgentEvent) {
    let mut subscribers = SUBSCRIBERS.lock().unwrap_or_else(|error| error.into_inner());
    subscribers.retain(|sink| sink.add(event.clone()).is_ok());
}

/// Reports the frame counter once a second until the run task aborts this task.
///
/// `dropped_frames` is always zero this milestone: the core drops frames in its own media queue
/// and exposes no counter for them.
async fn report_stats(frames: Arc<AtomicU64>) {
    let mut ticker = tokio::time::interval(Duration::from_secs(1));
    // The first tick of an interval is immediate; only the following ones report.
    ticker.tick().await;
    loop {
        ticker.tick().await;
        emit(AgentEvent::Stats {
            sent_frames: frames.load(Ordering::Relaxed),
            dropped_frames: 0,
        });
    }
}

/// The runtime the agent runs on, started on first use.
fn runtime() -> Result<&'static Runtime, String> {
    match &*RUNTIME {
        Ok(runtime) => Ok(runtime),
        Err(message) => Err(message.clone()),
    }
}

/// A [`FrameSource`] that counts the frames the core pulls from it.
///
/// The core asks for a frame only while a stream it was told to record is active, so the first
/// frame through here is the earliest proof that registration, attach and media all work.
struct CountingSource<S> {
    inner: S,
    frames: Arc<AtomicU64>,
}

impl<S: FrameSource> FrameSource for CountingSource<S> {
    async fn next_frame(&mut self) -> EncodedFrame {
        let frame = self.inner.next_frame().await;
        if self.frames.fetch_add(1, Ordering::Relaxed) == 0 {
            emit(AgentEvent::Online);
        }
        frame
    }

    fn set_parameters(
        &mut self,
        resolution: &str,
        fps: u32,
        codec: Codec,
        bitrate_kbps: u32,
    ) -> Result<(), scwc_core::SourceError> {
        self.inner
            .set_parameters(resolution, fps, codec, bitrate_kbps)
    }

    async fn take_photo(&mut self) -> Result<Vec<u8>, scwc_core::SourceError> {
        self.inner.take_photo().await
    }

    fn preferred_codec(&self) -> Codec {
        self.inner.preferred_codec()
    }
}

/// Parses the protocol's codec vocabulary, which is closed and case-sensitive.
fn parse_codec(name: &str) -> Result<Codec, String> {
    match name {
        "h264" => Ok(Codec::H264),
        "h265" => Ok(Codec::H265),
        other => Err(format!(
            "unsupported codec {other:?}: this client encodes h264 or h265"
        )),
    }
}

/// The registration lists announced for the one camera this agent drives.
///
/// The requested pair comes first, followed by everything the camera itself announces, which is
/// the list the server validates a `switch_camera` against. A camera the host cannot enumerate
/// is announced with the requested pair alone.
fn capabilities(camera_id: &str, resolution: &str, fps: u32) -> (Vec<String>, Vec<u32>) {
    let mut resolutions = vec![resolution.to_owned()];
    let mut framerates = vec![fps];

    if let Ok(cameras) = enumerate_cameras() {
        if let Some(camera) = cameras.iter().find(|camera| camera.id == camera_id) {
            for (preset, rates) in &camera.presets {
                if !resolutions.iter().any(|entry| entry == preset) {
                    resolutions.push(preset.clone());
                }
                for rate in rates {
                    if !framerates.contains(rate) {
                        framerates.push(*rate);
                    }
                }
            }
        }
    }

    (resolutions, framerates)
}
