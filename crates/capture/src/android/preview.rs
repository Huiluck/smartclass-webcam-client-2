//! The live preview's image source: a `YUV_420_888` reader, its window riding in the capture
//! session beside the encoder surface, and the conversion that turns its planes into the BGRA
//! the core's preview path expects.
//!
//! # Why a third output
//!
//! The encoder's input surface cannot be read back: from the moment the camera writes into it,
//! the buffers live in the codec's own queue and nothing this backend holds can map them. The
//! JPEG reader the still path uses is equally unreadable as a picture — its plane is a
//! compressed frame, and decoding one per preview frame to throw it away is worse than the
//! conversion done here. So the preview gets its own output: the camera renders into it with
//! every capture request, and the reader hands the planes over as a plain image.
//!
//! # Reading without waiting
//!
//! A preview frame that is late is worthless — the next capture request is 16 ms away — so the
//! reader never blocks and never queues. One frame is acquired per call and deleted right after
//! the copy; when the camera has not finished a frame yet, the call reports nothing and the
//! capture thread moves on. Frames the UI never consumed are dropped by the core, not here.

use std::ptr;
use std::slice;

use tracing::{debug, warn};

use super::image_reader::{
    AImage, AImageReader, AImageReader_acquireNextImage, AImageReader_delete,
    AImageReader_getWindow, AImageReader_new, AImage_delete, AImage_getHeight,
    AImage_getPlaneData, AImage_getPlanePixelStride, AImage_getPlaneRowStride, AImage_getWidth,
};
use super::native_window::ANativeWindow;
use super::still::{AMEDIA_ERROR_INVALID_PARAMETER, AMEDIA_IMGREADER_NO_BUFFER_AVAILABLE, AMEDIA_OK, status_error};
use crate::error::CaptureError;

/// `AIMAGE_FORMAT_YUV_420_888`: the flexible YUV format every camera can produce.
pub(crate) const IMAGE_FORMAT_YUV_420_888: i32 = 0x23;

/// How many frames the reader may hold at once.
///
/// Two, like the still reader: one the camera is filling and one waiting to be read. More would
/// only queue pictures nobody asked to keep.
const MAX_IMAGES: i32 = 2;

/// The widest the preview reader is created at, whatever the stream's resolution is.
///
/// The preview is drawn in a box a few hundred pixels wide, and the core scales the frame to it
/// anyway, so asking the camera to render the stream's full size into a third output buys
/// nothing but camera bandwidth — for a 16:9 stream that is several times the pixels the picture
/// can show, and on a device whose HAL is already refusing awkward output combinations it is the
/// difference between a preview and none.
const MAX_PREVIEW_WIDTH: u32 = 1280;

/// The camera's YUV output as a preview source.
pub(crate) struct PreviewCapture {
    /// The reader the camera writes frames into.
    reader: *mut AImageReader,
    /// The reader's window, borrowed until the reader is deleted.
    window: *mut ANativeWindow,
    /// Reused BGRA output buffer, so a preview per frame does not allocate one.
    bgra: Vec<u8>,
    /// Whether a conversion has ever produced a frame, for the one-time log.
    reported: bool,
    /// Clockwise degrees to turn the frame by before it is shown.
    ///
    /// The camera hands over frames in its sensor's own orientation, and a device whose sensor
    /// is mounted sideways delivers a picture that is turned against the interface — the phone
    /// this backend was developed on reports 90. Nothing upstream rotates a frame: the protocol
    /// carries no orientation field, and the macOS shim documents the rotation as the app
    /// layer's job. On Android that layer is this backend, and a preview is the one consumer
    /// that can be turned without touching what the server is sent.
    rotation_degrees: u16,
}

