//! Windows backend: Media Foundation for capture and encoding, WIC for stills.
//!
//! Unlike the macOS backend this one is pure Rust — Media Foundation is COM and the
//! `windows` crate covers it — so no shim is compiled.
//!
//! # Shape of the pipeline
//!
//! A camera is an `IMFMediaSource` obtained from an `IMFActivate`; an `IMFSourceReader`
//! turns it into a stream of uncompressed frames (NV12, or YUY2 where the camera or its
//! driver will not deliver NV12), and an encoder MFT from `MFTEnumEx` turns those into
//! H.264 or H.265 access units. Hardware transforms — NVENC, Quick Sync, AMF — are tried
//! first, and the inbox software encoder is the fallback, loudly.
//!
//! Every Media Foundation object lives on one dedicated pump thread:
//!
//! ```text
//! CaptureSource ──command──▶ pump thread: IMFSourceReader ─▶ encoder MFT ─▶ frame_cb
//! ```
//!
//! The thread never blocks on the queue in `source.rs` for long — a full queue only stalls
//! a keyframe — so a `stop` or a `take_photo` is served within one frame interval. It lives
//! in the multithreaded apartment, which needs no message pump, and it is the only thread
//! that touches the Media Foundation objects.
//!
//! # Frame format
//!
//! The callbacks in `source.rs` expect Annex B access units with parameter sets in front of
//! every IDR. Encoder MFTs are inconsistent about this: the inbox H.264 encoder emits Annex
//! B, some vendor MFTs emit length-prefixed NAL units, and the parameter sets are published
//! out of band through `MF_MT_MPEG_SEQUENCE_HEADER`. [`to_annex_b`] normalises the first two
//! and [`Encoder::with_sequence_header`] handles the third.
//!
//! # Uncertainty
//!
//! Nothing in this file has been run against a camera: the backend was written on a host
//! that cannot execute it. The places where the documented behaviour is most likely to
//! differ in practice are marked `UNCERTAIN`.

use std::ffi::c_void;
use std::mem::{ManuallyDrop, replace};
use std::ptr;
use std::slice;
use std::sync::mpsc::{self, Receiver, Sender, SyncSender, TryRecvError};
use std::thread::{self, JoinHandle};

use scwc_protocol::Codec;
use tracing::{debug, error, warn};
use windows::Win32::Foundation::{E_ACCESSDENIED, HGLOBAL, RPC_E_CHANGED_MODE, VARIANT_TRUE};
use windows::Win32::Graphics::Imaging::{
    CLSID_WICImagingFactory, GUID_ContainerFormatJpeg, GUID_WICPixelFormat24bppBGR,
    IWICBitmapFrameEncode, IWICImagingFactory, IWICStream, WICBitmapEncoderNoCache,
};
use windows::Win32::Media::MediaFoundation::{
    ICodecAPI, IMFActivate, IMFAttributes, IMFMediaSource, IMFMediaType, IMFSample,
    IMFSourceReader, IMFTransform, MFCreateAttributes, MFCreateMediaType, MFCreateMemoryBuffer,
    MFCreateSample, MFCreateSourceReaderFromMediaSource, MFEnumDeviceSources,
    MFTEnumEx, MFT_CATEGORY_VIDEO_ENCODER, MFT_ENUM_FLAG_ASYNCMFT, MFT_ENUM_FLAG_HARDWARE,
    MFT_ENUM_FLAG_LOCALMFT, MFT_ENUM_FLAG_SORTANDFILTER, MFT_ENUM_FLAG_SYNCMFT,
    MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, MFT_MESSAGE_NOTIFY_START_OF_STREAM,
    MFT_OUTPUT_DATA_BUFFER, MFT_OUTPUT_STREAM_PROVIDES_SAMPLES, MFT_REGISTER_TYPE_INFO,
    MFMediaType_Video, MFStartup, MFShutdown, MFVideoFormat_H264, MFVideoFormat_HEVC,
    MFVideoFormat_NV12, MFVideoFormat_YUY2, MFVideoInterlace_Progressive, MF_MT_AVG_BITRATE,
    MF_MT_DEFAULT_STRIDE, MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE, MF_MT_INTERLACE_MODE,
    MF_MT_MAJOR_TYPE, MF_MT_MPEG_SEQUENCE_HEADER, MF_MT_PIXEL_ASPECT_RATIO, MF_MT_SUBTYPE,
    MF_DEVSOURCE_ATTRIBUTE_FRIENDLY_NAME, MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE,
    MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_GUID,
    MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_SYMBOLIC_LINK, MF_LOW_LATENCY, MF_VERSION,
    MF_E_INVALIDMEDIATYPE, MF_E_INVALIDTYPE, MF_E_NOTACCEPTING, MF_E_TRANSFORM_NEED_MORE_INPUT,
    MF_E_TRANSFORM_STREAM_CHANGE, MF_SOURCE_READERF_CURRENTMEDIATYPECHANGED,
    MF_SOURCE_READERF_ENDOFSTREAM, MF_SOURCE_READERF_ERROR, MF_SOURCE_READER_FIRST_VIDEO_STREAM,
    MF_SOURCE_READER_ENABLE_ADVANCED_VIDEO_PROCESSING, MFSTARTUP_NOSOCKET,
    MFSampleExtension_CleanPoint, MF_TRANSFORM_ASYNC, MF_TRANSFORM_ASYNC_UNLOCK,
    CODECAPI_AVEncCommonMeanBitRate, CODECAPI_AVEncCommonRateControlMode, CODECAPI_AVEncMPVGOPSize,
    CODECAPI_AVLowLatencyMode, eAVEncCommonRateControlMode_CBR,
};
use windows::Win32::System::Com::StructuredStorage::{CreateStreamOnHGlobal, IPropertyBag2};
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree,
    CoUninitialize, IStream, STATFLAG_NONAME, STATSTG, STREAM_SEEK_SET,
};
use windows::Win32::System::Variant::{
    VARENUM, VARIANT, VARIANT_0, VARIANT_0_0, VARIANT_0_0_0, VT_BOOL, VT_UI4,
};
use windows::core::{GUID, Interface};

use crate::error::CaptureError;
use crate::source::{FrameCallback, JpegCallback};
use crate::{CameraInfo, CaptureConfig};

/// How many media types of one device are inspected before the enumeration gives up.
///
/// Only a guard: `GetNativeMediaType`/`GetMediaTypeByIndex` signal the end with an error.
const MAX_MEDIA_TYPES: u32 = 256;

/// How long a `set_parameters` rebuild waits before the command is considered lost.
const COMMAND_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// `MF_SOURCE_READER_FIRST_VIDEO_STREAM` as the plain `u32` the source reader methods take.
const VIDEO_STREAM: u32 = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;

/// Video capture devices of the host, with the formats each of them announces.
///
/// Each device is activated to read its presentation descriptor, which is the cheapest way
/// to reach the media types a camera supports; a device that is already in use may refuse
/// to activate, in which case it is still listed, just without presets.
///
/// Runs on the calling thread, which therefore has to be able to host COM — see
/// [`ComApartment`]. No capture objects are created or kept here.
pub(crate) fn enumerate_cameras() -> Result<Vec<CameraInfo>, CaptureError> {
    let _apartment = ComApartment::init()?;
    let _runtime = MfRuntime::start()?;

    let devices = device_activates()?;
    if devices.is_empty() {
        return Err(CaptureError::NoCamera);
    }

    let mut cameras = Vec::with_capacity(devices.len());
    for device in &devices {
        let id = attribute_string(device, &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_SYMBOLIC_LINK)
            .unwrap_or_default();
        let name = attribute_string(device, &MF_DEVSOURCE_ATTRIBUTE_FRIENDLY_NAME).unwrap_or_default();
        let presets = match device_presets(device) {
            Ok(presets) => presets,
            Err(failure) => {
                debug!(camera = %name, error = %failure, "cannot read the formats of this camera");
                Vec::new()
            }
        };
        cameras.push(CameraInfo { id, name, presets });
    }
    Ok(cameras)
}

/// One capture pipeline, as the Rust side sees it.
///
/// The thread is joined by [`Handle::close`] — [`CaptureSource`](crate::CaptureSource)
/// always calls it before dropping the callback state the pump thread still points at.
pub(crate) struct Handle {
    commands: Sender<Command>,
    pump: Option<JoinHandle<()>>,
}

/// What the pump thread is asked to do between frames.
enum Command {
    /// Rebuild the camera and the encoder; the reply carries the outcome.
    Reconfigure {
        width: u32,
        height: u32,
        fps: u32,
        codec: Codec,
        bitrate_kbps: u32,
        reply: SyncSender<Result<(), CaptureError>>,
    },
    /// Encode the latest camera frame, hand it to `cb`, then reply.
    Photo {
        cb: JpegCallback,
        user: SendPtr,
        reply: SyncSender<Result<(), CaptureError>>,
    },
    /// Stop capturing and leave the pump loop.
    Stop,
}

/// A `user` pointer on its way to the pump thread.
struct SendPtr(*mut c_void);

// SAFETY: the pointer is the one `source.rs` registered for the whole life of the
// [`CaptureSource`](crate::CaptureSource): it addresses the `CallbackState` box, which
// outlives the backend, and the pump thread only ever passes it back into the callbacks.
unsafe impl Send for SendPtr {}

/// The pump thread is gone, so nothing will ever answer a command again.
fn pump_gone() -> CaptureError {
    CaptureError::Os("the capture thread is gone".to_owned())
}

