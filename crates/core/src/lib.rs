//! Core SwiftDrop engine — orchestration, settings, session lifecycle,
//! and the web UI.
//!
//! This crate glues together the three lower-level crates and exposes a
//! high-level, async API the app can call (spec §7.4):
//!
//! * `settings` — platform-aware configuration + on-disk persistence.
//! * `sessions` — the full connection lifecycle: TLS accept/connect,
//!   hello + exporter-bound auth, SAS pairing with trust-store updates,
//!   offer/accept, streaming, integrity verification, and completion.
//!   This is the §21 "first slice" — a working end-to-end transfer.
//! * `web` — the app itself: a hand-rolled HTTP server whose single page
//!   is the whole UI (send, receive, and phone upload). Deliberately
//!   unauthenticated (see its module docs for the trade).
//! * `progress` — the live byte counters `sessions` writes and the UI
//!   polls, so a transfer draws a real bar and speed (spec FR-UI-004,
//!   §9.3). Lock-free and local: nothing new goes on the wire.
//!
//! The non-technical owner's requirement ("document everything properly")
//! is honored: every public type has rationale, every invariant is
//! explained, and we never log secrets (spec NFR-SEC-007).

pub mod config;
pub mod progress;
pub mod sessions;
pub mod settings;
pub mod web;

// The live-counter type callers put into `SendOptions`/`ReceiveOptions` and
// read while the transfer runs (spec §9.3).
pub use progress::TransferProgress;
pub use sessions::{
    receive, send, AutoAccept, OfferLimits, Peer, ReceiveOptions, SendOptions, SendOutcome,
    SessionError, SessionPrompt,
};
pub use settings::Settings;
// Re-exported so the CLI and integration tests can inspect pairing
// outcomes without depending on the security/transfer crates directly.
pub use swiftdrop_security::{TrustStore, TRUST_FILE};
pub use swiftdrop_transfer::paths::CollisionPolicy;
