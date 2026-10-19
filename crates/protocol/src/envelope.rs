//! The versioned JSON envelope that wraps every control message (spec §8.4).

use crate::errors::ProtocolError;
use crate::messages::Message;
use crate::PROTOCOL_ID;
use serde::{Deserialize, Serialize};

/// The outer wrapper for all control messages.
///
/// ```json
/// {"protocol":"SwiftDrop/1","type":"transfer.offer",
///  "request_id":"…","payload":{ … }}
/// ```
///
/// `payload` is kept as raw JSON here and only converted to a typed struct
/// after the `type` and `protocol` fields have been checked — this lets us
/// reject *unknown* message types and *wrong* versions before doing any
/// deeper (and potentially expensive) parsing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Envelope {
    /// Must equal [`PROTOCOL_ID`] exactly.
    pub protocol: String,
    /// The message type, e.g. `transfer.offer`.
    #[serde(rename = "type")]
    pub msg_type: String,
    /// Random correlation id (CSPRNG; see [`crate::new_request_id`]).
    pub request_id: String,
    /// Message-specific body.
    pub payload: serde_json::Value,
}

impl Envelope {
    /// Wrap a typed message for sending.
    ///
    /// Serialization failures are a programming error (all payload types
    /// are serializable), so we panic there rather than invent a wire code.
    pub fn wrap(msg: &Message, request_id: String) -> Self {
        let payload = serde_json::to_value(msg)
            .expect("internal error: control payload must be serializable");
        // `Message` serializes as {type, payload} via its tag attributes;
        // extract just the payload part for the envelope shape.
        let payload = match payload {
            serde_json::Value::Object(mut map) => {
                map.remove("payload").unwrap_or(serde_json::Value::Null)
            }
            other => other,
        };
        Self {
            protocol: PROTOCOL_ID.to_string(),
            msg_type: msg.type_name().to_string(),
            request_id,
            payload,
        }
    }

    /// Parse + strictly validate an envelope received from a peer.
    ///
    /// Order of checks (fail fast, most fundamental first):
    /// 1. JSON shape (serde).
    /// 2. Protocol major version → `UNSUPPORTED_VERSION`.
    /// 3. `request_id` syntax (length + charset) → `INVALID_METADATA`.
    /// 4. Known message type + payload schema (in [`Self::into_message`]).
    pub fn parse(json: &str) -> Result<Self, ProtocolError> {
        let env: Envelope = serde_json::from_str(json)
            .map_err(|e| ProtocolError::MalformedEnvelope(format!("json decode failed: {e}")))?;

        if env.protocol != PROTOCOL_ID {
            return Err(ProtocolError::UnsupportedVersion {
                expected: PROTOCOL_ID,
            });
        }

        crate::validate::check_request_id(&env.request_id)?;

        Ok(env)
    }

    /// Convert a validated envelope into a typed [`Message`].
    ///
    /// Unknown `type` values produce a safe `PROTOCOL_ERROR` (spec §8.9).
    pub fn into_message(self) -> Result<Message, ProtocolError> {
        // Re-serialize {type, payload} into the internally-tagged enum shape
        // `Message` expects, then let serde validate the payload schema.
        let tagged = serde_json::json!({
            "type": self.msg_type,
            "payload": self.payload,
        });
        serde_json::from_value(tagged).map_err(|e| {
            // serde phrases an unrecognized internally-tagged variant as
            // "unknown variant `xyz`" — map that to the distinct error the
            // spec requires for unknown message types (§8.9).
            let text = e.to_string();
            if text.contains("unknown variant") {
                ProtocolError::UnknownMessageType(self.msg_type)
            } else {
                ProtocolError::MalformedEnvelope(format!(
                    "message '{}' payload invalid: {text}",
                    self.msg_type
                ))
            }
        })
    }
}
