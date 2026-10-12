//! The [`FrameSource`] implementation: channel plumbing between the backend callbacks and
//! the core.
//!
//! The encoded frames go one way — backend callback to core — and the live preview goes the
//! other: every backend offers the frames it captures to [`scwc_core::preview`], which owns
//! the sink, the conversion and the scaling. A backend that can hand over its platform's own
//! buffer does that too, and the core decides which of the two a consumer is waiting for.

use std::ffi::c_void;
use std::ptr;
use std::slice;
use std::time::Duration;

use bytes::Bytes;
use scwc_core::{EncodedFrame, FrameSource, MEDIA_QUEUE_CAPACITY, SourceError};
use scwc_protocol::Codec;
use tokio::sync::mpsc;
use tracing::{error, warn};

use crate::CaptureConfig;
use crate::error::CaptureError;
use crate::sys;

/// Backend entry point for one encoded access unit.
pub(crate) type FrameCallback =
    unsafe extern "C" fn(user: *mut c_void, data: *const u8, len: usize, is_keyframe: u8);

/// Backend entry point for one JPEG still.
pub(crate) type JpegCallback =
    unsafe extern "C" fn(user: *mut c_void, data: *const u8, len: usize);

/// Backend entry point for one captured frame handed over as the platform's own buffer.
///
/// The Apple shim lends the buffer — a `CVPixelBufferRef` — for the duration of the call, and
/// only the shim knows what a refcount is on it: a frame the preview keeps is retained through
/// [`retain_frame_buffer`], a frame it replaces is released through [`release_frame_buffer`], so
/// no CoreVideo type enters this crate's public surface.
#[cfg(any(target_os = "macos", target_os = "ios"))]
pub(crate) type FrameSlotCallback =
    unsafe extern "C" fn(user: *mut c_void, buffer: *mut c_void);

/// The frame-slot callback the Apple shim hands its captured buffers to.
///
/// The gate is the core's own: a camera whose frames nobody renders as a platform buffer costs
/// this a load and a return.
#[cfg(any(target_os = "macos", target_os = "ios"))]
pub(crate) unsafe extern "C" fn on_native_frame(_user: *mut c_void, buffer: *mut c_void) {
    if buffer.is_null() || !scwc_core::preview::native_preview_active() {
        return;
    }
    // SAFETY: `buffer` is the buffer the shim lent for the duration of this call; the frame is
    // retained through the shim, and the one it replaces is released the same way, so the slot
    // owns exactly one reference to whatever it holds.
    let retained = unsafe { sys::retain_buffer(buffer) };
    if let Some(replaced) = scwc_core::preview::publish_native_frame(retained) {
        // SAFETY: the slot handed back the reference it had retained, and nothing else holds it.
        unsafe { sys::release_buffer(replaced) };
    }
}

/// Retains one frame buffer so a consumer can own what it keeps; release it with
/// [`release_frame_buffer`].
///
/// # Safety
///
/// `buffer` is a buffer the capture callback lent — a `CVPixelBufferRef` — and it is still alive:
/// a buffer may only be retained inside the call that lent it, or from a reference already
/// retained that way.
///
/// A buffer that is null is returned unchanged, so callers need no null check.
#[cfg(any(target_os = "macos", target_os = "ios"))]
pub unsafe fn retain_frame_buffer(buffer: *mut c_void) -> *mut c_void {
    // SAFETY: the caller upholds the contract above.
    unsafe { sys::retain_buffer(buffer) }
}

/// Releases one reference to a buffer retained by [`retain_frame_buffer`].
///
/// # Safety
///
/// `buffer` came from [`retain_frame_buffer`] (or from [`scwc_core::preview::take_native_frame`],
/// which hands on such a reference) and has not been released yet. Nothing may use the buffer
/// after this returns.
///
/// A buffer that is null is ignored, so callers need no null check.
#[cfg(any(target_os = "macos", target_os = "ios"))]
pub unsafe fn release_frame_buffer(buffer: *mut c_void) {
    // SAFETY: the caller upholds the contract above.
    unsafe { sys::release_buffer(buffer) };
}

