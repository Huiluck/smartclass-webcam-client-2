use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::Codec;

/// One entry of the `cameras` array announced by `GET /ws/register`.
///
/// `resolution` and `fps` are the parameters this camera is *at* for the current
/// connection, not a free choice: only a `switch_camera` command changes them, and
/// the server rejects a registration whose current pair is missing from the
/// corresponding supported list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CameraRegistration {
    /// Camera index; must equal the element's index in the `cameras` array.
    pub camera_enum: u32,
    /// Resolution the camera is currently at, conventionally `WIDTHxHEIGHT`.
    pub resolution: String,
    /// Frame rate the camera is currently at.
    pub fps: u32,
    /// Every resolution the camera accepts; at least one entry.
    pub supported_resolutions: Vec<String>,
    /// Every frame rate the camera accepts; at least one entry.
    pub supported_framerates: Vec<u32>,
    /// Codecs the camera can produce, the preferred one first; at least one entry.
    pub supported_codec: Vec<Codec>,
    /// Free-form device-specific data, for example a camera label.
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub attrs: Map<String, Value>,
}

/// Body of `GET /ws/register`, carrying the device's full camera set.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegisterRequest {
    /// Id of the device whose bearer token is presented.
    pub device_id: String,
    /// Every camera the device drives; the order defines the camera numbering.
    pub cameras: Vec<CameraRegistration>,
}

/// `200 OK` body of `GET /ws/register`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegisterResponse {
    /// Single-use WebSocket ticket: 64 lowercase hex characters.
    pub device_websocket_id: String,
    /// Expiry of the pending ticket, RFC3339 with sub-second precision in UTC.
    pub expires_at: String,
    /// Ready-to-use upgrade path, `/ws/device/` followed by the ticket.
    pub websocket_path: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const REGISTER_REQUEST: &str = r#"{
        "device_id": "01J8ZK9WQ7X3YV0M4N5P6Q7R8S",
        "cameras": [
            {
                "camera_enum": 0,
                "resolution": "1920x1080",
                "fps": 30,
                "supported_resolutions": ["1920x1080", "1280x720"],
                "supported_framerates": [30, 15],
                "supported_codec": ["h264", "h265", "mjpeg"],
                "attrs": {"label": "front"}
            },
            {
                "camera_enum": 1,
                "resolution": "1280x720",
                "fps": 15,
                "supported_resolutions": ["1280x720"],
                "supported_framerates": [15],
                "supported_codec": ["mjpeg"]
            }
        ]
    }"#;

    const REGISTER_RESPONSE: &str = r#"{
        "device_websocket_id": "0b30557a9fc4e90e33587da2c7ec11365b80a5caef14395e83a8cdf2173c6186",
        "expires_at": "2026-10-04T10:01:00Z",
        "websocket_path": "/ws/device/0b30557a9fc4e90e33587da2c7ec11365b80a5caef14395e83a8cdf2173c6186"
    }"#;

    #[test]
    fn decodes_documented_registration_request() {
        let request: RegisterRequest = serde_json::from_str(REGISTER_REQUEST).unwrap();

        assert_eq!(request.device_id, "01J8ZK9WQ7X3YV0M4N5P6Q7R8S");
        assert_eq!(request.cameras.len(), 2);

        let first = &request.cameras[0];
        assert_eq!(first.camera_enum, 0);
        assert_eq!(first.resolution, "1920x1080");
        assert_eq!(first.fps, 30);
        assert_eq!(first.supported_resolutions, ["1920x1080", "1280x720"]);
        assert_eq!(first.supported_framerates, [30, 15]);
        assert_eq!(
            first.supported_codec,
            [Codec::H264, Codec::H265, Codec::Mjpeg]
        );
        assert_eq!(first.attrs.get("label"), Some(&json!("front")));

        let second = &request.cameras[1];
        assert_eq!(second.camera_enum, 1);
        assert_eq!(second.supported_codec, [Codec::Mjpeg]);
        assert!(second.attrs.is_empty());

        let original: Value = serde_json::from_str(REGISTER_REQUEST).unwrap();
        assert_eq!(serde_json::to_value(&request).unwrap(), original);
    }

    #[test]
    fn omits_absent_attrs() {
        let camera = CameraRegistration {
            camera_enum: 0,
            resolution: "3840x2160".to_owned(),
            fps: 60,
            supported_resolutions: vec!["3840x2160".to_owned(), "1920x1080".to_owned()],
            supported_framerates: vec![60, 30],
            supported_codec: vec![Codec::H265, Codec::H264],
            attrs: Map::new(),
        };

        assert_eq!(serde_json::to_value(&camera).unwrap(), json!({
            "camera_enum": 0,
            "resolution": "3840x2160",
            "fps": 60,
            "supported_resolutions": ["3840x2160", "1920x1080"],
            "supported_framerates": [60, 30],
            "supported_codec": ["h265", "h264"]
        }));
    }

    #[test]
    fn decodes_documented_registration_response() {
        let response: RegisterResponse = serde_json::from_str(REGISTER_RESPONSE).unwrap();

        assert_eq!(
            response.device_websocket_id,
            "0b30557a9fc4e90e33587da2c7ec11365b80a5caef14395e83a8cdf2173c6186"
        );
        assert_eq!(response.expires_at, "2026-10-04T10:01:00Z");
        assert_eq!(
            response.websocket_path,
            "/ws/device/0b30557a9fc4e90e33587da2c7ec11365b80a5caef14395e83a8cdf2173c6186"
        );

        let original: Value = serde_json::from_str(REGISTER_RESPONSE).unwrap();
        assert_eq!(serde_json::to_value(&response).unwrap(), original);
    }

    #[test]
    fn keeps_sub_second_expiry_verbatim() {
        let response: RegisterResponse = serde_json::from_str(
            r#"{"device_websocket_id":"0b30557a","expires_at":"2026-10-04T10:01:00.512384921Z","websocket_path":"/ws/device/0b30557a"}"#,
        )
        .unwrap();

        assert_eq!(response.expires_at, "2026-10-04T10:01:00.512384921Z");
    }
}
