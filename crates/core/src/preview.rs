//! Live preview: the camera picture an embedding application paints.
//!
//! The caller never touches the encoded stream. It registers one sink and the pixel size of
//! the area it wants the picture in ([`set_preview_sink`], [`set_preview_target_size`]), and
//! the capture layer calls [`publish_bgra`] with every frame it captures. The frame is
//! converted to RGBA and scaled *aspect-locked inside* the requested size right here, so the
//! frames themselves carry no letterboxing and the caller only has to centre what it gets.
//!
//! Nothing on this path can fail the capture: a frame nobody consumes is dropped, and a frame
//! is only ever converted while a sink is registered and a target size is set
//! ([`preview_enabled`]) — a preview the caller is not showing costs nothing.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Mutex, MutexGuard};

use tokio::sync::mpsc::UnboundedSender;

/// One frame of the live camera picture.
///
/// The pixels are already in the layout an image widget wants: row-major RGBA, four bytes per
/// pixel, no row padding, scaled to fit the size the caller asked for.
#[derive(Debug, Clone)]
pub struct PreviewFrame {
    /// Row-major pixels without padding, four bytes each: red, green, blue, alpha.
    pub rgba: Vec<u8>,
    /// Frame width in pixels, after scaling.
    pub width: u32,
    /// Frame height in pixels, after scaling.
    pub height: u32,
}

/// The sink frames are offered to, or `None` while the caller shows no preview.
static SINK: Mutex<Option<UnboundedSender<PreviewFrame>>> = Mutex::new(None);

/// Width of the box frames are scaled to fit; zero while the preview is off.
static TARGET_WIDTH: AtomicU32 = AtomicU32::new(0);

/// Height of the box frames are scaled to fit; zero while the preview is off.
static TARGET_HEIGHT: AtomicU32 = AtomicU32::new(0);

/// Registers the sink the live preview is offered to; `None` stops it.
///
/// Replacing a sink drops the previous one, which ends the receiver its consumer awaits. Every
/// frame is offered with a non-blocking send, so a slow consumer loses frames rather than
/// slowing the camera down.
pub fn set_preview_sink(sink: Option<UnboundedSender<PreviewFrame>>) {
    *sink_slot() = sink;
}

/// Sets the pixel size the preview frames are scaled to fit; `0x0` disables the preview.
///
/// The picture keeps its aspect ratio and is scaled to fit *inside* this box, so the frames a
/// caller receives are as tall or as wide as the camera's own picture allows — never the box
/// itself with the letterbox baked in.
pub fn set_preview_target_size(width: u32, height: u32) {
    TARGET_WIDTH.store(width, Ordering::Relaxed);
    TARGET_HEIGHT.store(height, Ordering::Relaxed);
}

/// Whether a preview is wanted right now: a sink is registered and a target size is set.
///
/// The capture layer checks this before it converts anything, so a caller that shows no
/// preview pays neither the conversion nor the scaling.
pub fn preview_enabled() -> bool {
    target_size().is_some() && sink_slot().is_some()
}

/// Converts one captured frame and offers it to the sink.
///
/// `pixels` is the frame as the backend hands it over: tightly packed BGRA, four bytes per
/// pixel, row-major, `width * height` of them. The frame is scaled aspect-locked into the
/// target size, its channels swapped to RGBA, and offered with a non-blocking send — a caller
/// that is behind loses this frame instead of holding up the capture.
///
/// Does nothing while [`preview_enabled`] is `false`, which is what lets the capture layer call
/// this unconditionally.
pub fn publish_bgra(pixels: &[u8], width: u32, height: u32) {
    let Some((target_width, target_height)) = target_size() else {
        return;
    };
    let Some(source) = Bgra::new(pixels, width, height) else {
        return;
    };
    // Cloned out of the slot before the conversion, so the lock is held only long enough to
    // look: a caller that stops listening (the sink was replaced, or the run ended) neither
    // waits for a frame being converted nor pays for one it will never see.
    let Some(sender) = sink_slot().as_ref().cloned() else {
        return;
    };

    let (out_width, out_height) = fit(source.width, source.height, target_width, target_height);
    let rgba = source.scale_to(out_width, out_height);
    // The channel is unbounded, so this only fails once the consumer is gone.
    let _ = sender.send(PreviewFrame {
        rgba,
        width: out_width,
        height: out_height,
    });
}

