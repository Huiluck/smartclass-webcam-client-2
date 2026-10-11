//! V4L2 capture: device discovery, format negotiation and mmap streaming.
//!
//! The kernel ABI is declared here by hand — structs, `FourCC`s and the ioctl numbers —
//! so that the crate compiles on a host without `linux/videodev2.h`. The ioctl numbers are
//! *computed* from the struct layouts with the kernel's `_IOC` encoding and then checked at
//! compile time against the values the kernel documents, which turns a layout slip into a
//! build failure instead of a runtime `EINVAL`.
//!
//! Only the single-planar capture API is used (`V4L2_BUF_TYPE_VIDEO_CAPTURE`); the
//! multiplanar variants (`NV12M`, `YUV420M`, …) are ignored, and so are
//! `V4L2_PIX_FMT_YUV420` in favour of `NV12`, since that is what the encoder uploads best.

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::path::{Path, PathBuf};
use std::ptr;

use crate::error::CaptureError;

/// The `_IOC` direction bit for a read (kernel → user) argument.
const IOC_READ: u32 = 2;
/// The `_IOC` direction bit for a write (user → kernel) argument.
const IOC_WRITE: u32 = 1;
/// The ioctl type byte of the video device API (`'V'`).
const IOC_KIND: u8 = b'V';

/// Encodes one ioctl request number, mirroring `<linux/ioctl.h>`.
const fn ioc(direction: u32, nr: u8, size: usize) -> libc::c_ulong {
    (((direction as u64) << 30)
        | ((size as u64) << 16)
        | ((IOC_KIND as u64) << 8)
        | (nr as u64)) as libc::c_ulong
}

/// `_IOR(kind, nr, T)`.
const fn ior<T>(nr: u8) -> libc::c_ulong {
    ioc(IOC_READ, nr, size_of::<T>())
}

/// `_IOW(kind, nr, T)`.
const fn iow<T>(nr: u8) -> libc::c_ulong {
    ioc(IOC_WRITE, nr, size_of::<T>())
}

/// `_IOWR(kind, nr, T)`.
const fn iowr<T>(nr: u8) -> libc::c_ulong {
    ioc(IOC_READ | IOC_WRITE, nr, size_of::<T>())
}

const VIDIOC_QUERYCAP: libc::c_ulong = ior::<Capability>(0);
const VIDIOC_ENUM_FMT: libc::c_ulong = iowr::<FmtDesc>(2);
const VIDIOC_G_FMT: libc::c_ulong = iowr::<Format>(4);
const VIDIOC_S_FMT: libc::c_ulong = iowr::<Format>(5);
const VIDIOC_REQBUFS: libc::c_ulong = iowr::<RequestBuffers>(8);
const VIDIOC_QUERYBUF: libc::c_ulong = iowr::<Buffer>(9);
const VIDIOC_QBUF: libc::c_ulong = iowr::<Buffer>(15);
const VIDIOC_DQBUF: libc::c_ulong = iowr::<Buffer>(17);
const VIDIOC_STREAMON: libc::c_ulong = iow::<libc::c_int>(18);
const VIDIOC_STREAMOFF: libc::c_ulong = iow::<libc::c_int>(19);
const VIDIOC_G_PARM: libc::c_ulong = iowr::<StreamParm>(21);
const VIDIOC_S_PARM: libc::c_ulong = iowr::<StreamParm>(22);
const VIDIOC_TRY_FMT: libc::c_ulong = iowr::<Format>(64);
const VIDIOC_ENUM_FRAMESIZES: libc::c_ulong = iowr::<FrameSizeEnum>(74);
const VIDIOC_ENUM_FRAMEINTERVALS: libc::c_ulong = iowr::<FrameIntervalEnum>(75);

/// `v4l2_fourcc(a, b, c, d)`.
const fn fourcc(a: u8, b: u8, c: u8, d: u8) -> u32 {
    (a as u32) | ((b as u32) << 8) | ((c as u32) << 16) | ((d as u32) << 24)
}

/// One of the pixel formats this backend knows how to turn into an encoder input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PixelFormat {
    /// Planar Y/CbCr 4:2:0, uploadable as-is (after stride normalisation).
    Nv12,
    /// Packed Y/U/Y/V 4:2:2.
    Yuyv,
    /// Motion JPEG, decoded in software before encoding.
    Mjpeg,
}

impl PixelFormat {
    /// Every format the backend can consume, preferred first.
    ///
    /// `NV12` comes first because the encoder takes it without a colour conversion; `YUYV`
    /// needs a chroma downsample, `MJPEG` a full decode plus conversion. A 4K60 MJPEG-only
    /// camera therefore burns real CPU before the encoder sees a frame, which is the price
    /// of the format being the only one USB can carry at that rate.
    pub(crate) const PREFERENCE: [Self; 3] = [Self::Nv12, Self::Yuyv, Self::Mjpeg];

    /// The `V4L2_PIX_FMT_*` code of this format.
    pub(crate) const fn fourcc(self) -> u32 {
        match self {
            // 'NV12'
            Self::Nv12 => fourcc(b'N', b'V', b'1', b'2'),
            // 'YUYV'
            Self::Yuyv => fourcc(b'Y', b'U', b'Y', b'V'),
            // 'MJPG'
            Self::Mjpeg => fourcc(b'M', b'J', b'P', b'G'),
        }
    }

    /// Whether a frame of this format is a JPEG bitstream rather than raw pixels.
    pub(crate) const fn is_compressed(self) -> bool {
        matches!(self, Self::Mjpeg)
    }

    /// The number of bytes one row of this format occupies at `width` pixels.
    ///
    /// Only used when a driver reports no `bytesperline`; drivers that support capture
    /// streaming always fill the field in.
    pub(crate) const fn row_bytes(self, width: u32) -> u32 {
        match self {
            Self::Nv12 => width,
            Self::Yuyv => width * 2,
            Self::Mjpeg => 0,
        }
    }
}

