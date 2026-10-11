use serde::{de::DeserializeOwned, Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::{Codec, Result};

/// Envelope `channel` value; the protocol's complete vocabulary.
///
/// Only [`Channel::Control`] travels as text WebSocket frames; the other two are
/// carried inside the binary media framing (see [`crate::BinaryHeader`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Channel {
    /// Control channel: text frames in both directions.
    Control,
    /// Recording channel: binary video frames from the device.
    Recording,
    /// Photo channel: binary still images from the device.
    Photo,
}

/// Text-frame envelope carried on the `control` channel.
///
/// The same envelope shape prefixes binary media frames, where `payload` holds a
/// [`FramePayload`](crate::FramePayload) or [`PhotoPayload`](crate::PhotoPayload)
/// instead of a control payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Envelope {
    /// Channel the message belongs to; [`Channel::Control`] for text frames.
    pub channel: Channel,
    /// Message type, for example `start_recording` or `ack`.
    ///
    /// Kept as an open string so unknown types can be logged and ignored instead of
    /// failing to decode, as the protocol requires of forward-compatible peers.
    #[serde(rename = "type")]
    pub msg_type: String,
    /// Command identifier, echoed verbatim in the matching [`Envelope::ack`];
    /// absent on `ping` and normally absent on `pong`, `status` and `error`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// Type-specific payload; absent when the message carries none.
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub payload: Value,
}

impl Envelope {
    /// Builds a `control` envelope with a typed payload.
    pub fn typed<T: Serialize>(msg_type: impl Into<String>, payload: &T) -> Result<Self> {
        Ok(Self {
            channel: Channel::Control,
            msg_type: msg_type.into(),
            id: None,
            payload: serde_json::to_value(payload)?,
        })
    }

    /// Attaches the command id this message answers.
    pub fn with_id(mut self, id: impl Into<String>) -> Self {
        self.id = Some(id.into());
        self
    }

    /// Ack for a server command, echoing `id` exactly as received.
    pub fn ack(id: impl Into<String>, payload: &AckPayload) -> Result<Self> {
        Ok(Self::typed("ack", payload)?.with_id(id))
    }

    /// Successful ack: `{"ok": true}`.
    pub fn ack_ok(id: impl Into<String>) -> Result<Self> {
        Self::ack(id, &AckPayload::ok())
    }

    /// Failed ack: `{"ok": false, "error": ...}`.
    pub fn ack_error(id: impl Into<String>, error: impl Into<String>) -> Result<Self> {
        Self::ack(id, &AckPayload::failure(error))
    }

    /// Keepalive answer to a server `ping`.
    ///
    /// `ts` echoes the value from [`PingPayload`] when it was available; the device
    /// must not invent an `id` for a `pong`.
    pub fn pong(ts: Option<&str>) -> Result<Self> {
        Self::typed(
            "pong",
            &PongPayload {
                ts: ts.map(str::to_owned),
            },
        )
    }

    /// Device-initiated state report; the server logs the payload and nothing more.
    pub fn status(payload: &StatusPayload) -> Result<Self> {
        Self::typed("status", payload)
    }

    /// Device-reported problem, optionally attached to a command via [`Envelope::with_id`].
    pub fn error(message: impl Into<String>) -> Result<Self> {
        Self::typed(
            "error",
            &ErrorPayload {
                message: Some(message.into()),
            },
        )
    }

    /// Deserializes [`Envelope::payload`] into a typed payload struct.
    ///
    /// Returns [`ProtocolError::Json`](crate::ProtocolError::Json) when the payload does
    /// not match `T`, which is how a malformed command from the server is detected.
    pub fn payload_as<T: DeserializeOwned>(&self) -> Result<T> {
        Ok(T::deserialize(&self.payload)?)
    }
}

/// Payload of the server `switch_camera` command.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SwitchCameraPayload {
    /// Camera that must become the active one.
    pub camera_enum: u32,
    /// Resolution to switch to; absent keeps the resolution the camera is at.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolution: Option<String>,
    /// Frame rate to switch to; absent keeps the frame rate the camera is at.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fps: Option<u32>,
}

