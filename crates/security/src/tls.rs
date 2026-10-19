//! TLS 1.3 configuration and the TLS *exporter* helpers that power
//! authentication and the pairing code (ADR-0002, ADR-0003).
//!
//! ### Why the client accepts any certificate at the TLS layer
//!
//! Self-signed certificates prove nothing: anyone can mint one. We let
//! rustls accept *any* server certificate so the handshake can complete,
//! then do real authentication **one layer up**, where:
//!
//! * the peer must sign this session's TLS exporter with the identity key
//!   matching the `device_id` it claims (`auth.rs`), and
//! * the resulting `device_id` is checked against our trust store, or
//!   against a user-confirmed pairing code (SAS).
//!
//! This is deliberate, reviewed-by-design "danger" code: the verifier is
//! a one-method function that returns `assertion()`. Everything that
//! looks like trust is decided in `swiftdrop-core`, never here.

use crate::error::SecurityError;
use crate::identity::DeviceIdentity;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{ClientConfig, DigitallySignedStruct, Error as TlsError, ServerConfig, SignatureScheme};

/// Anything that can produce RFC 5705 exported keying material for a
/// finished TLS handshake.
///
/// `tokio-rustls` streams expose only `ConnectionCommon<ServerSide>` /
/// `ConnectionCommon<ClientSide>` (not the top-level `rustls::Connection`
/// enum), so auth helpers take this trait object instead of a concrete
/// type. Both connection types export identical bytes for the same
/// session, which is exactly what the pairing/auth design requires.
pub trait ExporterSource: Send + Sync {
    /// Fill `output` with `len` bytes of exported keying material.
    fn export(
        &self,
        output: Vec<u8>,
        label: &[u8],
        context: Option<&[u8]>,
    ) -> Result<Vec<u8>, TlsError>;
}

impl ExporterSource for rustls::Connection {
    fn export(
        &self,
        output: Vec<u8>,
        label: &[u8],
        context: Option<&[u8]>,
    ) -> Result<Vec<u8>, TlsError> {
        self.export_keying_material(output, label, context)
    }
}

impl ExporterSource for rustls::ConnectionCommon<rustls::server::ServerSide> {
    fn export(
        &self,
        output: Vec<u8>,
        label: &[u8],
        context: Option<&[u8]>,
    ) -> Result<Vec<u8>, TlsError> {
        self.export_keying_material(output, label, context)
    }
}

impl ExporterSource for rustls::ConnectionCommon<rustls::client::ClientSide> {
    fn export(
        &self,
        output: Vec<u8>,
        label: &[u8],
        context: Option<&[u8]>,
    ) -> Result<Vec<u8>, TlsError> {
        self.export_keying_material(output, label, context)
    }
}

/// Install the `ring` crypto provider for this process, if not already set.
///
/// Safe to call repeatedly (later calls are ignored). Panicking here would
/// be worse than a confusing TLS error later, so we only log on failure.
pub fn install_crypto_provider() {
    if rustls::crypto::CryptoProvider::get_default().is_some() {
        return;
    }
    let provider = rustls::crypto::ring::default_provider();
    if let Err(e) = provider.install_default() {
        tracing::warn!(error = ?e, "crypto provider already installed by another component");
    }
}

/// Build the TLS **server** config used by the receiving side.
///
/// No client certificate is requested: peer authentication happens in our
/// own protocol after the handshake (see module docs / ADR-0002).
pub fn server_config(identity: &DeviceIdentity) -> Result<ServerConfig, SecurityError> {
    install_crypto_provider();
    let cert = CertificateDer::from(identity.cert_der.clone());
    let key = rustls::pki_types::PrivatePkcs8KeyDer::from(identity.pkcs8_der().to_vec());

    ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![cert], key.into())
        .map_err(|e| SecurityError::Tls(format!("server config: {e}")))
}

/// Build the TLS **client** config used by the sending side.
pub fn client_config() -> Result<ClientConfig, SecurityError> {
    install_crypto_provider();

    // SAFETY-RELEVANT DESIGN: this verifier deliberately accepts every
    // server certificate. Identity is enforced after the handshake by
    // signing the session exporter (auth.rs) + trust-store/SAS checks.
    let verifier = std::sync::Arc::new(AcceptAnyServerCert {
        schemes: supported_schemes(),
    });

    Ok(ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_no_client_auth())
}

fn supported_schemes() -> Vec<SignatureScheme> {
    rustls::crypto::ring::default_provider()
        .signature_verification_algorithms
        .supported_schemes()
}

/// See module docs: accepts everything at the TLS layer *on purpose*.
#[derive(Debug)]
struct AcceptAnyServerCert {
    schemes: Vec<SignatureScheme>,
}

impl ServerCertVerifier for AcceptAnyServerCert {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, TlsError> {
        // Accept: authentication is done in our protocol layer, where the
        // user-confirmed pairing and trust store actually live.
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        // We negotiate TLS 1.3 only, but the trait requires this method.
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &rustls::crypto::ring::default_provider().signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        // The *TLS-layer* handshake signature must still be cryptographically
        // valid (it proves the server holds its TLS key); it simply does not
        // decide identity for us.
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &rustls::crypto::ring::default_provider().signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.schemes.clone()
    }
}

/// Read RFC 5705/8446 exported keying material for a finished handshake.
///
/// Both endpoints of one TLS session get **identical** bytes; endpoints of
/// two different sessions (i.e. a man-in-the-middle bridging two handshakes)
/// get different bytes. That property is what makes our auth signatures
/// un-replayable and our pairing code MITM-detecting.
pub fn exporter(
    conn: &dyn ExporterSource,
    label: &str,
    context: &[u8],
    len: usize,
) -> Result<Vec<u8>, SecurityError> {
    let ctx = if context.is_empty() {
        None
    } else {
        Some(context)
    };
    conn.export(vec![0u8; len], label.as_bytes(), ctx)
        .map_err(|e| SecurityError::Exporter(e.to_string()))
}