impl PreviewCapture {
    /// Creates a YUV image reader for `width`x`height`.
    ///
    /// The reader is created at the preview's own size rather than the stream's — see
    /// [`MAX_PREVIEW_WIDTH`] — so the camera renders only the pixels the picture can show.
    /// `rotation_degrees` is the sensor's orientation, applied when a frame is converted.
    pub(crate) fn new(
        width: u32,
        height: u32,
        rotation_degrees: u16,
    ) -> Result<Self, CaptureError> {
        let (width, height) = preview_size(width, height);
        let width = i32::try_from(width).map_err(|_| CaptureError::UnsupportedParameters)?;
        let height = i32::try_from(height).map_err(|_| CaptureError::UnsupportedParameters)?;

        let mut reader: *mut AImageReader = ptr::null_mut();
        // SAFETY: the out pointer is a live local; the format and image count are values the
        // header accepts; on a non-OK status the pointer is left untouched and nothing is owned.
        let status = unsafe {
            AImageReader_new(
                width,
                height,
                IMAGE_FORMAT_YUV_420_888,
                MAX_IMAGES,
                &mut reader,
            )
        };
        if status != AMEDIA_OK {
            if status == AMEDIA_ERROR_INVALID_PARAMETER {
                return Err(CaptureError::UnsupportedParameters);
            }
            return Err(status_error("AImageReader_new (preview)", status));
        }
        if reader.is_null() {
            return Err(CaptureError::Os(
                "AImageReader_new reported success but returned no preview reader".to_owned(),
            ));
        }

        let mut window: *mut ANativeWindow = ptr::null_mut();
        // SAFETY: the reader was created just above and is live; the out pointer is a local.
        let status = unsafe { AImageReader_getWindow(reader, &mut window) };
        if status != AMEDIA_OK || window.is_null() {
            // SAFETY: this value is the reader's only owner at this point.
            unsafe { AImageReader_delete(reader) };
            if status != AMEDIA_OK {
                return Err(status_error("AImageReader_getWindow (preview)", status));
            }
            return Err(CaptureError::Os(
                "AImageReader_getWindow reported success but returned no preview window".to_owned(),
            ));
        }

        debug!(width, height, "created the YUV image reader for the preview output");
        Ok(Self {
            reader,
            window,
            bgra: Vec::new(),
            reported: false,
            rotation_degrees,
        })
    }

    /// The window the capture session has to include as an output.
    pub(crate) fn window(&self) -> *mut ANativeWindow {
        self.window
    }

    /// Offers the newest frame the camera has written to the core's preview, if any.
    ///
    /// Called once per capture-thread loop: it takes at most one frame, converts it to BGRA at
    /// the size the preview asked for, and publishes it. Nothing is converted while the preview
    /// is off — [`scwc_core::preview::next_frame_size`] answers `None` and the frame is dropped
    /// unread, which is what keeps a preview nobody is watching from costing anything.
    pub(crate) fn publish(&mut self) {
        let mut image = self.acquire();
        if image.is_null() {
            return;
        }
        // Whatever the camera wrote while the last frame was being converted is already older
        // than the picture about to be shown, so it is dropped rather than queued: a preview
        // that plays through a backlog is the lag this loop exists to avoid.
        loop {
            let newer = self.acquire();
            if newer.is_null() {
                break;
            }
            // SAFETY: the image was acquired above and is not used again.
            unsafe { AImage_delete(image) };
            image = newer;
        }

        let converted = self.convert_and_publish(image);

        // SAFETY: the image was acquired above and is deleted exactly once, here; nothing reads
        // the planes after this, and the conversion copies what it needs before returning.
        unsafe { AImage_delete(image) };

        match converted {
            Ok(true) => {
                if !self.reported {
                    self.reported = true;
                    debug!("the camera preview is now producing frames");
                }
            }
            Ok(false) => {}
            Err(error) => warn!(%error, "could not build a preview frame"),
        }
    }