/// `V4L2_CAP_VIDEO_CAPTURE` / `V4L2_CAP_DEVICE_CAPS` / `V4L2_CAP_STREAMING`.
const CAP_VIDEO_CAPTURE: u32 = 0x0000_0001;
const CAP_STREAMING: u32 = 0x0400_0000;
const CAP_DEVICE_CAPS: u32 = 0x8000_0000;

/// `V4L2_BUF_TYPE_VIDEO_CAPTURE` / `V4L2_MEMORY_MMAP`.
const BUF_TYPE_VIDEO_CAPTURE: u32 = 1;
const MEMORY_MMAP: u32 = 1;

/// `V4L2_FIELD_NONE` — this backend only handles progressive frames.
const FIELD_NONE: u32 = 1;

/// `V4L2_FRMSIZE_TYPE_DISCRETE` / `V4L2_FRMSIZE_TYPE_STEPWISE`(and `_CONTINUOUS`, 2).
const FRMSIZE_DISCRETE: u32 = 1;

/// `V4L2_BUF_FLAG_ERROR` — the driver discarded this frame.
const BUF_FLAG_ERROR: u32 = 0x0000_0040;

/// How many driver buffers to ask for.
///
/// Four is the usual minimum that keeps a USB camera streaming while the process is busy
/// with the previous frame; the capture thread never holds more than one.
const BUFFER_COUNT: u32 = 4;

/// The largest number of buffers the driver may hand back for a `REQBUFS` request.
const MAX_BUFFER_COUNT: usize = 32;

/// A frame rate the device announces for one resolution.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Rate {
    /// Numerator of the frame interval, in seconds.
    pub(crate) numerator: u32,
    /// Denominator of the frame interval, in seconds.
    pub(crate) denominator: u32,
}

impl Rate {
    /// Frames per second, rounded to the nearest whole frame.
    ///
    /// A `0/0` interval (a driver that does not fill the field in) reports zero, which
    /// callers treat as "unknown" rather than as a usable rate.
    pub(crate) fn fps(self) -> u32 {
        if self.numerator == 0 || self.denominator == 0 {
            return 0;
        }
        let fps = self.denominator as u64 * 2 / self.numerator as u64;
        ((fps + 1) / 2).min(u32::MAX as u64) as u32
    }
}

/// One camera found on `/dev`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Probed {
    /// The identity callers store in [`CameraInfo::id`](crate::CameraInfo::id).
    pub(crate) id: String,
    /// Human-readable name, from `VIDIOC_QUERYCAP`'s `card`, falling back to the node name.
    pub(crate) name: String,
    /// `(width, height, frame rates)` triples every consumable format announced.
    pub(crate) formats: Vec<(u32, u32, Vec<u32>)>,
}

/// Opens a video node and asks the driver what it can stream.
///
/// Returns `Ok(None)` for a node that is not a capture device at all (`/dev/video*` also
/// holds metadata and codec nodes), which is not an error: the caller just skips it.
pub(crate) fn probe(path: &Path) -> Result<Option<Probed>, CaptureError> {
    let file = match open_node(path) {
        Ok(file) => file,
        // A node that vanished between the directory scan and this open, or one we may not
        // touch, is skipped rather than reported: enumeration is best-effort by design.
        Err(error) if node_unavailable(&error) => return Ok(None),
        Err(error) => return Err(error),
    };

    let capability = match query_capability(file.as_raw_fd()) {
        Ok(capability) => capability,
        Err(_) => return Ok(None),
    };
    if capability.device_caps() & CAP_VIDEO_CAPTURE == 0 {
        return Ok(None);
    }

    let name = capability.card();
    let id = stable_id(path);
    let formats = probe_formats(file.as_raw_fd(), path, capability.device_caps())?;

    Ok(Some(Probed {
        id,
        name: if name.is_empty() { node_name(path) } else { name },
        formats,
    }))
}

/// Whether an ioctl/`open` failure means "this node is not one we can use".
fn node_unavailable(error: &CaptureError) -> bool {
    match error {
        CaptureError::NoCamera => true,
        CaptureError::Os(message) => message.contains("not a video capture device"),
        _ => false,
    }
}

/// Collects the `(resolution, frame rates)` matrix of one device.
///
/// The matrix is built from format descriptors the driver *announces*; it is a claim, not a
/// promise (see [`CameraInfo::presets`](crate::CameraInfo::presets)). Drivers that cannot
/// enumerate frame sizes at all (the ioctl is optional) contribute the size they are
/// currently configured for with no frame rates attached.
fn probe_formats(
    fd: RawFd,
    _path: &Path,
    device_caps: u32,
) -> Result<Vec<(u32, u32, Vec<u32>)>, CaptureError> {
    let mut matrix: Vec<(u32, u32, Vec<u32>)> = Vec::new();

    for format in PixelFormat::PREFERENCE {
        let mut sizes = enumerate_sizes(fd, format);
        if sizes.is_empty() {
            // `VIDIOC_ENUM_FRAMESIZES` is optional; fall back to the current format so a
            // working camera is never listed without any resolution at all.
            if let Some(current) = current_size(fd, format) {
                sizes.push(current);
            } else {
                tracing::debug!(
                    format = ?format,
                    supported = device_caps & CAP_STREAMING != 0,
                    "camera announces no frame sizes for a format this backend can consume"
                );
                continue;
            }
        }

        for (width, height) in sizes {
            let rates = enumerate_rates(fd, format, width, height)
                .into_iter()
                .map(Rate::fps)
                .filter(|fps| *fps > 0)
                .collect::<Vec<_>>();
            merge_size(&mut matrix, width, height, rates);
        }
    }

    // Highest resolution first, then widest, then the fastest frame rate: the order an
    // operator scanning the list expects.
    matrix.sort_by(|left, right| {
        let left_pixels = u64::from(left.0) * u64::from(left.1);
        let right_pixels = u64::from(right.0) * u64::from(right.1);
        right_pixels
            .cmp(&left_pixels)
            .then(right.0.cmp(&left.0))
            .then(right.2.first().cmp(&left.2.first()))
    });
    for entry in &mut matrix {
        entry.2.sort_unstable_by(|left, right| right.cmp(left));
        entry.2.dedup();
    }
    Ok(matrix)
}