impl Handle {
    /// Starts the pump thread and waits for the first pipeline to come up.
    pub(crate) fn open(
        cfg: &CaptureConfig,
        frame_cb: FrameCallback,
        _jpeg_cb: JpegCallback,
        user: *mut c_void,
    ) -> Result<Self, CaptureError> {
        // Reject an unusable codec here, on the caller's thread, rather than through the
        // startup handshake.
        video_subtype(cfg.codec)?;

        let (commands, incoming) = mpsc::channel();
        let (ready, started) = mpsc::channel();
        let config = cfg.clone();
        let user = SendPtr(user);

        let pump = thread::Builder::new()
            .name("scwc-capture".to_owned())
            .spawn(move || pump_main(&config, frame_cb, user, &incoming, &ready))
            .map_err(|failure| CaptureError::Os(format!("cannot start the capture thread: {failure}")))?;

        // The thread reports the outcome of the first capture session, if it ever reports
        // anything: a channel that closes without a message means the thread died.
        match started.recv() {
            Ok(Ok(())) => Ok(Self {
                commands,
                pump: Some(pump),
            }),
            Ok(Err(failure)) => {
                let _ = pump.join();
                Err(failure)
            }
            Err(_) => {
                let _ = pump.join();
                Err(CaptureError::Os(
                    "the capture thread stopped while starting".to_owned(),
                ))
            }
        }
    }

    /// Rebuilds the pipeline with new capture and encoder parameters.
    pub(crate) fn set_parameters(
        &self,
        width: u32,
        height: u32,
        fps: u32,
        codec: Codec,
        bitrate_kbps: u32,
    ) -> Result<(), CaptureError> {
        let (reply, outcome) = mpsc::sync_channel(1);
        self.commands
            .send(Command::Reconfigure {
                width,
                height,
                fps,
                codec,
                bitrate_kbps,
                reply,
            })
            .map_err(|_| pump_gone())?;
        outcome.recv_timeout(COMMAND_TIMEOUT).unwrap_or_else(|_| Err(pump_gone()))
    }

    /// Captures one still, delivered through `jpeg_cb` before this function returns.
    pub(crate) fn take_photo(
        &self,
        jpeg_cb: JpegCallback,
        user: *mut c_void,
    ) -> Result<(), CaptureError> {
        let (reply, outcome) = mpsc::sync_channel(1);
        self.commands
            .send(Command::Photo {
                cb: jpeg_cb,
                user: SendPtr(user),
                reply,
            })
            .map_err(|_| pump_gone())?;
        outcome.recv_timeout(COMMAND_TIMEOUT).unwrap_or_else(|_| Err(pump_gone()))
    }

    /// Stops the pipeline; no callback fires afterwards.
    ///
    /// The join waits for the pump thread, which is between two camera frames at worst — but
    /// a camera that stops delivering frames without ending its stream leaves the thread
    /// inside `ReadSample`, and a synchronous source reader has no way to be told to give up.
    /// UNCERTAIN: an asynchronous reader (`IMFSourceReaderCallback` plus `ReadSample` with
    /// `MF_SOURCE_READER_ASYNC_CALLBACK`) would not have this property and is what a session
    /// that has to survive a camera being unplugged would use.
    pub(crate) fn close(&mut self) {
        let Some(pump) = self.pump.take() else {
            return;
        };
        // The send fails harmlessly when the thread has already left the loop.
        let _ = self.commands.send(Command::Stop);
        if pump.join().is_err() {
            error!("the capture thread panicked");
        }
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        // `CaptureSource::drop` closes explicitly; this only catches a leak of a `Handle`
        // that never got that far.
        self.close();
    }
}

/// Everything the pump thread owns.
struct Pump {
    /// Device the pipeline was opened for; `None` is the default camera and stays `None`
    /// across rebuilds so that `set_parameters` keeps following the default device.
    camera_id: Option<String>,
    pipeline: Option<Pipeline>,
    frame_cb: FrameCallback,
    user: SendPtr,
}

/// Whether the pump loop keeps running.
enum Step {
    Continue,
    Stop,
}

/// Body of the pump thread: brings up the first pipeline, then serves the commands.
fn pump_main(
    cfg: &CaptureConfig,
    frame_cb: FrameCallback,
    user: SendPtr,
    commands: &Receiver<Command>,
    ready: &Sender<Result<(), CaptureError>>,
) {
    // Declared first, so COM outlives every Media Foundation object created below.
    let _apartment = match ComApartment::init() {
        Ok(apartment) => apartment,
        Err(failure) => {
            let _ = ready.send(Err(failure));
            return;
        }
    };
    let _runtime = match MfRuntime::start() {
        Ok(runtime) => runtime,
        Err(failure) => {
            let _ = ready.send(Err(failure));
            return;
        }
    };

    let mut pump = Pump {
        camera_id: cfg.camera_id.clone(),
        pipeline: None,
        frame_cb,
        user,
    };
    if let Err(failure) = pump.rebuild(cfg) {
        let _ = ready.send(Err(failure));
        return;
    }
    let _ = ready.send(Ok(()));
    pump.run(commands);
}

impl Pump {
    /// Serves commands and encodes frames until the pipeline is stopped.
    fn run(&mut self, commands: &Receiver<Command>) {
        loop {
            // Commands first: a stop or a photo must not wait for the camera to produce
            // another frame.
            let mut step = Step::Continue;
            loop {
                match commands.try_recv() {
                    Ok(command) => step = self.serve(command),
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => step = Step::Stop,
                }
                if matches!(step, Step::Stop) {
                    break;
                }
            }
            if matches!(step, Step::Stop) {
                break;
            }

            if self.pipeline.is_none() {
                // Nothing to pull from: the next command decides whether capture resumes.
                match commands.recv() {
                    Ok(command) => {
                        if matches!(self.serve(command), Step::Stop) {
                            break;
                        }
                    }
                    Err(_) => break,
                }
                continue;
            }

            let pipeline = self
                .pipeline
                .as_mut()
                .expect("checked with `is_none` just above");
            if let Err(failure) = pipeline.pull(self.frame_cb, self.user.0) {
                error!(error = %failure, "the capture pipeline stopped");
                // The camera is released here; a later `set_parameters` rebuilds it.
                self.pipeline = None;
            }
        }
        // Dropping the pipeline stops the camera and frees the encoder.
    }

    /// Answers one command.
    fn serve(&mut self, command: Command) -> Step {
        match command {
            Command::Stop => Step::Stop,
            Command::Reconfigure {
                width,
                height,
                fps,
                codec,
                bitrate_kbps,
                reply,
            } => {
                let config = CaptureConfig {
                    camera_id: self.camera_id.clone(),
                    width,
                    height,
                    fps,
                    codec,
                    bitrate_kbps,
                };
                let outcome = self.rebuild(&config);
                let _ = reply.send(outcome);
                Step::Continue
            }
            Command::Photo { cb, user, reply } => {
                let outcome = match self.pipeline.as_ref() {
                    Some(pipeline) => pipeline.take_photo(cb, user.0),
                    None => Err(CaptureError::Os("no camera is running".to_owned())),
                };
                let _ = reply.send(outcome);
                Step::Continue
            }
        }
    }

    /// Replaces the pipeline with one built for `cfg`.
    ///
    /// The replacement is built next to the running pipeline first, so that a failed
    /// `switch_camera` leaves the current capture alone — but a camera usually refuses a
    /// second reader, so if that fails the running pipeline is released and the build is
    /// retried in its place.
    fn rebuild(&mut self, cfg: &CaptureConfig) -> Result<(), CaptureError> {
        let rebuilt = if self.pipeline.is_none() {
            Pipeline::open(cfg)?
        } else {
            match Pipeline::open(cfg) {
                Ok(pipeline) => pipeline,
                Err(first) => {
                    debug!(error = %first, "cannot rebuild next to the running pipeline; releasing the camera first");
                    self.pipeline = None;
                    Pipeline::open(cfg)?
                }
            }
        };
        self.pipeline = Some(rebuilt);
        Ok(())
    }
}

/// One live camera plus its encoder.
struct Pipeline {
    reader: IMFSourceReader,
    encoder: Encoder,
    /// Frame geometry as the reader actually delivers it.
    width: u32,
    height: u32,
    /// Bytes per row of the delivered frame; negative when the frame is bottom-up.
    stride: i32,
    format: PixelFormat,
    /// Latest camera frame, kept for `take_photo`.
    still: Option<StillFrame>,
    /// Reused staging for the frames the live preview gets.
    preview: Vec<u8>,
    /// Shortest spacing between two frames handed to the encoder, in 100 ns units.
    frame_interval: i64,
    /// Timestamp of the last frame handed to the encoder.
    last_submitted: Option<i64>,
}

