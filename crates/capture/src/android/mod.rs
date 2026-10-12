//! Android backend: Camera2 NDK for capture, `AMediaCodec` for encoding.
//!
//! Both APIs are reached through hand-written FFI (see [`camera2`]) so that the crate
//! cross-checks on any host, the same way the Linux backend declares the kernel ABI rather
//! than including `videodev2.h`.
//!
//! # What is different about Android
//!
//! - **Permission is per process.** The platform gates `ACameraManager_openCamera` on the
//!   `CAMERA` permission, which Android grants per application: an installed app gets it from
//!   the user, while on the phone this backend was first exercised against, a native binary
//!   started from `adb shell` was allowed to open the camera too. A run that is refused comes
//!   back as [`CaptureError::PermissionDenied`], not as a camera that never delivers.
//! - **Cameras have no names.** The NDK reports an id and a facing; the friendly name a
//!   user sees in the camera app is a vendor extra. Enumeration therefore builds a name out
//!   of the facing (`"Camera 0 (back)"`), which is what distinguishes the cameras in the
//!   registration the server stores.
//!
//! # State of this backend
//!
//! Everything the core can ask of a camera is wired: enumeration reads the camera's own
//! capability matrix, opening a camera brings up a hardware encoder with the camera writing
//! straight into its input surface, a capture thread moves encoded access units to the core,
//! `switch_camera` tears the stream down and builds it again on the same open device, and
//! `take_photo` submits a one-shot still request to a JPEG reader riding in the same session.
//! Thermal pressure is answered with an encoder bitrate step-down and nothing else, since the
//! protocol lets a device move neither `resolution` nor `fps` without an operator command (see
//! [`thermal`]).
//!
//! What has *not* happened is a frame delivered on hardware. The phone this backend was last
//! exercised against (a vivo V2405A, MediaTek) aborts its own camera HAL — `SIGABRT` inside
//! `processCaptureRequest`, a mutex used after destruction in the vendor's `libvcf_session.so`
//! — on the first request from a native client started through `adb shell`, at every geometry,
//! template and output tried, while the vendor's own camera app streams from the same device
//! without trouble. The media path is therefore written against the NDK headers and compiles
//! and links for `aarch64-linux-android`, and what remains to be proven on hardware is the
//! same path from inside an installed app, whose client identity this vendor framework does
//! recognize.

mod camera2;
mod media;
mod native_window;
mod session;
mod still;
mod thermal;

use std::ffi::c_void;
use std::sync::mpsc::{self, Receiver, Sender, SyncSender};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use scwc_protocol::Codec;
use tracing::{debug, error, info, warn};

use self::camera2::{ACameraDevice_StateCallbacks, LensFacing, Manager, Metadata};
use self::still::StillCapture;
use self::thermal::{ThermalMonitor, ThermalStatus};
use crate::camera_matrix::{fps_by_resolution, fps_ranges};
use crate::error::CaptureError;
use crate::source::{FrameCallback, JpegCallback};
use crate::{CameraInfo, CaptureConfig};

/// The `AIMAGE_FORMAT_*` values an encoder can be fed from.
///
/// `PRIVATE` is the zero-copy path: the camera writes straight into the encoder's input
/// surface. `YUV_420_888` is the fallback a software encoder needs, and is what the
/// capability matrix falls back to when a resolution has no `PRIVATE` entry (some cameras
/// only publish theirs for the sizes their own camera app records).
const VIDEO_FORMATS: [i32; 2] = [camera2::IMAGE_FORMAT_PRIVATE, camera2::IMAGE_FORMAT_YUV_420_888];

/// What one camera says it can stream, read from its static characteristics.
///
/// The list is the camera's *claim*: vendors misreport what their hardware sustains, which
/// is why the plan validates candidate pairs by creating a session before trusting them.
struct CapabilityMatrix {
    /// `(resolution, frame rates)` per resolution, largest resolution first.
    presets: Vec<(String, Vec<u32>)>,
    /// The frame-rate ranges auto-exposure advertises, ascending.
    ae_ranges: Vec<(u32, u32)>,
}

