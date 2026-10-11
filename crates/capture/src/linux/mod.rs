//! Linux backend: V4L2 for capture, VAAPI for encoding.
//!
//! Both halves are spoken to directly from Rust — the kernel ABI and the libva ABI are
//! declared by hand in [`v4l2`] and [`vaapi`] — so the crate builds and type-checks on hosts
//! that have neither. What that leaves to the runtime is described below, together with the
//! parts of the behaviour that only hardware can confirm.
//!
//! # What the host needs
//!
//! - **A video node per camera** under `/dev/video*`, readable by the process's user. On a
//!   typical distribution that means membership in the `video` group; without it, opening a
//!   camera fails with [`CaptureError::PermissionDenied`].
//! - **`libva.so.2` and `libva-drm.so.2`** (Debian/Ubuntu: `libva2`, `libva-drm2`) and a
//!   hardware driver for the GPU (`intel-media-va-driver`, `mesa-va-drivers`, the vendor's
//!   own package). They are linked by name; a host without them fails to link, which is the
//!   honest failure mode for a binary that promises hardware encoding.
//! - **A DRM render node** under `/dev/dri/renderD*` the process may open (the `render` group,
//!   `0666` on many distributions, or the same group as the video nodes). The node is the
//!   VAAPI display, so no X11 or Wayland session is involved and the client works from a
//!   service or an SSH session.
//! - **A hardware H.264 or H.265 encode entry point.** A driver that only decodes, or a
//!   machine with no render node, fails `open` with a clear error instead of silently
//!   falling back to software.
//!
//! # What the pipeline does
//!
//! `/dev/video*` is scanned for capture devices, each is asked for its formats, sizes and
//! frame rates, and a camera is identified by its `/dev/v4l/by-id` name (falling back to its
//! sysfs path) so that a camera keeps its identity across replugs. Capture is `mmap`
//! streaming into four driver buffers.
//!
//! Frames are turned into NV12 and encoded with VAAPI into Annex B access units with the
//! parameter sets in front of every IDR — see [`vaapi`] for the encoder's shape. Preferred
//! input formats are NV12, then YUYV, then MJPEG: an MJPEG-only camera (most USB webcams at
//! high resolution) is decoded in software with `jpeg-decoder`, a pure-Rust implementation
//! chosen over libjpeg-turbo because the latter would need a C shim and system headers. That
//! decode is the one place this backend can cost more CPU than the encode; it is bounded by
//! the camera's own frame rate.
//!
//! # Reconfiguration and stills
//!
//! The three entry points [`Handle::set_parameters`], [`Handle::take_photo`] and
//! [`Handle::close`] are requests to the capture thread, which handles them between frames:
//! the thread parks in `poll` on the camera and on a small pipe, and every request writes a
//! byte to that pipe, so a request is served as soon as the thread is free — a
//! reconfiguration does not wait for a frame, and a still is produced within one frame time.
//!
//! [`Handle::set_parameters`] rebuilds the whole pipeline (device, encoder, buffers) with the
//! new parameters, and it builds the replacement *before* dropping the running one, so a
//! camera that cannot deliver the new format leaves the current stream untouched and the
//! error reaches the caller. Stills are taken from the newest frame the capture thread has
//! seen: for a raw source that frame is kept as NV12 and converted to RGB for the JPEG
//! encoder, for an MJPEG source the compressed frame is kept and decoded only when a still is
//! actually requested.
//!
//! # Errors
//!
//! Opening reports [`CaptureError::NoCamera`] when no node is a usable camera,
//! [`CaptureError::PermissionDenied`] when the nodes exist but cannot be opened,
//! [`CaptureError::UnsupportedParameters`] for a format, frame rate or codec the hardware
//! does not offer, and [`CaptureError::Os`] for everything the driver or libva reports by
//! name. Once open, a capture error is logged and retried — a driver that stumbles on one
//! frame should not end a lesson — and only [`ERROR_LIMIT`] consecutive failures stop the
//! thread, which the core then observes as a source that never yields again.
//!
//! # Unverified behaviour
//!
//! This backend was written against the documented kernel and libva ABIs and has not been run
//! on Linux hardware. The parts most likely to need adjustment on a real machine:
//!
//! - **Slice headers.** The driver writes them; this crate supplies picture and slice
//!   parameters and no packed slice header. Should a driver require one, the packed-header
//!   attribute query (see [`vaapi`]) is where it shows up first.
//! - **Packed parameter sets.** The driver is asked to splice `SPS || PPS` (or
//!   `VPS || SPS || PPS`) in front of each IDR, and the query result is checked before the
//!   first frame, but whether each driver emits exactly those bytes is untested here.
//! - **NV12 surfaces.** The two surfaces are created with an explicit NV12 format attribute;
//!   a driver that ignores it and hands back another format would be detected by the
//!   staging-image check in [`vaapi`] rather than producing a wrong picture.
//! - **`VA_RC_CBR`.** The config asks for constant bitrate and falls back to the driver's own
//!   rate control when the combination is rejected; whether the fallback still respects
//!   `bitrate_kbps` depends on the driver.

