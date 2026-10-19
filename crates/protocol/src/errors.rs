//! Stable, machine-readable error codes — spec §8.8.
//!
//! Two rules from the spec govern this module:
//!
//! 1. The set of codes is **stable**: once published, a code keeps its
//!    meaning, so old and new clients agree on what happened.
//! 2. Codes travel to the *remote* peer; human-friendly sentences do not.
//!    We never send stack traces, filesystem paths, or secrets over the
//!    wire (spec NFR-SEC-007) — the local UI can be more descriptive.

use serde::{Deserialize, Serialize};

/// Every error code SwiftDrop can report to a peer.
///
/// The variant names are the exact strings on the wire (SCREAMING_SNAKE,
/// matching spec §8.8). New codes may be added; receiving an unknown code
/// must be handled gracefully by showing the raw string.
///
/// Note: no `thiserror::Error` derive here — we implement `Display`
/// manually (below) because the wire string *is* the display string.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ErrorCode {
    /// Peer speaks an incompatible protocol major version.
    #[serde(rename = "UNSUPPORTED_VERSION")]
    UnsupportedVersion,
    /// A transfer requires a paired/trusted peer and we are not paired.
    #[serde(rename = "AUTHENTICATION_REQUIRED")]
    AuthenticationRequired,
    /// The user declined the pairing confirmation on either device.
    #[serde(rename = "PAIRING_REJECTED")]
    PairingRejected,
    /// The receiver's user rejected the incoming transfer offer.
    #[serde(rename = "TRANSFER_REJECTED")]
    TransferRejected,
    /// A message failed schema validation (bad ids, sizes, counts…).
    #[serde(rename = "INVALID_METADATA")]
    InvalidMetadata,
    /// A file exceeds the receiver's configured maximum file size.
    #[serde(rename = "FILE_TOO_LARGE")]
    FileTooLarge,
    /// Not enough free disk space at the destination.
    #[serde(rename = "INSUFFICIENT_STORAGE")]
    InsufficientStorage,
    /// Destination directory missing, unreadable, or not writable.
    #[serde(rename = "DESTINATION_UNAVAILABLE")]
    DestinationUnavailable,
    /// Received bytes did not match the promised SHA-256 digest.
    #[serde(rename = "INTEGRITY_MISMATCH")]
    IntegrityMismatch,
    /// A user (or the OS) cancelled the transfer.
    #[serde(rename = "TRANSFER_CANCELLED")]
    TransferCancelled,
    /// Too many connection attempts in a short window (spec NFR-SEC-006).
    #[serde(rename = "RATE_LIMITED")]
    RateLimited,
    /// Unexpected internal failure — details stay local.
    #[serde(rename = "INTERNAL_ERROR")]
    InternalError,
    /// The peer sent a message that is not valid in the current state,
    /// or a message type this version does not understand.
    #[serde(rename = "PROTOCOL_ERROR")]
    ProtocolError,
    /// An operation exceeded its time budget (handshake, prompt, idle…).
    #[serde(rename = "TIMEOUT")]
    Timeout,
}

impl std::fmt::Display for ErrorCode {
    /// Wire representation (same as serde): `Display` is used when
    /// *sending* or logging a code; serde handles the wire format.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = serde_json::to_value(self)
            .ok()
            .and_then(|v| v.as_str().map(str::to_owned))
            .unwrap_or_else(|| "INTERNAL_ERROR".to_string());
        f.write_str(&s)
    }
}

impl ErrorCode {
    /// Convert an arbitrary string received from a peer into a known code.
    ///
    /// Unknown codes (from a newer peer) map to `ProtocolError` locally,
    /// but callers that *report* a peer-supplied code should display the
    /// original string — see `RemoteError::raw_code`.
    pub fn parse(s: &str) -> Option<Self> {
        serde_json::from_value(serde_json::Value::String(s.to_string())).ok()
    }
}

/// A protocol-level failure inside this process (local side).
///
/// Distinct from [`ErrorCode`] because it carries rich local context for
/// logging and the UI; only its *code* is ever sent to the peer.
#[derive(Debug, thiserror::Error)]
pub enum ProtocolError {
    #[error("peer speaks an incompatible protocol version (expected {expected})")]
    UnsupportedVersion { expected: &'static str },

    #[error("frame of {got} bytes exceeds the {limit}-byte limit")]
    FrameTooLarge { got: usize, limit: usize },

    #[error("invalid frame kind {0}")]
    InvalidFrameKind(u8),

    #[error("connection closed while reading a frame")]
    Closed,

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("malformed JSON envelope: {0}")]
    MalformedEnvelope(String),

