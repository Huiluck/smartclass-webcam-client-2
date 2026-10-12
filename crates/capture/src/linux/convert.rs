//! Turning captured frames into what the encoder and the photo path need.
//!
//! Two conversions happen in this backend. Frames go to *NV12* — the single format the
//! VAAPI encoder accepts, uploaded into a surface — and stills go to *RGB* first, because
//! that is what the JPEG encoder takes. Both are plain scalar loops; a webcam runs at a few
//! megapixels, the capture thread is already dominated by the encode, and a hand-written
//! SIMD path would be a second thing to get wrong on a target this crate cannot run on.
//!
//! The colour arithmetic is the standard limited-range BT.601 pair, matching the
//! coefficients the CLI's own fallback path uses.

use crate::error::CaptureError;

/// The JPEG quality the photo path encodes at.
///
/// Deliberately the same value the CLI's software fallback uses, so a still taken by this
/// backend and one taken by that path look alike.
const PHOTO_QUALITY: u8 = 85;

/// The number of bytes an NV12 image of this size occupies, row pitch included.
pub(crate) const fn nv12_len(width: usize, height: usize) -> usize {
    width * height * 3 / 2
}

/// Copies a packed NV12 frame into a tightly packed NV12 buffer, undoing the driver's row
/// padding.
///
/// The chroma plane keeps the luma pitch in V4L2, so both planes need the same
/// normalisation.
pub(crate) fn nv12_from_nv12(
    source: &[u8],
    pitch: usize,
    width: usize,
    height: usize,
    destination: &mut [u8],
) -> Result<(), CaptureError> {
    let luma_len = width * height;
    let chroma_len = nv12_len(width, height) - luma_len;
    require_capacity(destination, luma_len + chroma_len)?;
    let (luma, chroma) = destination.split_at_mut(luma_len);

    for row in 0..height {
        let source_row = row_slice(source, row * pitch, width)?;
        luma[row * width..(row + 1) * width].copy_from_slice(source_row);
    }

    let chroma_rows = height / 2;
    let chroma_row_bytes = width;
    let chroma_base = pitch * height;
    for row in 0..chroma_rows {
        let source_row = row_slice(source, chroma_base + row * pitch, chroma_row_bytes)?;
        chroma[row * chroma_row_bytes..(row + 1) * chroma_row_bytes].copy_from_slice(source_row);
    }
    Ok(())
}

/// Converts packed YUYV (4:2:2) into NV12, averaging each pair of chroma rows into one
/// 4:2:0 chroma row.
pub(crate) fn nv12_from_yuyv(
    source: &[u8],
    pitch: usize,
    width: usize,
    height: usize,
    destination: &mut [u8],
) -> Result<(), CaptureError> {
    let luma_len = width * height;
    require_capacity(destination, nv12_len(width, height))?;
    let (luma, chroma) = destination.split_at_mut(luma_len);

    let pairs = width / 2;
    let pairs_rounded = width.div_ceil(2);
    let pair_bytes = pairs_rounded * 4;
    for row in 0..height {
        let source_row = row_slice(source, row * pitch, pair_bytes)?;
        let destination_row = &mut luma[row * width..(row + 1) * width];
        for (index, pixel) in destination_row.iter_mut().enumerate() {
            *pixel = source_row[(index / 2) * 4 + (index % 2) * 2];
        }
    }

    let chroma_row_bytes = pairs * 2;
    for row in 0..height / 2 {
        let top = row_slice(source, (2 * row) * pitch, pair_bytes)?;
        let bottom = row_slice(source, (2 * row + 1) * pitch, pair_bytes)?;
        let destination_row = &mut chroma[row * chroma_row_bytes..(row + 1) * chroma_row_bytes];
        for pair in 0..pairs {
            let cb = (u16::from(top[pair * 4 + 1]) + u16::from(bottom[pair * 4 + 1]) + 1) / 2;
            let cr = (u16::from(top[pair * 4 + 3]) + u16::from(bottom[pair * 4 + 3]) + 1) / 2;
            destination_row[pair * 2] = cb as u8;
            destination_row[pair * 2 + 1] = cr as u8;
        }
    }
    Ok(())
}