/// Adds one announced `(width, height)` to the matrix, merging duplicate resolutions.
fn merge_size(matrix: &mut Vec<(u32, u32, Vec<u32>)>, width: u32, height: u32, rates: Vec<u32>) {
    match matrix
        .iter_mut()
        .find(|(seen_width, seen_height, _)| *seen_width == width && *seen_height == height)
    {
        Some((_, _, seen_rates)) => seen_rates.extend(rates),
        None => matrix.push((width, height, rates)),
    }
}

/// Walks `VIDIOC_ENUM_FRAMESIZES` for one pixel format.
///
/// Stepwise and continuous ranges are reported as their largest size: the protocol's
/// preset matrix is a list of discrete candidates, and the largest size of a range is the
/// one an operator wants (a smaller size can always be asked for, and the launch-time
/// validation pass will confirm what actually streams).
fn enumerate_sizes(fd: RawFd, format: PixelFormat) -> Vec<(u32, u32)> {
    let mut sizes = Vec::new();
    for index in 0..u32::MAX {
        let mut request = FrameSizeEnum {
            index,
            pixel_format: format.fourcc(),
            ..FrameSizeEnum::default()
        };
        if ioctl_mut(fd, VIDIOC_ENUM_FRAMESIZES, &mut request).is_err() {
            break;
        }
        match request.kind {
            FRMSIZE_DISCRETE => {
                // SAFETY: `kind` says the driver filled the discrete member in.
                let discrete = unsafe { request.size.discrete };
                if discrete.width > 0 && discrete.height > 0 {
                    sizes.push((discrete.width, discrete.height));
                }
            }
            _ => {
                // SAFETY: any other type means the stepwise member is the live one.
                let stepwise = unsafe { request.size.stepwise };
                if stepwise.max_width > 0 && stepwise.max_height > 0 {
                    sizes.push((stepwise.max_width, stepwise.max_height));
                    // A range that also lists a distinct minimum is worth reporting, so an
                    // operator can pick the small end without a second enumeration pass.
                    if stepwise.min_width > 0
                        && stepwise.min_height > 0
                        && (stepwise.min_width, stepwise.min_height)
                            != (stepwise.max_width, stepwise.max_height)
                    {
                        sizes.push((stepwise.min_width, stepwise.min_height));
                    }
                }
            }
        }
    }
    sizes
}

/// Reads back the size the device is currently set to for one format, if it is that format.
fn current_size(fd: RawFd, format: PixelFormat) -> Option<(u32, u32)> {
    let request = get_format(fd, format, 0, 0).ok()?;
    // SAFETY: `pix` is the live member after `G_FMT` filled the pix member in.
    let pix = unsafe { request.format.pix };
    (pix.pixelformat == format.fourcc() && pix.width > 0 && pix.height > 0)
        .then_some((pix.width, pix.height))
}

/// Walks `VIDIOC_ENUM_FRAMEINTERVALS` for one pixel format and size.
///
/// Ranges (stepwise/continuous) collapse to the fastest interval, i.e. the highest frame
/// rate the device announces.
fn enumerate_rates(fd: RawFd, format: PixelFormat, width: u32, height: u32) -> Vec<Rate> {
    let mut rates = Vec::new();
    for index in 0..u32::MAX {
        let mut request = FrameIntervalEnum {
            index,
            pixel_format: format.fourcc(),
            width,
            height,
            ..FrameIntervalEnum::default()
        };
        if ioctl_mut(fd, VIDIOC_ENUM_FRAMEINTERVALS, &mut request).is_err() {
            break;
        }
        if request.kind == FRMSIZE_DISCRETE {
            // SAFETY: `kind` says the discrete member is the live one.
            rates.push(rate_of(unsafe { request.interval.discrete }));
        } else {
            // SAFETY: any other type means the stepwise member is the live one.
            let stepwise = unsafe { request.interval.stepwise };
            rates.push(rate_of(stepwise.min));
        }
    }
    rates
}

/// Converts one driver-reported frame interval into a frame rate.
fn rate_of(interval: Fract) -> Rate {
    Rate {
        numerator: interval.numerator,
        denominator: interval.denominator,
    }
}

/// One open, streaming video node.
pub(crate) struct Device {
    file: OwnedFd,
    /// Path the node was opened by, kept for log messages.
    pub(crate) path: PathBuf,
    /// The format the driver accepted.
    pub(crate) format: PixelFormat,
    /// Negotiated picture width in pixels.
    pub(crate) width: u32,
    /// Negotiated picture height in pixels.
    pub(crate) height: u32,
    /// Row pitch of the first plane, straight from `VIDIOC_S_FMT`.
    pub(crate) pitch: u32,
    /// The frame rate the driver accepted, which is not always the one requested.
    pub(crate) fps: u32,
    /// Size of the image the driver expects to write, from `VIDIOC_S_FMT`.
    pub(crate) size_image: u32,
    buffers: Vec<Mapping>,
    streaming: bool,
}

