//! Every control message SwiftDrop devices exchange, as typed Rust structs.
//!
//! Each variant here corresponds to a row in spec §8.5's message table.
//! The schema of each payload is documented on the type; the schemas are
//! also published for humans in `docs/protocol.md`.
//!
//! Design rules:
//!
//! * Payloads are plain data — no behaviour (spec §7.4).
//! * Every field that crosses the wire is validated by [`crate::validate`]
//!   before it is trusted (spec NFR-SEC-004).
//! * Identifiers are opaque random strings, never derived from filenames,
//!   user names, or anything guessable (spec §10.3 control 4).

use serde::{Deserialize, Serialize};

/// Wire name of the `hello` message.
pub const TYPE_HELLO: &str = "hello";
/// Wire name of `pair.confirm`.
pub const TYPE_PAIR_CONFIRM: &str = "pair.confirm";
/// Wire name of `transfer.offer`.
pub const TYPE_OFFER: &str = "transfer.offer";
/// Wire name of `transfer.accept`.
pub const TYPE_ACCEPT: &str = "transfer.accept";
/// Wire name of `transfer.reject`.
pub const TYPE_REJECT: &str = "transfer.reject";
/// Wire name of `transfer.item_begin` — announces the bytes of one item.
pub const TYPE_ITEM_BEGIN: &str = "transfer.item_begin";
/// Wire name of `transfer.item_done` — receiver verified one item.
pub const TYPE_ITEM_DONE: &str = "transfer.item_done";
/// Wire name of `transfer.complete` — whole transfer verified + finalized.
pub const TYPE_COMPLETE: &str = "transfer.complete";
/// Wire name of `transfer.cancel`.
pub const TYPE_CANCEL: &str = "transfer.cancel";
/// Wire name of `transfer.progress`.
pub const TYPE_PROGRESS: &str = "transfer.progress";
/// Wire name of `transfer.error`.
pub const TYPE_ERROR: &str = "transfer.error";

/// One file as offered by the sender (spec §8.6).
///
/// `name` is a **display name only** — never a destination path. The
/// receiver sanitizes it and chooses where to write (spec FR-XFER-010,
/// spec §10.4). `sha256` is the lowercase hex digest the receiver will
/// independently recompute before marking the file complete.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OfferItem {
    /// Opaque random id, unique within the transfer.
    pub item_id: String,
    /// Display name (sanitization happens on the receiving side).
    pub name: String,
    /// Exact size in bytes the sender will stream. A sum-check against
    /// `transfer.offer.total_size_bytes` is mandatory (spec §8.6).
    pub size_bytes: u64,
    /// Lowercase hex SHA-256 of the file content. Required by this
    /// implementation: the sender pre-hashes before offering so the
    /// receiver can always verify (spec NFR-REL-001, FR-XFER-012).
    /// (The spec permits `null`; we choose "always present" — see ADR-0007.)
    pub sha256: Option<String>,
}

/// `transfer.offer` — sender → receiver, before any file bytes (spec §8.6).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TransferOffer {
    /// Opaque random transfer id (generated with [`crate::new_request_id`]).
    pub transfer_id: String,
    /// One or more files being offered (spec FR-XFER-001).
    pub items: Vec<OfferItem>,
    /// Declared total. Receivers must verify it equals the sum of item
    /// sizes *and* passes configured limits before trusting it (spec §8.6).
    pub total_size_bytes: u64,
}

/// `transfer.accept` — receiver → sender, after the user approved
/// the offer (spec FR-XFER-003).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TransferAccept {
    pub transfer_id: String,
}

/// `transfer.reject` — receiver → sender with a safe reason code.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TransferReject {
    pub transfer_id: String,
    /// Stable code such as `TRANSFER_REJECTED` or `FILE_TOO_LARGE`.
    pub code: crate::errors::ErrorCode,
    /// Optional short human message. No paths, no secrets (spec §8.8).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// `transfer.item_begin` — sender → receiver.
///
/// Announces "the next `size_bytes` data frames belong to this item".
/// Required so one connection can carry several files back to back
/// (multi-file transfers are P0: spec FR-XFER-001). The size is restated
/// here so the receiver can cross-check it against the original offer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TransferItemBegin {
    pub transfer_id: String,
    pub item_id: String,
    pub size_bytes: u64,
}

