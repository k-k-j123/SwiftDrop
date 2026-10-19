//! Peer discovery for SwiftDrop — spec §5.1, §8.3.
//!
//! Two ways to find a peer, in order of preference:
//!
//! 1. **mDNS/DNS-SD** ([`advertise`] + [`browse`]): every running SwiftDrop
//!    advertises `_swiftdrop._tcp.local.` on the local network; peers
//!    browse that service type and maintain a live list of who is around.
//! 2. **Manual address** ([`manual`]): when multicast is blocked (guest
//!    Wi-Fi, VPNs, client isolation — spec FR-DISC-010) the user can type
//!    `ip:port` directly. Same connection path, just a different way to
//!    find the address.
//!
//! ## Everything here is untrusted (spec FR-DISC-008, NFR-SEC-003)
//!
//! An mDNS advertisement is *broadcast to everyone on the network* and can
//! be forged by anyone nearby: a laptop can claim to be "Alice's Laptop"
//! with Alice's device id. Nothing in this crate is used for
//! authentication. It only answers "where can I try to connect?" — the
//! `swiftdrop-security` handshake proves *who* the peer actually is, and
//! trust decisions happen only after that.
//!
//! ## What we advertise (spec §8.3, FR-DISC-007)
//!
//! Exactly three TXT fields and nothing else:
//!
//! | key      | value                              | why                                   |
//! |----------|------------------------------------|---------------------------------------|
//! | `proto`  | protocol major version (`1`)       | skip incompatible peers early         |
//! | `id`     | opaque device id (SHA-256 hex)     | pre-filter known/untrusted peers      |
//! | `name`   | short display label                | lets the UI show a human-readable name|
//!
//! No filenames, no transfer state, no trust lists, no secrets
//! (spec §8.3). The mDNS *instance* name is an opaque random label
//! (`sd-<hex>`) so even the DNS label leaks no user data; the human name
//! lives only in the TXT record.

pub mod advertise;
pub mod browse;
pub mod instance;
pub mod manual;
pub mod peer;
pub mod txt;

pub use advertise::{Advertiser, AdvertiseConfig};
pub use browse::{Discovery, DiscoveryEvent};
pub use manual::parse_manual_target;
pub use peer::{DiscoverySource, PeerInfo, PeerStatus};

/// Our DNS-SD service type (spec §8.3).
pub const SERVICE_TYPE: &str = "_swiftdrop._tcp.local.";

/// Default TCP port a SwiftDrop listener binds to when the user does not
/// choose one. The spec leaves the number open; we pick an unlikely-to-
/// clash high port. Receivers actually advertise whatever port they bind
/// (an ephemeral port is used if this one is taken), so a clash never
/// breaks discovery — the advertisement always carries the real port.
pub const DEFAULT_PORT: u16 = 52915;

/// Shortest things we accept as a display name before falling back.
pub const MAX_DISPLAY_NAME_CHARS: usize = 64;

/// Trim and bound an arbitrary user-supplied display name.
///
/// Used for both the advertisement and (later) settings storage so the
/// same rules apply everywhere. Never fails: empty input becomes
/// `None` so callers can substitute a default (the hostname).
pub fn normalize_display_name(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    // `.chars().take()` counts characters, not bytes — a display name of
    // 64 CJK characters is still fine in a TXT record.
    let bounded: String = trimmed.chars().take(MAX_DISPLAY_NAME_CHARS).collect();
    Some(bounded.trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_name_normalization() {
        assert_eq!(normalize_display_name("  Alice's Laptop  "), Some("Alice's Laptop".into()));
        assert_eq!(normalize_display_name("   "), None);
        assert_eq!(normalize_display_name(""), None);
        // Bounded by characters, not bytes.
        let long = "あ".repeat(200);
        assert_eq!(normalize_display_name(&long).unwrap().chars().count(), 64);
    }
}