impl CapabilityMatrix {
    /// Reads the matrix out of a camera's characteristics.
    fn read(characteristics: &Metadata) -> Self {
        let ae_ranges = match characteristics.entry(camera2::TAG_CONTROL_AE_AVAILABLE_TARGET_FPS_RANGES)
        {
            Some(entry) => entry.as_i32().map(fps_ranges).unwrap_or_default(),
            None => {
                debug!("the camera advertises no auto-exposure frame-rate ranges");
                Vec::new()
            }
        };

        let presets = match characteristics.entry(camera2::TAG_SCALER_AVAILABLE_MIN_FRAME_DURATIONS) {
            Some(entry) => match entry.as_i64() {
                Some(durations) => fps_by_resolution(durations, &VIDEO_FORMATS),
                None => {
                    warn!("the camera reports frame durations in an unexpected type");
                    Vec::new()
                }
            },
            None => {
                debug!("the camera reports no minimum frame durations");
                Vec::new()
            }
        };

        Self { presets, ae_ranges }
    }

    /// Rejects a request the camera will not accept, and flags one it merely doubts.
    ///
    /// A size the camera does not publish at all cannot start a session, so the request
    /// fails here with the reason. A published size at an unpublished frame rate is only a
    /// warning: durations are a claim about the sensor, and the session created later is
    /// what actually decides.
    fn check(&self, width: u32, height: u32, fps: u32) -> Result<(), CaptureError> {
        if self.presets.is_empty() {
            // The camera published no durations at all; nothing to check against.
            return Ok(());
        }

        let requested = format!("{width}x{height}");
        match self.presets.iter().find(|(preset, _)| preset == &requested) {
            None => {
                let sizes: Vec<&str> = self
                    .presets
                    .iter()
                    .map(|(preset, _)| preset.as_str())
                    .collect();
                warn!(resolution = %requested, available = ?sizes, "the camera does not publish this size");
                Err(CaptureError::UnsupportedParameters)
            }
            Some((_, rates)) if !rates.contains(&fps) => {
                warn!(
                    resolution = %requested,
                    requested = fps,
                    announced = ?rates,
                    "the camera does not publish this frame rate; trying it anyway"
                );
                Ok(())
            }
            Some(_) => Ok(()),
        }
    }
}

/// Lists the cameras of the device.
///
/// A camera whose characteristics cannot be read is still listed, without presets, for the
/// same reason the Windows backend does it: an operator picking a camera is better served
/// by seeing the device than by an enumeration that hides it.
pub(crate) fn enumerate_cameras() -> Result<Vec<CameraInfo>, CaptureError> {
    let manager = Manager::create()?;
    let ids = manager.camera_ids()?;
    if ids.is_empty() {
        return Err(CaptureError::NoCamera);
    }

    let mut cameras = Vec::with_capacity(ids.len());
    let mut denied = false;

    for id in &ids {
        let (name, presets, level) = match manager.characteristics(id) {
            Ok(characteristics) => {
                let matrix = CapabilityMatrix::read(&characteristics);
                (
                    camera_name(id, characteristics.lens_facing()),
                    matrix.presets,
                    characteristics.hardware_level(),
                )
            }
            Err(CaptureError::PermissionDenied) => {
                denied = true;
                (camera_name(id, None), Vec::new(), None)
            }
            Err(error) => {
                debug!(camera = %id, %error, "cannot read this camera's characteristics");
                (camera_name(id, None), Vec::new(), None)
            }
        };

        debug!(
            camera = %id,
            %name,
            ?level,
            resolutions = presets.len(),
            "found a camera"
        );
        cameras.push(CameraInfo {
            id: id.clone(),
            name,
            presets,
        });
    }

    if cameras.is_empty() {
        return Err(if denied {
            CaptureError::PermissionDenied
        } else {
            CaptureError::NoCamera
        });
    }
    Ok(cameras)
}

/// The name shown for a camera the NDK only identifies by number.
fn camera_name(id: &str, facing: Option<LensFacing>) -> String {
    match facing {
        Some(LensFacing::Front) => format!("Camera {id} (front)"),
        Some(LensFacing::Back) => format!("Camera {id} (back)"),
        Some(LensFacing::External) => format!("Camera {id} (external)"),
        None => format!("Camera {id}"),
    }
}

