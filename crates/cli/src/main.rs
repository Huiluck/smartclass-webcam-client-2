//! Headless device client.
//!
//! Registers one camera with a real smartclass-webcam-server, attaches, streams either a
//! synthetic 4K test pattern encoded by a software H.264 encoder (`--source synthetic`) or a
//! camera of this machine through the platform capture pipeline (`--source camera`), and runs
//! until SIGTERM/SIGINT. It is the binary the end-to-end harness drives; the wire protocol,
//! session and media queue live in [`scwc_core`], the platform capture pipeline in
//! [`scwc_capture`].
//!
//! The synthetic source is behind the `synthetic` feature, which is on by default and off in
//! the Android build: the software encoder it needs does not build for that target, and a
//! phone streams its own camera rather than a test pattern.

#[cfg(feature = "synthetic")]
mod source;

use std::process::ExitCode;

use clap::{Parser, ValueEnum};
use scwc_capture::{CameraInfo, CaptureConfig, enumerate_cameras, open};
use scwc_core::{ClientConfig, FrameSource, run};
use scwc_protocol::{CameraRegistration, Codec};
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;

#[cfg(feature = "synthetic")]
use crate::source::SynthSource;

/// Capability matrix announced when the capture source reports none of its own.
///
/// The configured pair is prepended to these fallbacks. The protocol announces one flat
/// resolution list and one flat frame-rate list rather than a matrix, so every fallback rate is
/// announced for the camera as a whole.
const FALLBACK_PRESETS: [(&str, &[u32]); 2] =
    [("1920x1080", &[60, 30]), ("1280x720", &[60, 30])];

/// The codec choice on the command line.
///
/// The synthetic source only ever encodes H.264: H.265 is accepted so the flag matches the
/// protocol vocabulary, but that camera is announced as H.264 with a warning. A camera source
/// honours both, through whichever encoder the platform provides.
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
    fn from(value: CodecArg) -> Self {
        match value {
            CodecArg::H264 => Codec::H264,
            CodecArg::H265 => Codec::H265,
        }
    }
}

/// Where the client gets its media from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum SourceArg {
    /// The built-in 4K test pattern, encoded by a software H.264 encoder.
    #[value(name = "synthetic")]
    Synthetic,
    /// A camera of this machine, encoded by the platform encoder.
    #[value(name = "camera")]
    Camera,
}

/// The source a run uses when `--source` is omitted.
///
/// A build without the synthetic feature has no test pattern to fall back on, so its only
/// source is the camera — which is also the one a phone build wants.
fn default_source() -> SourceArg {
    if cfg!(feature = "synthetic") {
        SourceArg::Synthetic
    } else {
        SourceArg::Camera
    }
}

/// Command line of `scwc-cli`.
#[derive(Debug, Parser)]
#[command(name = "scwc-cli", version, about, long_about = None)]
struct Args {
    /// Base URL of the smartclass-webcam-server, for example `http://127.0.0.1:18080`.
    #[arg(long, required_unless_present = "list_cameras")]
    server: Option<String>,
    /// Device id the bearer token was issued for.
    #[arg(long, required_unless_present = "list_cameras")]
    device_id: Option<String>,
    /// Device bearer token (`wdt_...`).
    #[arg(long, required_unless_present = "list_cameras")]
    token: Option<String>,
    /// Frame source to stream from; defaults to the test pattern on a build that has one and
    /// to the camera otherwise.
    #[arg(long, value_enum)]
    source: Option<SourceArg>,
    /// Camera to open, by the id `--list-cameras` reports; defaults to the first camera found.
    #[arg(long)]
    camera_id: Option<String>,
    /// Print the cameras of this machine and exit.
    #[arg(long)]
    list_cameras: bool,
    /// Capture resolution as `WIDTHxHEIGHT`, both sides even and at most 3840x2160.
    #[arg(long, value_parser = parse_resolution, required_unless_present = "list_cameras")]
    resolution: Option<Resolution>,
    /// Capture frame rate in frames per second.
    #[arg(
        long,
        value_parser = clap::value_parser!(u32).range(1..=240),
        required_unless_present = "list_cameras"
    )]
    fps: Option<u32>,
    /// Encoder bitrate in kilobits per second.
    #[arg(
        long,
        value_parser = clap::value_parser!(u32).range(1..),
        required_unless_present = "list_cameras"
    )]
    bitrate_kbps: Option<u32>,
    /// Preferred codec; the synthetic source only really encodes `h264`.
    #[arg(long, value_enum, default_value = "h264")]
    codec: CodecArg,
}