/// Converts a decoded RGB24 image into NV12.
///
/// Chroma is sampled from the top-left pixel of each 2x2 block rather than averaged, and the
/// luma/chroma arithmetic is limited range (video swing), which is what VAAPI assumes of an
/// NV12 surface.
pub(crate) fn nv12_from_rgb(
    rgb: &[u8],
    width: usize,
    height: usize,
    destination: &mut [u8],
) -> Result<(), CaptureError> {
    let luma_len = width * height;
    require_capacity(rgb, width * height * 3)?;
    require_capacity(destination, nv12_len(width, height))?;
    let (luma, chroma) = destination.split_at_mut(luma_len);
    let chroma_row_bytes = width;

    for row in 0..height / 2 {
        for column in 0..width / 2 {
            let mut samples = [(0u8, 0u8, 0u8); 4];
            for (index, (offset_y, offset_x)) in
                [(0, 0), (0, 1), (1, 0), (1, 1)].into_iter().enumerate()
            {
                let pixel = (row * 2 + offset_y) * width + column * 2 + offset_x;
                samples[index] = rgb_at(rgb, pixel);
            }
            for (index, (offset_y, offset_x)) in
                [(0, 0), (0, 1), (1, 0), (1, 1)].into_iter().enumerate()
            {
                let (red, green, blue) = samples[index];
                luma[(row * 2 + offset_y) * width + column * 2 + offset_x] =
                    luma_of(red, green, blue);
            }
            let (red, green, blue) = samples[0];
            let (cb, cr) = chroma_of(red, green, blue);
            chroma[row * chroma_row_bytes + column * 2] = cb;
            chroma[row * chroma_row_bytes + column * 2 + 1] = cr;
        }
    }

    // An odd height or width leaves a final row/column of luma that no chroma block covers.
    if height % 2 == 1 {
        let row = height - 1;
        for column in 0..width {
            let (red, green, blue) = rgb_at(rgb, row * width + column);
            luma[row * width + column] = luma_of(red, green, blue);
        }
    }
    if width % 2 == 1 {
        for row in 0..height {
            let (red, green, blue) = rgb_at(rgb, row * width + width - 1);
            luma[row * width + width - 1] = luma_of(red, green, blue);
        }
    }
    Ok(())
}

/// Decodes one MJPEG frame into `rgb`, replacing its contents, and reports the image size.
///
/// The decoder is `jpeg-decoder`, a pure-Rust implementation: the alternative on this
/// platform would be libjpeg-turbo through a C shim, which would make the crate need system
/// headers to build. Decoding costs roughly an order of magnitude more CPU than the colour
/// conversion around it, which is the accepted price of a pure-Rust build — see the module
/// documentation on cost.
pub(crate) fn decode_mjpeg(jpeg: &[u8], rgb: &mut Vec<u8>) -> Result<(usize, usize), CaptureError> {
    let mut decoder = jpeg_decoder::Decoder::new(jpeg);
    let pixels = decoder
        .decode()
        .map_err(|error| CaptureError::Os(format!("MJPEG frame decode failed: {error}")))?;
    let info = decoder
        .info()
        .ok_or_else(|| CaptureError::Os("MJPEG frame decoded without image metadata".to_owned()))?;
    let width = usize::from(info.width);
    let height = usize::from(info.height);
    if width == 0 || height == 0 {
        return Err(CaptureError::Os("MJPEG frame decoded to an empty image".to_owned()));
    }
    if pixels.len() < width * height * 3 {
        return Err(CaptureError::Os(format!(
            "MJPEG frame decoded to {} bytes, expected {}",
            pixels.len(),
            width * height * 3
        )));
    }
    rgb.clear();
    rgb.extend_from_slice(&pixels[..width * height * 3]);
    Ok((width, height))
}