/// The camera a run should open.
///
/// `--camera-id` selects one of the ids [`enumerate_cameras`] reports; without it the back
/// camera wins, because that is the one pointed at a classroom, falling back to whatever
/// the device lists first. A phone exposes several cameras and only the back one is
/// normally wired to the encoder at 4K.
fn resolve_camera_id(manager: &Manager, requested: Option<&str>) -> Result<String, CaptureError> {
    let ids = manager.camera_ids()?;
    if ids.is_empty() {
        return Err(CaptureError::NoCamera);
    }

    if let Some(requested) = requested {
        return ids
            .into_iter()
            .find(|id| id == requested)
            .ok_or_else(|| CaptureError::Os(format!("no camera of this device has id {requested:?}")));
    }

    for id in &ids {
        let is_back = manager
            .characteristics(id)
            .ok()
            .and_then(|characteristics| characteristics.lens_facing())
            == Some(LensFacing::Back);
        if is_back {
            return Ok(id.clone());
        }
    }

    Ok(ids.into_iter().next().expect("checked non-empty above"))
}

/// One camera's capture pipeline, as the core sees it.
///
/// The platform objects live on a thread of their own (see [`pump`]); this handle is the
/// channel to it. That split is what the other backends do too, and here it also keeps every
/// NDK call on one thread, which is the contract the framework is happiest with.
pub(crate) struct Handle {
    /// Requests to the capture thread.
    commands: Sender<Command>,
    /// The capture thread, joined by [`Handle::close`].
    pump: Option<JoinHandle<()>>,
}

// SAFETY: the handle is a channel sender and a join handle, both of which are `Send`; every
// platform object belongs to the capture thread, and the callbacks it registers never touch
// shared state.
unsafe impl Send for Handle {}

/// One request from the owning thread to the capture thread.
enum Command {
    /// Rebuild the pipeline with these capture and encoder parameters.
    SetParameters {
        /// Requested picture width in pixels.
        width: u32,
        /// Requested picture height in pixels.
        height: u32,
        /// Requested frame rate.
        fps: u32,
        /// Codec to encode with.
        codec: Codec,
        /// Target bitrate in kilobits per second.
        bitrate_kbps: u32,
        /// Where the outcome is reported.
        reply: SyncSender<Result<(), CaptureError>>,
    },
    /// Capture one still and deliver it through this callback.
    TakePhoto {
        /// The backend entry point to deliver the still through.
        cb: JpegCallback,
        /// The opaque pointer the callback is invoked with.
        user: SendPtr,
        /// Where the outcome is reported.
        reply: SyncSender<Result<(), CaptureError>>,
    },
    /// Stop capturing and leave the capture thread.
    Stop,
}

/// A `user` pointer on its way to the capture thread.
struct SendPtr(*mut c_void);

// SAFETY: the pointer is the `CallbackState` box `source.rs` keeps alive for the whole life of
// the `CaptureSource`, which outlives this backend; the capture thread only hands it back to
// the callbacks that were registered with it.
unsafe impl Send for SendPtr {}

/// How long the capture thread blocks for an encoded frame before looking at its commands.
///
/// It bounds how long teardown waits for the thread: a shorter timeout costs a wakeup every
/// few milliseconds, a longer one delays a shutdown.
const DEQUEUE_TIMEOUT_US: i64 = 50_000;

/// How long [`Handle::open`] waits for the capture thread to bring the pipeline up.
const STARTUP_TIMEOUT: Duration = Duration::from_secs(10);

/// How long a still capture may take before the photo path gives up.
///
/// The one-shot request has to work its way through the camera's own settling (auto-exposure
/// and focus for the still) before the JPEG is written; five seconds is far past what a phone
/// needs and still bounds how long the capture thread — which stops draining the encoder while
/// it waits — can be held away from the video stream.
const STILL_TIMEOUT: Duration = Duration::from_secs(5);

/// How often the capture thread asks the platform how hot the device is.
///
/// The reading is a binder call into the thermal service, so it is polled at a rate that is
/// irrelevant next to the encoder's own load, not on every loop iteration.
const THERMAL_POLL_INTERVAL: Duration = Duration::from_secs(2);

/// What the capture thread needs to build the pipeline.
struct Setup {
    /// Capture and encoder parameters.
    cfg: CaptureConfig,
    /// Where encoded frames are delivered.
    frame_cb: FrameCallback,
    /// The opaque pointer the callbacks are invoked with.
    user: SendPtr,
}

