//! Core SwiftDrop engine — orchestration, settings, and session lifecycle.
//!
//! This crate glues together the four lower-level crates and exposes a
//! high-level, async API the UI/CLI can call (spec §7.4):
//!
//! * `settings` — platform-aware configuration + on-disk persistence.
//! * `sessions` — the full connection lifecycle: TLS accept/connect,
//!   hello + exporter-bound auth, SAS pairing with trust-store updates,
//!   offer/accept, streaming, integrity verification, and completion.
//!   This is the §21 "first slice" — a working end-to-end transfer.
//! * `transport` — placeholder alias keeping the API shape stable for a
//!   future QUIC/USB adapter (spec §8.1, §13.1).
//! * `web` — a no-install phone path: an HTTP upload server for browsers,
//!   which cannot speak the SwiftDrop wire protocol. Speed-first,
//!   deliberately unauthenticated (see its module docs for the trade).
//!
//! The non-technical owner's requirement ("document everything properly")
//! is honored: every public type has rationale, every invariant is
//! explained, and we never log secrets (spec NFR-SEC-007).

pub mod config;
pub mod sessions;
pub mod settings;
pub mod transport;
pub mod web;

pub use sessions::{
    receive, send, AutoAccept, OfferLimits, Peer, ReceiveOptions, SendOptions, SendOutcome,
    SessionError, SessionPrompt,
};
pub use settings::Settings;
// Re-exported so the CLI and integration tests can inspect pairing
// outcomes without depending on the security/transfer crates directly.
pub use swiftdrop_security::{TrustStore, TRUST_FILE};
pub use swiftdrop_transfer::paths::CollisionPolicy;
