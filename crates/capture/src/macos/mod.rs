//! macOS backend: the Objective-C++ shim over AVFoundation and VideoToolbox.
//!
//! The shim (`shim.mm`, built by `build.rs`) owns every platform object; this module only
//! translates between Rust values and the C ABI declared in `shim.h`. Failure detail is
//! reported out of band as a thread-local message, which works because every call into the
//! shim happens on the caller's own thread.

use std::ffi::{CStr, CString, c_char, c_void};
use std::ptr;

use scwc_protocol::Codec;
use tracing::error;

use crate::error::CaptureError;
use crate::source::{FrameCallback, JpegCallback};
use crate::{CameraInfo, CaptureConfig};

/// The shim's success code.
const OK: i32 = 0;
/// The shim found no usable camera.
const ERR_NO_CAMERA: i32 = 1;
/// The camera cannot produce the requested format.
const ERR_UNSUPPORTED: i32 = 2;
/// The process is not allowed to use the camera.
const ERR_PERMISSION: i32 = 3;

/// Shim codec id for H.264.
const CODEC_H264: i32 = 0;
/// Shim codec id for H.265.
const CODEC_H265: i32 = 1;

/// Size of the buffer the shim writes a failure message into.
const ERROR_BUFFER: usize = 512;

/// Backend entry point invoked once per enumerated camera.
type CameraBeginCallback = unsafe extern "C" fn(user: *mut c_void, id: *const c_char, name: *const c_char);

/// Backend entry point invoked once per announced frame rate of a camera.
type CameraFormatCallback =
    unsafe extern "C" fn(user: *mut c_void, resolution: *const c_char, fps: u32);

/// One capture request, as the shim expects it.
///
/// The fields are only written on this side; the shim reads them across the ABI.
#[repr(C)]
#[allow(dead_code)]
struct MacConfig {
    /// NUL-terminated device `uniqueID`, or null for the system default camera.
    camera_id: *const c_char,
    width: u32,
    height: u32,
    fps: u32,
    /// One of [`CODEC_H264`] / [`CODEC_H265`].
    codec: i32,
    bitrate_kbps: u32,
}

unsafe extern "C" {
    fn scwc_mac_enumerate(
        begin_cb: CameraBeginCallback,
        format_cb: CameraFormatCallback,
        user: *mut c_void,
    ) -> i32;
    fn scwc_mac_open(
        cfg: *const MacConfig,
        frame_cb: FrameCallback,
        user: *mut c_void,
        out_handle: *mut *mut c_void,
    ) -> i32;
    fn scwc_mac_set_params(
        handle: *mut c_void,
        width: u32,
        height: u32,
        fps: u32,
        codec: i32,
        bitrate_kbps: u32,
    ) -> i32;
    fn scwc_mac_take_photo(handle: *mut c_void, jpeg_cb: JpegCallback, user: *mut c_void) -> i32;
    fn scwc_mac_close(handle: *mut c_void);
    fn scwc_mac_last_error(buf: *mut c_char, buf_len: usize) -> i32;
}

/// A live shim capture pipeline.
pub(crate) struct Handle {
    raw: *mut c_void,
}

// SAFETY: the shim owns its capture and encoder threads and serializes `set_params`,
// `take_photo` and `close` against them with its own locks; the handle is only used from
// the thread that owns the `CaptureSource`.
unsafe impl Send for Handle {}

impl Handle {
    /// Starts capturing; frames and photos are delivered through `frame_cb`/`jpeg_cb`.
    pub(crate) fn open(
        cfg: &CaptureConfig,
        frame_cb: FrameCallback,
        jpeg_cb: JpegCallback,
        user: *mut c_void,
    ) -> Result<Self, CaptureError> {
        let codec = shim_codec(cfg.codec)?;
        let camera_id = cfg
            .camera_id
            .as_deref()
            .map(CString::new)
            .transpose()
            .map_err(|_| CaptureError::Os("camera id contains a NUL byte".to_owned()))?;
        let raw_cfg = MacConfig {
            camera_id: camera_id.as_ref().map_or(ptr::null(), |id| id.as_ptr()),
            width: cfg.width,
            height: cfg.height,
            fps: cfg.fps,
            codec,
            bitrate_kbps: cfg.bitrate_kbps,
        };

        let mut raw = ptr::null_mut();
        // SAFETY: `raw_cfg` and its camera id outlive the call, which copies what it needs.
        let status = unsafe { scwc_mac_open(&raw_cfg, frame_cb, user, &mut raw) };
        if status != OK {
            return Err(last_error());
        }
        if raw.is_null() {
            return Err(CaptureError::Os(
                "capture backend returned no handle".to_owned(),
            ));
        }
        Ok(Self { raw })
    }