/// `transfer.item_done` — receiver → sender, after verifying one item.
///
/// The receiver only sends this once the file is size-checked, digest-
/// checked, fsync'd, and atomically renamed into place (spec FR-XFER-009,
/// FR-XFER-012). If `ok` is false, `code` explains why and the sender
/// should treat the item as failed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TransferItemDone {
    pub transfer_id: String,
    pub item_id: String,
    /// True only after full verification + finalization succeeded.
    pub ok: bool,
    /// Digest the receiver computed over the received bytes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    /// Failure code (only when `ok == false`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<crate::errors::ErrorCode>,
}

/// `transfer.complete` — receiver → sender, when *all* items verified
/// and were finalized. This is the only message that may lead both sides
/// to report success (spec FR-XFER-008: failures never look completed).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TransferComplete {
    pub transfer_id: String,
    /// Names the receiver actually saved (post-sanitization, post-collision
    /// policy), so the sender's history shows the real outcome.
    #[serde(default)]
    pub saved_names: Vec<String>,
}

/// `transfer.cancel` — either direction, at any time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TransferCancel {
    pub transfer_id: String,
    /// Why: user pressed cancel, peer went away, disk error…
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<crate::errors::ErrorCode>,
}

/// `transfer.progress` — receiver → sender, during streaming.
///
/// Reports **bytes durably written by the receiver**, not bytes the sender
/// pushed into the socket. The spec is explicit that these differ and that
/// the receiver's view is the honest one (spec §9.3). The sender's UI
/// displays min(bytes_sent, bytes_reported_written).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TransferProgress {
    pub transfer_id: String,
    /// Bytes written (to temporary storage) for the current item.
    pub item_id: String,
    pub bytes_written: u64,
    /// Bytes written across all items of this transfer so far.
    pub total_written: u64,
}

/// `transfer.error` — either direction. Reports a typed failure.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TransferError {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transfer_id: Option<String>,
    pub code: crate::errors::ErrorCode,
    /// Short, safe human message (never a stack trace or path).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// `hello` — both directions, first message after the TLS handshake.
///
/// Carries the proof that the sender owns its claimed device identity:
/// `auth_sig` is an ECDSA P-256/SHA-256 signature over
/// `TLS-exporter("swiftdrop-auth-v1") || device_key_bytes`, so it is
/// bound to *this specific TLS session* and cannot be replayed (see
/// ADR-0002). See `swiftdrop-security` for how it is produced/checked.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hello {
    /// Protocol minor version the peer implements (major is fixed by the
    /// envelope's `protocol` field).
    pub protocol_minor: u16,
    /// Stable device identity: SHA-256 of the device's P-256 public key
    /// point, lowercase hex (64 chars). Persisted across restarts
    /// (spec FR-PAIR-004) because the key itself is persisted.
    pub device_id: String,
    /// Human-friendly device name. **Cosmetic and unauthenticated** —
    /// trust decisions use `device_id`, never this string.
    pub display_name: String,
    /// The device's P-256 public key as a SEC1 *compressed* point
    /// (33 bytes, base64). This is the key the signature verifies with.
    pub device_key: String,
    /// base64 of the ECDSA P-256 signature described above.
    pub auth_sig: String,
    /// Optional capability flags (reserved; unknown flags are ignored).
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub capabilities: std::collections::BTreeMap<String, bool>,
}

/// `pair.confirm` — both directions during pairing.
///
/// Sent *after* both users have seen the same short authentication string
/// (SAS). Trust is recorded only when **both** sides confirm `accepted`
/// (spec FR-PAIR-003/009).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairConfirm {
    pub accepted: bool,
    /// Optional, safe reason shown to the other side when declining.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Every control message, tagged with its wire name.
///
/// This is the closed set of understood messages. A message type not in
/// this enum causes a `PROTOCOL_ERROR` reply (spec §8.9: unknown message
/// types must return a safe protocol error, never be guessed at).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "payload", rename_all = "snake_case")]
pub enum Message {
    Hello(Hello),
    #[serde(rename = "pair.confirm")]
    PairConfirm(PairConfirm),
    #[serde(rename = "transfer.offer")]
    Offer(TransferOffer),
    #[serde(rename = "transfer.accept")]
    Accept(TransferAccept),
    #[serde(rename = "transfer.reject")]
    Reject(TransferReject),
    #[serde(rename = "transfer.item_begin")]
    ItemBegin(TransferItemBegin),
    #[serde(rename = "transfer.item_done")]
    ItemDone(TransferItemDone),
    #[serde(rename = "transfer.complete")]
    Complete(TransferComplete),
    #[serde(rename = "transfer.cancel")]
    Cancel(TransferCancel),
    #[serde(rename = "transfer.progress")]
    Progress(TransferProgress),
    #[serde(rename = "transfer.error")]
    Error(TransferError),
}