impl Pipeline {
    /// Opens the camera and the encoder for `cfg`.
    ///
    /// Fails with [`CaptureError::UnsupportedParameters`] when the camera cannot capture at
    /// the requested size, when it will not deliver an uncompressed layout an encoder takes,
    /// or when no encoder produces `cfg.codec`.
    fn open(cfg: &CaptureConfig) -> Result<Self, CaptureError> {
        if cfg.width == 0 || cfg.height == 0 || cfg.fps == 0 {
            return Err(CaptureError::UnsupportedParameters);
        }
        let output_subtype = video_subtype(cfg.codec)?;

        let device = select_device(cfg.camera_id.as_deref())?;
        // SAFETY: the activation is live and only the pump thread uses what it creates.
        let source: IMFMediaSource = unsafe { device.ActivateObject() }
            .map_err(capture_error("activating the camera"))?;

        let attributes = create_attributes(2)?;
        // SAFETY: the attribute store is live and this is one of its documented keys.
        // Letting the reader insert the video processor is what makes MJPEG-only cameras
        // usable at all: it converts to NV12 on the way out.
        unsafe { attributes.SetUINT32(&MF_SOURCE_READER_ENABLE_ADVANCED_VIDEO_PROCESSING, 1) }
            .map_err(capture_error("configuring the source reader"))?;
        // SAFETY: same store, Media Foundation's own factory function.
        let reader = unsafe { MFCreateSourceReaderFromMediaSource(&source, &attributes) }
            .map_err(capture_error("opening the camera"))?;

        // The camera has to capture at the requested size; the announced frame rate closest
        // to the request is chosen, and the surplus frames are dropped further down.
        let (native, native_fps) = select_native_type(&reader, cfg)?;
        // SAFETY: the reader is live and the type was built by Media Foundation.
        unsafe { reader.SetCurrentMediaType(VIDEO_STREAM, None, &native) }
            .map_err(capture_error("setting the camera format"))?;

        // Encoders take uncompressed frames; NV12 is what the hardware ones want, YUY2 is the
        // fallback for cameras whose driver will not convert.
        for format in [PixelFormat::Nv12, PixelFormat::Yuy2] {
            let requested = build_frame_type(format, cfg.width, cfg.height, native_fps, None)?;
            // SAFETY: as above; a layout the reader cannot produce is a normal outcome here.
            if let Err(failure) = unsafe { reader.SetCurrentMediaType(VIDEO_STREAM, None, &requested) } {
                debug!(?format, code = ?failure.code(), "the camera will not deliver this frame layout");
                continue;
            }

            // SAFETY: the reader has a current type now.
            let Ok(current) = (unsafe { reader.GetCurrentMediaType(VIDEO_STREAM) }) else {
                continue;
            };
            // The reader may have adjusted the type; the encoder has to be given what the
            // reader really delivers, stride included.
            let (width, height) = attribute_size(&current, &MF_MT_FRAME_SIZE)
                .unwrap_or((cfg.width, cfg.height));
            let stride = attribute_u32(&current, &MF_MT_DEFAULT_STRIDE)
                .map(|value| value as i32)
                .unwrap_or_else(|| format.tight_stride(width));
            let input = build_frame_type(format, width, height, native_fps, Some(stride))?;

            // The GOP is sized in frames of what the server actually receives: the throttle
            // above drops the camera's surplus frames, so two seconds is `2 * cfg.fps` frames.
            match Encoder::open(
                cfg.codec,
                output_subtype,
                &input,
                cfg.fps.saturating_mul(2),
                cfg.bitrate_kbps,
            ) {
                Ok(encoder) => {
                    return Ok(Self {
                        reader,
                        encoder,
                        width,
                        height,
                        stride,
                        format,
                        still: None,
                        preview: Vec::new(),
                        frame_interval: frame_interval(cfg.fps),
                        last_submitted: None,
                    })
                }
                Err(failure) => {
                    warn!(?format, error = %failure, "no encoder takes camera frames in this layout");
                }
            }
        }
        Err(CaptureError::UnsupportedParameters)
    }

    /// Reads one camera frame, runs it through the encoder and reports every access unit.
    ///
    /// Returns when the encoder failed; a return of `Ok` means one camera frame was
    /// processed. End of stream and device loss are reported as errors, which drops the
    /// pipeline and stops the frames.
    fn pull(&mut self, frame_cb: FrameCallback, user: *mut c_void) -> Result<(), CaptureError> {
        loop {
            let mut flags = 0u32;
            let mut sample: Option<IMFSample> = None;
            // SAFETY: the reader is live and the out parameters are locals. `ReadSample`
            // blocks until the camera has a frame, which is the pacing of this thread.
            unsafe {
                self.reader.ReadSample(
                    VIDEO_STREAM,
                    0,
                    None,
                    Some(&mut flags),
                    None,
                    Some(&mut sample),
                )
            }
            .map_err(capture_error("reading a camera frame"))?;

            if let Some(sample) = sample {
                return self.submit(&sample, frame_cb, user);
            }

            if flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32 != 0 {
                return Err(CaptureError::Os("the camera ended the stream".to_owned()));
            }
            if flags & MF_SOURCE_READERF_ERROR.0 as u32 != 0 {
                return Err(CaptureError::Os("the camera reported an error".to_owned()));
            }
            if flags & MF_SOURCE_READERF_CURRENTMEDIATYPECHANGED.0 as u32 != 0 {
                // The camera switched format behind our back; the frame after this one is
                // still framed as before, so keep going and note it.
                warn!("the camera changed its media type mid-stream");
            }
            // A stream tick carries no frame; read again.
        }
    }

    /// Feeds one camera frame to the encoder and hands the resulting access units over.
    fn submit(
        &mut self,
        sample: &IMFSample,
        frame_cb: FrameCallback,
        user: *mut c_void,
    ) -> Result<(), CaptureError> {
        self.capture_still(sample);
        // The preview comes from the frame the photo path just kept, which is the one the
        // encoder is about to be given: what the interface shows is what is being recorded.
        self.publish_preview();
        if !self.should_submit(sample) {
            return Ok(());
        }

        if !self.encoder.submit(sample)? {
            // The MFT is still holding the previous frame: drain it and try again.
            let pending = self.encoder.drain()?;
            self.emit(pending, frame_cb, user)?;
            if !self.encoder.submit(sample)? {
                return Err(CaptureError::Os("the encoder refused a camera frame".to_owned()));
            }
        }

        let ready = self.encoder.drain()?;
        self.emit(ready, frame_cb, user)
    }

    /// Whether this frame should reach the encoder.
    ///
    /// Cameras stream at the rate of the format they announced, which can be higher than the
    /// requested one; dropping the surplus keeps the output at the bitrate the request was
    /// sized for. The decision uses the frame timestamps, so it is approximate — and it is
    /// skipped entirely for frames without one.
    fn should_submit(&mut self, sample: &IMFSample) -> bool {
        // SAFETY: the sample is a live Media Foundation sample.
        let Ok(time) = (unsafe { sample.GetSampleTime() }) else {
            return true;
        };
        let keep = match self.last_submitted {
            // A camera that restarted its clock would otherwise never pass a frame again.
            Some(last) if time <= last => true,
            Some(last) => time - last >= self.frame_interval - self.frame_interval / 4,
            None => true,
        };
        if keep {
            self.last_submitted = Some(time);
        }
        keep
    }

    /// Keeps a copy of the frame `take_photo` will encode.
    ///
    /// The source reader recycles its buffers, so the bytes have to be copied to survive the
    /// next `ReadSample`. One frame-sized copy per camera frame is the price of a photo that
    /// is immediate rather than one camera frame late; the buffer is reused.
    fn capture_still(&mut self, sample: &IMFSample) {
        let width = self.width;
        let height = self.height;
        let stride = self.stride;
        let format = self.format;
        let still = self.still.get_or_insert_with(|| StillFrame {
            width,
            height,
            stride,
            format,
            data: Vec::new(),
        });
        match copy_sample_bytes(sample, &mut still.data) {
            Ok(()) => {
                still.width = width;
                still.height = height;
                still.stride = stride;
                still.format = format;
            }
            Err(failure) => warn!(error = %failure, "cannot keep a still of the current camera frame"),
        }
    }

    /// Offers the newest camera frame to the live preview.
    ///
    /// Built from the copy the photo path keeps rather than from the reader's own buffer, which
    /// the source recycles. Nothing is converted while no preview is wanted: the conversion is a
    /// whole-frame pass.
    fn publish_preview(&mut self) {
        if !scwc_core::preview::preview_enabled() {
            return;
        }
        let Some(still) = self.still.as_ref().filter(|still| !still.data.is_empty()) else {
            return;
        };
        match still.to_bgra(&mut self.preview) {
            Ok(()) => scwc_core::preview::publish_bgra(&self.preview, still.width, still.height),
            Err(error) => warn!(%error, "could not build the preview frame"),
        }
    }

    /// Hands each encoded access unit to the core, normalised to Annex B.
    fn emit(
        &mut self,
        samples: Vec<IMFSample>,
        frame_cb: FrameCallback,
        user: *mut c_void,
    ) -> Result<(), CaptureError> {
        for sample in &samples {
            // The encoder publishes its parameter sets once it has started; pick them up the
            // first time they appear.
            if self.encoder.sequence_header().is_none() {
                self.encoder.refresh_sequence_header();
            }

            let payload = sample_bytes(sample)?;
            let (annex_b, has_idr) = to_annex_b(self.encoder.codec, &payload);
            let keyframe = is_clean_point(sample) || has_idr;
            let data = if keyframe {
                self.encoder.with_sequence_header(annex_b)
            } else {
                annex_b
            };
            if data.is_empty() {
                continue;
            }
            // SAFETY: `frame_cb` and `user` are the pair `source.rs` handed to `open`; the
            // callback copies the bytes before it returns, and `CaptureSource::drop` closes
            // the pipeline before the state behind `user` is freed.
            unsafe { frame_cb(user, data.as_ptr(), data.len(), keyframe as u8) };
        }
        Ok(())
    }

    /// Encodes the latest camera frame as JPEG and hands it to `cb`.
    fn take_photo(&self, cb: JpegCallback, user: *mut c_void) -> Result<(), CaptureError> {
        let Some(still) = self.still.as_ref().filter(|still| !still.data.is_empty()) else {
            return Err(CaptureError::Os(
                "no camera frame has been captured yet".to_owned(),
            ));
        };
        let jpeg = encode_jpeg(still)?;
        // SAFETY: see `Pipeline::emit`; the callback copies the bytes before returning.
        unsafe { cb(user, jpeg.as_ptr(), jpeg.len()) };
        Ok(())
    }
}

