//! # SwiftDrop security — identity, TLS, pairing, and trust
//!
//! This crate implements the security model described in spec §10
//! ("Security and threat model") for the MVP. Three ideas hold it up:
//!
//! ## 1. Every installation has a durable *device identity*
//!
//! A P-256 key pair is generated the first time SwiftDrop runs and is
//! stored in the user's config directory with owner-only permissions
//! (`0600` on Unix). The device's `device_id` is
//! `SHA-256(public-key-point)` rendered as hex — it never changes across
//! restarts as long as the key file survives (spec FR-PAIR-004).
//!
//! ## 2. TLS gives us an encrypted pipe + a *channel binding*
//!
//! Every connection is protected by TLS 1.3 (spec NFR-SEC-001). The
//! TLS certificates are self-signed and **not** used to decide who the
//! peer is (a self-signed cert proves nothing on its own). What we use
//! from TLS is the RFC 5705/8446 *exported keying material* — a secret
//! derived from the handshake that both sides compute identically and an
//! attacker terminating TLS separately on each side cannot reproduce.
//!
//! That exporter is the foundation of both:
//!
//! * **Authentication** — each side signs the exporter with its identity
//!   key in the `hello` message (`swiftdrop-security::sign_auth`). This
//!   proves "I hold the private key for the identity I claim, *in this
//!   very session*" and cannot be replayed from a recording.
//! * **The pairing code (SAS)** — both screens display
//!   `derive_sas(exporter, my_id, their_id)`. If a man-in-the-middle
//!   relays the connection, the two endpoints are separate TLS sessions
//!   with different exporters, so the two users see *different* codes and
//!   notice immediately (spec FR-PAIR-009).
//!
//! ## 3. Trust is earned by the user and stored locally
//!
//! A fingerprint that appears in discovery is untrusted until the short
//! authentication string is confirmed by a human on both devices
//! (spec FR-PAIR-002/003). Confirmed identities are appended to
//! `trusted_devices.json`; removing one revokes trust for future
//! sessions (spec FR-PAIR-005/008).
//!
//! Modules: [`identity`] · [`tls`] · [`auth`] · [`trust`] · [`error`]

pub mod auth;
pub mod error;
pub mod identity;
pub mod tls;
pub mod trust;

pub use error::SecurityError;
pub use identity::DeviceIdentity;
pub use trust::{TrustStore, TrustedDevice, TRUST_FILE};

/// Label for the TLS exporter used to sign `hello` messages (ADR-0003).
pub const AUTH_EXPORTER_LABEL: &str = "swiftdrop-auth-v1";
/// Label for the TLS exporter used to derive the pairing code (ADR-0003).
pub const SAS_EXPORTER_LABEL: &str = "swiftdrop-sas-v1";
