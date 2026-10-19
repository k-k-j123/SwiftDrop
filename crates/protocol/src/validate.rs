//! Strict validation of everything a peer may send us (spec NFR-SEC-004).
//!
//! Nothing that arrives over the network is trusted until it passes through
//! this module. All checks are pure functions so they are cheap to unit-test
//! (spec §14.1 lists exactly these cases).

use crate::errors::ProtocolError;
use crate::messages::TransferOffer;

/// Maximum length of an opaque id (`request_id`, `transfer_id`, `item_id`).
pub const MAX_ID_LEN: usize = 128;
/// Minimum length — ids shorter than this are suspicious (we always emit 32).
pub const MIN_ID_LEN: usize = 8;

/// Maximum length of a sender-supplied *display* name, in UTF-8 bytes.
///
/// Generous enough for real filenames (Linux allows 255 bytes per path
/// component), tight enough to bound allocations.
pub const MAX_NAME_LEN: usize = 512;

/// Limits applied to an incoming offer. Callers (the receiver) fill these
/// in from user settings; defaults match the MVP defaults in spec §11.1.
#[derive(Debug, Clone, Copy)]
pub struct OfferLimits {
    /// Largest single file accepted.
    pub max_file_bytes: u64,
    /// Largest sum of all files in one offer.
    pub max_total_bytes: u64,
    /// Largest number of files in one offer.
    pub max_items: usize,
}

impl Default for OfferLimits {
    fn default() -> Self {
        Self {
            // 64 GiB per file / total for one transfer, 1000 files.
            // Users can lower these in settings.json (spec FR-XFER-020).
            max_file_bytes: 64 * 1024 * 1024 * 1024,
            max_total_bytes: 64 * 1024 * 1024 * 1024,
            max_items: 1000,
        }
    }
}

/// Check an opaque id: printable ASCII from `[A-Za-z0-9_-]`, bounded length.
///
/// Ids only ever index our own random strings, so allowing this narrow
/// alphabet costs nothing and blocks control characters, quotes, and
/// anything a log or terminal might mis-render.
pub fn check_opaque_id(field: &'static str, id: &str) -> Result<(), ProtocolError> {
    if id.len() < MIN_ID_LEN || id.len() > MAX_ID_LEN {
        return Err(ProtocolError::InvalidField {
            field,
            reason: format!("length {} outside {MIN_ID_LEN}..={MAX_ID_LEN}", id.len()),
        });
    }
    if !id
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        return Err(ProtocolError::InvalidField {
            field,
            reason: "contains characters outside [A-Za-z0-9_-]".into(),
        });
    }
    Ok(())
}

/// `request_id` validation for every envelope (spec §8.4: "validated for
/// length and syntax").
pub fn check_request_id(id: &str) -> Result<(), ProtocolError> {
    check_opaque_id("request_id", id)
}

/// Validate a sender-supplied *display* name before it reaches the UI.
///
/// This is a **pre-check**, not the final defense: the receiver still runs
/// the name through `swiftdrop-transfer::paths::sanitize_file_name` before
/// anything touches the filesystem (defense in depth, spec §10.4).
pub fn check_display_name(name: &str) -> Result<(), ProtocolError> {
    if name.is_empty() {
        return Err(ProtocolError::InvalidField {
            field: "name",
            reason: "empty".into(),
        });
    }
    if name.len() > MAX_NAME_LEN {
        return Err(ProtocolError::InvalidField {
            field: "name",
            reason: format!("longer than {MAX_NAME_LEN} bytes"),
        });
    }
    if name.contains('\0') {
        return Err(ProtocolError::InvalidField {
            field: "name",
            reason: "contains NUL".into(),
        });
    }
    Ok(())
}

/// Validate a SHA-256 digest as it appears on the wire: exactly 64 hex chars.
pub fn check_sha256_hex(digest: &str) -> Result<(), ProtocolError> {
    if digest.len() != 64 || !digest.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(ProtocolError::InvalidField {
            field: "sha256",
            reason: "must be 64 hexadecimal characters".into(),
        });
    }
    Ok(())
}