mod bitstream;
mod convert;
mod vaapi;
mod v4l2;

use std::ffi::c_void;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::sync::mpsc::{self, Receiver, Sender, SyncSender, TryRecvError};
use std::thread::JoinHandle;
use std::time::Duration;

use scwc_protocol::Codec;

use crate::error::CaptureError;
use crate::source::{FrameCallback, JpegCallback};
use crate::{CameraInfo, CaptureConfig};

use self::convert::{
    decode_mjpeg, encode_jpeg, nv12_from_nv12, nv12_from_rgb, nv12_from_yuyv, nv12_len,
    rgb_from_nv12,
};
use self::vaapi::{Codec as VaCodec, Encoder};
use self::v4l2::{Device, PixelFormat};

/// The name the capture thread is given, so it is recognisable in a debugger and in logs.
const CAPTURE_THREAD: &str = "scwc-capture";

/// How long the capture thread waits after a failed frame before trying again.
///
/// A camera that was unplugged keeps its descriptor readable, so without a pause the thread
/// would spin on the failing ioctl and flood the log.
const ERROR_BACKOFF: Duration = Duration::from_millis(100);

/// How many consecutive frame failures end the capture thread.
///
/// Five seconds of nothing usable from the camera: enough to ride out a USB hiccup or a
/// driver reset, short of leaving a thread spinning on a device that is gone for good.
const ERROR_LIMIT: u32 = 50;

// ---------------------------------------------------------------------------------------
// Requests to the capture thread.
// ---------------------------------------------------------------------------------------

/// A pointer to the caller's callback state, moved into the capture thread.
struct SendPtr(*mut c_void);

// SAFETY: the pointer is the `CallbackState` box that the caller keeps alive until its
// backend has stopped, and it is only ever passed back into the callbacks on the capture
// thread, which is where the callbacks expect to be called.
unsafe impl Send for SendPtr {}

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
    /// Encode the newest frame as JPEG and deliver it through this callback.
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

/// A live capture pipeline.
pub(crate) struct Handle {
    /// The channel requests travel on.
    commands: Sender<Command>,
    /// Write end of the pipe that wakes the capture thread.
    wake: OwnedFd,
    /// The capture thread, joined by [`Handle::close`].
    pump: Option<JoinHandle<()>>,
}

// SAFETY: the handle talks to a thread that owns every platform object; the only pointer it
// carries is the caller's opaque callback state, which that thread passes back into the
// callbacks. The handle itself is used from the thread that owns the `CaptureSource`.
unsafe impl Send for Handle {}

