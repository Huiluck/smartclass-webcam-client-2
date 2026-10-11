//! The synthetic frame source: a moving test pattern encoded to H.264 by `openh264`.
//!
//! This is the M1 stand-in for a real capture/encode pipeline. It renders an
//! I420 pattern with cheap integer math — a scrolling gradient, a moving box and a
//! burned-in frame counter — and hands the encoder one access unit per call. The
//! output is Annex B, exactly what [`FrameSource`] promises.

use std::time::{Duration, Instant};

use openh264::OpenH264API;
use openh264::encoder::{
    BitRate, Complexity, EncodedBitStream, Encoder, EncoderConfig, FrameRate, FrameType,
    IntraFramePeriod, RateControlMode, UsageType,
};
use openh264::formats::YUVSource;
use scwc_core::{EncodedFrame, FrameSource, SourceError};
use scwc_protocol::Codec;
use tokio::time::{Interval, MissedTickBehavior, interval};
use tracing::{info, warn};

/// How often the achieved frame rate is reported.
const REPORT_PERIOD: Duration = Duration::from_secs(5);

/// JPEG quality used by [`SynthSource::take_photo`].
const PHOTO_QUALITY: u8 = 85;

/// A four-byte H.264 start code; every NAL in the Annex B output is prefixed with one.
const START_CODE: [u8; 4] = [0x00, 0x00, 0x00, 0x01];

/// A 5x7 bitmap font for the digits `0`-`9`, one byte per row, most-significant bit leftmost.
const DIGITS: [[u8; 7]; 10] = [
    [0b01110, 0b10001, 0b10011, 0b10101, 0b11001, 0b10001, 0b01110],
    [0b00100, 0b01100, 0b00100, 0b00100, 0b00100, 0b00100, 0b01110],
    [0b01110, 0b10001, 0b00001, 0b00010, 0b00100, 0b01000, 0b11111],
    [0b11111, 0b00010, 0b00100, 0b00010, 0b00001, 0b10001, 0b01110],
    [0b00010, 0b00110, 0b01010, 0b10010, 0b11111, 0b00010, 0b00010],
    [0b11111, 0b10000, 0b11110, 0b00001, 0b00001, 0b10001, 0b01110],
    [0b00110, 0b01000, 0b10000, 0b11110, 0b10001, 0b10001, 0b01110],
    [0b11111, 0b00001, 0b00010, 0b00100, 0b01000, 0b01000, 0b01000],
    [0b01110, 0b10001, 0b10001, 0b01110, 0b10001, 0b10001, 0b01110],
    [0b01110, 0b10001, 0b10001, 0b01111, 0b00001, 0b00010, 0b01100],
];

/// The synthetic capture pipeline behind a [`FrameSource`].
///
/// One camera at a time, exactly like the protocol's model: the encoder is re-created
/// whenever `switch_camera` moves the pipeline to another announced resolution/fps pair.
pub struct SynthSource {
    encoder: Encoder,
    width: usize,
    height: usize,
    fps: u32,
    bitrate_kbps: u32,
    supported_resolutions: Vec<(u32, u32)>,
    supported_framerates: Vec<u32>,
    interval: Interval,
    yuv: Vec<u8>,
    row: Vec<u8>,
    frame_index: u64,
    reported_at: Instant,
    frames_since_report: u64,
}

impl SynthSource {
    /// Builds the pipeline for one camera.
    ///
    /// `supported_resolutions` and `supported_framerates` are the lists the registration
    /// announced; `set_parameters` accepts nothing outside them.
    pub fn new(
        width: u32,
        height: u32,
        fps: u32,
        bitrate_kbps: u32,
        supported_resolutions: Vec<(u32, u32)>,
        supported_framerates: Vec<u32>,
    ) -> Result<Self, SourceError> {
        let encoder = build_encoder(fps, bitrate_kbps)?;
        let (width, height) = (width as usize, height as usize);

        info!(
            width,
            height,
            fps,
            bitrate_kbps,
            "synthetic frame source ready"
        );

        Ok(Self {
            encoder,
            width,
            height,
            fps,
            bitrate_kbps,
            supported_resolutions,
            supported_framerates,
            interval: new_interval(fps),
            yuv: vec![0u8; width * height * 3 / 2],
            row: vec![0u8; width],
            frame_index: 0,
            reported_at: Instant::now(),
            frames_since_report: 0,
        })
    }