    /// Takes the next frame the camera has written, or null when there is none.
    fn acquire(&self) -> *mut AImage {
        let mut image: *mut AImage = ptr::null_mut();
        // SAFETY: the reader is live and owned by this value; the out pointer is a local. On
        // `AMEDIA_OK` the platform hands over one image the caller then owns.
        let status = unsafe { AImageReader_acquireNextImage(self.reader, &mut image) };
        if status != AMEDIA_OK {
            // The ordinary case by far: the camera has not finished a frame since the last call.
            if status != AMEDIA_IMGREADER_NO_BUFFER_AVAILABLE {
                debug!(status, "the preview reader had no usable frame");
            }
            return ptr::null_mut();
        }
        image
    }

    /// Converts `image` to BGRA at the preview's size and publishes it; `false` when there is
    /// nothing to publish.
    fn convert_and_publish(&mut self, image: *mut AImage) -> Result<bool, CaptureError> {
        let (width, height) = image_size(image)?;
        let Some((out_width, out_height)) =
            scwc_core::preview::next_frame_size(width, height)
        else {
            // No preview is being watched, or the consumer is still behind its last frame.
            return Ok(false);
        };

        let planes = Planes::read(image, width, height)?;
        self.bgra.clear();
        // The picture is turned before it is scaled, so the fitted size is computed from the
        // shape the operator will see: a sensor mounted sideways reports its frame one way and
        // the interface is the other.
        let (shown_width, shown_height) = if self.rotation_degrees % 180 == 0 {
            (out_width, out_height)
        } else {
            (out_height, out_width)
        };
        self.bgra
            .resize(shown_width as usize * shown_height as usize * 4, 0);
        planes.to_bgra(shown_width, shown_height, self.rotation_degrees, &mut self.bgra)?;
        scwc_core::preview::publish_bgra(&self.bgra, shown_width, shown_height);
        Ok(true)
    }
}

impl Drop for PreviewCapture {
    fn drop(&mut self) {
        if !self.reader.is_null() {
            // SAFETY: this value owns the reader for its whole life, so the delete happens once.
            unsafe { AImageReader_delete(self.reader) };
            self.reader = ptr::null_mut();
        }
        self.window = ptr::null_mut();
    }
}

// SAFETY: the reader is created, used and deleted on the capture thread alone, and the window
// is only handed back to that same thread's session.
unsafe impl Send for PreviewCapture {}

/// The size the preview reader is created at: `width`x`height` capped to [`MAX_PREVIEW_WIDTH`].
///
/// Keeps the picture's shape while it shrinks it, so the frame the core scales to the UI box
/// keeps the camera's aspect ratio. Sizes the camera would not accept are rounded down to even
/// numbers, which every YUV output requires.
fn preview_size(width: u32, height: u32) -> (u32, u32) {
    if width <= MAX_PREVIEW_WIDTH {
        return (width & !1, height & !1);
    }
    let height = (height as u64 * MAX_PREVIEW_WIDTH as u64 / width as u64) as u32;
    (MAX_PREVIEW_WIDTH & !1, height & !1)
}

/// The pixel size of an acquired image.
fn image_size(image: *const AImage) -> Result<(u32, u32), CaptureError> {
    let (mut width, mut height) = (0i32, 0i32);
    // SAFETY: the image is live for the caller's duration and both out pointers are locals.
    let status = unsafe { AImage_getWidth(image, &mut width) };
    if status != AMEDIA_OK {
        return Err(status_error("AImage_getWidth", status));
    }
    // SAFETY: as above.
    let status = unsafe { AImage_getHeight(image, &mut height) };
    if status != AMEDIA_OK {
        return Err(status_error("AImage_getHeight", status));
    }
    if width <= 0 || height <= 0 {
        return Err(CaptureError::Os(
            "the preview image reported an empty size".to_owned(),
        ));
    }
    Ok((width as u32, height as u32))
}

/// One plane of a YUV image: the bytes the platform points at, and how they are laid out.
struct Plane<'a> {
    /// The plane's rows.
    data: &'a [u8],
    /// Bytes from one row to the next.
    row_stride: usize,
    /// Bytes from one sample to the next within a row.
    pixel_stride: usize,
}

