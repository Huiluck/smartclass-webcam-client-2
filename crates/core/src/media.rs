//! Media path: the frame-source abstraction, the per-stream sender and the drop policy.

use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use bytes::Bytes;
use scwc_protocol::{BinaryHeader, Codec, FramePayload, encode_frame};
use tokio::sync::{Mutex, mpsc, watch};
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::Message;
use tracing::{info, warn};

use crate::clock::now_rfc3339_nano;
use crate::error::SourceError;

/// Capacity of the queue between a capture task and the socket writer.
///
/// The queue absorbs a burst of frames around a keyframe-sized hiccup; once it is full,
/// delta frames are discarded until the next keyframe instead of stalling the capture
/// pipeline, while keyframes always make it through.
pub const MEDIA_QUEUE_CAPACITY: usize = 120;

/// One encoded access unit produced by a [`FrameSource`].
///
/// Exactly one frame per call: the core assigns `seq` and the capture timestamp, so the
/// source only has to hand over bytes and tell whether they start a decodable GOP.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncodedFrame {
    /// The encoded access unit; passed to the socket without a copy.
    pub data: Bytes,
    /// `true` when the frame can be decoded without earlier frames.
    pub is_keyframe: bool,
}

impl EncodedFrame {
    /// Builds a frame from its bytes and keyframe flag.
    pub fn new(data: impl Into<Bytes>, is_keyframe: bool) -> Self {
        Self {
            data: data.into(),
            is_keyframe,
        }
    }
}

/// Where the core gets its media from.
///
/// Implemented once per platform (a synthetic test pattern, VideoToolbox, MediaCodec, ...)
/// and owned by the core, which pulls frames and reconfigures the pipeline; the source
/// never talks to the server.
///
/// # How the core drives a source
///
/// The core holds an exclusive handle to the source, so the methods above never run
/// concurrently. While a stream is active it keeps calling [`FrameSource::next_frame`] and
/// holds that handle until a frame arrives, which means a [`FrameSource::set_parameters`]
/// or [`FrameSource::take_photo`] issued mid-stream waits for the next frame boundary —
/// at 60 fps that is under 20 ms, but a source that can idle for seconds should be able to
/// produce a frame (or return one promptly) rather than block reconfiguration forever.
pub trait FrameSource: Send {
    /// Produces the next encoded access unit.
    ///
    /// The core calls this only while a stream is active, and waits as long as the source
    /// needs; it paces nothing on its own. Returning a frame is the only way to make
    /// progress, so an idle camera blocks this call.
    fn next_frame(&mut self) -> impl Future<Output = EncodedFrame> + Send;

    /// Reconfigures the capture pipeline for a `switch_camera` command.
    ///
    /// Returning [`Err`] makes the core answer `ack {ok: false, error}` and leave the
    /// server's view of the camera unchanged.
    fn set_parameters(
        &mut self,
        resolution: &str,
        fps: u32,
        codec: Codec,
        bitrate_kbps: u32,
    ) -> Result<(), SourceError>;

    /// Captures a single still image as JPEG bytes.
    fn take_photo(&mut self) -> impl Future<Output = Result<Vec<u8>, SourceError>> + Send;

    /// The codec the source encodes with unless a command overrides it.
    fn preferred_codec(&self) -> Codec;
}

/// Encoder bitrate assumed when a command carries no preference.
///
/// Kept deliberately conservative: the protocol lets the server lower it through a later
/// `switch_camera`, and overshooting costs uplink for no visible quality.
pub(crate) fn default_bitrate_kbps(codec: Codec) -> u32 {
    match codec {
        Codec::H265 => 30_000,
        Codec::H264 => 53_000,
        _ => 30_000,
    }
}

/// Counters describing how the media queue behaved since the client started.
#[derive(Debug, Default)]
pub(crate) struct DropStats {
    sent: AtomicU64,
    dropped: AtomicU64,
    bursts: AtomicU64,
    mismatched: AtomicU64,
}

impl DropStats {
    /// Records a frame handed to the socket.
    pub(crate) fn record_sent(&self) {
        self.sent.fetch_add(1, Ordering::Relaxed);
    }

    /// Records `count` frames discarded by the overflow policy.
    pub(crate) fn record_dropped(&self, count: u64) {
        self.dropped.fetch_add(count, Ordering::Relaxed);
    }

    /// Records the start of one drop burst.
    pub(crate) fn record_burst(&self) {
        self.bursts.fetch_add(1, Ordering::Relaxed);
    }

    /// Records a frame discarded because its stream was no longer active.
    pub(crate) fn record_mismatch(&self) {
        self.mismatched.fetch_add(1, Ordering::Relaxed);
    }