/// Validate a whole `transfer.offer` against [`OfferLimits`].
///
/// Implements every rule from spec §8.6:
/// * ids well-formed,
/// * names non-empty, bounded, NUL-free,
/// * sizes within per-file and aggregate limits,
/// * `total_size_bytes` **equals the sum of item sizes** (a lying total
///   is rejected rather than "trusted but checked later"),
/// * `sha256` (when present) is 64 hex chars,
/// * at least one item, at most `max_items`.
///
/// NOTE: disk-space and destination checks happen later, at *accept* time,
/// because they depend on the local machine, not on the message itself.
pub fn check_offer(offer: &TransferOffer, limits: &OfferLimits) -> Result<(), ProtocolError> {
    check_opaque_id("transfer_id", &offer.transfer_id).map_err(|e| match e {
        ProtocolError::InvalidField { reason, .. } => {
            ProtocolError::InvalidOffer(format!("transfer_id invalid: {reason}"))
        }
        other => other,
    })?;

    if offer.items.is_empty() {
        return Err(ProtocolError::InvalidOffer(
            "offer contains no items".into(),
        ));
    }
    if offer.items.len() > limits.max_items {
        return Err(ProtocolError::InvalidOffer(format!(
            "{} items exceeds the limit of {}",
            offer.items.len(),
            limits.max_items
        )));
    }

    let mut sum: u64 = 0;
    for item in &offer.items {
        check_opaque_id("item_id", &item.item_id).map_err(|e| match e {
            ProtocolError::InvalidField { reason, .. } => {
                ProtocolError::InvalidOffer(format!("item_id invalid: {reason}"))
            }
            other => other,
        })?;
        check_display_name(&item.name)
            .map_err(|e| ProtocolError::InvalidOffer(format!("item name invalid: {e}")))?;

        if item.size_bytes > limits.max_file_bytes {
            return Err(ProtocolError::InvalidOffer(format!(
                "file '{}' ({},{} bytes) exceeds per-file limit of {} bytes",
                item.name, item.size_bytes, item.size_bytes, limits.max_file_bytes
            )));
        }

        // checked_add: a peer sending sizes that overflow u64 must fail
        // closed, never wrap around to a small number (spec §14.3).
        sum = sum.checked_add(item.size_bytes).ok_or_else(|| {
            ProtocolError::InvalidOffer("sum of item sizes overflows 64-bit bytes".into())
        })?;

        if let Some(d) = &item.sha256 {
            check_sha256_hex(d).map_err(|e| {
                ProtocolError::InvalidOffer(format!("item '{d0}' digest: {e}", d0 = item.name))
            })?;
        }
    }

    if sum != offer.total_size_bytes {
        return Err(ProtocolError::InvalidOffer(format!(
            "declared total {} != actual sum {sum} of item sizes",
            offer.total_size_bytes
        )));
    }
    if sum > limits.max_total_bytes {
        return Err(ProtocolError::InvalidOffer(format!(
            "offer total {sum} exceeds limit of {} bytes",
            limits.max_total_bytes
        )));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::messages::OfferItem;

    fn item(size: u64) -> OfferItem {
        OfferItem {
            item_id: "i_1234567890abcdef".into(),
            name: "photo.jpg".into(),
            size_bytes: size,
            sha256: Some("e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855".into()),
        }
    }

    fn offer(items: Vec<OfferItem>) -> TransferOffer {
        let total = items.iter().map(|i| i.size_bytes).sum();
        TransferOffer {
            transfer_id: "t_1234567890abcdef".into(),
            items,
            total_size_bytes: total,
        }
    }

    #[test]
    fn accepts_sane_offer() {
        let limits = OfferLimits::default();
        assert!(check_offer(&offer(vec![item(10), item(0)]), &limits).is_ok());
    }

    /// Zero-byte files must work (spec FR-XFER-017).
    #[test]
    fn accepts_empty_file() {
        assert!(check_offer(&offer(vec![item(0)]), &OfferLimits::default()).is_ok());
    }

    #[test]
    fn rejects_total_that_does_not_match_sum() {
        let mut o = offer(vec![item(100)]);
        o.total_size_bytes = 99;
        let err = check_offer(&o, &OfferLimits::default()).unwrap_err();
        assert!(err.to_string().contains("!= actual sum"));
    }

    #[test]
    fn rejects_size_overflow() {
        // Build the offer by hand: summing these sizes would overflow u64
        // in debug mode — which is exactly what the validator must survive.
        let o = TransferOffer {
            transfer_id: "t_1234567890abcdef".into(),
            items: vec![item(u64::MAX), item(1)],
            total_size_bytes: u64::MAX, // declared total looks "valid"; sum overflows
        };
        // Limits set absurdly high so the per-file checks pass and the
        // *summation overflow* path is what rejects the offer.
        let limits = OfferLimits {
            max_file_bytes: u64::MAX,
            max_total_bytes: u64::MAX,
            max_items: 1000,
        };
        assert!(check_offer(&o, &limits).is_err());
    }

    #[test]
    fn rejects_oversized_file() {
        let limits = OfferLimits {
            max_file_bytes: 100,
            ..Default::default()
        };
        assert!(check_offer(&offer(vec![item(101)]), &limits).is_err());
    }

    #[test]
    fn rejects_too_many_items() {
        let limits = OfferLimits {
            max_items: 2,
            ..Default::default()
        };
        let items = vec![item(1), item(2), item(3)];
        assert!(check_offer(&offer(items), &limits).is_err());
    }

    #[test]
    fn rejects_empty_offer() {
        assert!(check_offer(&offer(vec![]), &OfferLimits::default()).is_err());
    }

    #[test]
    fn rejects_bad_ids() {
        for bad in [
            "",
            "short",
            "has spaces here",
            "has.dot",
            "x\"quote",
            "a/b/c",
        ] {
            assert!(
                check_opaque_id("transfer_id", bad).is_err(),
                "should reject {bad:?}"
            );
        }
        assert!(check_opaque_id("transfer_id", "ok_id-12345678").is_ok());
    }

    #[test]
    fn rejects_bad_names() {
        assert!(check_display_name("").is_err());
        assert!(check_display_name(&"a".repeat(1000)).is_err());
        assert!(check_display_name("with\0nul").is_err());
        assert!(check_display_name("unicode ☃ ok.txt").is_ok());
    }

    #[test]
    fn rejects_bad_digest() {
        assert!(
            check_sha256_hex("nothex-nothex-nothex-nothex-nothex-nothex-nothex-nothex-noth")
                .is_err()
        );
        assert!(check_sha256_hex(&"A".repeat(64)).is_ok()); // uppercase allowed
        assert!(check_sha256_hex(&"g".repeat(64)).is_err()); // 'g' not hex
        assert!(check_sha256_hex(
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b85"
        )
        .is_err()); // 63 chars
    }
}