    /// Fills the I420 buffer with the current frame of the test pattern.
    fn render(&mut self) {
        let width = self.width;
        let height = self.height;
        let frame = self.frame_index;
        let y_len = width * height;
        let chroma_len = y_len / 4;

        // The gradient depends on `x` alone, so build one row and memcpy it per scanline.
        let scroll = (frame as u32).wrapping_mul(3);
        for (x, value) in self.row.iter_mut().enumerate() {
            *value = ((x as u32).wrapping_add(scroll) & 0xFF) as u8;
        }

        // A high-contrast box sweeping horizontally keeps P-frames busy, so the encoded
        // sizes stay realistic instead of collapsing to an almost static gradient.
        let box_w = (width / 6).max(2);
        let box_h = (height / 6).max(2);
        let box_x = ((frame as usize).wrapping_mul(8)) % (width - box_w).max(1);
        let box_y = height / 3;

        let (y_plane, chroma) = self.yuv.split_at_mut(y_len);
        let (u_plane, v_plane) = chroma.split_at_mut(chroma_len);

        for y in 0..height {
            let line = &mut y_plane[y * width..(y + 1) * width];
            line.copy_from_slice(&self.row);
            if y >= box_y && y < box_y + box_h {
                line[box_x..(box_x + box_w).min(width)].fill(235);
            }
        }

        u_plane.fill(128);
        v_plane.fill(128);
        let cw = width / 2;
        let ch = height / 2;
        let cx0 = (box_x / 2).min(cw);
        let cx1 = ((box_x + box_w) / 2).min(cw);
        let cy0 = (box_y / 2).min(ch);
        let cy1 = ((box_y + box_h) / 2).min(ch);
        if cx1 > cx0 && cy1 > cy0 {
            for cy in cy0..cy1 {
                let span = cy * cw + cx0..cy * cw + cx1;
                u_plane[span.clone()].fill(90);
                v_plane[span].fill(190);
            }
        }

        draw_counter(y_plane, width, height, frame);
    }

    /// Logs the achieved frame rate once per [`REPORT_PERIOD`].
    fn report_rate(&mut self) {
        self.frames_since_report += 1;
        let elapsed = self.reported_at.elapsed();
        if elapsed < REPORT_PERIOD {
            return;
        }

        let achieved = self.frames_since_report as f64 / elapsed.as_secs_f64();
        info!(
            target_fps = self.fps,
            achieved_fps = achieved,
            frames = self.frame_index,
            width = self.width,
            height = self.height,
            "synthetic source rate"
        );
        self.reported_at = Instant::now();
        self.frames_since_report = 0;
    }
}

impl FrameSource for SynthSource {
    async fn next_frame(&mut self) -> EncodedFrame {
        // Self-paced: the core never paces a source, so a synthetic one has to emulate
        // the camera's own frame clock.
        self.interval.tick().await;
        self.frame_index = self.frame_index.wrapping_add(1);
        self.render();

        let view = YuvView {
            data: &self.yuv,
            width: self.width,
            height: self.height,
        };
        let (data, is_keyframe) = match self.encoder.encode(&view) {
            Ok(bitstream) => {
                let is_keyframe =
                    matches!(bitstream.frame_type(), FrameType::IDR | FrameType::I);
                (annex_b(&bitstream), is_keyframe)
            }
            Err(error) => {
                warn!(%error, "openh264 encode failed; forcing a keyframe for the next frame");
                self.encoder.force_intra_frame();
                (Vec::new(), true)
            }
        };

        self.report_rate();
        EncodedFrame::new(data, is_keyframe)
    }