/// The command line a streaming run needs.
///
/// Kept separate from [`Args`] so that `--list-cameras` can run without a server, a token or a
/// capture setup.
struct RunArgs {
    server: String,
    device_id: String,
    token: String,
    source: SourceArg,
    camera_id: Option<String>,
    resolution: Resolution,
    fps: u32,
    bitrate_kbps: u32,
    codec: CodecArg,
}

impl Args {
    /// Splits the parsed command line into the arguments a streaming run needs.
    ///
    /// clap rejects a run that omits any of the required flags, so the error path only guards a
    /// mismatch between the `required_unless_present` attributes and this list.
    fn into_run_args(self) -> Result<RunArgs, String> {
        Ok(RunArgs {
            server: required(self.server, "--server")?,
            device_id: required(self.device_id, "--device-id")?,
            token: required(self.token, "--token")?,
            source: self.source.unwrap_or_else(default_source),
            camera_id: self.camera_id,
            resolution: required(self.resolution, "--resolution")?,
            fps: required(self.fps, "--fps")?,
            bitrate_kbps: required(self.bitrate_kbps, "--bitrate-kbps")?,
            codec: self.codec,
        })
    }
}

/// Takes a flag a streaming run requires, naming it when it is absent.
fn required<T>(value: Option<T>, name: &str) -> Result<T, String> {
    value.ok_or_else(|| format!("{name} is required unless --list-cameras is given"))
}

/// A validated `WIDTHxHEIGHT` pair.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Resolution {
    width: u32,
    height: u32,
}

impl Resolution {
    /// Canonical `WIDTHxHEIGHT` spelling used in the registration.
    fn as_string(self) -> String {
        format!("{}x{}", self.width, self.height)
    }
}

/// Parses and validates the `--resolution` flag.
fn parse_resolution(value: &str) -> Result<Resolution, String> {
    let (width, height) = value
        .split_once(['x', 'X'])
        .ok_or_else(|| format!("{value:?} is not WIDTHxHEIGHT"))?;
    let width: u32 = width
        .trim()
        .parse()
        .map_err(|_| format!("{width:?} is not a width"))?;
    let height: u32 = height
        .trim()
        .parse()
        .map_err(|_| format!("{height:?} is not a height"))?;

    if width == 0 || height == 0 {
        return Err("resolution sides must be positive".to_owned());
    }
    if width % 2 != 0 || height % 2 != 0 {
        return Err("resolution sides must be even (I420 chroma is half-resolution)".to_owned());
    }
    // 3840x2160 is the largest capture the client targets on any platform, and the synthetic
    // source's software encoder refuses anything above it in either orientation.
    if width.max(height) > 3840 || width.min(height) > 2160 {
        return Err("resolution exceeds the 3840x2160 maximum the client captures".to_owned());
    }

    Ok(Resolution { width, height })
}

#[tokio::main]
async fn main() -> ExitCode {
    init_tracing();
    let args = Args::parse();

    if args.list_cameras {
        return list_cameras();
    }
    let args = match args.into_run_args() {
        Ok(args) => args,
        Err(error) => {
            error!(%error, "invalid command line");
            return ExitCode::from(2);
        }
    };

    let source = args.source;
    match source {
        #[cfg(feature = "synthetic")]
        SourceArg::Synthetic => run_synthetic(args).await,
        #[cfg(not(feature = "synthetic"))]
        SourceArg::Synthetic => {
            error!(
                "this build has no synthetic source: the software encoder is desktop-only; \
                 stream a camera with --source camera"
            );
            ExitCode::from(2)
        }
        SourceArg::Camera => run_camera(args).await,
    }
}