impl Handle {
    /// Opens a camera and starts capturing.
    ///
    /// The work happens on the capture thread, so a failure there — no permission, an
    /// unsupported geometry, no encoder for the codec — is reported here as the error it was.
    /// Fails with [`CaptureError::PermissionDenied`] when this process does not hold the
    /// `CAMERA` permission, with [`CaptureError::UnsupportedParameters`] when the camera or the
    /// encoder cannot do what was asked, and with [`CaptureError::NoCamera`] when the device has
    /// no camera to open.
    pub(crate) fn open(
        cfg: &CaptureConfig,
        frame_cb: FrameCallback,
        _jpeg_cb: JpegCallback,
        user: *mut c_void,
    ) -> Result<Self, CaptureError> {
        let (commands, requests) = mpsc::channel();
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let setup = Setup {
            cfg: cfg.clone(),
            frame_cb,
            user: SendPtr(user),
        };

        let pump = thread::Builder::new()
            .name("scwc-capture-android".to_owned())
            .spawn(move || pump(setup, requests, ready_tx))
            .map_err(|error| {
                CaptureError::Os(format!("cannot start the capture thread: {error}"))
            })?;

        match ready_rx.recv_timeout(STARTUP_TIMEOUT) {
            Ok(Ok(())) => Ok(Self {
                commands,
                pump: Some(pump),
            }),
            Ok(Err(error)) => {
                let _ = pump.join();
                Err(error)
            }
            Err(error) => {
                let _ = pump.join();
                Err(CaptureError::Os(format!(
                    "the capture thread did not report ready: {error}"
                )))
            }
        }
    }

    /// Reconfigures the pipeline for a `switch_camera` command.
    ///
    /// The session and the encoder cannot change geometry while they run, so the capture thread
    /// tears the stream down and builds a new one on the same open device. Blocks until that
    /// has happened — at most one frame — and reports the camera's answer, which is what the
    /// core turns into `ack {ok: true/false}`.
    pub(crate) fn set_parameters(
        &self,
        width: u32,
        height: u32,
        fps: u32,
        codec: Codec,
        bitrate_kbps: u32,
    ) -> Result<(), CaptureError> {
        let (reply, outcome) = mpsc::sync_channel(1);
        self.send(Command::SetParameters {
            width,
            height,
            fps,
            codec,
            bitrate_kbps,
            reply,
        });
        outcome.recv().unwrap_or(Err(CaptureError::ChannelClosed))
    }

    /// Captures one still, delivered through `jpeg_cb` before this function returns.
    ///
    /// Blocks for the camera's still sequence, which the core allows: it reconfigures and takes
    /// photos at frame boundaries anyway. A photo the camera cannot produce — no JPEG output in
    /// this session — comes back as [`CaptureError::UnsupportedParameters`].
    pub(crate) fn take_photo(
        &self,
        jpeg_cb: JpegCallback,
        user: *mut c_void,
    ) -> Result<(), CaptureError> {
        let (reply, outcome) = mpsc::sync_channel(1);
        self.send(Command::TakePhoto {
            cb: jpeg_cb,
            user: SendPtr(user),
            reply,
        });
        outcome.recv().unwrap_or(Err(CaptureError::ChannelClosed))
    }

    /// Hands a command to the capture thread.
    ///
    /// Nothing wakes the thread: it looks at its channel every
    /// [`DEQUEUE_TIMEOUT_US`](DEQUEUE_TIMEOUT_US) microseconds while waiting for the encoder,
    /// so a command is picked up within that, and the platform keeps every call on the one
    /// thread that owns the objects.
    fn send(&self, command: Command) {
        if self.commands.send(command).is_err() {
            debug!("the capture thread is gone; the command is dropped with it");
        }
    }

    /// Stops capturing and releases the camera.
    ///
    /// Joins the capture thread, which drops the whole pipeline in the order that keeps the
    /// framework's references valid.
    pub(crate) fn close(&mut self) {
        // A send failure means the thread already left, which is not an error here.
        self.send(Command::Stop);
        if let Some(pump) = self.pump.take() {
            let _ = pump.join();
        }
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        self.close();
    }
}