/// One configured encoder MFT.
struct Encoder {
    transform: IMFTransform,
    codec: Codec,
    /// True when the MFT allocates its own output samples.
    provides_samples: bool,
    /// Buffer size to allocate when it does not.
    output_buffer_size: u32,
    /// Parameter sets from the encoder's output type, once it published them.
    sequence_header: Option<Vec<u8>>,
}

impl Encoder {
    /// Finds and configures an encoder for `codec` that takes frames shaped like `input`.
    fn open(
        codec: Codec,
        output_subtype: GUID,
        input: &IMFMediaType,
        gop_frames: u32,
        bitrate_kbps: u32,
    ) -> Result<Self, CaptureError> {
        // SAFETY: `input` is a live media type built by this crate.
        let input_subtype =
            unsafe { input.GetGUID(&MF_MT_SUBTYPE) }.map_err(capture_error("reading a frame layout"))?;
        let (width, height) = attribute_size(input, &MF_MT_FRAME_SIZE).unwrap_or_default();
        // The output type mirrors the frames the encoder is given; their rate is the camera's,
        // which is also what the frame timestamps carry.
        let (rate, _) = attribute_ratio(input, &MF_MT_FRAME_RATE).unwrap_or((30, 1));
        let output = build_output_type(output_subtype, width, height, rate, bitrate_kbps)?;

        let transform = find_transform(codec, &input_subtype, &output_subtype)?;
        Self::configure(transform, codec, input, &output, gop_frames, bitrate_kbps)
    }

    /// Wires up the MFT: media types, rate control, streaming.
    ///
    /// `gop_frames` is the GOP length in frames of the stream this pipeline sends, which is
    /// not the camera's frame rate when the surplus frames are dropped.
    fn configure(
        transform: IMFTransform,
        codec: Codec,
        input: &IMFMediaType,
        output: &IMFMediaType,
        gop_frames: u32,
        bitrate_kbps: u32,
    ) -> Result<Self, CaptureError> {
        // SAFETY: the transform is live. Hardware transforms are asynchronous MFTs;
        // unlocking them is what allows driving them with the blocking
        // `ProcessInput`/`ProcessOutput` model used here.
        // UNCERTAIN: documented, but not exercised on this host. A transform that keeps
        // behaving asynchronously would need the event model instead.
        //
        // UNCERTAIN: hardware transforms normally run on D3D11 surfaces, which is what
        // `MFT_MESSAGE_SET_D3D_MANAGER` is for; here they are handed the reader's
        // system-memory NV12 as it is. The encoders accept that — the frame is staged into
        // GPU memory inside the transform — but it costs a copy per frame and is the first
        // thing to change if a hardware path underperforms.
        let attributes = unsafe { transform.GetAttributes() }.map_err(capture_error("reading encoder attributes"))?;
        let asynchronous = unsafe { attributes.GetUINT32(&MF_TRANSFORM_ASYNC) }.unwrap_or(0) != 0;
        if asynchronous {
            unsafe { attributes.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1) }
                .map_err(capture_error("unlocking an asynchronous encoder"))?;
        }
        // Best effort: a latency hint the inbox software encoder ignores.
        let _ = unsafe { attributes.SetUINT32(&MF_LOW_LATENCY, 1) };

        // Encoders negotiate their output type first, so that they can derive what their
        // input has to look like.
        // SAFETY: both types are live media types.
        unsafe { transform.SetOutputType(0, output, 0) }.map_err(capture_error("setting the encoder output type"))?;
        unsafe { transform.SetInputType(0, input, 0) }.map_err(capture_error("setting the encoder input type"))?;

        // SAFETY: the transform is configured.
        let info = unsafe { transform.GetOutputStreamInfo(0) }.map_err(capture_error("reading the encoder output stream"))?;
        let provides_samples =
            info.dwFlags & MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32 != 0;
        if !provides_samples {
            // UNCERTAIN: the inbox software encoder allocates its own samples; a transform
            // that does not must be handed a sample per output buffer.
            debug!(size = info.cbSize, "the encoder wants caller-allocated output samples");
        }

        // Rate control: the protocol wants a fixed budget, so CBR at a low latency, and a GOP
        // of two seconds so that a lost packet costs at most that much.
        // SAFETY: the transform is configured; MFTs that expose `ICodecAPI` support these
        // properties, the rest are logged and left at their defaults.
        if let Ok(codec_api) = transform.cast::<ICodecAPI>() {
            set_codec_value(
                &codec_api,
                &CODECAPI_AVEncCommonMeanBitRate,
                &variant_u32(bitrate_kbps.saturating_mul(1000)),
            );
            set_codec_value(
                &codec_api,
                &CODECAPI_AVEncCommonRateControlMode,
                &variant_u32(eAVEncCommonRateControlMode_CBR.0 as u32),
            );
            set_codec_value(&codec_api, &CODECAPI_AVLowLatencyMode, &variant_bool(true));
            set_codec_value(
                &codec_api,
                &CODECAPI_AVEncMPVGOPSize,
                &variant_u32(gop_frames.max(1)),
            );
        }

        // SAFETY: the types are set, so the transform is ready to stream; `START_OF_STREAM`
        // carries no parameter.
        unsafe { transform.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0) }
            .map_err(capture_error("starting the encoder"))?;
        unsafe { transform.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0) }
            .map_err(capture_error("starting the encoder"))?;

        Ok(Self {
            transform,
            codec,
            provides_samples,
            output_buffer_size: info.cbSize,
            sequence_header: None,
        })
    }

    /// Parameter sets the encoder published, if it did.
    fn sequence_header(&self) -> Option<&[u8]> {
        self.sequence_header.as_deref()
    }

    /// Prepends the parameter sets to a keyframe the encoder did not precede them with.
    fn with_sequence_header(&self, access_unit: Vec<u8>) -> Vec<u8> {
        let Some(header) = self.sequence_header() else {
            return access_unit;
        };
        if access_unit.starts_with(header) {
            return access_unit;
        }
        let mut framed = Vec::with_capacity(header.len() + access_unit.len());
        framed.extend_from_slice(header);
        framed.extend_from_slice(&access_unit);
        framed
    }

    /// Reads the parameter sets off the encoder's current output type.
    ///
    /// The attribute only exists once the encoder has something to put in it, so this is
    /// called until it succeeds.
    fn refresh_sequence_header(&mut self) {
        // SAFETY: the transform is configured and streaming.
        let Ok(media_type) = (unsafe { self.transform.GetOutputCurrentType(0) }) else {
            return;
        };
        let Some(header) = attribute_blob(&media_type, &MF_MT_MPEG_SEQUENCE_HEADER) else {
            return;
        };
        if is_annex_b(&header) {
            self.sequence_header = Some(header);
        } else {
            // UNCERTAIN: the attribute is documented as holding the parameter sets, but not
            // in which shape; anything that is not an Annex B stream is left alone rather
            // than spliced into the frames.
            warn!(bytes = header.len(), "ignoring parameter sets in an unknown format");
            self.sequence_header = None;
        }
    }

    /// Feeds one frame in.
    ///
    /// `Ok(false)` means the MFT is not accepting input yet and its output has to be drained
    /// first.
    fn submit(&mut self, sample: &IMFSample) -> Result<bool, CaptureError> {
        // SAFETY: the sample is live and belongs to the reader that produced it.
        match unsafe { self.transform.ProcessInput(0, sample, 0) } {
            Ok(()) => Ok(true),
            Err(failure) if failure.code() == MF_E_NOTACCEPTING => Ok(false),
            Err(failure) => Err(capture_error("feeding a camera frame to the encoder")(failure)),
        }
    }

    /// Pulls every access unit the encoder has ready.
    fn drain(&mut self) -> Result<Vec<IMFSample>, CaptureError> {
        let mut samples = Vec::new();
        loop {
            let mut output = [MFT_OUTPUT_DATA_BUFFER {
                dwStreamID: 0,
                pSample: ManuallyDrop::new(None),
                dwStatus: 0,
                pEvents: ManuallyDrop::new(None),
            }];
            if !self.provides_samples {
                output[0].pSample = ManuallyDrop::new(Some(create_output_sample(self.output_buffer_size)?));
            }

            let mut status = 0u32;
            // SAFETY: the transform is streaming and the buffer array is a live local.
            let outcome = unsafe { self.transform.ProcessOutput(0, &mut output, &mut status) };
            // The sample fields are `ManuallyDrop`, so whatever the MFT put there has to be
            // taken back explicitly or it leaks.
            let mut output = output.into_iter().next().expect("one output buffer");
            let _events = ManuallyDrop::into_inner(replace(&mut output.pEvents, ManuallyDrop::new(None)));
            let sample = ManuallyDrop::into_inner(replace(&mut output.pSample, ManuallyDrop::new(None)));

            match outcome {
                Ok(()) => match sample {
                    Some(sample) => samples.push(sample),
                    None => {
                        // Success without a sample means the stream is done; retrying would
                        // spin on a callback thread.
                        debug!("the encoder reported an output without a sample");
                        return Ok(samples);
                    }
                },
                Err(failure) if failure.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => {
                    return Ok(samples)
                }
                Err(failure) if failure.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                    self.renegotiate_output()?;
                }
                Err(failure) => return Err(capture_error("reading an encoded frame")(failure)),
            }
        }
    }

    /// Re-negotiates the output type after `MF_E_TRANSFORM_STREAM_CHANGE`.
    ///
    /// UNCERTAIN: encoders mostly raise this while settling, and the parameters that change
    /// (profile, size) are not re-derived here — the new type is only installed and logged.
    fn renegotiate_output(&mut self) -> Result<(), CaptureError> {
        warn!("the encoder changed its output type mid-stream");
        // SAFETY: the transform is streaming.
        let media_type = unsafe { self.transform.GetOutputAvailableType(0, 0) }
            .map_err(capture_error("querying the encoder output type"))?;
        unsafe { self.transform.SetOutputType(0, &media_type, 0) }
            .map_err(capture_error("setting the encoder output type"))?;
        // The parameter sets may have changed with the type.
        self.sequence_header = None;
        Ok(())
    }
}

