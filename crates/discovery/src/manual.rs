//! Manual connection targets — spec FR-DISC-005.
//!
//! When mDNS discovery fails (guest Wi-Fi with client isolation, VPNs,
//! multicast filtering — spec FR-DISC-010), the user can type an address
//! directly. This module turns that text into the same [`PeerInfo`] the
//! mDNS path produces, so everything downstream is identical.
//!
//! Accepted forms:
//!
//! | input              | meaning                                   |
//! |--------------------|-------------------------------------------|
//! | `192.168.1.5`      | IPv4, default port                        |
//! | `192.168.1.5:4000` | IPv4 + explicit port                      |
//! | `fd00::1`          | IPv6, default port                        |
//! | `[fd00::1]:4000`   | IPv6 + explicit port (brackets required)  |
//! | `laptop.local`     | hostname, default port (resolved once)    |
//! | `laptop.local:4000`| hostname + port (resolved once)           |
//!
//! Hostnames are resolved *immediately*, once, via the system resolver.
//! If the peer's address changes later, re-run the command — documented
//! limitation of the manual path (mDNS handles changes automatically).

use crate::peer::{DiscoverySource, PeerInfo};
use std::net::{IpAddr, ToSocketAddrs};

/// Why a manual entry was refused.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum ManualError {
    #[error("the address is empty")]
    Empty,
    #[error("invalid port {0:?} (expected 1-65535)")]
    BadPort(String),
    #[error(
        "could not understand address {0:?} — expected e.g. 192.168.1.5:4000 or [fd00::1]:4000"
    )]
    Unparseable(String),
    #[error("could not resolve host {0:?} — check the spelling or use the IP address")]
    ResolveFailed(String),
}

/// Parse a user-typed target into a connectable [`PeerInfo`].
///
/// `default_port` is used when the input has no `:port` part.
pub fn parse_manual_target(input: &str, default_port: u16) -> Result<PeerInfo, ManualError> {
    let raw = input.trim();
    if raw.is_empty() {
        return Err(ManualError::Empty);
    }

    // Split host / port. Bracketed IPv6 keeps its colons; everything
    // else splits on the *last* colon. Note "fd00::1:5000" without
    // brackets parses as one IPv6 address (never as host+port) — the
    // bracketed form is required to attach a port, which we document in
    // the CLI help text.
    let (host, port): (&str, u16) = if let Some(rest) = raw.strip_prefix('[') {
        // [v6]:port  or  [v6]
        let (addr, after) = rest
            .split_once(']')
            .ok_or_else(|| ManualError::Unparseable(raw.to_string()))?;
        let port = match after.strip_prefix(':') {
            Some(p) => Some(parse_port(p)?),
            None => None,
        };
        (addr, port.unwrap_or(default_port))
    } else if raw.parse::<IpAddr>().is_ok() {
        // A bare IP address (v4 or v6) — no port part.
        (raw, default_port)
    } else if let Some((host_part, port_part)) = raw.rsplit_once(':') {
        // "host:port" where host is a hostname (or IPv4, handled above).
        // If there are multiple colons this isn't a valid host:port pair
        // (unbracketed IPv6), so reject rather than guess.
        if host_part.contains(':') {
            return Err(ManualError::Unparseable(raw.to_string()));
        }
        (host_part, parse_port(port_part)?)
    } else {
        // Bare hostname.
        (raw, default_port)
    };

    // Resolve now so the user gets "cannot resolve" immediately instead
    // of a mysterious connection timeout later.
    let addresses = resolve(host)?;
    if addresses.is_empty() {
        return Err(ManualError::ResolveFailed(host.to_string()));
    }

    Ok(PeerInfo {
        display_name: host.to_string(),
        // The real id is learned (and proven) during the handshake; a
        // manual target makes no claims.
        device_id: None,
        addresses,
        port,
        // Unknown until the handshake says otherwise; the manual path
        // exists precisely for cases where discovery couldn't tell us.
        proto_major: 0,
        source: DiscoverySource::Manual,
        fullname: None,
    })
}

fn parse_port(s: &str) -> Result<u16, ManualError> {
    match s.parse::<u16>() {
        Ok(0) | Err(_) => Err(ManualError::BadPort(s.to_string())),
        Ok(p) => Ok(p),
    }
}

