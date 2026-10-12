//! Requests an embedding application sends into a running client.
//!
//! A client is configured once, by [`ClientConfig`](crate::ClientConfig), but an application
//! that shows a user what the device is doing needs to move it while it runs: onto another
//! resolution, or onto another server. [`UiCommand`] is that vocabulary, and it deliberately
//! describes the same two things the server can ask for, so both travel the same code paths.

use scwc_protocol::Codec;

use crate::config::ServerConfig;

/// Capture parameters an application asks the client to move the pipeline to.
///
/// Every field is optional: a `None` keeps what the camera is believed to be at, and a `None`
/// bitrate asks the client to derive one, which is what an empty bitrate field in a user
/// interface means — the protocol carries no bitrate at all, so the client computes one from
/// the codec, the resolution and the frame rate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SwitchParams {
    /// Camera to switch, by its `camera_enum`.
    pub camera_enum: u32,
    /// Protocol resolution, for example `1920x1080`; `None` keeps the current one.
    pub resolution: Option<String>,
    /// Frame rate in frames per second; `None` keeps the current one.
    pub fps: Option<u32>,
    /// Codec to encode with; `None` keeps the current one.
    pub codec: Option<Codec>,
    /// Encoder bitrate in kilobits per second; `None` derives it from the rest.
    pub bitrate_kbps: Option<u32>,
}

/// A request from the embedding application to a running client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UiCommand {
    /// Move the capture pipeline onto a parameter set.
    ///
    /// Treated exactly like a server-issued `switch_camera`: applied at once while no stream
    /// runs, and, while one does, remembered latest-wins and applied when that stream ends so a
    /// recording keeps the parameters it started with. A successful switch is persisted and
    /// reported as [`CoreEvent::ParametersApplied`](crate::CoreEvent::ParametersApplied).
    SwitchParams(SwitchParams),
    /// Register again, with the credentials this command carries instead of the configured ones.
    ///
    /// The client reconnects at the first opportunity: right away while no stream is live, and
    /// once the live one ends otherwise (see
    /// [`ClientConfig::commands`](crate::ClientConfig::commands)).
    Reconnect(ServerConfig),
}
