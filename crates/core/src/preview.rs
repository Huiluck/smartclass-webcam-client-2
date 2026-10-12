//! Live preview: the camera picture an embedding application paints.
//!
//! A consumer picks one of two paths. The simple one takes finished frames — it registers a sink
//! and the pixel size of the area it wants the picture in ([`set_preview_sink`],
//! [`set_preview_target_size`]), the capture layer asks [`next_frame_size`] where to render the
//! next frame and calls [`publish_bgra`] with it, and the frame is converted to RGBA and scaled
//! *aspect-locked inside* the requested size right here, so the frames themselves carry no
//! letterboxing and the consumer only has to centre what it gets. Everything on that path is
//! converted and copied, so it suits the consumers that cannot take a platform buffer.
//!
//! The other path takes the platform's own buffer ([`publish_native_frame`],
//! [`set_native_frame_listener`]) and is what a renderer that understands it wants: the newest
//! frame sits in [`native_frame`], retained, until the consumer renders it or a newer frame
//! replaces it, and nothing is converted, scaled or copied on the way. This module never touches
//! the buffer itself — it is an opaque pointer here — so the platforms keep their own retain and
//! release.
//!
//! The RGBA path is consumer-paced. [`next_frame_size`] hands out the size of the next frame only
//! while the consumer is ready for it — no sink, no target box, or a frame the consumer has not
//! reported through [`preview_frame_consumed`] all mean no frame at all — so a producer never
//! renders, converts or scales anything a consumer cannot keep up with. The Apple shim renders at
//! exactly that size, which is what keeps a preview from ever costing the full capture
//! resolution. The native path needs no pacing: the consumer renders the newest buffer whenever
//! it likes, and the frames it misses cost a retain and a release.
//!
//! Nothing on either path can fail the capture: a frame nobody consumes is dropped, and no frame
//! is produced at all while the consumer shows no preview.

use std::ffi::c_void;
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, Ordering};
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

/// Whether the consumer is ready for another frame.
///
/// Taken when a frame is offered ([`publish_bgra`]) and handed back when the consumer says it is
/// done with it ([`preview_frame_consumed`]) — or when a sink is registered, or a target size is
/// set, both of which mean nothing is in flight and one frame may go out. This is the whole
/// pacing mechanism: a consumer that is behind simply finds the permit gone, and the capture
/// path does no preview work at all until it is back.
static PERMIT: AtomicBool = AtomicBool::new(true);

/// The newest camera frame published as the platform's own buffer, or null while there is none.
///
/// Opaque here on purpose: this module stores the pointer and hands it on, and the platform that
/// produced the buffer is the one that retains and releases it.
static NATIVE_FRAME: AtomicPtr<c_void> = AtomicPtr::new(ptr::null_mut());

/// The consumer told when a native frame is published, if one is registered.
static NATIVE_LISTENER: Mutex<Option<fn()>> = Mutex::new(None);

/// Whether a native-frame consumer is registered, mirrored out of the mutex so that a capture
/// callback can ask without taking a lock on every frame.
static NATIVE_ACTIVE: AtomicBool = AtomicBool::new(false);

/// Registers the sink the live preview is offered to; `None` stops it.
///
/// Replacing a sink drops the previous one, which ends the receiver its consumer awaits, and
/// arms the next frame: nothing is in flight for a consumer that just arrived. Every frame is
/// offered without waiting, so a slow consumer loses frames rather than slowing the camera down.
pub fn set_preview_sink(sink: Option<UnboundedSender<PreviewFrame>>) {
    *sink_slot() = sink;
    arm();
}

/// Sets the pixel size the preview frames are scaled to fit; `0x0` disables the preview.
///
/// The picture keeps its aspect ratio and is scaled to fit *inside* this box, so the frames a
/// caller receives are as tall or as wide as the camera's own picture allows — never the box
/// itself with the letterbox baked in. Like a new sink, this arms the next frame: a caller that
/// resizes its preview is laying it out again and wants a frame for it.
pub fn set_preview_target_size(width: u32, height: u32) {
    TARGET_WIDTH.store(width, Ordering::Relaxed);
    TARGET_HEIGHT.store(height, Ordering::Relaxed);
    arm();
}

/// Whether a preview is configured at all: a sink is registered and a target size is set.
///
/// This says nothing about whether a frame may be produced *now* — a consumer that has not
/// reported the frame it was handed is behind — so a producer asks [`next_frame_size`] rather
/// than this.
pub fn preview_enabled() -> bool {
    target_size().is_some() && sink_slot().is_some()
}

