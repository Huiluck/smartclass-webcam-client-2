pub mod api;
mod frb_generated;

/// The native texture path: the app's macOS preview plugin paints the camera's own buffers, so a
/// preview costs no conversion and no copy at all.
///
/// The glue is a C ABI rather than the bridge on purpose. The plugin runs in this process, the
/// buffer never crosses into Dart, and the FRB surface would only add a copy and a Dart object to
/// every frame — the plugin's texture asks [`scwc_preview_latest_frame`] for the newest retained
/// buffer and is told when there is one through the callback it registers.
#[cfg(target_os = "macos")]
mod preview_texture {
    use std::ffi::c_void;
    use std::ptr;
    use std::sync::Mutex;

    /// The plugin's "a new frame is ready" callback, or `None` while no texture is registered.
    static FRAME_READY: Mutex<Option<extern "C" fn()>> = Mutex::new(None);

    /// Tells the registered plugin that the newest buffer is ready to be painted.
    fn announce() {
        let callback = *FRAME_READY.lock().unwrap_or_else(|error| error.into_inner());
        if let Some(callback) = callback {
            callback();
        }
    }

    /// Registers the callback the capture path tells when a new frame is ready, or clears it.
    ///
    /// # Safety
    ///
    /// `callback` is either null — which stops the path and releases the frame it was holding —
    /// or a function that stays alive while it is registered. It is called from the capture
    /// thread, once per captured frame, so it must return promptly.
    #[no_mangle]
    pub unsafe extern "C" fn scwc_preview_register_frame_callback(
        callback: Option<extern "C" fn()>,
    ) {
        *FRAME_READY.lock().unwrap_or_else(|error| error.into_inner()) = callback;
        if callback.is_none() {
            // Stop listening before the last frame goes: a texture that is registered again
            // starts from the frames captured after it, not from one painted before it.
            scwc_core::preview::set_native_frame_listener(None);
            if let Some(frame) = scwc_core::preview::take_native_frame() {
                // SAFETY: the slot handed over the one reference it owned, and the registered
                // path is the only other holder, which has just been cleared.
                unsafe { scwc_capture::release_frame_buffer(frame) };
            }
        } else {
            scwc_core::preview::set_native_frame_listener(Some(announce));
            // The two paths are alternatives, and this one is the cheaper: a consumer that takes
            // platform buffers wants no frames converted for it, so the stream path is switched
            // off here rather than left to convert every frame nobody reads. A later layout from
            // a client that shows the stream anyway sets a box again and re-enables it.
            scwc_core::preview::set_preview_target_size(0, 0);
        }
    }

    /// The newest camera frame as a retained platform buffer, or null while there is none.
    ///
    /// # Safety
    ///
    /// The answer is either null or a `CVPixelBufferRef` with one reference for the caller to
    /// release; the pointer stays valid until that release, and is already null-safe.
    #[no_mangle]
    pub unsafe extern "C" fn scwc_preview_latest_frame() -> *mut c_void {
        let frame = scwc_core::preview::native_frame();
        if frame.is_null() {
            return ptr::null_mut();
        }
        // SAFETY: the slot holds a live reference to this buffer, so retaining it gives the
        // caller one of its own.
        unsafe { scwc_capture::retain_frame_buffer(frame) }
    }
}

/// Installs the process-wide rustls crypto provider the `wss://` attach needs.
///
/// rustls 0.23 builds a TLS configuration only when it can tell which crypto provider to use,
/// and it cannot tell when the crate was compiled with no provider feature at all — which is
/// how `tokio-tungstenite` and `tungstenite` take their rustls dependency, and therefore how
/// the device WebSocket would get it. `scwc-core` selects `ring` for that reason (see its
/// `Cargo.toml`); installing it here makes the choice explicit and independent of every other
/// crate in the graph, so the first `wss://` attach cannot panic on it.
///
/// Called from the bridge's own init, and idempotent: an install that finds a provider already
/// in place — a second init in a test, an application that calls this itself — keeps the first
/// one and is not an error.
pub fn install_crypto_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}
