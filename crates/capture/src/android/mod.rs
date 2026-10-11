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
//! Capture and encode are wired: enumeration reads the camera's own capability matrix, opening
//! a camera brings up a hardware encoder with the camera writing straight into its input
//! surface, and a capture thread moves encoded access units to the core. Enumeration and the
//! device open have run on a phone (`adb shell`, no app); the media path has not yet — it is
//! written against the NDK headers and compiles, and is the next thing to prove on hardware.
//!
//! Two parts are still refused rather than faked: `switch_camera` reconfiguration, which has to
//! tear the session and the encoder down and build them again, and still capture for
//! `take_photo`, which needs a second output and a JPEG reader.

mod camera2;
mod media;
mod native_window;
mod session;

use std::ffi::c_void;
use std::sync::mpsc::{self, Receiver, Sender, SyncSender};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use scwc_protocol::Codec;
use tracing::{debug, info, warn};

use self::camera2::{ACameraDevice_StateCallbacks, LensFacing, Manager, Metadata};
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
    #[allow(dead_code)]
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
    /// Rebuilding means tearing the session and the encoder down and building them again on the
    /// same device, which is what `switch_camera` needs when it changes resolution, frame rate
    /// or codec. It is the next piece of this backend; until it lands the server is told so
    /// rather than being left to believe the camera changed.
    pub(crate) fn set_parameters(
        &self,
        _width: u32,
        _height: u32,
        _fps: u32,
        _codec: Codec,
        _bitrate_kbps: u32,
    ) -> Result<(), CaptureError> {
        Err(CaptureError::Os(
            "reconfiguring an Android capture is not implemented yet".to_owned(),
        ))
    }

    /// Captures one still.
    pub(crate) fn take_photo(
        &self,
        _jpeg_cb: JpegCallback,
        _user: *mut c_void,
    ) -> Result<(), CaptureError> {
        Err(CaptureError::Os(
            "Android still capture is not implemented yet".to_owned(),
        ))
    }

    /// Stops capturing and releases the camera.
    ///
    /// Joins the capture thread, which drops the whole pipeline in the order that keeps the
    /// framework's references valid.
    pub(crate) fn close(&mut self) {
        // A send failure means the thread already left, which is not an error here.
        let _ = self.commands.send(Command::Stop);
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

    loop {
        match requests.try_recv() {
            Ok(Command::Stop) => break,
            Ok(Command::SetParameters { reply, .. }) => {
                // Unreachable while `Handle::set_parameters` refuses, and honest if it stops
                // refusing before the rebuild exists: better a reported failure than a camera
                // that silently keeps its old geometry.
                let _ = reply.send(Err(CaptureError::Os(
                    "rebuilding an Android capture is not implemented yet".to_owned(),
                )));
            }
            Err(mpsc::TryRecvError::Empty) => {}
            Err(mpsc::TryRecvError::Disconnected) => break,
        }

        match pipeline.encoder.next_access_unit(DEQUEUE_TIMEOUT_US) {
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

/// The camera-side objects the capture thread owns, in teardown order.
///
/// Field order is the drop order, and it is not arbitrary: the session has to stop feeding the
/// encoder before the encoder goes away, the encoder has to release its surface before the
/// device that drives it is closed, and the device has to outlive the session built on it. The
/// manager and the callbacks outlive everything, because the framework keeps pointers to both
/// for as long as the device is open.
///
/// Most of these fields are never read: they are held for what their `Drop` does, in the order
/// above. The ones that are marked are the ones nothing else touches even by name.
struct Pipeline {
    /// The running capture session.
    #[allow(dead_code)]
    session: session::Session,
    /// The hardware encoder the session fills.
    ///
    /// Owns the input surface's reference, which it releases when it stops; the session only
    /// borrows the pointer, which is why the session is declared above and stops feeding the
    /// surface before the encoder gives it back.
    encoder: media::Encoder,
    /// The opened camera.
    #[allow(dead_code)]
    device: camera2::Device,
    /// The callbacks the framework holds for the lifetime of the device.
    #[allow(dead_code)]
    callbacks: Box<ACameraDevice_StateCallbacks>,
    /// The manager the device was opened through.
    #[allow(dead_code)]
    manager: Manager,
    /// The camera's characteristics, kept for the geometry the encoder was configured with.
    #[allow(dead_code)]
    characteristics: Metadata,
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

        let session = session::Session::start(&mut device, encoder.input_surface(), cfg.fps)?;

        info!(
            camera = %camera_id,
            width = cfg.width,
            height = cfg.height,
            fps = cfg.fps,
            codec = ?cfg.codec,
            "android capture pipeline running"
        );

        Ok(Self {
            session,
            encoder,
            device,
            callbacks,
            manager,
            characteristics,
        })
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