/// The capture thread: builds the pipeline, then moves frames from the encoder to the core.
///
/// Everything the framework holds a pointer to lives in `pipeline` for the whole loop, and is
/// dropped — in declaration order — when the loop ends, after the session has stopped feeding
/// the encoder.
fn pump(setup: Setup, requests: Receiver<Command>, ready: SyncSender<Result<(), CaptureError>>) {
    let mut pipeline = match Pipeline::open(&setup) {
        Ok(pipeline) => pipeline,
        Err(error) => {
            // The opening thread is waiting for exactly this.
            let _ = ready.send(Err(error));
            return;
        }
    };

    if ready.send(Ok(())).is_err() {
        // The opener gave up; the pipeline still has to be torn down, which the drop does.
        return;
    }

    let mut next_thermal_poll = Instant::now() + THERMAL_POLL_INTERVAL;
    loop {
        match requests.try_recv() {
            Ok(Command::Stop) => break,
            Ok(Command::SetParameters {
                width,
                height,
                fps,
                codec,
                bitrate_kbps,
                reply,
            }) => {
                let cfg = CaptureConfig {
                    camera_id: pipeline.cfg.camera_id.clone(),
                    width,
                    height,
                    fps,
                    codec,
                    bitrate_kbps,
                };
                let outcome = pipeline.rebuild(&cfg);
                let _ = reply.send(outcome);
            }
            Ok(Command::TakePhoto { cb, user, reply }) => {
                let outcome = pipeline.take_photo(cb, user.0);
                let _ = reply.send(outcome);
            }
            Err(mpsc::TryRecvError::Empty) => {}
            Err(mpsc::TryRecvError::Disconnected) => break,
        }

        if Instant::now() >= next_thermal_poll {
            next_thermal_poll = Instant::now() + THERMAL_POLL_INTERVAL;
            pipeline.poll_thermal();
        }

        // A pipeline whose last rebuild failed has nothing left to pull frames from; leaving
        // the loop closes the command channel, which is how the source learns capture stopped.
        let Some(stream) = pipeline.stream.as_mut() else {
            warn!("the Android capture stream is down; the client will report no more frames");
            break;
        };

        match stream.encoder.next_access_unit(DEQUEUE_TIMEOUT_US) {
            Ok(Some(unit)) => {
                // SAFETY: `user` is the `CallbackState` box `source.rs` keeps alive for as long
                // as this backend, and the bytes live until `unit` is dropped.
                unsafe {
                    (setup.frame_cb)(
                        setup.user.0,
                        unit.data.as_ptr(),
                        unit.data.len(),
                        u8::from(unit.is_keyframe),
                    )
                };
            }
            // Nothing encoded within the timeout: look at the commands again.
            Ok(None) => {}
            Err(error) => {
                warn!(%error, "the Android encoder stopped producing frames");
                break;
            }
        }
    }

    debug!("leaving the Android capture thread");
}

/// One capture stream: the session, the encoder it feeds, and the still reader alongside them.
///
/// Field order is the drop order: the session has to stop feeding both surfaces before either
/// goes away, the encoder gives its input surface back when it stops, and the still reader owns
/// the window the session's still output pointed at, so it outlives the session as well.
struct Stream {
    /// The running capture session.
    #[allow(dead_code)]
    session: session::Session,
    /// The hardware encoder the session fills.
    encoder: media::Encoder,
    /// The JPEG reader `take_photo` reads from, when the camera accepted one.
    still: Option<StillCapture>,
}

impl Stream {
    /// Brings up the chain: encoder, still reader, session.
    fn start(device: &mut camera2::Device, cfg: &CaptureConfig) -> Result<Self, CaptureError> {
        // The encoder first: the session needs the surface its codec was configured with, and
        // starting the codec before the camera writes into it is what keeps the first frames
        // from arriving at a codec that is not running yet.
        let mut encoder = media::Encoder::create(
            cfg.codec,
            cfg.width,
            cfg.height,
            cfg.fps,
            cfg.bitrate_kbps,
        )?;
        encoder.start()?;

        // The still output is best-effort: a device that cannot bring up a JPEG reader loses
        // `take_photo`, not the stream, and so does a camera that refuses to carry a still
        // output beside the video one (the stream combination is a device property).
        let mut still = match StillCapture::new(cfg.width, cfg.height) {
            Ok(still) => Some(still),
            Err(error) => {
                warn!(%error, "no still output for this Android stream; take_photo will refuse");
                None
            }
        };
        let still_window = still.as_ref().map(StillCapture::window);
        let session = match session::Session::start(
            device,
            encoder.input_surface(),
            still_window,
            cfg.fps,
        ) {
            Ok(session) => session,
            Err(error) if still_window.is_some() => {
                warn!(%error, "the camera refused the still output; rebuilding the session without it");
                still = None;
                session::Session::start(device, encoder.input_surface(), None, cfg.fps)?
            }
            Err(error) => return Err(error),
        };

        Ok(Self {
            session,
            encoder,
            still,
        })
    }
}

