//! Wire format for the SmartClass Webcam device protocol.
//!
//! This crate owns the message shapes exchanged over the device WebSocket — the
//! registration body, the text `control` envelope and the binary media framing —
//! and nothing about the socket itself. It is transport agnostic so the same
//! types serve the real server, a local mock and the unit tests.
//!
//! Reference: <https://crazy4chicken.github.io/smartclass-webcam-server/protocol/>.
//!
//! # Layout
//!
//! - [`RegisterRequest`] / [`RegisterResponse`]: the `GET /ws/register` exchange.
//! - [`Envelope`]: text frames on the `control` channel, with typed payloads such as
//!   [`StartRecordingPayload`] and [`AckPayload`].
//! - [`encode_frame`] / [`decode_frame`]: the `uint32` big-endian header length, JSON header
//!   and raw media body used by the `recording` and `photo` channels.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod binary;
mod codec;
mod control;
mod registration;

pub use binary::{
    BinaryHeader, FramePayload, MediaPayload, MediaType, MAX_FRAME_LEN, MAX_HEADER_LEN,
    PhotoPayload, decode_frame, encode_frame,
};
pub use codec::Codec;
pub use control::{
    AckPayload, Channel, Envelope, ErrorPayload, PingPayload, PongPayload, StartRecordingPayload,
    StatusPayload, StopRecordingPayload, SwitchCameraPayload, TakePhotoPayload,
};
pub use registration::{CameraRegistration, RegisterRequest, RegisterResponse};

use serde_json::Error as JsonError;

/// Errors produced while encoding or decoding protocol messages.
#[derive(Debug, thiserror::Error)]
pub enum ProtocolError {
    /// JSON header length outside the protocol's `1..=65536` byte range.
    #[error("binary header length {len} is outside the allowed 1..=65536 bytes")]
    HeaderLengthOutOfRange {
        /// Declared or produced header length, in bytes.
        len: usize,
    },
    /// Whole frame (length prefix + header + body) at or above the 16 MiB limit.
    #[error("frame of {len} bytes reaches the {limit} byte limit")]
    FrameTooLarge {
        /// Total frame size, in bytes.
        len: usize,
        /// Protocol limit the frame reached, in bytes.
        limit: usize,
    },
    /// Frame shorter than its length prefix or the header it declares.
    #[error("frame truncated: {needed} bytes needed, {available} available")]
    Truncated {
        /// Smallest frame size that would be decodable, in bytes.
        needed: usize,
        /// Actual frame size, in bytes.
        available: usize,
    },
    /// JSON header, payload or envelope did not match the expected schema.
    #[error("invalid JSON: {0}")]
    Json(#[from] JsonError),
    /// Binary header channel/type combination does not match the payload it carries.
    #[error("binary header mismatch: channel {channel:?} and type {msg_type:?} do not match the payload")]
    HeaderMismatch {
        /// Channel declared by the header.
        channel: Channel,
        /// Message type declared by the header.
        msg_type: MediaType,
    },
}

/// Result alias for fallible protocol operations.
pub type Result<T, E = ProtocolError> = std::result::Result<T, E>;