impl Device {
    /// Opens a node and negotiates a streaming format.
    ///
    /// Format preference is [`PixelFormat::PREFERENCE`]; a format is accepted only when the
    /// driver reports back exactly the requested size, and a frame rate at least half the
    /// requested one (cameras commonly offer 30 and 60 fps and nothing in between, so an
    /// exact match would reject working hardware).
    pub(crate) fn open(
        path: &Path,
        width: u32,
        height: u32,
        fps: u32,
    ) -> Result<Self, CaptureError> {
        let file = open_node(path)?;
        let fd = file.as_raw_fd();

        let capability = query_capability(fd).map_err(|error| match error {
            CaptureError::NoCamera => CaptureError::NoCamera,
            other => other,
        })?;
        if capability.device_caps() & CAP_VIDEO_CAPTURE == 0 {
            return Err(CaptureError::NoCamera);
        }
        if capability.device_caps() & CAP_STREAMING == 0 {
            return Err(CaptureError::UnsupportedParameters);
        }

        let mut accepted = None;
        for format in PixelFormat::PREFERENCE {
            // A candidate the driver refuses outright must not end the search for one it
            // accepts: it gets adjusted (`continue` below) or rejected, and the next
            // preference is tried either way.
            let candidate = match set_format(fd, format, width, height) {
                Ok(candidate) => candidate,
                Err(error) => {
                    tracing::debug!(?format, %error, "format rejected");
                    continue;
                }
            };
            // SAFETY: `S_FMT` filled the pix member in.
            let pix = unsafe { candidate.format.pix };
            if pix.width != width || pix.height != height || pix.pixelformat != format.fourcc() {
                tracing::debug!(
                    ?format,
                    width = pix.width,
                    height = pix.height,
                    "driver adjusted the requested format"
                );
                continue;
            }

            let pitch = if pix.bytesperline != 0 {
                pix.bytesperline
            } else {
                format.row_bytes(pix.width)
            };
            let negotiated = set_rate(fd, fps).unwrap_or_else(|| fps);
            if negotiated * 2 < fps {
                tracing::debug!(
                    ?format,
                    negotiated,
                    requested = fps,
                    "driver cannot sustain the requested frame rate"
                );
                continue;
            }

            accepted = Some((format, pix, pitch, negotiated));
            break;
        }

        let Some((format, pix, pitch, fps)) = accepted else {
            return Err(CaptureError::UnsupportedParameters);
        };

        let mut device = Self {
            file,
            path: path.to_path_buf(),
            format,
            width: pix.width,
            height: pix.height,
            pitch,
            fps,
            size_image: pix.sizeimage,
            buffers: Vec::new(),
            streaming: false,
        };
        device.map_buffers()?;
        device.stream_on()?;
        tracing::info!(
            path = %device.path.display(),
            format = ?format,
            width = device.width,
            height = device.height,
            fps = device.fps,
            "V4L2 stream started"
        );
        Ok(device)
    }

    /// Asks for `mmap` buffers and maps them into this process.
    fn map_buffers(&mut self) -> Result<(), CaptureError> {
        let fd = self.file.as_raw_fd();
        let mut request = RequestBuffers {
            count: BUFFER_COUNT,
            kind: BUF_TYPE_VIDEO_CAPTURE,
            memory: MEMORY_MMAP,
            ..RequestBuffers::default()
        };
        ioctl_mut(fd, VIDIOC_REQBUFS, &mut request)
            .map_err(|error| backend("VIDIOC_REQBUFS", &error))?;
        if request.count == 0 {
            return Err(CaptureError::UnsupportedParameters);
        }
        let count = (request.count as usize).min(MAX_BUFFER_COUNT);

        let mut mappings = Vec::with_capacity(count);
        for index in 0..count as u32 {
            let mut buffer = Buffer {
                index,
                kind: BUF_TYPE_VIDEO_CAPTURE,
                memory: MEMORY_MMAP,
                ..Buffer::default()
            };
            ioctl_mut(fd, VIDIOC_QUERYBUF, &mut buffer)
                .map_err(|error| backend("VIDIOC_QUERYBUF", &error))?;
            if buffer.length == 0 {
                return Err(backend(
                    "VIDIOC_QUERYBUF",
                    &io::Error::other("driver reported a zero-length buffer"),
                ));
            }
            // SAFETY: the offset is the driver's own cookie for this node, and the length
            // is what the driver reported for the buffer.
            let mapping = unsafe { Mapping::map(fd, buffer.offset(), buffer.length as usize) }
                .map_err(|error| backend("mmap", &error))?;
            mappings.push(mapping);

            // Queue every buffer so the driver has somewhere to put the first frames.
            let mut queue = Buffer {
                index,
                kind: BUF_TYPE_VIDEO_CAPTURE,
                memory: MEMORY_MMAP,
                ..Buffer::default()
            };
            ioctl_mut(fd, VIDIOC_QBUF, &mut queue)
                .map_err(|error| backend("VIDIOC_QBUF", &error))?;
        }
        self.buffers = mappings;
        Ok(())
    }

    /// Starts the stream.
    fn stream_on(&mut self) -> Result<(), CaptureError> {
        let fd = self.file.as_raw_fd();
        let mut kind = BUF_TYPE_VIDEO_CAPTURE as libc::c_int;
        ioctl_ref(fd, VIDIOC_STREAMON, &mut kind)
            .map_err(|error| backend("VIDIOC_STREAMON", &error))?;
        self.streaming = true;
        Ok(())
    }

    /// Stops the stream and unmaps every buffer. Idempotent.
    pub(crate) fn stop(&mut self) {
        if self.streaming {
            let mut kind = BUF_TYPE_VIDEO_CAPTURE as libc::c_int;
            if let Err(error) = ioctl_ref(self.file.as_raw_fd(), VIDIOC_STREAMOFF, &mut kind) {
                tracing::warn!(%error, "VIDIOC_STREAMOFF failed");
            }
            self.streaming = false;
        }
        self.buffers.clear();
    }

    /// The node's file descriptor, for `poll`.
    pub(crate) fn raw_fd(&self) -> RawFd {
        self.file.as_raw_fd()
    }

