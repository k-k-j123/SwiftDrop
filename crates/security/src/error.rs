//! Error type for the security crate.
//!
//! These errors stay **local**: they are logged and shown to the local
//! user, but their `Display` text is never sent over the wire (it may
//! contain file paths from the config directory — spec NFR-SEC-007).

use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum SecurityError {
    /// Key or certificate file could not be read/written.
    #[error("identity storage error at {path}: {source}")]
    IdentityIo {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    /// The stored key/cert is corrupt or unusable.
    #[error("stored device identity is invalid: {0}")]
    IdentityInvalid(String),

    /// Certificate generation failed (should not happen with our inputs).
    #[error("certificate generation failed: {0}")]
    Certificate(String),

    /// TLS configuration or handshake problem.
    #[error("TLS error: {0}")]
    Tls(String),

    /// The peer's `auth_sig` did not verify against its claimed key.
    #[error("peer failed to prove possession of its identity key")]
    AuthSignatureInvalid,

    /// The claimed `device_id` does not match the claimed public key
    /// (i.e. the peer is lying about its own identity hash).
    #[error("claimed device_id does not match claimed device key")]
    IdentityMismatch,

    /// Key bytes could not be parsed (corrupt message or wrong curve).
    #[error("invalid device key material: {0}")]
    DeviceKeyInvalid(String),

    /// Signature bytes could not be parsed/verified.
    #[error("invalid signature: {0}")]
    SignatureInvalid(String),

    /// trusted_devices.json is unreadable or malformed.
    #[error("trust store error: {0}")]
    TrustStore(String),

    /// Could not read TLS exported keying material (handshake state).
    #[error("TLS keying material unavailable: {0}")]
    Exporter(String),
}
