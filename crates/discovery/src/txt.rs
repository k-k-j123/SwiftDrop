//! TXT record contents for the `_swiftdrop._tcp.local.` advertisement —
//! spec §8.3, FR-DISC-007, FR-DISC-003.
//!
//! Three keys, each with a strict parse path, because every byte here
//! comes from an attacker-controllable network (see crate docs):
//!
//! * `proto` — protocol **major** version, decimal. Parsed with bounds
//!   checking; garbage becomes "incompatible", never a panic.
//! * `id` — claimed device id (lowercase hex SHA-256, 64 chars). Shape-
//!   checked here; *ownership* is proven later by the handshake signature
//!   (spec FR-PAIR-002), never by this advertisement.
//! * `name` — short human display label. Length-capped on both ends so
//!   a hostile peer can't stuff megabytes through a TXT record.

use crate::peer::{DiscoverySource, PeerInfo};

/// TXT key for the protocol major version.
pub const KEY_PROTO: &str = "proto";
/// TXT key for the claimed opaque device id.
pub const KEY_ID: &str = "id";
/// TXT key for the human display label.
pub const KEY_NAME: &str = "name";

/// Longest display label we accept *from the network* (bytes). The RFC
/// caps a single TXT string at 255 bytes; we stay well under so names
/// plus key overhead always fit.
pub const MAX_REMOTE_NAME_BYTES: usize = 64;

/// Longest device id we accept from the network (hex chars). Our ids are
/// 64; accepting up to 128 keeps leeway for future hash lengths without
/// letting a peer push arbitrary data into logs/UI.
const MAX_REMOTE_ID_CHARS: usize = 128;

/// The three fields of our advertisement, ready to hand to `mdns-sd`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Advertisement {
    /// Protocol major version we speak.
    pub proto_major: u16,
    /// Our device id (already computed by `swiftdrop-security`).
    pub device_id: String,
    /// Human display label (already normalized by the caller).
    pub display_name: String,
}

impl Advertisement {
    /// Render as `(key, value)` pairs for `mdns-sd`'s TXT properties.
    ///
    /// The device id is published deliberately: it lets a peer's UI say
    /// "already trusted / untrusted" *before* connecting, purely as a
    /// filter. It is not a secret — it is the hash of a public key — and
    /// forging it only fools the filter, never the handshake.
    pub fn txt_pairs(&self) -> Vec<(String, String)> {
        vec![
            (KEY_PROTO.to_string(), self.proto_major.to_string()),
            (KEY_ID.to_string(), self.device_id.clone()),
            (KEY_NAME.to_string(), self.display_name.clone()),
        ]
    }
}

/// What a remote advertisement claimed, before we trust any of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedAdvert {
    pub proto_major: u16,
    pub device_id: Option<String>,
    pub display_name: String,
}

/// Parse TXT properties (key → optional value bytes) into a [`PeerInfo`].
///
/// Lenient by design: a missing or garbled field degrades the entry
/// (no id, generic name, incompatible version) rather than dropping the
/// peer entirely — the UI can then explain "incompatible app" instead of
/// silently showing nothing (spec FR-DISC-004, FR-DISC-010).
pub fn parse_advert(
    fullname: &str,
    addresses: Vec<std::net::IpAddr>,
    port: u16,
    lookup: &dyn Fn(&str) -> Option<String>,
) -> PeerInfo {
    let proto_major = lookup(KEY_PROTO)
        .and_then(|v| v.trim().parse::<u16>().ok())
        .unwrap_or(0); // 0 = unknown → will never match a real major
    let device_id = lookup(KEY_ID).and_then(|v| {
        let v = v.trim().to_ascii_lowercase();
        if is_plausible_device_id(&v) {
            Some(v)
        } else {
            None
        }
    });
    let display_name = lookup(KEY_NAME)
        .map(|v| sanitize_remote_name(&v))
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "Unnamed device".to_string());

    PeerInfo {
        display_name,
        device_id,
        addresses,
        port,
        proto_major,
        source: DiscoverySource::Mdns,
        fullname: Some(fullname.to_string()),
    }
}