/// Payload of the server `start_recording` command.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StartRecordingPayload {
    /// Camera to record.
    pub camera_enum: u32,
    /// Opaque server-issued stream id the frames must carry.
    pub stream_id: String,
    /// Requested codec; absent means the device's preferred one, the first entry of
    /// `supported_codec`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codec: Option<Codec>,
}

/// Payload of the server `stop_recording` command.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StopRecordingPayload {
    /// Camera whose stream must stop.
    pub camera_enum: u32,
    /// Stream the device must stop feeding.
    pub stream_id: String,
}

/// Payload of the server `take_photo` command.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TakePhotoPayload {
    /// Camera to capture with.
    pub camera_enum: u32,
    /// Correlation id the resulting photo must echo.
    pub request_id: String,
}

/// Payload of the server `ping` keepalive.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PingPayload {
    /// Server clock time, RFC3339Nano in UTC; informational.
    pub ts: String,
}

/// Payload of the device `pong` keepalive answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PongPayload {
    /// Echo of the `ping` timestamp, when the ping carried one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ts: Option<String>,
}

/// Payload of the device `ack` message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AckPayload {
    /// `true` when the command succeeded.
    pub ok: bool,
    /// Human-readable failure reason; expected whenever `ok` is `false`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl AckPayload {
    /// Success payload: `{"ok": true}`.
    pub fn ok() -> Self {
        Self {
            ok: true,
            error: None,
        }
    }

    /// Failure payload: `{"ok": false, "error": ...}`.
    pub fn failure(error: impl Into<String>) -> Self {
        Self {
            ok: false,
            error: Some(error.into()),
        }
    }
}

/// Payload of the device `status` report.
///
/// The server imposes no schema, so any JSON object is valid; use [`StatusPayload::from`]
/// or insert into [`StatusPayload::fields`] directly.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusPayload {
    /// Free-form report entries.
    #[serde(flatten)]
    pub fields: Map<String, Value>,
}

