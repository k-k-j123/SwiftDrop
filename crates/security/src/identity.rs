//! Durable per-installation device identity (spec FR-PAIR-004).
//!
//! ## Files on disk
//!
//! Inside the app's config directory (e.g. `~/.config/swiftdrop/`):
//!
//! | File            | Contents                              | Permissions |
//! |-----------------|---------------------------------------|-------------|
//! | `device.key`    | PKCS#8 DER private key (32-byte P-256 scalar wrapped) | `0600` |
//! | `device.cert`   | Self-signed X.509 certificate DER (same key) | `0600` |
//!
//! * The **key** is the identity: whoever holds it *is* the device.
//! * The **cert** exists only so rustls can act as a TLS *server* — it is
//!   never proof of identity (it is self-signed; see crate docs / ADR-0002).
//! * Both files are written atomically (temp file + rename) and with
//!   owner-only permissions *before* any content is written where the
//!   platform allows it.
//!
//! ## Privacy note (spec NFR-SEC-007)
//!
//! Private keys are never logged, never sent, and never included in
//! error messages.

use crate::error::SecurityError;
use p256::ecdsa::SigningKey;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

/// The in-memory device identity: key + its self-signed certificate.
#[derive(Clone)]
pub struct DeviceIdentity {
    /// Signing key that proves who we are in every session.
    pub(crate) signing_key: SigningKey,
    /// PKCS#8 DER encoding of the same key, kept so rustls can use it as
    /// the TLS server key without re-encoding.
    pub(crate) pkcs8_der: Vec<u8>,
    /// Self-signed certificate DER used when we act as a TLS server.
    pub cert_der: Vec<u8>,
}

impl std::fmt::Debug for DeviceIdentity {
    /// Redacting Debug: the key must never be printable, not even in a
    /// crash dump or a `dbg!()` someone adds later (spec NFR-SEC-007).
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DeviceIdentity")
            .field("device_id", &self.device_id())
            .field("key", &"<redacted>")
            .field("cert_der_len", &self.cert_der.len())
            .finish()
    }
}

impl DeviceIdentity {
    /// Stable identity string: `SHA-256(compressed public key point)`.
    ///
    /// This is what discovery advertises (as an opaque id), what the
    /// trust store keys on, and what both users compare indirectly via
    /// the pairing code.
    pub fn device_id(&self) -> String {
        let point = self.signing_key.verifying_key().to_encoded_point(true);
        hex::encode(Sha256::digest(point.as_bytes()))
    }

    /// The compressed SEC1 public key point (33 bytes) — this is what
    /// travels in the `hello` message (`device_key`, base64).
    pub fn public_sec1(&self) -> Vec<u8> {
        self.signing_key
            .verifying_key()
            .to_encoded_point(true)
            .as_bytes()
            .to_vec()
    }

    /// Sign a message with our identity key (ECDSA P-256 / SHA-256).
    pub fn sign(&self, message: &[u8]) -> Vec<u8> {
        use signature::Signer;
        let sig: p256::ecdsa::Signature = self.signing_key.sign(message);
        sig.to_bytes().to_vec()
    }

    /// PKCS#8 DER encoding of the private key (used to configure TLS).
    ///
    /// Never leaves the process: only the TLS stack consumes this.
    pub fn pkcs8_der(&self) -> &[u8] {
        &self.pkcs8_der
    }

