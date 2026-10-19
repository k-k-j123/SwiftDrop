//! Advertising our SwiftDrop service via mDNS/DNS-SD — spec FR-DISC-001,
//! §8.3.
//!
//! [`Advertiser`] wraps the `mdns-sd` daemon: it registers
//! `_swiftdrop._tcp.local.` with an opaque instance label, our listening
//! port, and the three TXT fields (see [`crate::txt`]). When the app
//! stops, the advertisement is withdrawn so we don't linger in other
//! peers' lists (spec §11.1 "keep the listener closed or undiscoverable
//! when the user disables receiving" applies at the app layer; this type
//! is the mechanism).

use crate::instance::instance_label;
use crate::txt::Advertisement;
use mdns_sd::{ServiceDaemon, ServiceInfo};
use std::net::IpAddr;

/// Errors from talking to the mDNS daemon.
#[derive(Debug, thiserror::Error)]
pub enum DiscoveryError {
    /// The mDNS daemon thread could not start (usually: UDP port 5353
    /// already taken by another mDNS responder such as Avahi/Bonjour,
    /// or no network interfaces exist).
    #[error("could not start the mDNS daemon: {0}")]
    DaemonStart(String),
    /// Registering/browsing failed.
    #[error("mDNS operation failed: {0}")]
    Mdns(String),
}

impl From<mdns_sd::Error> for DiscoveryError {
    fn from(e: mdns_sd::Error) -> Self {
        Self::Mdns(e.to_string())
    }
}

/// Everything needed to announce "SwiftDrop is listening here".
#[derive(Debug, Clone)]
pub struct AdvertiseConfig {
    /// TCP port our listener actually bound (never assume it equals the
    /// requested port — the OS may hand us an ephemeral one).
    pub port: u16,
    /// Our device id (hex), as computed by `swiftdrop-security`.
    pub device_id: String,
    /// Human display label, already normalized.
    pub display_name: String,
    /// Protocol major version we speak.
    pub proto_major: u16,
}

/// A live mDNS advertisement. Dropping this stops advertising (the
/// daemon is shut down), so `Advertiser` should simply be held for as
/// long as the receiver is active.
pub struct Advertiser {
    daemon: ServiceDaemon,
    /// Service instance full name, e.g. `sd-3f9a01._swiftdrop._tcp.local.`.
    /// Kept for debugging and explicit unregister.
    fullname: String,
}

impl std::fmt::Debug for Advertiser {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Advertiser")
            .field("fullname", &self.fullname)
            .finish_non_exhaustive()
    }
}

impl Advertiser {
    /// Start the mDNS daemon and announce the service.
    pub fn start(config: AdvertiseConfig) -> Result<Self, DiscoveryError> {
        let daemon = ServiceDaemon::new().map_err(|e| DiscoveryError::DaemonStart(e.to_string()))?;
        let instance = instance_label();

        // Hostname: our machine's hostname, DNS-escaped, suffixed with
        // ".local." as mDNS requires. The library normalizes casing.
        let host_name = format!("{}.local.", local_hostname_label());

        let advertisement = Advertisement {
            proto_major: config.proto_major,
            device_id: config.device_id,
            display_name: config.display_name,
        };

        // Seed with the primary outbound address (a UDP "connect"
        // sends no packets; it just asks the kernel which source
        // address it would use). `enable_addr_auto` then keeps the
        // record current when interfaces come and go (FR-DISC-009).
        let seed_addresses: Vec<std::net::IpAddr> = primary_outbound_ipv4()
            .map(|ip| vec![ip])
            .unwrap_or_default();

        // `mdns-sd` wants its own TxtProperty type; convert our
        // (key, value) pairs here at the boundary.
        let txt_properties: Vec<mdns_sd::TxtProperty> = advertisement
            .txt_pairs()
            .into_iter()
            .map(mdns_sd::TxtProperty::from)
            .collect();

        let service = ServiceInfo::new(
            crate::SERVICE_TYPE,
            &instance,
            &host_name,
            &seed_addresses[..],
            config.port,
            txt_properties,
        )
        .map_err(|e| DiscoveryError::Mdns(e.to_string()))?
        .enable_addr_auto();

        let fullname = service.get_fullname().to_string();
        daemon.register(service)?;
        tracing::info!(%fullname, port = config.port, "advertising SwiftDrop service");

        Ok(Self { daemon, fullname })
    }

    /// The registered service instance full name.
    pub fn fullname(&self) -> &str {
        &self.fullname
    }

    /// Withdraw the advertisement and stop the daemon. Safe to call
    /// multiple times via `Option` juggling by the caller; here it just
    /// consumes `self`.
    pub fn shutdown(self) {
        // `unregister`/`shutdown` return status channels; we don't need
        // to await them — dropping the daemon tears everything down, and
        // a graceful unregister is best-effort.
        let _ = self.daemon.unregister(&self.fullname);
        let _ = self.daemon.shutdown();
    }
}

impl Drop for Advertiser {
    fn drop(&mut self) {
        // Best-effort: withdraw the advertisement so peers don't keep a
        // stale entry for the full TTL (120s) if the app crashes.
        let _ = self.daemon.unregister(&self.fullname);
        let _ = self.daemon.shutdown();
    }
}

/// The kernel-chosen source address for outbound traffic, or `None`.
fn primary_outbound_ipv4() -> Option<IpAddr> {
    // UDP connect performs no I/O on the network; it only asks the
    // routing table "which source address would reach 8.8.8.8?".
    let socket = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("8.8.8.8:53").ok()?;
    socket.local_addr().ok().map(|a| a.ip())
}

/// A DNS-label-safe version of the machine hostname.
fn local_hostname_label() -> String {
    let raw = hostname::get()
        .map(|h| h.to_string_lossy().to_string())
        .unwrap_or_default();
    let label: String = raw
        .trim()
        .to_ascii_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '-' })
        .collect::<String>()
        .trim_matches('-')
        .to_string();
    if label.is_empty() || label.len() > 63 {
        "swiftdrop-host".to_string()
    } else {
        label
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hostname_label_is_dns_safe() {
        let label = local_hostname_label();
        assert!(!label.is_empty());
        assert!(label.len() <= 63);
        assert!(label
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-'));
    }

    /// Weird hostnames (VMs often have underscores/dots) collapse to a
    /// legal label instead of failing registration.
    #[test]
    fn hostname_with_weird_chars_is_sanitized() {
        // We can't override the real hostname in-process, so test the
        // transform indirectly: any label this function returns must be
        // registrable in the ".local." zone.
        let label = local_hostname_label();
        assert!(!label.starts_with('-') && !label.ends_with('-'));
    }
}
