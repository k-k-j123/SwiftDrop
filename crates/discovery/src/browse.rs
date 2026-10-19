//! Browsing for SwiftDrop peers via mDNS/DNS-SD — spec FR-DISC-002,
//! FR-DISC-003, FR-DISC-008.
//!
//! [`Discovery`] opens a browse session on `_swiftdrop._tcp.local.` and
//! converts the mDNS daemon's event stream into a small, stable stream
//! of [`DiscoveryEvent`]s for the rest of the app:
//!
//! * `ServiceFound` (PTR seen) is ignored on its own — we only emit a
//!   peer once it is fully *resolved* (addresses + TXT known).
//! * `ServiceResolved` → [`DiscoveryEvent::Found`] (first time) or
//!   [`DiscoveryEvent::Updated`] (record refreshed / address change,
//!   spec FR-DISC-009).
//! * `ServiceRemoved` → [`DiscoveryEvent::Lost`] (peer shut down or left
//!   the network).
//!
//! The mapping runs on a dedicated tokio task so the caller just awaits
//! [`Discovery::next_event`]. All mDNS data flowing through here is
//! **untrusted** (crate docs); [`crate::txt::parse_advert`] degrades
//! hostile values instead of rejecting the peer, and the caller must
//! still authenticate before believing anything.

use crate::peer::PeerInfo;
use crate::txt::parse_advert;
use mdns_sd::{Receiver as MdnsReceiver, ScopedIp, ServiceDaemon, ServiceEvent};
use std::collections::HashMap;
use std::net::IpAddr;

/// An change in the set of SwiftDrop peers on the network.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiscoveryEvent {
    /// A peer we had not seen before resolved successfully.
    Found(PeerInfo),
    /// A known peer refreshed its records (new addresses, renamed…).
    /// The peer list should replace its old entry with this one.
    Updated(PeerInfo),
    /// A peer's advertisement disappeared (app closed, left the network,
    /// or its TTL expired). Keyed by mDNS full name.
    Lost { fullname: String },
}

/// Live mDNS browsing. Holds the daemon; dropping it stops discovery.
pub struct Discovery {
    daemon: ServiceDaemon,
    events: tokio::sync::mpsc::Receiver<DiscoveryEvent>,
}

impl std::fmt::Debug for Discovery {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Discovery").finish_non_exhaustive()
    }
}

impl Discovery {
    /// Start browsing. Spawns one background task that owns the mDNS
    /// event stream; `our_major` is only used by callers for
    /// compatibility verdicts (kept here so the type documents intent).
    pub fn start() -> Result<Self, crate::advertise::DiscoveryError> {
        let daemon =
            ServiceDaemon::new().map_err(|e| crate::advertise::DiscoveryError::DaemonStart(e.to_string()))?;
        let mdns_rx = daemon.browse(crate::SERVICE_TYPE)?;
        let (tx, rx) = tokio::sync::mpsc::channel(64);
        tokio::spawn(map_events(mdns_rx, tx));
        Ok(Self { daemon, events: rx })
    }

    /// Wait for the next peer-list change. `None` means the mDNS event
    /// stream ended (daemon shut down).
    pub async fn next_event(&mut self) -> Option<DiscoveryEvent> {
        self.events.recv().await
    }

    /// Non-blocking variant for callers with their own select loops.
    pub fn poll_event(&mut self) -> Option<DiscoveryEvent> {
        self.events.try_recv().ok()
    }

    /// Stop browsing and shut the daemon down.
    pub fn stop(self) {
        let _ = self.daemon.shutdown();
    }
}

/// The mDNS daemon's event stream → our [`DiscoveryEvent`] stream.
///
/// State: `seen` maps mDNS full names to the peer info we last emitted,
/// so (a) repeats become `Updated` instead of duplicate `Found`s, and
/// (b) `Lost` can include the device id for nicer CLI output later.
async fn map_events(
    rx: MdnsReceiver<ServiceEvent>,
    tx: tokio::sync::mpsc::Sender<DiscoveryEvent>,
) {
    let mut seen: HashMap<String, PeerInfo> = HashMap::new();
    loop {
        let event = match rx.recv_async().await {
            Ok(e) => e,
            Err(_) => {
                // Daemon gone (shutdown) — nothing left to forward.
                return;
            }
        };
        match event {
            ServiceEvent::ServiceResolved(info) => {
                let fullname = info.get_fullname().to_string();
                let port = info.get_port();
                // TXT lookup adapter: mdns-sd gives us `&str` values.
                let lookup = |key: &str| info.get_property_val_str(key).map(str::to_string);
                let addresses = resolved_addresses(&info);
                let peer = parse_advert(&fullname, addresses, port, &lookup);
                let event = if seen.insert(fullname.clone(), peer.clone()).is_some() {
                    DiscoveryEvent::Updated(peer)
                } else {
                    tracing::debug!(%fullname, port, "discovered SwiftDrop peer");
                    DiscoveryEvent::Found(peer)
                };
                if tx.send(event).await.is_err() {
                    return; // receiver dropped
                }
            }
            ServiceEvent::ServiceRemoved(_, fullname) => {
                if seen.remove(&fullname).is_some()
                    && tx
                        .send(DiscoveryEvent::Lost { fullname })
                        .await
                        .is_err()
                {
                    return;
                }
            }
            // ServiceFound: name known but not resolved yet — nothing to
            // show a user until addresses and TXT arrive.
            // SearchStarted/SearchStopped: bookkeeping only.
            _ => {}
        }
    }
}

/// All IP addresses of a resolved service, v4 and v6, deduplicated.
fn resolved_addresses(info: &mdns_sd::ResolvedService) -> Vec<IpAddr> {
    let mut out: Vec<IpAddr> = info
        .get_addresses()
        .iter()
        .filter_map(|scoped| match scoped {
            ScopedIp::V4(v4) => Some(IpAddr::V4(*v4.addr())),
            ScopedIp::V6(v6) => Some(IpAddr::V6(*v6.addr())),
            _ => None,
        })
        .collect();
    out.sort();
    out.dedup();
    out
}