    /// Waits until a frame is ready or `stop_fd` becomes readable.
    ///
    /// Returns `Ok(false)` when the caller's stop pipe fired first, `Ok(true)` when the
    /// wait ended with the device readable or the timeout expired — the caller must call
    /// [`Device::dequeue`] either way, which reports `Ok(None)` when there is nothing yet.
    pub(crate) fn wait_readable(
        &self,
        stop_fd: RawFd,
        timeout: libc::c_int,
    ) -> Result<bool, CaptureError> {
        let mut fds = [
            libc::pollfd {
                fd: self.raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: stop_fd,
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        // SAFETY: `fds` is a valid array of two descriptors for the duration of the call.
        let ready = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, timeout) };
        if ready < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                return Ok(true);
            }
            return Err(backend("poll", &error));
        }
        if fds[1].revents & libc::POLLIN != 0 {
            return Ok(false);
        }
        Ok(true)
    }

    /// Takes the next filled buffer off the driver's queue.
    ///
    /// `Ok(None)` means the buffer queue is momentarily empty (`EAGAIN`) or the driver
    /// discarded the frame (`V4L2_BUF_FLAG_ERROR`); either way the caller retries.
    pub(crate) fn dequeue(&self) -> Result<Option<Dequeued>, CaptureError> {
        let mut buffer = Buffer {
            kind: BUF_TYPE_VIDEO_CAPTURE,
            memory: MEMORY_MMAP,
            ..Buffer::default()
        };
        match ioctl_mut(self.raw_fd(), VIDIOC_DQBUF, &mut buffer) {
            Ok(()) => {}
            Err(error) if error.raw_os_error() == Some(libc::EAGAIN) => return Ok(None),
            Err(error) => return Err(backend("VIDIOC_DQBUF", &error)),
        }
        if buffer.flags & BUF_FLAG_ERROR != 0 {
            tracing::warn!(sequence = buffer.sequence, "driver discarded a corrupted frame");
            self.requeue(buffer.index)?;
            return Ok(None);
        }
        Ok(Some(Dequeued {
            index: buffer.index,
            used: (buffer.bytesused as usize).min(self.buffer_len(buffer.index)),
            sequence: buffer.sequence,
        }))
    }

    /// Hands a buffer back to the driver.
    pub(crate) fn requeue(&self, index: u32) -> Result<(), CaptureError> {
        let mut buffer = Buffer {
            index,
            kind: BUF_TYPE_VIDEO_CAPTURE,
            memory: MEMORY_MMAP,
            ..Buffer::default()
        };
        ioctl_mut(self.raw_fd(), VIDIOC_QBUF, &mut buffer)
            .map_err(|error| backend("VIDIOC_QBUF", &error))
    }

    /// The mapped contents of one buffer.
    pub(crate) fn buffer(&self, index: u32) -> &[u8] {
        self.buffers
            .get(index as usize)
            .map(Mapping::as_slice)
            .unwrap_or(&[])
    }

    /// Length of one mapped buffer in bytes.
    fn buffer_len(&self, index: u32) -> usize {
        self.buffers
            .get(index as usize)
            .map_or(0, |mapping| mapping.as_slice().len())
    }
}

impl Drop for Device {
    fn drop(&mut self) {
        self.stop();
    }
}

/// One frame taken off the driver's queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Dequeued {
    /// Index of the driver buffer holding the frame.
    pub(crate) index: u32,
    /// How many bytes of that buffer the driver filled.
    pub(crate) used: usize,
    /// Driver sequence number, useful in logs.
    pub(crate) sequence: u32,
}

/// One `mmap`ed driver buffer.
struct Mapping {
    address: ptr::NonNull<u8>,
    len: usize,
}

// SAFETY: the mapping is process-wide memory. Moving the handle between threads is what
// `mmap` supports; the capture thread is the only one that ever reads or writes it.
unsafe impl Send for Mapping {}

impl Mapping {
    /// Maps one driver buffer.
    ///
    /// # Safety
    ///
    /// `fd` must be an open video node and `offset`/`len` must describe a buffer the driver
    /// reported through `VIDIOC_QUERYBUF`.
    unsafe fn map(fd: RawFd, offset: u32, len: usize) -> io::Result<Self> {
        // SAFETY: the caller guarantees the descriptor and offset are a valid buffer.
        let address = unsafe {
            libc::mmap(
                ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd,
                libc::off_t::from(offset),
            )
        };
        if address == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        let address = ptr::NonNull::new(address.cast::<u8>()).ok_or_else(|| {
            io::Error::other("mmap returned a null address for a non-empty mapping")
        })?;
        Ok(Self { address, len })
    }

    /// Borrows the mapping as bytes.
    fn as_slice(&self) -> &[u8] {
        // SAFETY: `address` is a live mapping of `len` bytes owned by `self`, and only the
        // capture thread touches it.
        unsafe { std::slice::from_raw_parts(self.address.as_ptr(), self.len) }
    }
}

impl Drop for Mapping {
    fn drop(&mut self) {
        // SAFETY: `address`/`len` are exactly what `mmap` returned for `self`.
        unsafe {
            libc::munmap(self.address.as_ptr().cast::<libc::c_void>(), self.len);
        }
    }
}

