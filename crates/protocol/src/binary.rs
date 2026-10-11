use serde::{Deserialize, Serialize};

use crate::control::Channel;
use crate::{ProtocolError, Result};

/// Maximum JSON header length, in bytes, a binary frame may declare.
pub const MAX_HEADER_LEN: usize = 65_536;

/// Maximum total size of a binary frame (length prefix + header + body), in bytes.
///
/// A frame must stay strictly below this value. Reaching it trips the server's
/// WebSocket read limit, which closes the connection with close code `1009`, so
/// media that cannot fit must be refused locally instead.
pub const MAX_FRAME_LEN: usize = 16 * 1024 * 1024;

/// Size of the big-endian `uint32` header length prefix, in bytes.
const LENGTH_PREFIX_LEN: usize = 4;

/// Binary media frame header: `channel`, `type` and a typed payload.
///
/// It is serialized to UTF-8 JSON, prefixed by its length and followed by the raw
/// media bytes; [`encode_frame`] and [`decode_frame`] own that framing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BinaryHeader {
    /// Channel the frame belongs to: [`Channel::Recording`] or [`Channel::Photo`].
    pub channel: Channel,
    /// Message type; must agree with the payload variant.
    #[serde(rename = "type")]
    pub msg_type: MediaType,
    /// Media-specific payload.
    pub payload: MediaPayload,
}

impl BinaryHeader {
    /// Header for one encoded video frame on the `recording` channel.
    pub fn frame(payload: FramePayload) -> Self {
        Self {
            channel: Channel::Recording,
            msg_type: MediaType::Frame,
            payload: MediaPayload::Frame(payload),
        }
    }

    /// Header for one still image on the `photo` channel.
    pub fn photo(payload: PhotoPayload) -> Self {
        Self {
            channel: Channel::Photo,
            msg_type: MediaType::Photo,
            payload: MediaPayload::Photo(payload),
        }
    }

    /// Rejects headers whose `channel`/`type` pair does not match their payload variant.
    fn check(&self) -> Result<()> {
        let (channel, msg_type) = match &self.payload {
            MediaPayload::Frame(_) => (Channel::Recording, MediaType::Frame),
            MediaPayload::Photo(_) => (Channel::Photo, MediaType::Photo),
        };

        if self.channel != channel || self.msg_type != msg_type {
            return Err(ProtocolError::HeaderMismatch {
                channel: self.channel,
                msg_type: self.msg_type,
            });
        }

        Ok(())
    }
}

/// Binary `type` value; the media channels carry exactly one each.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MediaType {
    /// One encoded video frame.
    Frame,
    /// One still image.
    Photo,
}

/// Payload of a binary media header, discriminated by `type`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum MediaPayload {
    /// Payload of a `recording`/`frame` message.
    Frame(FramePayload),
    /// Payload of a `photo`/`photo` message.
    Photo(PhotoPayload),
}

/// Payload of a `recording`/`frame` message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FramePayload {
    /// Camera that produced the frame.
    pub camera_enum: u32,
    /// Active stream the frame belongs to; must match the `start_recording` command.
    pub stream_id: String,
    /// Monotonically increasing per-stream counter, starting at `0`.
    ///
    /// The server stores frames in arrival order and only reads the first and last
    /// value of a flush batch, so `seq` exists for consumers to detect gaps.
    #[serde(default)]
    pub seq: u64,
    /// Capture time, RFC3339Nano in UTC.
    ///
    /// Absent when the device has no clock reading; the server then falls back to the
    /// stream's registered `fps` to estimate segment durations.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub ts: String,
}

/// Payload of a `photo`/`photo` message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PhotoPayload {
    /// Camera that captured the image.
    pub camera_enum: u32,
    /// Correlation id copied from the `take_photo` command; absent when unsolicited.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    /// Media type of the body; absent means the canonical `image/jpeg`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_type: Option<String>,
    /// Capture time, RFC3339Nano in UTC; absent means the server's receive time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ts: Option<String>,
}

/// Frames `header` and `body` into one binary WebSocket message.
///
/// The result is a big-endian `uint32` header length, the UTF-8 JSON header and the
/// raw body. A frame that would be rejected by the server — header length outside
/// `1..=65536` bytes or a total size reaching [`MAX_FRAME_LEN`] — is refused here,
/// where the device can still react instead of losing the connection.
pub fn encode_frame(header: &BinaryHeader, body: &[u8]) -> Result<Vec<u8>> {
    header.check()?;

    let header_bytes = serde_json::to_vec(header)?;
    let header_len = header_bytes.len();
    if !(1..=MAX_HEADER_LEN).contains(&header_len) {
        return Err(ProtocolError::HeaderLengthOutOfRange { len: header_len });
    }

    let total_len = LENGTH_PREFIX_LEN + header_len + body.len();
    if total_len >= MAX_FRAME_LEN {
        return Err(ProtocolError::FrameTooLarge {
            len: total_len,
            limit: MAX_FRAME_LEN,
        });
    }

    let mut frame = Vec::with_capacity(total_len);
    frame.extend_from_slice(&(header_len as u32).to_be_bytes());
    frame.extend_from_slice(&header_bytes);
    frame.extend_from_slice(body);

    Ok(frame)
}