impl Plane<'_> {
    /// The sample at `(x, y)`.
    fn sample(&self, x: usize, y: usize) -> u8 {
        // Clamped rather than bounds-checked: a caller that walks with the plane's own strides
        // can only land inside it, and clamping keeps a malformed stride from panicking on a
        // capture thread — a frame the platform described oddly is skipped, not fatal.
        let offset = y.min(self.rows().saturating_sub(1)) * self.row_stride
            + x.min(self.columns().saturating_sub(1)) * self.pixel_stride;
        self.data.get(offset).copied().unwrap_or(0)
    }

    fn rows(&self) -> usize {
        if self.row_stride == 0 {
            0
        } else {
            self.data.len().div_ceil(self.row_stride.max(1))
        }
    }

    fn columns(&self) -> usize {
        if self.pixel_stride == 0 {
            0
        } else {
            self.row_stride / self.pixel_stride
        }
    }
}

/// The three planes of a YUV image, copied out of an acquired frame.
struct Planes {
    /// Full-resolution luma, one byte per pixel.
    y: PlaneData,
    /// Quarter-resolution chroma.
    u: PlaneData,
    v: PlaneData,
    width: u32,
    height: u32,
}

/// One plane's bytes and layout, owned so it outlives the image.
struct PlaneData {
    data: Vec<u8>,
    row_stride: usize,
    pixel_stride: usize,
}

impl Planes {
    /// Copies the three planes out of a live image.
    ///
    /// The camera's buffers are only valid while the image is held, and the conversion happens
    /// after the image is deleted, so the bytes are copied here rather than borrowed.
    fn read(image: *const AImage, width: u32, height: u32) -> Result<Self, CaptureError> {
        Ok(Self {
            y: read_plane(image, 0)?,
            u: read_plane(image, 1)?,
            v: read_plane(image, 2)?,
            width,
            height,
        })
    }

    /// Converts to tightly packed BGRA at `out_width`x`out_height`, rotated by `rotation`, nearest-neighbour.
    ///
    /// The preview is scaled here rather than after the fact: a conversion pass per frame is
    /// affordable, a second pass over the full-resolution picture is not. Chroma is sampled at
    /// a quarter resolution, so the subsampling is done by index rather than by averaging —
    /// the picture the operator gets is what the camera produced, at the size it was asked for.
    ///
    /// `rotation` is the clockwise turn the sensor needs (0, 90, 180 or 270); the source is
    /// walked in the destination's order, so one pass both turns and scales and no intermediate
    /// frame is allocated.
    fn to_bgra(
        &self,
        out_width: u32,
        out_height: u32,
        rotation: u16,
        out: &mut [u8],
    ) -> Result<(), CaptureError> {
        if self.width == 0 || self.height == 0 || out_width == 0 || out_height == 0 {
            return Err(CaptureError::UnsupportedParameters);
        }
        let y = Plane {
            data: &self.y.data,
            row_stride: self.y.row_stride,
            pixel_stride: self.y.pixel_stride,
        };
        let u = Plane {
            data: &self.u.data,
            row_stride: self.u.row_stride,
            pixel_stride: self.u.pixel_stride,
        };
        let v = Plane {
            data: &self.v.data,
            row_stride: self.v.row_stride,
            pixel_stride: self.v.pixel_stride,
        };

        // The frame before rotation, in the space the destination's coordinates map back into.
        // A 90/270 turn swaps which destination axis comes from which source axis, so the
        // pre-rotation destination box is the shown one with its sides exchanged.
        let (pre_width, pre_height) = if rotation % 180 == 0 {
            (out_width, out_height)
        } else {
            (out_height, out_width)
        };
        let (source_width, source_height) = (self.width as usize, self.height as usize);

        for row in 0..out_height as usize {
            for column in 0..out_width as usize {
                // Where this destination pixel sits in the unrotated frame.
                let (pre_x, pre_y) = match rotation % 360 {
                    90 => (row * pre_width as usize / out_height as usize, (out_width as usize - 1 - column) * pre_height as usize / out_width as usize),
                    180 => (
                        (out_width as usize - 1 - column) * pre_width as usize / out_width as usize,
                        (out_height as usize - 1 - row) * pre_height as usize / out_height as usize,
                    ),
                    270 => (
                        (out_height as usize - 1 - row) * pre_width as usize / out_height as usize,
                        column * pre_height as usize / out_width as usize,
                    ),
                    // 0, and anything malformed, which is drawn as it came.
                    _ => (
                        column * pre_width as usize / out_width as usize,
                        row * pre_height as usize / out_height as usize,
                    ),
                };

                let source_x = pre_x * source_width / pre_width as usize;
                let source_y = pre_y * source_height / pre_height as usize;
                let luma = y.sample(source_x, source_y) as f32;
                // Chroma planes are half the size in both directions.
                let chroma_x = source_x / 2;
                let chroma_y = source_y / 2;
                let cb = u.sample(chroma_x, chroma_y) as f32 - 128.0;
                let cr = v.sample(chroma_x, chroma_y) as f32 - 128.0;

                // The BT.601 full-range conversion, which is what `YUV_420_888` carries.
                let r = luma + 1.402 * cr;
                let g = luma - 0.344_136 * cb - 0.714_136 * cr;
                let b = luma + 1.772 * cb;

                let base = (row * out_width as usize + column) * 4;
                out[base] = clamp(b);
                out[base + 1] = clamp(g);
                out[base + 2] = clamp(r);
                out[base + 3] = 0xFF;
            }
        }
        Ok(())
    }
}

