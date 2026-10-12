#[flutter_rust_bridge::frb(sync)] // Synchronous mode for simplicity of the demo
pub fn greet(name: String) -> String {
    format!("Hello, {name}!")
}

#[flutter_rust_bridge::frb(init)]
pub fn init_app() {
    // The `wss://` attach builds its TLS configuration on the process-wide rustls provider, so
    // the one this crate selects is installed before anything can connect. Idempotent, because
    // the bridge may be initialized more than once.
    crate::install_crypto_provider();
    // Default utilities - feel free to customize
    flutter_rust_bridge::setup_default_user_utils();
}
