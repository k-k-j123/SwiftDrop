//! The peer list data model — spec FR-DISC-002, FR-DISC-004.
//!
//! Both discovery paths (mDNS and manual entry) produce the same
//! [`PeerInfo`] so everything downstream (core, CLI, later the Flutter UI)
//! treats them identically.

use std::net::IpAddr;

/// Where a peer entry came from. Purely informational — it changes
/// nothing about how the connection is authenticated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiscoverySource {
    /// Found via mDNS on the local network.
    Mdns,
    /// Typed in by the user because discovery failed (spec FR-DISC-005).
    Manual,
}

/// Lifecycle of a peer as far as *discovery* knows (spec FR-DISC-004).
///
/// Discovery can only observe "advertised" and "gone". States that need
/// an actual connection (`Connecting`, `Transferring`) live in `core`,
/// which owns the session. This enum covers the two discovery-visible
/// states plus the compatibility verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeerStatus {
    /// Advertised on the network right now.
    Available,
    /// Advertising a protocol major version we don't speak.
    Unsupported,
    /// Was seen, then its advertisement disappeared.
    Offline,
}

/// One peer we could try to connect to.
///
/// **Untrusted**: every field arrives from the network (or a text box)
/// and may be a lie. See the crate-level docs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerInfo {
    /// Human-readable label (from the `name` TXT field, or user input).
    /// May be empty for bare manual addresses.
    pub display_name: String,
    /// Claimed device id (SHA-256 hex of the peer's public key). `None`
    /// for manual targets — the real id is learned during the handshake.
    /// For mDNS peers this is a *claim* until the handshake proves it.
    pub device_id: Option<String>,
    /// Addresses to try, in order. mDNS may resolve several (dual-stack,
    /// multiple interfaces); we prefer IPv4 first for reachability.
    pub addresses: Vec<IpAddr>,
    /// TCP port of the peer's listener.
    pub port: u16,
    /// Protocol major version the peer says it speaks.
    pub proto_major: u16,
    /// How we found it.
    pub source: DiscoverySource,
    /// mDNS service instance full name (e.g. `sd-1a2b3c._swiftdrop._tcp.local.`).
    /// Used to match removal events; `None` for manual targets.
    pub fullname: Option<String>,
}

impl PeerInfo {
    /// Does this peer speak our protocol major version?
    ///
    /// The major version is checked *before* connecting only as a
    /// courtesy (spec FR-DISC-003): it lets the UI grey out an old app
    /// instead of failing mid-handshake. The authenticated hello exchange
    /// in `core` re-checks it over the secure channel regardless.
    pub fn is_compatible_with(&self, our_major: u16) -> bool {
        self.proto_major == our_major
    }

    /// Discovery's verdict for the UI (spec FR-DISC-004).
    ///
    /// `proto_major == 0` means *unknown* (a manual target, or an
    /// advertisement whose `proto` field was garbled). Unknown is shown
    /// as `Available`: greying a peer out based on information we don't
    /// have would hide perfectly working peers, and the authenticated
    /// hello exchange states the real version a moment later anyway.
    /// Only a *definite* mismatch (peer says major 2, we speak 1) is
    /// `Unsupported`.
    pub fn status(&self, our_major: u16) -> PeerStatus {
        if self.proto_major == 0 || self.proto_major == our_major {
            PeerStatus::Available
        } else {
            PeerStatus::Unsupported
        }
    }

    /// Candidate socket addresses (IP + port), IPv4 first, deduplicated.
    ///
    /// Order matters: on dual-stack networks IPv6 link-local addresses
    /// often exist but are unreachable without a zone id, so we try
    /// IPv4 first and fall back.
    pub fn socket_addrs(&self) -> Vec<std::net::SocketAddr> {
        let mut v4: Vec<_> = self
            .addresses
            .iter()
            .filter(|a| a.is_ipv4())
            .map(|a| std::net::SocketAddr::new(*a, self.port))
            .collect();
        let v6: Vec<_> = self
            .addresses
            .iter()
            .filter(|a| a.is_ipv6())
            .map(|a| std::net::SocketAddr::new(*a, self.port))
            .collect();
        v4.extend(v6);
        v4.sort();
        v4.dedup();
        v4
    }

    /// Short label for logs/UI: display name, else device id prefix,
    /// else "unknown peer". Never contains addresses.
    pub fn label(&self) -> String {
        if !self.display_name.is_empty() {
            self.display_name.clone()
        } else if let Some(id) = &self.device_id {
            format!("peer {}", &id[..8.min(id.len())])
        } else {
            "unknown peer".to_string()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, Ipv6Addr};

    fn peer(addrs: Vec<IpAddr>) -> PeerInfo {
        PeerInfo {
            display_name: "Test Peer".into(),
            device_id: Some("ab".repeat(32)),
            addresses: addrs,
            port: 52915,
            proto_major: 1,
            source: DiscoverySource::Mdns,
            fullname: Some("sd-abc._swiftdrop._tcp.local.".into()),
        }
    }

    #[test]
    fn compatibility_is_major_only() {
        let p = peer(vec![]);
        assert!(p.is_compatible_with(1));
        assert!(!p.is_compatible_with(2));
        assert_eq!(p.status(1), PeerStatus::Available);
        assert_eq!(p.status(2), PeerStatus::Unsupported);
    }

    /// IPv4 addresses are tried before IPv6 (link-local v6 is often a
    /// dead end without a zone id).
    #[test]
    fn socket_addrs_prefer_ipv4() {
        let p = peer(vec![
            IpAddr::V6(Ipv6Addr::new(0xfe80, 0, 0, 0, 0, 0, 0, 1)),
            IpAddr::V4(Ipv4Addr::new(192, 168, 1, 10)),
        ]);
        let addrs = p.socket_addrs();
        assert_eq!(addrs.len(), 2);
        assert!(addrs[0].is_ipv4());
        assert!(addrs[1].is_ipv6());
        assert!(addrs.iter().all(|a| a.port() == 52915));
    }

    /// A peer with no name still gets a stable, non-panicking label even
    /// when the id is short (a hostile peer could send a 1-char id).
    #[test]
    fn label_never_panics_on_short_or_missing_id() {
        let mut p = peer(vec![]);
        p.display_name = String::new();
        p.device_id = Some("x".into());
        assert_eq!(p.label(), "peer x");
        p.device_id = None;
        assert_eq!(p.label(), "unknown peer");
    }
}