/// Backend entry point for one live preview frame: tightly packed BGRA, `width * height` of them.
///
/// The Apple shim hands a frame over this way, having already rendered it at the size
/// [`preview_wanted`] asked for — CoreVideo's buffers carry row padding the core knows nothing
/// about. The Linux and Windows backends convert their frames in this crate and reach
/// [`scwc_core::preview::publish_bgra`] directly.
#[cfg(any(target_os = "macos", target_os = "ios"))]
pub(crate) type PreviewCallback =
    unsafe extern "C" fn(user: *mut c_void, data: *const u8, len: usize, width: u32, height: u32);

/// Backend entry point a backend asks where to render the next preview frame, if anywhere.
///
/// A conversion reads the whole frame, so a backend that can skip it cheaply — the Apple shim
/// renders the frame with CoreImage for every preview it produces — asks this first, with the
/// frame it holds. Non-zero means one is wanted, and the size to render at is written to
/// `out_width`/`out_height`; zero means the backend must do nothing at all.
#[cfg(any(target_os = "macos", target_os = "ios"))]
pub(crate) type PreviewWanted = unsafe extern "C" fn(
    user: *mut c_void,
    frame_width: u32,
    frame_height: u32,
    out_width: *mut u32,
    out_height: *mut u32,
) -> i32;

/// The preview callback the Apple shim hands its frames to.
#[cfg(any(target_os = "macos", target_os = "ios"))]
pub(crate) unsafe extern "C" fn on_preview(
    _user: *mut c_void,
    data: *const u8,
    len: usize,
    width: u32,
    height: u32,
) {
    if data.is_null() {
        return;
    }
    // SAFETY: the caller hands back a pointer to a buffer that stays valid for the duration of
    // the call, which converts (and copies) what it needs before returning.
    let pixels = unsafe { slice::from_raw_parts(data, len) };
    scwc_core::preview::publish_bgra(pixels, width, height);
}

/// Where the next preview frame should be rendered, if anywhere; see
/// [`scwc_core::preview::next_frame_size`].
///
/// This is the gate the Apple shim consults on the capture queue for every captured frame, so it
/// does nothing but look at the core's state and do a little arithmetic.
#[cfg(any(target_os = "macos", target_os = "ios"))]
pub(crate) unsafe extern "C" fn preview_wanted(
    _user: *mut c_void,
    frame_width: u32,
    frame_height: u32,
    out_width: *mut u32,
    out_height: *mut u32,
) -> i32 {
    let Some((width, height)) = scwc_core::preview::next_frame_size(frame_width, frame_height)
    else {
        return 0;
    };
    if !out_width.is_null() {
        // SAFETY: the caller promises both out-parameters point at a `uint32_t`.
        unsafe { *out_width = width };
    }
    if !out_height.is_null() {
        // SAFETY: as above.
        unsafe { *out_height = height };
    }
    1
}

/// How long a keyframe keeps retrying for space in the queue before it is dropped anyway.
///
/// The retry runs on the backend's own callback thread, and a blocked one back-pressures the
/// whole capture path: VideoToolbox stops accepting frames, the capture delegate blocks inside
/// `EncodeFrame`, and the frames nobody sees any more — the preview included — arrive in a burst
/// once it unblocks. Measured on a machine with the camera in standby, the old budget (1000
/// tries, two seconds) held a delegate call for 1960ms and the preview was short by 1988ms per
/// occurrence, so the window has to be small: it is there to ride out a consumer that is behind
/// by a frame or two, not to wait for one that is not reading at all. That case ends the same
/// way it did before — the keyframe is dropped, a warning is logged, and the consumer gets its
/// keyframe from the next GOP.
const KEYFRAME_RETRY_WINDOW: Duration = Duration::from_millis(50);

/// How long a keyframe waits for space in the queue before retrying.
const KEYFRAME_RETRY_DELAY: Duration = Duration::from_millis(2);

/// Data shared with the backend callbacks through the `user` pointer.
///
/// Lives inside [`CaptureSource`], so the pointers the backend holds stay valid for as long
/// as the backend itself: closing the backend before dropping this box is what
/// [`CaptureSource`]'s `Drop` guarantees.
pub(crate) struct CallbackState {
    frames: mpsc::Sender<EncodedFrame>,
    photos: mpsc::Sender<Vec<u8>>,
    /// When the queue-full warning was last emitted; the mutex is uncontended except
    /// on the (already slow) full-queue path.
    last_full_warning: std::sync::Mutex<std::time::Instant>,
}

