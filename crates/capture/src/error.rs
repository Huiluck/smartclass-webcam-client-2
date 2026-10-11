//! Failure of a camera capture operation.

use scwc_core::SourceError;

/// Failure of [`enumerate_cameras`](crate::enumerate_cameras) or [`open`](crate::open).
///
/// The variants are deliberately coarse: the core only needs a message for its logs and
/// for `ack {ok: false, error}`, while the backend-specific detail (which platform call
/// failed and why) is logged by the backend as it happens.
#[derive(Debug, thiserror::Error)]
pub enum CaptureError {
    /// The host exposes no usable video capture device.
    #[error("no camera available")]
    NoCamera,
    /// The camera cannot deliver the requested resolution, frame rate or codec.
    #[error("unsupported capture parameters")]
    UnsupportedParameters,
    /// The process is not allowed to use the camera.
    ///
    /// On macOS a CLI process inherits the permission of the terminal that launched it, so
    /// this means the invoking application is missing the camera entitlement in
    /// System Settings > Privacy & Security > Camera; on Windows it means the camera privacy
    /// setting denies the desktop application access.
    #[error("camera access denied")]
    PermissionDenied,
    /// The platform backend failed outside the cases above.
    #[error("capture backend failure: {0}")]
    Os(String),
    /// The channel between the backend callbacks and the caller was closed.
    #[error("capture callback channel closed")]
    ChannelClosed,
}

impl From<CaptureError> for SourceError {
    fn from(error: CaptureError) -> Self {
        SourceError::from(error.to_string())
    }
}
