pub mod api;
mod frb_generated;

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