/// Opens a camera and starts capturing.
///
/// Returns as soon as the capture session is running; frames arrive through the returned
/// source. Fails with [`CaptureError::UnsupportedParameters`] when the camera cannot
/// deliver the requested resolution, frame rate or codec, and with
/// [`CaptureError::PermissionDenied`] when the process may not use the camera.
pub fn open(cfg: CaptureConfig) -> Result<CaptureSource, CaptureError> {
    let (frames_tx, frames_rx) = mpsc::channel(MEDIA_QUEUE_CAPACITY);
    let (photos_tx, photos_rx) = mpsc::channel(1);
    let state = Box::new(CallbackState {
        frames: frames_tx,
        photos: photos_tx,
        // Long ago, so the first full queue still warns immediately.
        last_full_warning: std::sync::Mutex::new(
            std::time::Instant::now() - Duration::from_secs(60),
        ),
    });

    // The backend copies everything it needs out of `cfg` and `state` before returning.
    let user = ptr::from_ref(&*state).cast_mut().cast::<c_void>();
    let handle = sys::Handle::open(&cfg, on_frame, on_photo, user)?;

    Ok(CaptureSource {
        handle,
        cfg,
        frames: frames_rx,
        photos: photos_rx,
        channel_closed_reported: false,
        // Kept alive with `handle`: the callbacks read it through `user`.
        state,
    })
}

/// The callback the backend invokes for every encoded access unit.
///
/// Sends the frame into the queue, dropping delta frames when the queue is full. A keyframe
/// waits for space instead, which stalls the encoder — the same backpressure the core's own
/// queue applies one hop later — and gives up only after
/// [`KEYFRAME_RETRY_LIMIT`] attempts, so a consumer that stopped reading cannot wedge the
/// backend thread that teardown has to join.
pub(crate) unsafe extern "C" fn on_frame(
    user: *mut c_void,
    data: *const u8,
    len: usize,
    is_keyframe: u8,
) {
    // SAFETY: the backend passes back the pointer it was given, which stays valid until
    // `CaptureSource::drop` has closed it.
    let state = unsafe { &*user.cast::<CallbackState>() };
    if data.is_null() || len == 0 {
        return;
    }
    let frame = EncodedFrame::new(
        Bytes::copy_from_slice(unsafe { slice::from_raw_parts(data, len) }),
        is_keyframe != 0,
    );

    match state.frames.try_send(frame) {
        Ok(()) | Err(mpsc::error::TrySendError::Closed(_)) => {}
        Err(mpsc::error::TrySendError::Full(frame)) => {
            if !frame.is_keyframe {
                return;
            }
            let mut frame = frame;
            let deadline = std::time::Instant::now() + KEYFRAME_RETRY_WINDOW;
            loop {
                match state.frames.try_send(frame) {
                    Ok(()) | Err(mpsc::error::TrySendError::Closed(_)) => return,
                    Err(mpsc::error::TrySendError::Full(returned)) => {
                        frame = returned;
                        if std::time::Instant::now() >= deadline {
                            break;
                        }
                        std::thread::sleep(KEYFRAME_RETRY_DELAY);
                    }
                }
            }
            // Rate-limit: a camera that keeps encoding with no consumer reading has a
            // permanently full queue, which must not spam the log (or the UI event
            // stream, which forwards every warning).
            let mut last = state
                .last_full_warning
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if last.elapsed() >= Duration::from_secs(30) {
                *last = std::time::Instant::now();
                warn!("dropping a keyframe: the frame queue stayed full");
            }
        }
    }
}

/// The callback the backend invokes once per `take_photo`, before it returns.
pub(crate) unsafe extern "C" fn on_photo(user: *mut c_void, data: *const u8, len: usize) {
    // SAFETY: see `on_frame`.
    let state = unsafe { &*user.cast::<CallbackState>() };
    if data.is_null() || len == 0 {
        return;
    }
    let jpeg = unsafe { slice::from_raw_parts(data, len) }.to_vec();
    if state.photos.try_send(jpeg).is_err() {
        warn!("dropping a photo: no `take_photo` call is waiting for it");
    }
}

