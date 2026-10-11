//! Client configuration.

use std::fmt;

use scwc_protocol::CameraRegistration;

use crate::CoreError;
use crate::register::ws_url;

/// Everything the core needs to reach one server as one device.
///
/// The server is never hardcoded: the base URL, the device identity and the camera set
/// all come from the caller (a CLI, a config file or an FFI bridge).
#[derive(Clone)]
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
        }
    }

    /// Checks the locally verifiable invariants.
    ///
    /// Returns [`CoreError::Config`] — the one fatal error — when the client could never
    /// work, for example an unparsable server URL, an empty token or a camera whose
    /// `camera_enum` is not its index. Camera capability lists themselves are left to the
    /// server, which answers `400` with a problem detail.
    pub fn validate(&self) -> Result<(), CoreError> {
        ws_url(&self.server, "/")?;

        if self.device_id.trim().is_empty() {
            return Err(CoreError::Config("device_id must not be empty".to_owned()));
        }
        if self.device_token.trim().is_empty() {
            return Err(CoreError::Config(
                "device_token must not be empty".to_owned(),
            ));
        }
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
            .finish()
    }
}