impl Handle {
    /// Starts capturing; frames and photos are delivered through `frame_cb`/`jpeg_cb`.
    ///
    /// Returns once the camera is streaming and the encoder is ready. Everything that can
    /// fail — resolving the camera, negotiating the format, opening a render node and
    /// creating surfaces — happens on the capture thread before it reports back, so a broken
    /// pipeline is an error here rather than a silent source that never yields.
    pub(crate) fn open(
        cfg: &CaptureConfig,
        frame_cb: FrameCallback,
        // Photos carry their own callback in `take_photo`, so this one is unused here; it
        // stays in the signature because every backend takes the same pair.
        _jpeg_cb: JpegCallback,
        user: *mut c_void,
    ) -> Result<Self, CaptureError> {
        let (commands, requests) = mpsc::channel();
        let (ready, initialized) = mpsc::sync_channel(1);
        let (read, write) = wake_pipe()?;

        let config = cfg.clone();
        let user = SendPtr(user);
        let pump = std::thread::Builder::new()
            .name(CAPTURE_THREAD.to_owned())
            .spawn(move || match Capture::start(config, requests, read, frame_cb, user) {
                Ok(mut capture) => {
                    // A receiver that went away means the handle is being dropped; the thread
                    // has nobody left to report to and simply ends.
                    let _ = ready.send(Ok(()));
                    capture.run();
                }
                Err(error) => {
                    let _ = ready.send(Err(error));
                }
            })
            .map_err(|error| {
                CaptureError::Os(format!("could not start the capture thread: {error}"))
            })?;

        match initialized.recv() {
            Ok(Ok(())) => Ok(Self {
                commands,
                wake: write,
                pump: Some(pump),
            }),
            Ok(Err(error)) => {
                let _ = pump.join();
                Err(error)
            }
            Err(_) => {
                let _ = pump.join();
                Err(CaptureError::ChannelClosed)
            }
        }
    }

    /// Rebuilds the pipeline with new capture and encoder parameters.
    ///
    /// Blocks until the capture thread has handled the request, which is at most one frame.
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
        })?;
        outcome.recv().unwrap_or(Err(CaptureError::ChannelClosed))
    }

    /// Captures one still, delivered through `jpeg_cb` before this function returns.
    ///
    /// Blocks for the JPEG encode plus the wait for the capture thread, which is the cost the
    /// core already accounts for: it calls this at a frame boundary.
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
        })?;
        outcome.recv().unwrap_or(Err(CaptureError::ChannelClosed))
    }

    /// Hands a request to the capture thread and wakes it.
    fn send(&self, command: Command) -> Result<(), CaptureError> {
        if self.commands.send(command).is_err() {
            return Err(CaptureError::ChannelClosed);
        }
        wake(&self.wake);
        Ok(())
    }

    /// Stops the pipeline and waits for it: no callback can fire afterwards.
    pub(crate) fn close(&mut self) {
        if self.commands.send(Command::Stop).is_ok() {
            wake(&self.wake);
        }
        if let Some(pump) = self.pump.take() {
            let _ = pump.join();
        }
    }
}

// ---------------------------------------------------------------------------------------
// Enumeration.
// ---------------------------------------------------------------------------------------

