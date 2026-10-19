//! Session authentication and the pairing *short authentication string*
//! (SAS) — both derived from the TLS exporter (spec §10.5, ADR-0003).
//!
//! ## Authentication message (`hello.auth_sig`)
//!
//! ```text
//! message_to_sign = TLS_EXPORTER("swiftdrop-auth-v1", len=32) || device_key_sec1
//! auth_sig        = ECDSA_P256_SHA256_sign(message_to_sign)
//! ```
//!
//! Properties this buys us:
//!
//! * **Session-bound** — the exporter only exists after *this* TLS
//!   handshake; a signature captured from another session fails here.
//! * **Identity-bound** — signing over `device_key_sec1` means the
//!   signature only verifies for the exact public key the peer claims;
//!   and `device_id` is recomputed by us as `SHA-256(device_key_sec1)`,
//!   so a peer cannot claim someone else's id (spec FR-PAIR-002).
//! * **No replay** — an attacker who records our bytes and replays them
//!   into a new session computes a different exporter, so verification
//!   fails.
//!
//! ## Pairing code (SAS)
//!
//! ```text
//! ids          = sorted(my_device_id, their_device_id)
//! SAS_EXPORTER = TLS_EXPORTER("swiftdrop-sas-v1", context=ids, len=8)
//! code         = (first 4 bytes as u32, big-endian) mod 1_000_000  → "042317"
//! ```
//!
//! Both devices in one TLS session derive **identical** exporters and know
//! both ids, so both display the same 6 digits. A man-in-the-middle must
//! terminate TLS twice (two sessions ⇒ two exporters), which makes the two
//! screens show *different* codes — the user seeing a mismatch cancels
//! (spec FR-PAIR-009). The construction rests entirely on the reviewed
//! RFC 5705/8446 exporter; SwiftDrop invents no cryptography
//! (spec NFR-SEC-002).

use crate::error::SecurityError;
use crate::identity::DeviceIdentity;
use crate::{tls, AUTH_EXPORTER_LABEL, SAS_EXPORTER_LABEL};
use p256::PublicKey;
use sha2::{Digest, Sha256};
use signature::Verifier;

/// Compute `device_id` from a SEC1 compressed public key point.
///
/// Both sides run this over the *same* `device_key` field from `hello`,
/// which is what makes the ids comparable (and what ties the trust store
/// entry to the key, not to a name).
pub fn device_id_from_key(sec1_point: &[u8]) -> Result<String, SecurityError> {
    // Validate the point parses before hashing: hashing arbitrary bytes
    // would happily produce an "id" for a garbage key.
    PublicKey::from_sec1_bytes(sec1_point)
        .map_err(|e| SecurityError::DeviceKeyInvalid(e.to_string()))?;
    Ok(hex::encode(Sha256::digest(sec1_point)))
}

/// Build the signed `auth_sig` payload for our `hello` message.
pub fn sign_auth(
    identity: &DeviceIdentity,
    conn: &dyn tls::ExporterSource,
) -> Result<Vec<u8>, SecurityError> {
    let exporter = tls::exporter(conn, AUTH_EXPORTER_LABEL, &[], 32)?;
    let mut msg = exporter;
    msg.extend_from_slice(&identity.public_sec1());
    Ok(identity.sign(&msg))
}