/// Builds the synthetic test pattern and registers it as the only camera.
#[cfg(feature = "synthetic")]
async fn run_synthetic(args: RunArgs) -> ExitCode {
    if args.codec == CodecArg::H265 {
        warn!(
            "the synthetic source cannot encode h265; announcing h264 instead and \
             registering the camera as h264"
        );
    }
    let codec = Codec::H264;

    let (supported_resolutions, supported_framerates) =
        capabilities(&args.resolution, args.fps, FALLBACK_PRESETS);
    let supported_pairs = supported_resolutions
        .iter()
        .filter_map(|entry| source::parse_dimensions(entry))
        .collect();
    let source = match SynthSource::new(
        args.resolution.width,
        args.resolution.height,
        args.fps,
        args.bitrate_kbps,
        supported_pairs,
        supported_framerates.clone(),
    ) {
        Ok(source) => source,
        Err(error) => {
            error!(%error, "could not start the synthetic frame source");
            return ExitCode::FAILURE;
        }
    };

    let camera = CameraRegistration {
        camera_enum: 0,
        resolution: args.resolution.as_string(),
        fps: args.fps,
        supported_resolutions,
        supported_framerates,
        supported_codec: vec![codec],
        attrs: Default::default(),
    };

    finish(
        ClientConfig::new(args.server, args.device_id, args.token, vec![camera]),
        source,
    )
    .await
}

/// Opens the platform capture pipeline and registers that camera as the only camera.
async fn run_camera(args: RunArgs) -> ExitCode {
    let camera_info = select_camera(args.camera_id.as_deref());
    let (supported_resolutions, supported_framerates) = match &camera_info {
        Some(info) => {
            info!(camera = %info.name, camera_id = %info.id, "using camera");
            capabilities(
                &args.resolution,
                args.fps,
                info.presets
                    .iter()
                    .map(|(preset, rates)| (preset.as_str(), rates.as_slice())),
            )
        }
        None => {
            warn!("no camera reported its capabilities; announcing the fallback lists");
            capabilities(&args.resolution, args.fps, FALLBACK_PRESETS)
        }
    };

    let codec = Codec::from(args.codec);
    let config = CaptureConfig {
        camera_id: args
            .camera_id
            .clone()
            .or_else(|| camera_info.as_ref().map(|info| info.id.clone())),
        width: args.resolution.width,
        height: args.resolution.height,
        fps: args.fps,
        codec,
        bitrate_kbps: args.bitrate_kbps,
    };
    let source = match open(config) {
        Ok(source) => source,
        Err(error) => {
            error!(%error, "could not open the camera");
            return ExitCode::FAILURE;
        }
    };

    // VideoToolbox encodes both; the configured codec stays first as the preferred one.
    let supported_codec = match codec {
        Codec::H265 => vec![Codec::H265, Codec::H264],
        _ => vec![Codec::H264, Codec::H265],
    };
    let camera = CameraRegistration {
        camera_enum: 0,
        resolution: args.resolution.as_string(),
        fps: args.fps,
        supported_resolutions,
        supported_framerates,
        supported_codec,
        attrs: Default::default(),
    };

    finish(
        ClientConfig::new(args.server, args.device_id, args.token, vec![camera]),
        source,
    )
    .await
}

/// Announces one camera and runs the client until a shutdown signal asks it to stop.
async fn finish<F: FrameSource + 'static>(config: ClientConfig, source: F) -> ExitCode {
    info!(server = %config.server, device_id = %config.device_id, "starting device client");

    match run(config, source, shutdown_signal()).await {
        Ok(()) => {
            info!("device client stopped");
            ExitCode::SUCCESS
        }
        Err(error) if error.is_fatal() => {
            error!(%error, "invalid configuration");
            ExitCode::from(2)
        }
        Err(error) => {
            error!(%error, "device client stopped with an error");
            ExitCode::FAILURE
        }
    }
}

