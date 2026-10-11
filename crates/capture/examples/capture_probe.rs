//! Capture probe: measures what a camera and the hardware encoder actually deliver.
//!
//! Lists the cameras when no id is given, opens one, then pulls frames for a fixed number
//! of seconds and prints the achieved frame rate plus the average and maximum encoded frame
//! size every second, with the keyframe sizes broken out.

use std::process::ExitCode;
use std::time::{Duration, Instant};

use clap::{Parser, ValueEnum};
use scwc_capture::{CaptureConfig, CameraInfo, enumerate_cameras, open};
use scwc_core::FrameSource;
use scwc_protocol::Codec;
use tokio::time::timeout;
use tracing_subscriber::EnvFilter;

/// Encoder bitrate used unless the codec says otherwise.
const BITRATE_KBPS: u32 = 20_000;

/// The probe's codec choice.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum CodecArg {
    /// H.264/AVC.
    #[value(name = "h264")]
    H264,
    /// H.265/HEVC.
    #[value(name = "h265")]
    H265,
}

impl From<CodecArg> for Codec {
    fn from(arg: CodecArg) -> Self {
        match arg {
            CodecArg::H264 => Codec::H264,
            CodecArg::H265 => Codec::H265,
        }
    }
}

/// A capture resolution in the protocol's `WIDTHxHEIGHT` form.
#[derive(Debug, Clone, Copy)]
struct Resolution {
    width: u32,
    height: u32,
}

impl std::str::FromStr for Resolution {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let (width, height) = value
            .split_once(['x', 'X'])
            .ok_or_else(|| format!("expected WIDTHxHEIGHT, got {value:?}"))?;
        let width: u32 = width
            .trim()
            .parse()
            .map_err(|error| format!("invalid width {width:?}: {error}"))?;
        let height: u32 = height
            .trim()
            .parse()
            .map_err(|error| format!("invalid height {height:?}: {error}"))?;
        if width == 0 || height == 0 {
            return Err("resolution sides must be non-zero".to_owned());
        }
        Ok(Self { width, height })
    }
}

/// Command line of `capture_probe`.
#[derive(Debug, Parser)]
#[command(name = "capture_probe", version, about, long_about = None)]
struct Args {
    /// Camera `uniqueID`; without it every camera is listed and the first one is used.
    #[arg(long)]
    camera_id: Option<String>,
    /// Capture resolution as `WIDTHxHEIGHT`.
    #[arg(long, value_parser = clap::value_parser!(Resolution), default_value = "1920x1080")]
    resolution: Resolution,
    /// Capture frame rate in frames per second.
    #[arg(long, value_parser = clap::value_parser!(u32).range(1..=240), default_value_t = 30)]
    fps: u32,
    /// How long to capture for.
    #[arg(long, value_parser = clap::value_parser!(u64).range(1..), default_value_t = 10)]
    seconds: u64,
    /// Codec the hardware encoder produces.
    #[arg(long, value_enum, default_value = "h264")]
    codec: CodecArg,
}

/// Prints one camera in the form the operator reads out on the command line.
fn print_camera(camera: &CameraInfo) {
    println!("{}  {}", camera.id, camera.name);
    for (resolution, fps) in &camera.presets {
        println!("    {:>10}  {}", resolution, join_fps(fps));
    }
}