/// Converts a packed NV12 frame into RGB24, replacing the contents of `rgb`.
pub(crate) fn rgb_from_nv12(
    source: &[u8],
    pitch: usize,
    width: usize,
    height: usize,
    rgb: &mut Vec<u8>,
) -> Result<(), CaptureError> {
    let luma_len = width * height;
    let chroma_base = pitch * height;
    require_capacity(source, luma_len)?;
    rgb.clear();
    rgb.reserve(luma_len * 3);

    for row in 0..height {
        let luma_row = row_slice(source, row * pitch, width)?;
        let chroma_row = row_slice(source, chroma_base + (row / 2) * pitch, width)?;
        for column in 0..width {
            let y = luma_row[column];
            let cb = chroma_row[(column / 2) * 2];
            let cr = chroma_row[(column / 2) * 2 + 1];
            let (red, green, blue) = rgb_of(y, cb, cr);
            rgb.extend_from_slice(&[red, green, blue]);
        }
    }
    Ok(())
}

/// Converts NV12 into tightly packed BGRA at the size a preview is wanted at, replacing the
/// contents of `bgra`.
///
/// The output is `out_width`x`out_height` and comes from the frame's own `width`x`height`, so a
/// preview pass costs the pixels it will be shown at rather than the capture resolution. Each
/// output pixel takes the source pixel its centre falls on: resampling the frame for a preview
/// would cost more than the conversion does.
///
/// This is the layout the core wants — four bytes per pixel, blue first — which it swaps into the
/// RGBA an image widget takes.
pub(crate) fn bgra_from_nv12(
    source: &[u8],
    pitch: usize,
    width: usize,
    height: usize,
    out_width: usize,
    out_height: usize,
    bgra: &mut Vec<u8>,
) -> Result<(), CaptureError> {
    let luma_len = width * height;
    let chroma_base = pitch * height;
    require_capacity(source, luma_len)?;
    bgra.clear();
    bgra.reserve(out_width * out_height * 4);

    for out_row in 0..out_height {
        let row = source_index(out_row, out_height, height);
        let luma_row = row_slice(source, row * pitch, width)?;
        let chroma_row = row_slice(source, chroma_base + (row / 2) * pitch, width)?;
        for out_column in 0..out_width {
            let column = source_index(out_column, out_width, width);
            let (red, green, blue) = rgb_of(
                luma_row[column],
                chroma_row[(column / 2) * 2],
                chroma_row[(column / 2) * 2 + 1],
            );
            bgra.extend_from_slice(&[blue, green, red, u8::MAX]);
        }
    }
    Ok(())
}

/// The source index one output pixel samples, nearest the centre of the pixel it covers.
fn source_index(out_index: usize, out_len: usize, source_len: usize) -> usize {
    ((2 * out_index + 1) * source_len / (2 * out_len)).min(source_len - 1)
}

/// Converts packed YUYV (4:2:2) into RGB24, replacing the contents of `rgb`.
pub(crate) fn rgb_from_yuyv(
    source: &[u8],
    pitch: usize,
    width: usize,
    height: usize,
    rgb: &mut Vec<u8>,
) -> Result<(), CaptureError> {
    let pairs_rounded = width.div_ceil(2);
    let pair_bytes = pairs_rounded * 4;
    rgb.clear();
    rgb.reserve(width * height * 3);

    for row in 0..height {
        let source_row = row_slice(source, row * pitch, pair_bytes)?;
        for pair in 0..width.div_ceil(2) {
            let y0 = source_row[pair * 4];
            let cb = source_row[pair * 4 + 1];
            let y1 = source_row[pair * 4 + 2];
            let cr = source_row[pair * 4 + 3];
            let (red, green, blue) = rgb_of(y0, cb, cr);
            rgb.extend_from_slice(&[red, green, blue]);
            if pair * 2 + 1 < width {
                let (red, green, blue) = rgb_of(y1, cb, cr);
                rgb.extend_from_slice(&[red, green, blue]);
            }
        }
    }
    Ok(())
}