    fn set_parameters(
        &mut self,
        resolution: &str,
        fps: u32,
        codec: Codec,
        bitrate_kbps: u32,
    ) -> Result<(), SourceError> {
        if codec != Codec::H264 {
            return Err(SourceError::new(format!(
                "the synthetic source only encodes h264, not {codec:?}"
            )));
        }

        let (width, height) = parse_dimensions(resolution).ok_or_else(|| {
            SourceError::new(format!("resolution {resolution:?} is not WIDTHxHEIGHT"))
        })?;
        if !self.supported_resolutions.contains(&(width, height)) {
            return Err(SourceError::new(format!(
                "resolution {resolution} is not announced by this camera"
            )));
        }
        if !self.supported_framerates.contains(&fps) {
            return Err(SourceError::new(format!(
                "{fps} fps is not announced by this camera"
            )));
        }

        if (width as usize, height as usize, fps, bitrate_kbps)
            == (self.width, self.height, self.fps, self.bitrate_kbps)
        {
            return Ok(());
        }

        let encoder = build_encoder(fps, bitrate_kbps)?;
        self.encoder = encoder;
        self.width = width as usize;
        self.height = height as usize;
        self.fps = fps;
        self.bitrate_kbps = bitrate_kbps;
        self.interval = new_interval(fps);
        self.yuv = vec![0u8; self.width * self.height * 3 / 2];
        self.row = vec![0u8; self.width];

        info!(
            width = self.width,
            height = self.height,
            fps,
            bitrate_kbps,
            "frame source reconfigured"
        );
        Ok(())
    }

    async fn take_photo(&mut self) -> Result<Vec<u8>, SourceError> {
        self.render();
        let rgb = yuv_to_rgb(&self.yuv, self.width, self.height);

        let mut jpeg = Vec::new();
        jpeg_encoder::Encoder::new(&mut jpeg, PHOTO_QUALITY)
            .encode(
                &rgb,
                self.width as u16,
                self.height as u16,
                jpeg_encoder::ColorType::Rgb,
            )
            .map_err(|error| SourceError::new(format!("jpeg encoding failed: {error}")))?;

        info!(bytes = jpeg.len(), "encoded a still photo");
        Ok(jpeg)
    }

    fn preferred_codec(&self) -> Codec {
        Codec::H264
    }
}

/// Builds an H.264 encoder for the given rate, with a two-second GOP.
///
/// Complexity is pinned low: this is a synthetic real-time source, and software 4K
/// encoding cannot spare the cycles a high-complexity search would cost.
fn build_encoder(fps: u32, bitrate_kbps: u32) -> Result<Encoder, SourceError> {
    let config = EncoderConfig::new()
        .bitrate(BitRate::from_bps(bitrate_kbps.saturating_mul(1000)))
        .max_frame_rate(FrameRate::from_hz(fps as f32))
        .intra_frame_period(IntraFramePeriod::from_num_frames(fps.saturating_mul(2)))
        .rate_control_mode(RateControlMode::Bitrate)
        .usage_type(UsageType::CameraVideoRealTime)
        .complexity(Complexity::Low)
        .skip_frames(false);

    Encoder::with_api_config(OpenH264API::from_source(), config)
        .map_err(|error| SourceError::new(format!("openh264 encoder init failed: {error}")))
}

/// A self-paced ticker that never bursts to catch up after a slow frame.
fn new_interval(fps: u32) -> Interval {
    let period = Duration::from_secs_f64(1.0 / f64::from(fps.max(1)));
    let mut interval = interval(period);
    interval.set_missed_tick_behavior(MissedTickBehavior::Delay);
    interval
}

/// Splits a `WIDTHxHEIGHT` string into its dimensions.
pub(crate) fn parse_dimensions(resolution: &str) -> Option<(u32, u32)> {
    let (width, height) = resolution.split_once(['x', 'X'])?;
    Some((width.trim().parse().ok()?, height.trim().parse().ok()?))
}

