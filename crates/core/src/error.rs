//! Error types of the device-client core.

use scwc_protocol::ProtocolError;
use tokio_tungstenite::tungstenite::Error as WebSocketError;

/// Result alias for fallible core operations.
pub type Result<T, E = CoreError> = std::result::Result<T, E>;

/// Failure of a device-client core operation.
///
/// The status variants are produced for both halves of the exchange: a non-success
/// response to `GET /ws/register` and a rejected WebSocket upgrade, where the ticket
/// status is the useful part.
#[derive(Debug, thiserror::Error)]
pub enum CoreError {
    /// The configuration cannot produce a usable client.
    ///
    /// This is the only fatal error: [`run`](crate::run) returns it instead of retrying.
    #[error("invalid configuration: {0}")]
    Config(String),
    /// The device token was rejected (`401`); the token is wrong or revoked.
    #[error("device token rejected (HTTP 401): {0}")]
    Unauthorized(String),
    /// The ticket is unknown or expired (`404`); restart at registration.
    #[error("ticket unknown or expired (HTTP 404): {0}")]
    NotFound(String),
    /// The ticket is still attached to another connection (`409`); restart at registration.
    #[error("ticket already attached (HTTP 409): {0}")]
    Conflict(String),
    /// Any other non-success status, for example `400`, `413` or `500`.
    #[error("server returned HTTP {status}: {detail}")]
    Http {
        /// HTTP status code the server answered with.
        status: u16,
        /// Server-supplied problem detail, or a stand-in when the body was unusable.
        detail: String,
    },
    /// The HTTP request itself failed (DNS, connect, TLS, timeout, body).
    #[error("registration transport failure: {0}")]
    Transport(#[from] reqwest::Error),
    /// The WebSocket failed outside the handshake's HTTP status.
    #[error("websocket failure: {0}")]
    WebSocket(#[from] WebSocketError),
    /// A wire-format message could not be encoded or decoded.
    #[error("protocol violation: {0}")]
    Protocol(#[from] ProtocolError),
    /// JSON serialization or deserialization failed outside the protocol types.
    #[error("json failure: {0}")]
    Json(#[from] serde_json::Error),
}

impl CoreError {
    /// True when retrying cannot help: the configuration itself is broken.
    pub fn is_fatal(&self) -> bool {
        matches!(self, Self::Config(_))
    }

    /// Classifies an HTTP status code into the matching variant.
    ///
    /// `401`, `404` and `409` carry behaviour (bad token, unknown/expired ticket, ticket
    /// still attached) and get their own variants; everything else keeps its raw status.
    pub(crate) fn from_status(status: u16, detail: impl Into<String>) -> Self {
        let detail = detail.into();
        match status {
            401 => Self::Unauthorized(detail),
            404 => Self::NotFound(detail),
            409 => Self::Conflict(detail),
            status => Self::Http { status, detail },
        }
    }
}

/// Failure reported by a [`FrameSource`](crate::FrameSource).
///
/// Sources are platform code (or test doubles); the core only needs a message it can put
/// into `ack {ok: false, error}` and its logs.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct SourceError(String);

impl SourceError {
    /// Wraps a human-readable failure message.
    pub fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }

    /// The failure message.
    pub fn message(&self) -> &str {
        &self.0
    }
}

impl From<String> for SourceError {
    fn from(message: String) -> Self {
        Self(message)
    }
}

impl From<&str> for SourceError {
    fn from(message: &str) -> Self {
        Self(message.to_owned())
    }
}
