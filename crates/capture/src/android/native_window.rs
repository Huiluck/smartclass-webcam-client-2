//! The `ANativeWindow` handle the camera writes into and the encoder reads from.
//!
//! Both halves of the Android pipeline name this type — the capture session wraps a window in an
//! output, and the encoder hands out the window its codec was configured with — so the opaque
//! type lives here rather than in either of them. Two separate declarations would be the same C
//! object in two incompatible Rust types, which is exactly the kind of mismatch that only shows
//! up as a confusing build error after both sides are written.
//!
//! The reference count is not a Rust `Clone`. `AMediaCodec_createInputSurface` hands the caller
//! one reference, and its header is explicit that the application releases it; the encoder owns
//! that one for as long as it lives (see [`super::media::Encoder::stop`]). The camera session
//! only borrows the pointer, and must not release it.

/// Opaque `ANativeWindow`.
#[repr(C)]
pub(crate) struct ANativeWindow {
    _private: [u8; 0],
}

// `native_window.h` declares these in `libandroid`, which the media stack's `libnativewindow`
// re-exports; one link name is enough.
#[link(name = "android")]
unsafe extern "C" {
    fn ANativeWindow_release(window: *mut ANativeWindow);
}

/// Drops one reference on a window.
///
/// # Safety
///
/// `window` must be a live `ANativeWindow*`, and the caller must own a reference on it — either
/// the one `AMediaCodec_createInputSurface` handed over or one taken with
/// `ANativeWindow_acquire`. Releasing a reference twice, or releasing one the caller does not
/// own, frees a surface that is still in use.
pub(crate) unsafe fn release(window: *mut ANativeWindow) {
    // SAFETY: the caller guarantees the pointer is live and that it owns one reference.
    unsafe { ANativeWindow_release(window) };
}