/// The camera-side objects the capture thread owns, and the stream running on them.
///
/// Field order is the drop order, and it is not arbitrary: the stream has to stop before the
/// device that drives it is closed, and the manager and the callbacks outlive everything,
/// because the framework keeps pointers to both for as long as the device is open.
struct Pipeline {
    /// The running stream, or `None` while a rebuild is in flight or after one failed.
    stream: Option<Stream>,
    /// The parameters the running stream was configured with.
    cfg: CaptureConfig,
    /// The opened camera.
    #[allow(dead_code)]
    device: camera2::Device,
    /// The callbacks the framework holds for the lifetime of the device.
    #[allow(dead_code)]
    callbacks: Box<ACameraDevice_StateCallbacks>,
    /// The manager the device was opened through.
    #[allow(dead_code)]
    manager: Manager,
    /// The camera's characteristics, kept for the capability check on reconfiguration.
    characteristics: Metadata,
    /// The platform's thermal service, when this device has one.
    thermal: Option<ThermalMonitor>,
    /// The thermal status the encoder's bitrate currently reflects.
    thermal_status: ThermalStatus,
}

impl Pipeline {
    /// Brings up the whole chain: camera, encoder, session.
    fn open(setup: &Setup) -> Result<Self, CaptureError> {
        let cfg = &setup.cfg;
        let manager = Manager::create()?;
        let camera_id = resolve_camera_id(&manager, cfg.camera_id.as_deref())?;
        let characteristics = manager.characteristics(&camera_id)?;
        let matrix = CapabilityMatrix::read(&characteristics);

        info!(
            camera = %camera_id,
            resolutions = matrix.presets.len(),
            ae_ranges = ?matrix.ae_ranges,
            orientation = characteristics.sensor_orientation(),
            level = ?characteristics.hardware_level(),
            "camera capability matrix"
        );
        for (preset, rates) in &matrix.presets {
            debug!(resolution = %preset, framerates = ?rates, "announced stream configuration");
        }
        matrix.check(cfg.width, cfg.height, cfg.fps)?;
        if !matrix.ae_ranges.is_empty() && !matrix.ae_ranges.contains(&(cfg.fps, cfg.fps)) {
            warn!(
                fps = cfg.fps,
                announced = ?matrix.ae_ranges,
                "the camera does not advertise this rate as a constant frame rate"
            );
        }

        // The call that proves the process may use a camera at all.
        let mut callbacks = Box::new(ACameraDevice_StateCallbacks {
            context: std::ptr::null_mut(),
            on_disconnected: Some(on_disconnected),
            on_error: Some(on_error),
        });
        let mut device = manager.open(&camera_id, &mut callbacks)?;
        let stream = Stream::start(&mut device, cfg)?;

        // The thermal service is API 30 and may be absent; without it the encoder simply never
        // throttles, which is the behaviour of every device that predates the service.
        let thermal = match ThermalMonitor::start() {
            Some(monitor) => {
                debug!(status = monitor.status().name(), "thermal monitoring is available");
                Some(monitor)
            }
            None => {
                debug!(
                    "no thermal service on this device; the encoder bitrate will not follow heat"
                );
                None
            }
        };

        let mut pipeline = Self {
            stream: Some(stream),
            cfg: cfg.clone(),
            device,
            callbacks,
            manager,
            characteristics,
            thermal,
            thermal_status: ThermalStatus::Nominal,
        };
        // A device that is already hot starts stepped down rather than waiting for the first
        // two-second poll to notice.
        pipeline.poll_thermal();

        info!(
            camera = %camera_id,
            width = cfg.width,
            height = cfg.height,
            fps = cfg.fps,
            codec = ?cfg.codec,
            "android capture pipeline running"
        );

        Ok(pipeline)
    }

