//! Core SwiftDrop engine — orchestration, settings, and session lifecycle.
//!
//! This crate glues together the four lower-level crates and exposes a
//! high-level, async API the UI/CLI can call (spec §7.4):
//!
//! * `settings` — platform-aware configuration + on-disk persistence.
//! * `sessions` — incoming/outgoing connection state machines, TLS, hello,
//!   auth/SAS, pairing/trust, offer/accept, streaming, progress/cancel.
//! * `app` — public `SwiftDrop` facade (not fully fleshed yet, but
//!   designed to be testable without the UI).
//!
//! The non-technical owner's requirement ("document everything properly")
//! is honored: every public type has rationale, every invariant is
//! explained, and we never log secrets (spec NFR-SEC-007).

pub mod config;
pub mod settings;
pub mod sessions;
pub mod transport;

pub use settings::Settings;