/// Payload of the device `error` report.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorPayload {
    /// Free-form description of the problem.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const SWITCH_CAMERA: &str = r#"{
        "channel": "control",
        "type": "switch_camera",
        "id": "01J8ZKQ3B5N7P9R1T3V5X7Z9B1",
        "payload": {"camera_enum": 1, "resolution": "1280x720", "fps": 15}
    }"#;

    const START_RECORDING: &str = r#"{
        "channel": "control",
        "type": "start_recording",
        "id": "01J8ZKQ3B5N7P9R1T3V5X7Z9B1",
        "payload": {
            "camera_enum": 0,
            "stream_id": "01J8ZKQ3B5N7P9R1T3V5X7Z9B2",
            "codec": "h264"
        }
    }"#;

    const STOP_RECORDING: &str = r#"{
        "channel": "control",
        "type": "stop_recording",
        "id": "01J8ZKQ3B5N7P9R1T3V5X7Z9B1",
        "payload": {
            "camera_enum": 0,
            "stream_id": "01J8ZKQ3B5N7P9R1T3V5X7Z9B2"
        }
    }"#;

    const TAKE_PHOTO: &str = r#"{
        "channel": "control",
        "type": "take_photo",
        "id": "01J8ZKQ3B5N7P9R1T3V5X7Z9B1",
        "payload": {
            "camera_enum": 0,
            "request_id": "01J8ZKQ3B5N7P9R1T3V5X7Z9B3"
        }
    }"#;

    const PING: &str = r#"{
        "channel": "control",
        "type": "ping",
        "payload": {"ts": "2026-10-04T10:00:30Z"}
    }"#;

    const ACK_OK: &str = r#"{
        "channel": "control",
        "type": "ack",
        "id": "01J8ZKQ3B5N7P9R1T3V5X7Z9B1",
        "payload": {"ok": true}
    }"#;

    const ACK_ERROR: &str = r#"{
        "channel": "control",
        "type": "ack",
        "id": "01J8ZKQ3B5N7P9R1T3V5X7Z9B1",
        "payload": {"ok": false, "error": "camera 0 is busy"}
    }"#;

    const PONG: &str = r#"{
        "channel": "control",
        "type": "pong",
        "payload": {"ts": "2026-10-04T10:00:30Z"}
    }"#;

    const STATUS: &str = r#"{
        "channel": "control",
        "type": "status",
        "payload": {
            "ts": "2026-10-04T10:00:31Z",
            "active_camera": 1,
            "recording": false
        }
    }"#;

    const ERROR: &str = r#"{
        "channel": "control",
        "type": "error",
        "id": "01J8ZKQ3B5N7P9R1T3V5X7Z9B1",
        "payload": {"message": "camera 1 is unavailable"}
    }"#;

    fn parse(document: &str) -> Envelope {
        serde_json::from_str(document).unwrap()
    }

    /// Decodes a documented example, then checks that re-encoding reproduces it exactly
    /// (field order and whitespace excluded, since both are free on the wire).
    fn assert_round_trip(document: &str, expected_type: &str) -> Envelope {
        let envelope = parse(document);
        assert_eq!(envelope.channel, Channel::Control);
        assert_eq!(envelope.msg_type, expected_type);

        let original: Value = serde_json::from_str(document).unwrap();
        let encoded = serde_json::to_value(&envelope).unwrap();
        assert_eq!(encoded, original);

        envelope
    }

    #[test]
    fn decodes_switch_camera() {
        let envelope = assert_round_trip(SWITCH_CAMERA, "switch_camera");
        assert_eq!(
            envelope.id.as_deref(),
            Some("01J8ZKQ3B5N7P9R1T3V5X7Z9B1")
        );
        assert_eq!(
            envelope.payload_as::<SwitchCameraPayload>().unwrap(),
            SwitchCameraPayload {
                camera_enum: 1,
                resolution: Some("1280x720".to_owned()),
                fps: Some(15),
            }
        );
    }

    #[test]
    fn decodes_switch_camera_without_parameters() {
        let envelope = parse(r#"{"channel":"control","type":"switch_camera","payload":{"camera_enum":0}}"#);
        assert_eq!(
            envelope.payload_as::<SwitchCameraPayload>().unwrap(),
            SwitchCameraPayload {
                camera_enum: 0,
                resolution: None,
                fps: None,
            }
        );
    }

    #[test]
    fn decodes_start_recording() {
        let envelope = assert_round_trip(START_RECORDING, "start_recording");
        assert_eq!(
            envelope.payload_as::<StartRecordingPayload>().unwrap(),
            StartRecordingPayload {
                camera_enum: 0,
                stream_id: "01J8ZKQ3B5N7P9R1T3V5X7Z9B2".to_owned(),
                codec: Some(Codec::H264),
            }
        );
    }

    #[test]
    fn decodes_stop_recording() {
        let envelope = assert_round_trip(STOP_RECORDING, "stop_recording");
        assert_eq!(
            envelope.payload_as::<StopRecordingPayload>().unwrap(),
            StopRecordingPayload {
                camera_enum: 0,
                stream_id: "01J8ZKQ3B5N7P9R1T3V5X7Z9B2".to_owned(),
            }
        );
    }

    #[test]
    fn decodes_take_photo() {
        let envelope = assert_round_trip(TAKE_PHOTO, "take_photo");
        assert_eq!(
            envelope.payload_as::<TakePhotoPayload>().unwrap(),
            TakePhotoPayload {
                camera_enum: 0,
                request_id: "01J8ZKQ3B5N7P9R1T3V5X7Z9B3".to_owned(),
            }
        );
    }

    #[test]
    fn decodes_ping() {
        let envelope = assert_round_trip(PING, "ping");
        assert_eq!(envelope.id, None);
        assert_eq!(
            envelope.payload_as::<PingPayload>().unwrap(),
            PingPayload {
                ts: "2026-10-04T10:00:30Z".to_owned(),
            }
        );
    }

    #[test]
    fn decodes_acks() {
        let success = assert_round_trip(ACK_OK, "ack");
        assert_eq!(
            success.payload_as::<AckPayload>().unwrap(),
            AckPayload {
                ok: true,
                error: None,
            }
        );

        let failure = assert_round_trip(ACK_ERROR, "ack");
        assert_eq!(
            failure.payload_as::<AckPayload>().unwrap(),
            AckPayload {
                ok: false,
                error: Some("camera 0 is busy".to_owned()),
            }
        );
    }

    #[test]
    fn decodes_pong() {
        let envelope = assert_round_trip(PONG, "pong");
        assert_eq!(
            envelope.payload_as::<PongPayload>().unwrap(),
            PongPayload {
                ts: Some("2026-10-04T10:00:30Z".to_owned()),
            }
        );
    }

    #[test]
    fn decodes_status() {
        let envelope = assert_round_trip(STATUS, "status");
        let payload = envelope.payload_as::<StatusPayload>().unwrap();
        assert_eq!(
            payload.fields.get("active_camera"),
            Some(&json!(1)),
            "free-form keys survive decoding"
        );
        assert_eq!(
            serde_json::to_value(payload).unwrap(),
            json!({"ts": "2026-10-04T10:00:31Z", "active_camera": 1, "recording": false})
        );
    }

    #[test]
    fn decodes_error() {
        let envelope = assert_round_trip(ERROR, "error");
        assert_eq!(
            envelope.payload_as::<ErrorPayload>().unwrap(),
            ErrorPayload {
                message: Some("camera 1 is unavailable".to_owned()),
            }
        );
    }

    #[test]
    fn ignores_unknown_types_and_absent_payload() {
        let envelope = parse(r#"{"channel":"control","type":"future_command"}"#);
        assert_eq!(envelope.msg_type, "future_command");
        assert_eq!(envelope.id, None);
        assert_eq!(envelope.payload, Value::Null);
    }

    #[test]
    fn builds_ack_envelopes() {
        let success = Envelope::ack_ok("01J8ZKQ3B5N7P9R1T3V5X7Z9B1").unwrap();
        assert_eq!(serde_json::to_value(&success).unwrap(), json!({
            "channel": "control",
            "type": "ack",
            "id": "01J8ZKQ3B5N7P9R1T3V5X7Z9B1",
            "payload": {"ok": true}
        }));

        let failure = Envelope::ack_error("01J8ZKQ3B5N7P9R1T3V5X7Z9B1", "camera 0 is busy").unwrap();
        assert_eq!(serde_json::to_value(&failure).unwrap(), json!({
            "channel": "control",
            "type": "ack",
            "id": "01J8ZKQ3B5N7P9R1T3V5X7Z9B1",
            "payload": {"ok": false, "error": "camera 0 is busy"}
        }));
    }

    #[test]
    fn builds_pong_envelopes() {
        let echoing = Envelope::pong(Some("2026-10-04T10:00:30Z")).unwrap();
        assert_eq!(serde_json::to_value(&echoing).unwrap(), json!({
            "channel": "control",
            "type": "pong",
            "payload": {"ts": "2026-10-04T10:00:30Z"}
        }));

        let empty = Envelope::pong(None).unwrap();
        assert_eq!(
            serde_json::to_value(&empty).unwrap(),
            json!({"channel": "control", "type": "pong", "payload": {}})
        );
    }

    #[test]
    fn builds_status_and_error_envelopes() {
        let mut fields = Map::new();
        fields.insert("recording".to_owned(), json!(false));
        let status = Envelope::status(&StatusPayload { fields }).unwrap();
        assert_eq!(
            serde_json::to_value(&status).unwrap(),
            json!({"channel": "control", "type": "status", "payload": {"recording": false}})
        );

        let error = Envelope::error("camera 1 is unavailable")
            .unwrap()
            .with_id("01J8ZKQ3B5N7P9R1T3V5X7Z9B1");
        assert_eq!(serde_json::to_value(&error).unwrap(), json!({
            "channel": "control",
            "type": "error",
            "id": "01J8ZKQ3B5N7P9R1T3V5X7Z9B1",
            "payload": {"message": "camera 1 is unavailable"}
        }));
    }

    #[test]
    fn rejects_mismatched_payload() {
        let envelope = parse(PING);
        assert!(envelope.payload_as::<AckPayload>().is_err());
    }
}