/// The size to render the next frame at, or `None` when no frame may be produced now.
///
/// `None` means one of three things, and a producer that gets it does nothing at all: no sink is
/// registered, no target box is set, or the consumer has not finished with the frame it was
/// handed. A frame without pixels answers `None` too, since there is nothing to render from it.
/// Otherwise the answer is the size to produce the frame *at*: `source_width` and
/// `source_height` are the frame the producer holds, and the result is that frame's aspect
/// ratio fitted inside the target box — so a producer that renders at this size does the work
/// of a preview and never the work of a full-resolution capture frame.
pub fn next_frame_size(source_width: u32, source_height: u32) -> Option<(u32, u32)> {
    if source_width == 0 || source_height == 0 {
        return None;
    }
    if !preview_enabled() || !permit_available() {
        return None;
    }

    let (target_width, target_height) = target_size()?;
    Some(fit(source_width, source_height, target_width, target_height))
}

/// The consumer is done with the frame it was handed, so one more may be produced.
///
/// The other half of the pacing [`next_frame_size`] starts: without this, exactly one frame
/// every time a sink or a target size is set, and nothing while the consumer is behind.
pub fn preview_frame_consumed() {
    arm();
}

/// Converts one captured frame and offers it to the sink.
///
/// `pixels` is the frame as the backend hands it over: tightly packed BGRA, four bytes per
/// pixel, row-major, `width * height` of them. The frame is scaled aspect-locked into the
/// target size, its channels swapped to RGBA, and offered without waiting — a caller that is
/// behind loses this frame instead of holding up the capture.
///
/// Does nothing while no frame may be produced — see [`next_frame_size`] — which is what lets a
/// producer call this unconditionally.
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
    // The frame is a frame, so it takes the consumer's permit — and gives it back only when the
    // consumer reports it. Until then, nothing more is produced for it, exactly as if the queue
    // were full.
    if !consume_permit() {
        return;
    }

    let (out_width, out_height) = fit(source.width, source.height, target_width, target_height);
    let rgba = source.scale_to(out_width, out_height);
    // The channel is unbounded, so this only fails once the consumer is gone.
    let _ = sender.send(PreviewFrame {
        rgba,
        width: out_width,
        height: out_height,
    });
}

/// Registers the consumer told when a new native frame is published; `None` stops it.
///
/// A native consumer is what [`native_preview_active`] reports and what [`publish_native_frame`]
/// notifies; the buffer itself is the consumer's to take with [`native_frame`]. Registering a
/// listener does not publish a frame: the next captured one arrives within a frame time.
pub fn set_native_frame_listener(listener: Option<fn()>) {
    *listener_slot() = listener;
    NATIVE_ACTIVE.store(listener.is_some(), Ordering::Release);
}

/// Whether a consumer of native frames is registered.
///
/// Cheap enough for a capture callback to check per frame, which is the point: a pipeline that
/// publishes native frames does the retain and the release only while something takes them.
pub fn native_preview_active() -> bool {
    NATIVE_ACTIVE.load(Ordering::Acquire)
}

/// Publishes `frame` as the newest native frame, returning the one it replaced.
///
/// `frame` is a platform buffer the caller has retained for the slot: this module only keeps the
/// pointer, and hands the consumer the same pointer through [`native_frame`]. A `Some` answer is
/// the buffer this publish replaced, whose reference is the caller's to release through its own
/// platform API — nothing is released here, because nothing here knows what a refcount is. The
/// registered listener, if any, is told once.
pub fn publish_native_frame(frame: *mut c_void) -> Option<*mut c_void> {
    let replaced = NATIVE_FRAME.swap(frame, Ordering::AcqRel);
    let listener = *listener_slot();
    if let Some(listener) = listener {
        listener();
    }
    (!replaced.is_null()).then_some(replaced)
}

/// The newest native frame, or null while none has been published.
///
/// The pointer is the published one, not a reference of the caller's: a consumer that keeps it
/// past the next publish has to retain it through its own platform API.
pub fn native_frame() -> *mut c_void {
    NATIVE_FRAME.load(Ordering::Acquire)
}

/// Removes the newest native frame, handing its reference to the caller to release.
///
/// This is what a consumer calls when it stops: afterwards no frame is published until the next
/// capture frame arrives.
pub fn take_native_frame() -> Option<*mut c_void> {
    let frame = NATIVE_FRAME.swap(ptr::null_mut(), Ordering::AcqRel);
    (!frame.is_null()).then_some(frame)
}

/// The listener slot, with a poisoned lock treated as an empty one.
fn listener_slot() -> MutexGuard<'static, Option<fn()>> {
    NATIVE_LISTENER.lock().unwrap_or_else(|error| error.into_inner())
}

/// Arms the next frame: nothing is in flight, so one more may be produced.
fn arm() {
    PERMIT.store(true, Ordering::Release);
}

/// Whether the consumer is ready for a frame.
fn permit_available() -> bool {
    PERMIT.load(Ordering::Acquire)
}