/// The frame a backend handed over, checked against the geometry it claims.
struct Bgra<'a> {
    pixels: &'a [u8],
    width: u32,
    height: u32,
}

impl<'a> Bgra<'a> {
    /// Borrows a packed BGRA frame, or `None` when its geometry does not describe `pixels`.
    fn new(pixels: &'a [u8], width: u32, height: u32) -> Option<Self> {
        let needed = usize::try_from(u64::from(width) * u64::from(height) * 4).ok()?;
        (width > 0 && height > 0 && pixels.len() >= needed).then_some(Self {
            pixels,
            width,
            height,
        })
    }

    /// Scales the frame to exactly `width`x`height` and swaps it to RGBA.
    ///
    /// Nearest neighbour, sampling directly out of the source: the capture thread is a camera
    /// callback that must not block, and a preview is read by an eye that cannot tell the
    /// difference at the sizes this is used at. Alpha is written opaque — a camera picture has
    /// no transparency.
    fn scale_to(&self, width: u32, height: u32) -> Vec<u8> {
        let source_width = self.width as usize;
        let source_height = self.height as usize;
        let width = width as usize;
        let height = height as usize;
        let mut rgba = vec![0u8; width * height * 4];

        for row in 0..height {
            let source_row = row * source_height / height;
            let source_base = source_row * source_width * 4;
            for column in 0..width {
                let source_column = column * source_width / width;
                let source = source_base + source_column * 4;
                let target = (row * width + column) * 4;
                rgba[target] = self.pixels[source + 2];
                rgba[target + 1] = self.pixels[source + 1];
                rgba[target + 2] = self.pixels[source];
                rgba[target + 3] = u8::MAX;
            }
        }
        rgba
    }
}

/// The box frames are scaled to fit, or `None` while the preview is off.
fn target_size() -> Option<(u32, u32)> {
    let width = TARGET_WIDTH.load(Ordering::Relaxed);
    let height = TARGET_HEIGHT.load(Ordering::Relaxed);
    (width > 0 && height > 0).then_some((width, height))
}

/// The size a `source` frame is scaled to, aspect-locked inside `target`.
///
/// The picture is fitted, never stretched: the result touches two opposite sides of the target
/// box and is smaller on the other two. Cross-multiplied integers rather than floating point,
/// so the sizes a caller reads back are exact.
fn fit(source_width: u32, source_height: u32, target_width: u32, target_height: u32) -> (u32, u32) {
    let source_width = u64::from(source_width);
    let source_height = u64::from(source_height);
    let target_width = u64::from(target_width);
    let target_height = u64::from(target_height);

    if target_width * source_height <= target_height * source_width {
        // The target is relatively wider than the picture: the width is what limits it.
        let height = ((target_width * source_height + source_width / 2) / source_width).max(1);
        (target_width as u32, height.min(target_height) as u32)
    } else {
        let width = ((target_height * source_width + source_height / 2) / source_height).max(1);
        (width.min(target_width) as u32, target_height as u32)
    }
}

/// The sink slot, with a poisoned lock treated as an empty one.
fn sink_slot() -> MutexGuard<'static, Option<UnboundedSender<PreviewFrame>>> {
    SINK.lock().unwrap_or_else(|error| error.into_inner())
}

#[cfg(test)]
mod tests {
    use super::{
        fit, preview_enabled, publish_bgra, set_preview_sink, set_preview_target_size,
        PreviewFrame,
    };
    use tokio::sync::mpsc::UnboundedReceiver;