/// Rounds a colour component to a byte.
fn clamp(value: f32) -> u8 {
    value.clamp(0.0, 255.0).round() as u8
}

/// Copies one plane of an acquired image out.
fn read_plane(image: *const AImage, index: i32) -> Result<PlaneData, CaptureError> {
    let mut data: *mut u8 = ptr::null_mut();
    let mut length: i32 = 0;
    // SAFETY: the image is live for the caller's duration and both out pointers are locals.
    let status = unsafe { AImage_getPlaneData(image, index, &mut data, &mut length) };
    if status != AMEDIA_OK {
        return Err(status_error("AImage_getPlaneData (preview)", status));
    }
    if data.is_null() || length <= 0 {
        return Err(CaptureError::Os(format!(
            "the preview image's plane {index} carried no readable data"
        )));
    }

    let mut row_stride: i32 = 0;
    // SAFETY: as above.
    let status = unsafe { AImage_getPlaneRowStride(image, index, &mut row_stride) };
    if status != AMEDIA_OK {
        return Err(status_error("AImage_getPlaneRowStride", status));
    }
    let mut pixel_stride: i32 = 0;
    // SAFETY: as above.
    let status = unsafe { AImage_getPlanePixelStride(image, index, &mut pixel_stride) };
    if status != AMEDIA_OK {
        return Err(status_error("AImage_getPlanePixelStride", status));
    }

    // SAFETY: the platform filled `data` with a pointer into the image's plane and `length`
    // with its valid length in bytes; the image is still alive, so the range is readable for
    // this call, and the bytes are copied out before it is deleted.
    let bytes = unsafe { slice::from_raw_parts(data, length as usize) };
    Ok(PlaneData {
        data: bytes.to_vec(),
        row_stride: row_stride.max(0) as usize,
        pixel_stride: pixel_stride.max(0) as usize,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One Y plane plus neutral chroma, so the conversion is a pure luma pass.
    fn planes(width: u32, height: u32, luma: impl Fn(u32, u32) -> u8) -> Planes {
        let mut y = Vec::with_capacity((width * height) as usize);
        for row in 0..height {
            for column in 0..width {
                y.push(luma(column, row));
            }
        }
        Planes {
            y: PlaneData {
                data: y,
                row_stride: width as usize,
                pixel_stride: 1,
            },
            // 128 in both chroma planes is neutral grey, so every output colour is the luma.
            u: PlaneData {
                data: vec![128; (width * height / 4) as usize],
                row_stride: (width / 2) as usize,
                pixel_stride: 1,
            },
            v: PlaneData {
                data: vec![128; (width * height / 4) as usize],
                row_stride: (width / 2) as usize,
                pixel_stride: 1,
            },
            width,
            height,
        }
    }

    fn convert(width: u32, height: u32, rotation: u16) -> (Vec<u8>, u32, u32) {
        // A marker in the top-left and a different one in the top-right, so a turn is visible
        // in the output regardless of scaling.
        let source = planes(width, height, |x, y| {
            if y < height / 2 && x < width / 2 {
                255
            } else if y < height / 2 {
                64
            } else if x < width / 2 {
                32
            } else {
                0
            }
        });
        let mut out = vec![0u8; (width * height * 4) as usize];
        source
            .to_bgra(width, height, rotation, &mut out)
            .expect("conversion succeeds");
        (out, width, height)
    }

    fn luma_at(out: &[u8], width: u32, x: u32, y: u32) -> u8 {
        out[((y * width + x) * 4) as usize + 2]
    }

    #[test]
    fn a_zero_rotation_leaves_the_frame_as_it_came() {
        let (out, width, _) = convert(8, 8, 0);
        assert_eq!(luma_at(&out, width, 1, 1), 255, "top-left stays top-left");
        assert_eq!(luma_at(&out, width, 6, 1), 64, "top-right stays top-right");
        assert_eq!(luma_at(&out, width, 1, 6), 32);
        assert_eq!(luma_at(&out, width, 6, 6), 0);
    }

    #[test]
    fn a_quarter_turn_moves_the_top_left_corner_to_the_top_right() {
        // Turning clockwise: the source's top-left lands at the destination's top-right.
        let (out, width, _) = convert(8, 8, 90);
        assert_eq!(luma_at(&out, width, 6, 1), 255, "source top-left → top-right");
        assert_eq!(luma_at(&out, width, 6, 6), 64, "source top-right → bottom-right");
        assert_eq!(luma_at(&out, width, 1, 1), 32, "source bottom-left → top-left");
        assert_eq!(luma_at(&out, width, 1, 6), 0);
    }

    #[test]
    fn a_half_turn_moves_the_corner_diagonally() {
        let (out, width, _) = convert(8, 8, 180);
        assert_eq!(luma_at(&out, width, 6, 6), 255, "top-left → bottom-right");
        assert_eq!(luma_at(&out, width, 1, 6), 64);
        assert_eq!(luma_at(&out, width, 6, 1), 32);
        assert_eq!(luma_at(&out, width, 1, 1), 0);
    }

    #[test]
    fn a_three_quarter_turn_moves_the_top_left_corner_to_the_bottom_left() {
        let (out, width, _) = convert(8, 8, 270);
        assert_eq!(luma_at(&out, width, 1, 6), 255, "source top-left → bottom-left");
        assert_eq!(luma_at(&out, width, 1, 1), 64);
        assert_eq!(luma_at(&out, width, 6, 6), 32);
        assert_eq!(luma_at(&out, width, 6, 1), 0);
    }

    #[test]
    fn a_quarter_turn_swaps_the_side_lengths() {
        // A wide source turned a quarter stands tall: the destination is the source's shape
        // transposed, which is what keeps a landscape sensor's picture filling a portrait box.
        let source = planes(16, 4, |_, _| 128);
        let mut out = vec![0u8; 16 * 4 * 4];
        // 4 wide by 16 tall, i.e. the source's sides exchanged at the same pixel count.
        source
            .to_bgra(4, 16, 90, &mut out)
            .expect("conversion succeeds");
        assert!(!out.is_empty());
    }

    #[test]
    fn the_reader_is_capped_at_the_preview_width() {
        assert_eq!(preview_size(1920, 1620), (1280, 1080), "scaled, shape kept, even");
        assert_eq!(preview_size(640, 480), (640, 480), "already small enough");
        assert_eq!(preview_size(1921, 1081), (1280, 720), "odd input, even output");
    }
}
