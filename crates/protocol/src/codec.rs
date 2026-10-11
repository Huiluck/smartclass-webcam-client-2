use serde::{Deserialize, Serialize};

/// Video codec identifiers accepted by the protocol's `supported_codec` list.
///
/// The vocabulary is closed and case-sensitive: the serialized names are exactly
/// `h264`, `h265`, `mjpeg`, `mpeg4`, `vp8`, `vp9` and `av1`, with no aliases
/// (`hevc` is rejected by the server) and no case folding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Codec {
    /// H.264/AVC.
    H264,
    /// H.265/HEVC.
    H265,
    /// Motion JPEG.
    Mjpeg,
    /// MPEG-4 Part 2.
    Mpeg4,
    /// Google VP8.
    Vp8,
    /// Google VP9.
    Vp9,
    /// AOMedia AV1.
    Av1,
}

#[cfg(test)]
mod tests {
    use super::Codec;
    use serde_json::json;

    #[test]
    fn round_trips_closed_vocabulary() {
        let vocabulary = [
            (Codec::H264, "h264"),
            (Codec::H265, "h265"),
            (Codec::Mjpeg, "mjpeg"),
            (Codec::Mpeg4, "mpeg4"),
            (Codec::Vp8, "vp8"),
            (Codec::Vp9, "vp9"),
            (Codec::Av1, "av1"),
        ];

        for (codec, name) in vocabulary {
            assert_eq!(serde_json::to_value(codec).unwrap(), json!(name));
            assert_eq!(serde_json::from_value::<Codec>(json!(name)).unwrap(), codec);
        }
    }

    #[test]
    fn rejects_aliases_and_case_variants() {
        for rejected in ["hevc", "H264", "H265", "MJPEG", "h264 ", ""] {
            assert!(
                serde_json::from_value::<Codec>(json!(rejected)).is_err(),
                "{rejected:?} must not decode as a codec"
            );
        }
    }
}