/// Lists the cameras of the host with the frame rates each format announces.
///
/// Enumeration is best-effort: a node that is not a capture device, or one that vanished or
/// is being claimed by another process, is skipped. A host with video nodes but no usable
/// camera is reported as [`CaptureError::NoCamera`] — unless every node was refused for
/// permission, which is reported as [`CaptureError::PermissionDenied`], because that is the
/// difference between "plug a camera in" and "fix your groups".
pub(crate) fn enumerate_cameras() -> Result<Vec<CameraInfo>, CaptureError> {
    let mut cameras = Vec::new();
    let mut denied = false;

    for node in v4l2::video_nodes() {
        match v4l2::probe(&node) {
            Ok(Some(probed)) => {
                tracing::debug!(node = %node.display(), id = %probed.id, "found a camera");
                cameras.push(CameraInfo {
                    id: probed.id,
                    name: probed.name,
                    presets: probed
                        .formats
                        .into_iter()
                        .map(|(width, height, rates)| (format!("{width}x{height}"), rates))
                        .collect(),
                });
            }
            Ok(None) => {}
            Err(CaptureError::PermissionDenied) => denied = true,
            Err(error) => {
                tracing::debug!(node = %node.display(), %error, "camera enumeration skipped a node");
            }
        }
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

// ---------------------------------------------------------------------------------------
// The capture thread.
// ---------------------------------------------------------------------------------------

/// Everything the capture thread owns.
struct Capture {
    /// The streaming camera.
    device: Device,
    /// The encoder the device's frames are compressed with.
    encoder: Encoder,
    /// Requests from the owning thread.
    commands: Receiver<Command>,
    /// Read end of the pipe every request writes to, which is what wakes the thread.
    wake: OwnedFd,
    /// The newest frame, as tightly packed NV12.
    ///
    /// It is the encoder's input, and for a raw source also the source of a still; for an
    /// MJPEG source it is only needed for the frame being encoded, because the still is
    /// taken from [`Capture::compressed`].
    frame: Vec<u8>,
    /// The newest compressed frame, kept only when the source delivers JPEGs.
    compressed: Vec<u8>,
    /// Reused staging for decoding and colour conversion on the photo path.
    rgb: Vec<u8>,
    /// Reused JPEG output of the photo path.
    jpeg: Vec<u8>,
    /// Reused Annex B output of the encoder.
    encoded: Vec<u8>,
    /// Whether a frame of the current pipeline has arrived, i.e. whether a still can be taken.
    photo_valid: bool,
    /// Backend entry point for encoded frames.
    frame_cb: FrameCallback,
    /// Opaque pointer handed back to the frame callback.
    user: SendPtr,
}

impl Capture {
    /// Opens the camera and the encoder, and reports the pipeline ready.
    fn start(
        config: CaptureConfig,
        commands: Receiver<Command>,
        wake: OwnedFd,
        frame_cb: FrameCallback,
        user: SendPtr,
    ) -> Result<Self, CaptureError> {
        let node = v4l2::resolve_device(config.camera_id.as_deref())?;
        let device = Device::open(&node, config.width, config.height, config.fps)?;
        let codec = va_codec(config.codec)?;
        let encoder = vaapi::open_encoder(
            device.width,
            device.height,
            device.fps,
            codec,
            config.bitrate_kbps,
        )?;

        let frame = vec![0u8; nv12_len(device.width as usize, device.height as usize)];
        tracing::info!(
            node = %node.display(),
            width = device.width,
            height = device.height,
            fps = device.fps,
            format = ?device.format,
            "capture pipeline ready"
        );

        Ok(Self {
            device,
            encoder,
            commands,
            wake,
            frame,
            compressed: Vec::new(),
            rgb: Vec::new(),
            jpeg: Vec::new(),
            encoded: Vec::new(),
            photo_valid: false,
            frame_cb,
            user,
        })
    }

    /// Captures until a request stops the pipeline or the camera stops delivering.
    fn run(&mut self) {
        let mut failures = 0;
        loop {
            // Requests first: a reconfiguration and a still must not wait for a frame.
            if !self.handle_commands() {
                break;
            }
            match self.next_frame() {
                Ok(()) => failures = 0,
                Err(error) => {
                    failures += 1;
                    if failures == 1 {
                        tracing::warn!(%error, "capturing a frame failed");
                    } else {
                        tracing::debug!(%error, failures, "capturing a frame still fails");
                    }
                    if failures >= ERROR_LIMIT {
                        tracing::error!(
                            failures,
                            "giving up on the camera: it has not delivered a frame in five seconds"
                        );
                        break;
                    }
                    std::thread::sleep(ERROR_BACKOFF);
                }
            }
        }
    }

    /// Handles every queued request.
    ///
    /// Returns whether the pipeline should keep capturing.
    fn handle_commands(&mut self) -> bool {
        // Every queued request wrote a byte to the pipe; draining it here is what lets the
        // next wait block again instead of returning immediately.
        drain(&self.wake);
        loop {
            match self.commands.try_recv() {
                Ok(Command::SetParameters {
                    width,
                    height,
                    fps,
                    codec,
                    bitrate_kbps,
                    reply,
                }) => {
                    let outcome = self.rebuild(width, height, fps, codec, bitrate_kbps);
                    let _ = reply.send(outcome);
                }
                Ok(Command::TakePhoto { cb, user, reply }) => {
                    let outcome = self.take_photo(cb, user.0);
                    let _ = reply.send(outcome);
                }
                Ok(Command::Stop) => return false,
                Err(TryRecvError::Empty) => return true,
                // The owning thread dropped the handle without asking to stop, so there is
                // nobody left to deliver to.
                Err(TryRecvError::Disconnected) => return false,
            }
        }
    }

    /// Waits for a frame and encodes it, or returns once a request needs attention.
    ///
    /// Returns `Ok(())` when there was nothing to do: a request arrived, or the driver
    /// discarded the frame it handed over.
    fn next_frame(&mut self) -> Result<(), CaptureError> {
        if !self.device.wait_readable(self.wake.as_raw_fd(), -1)? {
            return Ok(());
        }
        let Some(frame) = self.device.dequeue()? else {
            return Ok(());
        };
        let outcome = self.encode(&frame);
        // The buffer has to go back even when the frame was unusable, or the driver's queue
        // drains to empty and the camera stops being asked for anything.
        self.device.requeue(frame.index)?;
        outcome
    }

    /// Turns one dequeued frame into NV12, encodes it and hands the access unit over.
    fn encode(&mut self, frame: &v4l2::Dequeued) -> Result<(), CaptureError> {
        let width = self.device.width as usize;
        let height = self.device.height as usize;

        match self.device.format {
            PixelFormat::Nv12 => nv12_from_nv12(
                self.device.buffer(frame.index),
                self.device.pitch as usize,
                width,
                height,
                &mut self.frame,
            )?,
            PixelFormat::Yuyv => nv12_from_yuyv(
                self.device.buffer(frame.index),
                self.device.pitch as usize,
                width,
                height,
                &mut self.frame,
            )?,
            PixelFormat::Mjpeg => {
                // The driver's payload length is what bounds the JPEG, which is the one place
                // it matters: the rest of the buffer is uninitialised as far as this process
                // knows.
                let jpeg = &self.device.buffer(frame.index)[..frame.used];
                let (decoded_width, decoded_height) = decode_mjpeg(jpeg, &mut self.rgb)?;
                if decoded_width != width || decoded_height != height {
                    return Err(CaptureError::Os(format!(
                        "the camera sent a {decoded_width}x{decoded_height} JPEG for a \
                         {width}x{height} stream"
                    )));
                }
                nv12_from_rgb(&self.rgb, width, height, &mut self.frame)?;
                // Keep the compressed frame: a still from the camera's own JPEG beats one
                // from re-encoded pixels, and it costs a copy of a fraction of the frame.
                self.compressed.clear();
                self.compressed.extend_from_slice(jpeg);
            }
        }

        self.photo_valid = true;
        self.encoded.clear();
        let keyframe = self.encoder.encode(&self.frame, &mut self.encoded)?;

        // SAFETY: `frame_cb` is the callback `open` was handed, `user` the pointer it was
        // given, and the slice stays valid for the duration of the call, which copies it.
        unsafe {
            (self.frame_cb)(
                self.user.0,
                self.encoded.as_ptr(),
                self.encoded.len(),
                u8::from(keyframe),
            );
        }
        Ok(())
    }

    /// Encodes the newest frame as JPEG and delivers it through the caller's callback.
    fn take_photo(&mut self, cb: JpegCallback, user: *mut c_void) -> Result<(), CaptureError> {
        if !self.photo_valid {
            return Err(CaptureError::Os(
                "no frame has arrived yet, so there is no still to take".to_owned(),
            ));
        }

        if self.device.format.is_compressed() {
            let (width, height) = decode_mjpeg(&self.compressed, &mut self.rgb)?;
            encode_jpeg(&self.rgb, width, height, &mut self.jpeg)?;
        } else {
            let width = self.device.width as usize;
            let height = self.device.height as usize;
            rgb_from_nv12(&self.frame, width, width, height, &mut self.rgb)?;
            encode_jpeg(&self.rgb, width, height, &mut self.jpeg)?;
        }

        // SAFETY: as in `encode`; the callback is expected to copy the bytes before returning.
        unsafe {
            (cb)(user, self.jpeg.as_ptr(), self.jpeg.len());
        }
        Ok(())
    }

    /// Rebuilds the pipeline: camera, encoder and buffers.
    ///
    /// The replacement is built before the running pipeline is dropped, so a camera that
    /// cannot deliver the new format leaves the stream that is on the air alone and reports
    /// the failure to the caller.
    fn rebuild(
        &mut self,
        width: u32,
        height: u32,
        fps: u32,
        codec: Codec,
        bitrate_kbps: u32,
    ) -> Result<(), CaptureError> {
        let codec = va_codec(codec)?;
        let node = self.device.path.clone();
        let device = Device::open(&node, width, height, fps)?;
        let encoder = vaapi::open_encoder(
            device.width,
            device.height,
            device.fps,
            codec,
            bitrate_kbps,
        )?;

        // A frame of the old pipeline is not a frame of the new one: its geometry and format
        // belong to a camera configuration that no longer exists.
        self.photo_valid = false;
        self.compressed.clear();
        self.frame.clear();
        self.frame
            .resize(nv12_len(device.width as usize, device.height as usize), 0);
        tracing::info!(
            node = %node.display(),
            width = device.width,
            height = device.height,
            fps = device.fps,
            bitrate_kbps,
            "capture parameters changed"
        );
        self.device = device;
        self.encoder = encoder;
        Ok(())
    }
}

/// The VAAPI codec a protocol codec maps onto.
///
/// The encoder is hardware: VAAPI exposes H.264 and H.265 encode entry points and nothing
/// else that these parameter buffers describe, so the remaining protocol codecs have no path
/// here — the caller is told the parameters are unsupported rather than receiving a stream
/// in a different codec than it asked for.
fn va_codec(codec: Codec) -> Result<VaCodec, CaptureError> {
    match codec {
        Codec::H264 => Ok(VaCodec::H264),
        Codec::H265 => Ok(VaCodec::Hevc),
        _ => Err(CaptureError::UnsupportedParameters),
    }
}

// ---------------------------------------------------------------------------------------
// A pipe the capture thread can be woken with.
// ---------------------------------------------------------------------------------------

/// Creates the pipe requests are announced on: close-on-exec and non-blocking on both ends.
fn wake_pipe() -> Result<(OwnedFd, OwnedFd), CaptureError> {
    let mut fds = [0 as RawFd; 2];
    // SAFETY: `fds` is a valid two-element array, which `pipe2` fills in.
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC | libc::O_NONBLOCK) } != 0 {
        return Err(CaptureError::Os(format!(
            "could not create a pipe: {}",
            io::Error::last_os_error()
        )));
    }
    // SAFETY: both descriptors were just created and are owned by this process.
    unsafe { Ok((OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1]))) }
}

/// Tells the capture thread that a request is waiting.
///
/// A failure other than "already full" means the pipe is gone, which can only happen after
/// the thread ended; either way there is nothing useful to do with the error here, so it is
/// left for the request's own reply channel to report.
fn wake(pipe: &OwnedFd) {
    let byte = [1u8];
    // SAFETY: `pipe` is the write end of the wake pipe, and `byte` is a valid one-byte buffer.
    let written = unsafe { libc::write(pipe.as_raw_fd(), byte.as_ptr().cast(), byte.len()) };
    if written < 0 {
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::WouldBlock {
            tracing::debug!(%error, "could not wake the capture thread");
        }
    }
}

/// Empties the wake pipe, so the next wait blocks again.
fn drain(pipe: &OwnedFd) {
    let mut bytes = [0u8; 64];
    loop {
        // SAFETY: `pipe` is the read end of the wake pipe and `bytes` is valid for its length.
        let read = unsafe { libc::read(pipe.as_raw_fd(), bytes.as_mut_ptr().cast(), bytes.len()) };
        if read <= 0 {
            return;
        }
    }
}