/// One camera frame kept for `take_photo`.
struct StillFrame {
    width: u32,
    height: u32,
    /// Bytes per row; negative when the frame is bottom-up.
    stride: i32,
    format: PixelFormat,
    data: Vec<u8>,
}

/// The uncompressed layouts this backend understands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PixelFormat {
    /// Luma plane followed by one interleaved chroma plane at half resolution.
    Nv12,
    /// Interleaved luma and chroma at half horizontal resolution.
    Yuy2,
}

impl PixelFormat {
    /// The Media Foundation subtype of this layout.
    fn subtype(self) -> GUID {
        match self {
            Self::Nv12 => MFVideoFormat_NV12,
            Self::Yuy2 => MFVideoFormat_YUY2,
        }
    }

    /// Bytes per row of a frame with no padding.
    fn tight_stride(self, width: u32) -> i32 {
        match self {
            Self::Nv12 => width as i32,
            Self::Yuy2 => width.saturating_mul(2) as i32,
        }
    }
}

impl StillFrame {
    /// Converts the frame to the packed, top-down 24-bit BGR the JPEG encoder takes.
    fn to_bgr(&self) -> Result<Vec<u8>, CaptureError> {
        let mut bgr = Vec::new();
        self.convert::<3>(&mut bgr)?;
        Ok(bgr)
    }

    /// Converts the frame to the packed, top-down BGRA the live preview wants, replacing the
    /// contents of `bgra`.
    fn to_bgra(&self, bgra: &mut Vec<u8>) -> Result<(), CaptureError> {
        self.convert::<4>(bgra)
    }

    /// Converts the frame into a packed, top-down buffer of `CHANNELS` bytes per pixel,
    /// replacing its contents; the fourth channel, when asked for, is opaque.
    fn convert<const CHANNELS: usize>(&self, out: &mut Vec<u8>) -> Result<(), CaptureError> {
        let width = self.width as usize;
        let height = self.height as usize;
        let stride = self.stride.unsigned_abs() as usize;
        if width == 0 || height == 0 || stride == 0 {
            return Err(CaptureError::Os("the captured frame has no pixels".to_owned()));
        }
        // A stride narrower than a row would run the loops below off the end of the copy. A
        // YUY2 row holds whole pixel pairs, and the even column of the last pair reads one
        // byte past its own pair, so that layout is counted in rounded-up pairs.
        let row_bytes = match self.format {
            PixelFormat::Nv12 => width,
            PixelFormat::Yuy2 => (width + (width & 1)) * 2,
        };
        if stride < row_bytes {
            return Err(CaptureError::Os(format!(
                "the captured frame has a {stride}-byte stride, {row_bytes} are needed"
            )));
        }
        let needed = match self.format {
            PixelFormat::Nv12 => stride * (height + height.div_ceil(2)),
            PixelFormat::Yuy2 => stride * height,
        };
        if self.data.len() < needed {
            return Err(CaptureError::Os(format!(
                "the captured frame holds {} bytes, {needed} are needed",
                self.data.len()
            )));
        }

        out.clear();
        out.resize(width * height * CHANNELS, 0);
        let bottom_up = self.stride < 0;
        for row in 0..height {
            let source_row = if bottom_up { height - 1 - row } else { row };
            let target = &mut out[row * width * CHANNELS..(row + 1) * width * CHANNELS];
            match self.format {
                PixelFormat::Nv12 => {
                    let luma = &self.data[source_row * stride..];
                    // One chroma row serves two luma rows.
                    let chroma = &self.data[stride * height + (source_row / 2) * stride..];
                    for column in 0..width {
                        let (u, v) = (chroma[column / 2 * 2] as i32, chroma[column / 2 * 2 + 1] as i32);
                        let pixel = yuv_to_bgr(luma[column] as i32, u, v);
                        let target = &mut target[column * CHANNELS..];
                        target[..3].copy_from_slice(&pixel);
                        if CHANNELS == 4 {
                            target[3] = u8::MAX;
                        }
                    }
                }
                PixelFormat::Yuy2 => {
                    let line = &self.data[source_row * stride..];
                    for column in 0..width {
                        let (y, u, v) = match column % 2 {
                            0 => (line[column * 2] as i32, line[column * 2 + 1] as i32, line[column * 2 + 3] as i32),
                            _ => (line[column * 2] as i32, line[column * 2 - 1] as i32, line[column * 2 + 1] as i32),
                        };
                        let pixel = yuv_to_bgr(y, u, v);
                        let target = &mut target[column * CHANNELS..];
                        target[..3].copy_from_slice(&pixel);
                        if CHANNELS == 4 {
                            target[3] = u8::MAX;
                        }
                    }
                }
            }
        }
        Ok(())
    }
}

/// Encodes one frame as JPEG through the Windows Imaging Component.
fn encode_jpeg(still: &StillFrame) -> Result<Vec<u8>, CaptureError> {
    let bgr = still.to_bgr()?;
    let stride = still.width * 3;

    // The encoded bytes land in an `IStream` over an auto-growing HGLOBAL, which is the only
    // way to find out how long they turned out.
    // SAFETY: COM is initialized on this thread; `null` hands the stream its own allocation.
    let memory: IStream = unsafe { CreateStreamOnHGlobal(HGLOBAL(ptr::null_mut()), true) }
        .map_err(capture_error("creating an in-memory stream"))?;
    // SAFETY: COM is initialized on this thread and the class is registered on every Windows.
    let factory: IWICImagingFactory =
        unsafe { CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER) }
            .map_err(capture_error("creating the imaging factory"))?;

    // SAFETY: the factory is live; the encoder writes into the stream, which outlives it.
    let stream: IWICStream = unsafe { factory.CreateStream() }.map_err(capture_error("creating an imaging stream"))?;
    unsafe { stream.InitializeFromIStream(&memory) }.map_err(capture_error("wrapping the memory stream"))?;
    let encoder = unsafe { factory.CreateEncoder(&GUID_ContainerFormatJpeg, ptr::null()) }
        .map_err(capture_error("creating a JPEG encoder"))?;
    unsafe { encoder.Initialize(&stream, WICBitmapEncoderNoCache) }
        .map_err(capture_error("starting the JPEG encoder"))?;

    let mut frame: Option<IWICBitmapFrameEncode> = None;
    let mut options: Option<IPropertyBag2> = None;
    unsafe { encoder.CreateNewFrame(&mut frame, &mut options) }
        .map_err(capture_error("adding a JPEG frame"))?;
    let frame = frame.ok_or_else(|| CaptureError::Os("the JPEG encoder returned no frame".to_owned()))?;
    // The default encoder options are the quality and subsampling the platform ships; a
    // quality knob would go here.
    unsafe { frame.Initialize(None::<&IPropertyBag2>) }.map_err(capture_error("initialising a JPEG frame"))?;
    unsafe { frame.SetSize(still.width, still.height) }.map_err(capture_error("sizing a JPEG frame"))?;
    let mut pixel_format = GUID_WICPixelFormat24bppBGR;
    unsafe { frame.SetPixelFormat(&mut pixel_format) }.map_err(capture_error("setting the JPEG pixel format"))?;
    if pixel_format != GUID_WICPixelFormat24bppBGR {
        return Err(CaptureError::Os(
            "the JPEG encoder does not accept 24-bit BGR".to_owned(),
        ));
    }
    unsafe { frame.WritePixels(still.height, stride, &bgr) }
        .map_err(capture_error("writing JPEG pixels"))?;
    unsafe { frame.Commit() }.map_err(capture_error("finishing a JPEG frame"))?;
    unsafe { encoder.Commit() }.map_err(capture_error("finishing the JPEG stream"))?;

    // SAFETY: the stream is live and committed; `Stat` reports the bytes written.
    let mut stat = STATSTG::default();
    unsafe { memory.Stat(&mut stat, STATFLAG_NONAME) }.map_err(capture_error("measuring the JPEG"))?;
    let mut jpeg = vec![0u8; stat.cbSize as usize];
    if jpeg.is_empty() {
        return Err(CaptureError::Os("the JPEG encoder produced no bytes".to_owned()));
    }
    // SAFETY: the buffer is writable for its whole length and the stream is rewound first.
    unsafe { memory.Seek(0, STREAM_SEEK_SET, None) }.map_err(capture_error("rewinding the JPEG"))?;
    let mut written = 0u32;
    // `Read` reports `S_FALSE` once the stream is exhausted, which is the normal end here.
    unsafe { memory.Read(jpeg.as_mut_ptr().cast(), jpeg.len() as u32, Some(&mut written)) }
        .ok()
        .map_err(capture_error("reading the JPEG"))?;
    jpeg.truncate(written as usize);
    if jpeg.is_empty() {
        return Err(CaptureError::Os("the JPEG encoder produced no bytes".to_owned()));
    }
    Ok(jpeg)
}

/// Turns one YUV triple into packed BGR, using the BT.601 limited-range matrix that
/// cameras and hardware encoders both work in.
fn yuv_to_bgr(y: i32, u: i32, v: i32) -> [u8; 3] {
    let luma = 298 * (y - 16);
    let blue = (u - 128) * 516;
    let red = (v - 128) * 409;
    let green = -100 * (u - 128) - 208 * (v - 128);
    [
        clamp_u8((luma + blue + 128) >> 8),
        clamp_u8((luma + green + 128) >> 8),
        clamp_u8((luma + red + 128) >> 8),
    ]
}

