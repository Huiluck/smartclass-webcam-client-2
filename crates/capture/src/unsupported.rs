//! Stub backend for hosts without a capture implementation.
//!
//! It exists so the crate — and therefore the whole workspace — builds anywhere. Every
//! entry point fails with [`CaptureError::Os`], which is what a caller on such a host sees.

use std::ffi::c_void;

use scwc_protocol::Codec;

use crate::error::CaptureError;
use crate::source::{FrameCallback, JpegCallback};
use crate::{CameraInfo, CaptureConfig};

/// The failure every entry point reports on unsupported hosts.
fn unimplemented() -> CaptureError {
    CaptureError::Os(format!(
        "camera capture is not implemented on {}",
        std::env::consts::OS
    ))
}

/// A pipeline that cannot exist on this host.
pub(crate) struct Handle;

impl Handle {
    /// Always fails: there is no capture backend to open.
    pub(crate) fn open(
        _cfg: &CaptureConfig,
        _frame_cb: FrameCallback,
        _jpeg_cb: JpegCallback,
        _user: *mut c_void,
    ) -> Result<Self, CaptureError> {
        Err(unimplemented())
    }

    /// Always fails: there is no pipeline to reconfigure.
    pub(crate) fn set_parameters(
        &self,
        _width: u32,
        _height: u32,
        _fps: u32,
        _codec: Codec,
        _bitrate_kbps: u32,
    ) -> Result<(), CaptureError> {
        Err(unimplemented())
    }

    /// Always fails: there is no frame to encode.
    pub(crate) fn take_photo(
        &self,
        _jpeg_cb: JpegCallback,
        _user: *mut c_void,
    ) -> Result<(), CaptureError> {
        Err(unimplemented())
    }

    /// Nothing to stop.
    pub(crate) fn close(&mut self) {}
}

/// Always fails: there is no device enumeration on this host.
pub(crate) fn enumerate_cameras() -> Result<Vec<CameraInfo>, CaptureError> {
    Err(unimplemented())
}
