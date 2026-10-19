//! mDNS *instance name* generation — the DNS label in
//! `sd-xxxxxx._swiftdrop._tcp.local.`.
//!
//! Decision (documented for the ADR on discovery): the DNS instance
//! label is **opaque and random** (`sd-` + 6 hex chars). The human
//! display name lives only in the TXT `name` field.
//!
//! Why not put the display name in the instance label (the usual DNS-SD
//! style, e.g. "Alice's Laptop._swiftdrop._tcp.local.")?
//!
//! * Every device on the network sees mDNS broadcasts; the instance name
//!   would leak the chosen label to *all* of them, permanently, in
//!   passive DNS caches. The spec allows a display label only if the
//!   privacy implications are accepted (§8.3); keeping it in TXT means
//!   it is only shared when someone actively browses SwiftDrop.
//! * DNS labels have a 63-byte limit and strict character rules; user
//!   text violates them constantly (spaces, emoji, non-ASCII). An opaque
//!   label sidesteps an entire class of escaping bugs.
//! * The random suffix guarantees uniqueness even when two machines
//!   share a hostname (VMs, containers), which mDNS probing would
//!   otherwise have to resolve by renaming.

use rand::RngCore;

/// Length of the random hex part of the instance label.
const HEX_CHARS: usize = 6;

/// Generate a fresh instance label like `sd-3f9a01`.
///
/// Deterministic-shape, cryptographically random suffix. Cheap enough to
/// regenerate per session (each app start re-registers).
pub fn instance_label() -> String {
    let mut bytes = [0u8; HEX_CHARS / 2];
    rand::rng().fill_bytes(&mut bytes);
    format!("sd-{}", &hex::encode(bytes)[..HEX_CHARS])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The label must be a legal DNS label: ≤63 bytes, only safe chars.
    #[test]
    fn labels_are_valid_dns_labels() {
        for _ in 0..64 {
            let label = instance_label();
            assert!(label.len() >= 4 && label.len() <= 63, "bad len: {label}");
            assert!(label.starts_with("sd-"));
            assert!(label[3..].bytes().all(|b| b.is_ascii_hexdigit()));
        }
    }

    /// Collisions must be vanishingly unlikely; check a few hundred
    /// samples for a duplicate to catch a broken RNG hookup.
    #[test]
    fn labels_dont_repeat() {
        let mut seen = std::collections::HashSet::new();
        for _ in 0..512 {
            assert!(seen.insert(instance_label()), "duplicate instance label");
        }
    }
}