/// Rounds a colour plane onto a byte.
fn clamp_u8(value: i32) -> u8 {
    value.clamp(0, 255) as u8
}

/// Rewrites one encoder access unit as the Annex B byte stream the protocol expects, and
/// reports whether it carries an IDR.
///
/// Parameter sets are not added here: the encoder either emits them inline or publishes them
/// through `MF_MT_MPEG_SEQUENCE_HEADER`, which
/// [`Encoder::with_sequence_header`] prepends.
fn to_annex_b(codec: Codec, payload: &[u8]) -> (Vec<u8>, bool) {
    if payload.is_empty() {
        return (Vec::new(), false);
    }
    if is_annex_b(payload) {
        return (payload.to_vec(), contains_idr(codec, payload));
    }
    match length_prefixed_to_annex_b(codec, payload) {
        Some(converted) => converted,
        None => {
            // Neither shape parses; hand the access unit over untouched so that the frame is
            // not lost, and say so loudly — the server will not decode it, but it will see
            // that a frame arrived instead of a gap.
            warn!(
                bytes = payload.len(),
                "an encoded frame is neither Annex B nor length-prefixed; passing it through"
            );
            (payload.to_vec(), false)
        }
    }
}

/// Rewrites an access unit of length-prefixed NAL units (a four-byte big-endian length in
/// front of each, as `AVCDecoderConfigurationRecord` uses) as Annex B.
///
/// `None` when the payload does not parse that way, so that a caller can tell a different
/// frame shape apart from a malformed one.
fn length_prefixed_to_annex_b(codec: Codec, payload: &[u8]) -> Option<(Vec<u8>, bool)> {
    let mut framed = Vec::with_capacity(payload.len() + 16);
    let mut keyframe = false;
    let mut offset = 0usize;
    while offset < payload.len() {
        let length = u32::from_be_bytes(payload.get(offset..offset + 4)?.try_into().ok()?) as usize;
        if length == 0 {
            return None;
        }
        let unit = payload.get(offset + 4..offset + 4 + length)?;
        framed.extend_from_slice(&[0, 0, 0, 1]);
        framed.extend_from_slice(unit);
        keyframe |= is_idr_nal(codec, unit);
        offset += 4 + length;
    }
    Some((framed, keyframe))
}

/// Whether an access unit already is an Annex B byte stream.
fn is_annex_b(payload: &[u8]) -> bool {
    payload.starts_with(&[0, 0, 0, 1]) || payload.starts_with(&[0, 0, 1])
}

/// Whether an Annex B byte stream holds an IDR, so that a frame the encoder did not label
/// still counts as a keyframe.
fn contains_idr(codec: Codec, payload: &[u8]) -> bool {
    let mut offset = 0usize;
    while offset + 3 <= payload.len() {
        match start_code_end(payload, offset) {
            Some(unit_start) => {
                if is_idr_nal(codec, &payload[unit_start..]) {
                    return true;
                }
                offset = unit_start;
            }
            None => offset += 1,
        }
    }
    false
}

/// Where the NAL unit after the start code at `offset` begins.
fn start_code_end(payload: &[u8], offset: usize) -> Option<usize> {
    let rest = payload.get(offset..)?;
    if rest.starts_with(&[0, 0, 0, 1]) {
        Some(offset + 4)
    } else if rest.starts_with(&[0, 0, 1]) {
        Some(offset + 3)
    } else {
        None
    }
}

/// Whether a NAL unit is an instantaneous decoder refresh.
///
/// H.265's recovery points are the two IDR types, the three broken-link types and the clean
/// random access type — the set a decoder can start at.
fn is_idr_nal(codec: Codec, unit: &[u8]) -> bool {
    let Some(&header) = unit.first() else {
        return false;
    };
    match codec {
        Codec::H264 => header & 0x1f == 5,
        Codec::H265 => matches!((header >> 1) & 0x3f, 16..=21),
        _ => false,
    }
}

/// Whether the encoder marked the sample as a clean point, its name for a keyframe.
fn is_clean_point(sample: &IMFSample) -> bool {
    // SAFETY: the sample is live; `MFSampleExtension_CleanPoint` is set only on encoder
    // output, so its absence is a "no".
    unsafe { sample.GetUINT32(&MFSampleExtension_CleanPoint) }
        .map(|value| value != 0)
        .unwrap_or(false)
}

/// Shortest spacing between two frames handed to the encoder, in 100 ns units.
fn frame_interval(fps: u32) -> i64 {
    10_000_000 / i64::from(fps.max(1))
}

/// The encoder output subtype for a protocol codec.
fn video_subtype(codec: Codec) -> Result<GUID, CaptureError> {
    match codec {
        Codec::H264 => Ok(MFVideoFormat_H264),
        Codec::H265 => Ok(MFVideoFormat_HEVC),
        _ => Err(CaptureError::UnsupportedParameters),
    }
}

/// The device source the request refers to.
///
/// Without a `camera_id` the first camera Media Foundation lists is taken, which is the one
/// [`enumerate_cameras`] reports first — Windows has no notion of a default camera. An id
/// that no device claims is [`CaptureError::NoCamera`].
fn select_device(camera_id: Option<&str>) -> Result<IMFActivate, CaptureError> {
    let devices = device_activates()?;
    let Some(camera_id) = camera_id else {
        return devices.into_iter().next().ok_or(CaptureError::NoCamera);
    };
    devices
        .into_iter()
        .find(|device| {
            attribute_string(device, &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_SYMBOLIC_LINK)
                .as_deref()
                == Some(camera_id)
        })
        .ok_or(CaptureError::NoCamera)
}

/// Every video capture device of the host, as activations.
fn device_activates() -> Result<Vec<IMFActivate>, CaptureError> {
    let attributes = create_attributes(1)?;
    // SAFETY: the attribute store is live; this is the documented way to ask for cameras.
    unsafe {
        attributes.SetGUID(
            &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE,
            &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_GUID,
        )
    }
    .map_err(capture_error("asking for video capture devices"))?;

    let mut devices: *mut Option<IMFActivate> = ptr::null_mut();
    let mut count = 0u32;
    // SAFETY: the out parameters are locals; the call allocates the array, which
    // `take_activations` frees.
    unsafe { MFEnumDeviceSources(&attributes, &mut devices, &mut count) }
        .map_err(capture_error("listing cameras"))?;
    // SAFETY: `MFEnumDeviceSources` returned the array and its length.
    Ok(unsafe { take_activations(devices, count) })
}

/// Takes over an interface array allocated by Media Foundation and frees the array.
///
/// Both `MFEnumDeviceSources` and `MFTEnumEx` hand back a `CoTaskMemAlloc`'d array of
/// interface pointers whose elements the caller owns — and which has to be freed even when
/// it is empty. `Option<T>` has the layout of a bare interface pointer (the null pointer is
/// `None`), which is how the `windows` crate models these out parameters.
///
/// # Safety
///
/// `array` must be the pointer such a call wrote, together with its reported length, and
/// must not be used again.
unsafe fn take_activations(array: *mut Option<IMFActivate>, count: u32) -> Vec<IMFActivate> {
    let mut activations = Vec::new();
    if !array.is_null() {
        // SAFETY: the caller vouches for the pointer and the length.
        for pointer in unsafe { slice::from_raw_parts(array, count as usize) } {
            // SAFETY: each element owns one reference, which reading the element moves into
            // the `Vec` rather than leaking.
            if let Some(activate) = unsafe { ptr::read(pointer) } {
                activations.push(activate);
            }
        }
        // SAFETY: the array came from the Media Foundation allocator.
        unsafe { CoTaskMemFree(Some(array.cast())) };
    }
    activations
}

/// The formats a device announces, largest resolution first and highest frame rate first
/// within a resolution — the same order the macOS backend reports.
///
/// Reaching the media types needs the device activated; a camera another process holds open
/// refuses that, which is reported as an error and ends up as an empty preset list.
fn device_presets(device: &IMFActivate) -> Result<Vec<(String, Vec<u32>)>, CaptureError> {
    // SAFETY: the activation is live and the source is only used on this thread.
    let source: IMFMediaSource = unsafe { device.ActivateObject() }
        .map_err(capture_error("activating a camera for enumeration"))?;
    // SAFETY: the source is live.
    let descriptor = unsafe { source.CreatePresentationDescriptor() }
        .map_err(capture_error("reading a camera's presentation descriptor"))?;

    let mut selected = windows::core::BOOL::default();
    let mut stream = None;
    // SAFETY: the descriptor is live and the out parameters are locals.
    unsafe { descriptor.GetStreamDescriptorByIndex(0, &mut selected, &mut stream) }
        .map_err(capture_error("reading a camera's video stream"))?;
    let stream = stream
        .ok_or_else(|| CaptureError::Os("the camera exposes no video stream".to_owned()))?;
    // SAFETY: the stream descriptor is live.
    let handler = unsafe { stream.GetMediaTypeHandler() }
        .map_err(capture_error("reading a camera's media types"))?;
    // SAFETY: as above.
    let count = unsafe { handler.GetMediaTypeCount() }
        .map_err(capture_error("counting a camera's media types"))?;

    let mut rows: Vec<ResolutionRow> = Vec::new();
    for index in 0..count.min(MAX_MEDIA_TYPES) {
        // SAFETY: the handler is live and `index` is within the counted range.
        let Ok(media_type) = (unsafe { handler.GetMediaTypeByIndex(index) }) else {
            break;
        };
        let Some((width, height)) = attribute_size(&media_type, &MF_MT_FRAME_SIZE) else {
            continue;
        };
        if width == 0 || height == 0 {
            continue;
        }
        let frame_rate = attribute_ratio(&media_type, &MF_MT_FRAME_RATE)
            .and_then(|(numerator, denominator)| (denominator != 0).then(|| numerator / denominator))
            .filter(|fps| *fps > 0);

        let index = match rows
            .iter()
            .position(|row| row.width == width && row.height == height)
        {
            Some(index) => index,
            None => {
                rows.push(ResolutionRow {
                    width,
                    height,
                    frame_rates: Vec::new(),
                });
                rows.len() - 1
            }
        };
        if let Some(fps) = frame_rate {
            let frame_rates = &mut rows[index].frame_rates;
            if !frame_rates.contains(&fps) {
                frame_rates.push(fps);
            }
        }
    }

    rows.sort_by(|left, right| {
        let (left_area, right_area) = (
            u64::from(left.width) * u64::from(left.height),
            u64::from(right.width) * u64::from(right.height),
        );
        right_area.cmp(&left_area).then(right.width.cmp(&left.width))
    });
    Ok(rows
        .into_iter()
        .map(|mut row| {
            row.frame_rates.sort_unstable_by(|left, right| right.cmp(left));
            (format!("{}x{}", row.width, row.height), row.frame_rates)
        })
        .collect())
}