/// System-resolver lookup for hostnames; literal IPs short-circuit.
fn resolve(host: &str) -> Result<Vec<IpAddr>, ManualError> {
    if let Ok(ip) = host.parse::<IpAddr>() {
        return Ok(vec![ip]);
    }
    // `to_socket_addrs` with port 0: we only want the addresses, the
    // port is irrelevant at this layer.
    let addrs = (host, 0)
        .to_socket_addrs()
        .map_err(|_| ManualError::ResolveFailed(host.to_string()))?
        .map(|a| a.ip())
        .collect::<Vec<_>>();
    Ok(addrs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::peer::PeerStatus;

    #[test]
    fn ipv4_with_and_without_port() {
        let p = parse_manual_target("192.168.1.5:4000", 52915).unwrap();
        assert_eq!(p.port, 4000);
        assert_eq!(p.addresses, vec![IpAddr::from([192, 168, 1, 5])]);
        assert_eq!(p.source, DiscoverySource::Manual);
        assert_eq!(p.device_id, None);

        let p = parse_manual_target("192.168.1.5", 52915).unwrap();
        assert_eq!(p.port, 52915);
    }

    #[test]
    fn bracketed_ipv6_with_port() {
        let p = parse_manual_target("[fd00::1]:4000", 52915).unwrap();
        assert_eq!(p.port, 4000);
        assert_eq!(p.addresses.len(), 1);
        assert!(p.addresses[0].is_ipv6());
    }

    #[test]
    fn bare_ipv6_uses_default_port() {
        // Unambiguous: the whole string is one IPv6 address.
        let p = parse_manual_target("fd00::1", 52915).unwrap();
        assert_eq!(p.port, 52915);
        assert!(p.addresses[0].is_ipv6());
    }

    /// "fd00::1:4000" is a *valid* IPv6 address (last hextet 4000), so
    /// it is treated as a bare address with the default port — never as
    /// host+port. The bracketed form is the only way to attach a port.
    #[test]
    fn unbracketed_ipv6_with_port_is_treated_as_address() {
        let p = parse_manual_target("fd00::1:4000", 52915).unwrap();
        assert_eq!(p.port, 52915);
        assert!(p.addresses[0].is_ipv6());
    }

    /// Multi-colon input that isn't a valid IPv6 address is rejected
    /// rather than guessed at.
    #[test]
    fn unparseable_multi_colon_input_is_rejected() {
        assert!(matches!(
            parse_manual_target("aa:bb:cc:4000", 52915),
            Err(ManualError::Unparseable(_))
        ));
        // A valid IPv6 must be bracketed to attach a port; plain "v6"
        // may contain colons but is recognized as an address.
    }

    #[test]
    fn hostname_resolves() {
        // "localhost" resolves on every sane system.
        let p = parse_manual_target("localhost:1234", 52915).unwrap();
        assert_eq!(p.port, 1234);
        assert!(!p.addresses.is_empty());
        assert!(p.addresses.iter().all(|a| a.is_loopback()));
    }

    #[test]
    fn bad_inputs_are_clean_errors() {
        assert_eq!(parse_manual_target("   ", 52915), Err(ManualError::Empty));
        assert!(matches!(
            parse_manual_target("192.168.1.5:0", 52915),
            Err(ManualError::BadPort(_))
        ));
        assert!(matches!(
            parse_manual_target("192.168.1.5:99999", 52915),
            Err(ManualError::BadPort(_))
        ));
        assert!(matches!(
            parse_manual_target("no such host.invalid", 52915),
            Err(ManualError::ResolveFailed(_))
        ));
    }

    /// Manual peers have proto_major 0, so they should be treated as
    /// Available until the authenticated handshake reveals incompatibility.
    #[test]
    fn manual_peer_status_is_available_despite_unknown_version() {
        let p = parse_manual_target("192.168.1.5", 52915).unwrap();
        assert_eq!(p.proto_major, 0);
        assert_eq!(p.status(1), PeerStatus::Available);
        assert_eq!(p.status(2), PeerStatus::Available);
    }
}