/// Flattens an encoded bitstream into Annex B: every NAL prefixed with a start code.
fn annex_b(bitstream: &EncodedBitStream<'_>) -> Vec<u8> {
    let mut out = Vec::new();
    for layer_index in 0..bitstream.num_layers() {
        let Some(layer) = bitstream.layer(layer_index) else {
            continue;
        };
        for nal_index in 0..layer.nal_count() {
            let Some(nal) = layer.nal_unit(nal_index) else {
                continue;
            };
            if !nal.starts_with(&START_CODE) && !nal.starts_with(&[0, 0, 1]) {
                out.extend_from_slice(&START_CODE);
            }
            out.extend_from_slice(nal);
        }
    }
    out
}

/// Burns the frame counter into the top-left corner of the luma plane.
fn draw_counter(y_plane: &mut [u8], width: usize, height: usize, frame: u64) {
    let scale = (height / 216).clamp(2, 24);
    let (ox, oy) = (8usize, 8usize);
    let digit_w = 5 * scale;
    let digit_h = 7 * scale;
    if oy + digit_h > height {
        return;
    }

    // Least-significant digit first, then drawn right-to-left from the left edge.
    let mut digits = [0u8; 20];
    let mut count = 0usize;
    let mut value = frame;
    loop {
        digits[count] = (value % 10) as u8;
        value /= 10;
        count += 1;
        if value == 0 {
            break;
        }
    }
    count = count.max(6);

    for position in 0..count {
        let digit = digits[count - 1 - position] as usize;
        let dx = ox + position * (digit_w + scale);
        if dx + digit_w > width {
            break;
        }
        for (row, &bits) in DIGITS[digit].iter().enumerate() {
            for column in 0..5usize {
                if bits & (1 << (4 - column)) == 0 {
                    continue;
                }
                let px = dx + column * scale;
                for py in (oy + row * scale)..(oy + (row + 1) * scale) {
                    y_plane[py * width + px..py * width + px + scale].fill(235);
                }
            }
        }
    }
}

/// Converts an I420 buffer to interleaved RGB using the BT.601 fixed-point matrix.
fn yuv_to_rgb(yuv: &[u8], width: usize, height: usize) -> Vec<u8> {
    let y_len = width * height;
    let cw = width / 2;
    let (luma, chroma) = yuv.split_at(y_len);
    let (u_plane, v_plane) = chroma.split_at(cw * (height / 2));

    let mut rgb = vec![0u8; y_len * 3];
    for y in 0..height {
        for x in 0..width {
            let luma = i32::from(luma[y * width + x]);
            let index = (y / 2) * cw + x / 2;
            let u = i32::from(u_plane[index]) - 128;
            let v = i32::from(v_plane[index]) - 128;

            let r = luma + ((v * 1436) >> 10);
            let g = luma - ((u * 352 + v * 731) >> 10);
            let b = luma + ((u * 1815) >> 10);

            let offset = (y * width + x) * 3;
            rgb[offset] = r.clamp(0, 255) as u8;
            rgb[offset + 1] = g.clamp(0, 255) as u8;
            rgb[offset + 2] = b.clamp(0, 255) as u8;
        }
    }
    rgb
}

/// A borrowed I420 frame; lets the encoder read the source buffer without a copy.
struct YuvView<'a> {
    data: &'a [u8],
    width: usize,
    height: usize,
}

impl YUVSource for YuvView<'_> {
    fn dimensions(&self) -> (usize, usize) {
        (self.width, self.height)
    }

    fn strides(&self) -> (usize, usize, usize) {
        let chroma = self.width / 2;
        (self.width, chroma, chroma)
    }

    fn y(&self) -> &[u8] {
        let luma = self.width * self.height;
        &self.data[..luma]
    }

    fn u(&self) -> &[u8] {
        let luma = self.width * self.height;
        let chroma = luma / 4;
        &self.data[luma..luma + chroma]
    }

    fn v(&self) -> &[u8] {
        let luma = self.width * self.height;
        let chroma = luma / 4;
        &self.data[luma + chroma..luma + 2 * chroma]
    }
}