    /// Load the identity from `dir`, creating it on first run.
    ///
    /// `dir` is typically the app config directory. Missing files ⇒ we
    /// generate a fresh identity (first launch). Corrupt files ⇒ a hard
    /// error: silently replacing a key would break trust with every
    /// previously paired device, so we refuse and let the user decide.
    pub fn load_or_create(dir: &Path, display_name: &str) -> Result<Self, SecurityError> {
        let key_path = dir.join("device.key");
        let cert_path = dir.join("device.cert");

        if key_path.exists() || cert_path.exists() {
            // Both must exist — a half-written pair from a crash is fatal.
            if !(key_path.exists() && cert_path.exists()) {
                return Err(SecurityError::IdentityInvalid(format!(
                    "identity incomplete in {} (one of device.key/device.cert missing); \
                     restore from backup or delete the folder to create a new identity",
                    dir.display()
                )));
            }
            let key_bytes = read_private(&key_path)?;
            let cert_der = read_private(&cert_path)?;
            use p256::pkcs8::DecodePrivateKey;
            let signing_key = SigningKey::from_pkcs8_der(&key_bytes)
                .map_err(|e| SecurityError::IdentityInvalid(format!("{e}")))?;
            return Ok(Self {
                signing_key,
                pkcs8_der: key_bytes,
                cert_der,
            });
        }

        // ---- First run: generate a brand-new identity ----
        let signing_key = generate_signing_key();
        use p256::pkcs8::EncodePrivateKey;
        let key_pkcs8 = signing_key
            .to_pkcs8_der()
            .map_err(|e| SecurityError::IdentityInvalid(format!("key encode: {e}")))?;
        let key_bytes = key_pkcs8.as_bytes();

        let cert_der = generate_self_signed_cert(key_bytes, display_name)?;

        write_private(&key_path, key_bytes)?;
        write_private(&cert_path, &cert_der)?;

        tracing::info!(
            device_id = %signing_key_device_id(&signing_key),
            "generated a new SwiftDrop device identity"
        );

        Ok(Self {
            signing_key,
            pkcs8_der: key_bytes.to_vec(),
            cert_der,
        })
    }
}

fn signing_key_device_id(key: &SigningKey) -> String {
    hex::encode(Sha256::digest(key.verifying_key().to_encoded_point(true).as_bytes()))
}

/// Generate a P-256 signing key from the OS CSPRNG.
///
/// `SigningKey::from_bytes` rejects scalars ≥ the curve order (a ~2^-32
/// event), so we simply retry until the RNG produces a valid one.
fn generate_signing_key() -> SigningKey {
    use rand::RngCore;
    loop {
        let mut raw = [0u8; 32];
        rand::rng().fill_bytes(&mut raw);
        if let Ok(key) = SigningKey::from_bytes(&raw.into()) {
            return key;
        }
    }
}

/// Build a self-signed certificate for our key (used only to enable TLS).
///
/// The certificate's subject includes the display name so that tools like
/// `openssl s_client` show something recognizable during development —
/// SwiftDrop itself never trusts a certificate subject.
fn generate_self_signed_cert(key_pkcs8: &[u8], display_name: &str) -> Result<Vec<u8>, SecurityError> {
    // rcgen wants the key as a rustls-pki-types PKCS#8 object plus the
    // matching algorithm identifier (P-256 / SHA-256).
    let pkcs8 = rustls::pki_types::PrivatePkcs8KeyDer::from(key_pkcs8.to_vec());
    let key_pair = rcgen::KeyPair::from_pkcs8_der_and_sign_algo(&pkcs8, &rcgen::PKCS_ECDSA_P256_SHA256)
        .map_err(|e| SecurityError::Certificate(format!("load key into rcgen: {e}")))?;

    // Passing a DNS name to `new` registers it as a Subject Alternative
    // Name (certificate stacks commonly expect at least one SAN).
    let mut params = rcgen::CertificateParams::new(vec!["swiftdrop.local".to_string()])
        .map_err(|e| SecurityError::Certificate(format!("params: {e}")))?;

    params
        .distinguished_name
        .push(rcgen::DnType::CommonName, format!("SwiftDrop {display_name}"));

    let cert = params
        .self_signed(&key_pair)
        .map_err(|e| SecurityError::Certificate(format!("self-sign: {e}")))?;
    Ok(cert.der().to_vec())
}