/// Shape-check a claimed device id: hex chars only, sane length.
///
/// This is *only* a hygiene check to keep junk out of the UI. A peer
/// passing this check has proven nothing — identity is proven exclusively
/// by the signature over the TLS exporter during the handshake.
pub fn is_plausible_device_id(s: &str) -> bool {
    let len = s.len();
    (8..=MAX_REMOTE_ID_CHARS).contains(&len) && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Cap a remote display name to a sane length and strip control chars.
///
/// Control characters would let a hostile peer fake terminal output or
/// inject zero-width text into the CLI/UI, so they are replaced, not
/// merely trimmed.
fn sanitize_remote_name(raw: &str) -> String {
    let cleaned: String = raw
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let mut out: String = cleaned
        .chars()
        .take(MAX_REMOTE_NAME_BYTES)
        .collect::<String>()
        .trim()
        .to_string();
    // Truncation may leave trailing dots/spaces; keep it tidy.
    while out.ends_with('.') {
        out.pop();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map_lookup<'a>(m: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |k: &str| m.iter().find(|(key, _)| *key == k).map(|(_, v)| v.to_string())
    }

    #[test]
    fn roundtrip_pairs_parse_back() {
        let adv = Advertisement {
            proto_major: 1,
            device_id: "ab".repeat(32),
            display_name: "Kitchen PC".into(),
        };
        let pairs = adv.txt_pairs();
        let lookup = |k: &str| {
            pairs
                .iter()
                .find(|(key, _)| key == k)
                .map(|(_, v)| v.clone())
        };
        let peer = parse_advert("sd-x._swiftdrop._tcp.local.", vec![], 52915, &lookup);
        assert_eq!(peer.proto_major, 1);
        assert_eq!(peer.device_id.as_deref(), Some("abababababababababababababababababababababababababababababababab"));
        assert_eq!(peer.display_name, "Kitchen PC");
        assert_eq!(peer.source, DiscoverySource::Mdns);
    }

    /// Garbage versions and ids degrade gracefully, never panic.
    #[test]
    fn hostile_values_degrade_instead_of_panicking() {
        let cases: &[(&str, &str)] = &[
            ("proto", "99999999999999999999"),
            ("proto", "banana"),
            ("proto", "-1"),
            ("id", "not hex!"),
            ("id", "ab"),
            ("name", ""),
        ];
        let lookup = map_lookup(cases);
        let peer = parse_advert("sd-x._swiftdrop._tcp.local.", vec![], 52915, &lookup);
        assert_eq!(peer.proto_major, 0, "unparseable proto must not match v1");
        assert_eq!(peer.device_id, None);
        assert_eq!(peer.display_name, "Unnamed device");
    }

    /// Control characters are neutralized so a hostile peer cannot inject
    /// terminal escapes into the CLI's peer list.
    #[test]
    fn remote_names_have_control_chars_stripped() {
        let lookup = map_lookup(&[("name", "\u{1b}[31mEvil\u{1b}[0m\tName")]);
        let peer = parse_advert("sd-x._swiftdrop._tcp.local.", vec![], 52915, &lookup);
        assert!(!peer.display_name.contains('\u{1b}'));
        assert!(!peer.display_name.contains('\t'));
        assert!(peer.display_name.contains("Evil"));
    }

    /// An over-long remote name is capped.
    #[test]
    fn remote_names_are_capped() {
        let long = "x".repeat(5000);
        let pairs = [("name", long.as_str())];
        let lookup = map_lookup(&pairs);
        let peer = parse_advert("sd-x._swiftdrop._tcp.local.", vec![], 52915, &lookup);
        assert!(peer.display_name.len() <= MAX_REMOTE_NAME_BYTES);
    }

    #[test]
    fn device_id_shape_check() {
        assert!(is_plausible_device_id(&"a1".repeat(32)));
        assert!(!is_plausible_device_id("short"));
        assert!(!is_plausible_device_id(&"z".repeat(64))); // not hex
        assert!(!is_plausible_device_id(&"ab".repeat(100))); // too long
    }
}
