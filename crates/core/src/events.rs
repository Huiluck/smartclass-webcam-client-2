//! Events the core reports to its caller.
//!
//! Everything the core does is logged through `tracing`, which is invisible to an embedding
//! application that installs no subscriber. [`CoreEvent`] carries the same lifecycle milestones
//! over an mpsc channel, so a caller can show a user what the client is doing — and, above all,
//! that it is failing.

use tokio::sync::mpsc::UnboundedSender;

/// One observable step of the client's register → attach → session cycle.
#[derive(Debug, Clone)]
pub enum CoreEvent {
    /// A registration — the attempt to obtain a ticket — is about to start.
    Registering,
    /// The device WebSocket handshake with a ticket succeeded.
    Attached,
    /// A session is running on the attached socket.
    SessionStarted,
    /// The session is gone; the client reconnects unless it was asked to stop.
    SessionEnded {
        /// The outcome as `Debug` renders it, for example `Closed`.
        reason: String,
    },
    /// The client is waiting out its backoff before the next registration attempt.
    Reconnecting {
        /// Why the previous attempt ended.
        reason: String,
        /// How long the client waits before trying again.
        delay_ms: u64,
    },
    /// Something went wrong; the client carries on regardless.
    Warn {
        /// Human-readable description of the failure.
        message: String,
    },
    /// The capture pipeline is now running this parameter set.
    ///
    /// Reported for every deliberate change — a `switch_camera`, the codec of a
    /// `start_recording`, a switch the embedding application asked for — and for the set a
    /// session starts from, so a caller can show the parameters the camera actually runs rather
    /// than the ones it asked for.
    ParametersApplied {
        /// Camera the parameters belong to, by its `camera_enum`.
        camera_enum: u32,
        /// Protocol resolution, for example `1920x1080`.
        resolution: String,
        /// Frame rate in frames per second.
        fps: u32,
        /// Codec the frames are encoded with, spelled as the protocol spells it (`h264`).
        codec: String,
        /// Encoder bitrate in kilobits per second.
        bitrate_kbps: u32,
    },
    /// Something the client wants its caller to know that is not a failure.
    ///
    /// A request that had to wait is the usual case: a parameter switch or a reconnect that
    /// arrived while a stream was live, and therefore happens when that stream ends.
    Notice {
        /// Human-readable message.
        message: String,
    },
}

/// Hands [`CoreEvent`]s to the caller without ever failing the pipeline.
///
/// Implemented for [`ClientConfig::events`](crate::ClientConfig::events), so internals can report
/// with `config.events.emit(event)` wherever the hook is optional.
pub(crate) trait Emit {
    /// Delivers `event`, dropping it when nobody listens.
    ///
    /// A closed receiver is not an error: the client's job is to capture and send media, not to
    /// keep its observer alive.
    fn emit(&self, event: CoreEvent);
}

impl Emit for Option<UnboundedSender<CoreEvent>> {
    fn emit(&self, event: CoreEvent) {
        if let Some(sender) = self {
            let _ = sender.send(event);
        }
    }
}