    /// Reconfigures the pipeline to new capture and encoder parameters.
    pub(crate) fn set_parameters(
        &self,
        width: u32,
        height: u32,
        fps: u32,
        codec: Codec,
        bitrate_kbps: u32,
    ) -> Result<(), CaptureError> {
        let codec = shim_codec(codec)?;
        // SAFETY: `raw` is a live handle for as long as `self` exists.
        let status = unsafe { scwc_mac_set_params(self.raw, width, height, fps, codec, bitrate_kbps) };
        if status != OK {
            return Err(last_error());
        }
        Ok(())
    }

    /// Captures one still, delivered through `jpeg_cb` before this function returns.
    pub(crate) fn take_photo(
        &self,
        jpeg_cb: JpegCallback,
        user: *mut c_void,
    ) -> Result<(), CaptureError> {
        // SAFETY: `raw` is a live handle for as long as `self` exists.
        let status = unsafe { scwc_mac_take_photo(self.raw, jpeg_cb, user) };
        if status != OK {
            return Err(last_error());
        }
        Ok(())
    }

    /// Stops the pipeline; no callback may fire afterwards.
    pub(crate) fn close(&mut self) {
        if !self.raw.is_null() {
            // SAFETY: the handle is live and consumed exactly once.
            unsafe { scwc_mac_close(self.raw) };
            self.raw = ptr::null_mut();
        }
    }
}

/// Lists the cameras of the host with the frame rates each format announces.
pub(crate) fn enumerate_cameras() -> Result<Vec<CameraInfo>, CaptureError> {
    let mut cameras: Vec<CameraInfo> = Vec::new();
    let user = ptr::from_mut(&mut cameras).cast::<c_void>();
    // SAFETY: the shim only writes through `user` for the duration of the call.
    let status = unsafe { scwc_mac_enumerate(on_camera_begin, on_camera_format, user) };
    if status != OK {
        return Err(last_error());
    }
    Ok(cameras)
}

/// Starts one camera in the sink the shim was handed.
unsafe extern "C" fn on_camera_begin(user: *mut c_void, id: *const c_char, name: *const c_char) {
    // SAFETY: `scwc_mac_enumerate` is always called with a pointer to a live `Vec`, and the
    // strings stay valid for the duration of the callback.
    unsafe {
        let cameras = &mut *user.cast::<Vec<CameraInfo>>();
        cameras.push(CameraInfo {
            id: c_string(id),
            name: c_string(name),
            presets: Vec::new(),
        });
    }
}

/// Appends one announced frame rate to the camera the shim is currently reporting.
unsafe extern "C" fn on_camera_format(user: *mut c_void, resolution: *const c_char, fps: u32) {
    // SAFETY: see `on_camera_begin`.
    unsafe {
        let cameras = &mut *user.cast::<Vec<CameraInfo>>();
        let Some(camera) = cameras.last_mut() else {
            return;
        };
        let resolution = c_string(resolution);

        // The shim emits a camera's rows grouped by resolution and already deduplicated;
        // merging again keeps the invariant local to this side.
        let matches_last = match camera.presets.last_mut() {
            Some(last) if last.0 == resolution => {
                if !last.1.contains(&fps) {
                    last.1.push(fps);
                }
                true
            }
            _ => false,
        };
        if !matches_last {
            camera.presets.push((resolution, vec![fps]));
        }
    }
}

/// The codec id the shim encodes with, or an error for codecs it cannot produce.
fn shim_codec(codec: Codec) -> Result<i32, CaptureError> {
    match codec {
        Codec::H264 => Ok(CODEC_H264),
        Codec::H265 => Ok(CODEC_H265),
        _ => Err(CaptureError::UnsupportedParameters),
    }
}

/// Reads the shim's thread-local failure message and maps it onto a [`CaptureError`].
fn last_error() -> CaptureError {
    let mut buf = [0 as c_char; ERROR_BUFFER];
    // SAFETY: the buffer is writable for `ERROR_BUFFER` bytes.
    let status = unsafe { scwc_mac_last_error(buf.as_mut_ptr(), buf.len()) };
    // SAFETY: the shim NUL-terminates the buffer.
    let message = unsafe { CStr::from_ptr(buf.as_ptr()) }
        .to_string_lossy()
        .into_owned();
    if !message.is_empty() {
        error!(code = status, detail = %message, "macOS capture backend failure");
    }

    match status {
        ERR_NO_CAMERA => CaptureError::NoCamera,
        ERR_UNSUPPORTED => CaptureError::UnsupportedParameters,
        ERR_PERMISSION => CaptureError::PermissionDenied,
        _ => CaptureError::Os(message),
    }
}

/// Copies a NUL-terminated string the shim owns for the duration of the call.
unsafe fn c_string(value: *const c_char) -> String {
    if value.is_null() {
        return String::new();
    }
    // SAFETY: the shim passes NUL-terminated strings that stay valid for the call.
    unsafe { CStr::from_ptr(value) }
        .to_string_lossy()
        .into_owned()
}