/// Opens a video node for read/write, non-blocking.
fn open_node(path: &Path) -> Result<OwnedFd, CaptureError> {
    let c_path = std::ffi::CString::new(path.as_os_str().as_encoded_bytes())
        .map_err(|_| CaptureError::Os(format!("device path {:?} contains a NUL byte", path)))?;
    // SAFETY: `c_path` is a valid NUL-terminated path.
    let fd = unsafe {
        libc::open(
            c_path.as_ptr(),
            libc::O_RDWR | libc::O_NONBLOCK | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        let error = io::Error::last_os_error();
        return Err(match error.raw_os_error() {
            Some(libc::ENOENT) | Some(libc::ENODEV) | Some(libc::ENXIO) => CaptureError::NoCamera,
            Some(libc::EACCES) | Some(libc::EPERM) => CaptureError::PermissionDenied,
            _ => backend("open", &error),
        });
    }
    // SAFETY: `fd` is a fresh descriptor owned by this function.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// `VIDIOC_QUERYCAP`.
fn query_capability(fd: RawFd) -> Result<Capability, CaptureError> {
    let mut capability = Capability::default();
    ioctl_mut(fd, VIDIOC_QUERYCAP, &mut capability)
        .map_err(|error| backend("VIDIOC_QUERYCAP", &error))?;
    Ok(capability)
}

/// Reads the driver's current format without changing it.
fn get_format(
    fd: RawFd,
    format: PixelFormat,
    width: u32,
    height: u32,
) -> Result<Format, CaptureError> {
    let mut request = Format::for_pix(format, width, height);
    ioctl_mut(fd, VIDIOC_G_FMT, &mut request).map_err(|error| backend("VIDIOC_G_FMT", &error))?;
    Ok(request)
}

/// `VIDIOC_S_FMT` with the exact size and format wanted.
fn set_format(
    fd: RawFd,
    format: PixelFormat,
    width: u32,
    height: u32,
) -> Result<Format, CaptureError> {
    let mut request = Format::for_pix(format, width, height);
    ioctl_mut(fd, VIDIOC_S_FMT, &mut request).map_err(|error| backend("VIDIOC_S_FMT", &error))?;
    Ok(request)
}

/// `VIDIOC_TRY_FMT`: asks whether a size/format would be accepted, without committing.
///
/// Not used by [`Device::open`], which lets `S_FMT` negotiate directly; kept for the
/// enumeration path's diagnostics.
#[allow(dead_code)]
fn try_format(
    fd: RawFd,
    format: PixelFormat,
    width: u32,
    height: u32,
) -> Result<Format, CaptureError> {
    let mut request = Format::for_pix(format, width, height);
    ioctl_mut(fd, VIDIOC_TRY_FMT, &mut request).map_err(|error| backend("VIDIOC_TRY_FMT", &error))?;
    Ok(request)
}

/// `VIDIOC_S_PARM` followed by `VIDIOC_G_PARM` to learn the rate the driver settled on.
///
/// Returns `None` when the driver refuses to set a rate at all; the caller then keeps the
/// requested one, because a camera that streams at its own fixed rate is not a failure.
fn set_rate(fd: RawFd, fps: u32) -> Option<u32> {
    let mut request = StreamParm {
        kind: BUF_TYPE_VIDEO_CAPTURE,
        ..StreamParm::default()
    };
    // `S_PARM`/`G_PARM` only touch the `capture` member for a capture device, and the frame
    // interval is a plain struct rather than a union member, so no `unsafe` is involved.
    request.parm.capture.timeperframe.numerator = 1;
    request.parm.capture.timeperframe.denominator = fps;
    if ioctl_mut(fd, VIDIOC_S_PARM, &mut request).is_err() {
        return None;
    }
    let mut readback = StreamParm {
        kind: BUF_TYPE_VIDEO_CAPTURE,
        ..StreamParm::default()
    };
    if ioctl_mut(fd, VIDIOC_G_PARM, &mut readback).is_err() {
        return None;
    }
    // SAFETY: see above.
    let interval = unsafe { readback.parm.capture.timeperframe };
    let negotiated = Rate {
        numerator: interval.numerator,
        denominator: interval.denominator,
    }
    .fps();
    (negotiated > 0).then_some(negotiated)
}

/// Runs one ioctl whose argument is a mutable struct pointer.
fn ioctl_mut<T>(fd: RawFd, request: libc::c_ulong, argument: &mut T) -> io::Result<()> {
    // SAFETY: `argument` is a valid pointer to a `T` for the duration of the call, which is
    // what every `_IOW(R)` request declared above expects.
    let result = unsafe {
        libc::ioctl(
            fd,
            request as _,
            ptr::from_mut(argument).cast::<libc::c_void>(),
        )
    };
    if result < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Runs one ioctl whose argument is a plain integer.
fn ioctl_ref(fd: RawFd, request: libc::c_ulong, argument: &mut libc::c_int) -> io::Result<()> {
    // SAFETY: `argument` is a valid pointer to a `c_int`, which is what `_IOW` expects for
    // `VIDIOC_STREAMON`/`VIDIOC_STREAMOFF`.
    let result = unsafe { libc::ioctl(fd, request as _, argument as *mut libc::c_int) };
    if result < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Wraps an operation name and an OS error into a backend failure.
fn backend(operation: &str, error: &io::Error) -> CaptureError {
    CaptureError::Os(format!("{operation} failed: {error}"))
}

/// The node name of a `/dev/videoN` path.
pub(crate) fn node_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// Every `/dev/video*` node, numerically ordered.
pub(crate) fn video_nodes() -> Vec<PathBuf> {
    let mut nodes: Vec<(u32, PathBuf)> = Vec::new();
    let Ok(entries) = std::fs::read_dir("/dev") else {
        return Vec::new();
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if let Some(index) = name.strip_prefix("video").and_then(|n| n.parse::<u32>().ok()) {
            nodes.push((index, entry.path()));
        }
    }
    nodes.sort_unstable_by_key(|(index, _)| *index);
    nodes.into_iter().map(|(_, path)| path).collect()
}

/// The identity of one video node: the `/dev/v4l/by-id` link that points at it when one
/// exists, otherwise the sysfs device path it belongs to.
///
/// The by-id name is the stable one an operator recognises (`usb-046d_HD_Pro_Webcam...`)
/// and it survives replugging into another port; the sysfs path is only stable per port,
/// which is still better than a bare `videoN` that any newly attached camera can claim.
pub(crate) fn stable_id(path: &Path) -> String {
    if let Some(link) = by_id_link(path) {
        return link;
    }
    sysfs_id(path).unwrap_or_else(|| path.to_string_lossy().into_owned())
}

/// The `/dev/v4l/by-id` entry name for a node, if udev has created one.
fn by_id_link(path: &Path) -> Option<String> {
    let target = std::fs::canonicalize(path).ok()?;
    let entries = std::fs::read_dir("/dev/v4l/by-id").ok()?;
    for entry in entries.flatten() {
        let Ok(resolved) = std::fs::canonicalize(entry.path()) else {
            continue;
        };
        if resolved == target {
            return Some(entry.file_name().to_string_lossy().into_owned());
        }
    }
    None
}

/// The sysfs device directory of a node, e.g. `/sys/devices/pci…/video4linux/video0`.
fn sysfs_id(path: &Path) -> Option<String> {
    let name = path.file_name()?;
    let link = Path::new("/sys/class/video4linux").join(name).join("device");
    let resolved = std::fs::canonicalize(link).ok()?;
    Some(resolved.to_string_lossy().into_owned())
}

/// Resolves the identity in a [`CaptureConfig`](crate::CaptureConfig) back to a device node.
///
/// Accepts, in order: an absolute path, a `/dev/v4l/by-id` link name, a bare `videoN` node
/// name, and finally any camera whose [`stable_id`] matches. `None` picks the first node
/// that reports itself as a capture device.
pub(crate) fn resolve_device(camera_id: Option<&str>) -> Result<PathBuf, CaptureError> {
    let Some(camera_id) = camera_id else {
        for node in video_nodes() {
            if probe(&node)?.is_some() {
                return Ok(node);
            }
        }
        return Err(CaptureError::NoCamera);
    };

    let direct = Path::new(camera_id);
    if direct.is_absolute() {
        return direct
            .exists()
            .then(|| direct.to_path_buf())
            .ok_or(CaptureError::NoCamera);
    }

    let by_id = Path::new("/dev/v4l/by-id").join(camera_id);
    if by_id.exists() {
        return Ok(by_id);
    }

    if camera_id.starts_with("video") {
        let node = Path::new("/dev").join(camera_id);
        if node.exists() {
            return Ok(node);
        }
    }

    for node in video_nodes() {
        if stable_id(&node) == camera_id {
            return Ok(node);
        }
    }
    Err(CaptureError::NoCamera)
}

// ---------------------------------------------------------------------------------------
// The kernel ABI, transcribed from `linux/videodev2.h`.
//
// Field order and types matter: the ioctl numbers above are derived from these layouts, and
// `_IOC_SIZE` is part of the request the kernel matches on, so a wrong size turns into a
// failed `ioctl` rather than a wrong result. The assertions at the end pin every size to
// the value the kernel's own headers produce.
// ---------------------------------------------------------------------------------------

/// `struct v4l2_capability`.
#[repr(C)]
#[derive(Clone, Copy)]
struct Capability {
    driver: [u8; 16],
    card: [u8; 32],
    bus_info: [u8; 32],
    version: u32,
    capabilities: u32,
    device_caps: u32,
    reserved: [u32; 3],
}

impl Default for Capability {
    fn default() -> Self {
        // SAFETY: every field is an integer or a byte array, so an all-zero value is valid.
        unsafe { std::mem::zeroed() }
    }
}

impl Capability {
    /// The capabilities of this node: `device_caps` when the driver separates them from the
    /// whole-device capabilities, `capabilities` otherwise.
    fn device_caps(&self) -> u32 {
        if self.capabilities & CAP_DEVICE_CAPS != 0 {
            self.device_caps
        } else {
            self.capabilities
        }
    }

    /// The driver's card name, NUL-trimmed.
    fn card(&self) -> String {
        nul_trimmed(&self.card)
    }
}

/// `struct v4l2_fmtdesc`.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct FmtDesc {
    index: u32,
    kind: u32,
    flags: u32,
    description: [u8; 32],
    pixelformat: u32,
    mbus_code: u32,
    reserved: [u32; 3],
}

/// `struct v4l2_frmsize_discrete`.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct FrameSizeDiscrete {
    width: u32,
    height: u32,
}

/// `struct v4l2_frmsize_stepwise`.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct FrameSizeStepwise {
    min_width: u32,
    max_width: u32,
    step_width: u32,
    min_height: u32,
    max_height: u32,
    step_height: u32,
}

/// The size member of `struct v4l2_frmsizeenum`.
#[repr(C)]
#[derive(Clone, Copy)]
union FrameSize {
    discrete: FrameSizeDiscrete,
    stepwise: FrameSizeStepwise,
}

impl Default for FrameSize {
    fn default() -> Self {
        Self {
            stepwise: FrameSizeStepwise::default(),
        }
    }
}

/// `struct v4l2_frmsizeenum`.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct FrameSizeEnum {
    index: u32,
    pixel_format: u32,
    kind: u32,
    size: FrameSize,
    reserved: [u32; 2],
}