impl Message {
    /// The wire `type` string for this message.
    pub fn type_name(&self) -> &'static str {
        match self {
            Self::Hello(_) => TYPE_HELLO,
            Self::PairConfirm(_) => TYPE_PAIR_CONFIRM,
            Self::Offer(_) => TYPE_OFFER,
            Self::Accept(_) => TYPE_ACCEPT,
            Self::Reject(_) => TYPE_REJECT,
            Self::ItemBegin(_) => TYPE_ITEM_BEGIN,
            Self::ItemDone(_) => TYPE_ITEM_DONE,
            Self::Complete(_) => TYPE_COMPLETE,
            Self::Cancel(_) => TYPE_CANCEL,
            Self::Progress(_) => TYPE_PROGRESS,
            Self::Error(_) => TYPE_ERROR,
        }
    }

    /// Map an unknown wire type name to a safe protocol error (spec §8.9).
    pub fn unknown_type(name: &str) -> crate::errors::ProtocolError {
        crate::errors::ProtocolError::UnknownMessageType(name.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::envelope::Envelope;
    use crate::PROTOCOL_ID;

    /// Every message must survive a JSON round-trip with its type tag
    /// intact — these are the "golden fixtures" spec §8.9 asks for.
    #[test]
    fn message_json_roundtrip_all_variants() {
        let msgs = vec![
            Message::Hello(Hello {
                protocol_minor: 0,
                device_id: "a".repeat(64),
                display_name: "Kitchen Laptop".into(),
                device_key: "AyD//////////".into(), // illustrative base64
                auth_sig: "MEUCIQDx".into(),
                capabilities: Default::default(),
            }),
            Message::PairConfirm(PairConfirm {
                accepted: true,
                reason: None,
            }),
            Message::Offer(TransferOffer {
                transfer_id: "t1".into(),
                items: vec![OfferItem {
                    item_id: "i1".into(),
                    name: "holiday photo ☃.jpg".into(),
                    size_bytes: 1234,
                    sha256: Some(
                        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855".into(),
                    ),
                }],
                total_size_bytes: 1234,
            }),
            Message::Accept(TransferAccept {
                transfer_id: "t1".into(),
            }),
            Message::Reject(TransferReject {
                transfer_id: "t1".into(),
                code: crate::errors::ErrorCode::TransferRejected,
                message: Some("No thanks".into()),
            }),
            Message::ItemBegin(TransferItemBegin {
                transfer_id: "t1".into(),
                item_id: "i1".into(),
                size_bytes: 1234,
            }),
            Message::ItemDone(TransferItemDone {
                transfer_id: "t1".into(),
                item_id: "i1".into(),
                ok: true,
                sha256: None,
                code: None,
            }),
            Message::Complete(TransferComplete {
                transfer_id: "t1".into(),
                saved_names: vec!["holiday photo ☃.jpg".into()],
            }),
            Message::Cancel(TransferCancel {
                transfer_id: "t1".into(),
                code: Some(crate::errors::ErrorCode::TransferCancelled),
            }),
            Message::Progress(TransferProgress {
                transfer_id: "t1".into(),
                item_id: "i1".into(),
                bytes_written: 64,
                total_written: 64,
            }),
            Message::Error(TransferError {
                transfer_id: None,
                code: crate::errors::ErrorCode::InternalError,
                message: Some("something safe".into()),
            }),
        ];

        for msg in msgs {
            let wire = msg.type_name().to_string();
            let env = Envelope::wrap(&msg, crate::new_request_id());
            let json = serde_json::to_string(&env).expect("serialize");
            let back = Envelope::parse(&json).expect("parse envelope");
            assert_eq!(back.protocol, PROTOCOL_ID);
            let decoded = back.into_message().expect("decode message");
            assert_eq!(decoded, msg, "round-trip mismatch for {wire}");
        }
    }

    /// An envelope with a wrong protocol id must be refused at parse time.
    #[test]
    fn envelope_rejects_wrong_protocol() {
        let json = r#"{"protocol":"SwiftDrop/99","type":"hello","request_id":"x","payload":{}}"#;
        let err = Envelope::parse(json).expect_err("version must be rejected");
        assert!(matches!(
            err,
            crate::errors::ProtocolError::UnsupportedVersion { .. }
        ));
    }
}
