//! Client configuration.

use std::fmt;
use std::path::PathBuf;

use scwc_protocol::CameraRegistration;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

use crate::CoreError;
use crate::command::UiCommand;
use crate::events::CoreEvent;
use crate::register::ws_url;

/// The server identity a client registers with.
///
/// It is the part of a [`ClientConfig`] an application may replace while the client runs,
/// through [`UiCommand::Reconnect`]: the device id and its token belong together, and the base
/// URL is what both are presented to.
#[derive(Clone, PartialEq, Eq)]
pub struct ServerConfig {
    /// Server base URL without a trailing path, for example `https://smartclass.example.edu`.
    pub server: String,
    /// Device id the token was issued for.
    pub device_id: String,
    /// Device bearer token (`wdt_...`). A credential: it is never logged and is redacted from
    /// [`Debug`] output.
    pub device_token: String,
}

impl fmt::Debug for ServerConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ServerConfig")
            .field("server", &self.server)
            .field("device_id", &self.device_id)
            .field(
                "device_token",
                &if self.device_token.is_empty() {
                    "<empty>"
                } else {
                    "<redacted>"
                },
            )
            .finish()
    }
}

/// Everything the core needs to reach one server as one device.
///
/// The server is never hardcoded: the base URL, the device identity and the camera set
/// all come from the caller (a CLI, a config file or an FFI bridge).
pub struct ClientConfig {
    /// Server base URL without a trailing path, for example `https://smartclass.example.edu`
    /// or `http://10.0.0.5:8080`. `http`/`https` (and `ws`/`wss`) are accepted; the core
    /// derives the WebSocket URL from it.
    pub server: String,
    /// Device id the token was issued for; announced on every registration.
    pub device_id: String,
    /// Device bearer token (`wdt_...`). A credential: it is never logged and is redacted
    /// from [`Debug`] output.
    pub device_token: String,
    /// Every camera this device drives, announced verbatim on every registration; the
    /// element index is the `camera_enum`.
    pub cameras: Vec<CameraRegistration>,
    /// Hook the client reports its lifecycle on; `None` — the [`new`](Self::new) default —
    /// discards every event.
    ///
    /// Best-effort: a receiver that is gone or closed never fails the client.
    pub events: Option<UnboundedSender<CoreEvent>>,
    /// File remembering the capture parameters the pipeline was last successfully switched
    /// to; `None` — the [`new`](Self::new) default — keeps the client stateless, so every
    /// session starts from the announced parameters.
    ///
    /// The file is written atomically and a missing or unusable one only means the client
    /// falls back to those announced values.
    pub state_path: Option<PathBuf>,
    /// Commands the embedding application sends while the client runs; `None` — the
    /// [`new`](Self::new) default — leaves the client driven by this configuration alone.
    ///
    /// The receiver is owned by [`run`](crate::run), which hands each command to the layer that
    /// can act on it: a switch reaches the control state of the running session (or the next
    /// one, while none is live), and a reconnect is deferred until the stream that is live when
    /// it arrives has ended, so a recording is never cut short by a configuration change.
    pub commands: Option<UnboundedReceiver<UiCommand>>,
}

impl ClientConfig {
    /// Builds a configuration from its parts.
    pub fn new(
        server: impl Into<String>,
        device_id: impl Into<String>,
        device_token: impl Into<String>,
        cameras: Vec<CameraRegistration>,
    ) -> Self {
        Self {
            server: server.into(),
            device_id: device_id.into(),
            device_token: device_token.into(),
            cameras,
            events: None,
            state_path: None,
            commands: None,
        }
    }

    /// Checks the locally verifiable invariants.
    ///
    /// Returns [`CoreError::Config`] — the one fatal error — when the client could never
    /// work, for example an unparsable server URL, an empty token or a camera whose
    /// `camera_enum` is not its index. Camera capability lists themselves are left to the
    /// server, which answers `400` with a problem detail.
    pub fn validate(&self) -> Result<(), CoreError> {
        validate_identity(&self.server, &self.device_id, &self.device_token)?;

        if self.cameras.is_empty() {
            return Err(CoreError::Config(
                "at least one camera must be announced".to_owned(),
            ));
        }

        for (index, camera) in self.cameras.iter().enumerate() {
            if camera.camera_enum as usize != index {
                return Err(CoreError::Config(format!(
                    "camera at index {index} declares camera_enum {}, which must equal its index",
                    camera.camera_enum
                )));
            }
        }

        Ok(())
    }

    /// Moves the client onto another server identity, as [`UiCommand::Reconnect`] does.
    ///
    /// The replacement is checked like the configured identity was, so a request cannot leave
    /// the client unable to register; a refused request leaves the client exactly as it was.
    pub fn set_server_config(&mut self, server_config: ServerConfig) -> Result<(), CoreError> {
        validate_identity(
            &server_config.server,
            &server_config.device_id,
            &server_config.device_token,
        )?;

        self.server = server_config.server;
        self.device_id = server_config.device_id;
        self.device_token = server_config.device_token;
        Ok(())
    }
}

/// Checks the parts of a configuration that name a server and the device it drives.
fn validate_identity(server: &str, device_id: &str, device_token: &str) -> Result<(), CoreError> {
    ws_url(server, "/")?;

    if device_id.trim().is_empty() {
        return Err(CoreError::Config("device_id must not be empty".to_owned()));
    }
    if device_token.trim().is_empty() {
        return Err(CoreError::Config(
            "device_token must not be empty".to_owned(),
        ));
    }

    Ok(())
}

impl fmt::Debug for ClientConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ClientConfig")
            .field("server", &self.server)
            .field("device_id", &self.device_id)
            .field(
                "device_token",
                &if self.device_token.is_empty() {
                    "<empty>"
                } else {
                    "<redacted>"
                },
            )
            .field("cameras", &self.cameras)
            .field("events", &self.events.is_some())
            .field("state_path", &self.state_path)
            .field("commands", &self.commands.is_some())
            .finish()
    }
}
