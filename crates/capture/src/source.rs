//! The [`FrameSource`] implementation: channel plumbing between the backend callbacks and
//! the core.

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

/// How long a keyframe waits for space in the queue before retrying.
const KEYFRAME_RETRY_DELAY: Duration = Duration::from_millis(2);

/// How often a keyframe retries before it is dropped anyway.
///
/// The wait has to end: the callback blocks a backend thread that teardown joins, so a
/// consumer that never drains its queue would otherwise deadlock the shutdown. At two
/// seconds a full queue already means the core is not reading.
const KEYFRAME_RETRY_LIMIT: usize = 1000;

/// Data shared with the backend callbacks through the `user` pointer.
///
/// Lives inside [`CaptureSource`], so the pointers the backend holds stay valid for as long
/// as the backend itself: closing the backend before dropping this box is what
/// [`CaptureSource`]'s `Drop` guarantees.
pub(crate) struct CallbackState {
    frames: mpsc::Sender<EncodedFrame>,
    photos: mpsc::Sender<Vec<u8>>,
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
            for attempt in 0..KEYFRAME_RETRY_LIMIT {
                match state.frames.try_send(frame) {
                    Ok(()) | Err(mpsc::error::TrySendError::Closed(_)) => return,
                    Err(mpsc::error::TrySendError::Full(returned)) => {
                        frame = returned;
                        if attempt + 1 < KEYFRAME_RETRY_LIMIT {
                            std::thread::sleep(KEYFRAME_RETRY_DELAY);
                        }
                    }
                }
            }
            warn!("dropping a keyframe: the frame queue stayed full");
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