/// One resolution of a device, with the frame rates announced for it.
struct ResolutionRow {
    width: u32,
    height: u32,
    frame_rates: Vec<u32>,
}

/// The camera format to capture in: exactly the requested size, and the announced frame rate
/// closest to the request.
///
/// The size is not negotiated: a camera that does not announce it is refused rather than
/// asked to scale, because the request comes from the presets this backend itself enumerated
/// and a mismatch therefore means the camera is not the one that was enumerated. That is
/// [`CaptureError::UnsupportedParameters`]; the frame rate is approximate by nature, so the
/// closest announced one wins and the surplus frames are dropped during capture.
fn select_native_type(
    reader: &IMFSourceReader,
    cfg: &CaptureConfig,
) -> Result<(IMFMediaType, u32), CaptureError> {
    let mut best: Option<(IMFMediaType, u32, u32)> = None;
    for index in 0..MAX_MEDIA_TYPES {
        // SAFETY: the reader is live; the end of the list is reported as an error.
        let Ok(media_type) = (unsafe { reader.GetNativeMediaType(VIDEO_STREAM, index) }) else {
            break;
        };
        let Some((width, height)) = attribute_size(&media_type, &MF_MT_FRAME_SIZE) else {
            continue;
        };
        if width != cfg.width || height != cfg.height {
            continue;
        }
        let Some((numerator, denominator)) = attribute_ratio(&media_type, &MF_MT_FRAME_RATE) else {
            continue;
        };
        if denominator == 0 {
            continue;
        }
        let fps = numerator / denominator;
        if fps == 0 {
            continue;
        }

        // Closest to the request, preferring the higher rate on a tie.
        let distance = fps.abs_diff(cfg.fps);
        let better = match &best {
            Some((_, best_fps, best_distance)) => {
                distance < *best_distance || (distance == *best_distance && fps > *best_fps)
            }
            None => true,
        };
        if better {
            best = Some((media_type, fps, distance));
        }
    }

    best.map(|(media_type, fps, _)| (media_type, fps))
        .ok_or(CaptureError::UnsupportedParameters)
}

/// Finds an encoder MFT for `codec` that takes `input_subtype` frames.
///
/// Hardware transforms come first — NVENC, Quick Sync and AMF all register as hardware — and
/// the inbox software H.264 encoder is the loud fallback, which will not hold a 4K stream.
fn find_transform(
    codec: Codec,
    input_subtype: &GUID,
    output_subtype: &GUID,
) -> Result<IMFTransform, CaptureError> {
    let input = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: *input_subtype,
    };
    let output = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: *output_subtype,
    };

    if let Some(activation) = first_activation(MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER, &input, &output)? {
        // SAFETY: the activation is live and the transform is used on this thread only.
        return unsafe { activation.ActivateObject() }
            .map_err(capture_error("starting a hardware encoder"));
    }

    warn!(
        ?codec,
        "no hardware encoder is registered for this codec; falling back to a software transform, which will not keep up with a large camera"
    );
    let flags = MFT_ENUM_FLAG_SORTANDFILTER
        | MFT_ENUM_FLAG_SYNCMFT
        | MFT_ENUM_FLAG_ASYNCMFT
        | MFT_ENUM_FLAG_LOCALMFT;
    let Some(activation) = first_activation(flags, &input, &output)? else {
        return Err(CaptureError::UnsupportedParameters);
    };
    // SAFETY: as above.
    unsafe { activation.ActivateObject() }.map_err(capture_error("starting a software encoder"))
}

/// The first encoder the platform reports for one input/output pair.
fn first_activation(
    flags: windows::Win32::Media::MediaFoundation::MFT_ENUM_FLAG,
    input: &MFT_REGISTER_TYPE_INFO,
    output: &MFT_REGISTER_TYPE_INFO,
) -> Result<Option<IMFActivate>, CaptureError> {
    let mut activations: *mut Option<IMFActivate> = ptr::null_mut();
    let mut count = 0u32;
    // SAFETY: the out parameters are locals; the call allocates the array, which
    // `take_activations` frees. The type information is read-only for the call.
    unsafe {
        MFTEnumEx(
            MFT_CATEGORY_VIDEO_ENCODER,
            flags,
            Some(input),
            Some(output),
            &mut activations,
            &mut count,
        )
    }
    .map_err(capture_error("looking for an encoder"))?;
    // SAFETY: `MFTEnumEx` returned the array and its length.
    Ok(unsafe { take_activations(activations, count) }.into_iter().next())
}

/// One uncompressed video media type.
///
/// `stride` is in bytes per row and negative for a bottom-up frame; the attribute is a
/// signed value stored in a `u32`.
fn build_frame_type(
    format: PixelFormat,
    width: u32,
    height: u32,
    fps: u32,
    stride: Option<i32>,
) -> Result<IMFMediaType, CaptureError> {
    let media_type = create_media_type()?;
    set_video_type(&media_type, format.subtype(), width, height, fps)?;
    if let Some(stride) = stride {
        // SAFETY: the media type is live and the key is one of Media Foundation's.
        unsafe { media_type.SetUINT32(&MF_MT_DEFAULT_STRIDE, stride as u32) }
            .map_err(capture_error("setting the frame stride"))?;
    }
    Ok(media_type)
}

/// One compressed video media type, the encoder's output.
fn build_output_type(
    subtype: GUID,
    width: u32,
    height: u32,
    fps: u32,
    bitrate_kbps: u32,
) -> Result<IMFMediaType, CaptureError> {
    let media_type = create_media_type()?;
    set_video_type(&media_type, subtype, width, height, fps)?;
    // SAFETY: the media type is live and the key is one of Media Foundation's. The protocol
    // carries the budget in kilobits per second, Media Foundation in bits per second.
    unsafe {
        media_type.SetUINT32(&MF_MT_AVG_BITRATE, bitrate_kbps.saturating_mul(1000))
    }
    .map_err(capture_error("setting the encoder bitrate"))?;
    Ok(media_type)
}

/// Fills in the attributes every video media type carries.
fn set_video_type(
    media_type: &IMFMediaType,
    subtype: GUID,
    width: u32,
    height: u32,
    fps: u32,
) -> Result<(), CaptureError> {
    // SAFETY: the media type is live and every key below is one of Media Foundation's.
    // `MF_MT_FRAME_SIZE`, the frame rate and the pixel aspect ratio are 64-bit ratios of two
    // 32-bit halves, most significant first.
    unsafe {
        media_type
            .SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
            .map_err(capture_error("setting MF_MT_MAJOR_TYPE"))?;
        media_type
            .SetGUID(&MF_MT_SUBTYPE, &subtype)
            .map_err(capture_error("setting MF_MT_SUBTYPE"))?;
        media_type
            .SetUINT64(
                &MF_MT_FRAME_SIZE,
                (u64::from(width) << 32) | u64::from(height),
            )
            .map_err(capture_error("setting MF_MT_FRAME_SIZE"))?;
        media_type
            .SetUINT64(&MF_MT_FRAME_RATE, (u64::from(fps) << 32) | 1)
            .map_err(capture_error("setting MF_MT_FRAME_RATE"))?;
        media_type
            .SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, (1_u64 << 32) | 1)
            .map_err(capture_error("setting MF_MT_PIXEL_ASPECT_RATIO"))?;
        media_type
            .SetUINT32(
                &MF_MT_INTERLACE_MODE,
                MFVideoInterlace_Progressive.0 as u32,
            )
            .map_err(capture_error("setting MF_MT_INTERLACE_MODE"))?;
    }
    Ok(())
}

/// An empty attribute store.
fn create_attributes(initial_size: u32) -> Result<IMFAttributes, CaptureError> {
    let mut attributes = None;
    // SAFETY: the out parameter is a valid local.
    unsafe { MFCreateAttributes(&mut attributes, initial_size) }
        .map_err(capture_error("creating an attribute store"))?;
    attributes.ok_or_else(|| CaptureError::Os("Media Foundation returned no attribute store".to_owned()))
}

/// An empty uncompressed video media type.
fn create_media_type() -> Result<IMFMediaType, CaptureError> {
    // SAFETY: Media Foundation's own factory function.
    unsafe { MFCreateMediaType() }.map_err(capture_error("creating a media type"))
}