    /// Frames written to the socket.
    pub(crate) fn sent_frames(&self) -> u64 {
        self.sent.load(Ordering::Relaxed)
    }

    /// Frames discarded by the overflow policy.
    pub(crate) fn dropped_frames(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    /// Drop bursts, each starting at an overflow and ending at the next keyframe.
    pub(crate) fn drop_bursts(&self) -> u64 {
        self.bursts.load(Ordering::Relaxed)
    }

    /// Frames discarded for belonging to a stream the server no longer knows.
    pub(crate) fn mismatched_frames(&self) -> u64 {
        self.mismatched.load(Ordering::Relaxed)
    }
}

/// One queued frame on its way to the socket writer.
///
/// The stream id travels with the frame so the writer can notice a frame that was queued
/// before its stream ended — the server discards those silently, so the client has to
/// drop and log them itself.
pub(crate) struct MediaFrame {
    /// Stream the frame was produced for.
    pub(crate) stream_id: Arc<str>,
    /// Ready-to-send WebSocket message.
    pub(crate) message: Message,
}

/// The stream a capture task feeds.
#[derive(Debug, Clone)]
pub(crate) struct StreamSpec {
    /// Camera that produces the frames.
    pub(crate) camera_enum: u32,
    /// Server-issued stream id carried by every frame.
    pub(crate) stream_id: String,
}

/// Spawns the capture task for one stream.
pub(crate) fn spawn<F: FrameSource + 'static>(
    source: Arc<Mutex<F>>,
    spec: StreamSpec,
    out: mpsc::Sender<MediaFrame>,
    stats: Arc<DropStats>,
    stop: watch::Receiver<bool>,
) -> JoinHandle<()> {
    tokio::spawn(send_frames(source, spec, out, stats, stop))
}

/// Pulls frames from the source and pushes them into the queue until stopped.
///
/// Frames are numbered as they leave the source, so a consumer sees a `seq` gap exactly
/// when the core discarded frames; `ts` is the capture time of each frame, which keeps
/// segment durations honest across those gaps.
async fn send_frames<F: FrameSource>(
    source: Arc<Mutex<F>>,
    spec: StreamSpec,
    out: mpsc::Sender<MediaFrame>,
    stats: Arc<DropStats>,
    mut stop: watch::Receiver<bool>,
) {
    let stream_id: Arc<str> = Arc::from(spec.stream_id.as_str());
    let mut seq: u64 = 0;
    let mut dropping = false;
    let mut burst_len: u64 = 0;

    loop {
        let frame = {
            let mut source = source.lock().await;
            tokio::select! {
                _ = stop.changed() => break,
                frame = source.next_frame() => frame,
            }
        };
        if stop.has_changed().unwrap_or(false) {
            break;
        }

        let frame_seq = seq;
        seq += 1;
        let is_keyframe = frame.is_keyframe;
        let timestamp = now_rfc3339_nano();

        if dropping && !is_keyframe {
            burst_len += 1;
            stats.record_dropped(1);
            continue;
        }

        let header = BinaryHeader::frame(FramePayload {
            camera_enum: spec.camera_enum,
            stream_id: spec.stream_id.clone(),
            seq: frame_seq,
            ts: timestamp,
        });
        let message = match encode_frame(&header, &frame.data) {
            Ok(body) => Message::binary(body),
            Err(error) => {
                warn!(
                    %error,
                    stream_id = %spec.stream_id,
                    bytes = frame.data.len(),
                    "refusing a frame the protocol cannot carry"
                );
                continue;
            }
        };
        let item = MediaFrame {
            stream_id: stream_id.clone(),
            message,
        };

        if dropping {
            // A keyframe is never dropped: wait for queue space and resume the stream there.
            if out.send(item).await.is_err() {
                break;
            }
            info!(
                stream_id = %spec.stream_id,
                dropped = burst_len,
                seq = frame_seq,
                "resuming media at a keyframe"
            );
            dropping = false;
            burst_len = 0;
            continue;
        }

        match out.try_send(item) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(item)) => {
                if is_keyframe {
                    // Backpressure: hold this frame (and the source) until the writer drains.
                    if out.send(item).await.is_err() {
                        break;
                    }
                } else {
                    stats.record_dropped(1);
                    stats.record_burst();
                    burst_len = 1;
                    dropping = true;
                    warn!(
                        stream_id = %spec.stream_id,
                        seq = frame_seq,
                        "media queue full; dropping delta frames until the next keyframe"
                    );
                }
            }
            Err(mpsc::error::TrySendError::Closed(_)) => break,
        }
    }

    if burst_len > 0 {
        info!(
            stream_id = %spec.stream_id,
            dropped = burst_len,
            "capture stopped while dropping frames"
        );
    }
}
