pub mod api;
mod frb_generated;

/// Installs the `log` backend once, so the core's diagnostics are readable.
///
/// The core reports registration rejections, retry delays and session loss through `tracing`.
/// In an app process none of that is visible: the library has no subscriber, and even a
/// `tracing_subscriber` writing to standard error would go nowhere, because an Android app's
/// streams are not attached to logcat. `tracing`'s `log` feature (enabled for the workspace)
/// turns every event into a `log` record, and `android_logger` hands those to logcat — which is
/// where an operator can actually read them. The records land under the emitting module's path,
/// such as `scwc_core::supervisor`, because the logger's tag applies to `log` records made here.
///
/// The first caller wins and later ones are no-ops, so the app may call this on each start.
pub fn init_logging() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        // A panic inside a spawned task is caught by the runtime and, with nobody awaiting the
        // handle, its message goes to standard error — which an app process discards. Routing it
        // through the same logger is the only way it becomes visible.
        std::panic::set_hook(Box::new(|info| {
            log::error!("rust panic: {info}");
        }));
        #[cfg(target_os = "android")]
        android_logger::init_once(
            android_logger::Config::default()
                .with_max_level(log::LevelFilter::Debug)
                .with_tag("scwc"),
        );
        #[cfg(not(target_os = "android"))]
        {
            // Nothing to install: the desktop shells (CLI, e2e harness) set up their own
            // subscriber, and standard error is visible where this runs.
        }
    });
}