/// Prints the cameras of this machine, one per line.
///
/// Enumeration legitimately fails before the process may see any camera — headless systems and
/// denied camera permissions both end here — so a failure is reported rather than panicked on.
fn list_cameras() -> ExitCode {
    match enumerate_cameras() {
        Ok(cameras) => {
            for camera in &cameras {
                println!("{}", format_camera(camera));
            }
            ExitCode::SUCCESS
        }
        Err(error) => {
            error!(%error, "could not enumerate the cameras of this machine");
            ExitCode::FAILURE
        }
    }
}

/// Renders one camera as `ID<TAB>NAME<TAB>RESOLUTION@FPS,FPS ...`.
fn format_camera(camera: &CameraInfo) -> String {
    let presets = camera
        .presets
        .iter()
        .map(|(resolution, framerates)| {
            let framerates: Vec<String> = framerates.iter().map(u32::to_string).collect();
            format!("{resolution}@{}", framerates.join(","))
        })
        .collect::<Vec<_>>()
        .join(" ");

    format!("{}\t{}\t{}", camera.id, camera.name, presets)
}

/// The camera the client announces and opens.
///
/// `--camera-id` selects by the id [`enumerate_cameras`] reports; without it the first camera of
/// this machine wins. Returns `None` when enumeration found nothing to work with: the caller then
/// announces the fallback capabilities and [`open`] has the last word on whether anything can be
/// captured at all.
fn select_camera(camera_id: Option<&str>) -> Option<CameraInfo> {
    let cameras = match enumerate_cameras() {
        Ok(cameras) => cameras,
        Err(error) => {
            warn!(%error, "could not enumerate the cameras of this machine");
            return None;
        }
    };

    match camera_id {
        Some(id) => {
            let camera = cameras.into_iter().find(|camera| camera.id == id);
            if camera.is_none() {
                warn!(%id, "no camera of this machine has this id");
            }
            camera
        }
        None => {
            let camera = cameras.into_iter().next();
            if camera.is_none() {
                warn!("this machine reports no camera");
            }
            camera
        }
    }
}

/// The capability lists the registration announces for one camera.
///
/// The protocol announces one flat resolution list and one flat frame-rate list, so the
/// per-resolution frame-rate matrix a camera reports is flattened into the union of its rates.
/// The configured pair comes first and is always announced, even when the source did not report
/// it: the server rejects a registration whose current pair is missing from its own supported
/// list.
fn capabilities<'a>(
    resolution: &Resolution,
    fps: u32,
    presets: impl IntoIterator<Item = (&'a str, &'a [u32])>,
) -> (Vec<String>, Vec<u32>) {
    let mut resolutions = vec![resolution.as_string()];
    let mut framerates = vec![fps];

    for (preset, rates) in presets {
        if !resolutions.iter().any(|entry| entry == preset) {
            resolutions.push(preset.to_owned());
        }
        for rate in rates {
            if !framerates.contains(rate) {
                framerates.push(*rate);
            }
        }
    }

    (resolutions, framerates)
}

/// Installs a tracing subscriber honouring `RUST_LOG`, defaulting to `info`.
fn init_tracing() {
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt().with_env_filter(filter).try_init();
}

/// Resolves when the process is asked to stop, either by SIGINT or SIGTERM.
async fn shutdown_signal() {
    let ctrl_c = async {
        if let Err(error) = tokio::signal::ctrl_c().await {
            warn!(%error, "could not listen for SIGINT");
            std::future::pending::<()>().await;
        }
    };

    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                signal.recv().await;
            }
            Err(error) => {
                warn!(%error, "could not listen for SIGTERM");
                std::future::pending::<()>().await;
            }
        }
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        () = ctrl_c => info!("SIGINT received; shutting down"),
        () = terminate => info!("SIGTERM received; shutting down"),
    }
}