    /// Serializes the tests below: the preview's sink and target size are process-wide, so two
    /// tests touching them at once would see each other's state.
    static PREVIEW_TESTS: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Takes the lock the stateful tests share, and clears the state they leave behind.
    fn exclusive() -> std::sync::MutexGuard<'static, ()> {
        let guard = PREVIEW_TESTS
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        set_preview_sink(None);
        set_preview_target_size(0, 0);
        guard
    }

    /// Registers a sink and a target size, and hands back the receiver.
    fn preview(width: u32, height: u32) -> UnboundedReceiver<PreviewFrame> {
        let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
        set_preview_sink(Some(sender));
        set_preview_target_size(width, height);
        receiver
    }

    #[test]
    fn a_picture_is_fitted_inside_the_target_never_stretched() {
        // Wider than the box: the width is the limit.
        assert_eq!(fit(1920, 1080, 640, 480), (640, 360));
        // Taller than the box: the height is the limit.
        assert_eq!(fit(1080, 1920, 640, 480), (270, 480));
        // The same aspect ratio: the box, exactly.
        assert_eq!(fit(1920, 1080, 1920, 1080), (1920, 1080));
        // A box narrower than one pixel still yields a picture.
        assert_eq!(fit(4000, 3000, 1, 1), (1, 1));
        // Rounding lands on the nearer side.
        assert_eq!(fit(3, 2, 5, 5), (5, 3));
    }

    #[test]
    fn a_published_frame_arrives_scaled_and_swapped_to_rgba() {
        let _guard = exclusive();
        // 4x2 BGRA, one distinct colour per column, so the swap is visible in every pixel.
        let source: Vec<u8> = [10u8, 20, 30, 0, 40, 50, 60, 0]
            .iter()
            .copied()
            .cycle()
            .take(4 * 2 * 4)
            .collect();
        let mut frames = preview(8, 8);
        assert!(preview_enabled());

        publish_bgra(&source, 4, 2);

        let frame = frames.try_recv().expect("one frame was offered");
        assert_eq!((frame.width, frame.height), (8, 4), "4:2 -> 8x8 fits at 8x4");
        assert_eq!(frame.rgba.len(), 8 * 4 * 4);
        // The first source pixel is B=10, G=20, R=30; RGBA wants it the other way around.
        assert_eq!(&frame.rgba[0..4], &[30, 20, 10, u8::MAX]);
    }

    #[test]
    fn nothing_is_converted_while_the_preview_is_off() {
        let _guard = exclusive();
        let mut frames = preview(0, 0);
        assert!(!preview_enabled(), "a zero-sized target means no preview");
        publish_bgra(&[0, 0, 0, 0], 1, 1);
        assert!(frames.try_recv().is_err(), "a disabled preview gets no frame");

        // A target with one axis at zero is no target either.
        for (width, height) in [(0, 2), (4, 0)] {
            set_preview_target_size(width, height);
            assert!(!preview_enabled(), "{width}x{height} is not a box");
            publish_bgra(&[0u8; 32], 4, 2);
            assert!(frames.try_recv().is_err(), "{width}x{height} gets no frame");
        }

        // A target nobody is waiting for is no preview either.
        set_preview_target_size(4, 2);
        set_preview_sink(None);
        assert!(!preview_enabled(), "no sink means no preview");
        publish_bgra(&[0u8; 32], 4, 2);
        assert!(frames.try_recv().is_err(), "a dropped sink gets no frame");
    }

    #[test]
    fn a_frame_whose_geometry_does_not_match_its_pixels_is_ignored() {
        let _guard = exclusive();
        let mut frames = preview(64, 64);
        publish_bgra(&[0u8; 15], 4, 4);
        publish_bgra(&[], 0, 0);
        assert!(frames.try_recv().is_err());
    }
}