/// Splits one binary WebSocket message into its header and raw body.
///
/// Used by tests and mocks; the device itself only ever encodes frames. The frame is
/// validated exactly like the server's decoder would, and the returned body borrows
/// from `frame`.
pub fn decode_frame(frame: &[u8]) -> Result<(BinaryHeader, &[u8])> {
    if frame.len() < LENGTH_PREFIX_LEN {
        return Err(ProtocolError::Truncated {
            needed: LENGTH_PREFIX_LEN,
            available: frame.len(),
        });
    }

    if frame.len() >= MAX_FRAME_LEN {
        return Err(ProtocolError::FrameTooLarge {
            len: frame.len(),
            limit: MAX_FRAME_LEN,
        });
    }

    let header_len = u32::from_be_bytes([frame[0], frame[1], frame[2], frame[3]]) as usize;
    if !(1..=MAX_HEADER_LEN).contains(&header_len) {
        return Err(ProtocolError::HeaderLengthOutOfRange { len: header_len });
    }

    let header_end = LENGTH_PREFIX_LEN + header_len;
    if frame.len() < header_end {
        return Err(ProtocolError::Truncated {
            needed: header_end,
            available: frame.len(),
        });
    }

    let header: BinaryHeader = serde_json::from_slice(&frame[LENGTH_PREFIX_LEN..header_end])?;
    header.check()?;

    Ok((header, &frame[header_end..]))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Header JSON exactly as [`encode_frame`] produces it for the documented frame example.
    const FRAME_HEADER_JSON: &str = concat!(
        r#"{"channel":"recording","type":"frame","payload":{"camera_enum":0,"#,
        r#""stream_id":"01J8ZKQ3B5N7P9R1T3V5X7Z9B2","seq":42,"ts":"2026-10-04T10:00:00Z"}}"#
    );

    const FRAME_BODY: &[u8] = &[
        0x00, 0x00, 0x00, 0x01, 0x67, 0x42, 0x00, 0x1e, 0x90, 0xa1, 0xb2, 0xc3, 0xd4, 0xe5, 0xf6,
        0xaa,
    ];

    const PHOTO_BODY: &[u8] = &[
        0xff, 0xd8, 0xff, 0xe0, 0x00, 0x10, 0x4a, 0x46, 0x49, 0x46, 0x00, 0x01,
    ];

    fn documented_frame() -> BinaryHeader {
        BinaryHeader::frame(FramePayload {
            camera_enum: 0,
            stream_id: "01J8ZKQ3B5N7P9R1T3V5X7Z9B2".to_owned(),
            seq: 42,
            ts: "2026-10-04T10:00:00Z".to_owned(),
        })
    }

    fn documented_photo() -> BinaryHeader {
        BinaryHeader::photo(PhotoPayload {
            camera_enum: 0,
            request_id: Some("01J8ZKQ3B5N7P9R1T3V5X7Z9B3".to_owned()),
            content_type: Some("image/jpeg".to_owned()),
            ts: Some("2026-10-04T10:00:05Z".to_owned()),
        })
    }

    #[test]
    fn encodes_and_decodes_documented_frame() {
        let header = documented_frame();
        let frame = encode_frame(&header, FRAME_BODY).unwrap();

        // 4 byte prefix + 144 byte header + 16 byte body, per the protocol example.
        assert_eq!(frame.len(), 164);
        assert_eq!(u32::from_be_bytes([frame[0], frame[1], frame[2], frame[3]]), 144);
        assert_eq!(&frame[4..148], FRAME_HEADER_JSON.as_bytes());
        assert_eq!(&frame[148..], FRAME_BODY);

        let (decoded, body) = decode_frame(&frame).unwrap();
        assert_eq!(decoded, header);
        assert_eq!(body, FRAME_BODY);
    }

    #[test]
    fn encodes_and_decodes_documented_photo() {
        let header = documented_photo();
        let frame = encode_frame(&header, PHOTO_BODY).unwrap();

        // 4 byte prefix + 160 byte header + 12 byte body, per the protocol example.
        assert_eq!(frame.len(), 176);
        assert_eq!(u32::from_be_bytes([frame[0], frame[1], frame[2], frame[3]]), 160);
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&frame[4..164]).unwrap(),
            serde_json::json!({
                "channel": "photo",
                "type": "photo",
                "payload": {
                    "camera_enum": 0,
                    "content_type": "image/jpeg",
                    "request_id": "01J8ZKQ3B5N7P9R1T3V5X7Z9B3",
                    "ts": "2026-10-04T10:00:05Z"
                }
            })
        );
        assert_eq!(&frame[164..], PHOTO_BODY);

        let (decoded, body) = decode_frame(&frame).unwrap();
        assert_eq!(decoded, header);
        assert_eq!(body, PHOTO_BODY);
    }

    #[test]
    fn decodes_frame_without_optional_fields() {
        let header_json = br#"{"channel":"recording","type":"frame",
            "payload":{"camera_enum":3,"stream_id":"01J8ZKQ3B5N7P9R1T3V5X7Z9B2"}}"#;
        let mut frame = (header_json.len() as u32).to_be_bytes().to_vec();
        frame.extend_from_slice(header_json);

        let (header, body) = decode_frame(&frame).unwrap();
        assert_eq!(
            header,
            BinaryHeader::frame(FramePayload {
                camera_enum: 3,
                stream_id: "01J8ZKQ3B5N7P9R1T3V5X7Z9B2".to_owned(),
                seq: 0,
                ts: String::new(),
            })
        );
        assert!(body.is_empty());
    }

    #[test]
    fn rejects_zero_header_length() {
        assert!(matches!(
            decode_frame(&[0x00, 0x00, 0x00, 0x00, b'{', b'}']),
            Err(ProtocolError::HeaderLengthOutOfRange { len: 0 })
        ));
    }

    #[test]
    fn rejects_declared_header_length_above_max() {
        let mut frame = vec![0u8; 8];
        frame[..4].copy_from_slice(&(MAX_HEADER_LEN as u32 + 1).to_be_bytes());

        assert!(matches!(
            decode_frame(&frame),
            Err(ProtocolError::HeaderLengthOutOfRange { len }) if len == MAX_HEADER_LEN + 1
        ));
    }

    #[test]
    fn rejects_produced_header_length_above_max() {
        let header = BinaryHeader::frame(FramePayload {
            camera_enum: 0,
            stream_id: "s".repeat(MAX_HEADER_LEN),
            seq: 0,
            ts: String::new(),
        });

        assert!(matches!(
            encode_frame(&header, &[]),
            Err(ProtocolError::HeaderLengthOutOfRange { len }) if len > MAX_HEADER_LEN
        ));
    }

    #[test]
    fn rejects_frame_at_or_above_limit() {
        let header = documented_frame();

        assert!(matches!(
            encode_frame(&header, &vec![0u8; MAX_FRAME_LEN]),
            Err(ProtocolError::FrameTooLarge { len, limit })
                if len > MAX_FRAME_LEN && limit == MAX_FRAME_LEN
        ));

        assert!(matches!(
            decode_frame(&vec![0u8; MAX_FRAME_LEN]),
            Err(ProtocolError::FrameTooLarge { len, limit })
                if len == MAX_FRAME_LEN && limit == MAX_FRAME_LEN
        ));
    }

    #[test]
    fn accepts_frame_just_below_limit() {
        let header = documented_frame();
        let body = vec![0u8; MAX_FRAME_LEN - 4 - FRAME_HEADER_JSON.len() - 1];
        let frame = encode_frame(&header, &body).unwrap();

        assert_eq!(frame.len(), MAX_FRAME_LEN - 1);
        assert_eq!(decode_frame(&frame).unwrap().1.len(), body.len());
    }

    #[test]
    fn rejects_truncated_frames() {
        assert!(matches!(
            decode_frame(&[0x00, 0x01, 0x02]),
            Err(ProtocolError::Truncated {
                needed: 4,
                available: 3
            })
        ));

        let mut frame = vec![0u8; 8];
        frame[..4].copy_from_slice(&100u32.to_be_bytes());
        assert!(matches!(
            decode_frame(&frame),
            Err(ProtocolError::Truncated {
                needed: 104,
                available: 8
            })
        ));
    }

    #[test]
    fn rejects_mismatched_channel_and_type() {
        let header_json = br#"{"channel":"photo","type":"frame","payload":{"camera_enum":0,"stream_id":"s"}}"#;
        let mut frame = (header_json.len() as u32).to_be_bytes().to_vec();
        frame.extend_from_slice(header_json);

        assert!(matches!(
            decode_frame(&frame),
            Err(ProtocolError::HeaderMismatch {
                channel: Channel::Photo,
                msg_type: MediaType::Frame,
            })
        ));
    }

    #[test]
    fn rejects_control_channel_in_binary_frame() {
        let header_json = br#"{"channel":"control","type":"frame","payload":{"camera_enum":0,"stream_id":"s"}}"#;
        let mut frame = (header_json.len() as u32).to_be_bytes().to_vec();
        frame.extend_from_slice(header_json);

        assert!(matches!(
            decode_frame(&frame),
            Err(ProtocolError::HeaderMismatch {
                channel: Channel::Control,
                msg_type: MediaType::Frame,
            })
        ));
    }

    #[test]
    fn rejects_malformed_header_json() {
        let mut frame = 2u32.to_be_bytes().to_vec();
        frame.extend_from_slice(b"[]");

        assert!(matches!(
            decode_frame(&frame),
            Err(ProtocolError::Json(_))
        ));
    }

    #[test]
    fn rejects_header_without_media_payload() {
        let header_json = br#"{"channel":"recording","type":"frame","payload":{"camera_enum":0}}"#;
        let mut frame = (header_json.len() as u32).to_be_bytes().to_vec();
        frame.extend_from_slice(header_json);

        assert!(matches!(
            decode_frame(&frame),
            Err(ProtocolError::Json(_) | ProtocolError::HeaderMismatch { .. })
        ));
    }
}
