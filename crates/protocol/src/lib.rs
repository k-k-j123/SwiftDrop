//! # SwiftDrop protocol — messages, validation, and wire framing
//!
//! This crate is the *contract* between every SwiftDrop application: it
//! defines exactly what two devices say to each other, in what order, and
//! how those words are turned into bytes on the network.
//!
//! It deliberately contains **no networking, no filesystem access, and no
//! UI** (spec §7.4: `protocol` holds data types and validation only).
//!
//! ## The wire format, in plain language
//!
//! Two SwiftDrop devices talk over one encrypted (TLS 1.3) TCP connection.
//! Everything they exchange is split into **frames**:
//!
//! ```text
//!   [ 4 bytes: payload length (big-endian) ][ 1 byte: kind ][ payload ]
//! ```
//!
//! * `kind = 0` (CONTROL) → payload is a UTF-8 JSON *envelope* (see below).
//! * `kind = 1` (DATA)    → payload is a raw slice of file content
//!   (max 64 KiB per frame, so memory stays bounded — spec NFR-PERF-005).
//!
//! ## The JSON envelope
//!
//! Every control message looks like this (spec §8.4):
//!
//! ```json
//! {
//!   "protocol": "SwiftDrop/1",
//!   "type": "transfer.offer",
//!   "request_id": "5VcQf1Pqk2Xb7...",
//!   "payload": { ... }
//! }
//! ```
//!
//! * `protocol` — must be exactly `"SwiftDrop/1"`. A different value means
//!   the peer speaks an incompatible version and we must refuse politely
//!   with `UNSUPPORTED_VERSION` (spec §8.9).
//! * `type`     — the message name (see [`messages::Message`]).
//! * `request_id` — a random, non-guessable correlation id (CSPRNG).
//! * `payload`  — the message-specific body.
//!
//! ## Module map
//!
//! * [`errors`]   — stable machine-readable error codes (spec §8.8).
//! * [`messages`] — every typed message + its JSON payload schema.
//! * [`envelope`] — wrapping/unwrapping messages in the envelope.
//! * [`framing`]  — reading/writing length-prefixed frames (async).
//! * [`validate`] — strict validation of ids, names, and transfer offers.

pub mod envelope;
pub mod errors;
pub mod framing;
pub mod messages;
pub mod validate;

/// The exact protocol identifier required in every envelope.
///
/// Major version bumps (breaking changes) produce `SwiftDrop/2` etc.
/// See spec §8.9 "Protocol compatibility".
pub const PROTOCOL_ID: &str = "SwiftDrop/1";

/// Minor version of this implementation. Used in the `hello` handshake to
/// signal compatible-but-newer capabilities within the same major version.
pub const PROTOCOL_MINOR: u16 = 0;

/// Maximum size of a single control (JSON) frame payload, in bytes.
///
/// A hard cap protects against a hostile peer sending a multi-gigabyte
/// "JSON message" to exhaust our memory (spec NFR-SEC-004/006).
pub const MAX_CONTROL_FRAME_BYTES: usize = 1024 * 1024; // 1 MiB

/// Maximum size of a single binary data frame payload, in bytes.
///
/// File content is chopped into chunks this small so the application never
/// needs to hold a whole file (or even a big part of one) in memory
/// (spec NFR-PERF-004/005: bounded buffers, backpressure).
pub const MAX_DATA_FRAME_BYTES: usize = 64 * 1024; // 64 KiB

/// Frame "kind" byte for JSON control messages.
pub const FRAME_KIND_CONTROL: u8 = 0;

/// Frame "kind" byte for raw file-content bytes.
pub const FRAME_KIND_DATA: u8 = 1;

/// Generate a fresh, cryptographically random request id.
///
/// Used for the `request_id` field of every envelope and for transfer /
/// item identifiers. 16 random bytes rendered as lowercase hex → 32
/// characters of entropy; guessing one is infeasible.
pub fn new_request_id() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 16];
    rand::rng().fill_bytes(&mut bytes);
    hex::encode(bytes)
}