    #[error("unknown message type: {0}")]
    UnknownMessageType(String),

    #[error("invalid {field}: {reason}")]
    InvalidField { field: &'static str, reason: String },

    #[error("offer rejected by validation: {0}")]
    InvalidOffer(String),

    #[error("unsupported or out-of-order message {got} while expecting {expected}")]
    OutOfOrder { got: String, expected: &'static str },
}

impl ProtocolError {
    /// The wire code a peer should be told when this error occurs.
    pub fn code(&self) -> ErrorCode {
        match self {
            Self::UnsupportedVersion { .. } => ErrorCode::UnsupportedVersion,
            Self::FrameTooLarge { .. }
            | Self::InvalidFrameKind(_)
            | Self::MalformedEnvelope(_)
            | Self::UnknownMessageType(_)
            | Self::InvalidField { .. }
            | Self::InvalidOffer(_)
            | Self::OutOfOrder { .. } => ErrorCode::InvalidMetadata,
            Self::Closed | Self::Io(_) => ErrorCode::ProtocolError,
        }
    }
}

/// An error reported *by the remote peer* over the wire.
///
/// We keep the peer's raw code string (in case it comes from a newer
/// version we don't recognize) plus its safe, human-readable message.
#[derive(Debug, Clone, thiserror::Error)]
#[error("{raw_code}: {message}")]
pub struct RemoteError {
    /// The stable code, if we recognize it.
    pub code: Option<ErrorCode>,
    /// Exactly what the peer said, for display or logs.
    pub raw_code: String,
    /// Peer-supplied human message. Treated as *untrusted text* — the UI
    /// must never render it as markup or execute anything because of it.
    pub message: String,
}

impl RemoteError {
    pub fn new(raw_code: &str, message: impl Into<String>) -> Self {
        Self {
            code: ErrorCode::parse(raw_code),
            raw_code: raw_code.to_string(),
            message: message.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Golden test: every code matches spec §8.8's published strings.
    #[test]
    fn wire_strings_are_stable() {
        assert_eq!(
            serde_json::to_string(&ErrorCode::UnsupportedVersion).unwrap(),
            "\"UNSUPPORTED_VERSION\""
        );
        assert_eq!(
            serde_json::to_string(&ErrorCode::AuthenticationRequired).unwrap(),
            "\"AUTHENTICATION_REQUIRED\""
        );
        assert_eq!(
            serde_json::to_string(&ErrorCode::PairingRejected).unwrap(),
            "\"PAIRING_REJECTED\""
        );
        assert_eq!(
            serde_json::to_string(&ErrorCode::TransferRejected).unwrap(),
            "\"TRANSFER_REJECTED\""
        );
        assert_eq!(
            serde_json::to_string(&ErrorCode::InvalidMetadata).unwrap(),
            "\"INVALID_METADATA\""
        );
        assert_eq!(
            serde_json::to_string(&ErrorCode::FileTooLarge).unwrap(),
            "\"FILE_TOO_LARGE\""
        );
        assert_eq!(
            serde_json::to_string(&ErrorCode::InsufficientStorage).unwrap(),
            "\"INSUFFICIENT_STORAGE\""
        );
        assert_eq!(
            serde_json::to_string(&ErrorCode::DestinationUnavailable).unwrap(),
            "\"DESTINATION_UNAVAILABLE\""
        );
        assert_eq!(
            serde_json::to_string(&ErrorCode::IntegrityMismatch).unwrap(),
            "\"INTEGRITY_MISMATCH\""
        );
        assert_eq!(
            serde_json::to_string(&ErrorCode::TransferCancelled).unwrap(),
            "\"TRANSFER_CANCELLED\""
        );
        assert_eq!(
            serde_json::to_string(&ErrorCode::RateLimited).unwrap(),
            "\"RATE_LIMITED\""
        );
        assert_eq!(
            serde_json::to_string(&ErrorCode::InternalError).unwrap(),
            "\"INTERNAL_ERROR\""
        );
    }

    /// Round-trip: parse(wire) == original.
    #[test]
    fn parse_roundtrip() {
        for code in [
            ErrorCode::Timeout,
            ErrorCode::ProtocolError,
            ErrorCode::IntegrityMismatch,
        ] {
            let wire = code.to_string();
            assert_eq!(ErrorCode::parse(&wire), Some(code), "wire = {wire}");
        }
        assert_eq!(ErrorCode::parse("MADE_UP_CODE"), None);
    }
}