/// Verify a peer's `auth_sig` from its `hello` message.
///
/// # Arguments
/// * `sec1_point` — the peer's claimed public key (`device_key`, decoded).
/// * `claimed_device_id` — the peer's claimed `device_id` (hex).
/// * `auth_sig` — the peer's signature (`auth_sig`, decoded).
/// * `conn` — the established TLS connection (source of the exporter).
///
/// Returns the *verified* `device_id` (which equals the claim if all is
/// well) — callers must use this value, never the raw claim, for trust
/// decisions.
pub fn verify_auth(
    sec1_point: &[u8],
    claimed_device_id: &str,
    auth_sig: &[u8],
    conn: &dyn tls::ExporterSource,
) -> Result<String, SecurityError> {
    // 1. The claimed id must be the hash of the claimed key.
    let computed_id = device_id_from_key(sec1_point)?;

    // 2. Parse the public key.
    let point = PublicKey::from_sec1_bytes(sec1_point)
        .map_err(|e| SecurityError::DeviceKeyInvalid(e.to_string()))?;
    let verifying: p256::ecdsa::VerifyingKey = point.into();

    // 3. Rebuild the exact message the peer must have signed.
    let exporter = tls::exporter(conn, AUTH_EXPORTER_LABEL, &[], 32)?;
    let mut msg = exporter;
    msg.extend_from_slice(sec1_point);

    // 4. Check the signature.
    let sig = p256::ecdsa::Signature::from_slice(auth_sig)
        .map_err(|e| SecurityError::SignatureInvalid(e.to_string()))?;
    verifying
        .verify(&msg, &sig)
        .map_err(|_| SecurityError::AuthSignatureInvalid)?;

    if computed_id != claimed_device_id {
        return Err(SecurityError::IdentityMismatch);
    }

    Ok(computed_id)
}

/// Derive the 6-digit pairing code both users must see and confirm.
///
/// See the module docs for why a man-in-the-middle makes the two sides
/// disagree. `id_a`/`id_b` may be passed in either order.
pub fn derive_sas(
    conn: &dyn tls::ExporterSource,
    id_a: &str,
    id_b: &str,
) -> Result<String, SecurityError> {
    // Canonical order so both ends compute the same context.
    let (first, second) = if id_a <= id_b {
        (id_a, id_b)
    } else {
        (id_b, id_a)
    };
    let mut context = first.as_bytes().to_vec();
    context.push(b'\n');
    context.extend_from_slice(second.as_bytes());

    let material = tls::exporter(conn, SAS_EXPORTER_LABEL, &context, 8)?;
    let value = u32::from_be_bytes([material[0], material[1], material[2], material[3]]);
    Ok(format!("{:06}", value % 1_000_000))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The exporter needs a real TLS handshake, so SAS/auth behaviour
    /// against live connections is covered by the integration tests in
    /// `crates/core/tests/`. Here we cover the pure helpers.

    #[test]
    fn device_id_is_sha256_of_key() {
        let dir = tempfile::tempdir().unwrap();
        let id = DeviceIdentity::load_or_create(dir.path(), "T").unwrap();
        let point = id.public_sec1();
        let derived = device_id_from_key(&point).unwrap();
        assert_eq!(derived, id.device_id());
        // 32 bytes of hex.
        assert_eq!(derived.len(), 64);
    }

    #[test]
    fn device_id_rejects_garbage_key() {
        assert!(
            device_id_from_key(&[0u8; 33]).is_err(),
            "zero point is invalid"
        );
        assert!(device_id_from_key(b"short").is_err());
    }

    #[test]
    fn device_id_is_deterministic() {
        let dir = tempfile::tempdir().unwrap();
        let id = DeviceIdentity::load_or_create(dir.path(), "T").unwrap();
        assert_eq!(
            device_id_from_key(&id.public_sec1()).unwrap(),
            id.device_id()
        );
    }

    /// Signature bytes are range-checked on parse: all-zero and
    /// out-of-range values fail, real signatures parse.
    #[test]
    fn signature_from_bytes_validates() {
        let dir = tempfile::tempdir().unwrap();
        let id = DeviceIdentity::load_or_create(dir.path(), "T").unwrap();
        let real = id.sign(b"a real message");
        assert!(p256::ecdsa::Signature::from_slice(&real).is_ok());
        // r = 0 / s = 0 are not valid scalars → rejected.
        assert!(p256::ecdsa::Signature::from_slice(&[0u8; 64]).is_err());
        // Wrong length → rejected.
        assert!(p256::ecdsa::Signature::from_slice(&[0u8; 10]).is_err());
        // r,s ≥ curve order → rejected.
        assert!(p256::ecdsa::Signature::from_slice(&[0xffu8; 64]).is_err());
    }
}
