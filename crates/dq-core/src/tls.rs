//! The TLS implementation every connection uses.

use std::sync::Once;

/// Makes ring the process-wide rustls provider. Drivers that build their TLS
/// settings with rustls defaults need exactly one provider, and the
/// workspace links two (one driver brings aws-lc-rs along), so the choice is
/// made explicitly, once, before the first connection.
pub fn install_crypto_provider() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}
