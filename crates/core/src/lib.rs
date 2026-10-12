//! Device-client core for the SmartClass Webcam protocol.
//!
//! This crate owns everything between "a camera can hand me encoded frames" and "the
//! server has those frames": registration, the device WebSocket session, control-command
//! dispatch and the media queue. Capture and encoding stay behind [`FrameSource`], so
//! the same core drives a synthetic test pattern, a desktop webcam or a phone camera.
//!
//! # Layout
//!
//! - `register`: `GET /ws/register` against the configured base URL.
//! - `session`: attach to `GET /ws/device/{ticket}` and pump the socket.
//! - `control`: parse server envelopes, keep the per-session device state, answer `ack`s.
//! - `media`: pull frames from the [`FrameSource`], queue them with a drop policy.
//! - `params`: remember the capture parameters a switch applied, in memory and on disk.
//! - `command`: the requests an embedding application sends while the client runs.
//! - [`run`]: the supervisor that ties all of the above into a reconnect loop.
//!
//! # Quick start
//!
//! ```no_run
//! use std::future::pending;
//!
//! use scwc_core::{ClientConfig, FrameSource, run};
//!
//! # async fn example<F: FrameSource + 'static>(source: F) {
//! // One `CameraRegistration` per camera; the list is announced verbatim.
//! let config = ClientConfig::new(
//!     "https://smartclass.example.edu",
//!     "camera-01",
//!     "wdt_...",
//!     vec![],
//! );
//! let _ = run(config, source, pending::<()>()).await;
//! # }
//! ```
//!
//! # Behaviour the caller can rely on
//!
//! - Every registration announces [`ClientConfig::cameras`] verbatim.
//! - Every server command is answered with exactly one `ack`, echoing its `id`; commands
//!   without an `id` are handled but not answered.
//! - Frames are sent only for a stream that a `start_recording` command announced, tagged
//!   with a per-stream `seq` starting at `0` and the capture time of each frame.
//! - A parameter switch (`switch_camera`, or the codec of a `start_recording`) is answered as
//!   soon as it is accepted; while a stream is active it reaches the capture pipeline when
//!   that stream ends, so a recording keeps the parameters it started with.
//! - The same switch travels the same path when the embedding application asks for it through
//!   [`ClientConfig::commands`], which is also how an application moves the client onto another
//!   server: a [`UiCommand::Reconnect`] registers again with the new credentials, at once while
//!   no stream is live and once the live one has ended otherwise, so a recording survives the
//!   request.
//! - Every parameter set that reaches the capture pipeline is reported as
//!   [`CoreEvent::ParametersApplied`] and persisted; a request that had to wait is reported as
//!   [`CoreEvent::Notice`].
//! - With [`ClientConfig::state_path`] set, the parameters the client was last switched to
//!   outlive the process and take precedence over the announced defaults at the next start.
//! - Media never blocks the frame source except while a keyframe waits for queue space;
//!   delta frames are discarded until the next keyframe when the queue overflows.
//! - Any close re-registers from scratch behind exponential backoff (1s, doubling, capped
//!   at 30s, reset after a session that stayed up longer than 60s).

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod clock;
mod command;
mod config;
mod control;
mod error;
mod events;
mod media;
mod params;
mod register;
mod session;
mod supervisor;

pub use command::{SwitchParams, UiCommand};
pub use config::{ClientConfig, ServerConfig};
pub use error::{CoreError, Result, SourceError};
pub use events::CoreEvent;
pub use media::{EncodedFrame, FrameSource, MEDIA_QUEUE_CAPACITY, default_bitrate_kbps};
pub use supervisor::run;

pub use scwc_protocol::CameraRegistration;

/// Re-export of the `bytes` crate, whose [`Bytes`](bytes::Bytes) carries
/// [`EncodedFrame::data`].
pub use bytes;