/// Renders a frame-rate list as `60, 30`.
fn join_fps(fps: &[u32]) -> String {
    fps.iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

/// Running totals of one measurement window.
struct Window {
    started: Instant,
    frames: u64,
    bytes: u64,
    max_bytes: usize,
    keyframes: u64,
    keyframe_bytes: u64,
    max_keyframe_bytes: usize,
}

impl Window {
    fn new() -> Self {
        Self {
            started: Instant::now(),
            frames: 0,
            bytes: 0,
            max_bytes: 0,
            keyframes: 0,
            keyframe_bytes: 0,
            max_keyframe_bytes: 0,
        }
    }

    fn record(&mut self, len: usize, is_keyframe: bool) {
        self.frames += 1;
        self.bytes += len as u64;
        self.max_bytes = self.max_bytes.max(len);
        if is_keyframe {
            self.keyframes += 1;
            self.keyframe_bytes += len as u64;
            self.max_keyframe_bytes = self.max_keyframe_bytes.max(len);
        }
    }

    /// Prints the window and starts a new one when a second has passed.
    fn report(&mut self, total: Duration) {
        let elapsed = self.started.elapsed();
        if elapsed < Duration::from_secs(1) {
            return;
        }

        let achieved = self.frames as f64 / elapsed.as_secs_f64();
        let average = self.bytes.checked_div(self.frames).unwrap_or(0);
        let keyframe_average = self.keyframe_bytes.checked_div(self.keyframes).unwrap_or(0);
        println!(
            "{:>4.0}s  fps={achieved:>5.1}  frames={:<6} avg={average:<9} max={:<9} keyframes={:<4} kf_avg={keyframe_average:<9} kf_max={}",
            total.as_secs_f64(),
            self.frames,
            self.max_bytes,
            self.keyframes,
            self.max_keyframe_bytes,
        );
        *self = Self::new();
    }

    /// Prints the final totals of the last, partial window.
    fn finish(&self, total: Duration) {
        let elapsed = self.started.elapsed();
        let average = self.bytes.checked_div(self.frames).unwrap_or(0);
        println!(
            "total {:>5.1}s  frames={:<6} fps={:>5.2}  avg={average:<9} max={:<9} keyframes={}",
            total.as_secs_f64(),
            self.frames,
            self.frames as f64 / elapsed.as_secs_f64().max(f64::MIN_POSITIVE),
            self.max_bytes,
            self.keyframes,
        );
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .try_init();
    let args = Args::parse();

    let camera_id = match args.camera_id {
        Some(id) => Some(id),
        None => {
            let cameras = match enumerate_cameras() {
                Ok(cameras) => cameras,
                Err(error) => {
                    eprintln!("cannot enumerate cameras: {error}");
                    return ExitCode::FAILURE;
                }
            };
            for camera in &cameras {
                print_camera(camera);
            }
            match cameras.first() {
                Some(camera) => {
                    println!("using camera {} ({})", camera.id, camera.name);
                    Some(camera.id.clone())
                }
                None => {
                    eprintln!("no camera to capture from");
                    return ExitCode::FAILURE;
                }
            }
        }
    };

    let config = CaptureConfig {
        camera_id,
        width: args.resolution.width,
        height: args.resolution.height,
        fps: args.fps,
        codec: args.codec.into(),
        bitrate_kbps: BITRATE_KBPS,
    };
    println!(
        "capturing {}x{} at {} fps, {:?}, {} kbps",
        config.width, config.height, config.fps, config.codec, config.bitrate_kbps
    );

    let mut source = match open(config) {
        Ok(source) => source,
        Err(error) => {
            eprintln!("cannot open the camera: {error}");
            return ExitCode::FAILURE;
        }
    };

    // Capture starts when the FIRST frame arrives: remote/virtual cameras (continuity,
    // phone-as-webcam) can take tens of seconds to deliver after the session opens, so the
    // measurement window begins at the first frame, not at open().
    let opened = Instant::now();
    let first = match timeout(Duration::from_secs(60), source.next_frame()).await {
        Ok(frame) => frame,
        Err(_) => {
            eprintln!("no frame within 60s of opening the camera (device never started?)");
            return ExitCode::FAILURE;
        }
    };
    println!("first frame after {:.1?} ({} bytes)", opened.elapsed(), first.data.len());

    let started = Instant::now();
    let deadline = started + Duration::from_secs(args.seconds);
    let mut window = Window::new();
    window.record(first.data.len(), first.is_keyframe);

    loop {
        let now = Instant::now();
        if now >= deadline {
            break;
        }
        match timeout(deadline - now, source.next_frame()).await {
            Ok(frame) => window.record(frame.data.len(), frame.is_keyframe),
            Err(_) => break,
        }
        window.report(started.elapsed());
    }

    window.finish(started.elapsed());
    ExitCode::SUCCESS
}