/// The sample an encoder without `MFT_OUTPUT_STREAM_PROVIDES_SAMPLES` writes into.
fn create_output_sample(size: u32) -> Result<IMFSample, CaptureError> {
    // A zero size means the stream has not settled; one byte keeps the MFT writing and the
    // frame is dropped for being empty.
    // SAFETY: Media Foundation's own factory functions.
    let buffer = unsafe { MFCreateMemoryBuffer(size.max(1)) }
        .map_err(capture_error("allocating an encoder output buffer"))?;
    let sample = unsafe { MFCreateSample() }.map_err(capture_error("allocating an encoder output sample"))?;
    // SAFETY: both objects are live.
    unsafe { sample.AddBuffer(&buffer) }.map_err(capture_error("attaching an encoder output buffer"))?;
    Ok(sample)
}

/// Copies one sample's bytes into `out`, reusing its allocation.
fn copy_sample_bytes(sample: &IMFSample, out: &mut Vec<u8>) -> Result<(), CaptureError> {
    // SAFETY: the sample is live; the contiguous buffer is borrowed for the call and
    // released with the local binding.
    let buffer = unsafe { sample.ConvertToContiguousBuffer() }
        .map_err(capture_error("reading an encoded frame"))?;
    let mut data = ptr::null_mut();
    let mut length = 0u32;
    // SAFETY: the buffer is live and the out parameters are locals.
    unsafe { buffer.Lock(&mut data, None, Some(&mut length)) }
        .map_err(capture_error("locking a captured frame"))?;
    out.clear();
    if length > 0 {
        // SAFETY: a locked buffer is readable for `length` bytes.
        out.extend_from_slice(unsafe { slice::from_raw_parts(data, length as usize) });
    }
    if let Err(failure) = unsafe { buffer.Unlock() } {
        warn!(code = ?failure.code(), "cannot unlock a captured frame");
    }
    Ok(())
}

/// Copies one sample's bytes out.
fn sample_bytes(sample: &IMFSample) -> Result<Vec<u8>, CaptureError> {
    let mut bytes = Vec::new();
    copy_sample_bytes(sample, &mut bytes)?;
    Ok(bytes)
}

/// Reads a string attribute, or `None` when the device does not publish it.
fn attribute_string(attributes: &IMFAttributes, key: &GUID) -> Option<String> {
    // SAFETY: the attribute store is live and the key is one of Media Foundation's.
    let length = unsafe { attributes.GetStringLength(key) }.ok()?;
    let mut value = vec![0u16; length as usize + 1];
    let mut written = 0u32;
    // SAFETY: the buffer is writable for its whole length.
    unsafe { attributes.GetString(key, &mut value, Some(&mut written)) }.ok()?;
    let end = value.iter().position(|unit| *unit == 0).unwrap_or(value.len());
    Some(String::from_utf16_lossy(&value[..end]))
}

/// Reads the packed `(width, height)` of `MF_MT_FRAME_SIZE`.
fn attribute_size(attributes: &IMFAttributes, key: &GUID) -> Option<(u32, u32)> {
    // SAFETY: the attribute store is live and the key is one of Media Foundation's.
    let packed = unsafe { attributes.GetUINT64(key) }.ok()?;
    // UNCERTAIN: `MFGetAttributeSize` is the documented reader and is not bound by the
    // `windows` crate; the attribute is a packed `UINT64` and is read the same way.
    Some(((packed >> 32) as u32, packed as u32))
}

/// Reads a packed `(numerator, denominator)` ratio attribute.
fn attribute_ratio(attributes: &IMFAttributes, key: &GUID) -> Option<(u32, u32)> {
    // SAFETY: the attribute store is live and the key is one of Media Foundation's.
    let packed = unsafe { attributes.GetUINT64(key) }.ok()?;
    Some(((packed >> 32) as u32, packed as u32))
}

/// Reads one `u32` attribute.
fn attribute_u32(attributes: &IMFAttributes, key: &GUID) -> Option<u32> {
    // SAFETY: the attribute store is live and the key is one of Media Foundation's.
    unsafe { attributes.GetUINT32(key) }.ok()
}

/// Reads one blob attribute.
fn attribute_blob(attributes: &IMFAttributes, key: &GUID) -> Option<Vec<u8>> {
    // SAFETY: the attribute store is live and the key is one of Media Foundation's.
    let size = unsafe { attributes.GetBlobSize(key) }.ok()?;
    if size == 0 {
        return None;
    }
    let mut blob = vec![0u8; size as usize];
    let mut written = 0u32;
    // SAFETY: the buffer is writable for its whole length.
    unsafe { attributes.GetBlob(key, &mut blob, Some(&mut written)) }.ok()?;
    blob.truncate(written as usize);
    Some(blob)
}

/// Sets one `ICodecAPI` property, reporting the ones the encoder does not implement.
///
/// Encoders are free to ignore properties they do not support; a failure here means the
/// encoder keeps its own default, not that capture fails.
fn set_codec_value(api: &ICodecAPI, key: &GUID, value: &VARIANT) {
    // SAFETY: the interface is live and the variant holds the type the key expects.
    if let Err(failure) = unsafe { api.SetValue(key, value) } {
        warn!(key = ?key, code = ?failure.code(), "the encoder rejected a codec property");
    }
}

/// An `ICodecAPI` value holding an unsigned 32-bit integer.
///
/// The variant is assembled field by field: the `windows` crate models `VARIANT` as the raw
/// union without the constructors the `windows-core` variant has.
fn variant_u32(value: u32) -> VARIANT {
    variant(VT_UI4, VARIANT_0_0_0 { ulVal: value })
}

/// An `ICodecAPI` value holding a boolean.
fn variant_bool(value: bool) -> VARIANT {
    variant(
        VT_BOOL,
        VARIANT_0_0_0 {
            boolVal: if value { VARIANT_TRUE } else { Default::default() },
        },
    )
}

/// Wraps one value in the variant tag that names it.
fn variant(tag: VARENUM, value: VARIANT_0_0_0) -> VARIANT {
    VARIANT {
        Anonymous: VARIANT_0 {
            Anonymous: ManuallyDrop::new(VARIANT_0_0 {
                vt: tag,
                Anonymous: value,
                ..Default::default()
            }),
        },
    }
}

/// Turns a Windows failure into a [`CaptureError`], logging the detail.
///
/// The variants are coarse on purpose — the core only shows the message — so the platform's
/// own code and text go to the log, except for the two cases the core reacts to.
fn mf_error(context: &str, failure: &windows::core::Error) -> CaptureError {
    let code = failure.code();
    if code == E_ACCESSDENIED {
        // The privacy setting denies the camera to this process.
        return CaptureError::PermissionDenied;
    }
    if code == MF_E_INVALIDMEDIATYPE || code == MF_E_INVALIDTYPE {
        return CaptureError::UnsupportedParameters;
    }
    error!(code = ?code, detail = %failure.message(), "{context}");
    CaptureError::Os(format!("{context}: {}", failure.message()))
}

/// Partial application of [`mf_error`], for the `map_err` calls above.
fn capture_error(context: &'static str) -> impl FnOnce(windows::core::Error) -> CaptureError {
    move |failure| mf_error(context, &failure)
}

/// The COM apartment of one thread.
///
/// Media Foundation needs COM; the pump thread and the enumerating thread each take a
/// multithreaded apartment of their own. A thread that is already in an apartment — a
/// caller that initialized COM as a single-threaded apartment — keeps it, which is why the
/// guard remembers whether it has to uninitialize.
struct ComApartment {
    owned: bool,
}

impl ComApartment {
    /// Joins the current thread to a multithreaded apartment.
    fn init() -> Result<Self, CaptureError> {
        // SAFETY: called on the thread whose apartment this is.
        let outcome = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        if outcome.is_ok() {
            // Any success counts as owned: `S_FALSE` means this thread was already in the
            // multithreaded apartment and still requires its own `CoUninitialize`.
            return Ok(Self { owned: true });
        }
        if outcome == RPC_E_CHANGED_MODE {
            // The thread is already in an apartment; Media Foundation works from any, but the
            // apartment belongs to the caller and must not be closed here.
            // UNCERTAIN: a single-threaded apartment has to pump messages for the callbacks
            // Media Foundation posts; a caller that did this is expected to already do so.
            debug!("COM is already initialized on this thread");
            return Ok(Self { owned: false });
        }
        Err(mf_error(
            "initializing COM",
            &windows::core::Error::from_hresult(outcome),
        ))
    }
}

impl Drop for ComApartment {
    fn drop(&mut self) {
        if self.owned {
            // SAFETY: this thread initialized the apartment and nothing else uses it now.
            unsafe { CoUninitialize() };
        }
    }
}

/// The Media Foundation platform, started for as long as the guard lives.
struct MfRuntime;

impl MfRuntime {
    /// Brings Media Foundation up on the calling thread.
    fn start() -> Result<Self, CaptureError> {
        // SAFETY: `MFStartup` may be called repeatedly; this is the documented entry point.
        // Sockets are not needed, only cameras and encoders.
        unsafe { MFStartup(MF_VERSION, MFSTARTUP_NOSOCKET) }
            .map_err(capture_error("starting Media Foundation"))?;
        Ok(Self)
    }
}

impl Drop for MfRuntime {
    fn drop(&mut self) {
        // SAFETY: matches the `MFStartup` above; every Media Foundation object this thread
        // created has been released by now, in the order the guards are declared.
        if let Err(failure) = unsafe { MFShutdown() } {
            warn!(code = ?failure.code(), "cannot shut Media Foundation down");
        }
    }
}