/// One camera's live capture pipeline.
///
/// Implements [`FrameSource`], so the core owns it exclusively: a frame callback never
/// races a reconfiguration, and a reconfiguration only happens at a frame boundary.
pub struct CaptureSource {
    handle: sys::Handle,
    cfg: CaptureConfig,
    frames: mpsc::Receiver<EncodedFrame>,
    photos: mpsc::Receiver<Vec<u8>>,
    channel_closed_reported: bool,
    /// Declared last: dropped after `handle`'s `Drop` has stopped the backend callbacks.
    state: Box<CallbackState>,
}

impl Drop for CaptureSource {
    fn drop(&mut self) {
        // Stops the session and drains in-flight callbacks, so no backend thread can touch
        // `state` after this returns.
        self.handle.close();
    }
}

impl FrameSource for CaptureSource {
    async fn next_frame(&mut self) -> EncodedFrame {
        match self.frames.recv().await {
            Some(frame) => frame,
            None => {
                if !self.channel_closed_reported {
                    self.channel_closed_reported = true;
                    error!("capture pipeline stopped: no further frames will arrive");
                }
                // The core keeps calling `next_frame` while a stream is active and has no
                // way to be told the source is gone, so idling is the honest answer.
                std::future::pending().await
            }
        }
    }

    fn set_parameters(
        &mut self,
        resolution: &str,
        fps: u32,
        codec: Codec,
        bitrate_kbps: u32,
    ) -> Result<(), SourceError> {
        let (width, height) = parse_resolution(resolution)
            .ok_or_else(|| SourceError::from(format!("malformed resolution {resolution:?}")))?;
        // Reconfiguring tears down and rebuilds the backend pipeline, so an
        // identical request must not touch it: session start re-applies the
        // announced parameters every time, and they usually match.
        if self.cfg.width == width
            && self.cfg.height == height
            && self.cfg.fps == fps
            && self.cfg.codec == codec
            && self.cfg.bitrate_kbps == bitrate_kbps
        {
            return Ok(());
        }
        self.handle
            .set_parameters(width, height, fps, codec, bitrate_kbps)?;
        self.cfg.width = width;
        self.cfg.height = height;
        self.cfg.fps = fps;
        self.cfg.codec = codec;
        self.cfg.bitrate_kbps = bitrate_kbps;
        Ok(())
    }

    async fn take_photo(&mut self) -> Result<Vec<u8>, SourceError> {
        // A result of an earlier photo that was never collected would reject this request.
        while self.photos.try_recv().is_ok() {}

        // Blocks for the duration of the JPEG encode (tens of milliseconds), which the core
        // permits: it reconfigures only at a frame boundary anyway.
        let user = ptr::from_ref(&*self.state).cast_mut().cast::<c_void>();
        self.handle.take_photo(on_photo, user)?;

        match self.photos.recv().await {
            Some(jpeg) => Ok(jpeg),
            None => Err(SourceError::from("capture pipeline stopped before the photo")),
        }
    }

    fn set_encoding(&mut self, enabled: bool) {
        self.handle.set_encoding(enabled);
    }

    fn preferred_codec(&self) -> Codec {
        self.cfg.codec
    }
}

/// Splits a protocol resolution (`"3840x2160"`) into its sides.
fn parse_resolution(resolution: &str) -> Option<(u32, u32)> {
    let (width, height) = resolution.split_once(['x', 'X'])?;
    let width = width.trim().parse().ok()?;
    let height = height.trim().parse().ok()?;
    (width > 0 && height > 0).then_some((width, height))
}

#[cfg(test)]
mod tests {
    use super::parse_resolution;

    #[test]
    fn parses_protocol_resolutions() {
        assert_eq!(parse_resolution("3840x2160"), Some((3840, 2160)));
        assert_eq!(parse_resolution("1280X720"), Some((1280, 720)));
        assert_eq!(parse_resolution(" 640x 480 "), Some((640, 480)));
    }

    #[test]
    fn rejects_malformed_resolutions() {
        for rejected in [
            "",
            "1920",
            "1920x",
            "x1080",
            "0x1080",
            "1920x0",
            "-1x10",
            "1920*1080",
        ] {
            assert_eq!(parse_resolution(rejected), None, "{rejected:?}");
        }
    }
}
