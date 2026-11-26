//! TLS client configuration.
//!
//! * **Crypto backend:** `ring`. AES-GCM runs on AES-NI and carry-less-multiply
//!   instructions on x86-64. (`aws-lc-rs` is the other rustls backend and is often a
//!   little faster, but needs `cmake` to build, which this machine doesn't have; swapping
//!   is a one-line change and step 20 can measure it.)
//! * **Cipher preference:** AES-128-GCM first. On CPUs with AES-NI it's the cheapest AEAD
//!   (fewer rounds than AES-256; ChaCha20 wins only without AES hardware). The server
//!   picks among what we offer, so order expresses preference; the safe defaults remain.
//! * **Roots:** Mozilla's CA set compiled in (`webpki-roots`): no dependency on the OS
//!   certificate store, identical behaviour on every machine.
//! * **No ALPN, no client certificate, no session resumption tuning:** one long-lived
//!   connection per venue; the handshake cost is paid once.
//!
//! **rustls does the handshake only (step 24).** Both its buffered and "unbuffered" read
//! APIs hand each decrypted record over as an owned `Vec` (in-place decryption is a TODO
//! in 0.23). So secret extraction is enabled, and after the handshake its
//! `rustls::kernel` API hands the traffic keys to our record layer ([`crate::record`]),
//! which decrypts in place; rustls keeps deriving keys on key updates and storing session
//! tickets. The REST client ([`crate::http`]) still uses rustls end to end (cold path).

use std::sync::Arc;

use rustls::crypto::ring as provider;
use rustls::{ClientConfig, RootCertStore};

/// Cipher suites in order of preference: AES-128-GCM first (TLS 1.3, then 1.2), then the
/// other safe defaults as fallbacks.
fn suites() -> Vec<rustls::SupportedCipherSuite> {
    use provider::cipher_suite::*;
    vec![
        TLS13_AES_128_GCM_SHA256,
        TLS13_AES_256_GCM_SHA384,
        TLS13_CHACHA20_POLY1305_SHA256,
        TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256,
        TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256,
        TLS_ECDHE_ECDSA_WITH_AES_256_GCM_SHA384,
        TLS_ECDHE_RSA_WITH_AES_256_GCM_SHA384,
        TLS_ECDHE_ECDSA_WITH_CHACHA20_POLY1305_SHA256,
        TLS_ECDHE_RSA_WITH_CHACHA20_POLY1305_SHA256,
    ]
}

/// The crypto provider with our cipher preference.
pub fn crypto_provider() -> Arc<rustls::crypto::CryptoProvider> {
    Arc::new(rustls::crypto::CryptoProvider {
        cipher_suites: suites(),
        ..provider::default_provider()
    })
}

/// The production client configuration: public CA roots.
pub fn client_config() -> Result<Arc<ClientConfig>, rustls::Error> {
    let mut roots = RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    client_config_with_roots(roots)
}

/// A client configuration trusting exactly `roots` (tests use a throwaway CA).
pub fn client_config_with_roots(roots: RootCertStore) -> Result<Arc<ClientConfig>, rustls::Error> {
    let mut config = ClientConfig::builder_with_provider(crypto_provider())
        .with_safe_default_protocol_versions()?
        .with_root_certificates(roots)
        .with_no_client_auth();
    // Step 24: after the handshake, rustls hands the traffic keys to our own record layer
    // (rustls::kernel), which decrypts in place. rustls keeps the secrets for key updates.
    config.enable_secret_extraction = true;
    Ok(Arc::new(config))
}

/// Fills `buf` with cryptographically secure random bytes (WebSocket keys and masks).
pub fn random(buf: &mut [u8]) -> Result<(), rustls::Error> {
    provider::default_provider()
        .secure_random
        .fill(buf)
        .map_err(|_| rustls::Error::FailedToGetRandomBytes)
}