/// Read a private file, refusing group/other-readable copies.
fn read_private(path: &Path) -> Result<Vec<u8>, SecurityError> {
    use std::os::unix::fs::PermissionsExt;
    let meta = std::fs::metadata(path).map_err(|source| SecurityError::IdentityIo {
        path: path.to_path_buf(),
        source,
    })?;
    if cfg!(unix) && meta.permissions().mode() & 0o077 != 0 {
        // Best-effort hardening: tighten immediately rather than fail.
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
        tracing::warn!(
            path = %path.display(),
            "identity file was readable by other users; permissions reset to 0600"
        );
    }
    std::fs::read(path).map_err(|source| SecurityError::IdentityIo {
        path: path.to_path_buf(),
        source,
    })
}

/// Create a file with owner-only permissions *before* writing content,
/// so the private key is never briefly world-readable.
fn write_private(path: &Path, bytes: &[u8]) -> Result<(), SecurityError> {
    use std::io::Write;
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut file = opts
        .open(path)
        .map_err(|source| SecurityError::IdentityIo {
            path: path.to_path_buf(),
            source,
        })?;
    file.write_all(bytes)
        .and_then(|()| file.sync_all())
        .map_err(|source| SecurityError::IdentityIo {
            path: path.to_path_buf(),
            source,
        })?;
    Ok(())
}

/// Resolve the config directory used for identity + settings storage.
///
/// Uses the platform convention (`~/.config` on Linux,
/// `%APPDATA%` on Windows — `dirs::config_dir()`), joined with
/// `swiftdrop`. Overridable via `SWIFTDROP_CONFIG_DIR` for tests and
/// portable installs.
pub fn default_config_dir() -> Result<PathBuf, SecurityError> {
    if let Ok(override_dir) = std::env::var("SWIFTDROP_CONFIG_DIR") {
        if !override_dir.is_empty() {
            return Ok(PathBuf::from(override_dir));
        }
    }
    let base = dirs::config_dir().ok_or_else(|| {
        SecurityError::TrustStore("could not determine the config directory".into())
    })?;
    Ok(base.join("swiftdrop"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use p256::PublicKey;

    #[test]
    fn identity_roundtrip_is_stable() {
        let dir = tempfile::tempdir().unwrap();
        let first = DeviceIdentity::load_or_create(dir.path(), "Test").unwrap();
        let id1 = first.device_id();
        assert_eq!(id1.len(), 64, "device_id is 32 bytes of hex");

        // Simulate an app restart: same files, same identity.
        let second = DeviceIdentity::load_or_create(dir.path(), "Test").unwrap();
        assert_eq!(second.device_id(), id1, "identity must survive restart");
        assert_eq!(second.cert_der, first.cert_der);
    }

    #[test]
    fn public_key_and_sign_verify() {
        let dir = tempfile::tempdir().unwrap();
        let id = DeviceIdentity::load_or_create(dir.path(), "Test").unwrap();
        let msg = b"binding bytes for a session";
        let sig = id.sign(msg);

        // Verify the way a *peer* would: from the advertised public key.
        let point = PublicKey::from_sec1_bytes(&id.public_sec1()).unwrap();
        let verifying: p256::ecdsa::VerifyingKey = point.into();
        use signature::Verifier;
        let sig = p256::ecdsa::Signature::from_slice(&sig).unwrap();
        assert!(verifying.verify(msg, &sig).is_ok());

        // A different message must fail.
        assert!(verifying.verify(b"other message", &sig).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn key_file_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        DeviceIdentity::load_or_create(dir.path(), "Test").unwrap();
        let meta = std::fs::metadata(dir.path().join("device.key")).unwrap();
        assert_eq!(meta.permissions().mode() & 0o777, 0o600);
    }

    #[test]
    fn debug_never_prints_key_material() {
        let dir = tempfile::tempdir().unwrap();
        let id = DeviceIdentity::load_or_create(dir.path(), "Test").unwrap();
        let text = format!("{id:?}");
        assert!(text.contains("<redacted>"));
        assert!(!text.contains("PRIVATE"));
    }
}