    /// Rebuilds the running stream for `switch_camera`, restoring it if the new parameters fail.
    ///
    /// The camera carries one session at a time, so the running stream goes before the new one
    /// is built. A rebuild that cannot come back up — neither with the new parameters nor with
    /// the previous ones — leaves [`Pipeline::stream`] empty and the capture thread ends, so the
    /// source reports that capture stopped instead of pretending to stream.
    fn rebuild(&mut self, cfg: &CaptureConfig) -> Result<(), CaptureError> {
        // Check the request against what the camera announced before touching the stream: a
        // geometry it never published cannot start a session, and refusing it here keeps the
        // running stream exactly as the server believes it is.
        let matrix = CapabilityMatrix::read(&self.characteristics);
        matrix.check(cfg.width, cfg.height, cfg.fps)?;

        let previous = self.cfg.clone();
        self.stream = None;
        match Stream::start(&mut self.device, cfg) {
            Ok(stream) => {
                self.stream = Some(stream);
                self.cfg = cfg.clone();
                self.apply_thermal_bitrate();
                info!(
                    width = cfg.width,
                    height = cfg.height,
                    fps = cfg.fps,
                    codec = ?cfg.codec,
                    bitrate_kbps = cfg.bitrate_kbps,
                    "android capture reconfigured"
                );
                Ok(())
            }
            Err(error) => {
                warn!(%error, "the requested Android capture parameters failed; restoring the previous stream");
                match Stream::start(&mut self.device, &previous) {
                    Ok(stream) => {
                        self.stream = Some(stream);
                        self.apply_thermal_bitrate();
                    }
                    Err(restore) => {
                        error!(%restore, "the previous Android capture parameters no longer work either");
                    }
                }
                Err(error)
            }
        }
    }

    /// Captures one still and delivers the JPEG through `cb`.
    fn take_photo(&mut self, cb: JpegCallback, user: *mut c_void) -> Result<(), CaptureError> {
        let Some(stream) = self.stream.as_mut() else {
            return Err(CaptureError::Os(
                "the Android capture stream is down".to_owned(),
            ));
        };
        let Stream {
            session,
            still: reader,
            ..
        } = stream;
        let Some(reader) = reader.as_mut() else {
            return Err(CaptureError::UnsupportedParameters);
        };

        // Anything the camera wrote for an earlier photo but nobody collected is dropped first,
        // so the JPEG handed to the callback below is the one this call asked for.
        reader.drain();
        session.capture_still()?;
        let jpeg = reader.take_jpeg(STILL_TIMEOUT)?;
        // SAFETY: `user` is the `CallbackState` box `source.rs` keeps alive for as long as this
        // backend, and the callback copies the bytes before returning.
        unsafe { cb(user, jpeg.as_ptr(), jpeg.len()) };
        debug!(bytes = jpeg.len(), "still delivered");
        Ok(())
    }

    /// Reads the platform's thermal status and steps the encoder's bitrate when it moves.
    fn poll_thermal(&mut self) {
        let status = match self.thermal.as_ref() {
            Some(monitor) => monitor.status(),
            None => return,
        };
        if status == self.thermal_status {
            return;
        }
        info!(
            from = self.thermal_status.name(),
            to = status.name(),
            "the device's thermal status changed"
        );
        self.thermal_status = status;
        self.apply_thermal_bitrate();
    }

    /// Sets the running encoder's bitrate to the configured rate scaled by the thermal status.
    ///
    /// This is the whole thermal policy the protocol leaves a device: `resolution` and `fps`
    /// may not change without an operator `switch_camera`, so heat is paid for in bitrate alone,
    /// and the rate returns to the configured one as soon as the device cools.
    fn apply_thermal_bitrate(&mut self) {
        let percent = self.thermal_status.bitrate_percent();
        let status = self.thermal_status;
        let Some(stream) = self.stream.as_mut() else {
            return;
        };
        let target = self.cfg.bitrate_kbps.saturating_mul(percent) / 100;
        match stream.encoder.set_bitrate(target) {
            Ok(()) => info!(
                bitrate_kbps = target,
                percent,
                status = status.name(),
                "encoder bitrate set for the thermal status"
            ),
            Err(error) => warn!(
                %error,
                status = status.name(),
                "the encoder refused the thermal bitrate change"
            ),
        }
    }
}

/// The framework took the camera away: another client outranks this one, or it was
/// unplugged.
///
/// The device pointer is deliberately unused: this increment registers no shared state, and
/// the callback can still fire while the handle is being dropped, so touching anything
/// through `context` would risk a use-after-free. The pump thread in the next increment
/// owns the real notification.
unsafe extern "C" fn on_disconnected(_context: *mut c_void, _device: *mut camera2::ACameraDevice) {
    warn!("the camera was disconnected; no further frames will arrive");
}

/// The device failed beyond recovery and has to be reopened.
unsafe extern "C" fn on_error(
    _context: *mut c_void,
    _device: *mut camera2::ACameraDevice,
    error: i32,
) {
    warn!(error, "the camera reported a fatal error");
}