/// Encodes an RGB24 image as JPEG, replacing the contents of `out`.
pub(crate) fn encode_jpeg(
    rgb: &[u8],
    width: usize,
    height: usize,
    out: &mut Vec<u8>,
) -> Result<(), CaptureError> {
    require_capacity(rgb, width * height * 3)?;
    let (Ok(width), Ok(height)) = (u16::try_from(width), u16::try_from(height)) else {
        // `jpeg_encoder` cannot express images wider or taller than 65535 pixels, and no
        // webcam does: refuse rather than encode a wrapped-around image.
        return Err(CaptureError::Os(format!(
            "photo size {width}x{height} is too large for the JPEG encoder"
        )));
    };
    let encoder = jpeg_encoder::Encoder::new(&mut *out, PHOTO_QUALITY);
    encoder
        .encode(rgb, width, height, jpeg_encoder::ColorType::Rgb)
        .map_err(|error| CaptureError::Os(format!("photo JPEG encoding failed: {error}")))?;
    Ok(())
}

/// The luma of one limited-range BT.601 sample.
fn luma_of(red: u8, green: u8, blue: u8) -> u8 {
    let red = i32::from(red);
    let green = i32::from(green);
    let blue = i32::from(blue);
    (((66 * red + 129 * green + 25 * blue + 128) >> 8) + 16).clamp(0, 255) as u8
}

/// The chroma pair of one limited-range BT.601 sample.
fn chroma_of(red: u8, green: u8, blue: u8) -> (u8, u8) {
    let red = i32::from(red);
    let green = i32::from(green);
    let blue = i32::from(blue);
    let cb = ((-38 * red - 74 * green + 112 * blue + 128) >> 8) + 128;
    let cr = ((112 * red - 94 * green - 18 * blue + 128) >> 8) + 128;
    (cb.clamp(0, 255) as u8, cr.clamp(0, 255) as u8)
}

/// One RGB triple back from a limited-range BT.601 luma/chroma pair.
///
/// The coefficients are the integer approximations the CLI's fallback path uses
/// (`r = y + 1436*v`, `g = y - 352*u - 731*v`, `b = y + 1815*u`), so stills produced here
/// and there agree.
fn rgb_of(luma: u8, cb: u8, cr: u8) -> (u8, u8, u8) {
    let y = i32::from(luma) - 16;
    let u = i32::from(cb) - 128;
    let v = i32::from(cr) - 128;
    let red = y + ((1436 * v) >> 10);
    let green = y - ((352 * u + 731 * v) >> 10);
    let blue = y + ((1815 * u) >> 10);
    (
        red.clamp(0, 255) as u8,
        green.clamp(0, 255) as u8,
        blue.clamp(0, 255) as u8,
    )
}

/// One RGB triple at a pixel index in an RGB24 buffer.
fn rgb_at(rgb: &[u8], pixel: usize) -> (u8, u8, u8) {
    let base = pixel * 3;
    match rgb.get(base..base + 3) {
        Some(samples) => (samples[0], samples[1], samples[2]),
        // Cannot happen after `require_capacity`, and reading a black pixel beats panicking
        // in a capture thread.
        None => (0, 0, 0),
    }
}

/// Borrows `len` bytes at `offset`, refusing a short buffer.
fn row_slice(source: &[u8], offset: usize, len: usize) -> Result<&[u8], CaptureError> {
    let end = offset
        .checked_add(len)
        .ok_or_else(|| CaptureError::Os("frame offset overflowed".to_owned()))?;
    source
        .get(offset..end)
        .ok_or_else(|| truncated(end, source.len()))
}

/// Refuses a buffer smaller than the conversion needs.
fn require_capacity(buffer: &[u8], needed: usize) -> Result<(), CaptureError> {
    if buffer.len() < needed {
        return Err(truncated(needed, buffer.len()));
    }
    Ok(())
}

/// The error for a frame shorter than its own geometry requires.
fn truncated(needed: usize, available: usize) -> CaptureError {
    CaptureError::Os(format!(
        "frame is {available} bytes but {needed} are needed for the negotiated format"
    ))
}