/// `struct v4l2_fract`.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Fract {
    numerator: u32,
    denominator: u32,
}

/// `struct v4l2_frmival_stepwise`.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct FrameIntervalStepwise {
    min: Fract,
    max: Fract,
    step: Fract,
}

/// The interval member of `struct v4l2_frmivalenum`.
#[repr(C)]
#[derive(Clone, Copy)]
union FrameInterval {
    discrete: Fract,
    stepwise: FrameIntervalStepwise,
}

impl Default for FrameInterval {
    fn default() -> Self {
        Self {
            stepwise: FrameIntervalStepwise::default(),
        }
    }
}

/// `struct v4l2_frmivalenum`.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct FrameIntervalEnum {
    index: u32,
    pixel_format: u32,
    width: u32,
    height: u32,
    kind: u32,
    interval: FrameInterval,
    reserved: [u32; 2],
}

/// `struct v4l2_pix_format`.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct PixFormat {
    width: u32,
    height: u32,
    pixelformat: u32,
    field: u32,
    bytesperline: u32,
    sizeimage: u32,
    colorspace: u32,
    priv_: u32,
    flags: u32,
    ycbcr_enc: u32,
    quantization: u32,
    xfer_func: u32,
}

/// The largest member of `struct v4l2_format`'s union.
///
/// The kernel writes `v4l2_pix_format` here for a capture device; the union is padded to
/// 200 bytes and 8-byte aligned by `v4l2_window`'s pointer, which is what the raw member
/// and the dummy pointer reproduce.
#[repr(C)]
#[derive(Clone, Copy)]
union FormatUnion {
    pix: PixFormat,
    raw: [u8; 200],
    align: *const u8,
}