/// Takes the consumer's permit, or `false` when it is not ready for another frame.
fn consume_permit() -> bool {
    PERMIT
        .compare_exchange(true, false, Ordering::AcqRel, Ordering::Acquire)
        .is_ok()
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
        fit, native_frame, native_preview_active, next_frame_size, preview_enabled,
        preview_frame_consumed, publish_bgra, publish_native_frame, set_native_frame_listener,
        set_preview_sink, set_preview_target_size, take_native_frame, PreviewFrame,
    };
    use std::ptr;
    use std::sync::atomic::{AtomicUsize, Ordering};
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
        set_native_frame_listener(None);
        // The frames these tests publish are invented pointers, and the platform that would own
        // their references is not here, so the slot is simply emptied.
        take_native_frame();
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
    fn a_frame_is_only_produced_while_the_consumer_is_ready() {
        let _guard = exclusive();
        let mut frames = preview(8, 8);
        // What a producer renders at: the source's aspect ratio inside the box, not the source.
        let frame = vec![0u8; 8 * 4 * 4];

        assert_eq!(next_frame_size(4, 2), Some((8, 4)));
        publish_bgra(&frame, 8, 4);
        assert!(frames.try_recv().is_ok(), "the first frame goes out");

        // The consumer has it, so nothing is produced and nothing is even rendered until it
        // says it is done.
        assert_eq!(next_frame_size(4, 2), None, "no permit, no size to render at");
        publish_bgra(&frame, 8, 4);
        assert!(frames.try_recv().is_err(), "a busy consumer gets no frame");

        preview_frame_consumed();
        assert_eq!(next_frame_size(4, 2), Some((8, 4)));
        publish_bgra(&frame, 8, 4);
        assert!(frames.try_recv().is_ok(), "the next frame goes out");
    }

    #[test]
    fn a_new_sink_or_a_resize_arms_the_next_frame() {
        let _guard = exclusive();
        let mut frames = preview(8, 8);
        let frame = vec![0u8; 16 * 8 * 4];

        publish_bgra(&frame, 8, 4);
        assert!(frames.try_recv().is_ok());
        assert_eq!(next_frame_size(4, 2), None, "the consumer has that one");

        // A caller that resizes its preview is laying it out again: one frame may go out, at
        // the new box's size.
        set_preview_target_size(16, 16);
        assert_eq!(next_frame_size(4, 2), Some((16, 8)));

        // So is a caller that registers a sink.
        publish_bgra(&frame, 16, 8);
        assert!(frames.try_recv().is_ok());
        let (sender, mut replacement) = tokio::sync::mpsc::unbounded_channel();
        set_preview_sink(Some(sender));
        assert_eq!(next_frame_size(4, 2), Some((16, 8)));
        publish_bgra(&frame, 16, 8);
        assert!(replacement.try_recv().is_ok(), "the new sink gets a frame");
    }

    #[test]
    fn a_frame_whose_geometry_does_not_match_its_pixels_is_ignored() {
        let _guard = exclusive();
        let mut frames = preview(64, 64);
        publish_bgra(&[0u8; 15], 4, 4);
        publish_bgra(&[], 0, 0);
        assert!(frames.try_recv().is_err());
    }

    #[test]
    fn a_native_frame_replaces_the_one_before_it_and_is_handed_back_to_be_released() {
        let _guard = exclusive();
        static NOTIFIED: AtomicUsize = AtomicUsize::new(0);
        fn notify() {
            NOTIFIED.fetch_add(1, Ordering::Relaxed);
        }

        // Nothing consumes native frames until a listener is registered, and that is what the
        // capture path asks before it retains anything.
        assert!(!native_preview_active());
        assert_eq!(native_frame(), ptr::null_mut());

        set_native_frame_listener(Some(notify));
        assert!(native_preview_active());

        let first = 0x1000usize as *mut std::ffi::c_void;
        let second = 0x2000usize as *mut std::ffi::c_void;
        assert_eq!(
            publish_native_frame(first),
            None,
            "nothing was published before"
        );
        assert_eq!(NOTIFIED.load(Ordering::Relaxed), 1, "the listener was told");
        assert_eq!(native_frame(), first);

        // A newer frame takes the slot, and the one it took is handed back for the publisher to
        // release through its own platform API.
        assert_eq!(publish_native_frame(second), Some(first));
        assert_eq!(NOTIFIED.load(Ordering::Relaxed), 2);
        assert_eq!(native_frame(), second);

        // Taking the newest frame empties the slot, so nothing is handed out twice.
        assert_eq!(take_native_frame(), Some(second));
        assert_eq!(native_frame(), ptr::null_mut());
        assert_eq!(take_native_frame(), None);

        // A publisher that keeps publishing while nothing listens swaps the slot but tells
        // nobody — which is why it asks `native_preview_active` first.
        set_native_frame_listener(None);
        assert!(!native_preview_active());
        assert_eq!(publish_native_frame(first), None);
        assert_eq!(NOTIFIED.load(Ordering::Relaxed), 2, "no listener, no notice");
        assert_eq!(take_native_frame(), Some(first));
    }
}