impl Default for FormatUnion {
    fn default() -> Self {
        Self { raw: [0; 200] }
    }
}

/// `struct v4l2_format`.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Format {
    kind: u32,
    format: FormatUnion,
}

impl Format {
    /// A single-planar capture format request for one pixel format and size.
    fn for_pix(format: PixelFormat, width: u32, height: u32) -> Self {
        let pix = PixFormat {
            width,
            height,
            pixelformat: format.fourcc(),
            field: FIELD_NONE,
            ..PixFormat::default()
        };
        Self {
            kind: BUF_TYPE_VIDEO_CAPTURE,
            format: FormatUnion { pix },
        }
    }
}

/// `struct v4l2_requestbuffers`.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct RequestBuffers {
    count: u32,
    kind: u32,
    memory: u32,
    capabilities: u32,
    flags: u8,
    reserved: [u8; 3],
}

/// `struct timeval` as the kernel sees it for a buffer timestamp.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct TimeVal {
    tv_sec: i64,
    tv_usec: i64,
}

/// `struct v4l2_timecode`.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Timecode {
    kind: u32,
    flags: u32,
    frames: u8,
    seconds: u8,
    minutes: u8,
    hours: u8,
    userbits: [u8; 4],
}

/// The memory-location member of `struct v4l2_buffer`.
#[repr(C)]
#[derive(Clone, Copy)]
union BufferMemory {
    offset: u32,
    userptr: libc::c_ulong,
    planes: *mut libc::c_void,
    fd: libc::c_int,
}

impl Default for BufferMemory {
    fn default() -> Self {
        Self { offset: 0 }
    }
}

/// `struct v4l2_buffer`.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Buffer {
    index: u32,
    kind: u32,
    bytesused: u32,
    flags: u32,
    field: u32,
    timestamp: TimeVal,
    timecode: Timecode,
    sequence: u32,
    memory: u32,
    memory_location: BufferMemory,
    length: u32,
    reserved2: u32,
    /// The trailing `request_fd`/`reserved` union; part of the struct's size, so it has to
    /// be declared even though this backend never uses requests.
    request_fd: u32,
}

impl Buffer {
    /// The `mmap` offset of this buffer, valid for `V4L2_MEMORY_MMAP`.
    fn offset(&self) -> u32 {
        // SAFETY: for a `MMAP` buffer the driver fills the `offset` member in.
        unsafe { self.memory_location.offset }
    }
}

/// `struct v4l2_captureparm`.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct CaptureParm {
    capability: u32,
    capturemode: u32,
    timeperframe: Fract,
    extendedmode: u32,
    readbuffers: u32,
    reserved: [u32; 4],
}

/// `struct v4l2_outputparm`.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct OutputParm {
    capability: u32,
    outputmode: u32,
    timeperframe: Fract,
    extendedmode: u32,
    writebuffers: u32,
    reserved: [u32; 4],
}

/// The parameter member of `struct v4l2_streamparm`.
#[repr(C)]
#[derive(Clone, Copy)]
union StreamParmUnion {
    capture: CaptureParm,
    output: OutputParm,
    raw: [u8; 200],
}

impl Default for StreamParmUnion {
    fn default() -> Self {
        Self { raw: [0; 200] }
    }
}

/// `struct v4l2_streamparm`.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct StreamParm {
    kind: u32,
    parm: StreamParmUnion,
}

/// Trims a fixed-size NUL-padded byte string.
fn nul_trimmed(bytes: &[u8]) -> String {
    let end = bytes.iter().position(|byte| *byte == 0).unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..end]).trim().to_owned()
}

/// The sizes the kernel's own headers give these structs, and the ioctl numbers that
/// follow from them.
///
/// The numbers are the ones `videodev2.h` documents (`0x80685600` for `VIDIOC_QUERYCAP`,
/// `0xc0405602` for `VIDIOC_ENUM_FMT`, `0xc0d05605` for `VIDIOC_S_FMT`, `0xc0145608` for
/// `VIDIOC_REQBUFS`, `0xc0585609` for `VIDIOC_QUERYBUF`, …); if one of the layouts above
/// ever drifts, the corresponding assertion fails the build instead of the ioctl.
const _: () = {
    assert!(size_of::<Capability>() == 104);
    assert!(size_of::<FmtDesc>() == 64);
    assert!(size_of::<FrameSizeEnum>() == 44);
    assert!(size_of::<FrameIntervalEnum>() == 52);
    assert!(size_of::<Format>() == 208);
    assert!(size_of::<PixFormat>() == 48);
    assert!(size_of::<RequestBuffers>() == 20);
    assert!(size_of::<Buffer>() == 88);
    assert!(size_of::<StreamParm>() == 204);

    assert!(VIDIOC_QUERYCAP == 0x8068_5600);
    assert!(VIDIOC_ENUM_FMT == 0xc040_5602);
    assert!(VIDIOC_G_FMT == 0xc0d0_5604);
    assert!(VIDIOC_S_FMT == 0xc0d0_5605);
    assert!(VIDIOC_REQBUFS == 0xc014_5608);
    assert!(VIDIOC_QUERYBUF == 0xc058_5609);
    assert!(VIDIOC_QBUF == 0xc058_560f);
    assert!(VIDIOC_DQBUF == 0xc058_5611);
    assert!(VIDIOC_STREAMON == 0x4004_5612);
    assert!(VIDIOC_STREAMOFF == 0x4004_5613);
    assert!(VIDIOC_G_PARM == 0xc0cc_5615);
    assert!(VIDIOC_S_PARM == 0xc0cc_5616);
    assert!(VIDIOC_TRY_FMT == 0xc0d0_5640);
    assert!(VIDIOC_ENUM_FRAMESIZES == 0xc02c_564a);
    assert!(VIDIOC_ENUM_FRAMEINTERVALS == 0xc034_564b);
};
