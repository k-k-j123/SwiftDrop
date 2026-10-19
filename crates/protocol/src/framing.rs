//! Length-prefixed frame I/O — turning messages and file bytes into a
//! reliable byte stream (spec §8.7 choice 2: "a length-prefixed binary
//! stream on a dedicated authenticated TCP connection").
//!
//! Why frames instead of HTTP for the MVP? See ADR-0004
//! (`docs/adr/0004-control-and-data-framing.md`): one authenticated
//! connection for control *and* data keeps the channel-binding
//! authentication simple and the dependency list small; the spec
//! explicitly permits either choice.
//!
//! Wire layout of one frame:
//!
//! ```text
//!  0          4        5
//!  +----------+--------+------------------+
//!  | len: u32 | kind:u8| payload (len B)  |
//!  +----------+--------+------------------+
//! ```
//!
//! `len` counts the payload only (not the kind byte). Endianness is
//! big-endian (network byte order) so machines of either endianness agree.

use crate::envelope::Envelope;
use crate::errors::ProtocolError;
use crate::messages::Message;
use crate::{FRAME_KIND_CONTROL, FRAME_KIND_DATA, MAX_CONTROL_FRAME_BYTES, MAX_DATA_FRAME_BYTES};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Read exactly one frame. Returns its kind + payload bytes.
///
/// Errors instead of guessing when:
/// * length exceeds the cap for that kind (`FrameTooLarge`),
/// * the kind byte is not 0 or 1 (`InvalidFrameKind`),
/// * the stream ends mid-frame (`Closed`).
pub async fn read_frame<R: AsyncRead + Unpin>(
    reader: &mut R,
) -> Result<(u8, Vec<u8>), ProtocolError> {
    let mut len_buf = [0u8; 4];
    reader.read_exact(&mut len_buf).await.map_err(map_eof)?;
    let len = u32::from_be_bytes(len_buf) as usize;

    let mut kind_buf = [0u8; 1];
    reader.read_exact(&mut kind_buf).await.map_err(map_eof)?;
    let kind = kind_buf[0];

    // Enforce the per-kind cap BEFORE allocating (spec NFR-SEC-004:
    // never allocate memory proportional to an untrusted length header).
    let cap = match kind {
        FRAME_KIND_CONTROL => MAX_CONTROL_FRAME_BYTES,
        FRAME_KIND_DATA => MAX_DATA_FRAME_BYTES,
        other => return Err(ProtocolError::InvalidFrameKind(other)),
    };
    if len > cap {
        return Err(ProtocolError::FrameTooLarge {
            got: len,
            limit: cap,
        });
    }

    let mut payload = vec![0u8; len];
    reader.read_exact(&mut payload).await.map_err(map_eof)?;
    Ok((kind, payload))
}

/// Map a mid-frame disconnect onto a clean `Closed` error.
fn map_eof(e: std::io::Error) -> ProtocolError {
    use std::io::ErrorKind;
    match e.kind() {
        ErrorKind::UnexpectedEof => ProtocolError::Closed,
        _ => ProtocolError::Io(e),
    }
}

/// Write one raw frame.
pub async fn write_frame<W: AsyncWrite + Unpin>(
    writer: &mut W,
    kind: u8,
    payload: &[u8],
) -> Result<(), ProtocolError> {
    if payload.len() > u32::MAX as usize {
        return Err(ProtocolError::FrameTooLarge {
            got: payload.len(),
            limit: u32::MAX as usize,
        });
    }
    let cap = match kind {
        FRAME_KIND_CONTROL => MAX_CONTROL_FRAME_BYTES,
        FRAME_KIND_DATA => MAX_DATA_FRAME_BYTES,
        other => return Err(ProtocolError::InvalidFrameKind(other)),
    };
    if payload.len() > cap {
        return Err(ProtocolError::FrameTooLarge {
            got: payload.len(),
            limit: cap,
        });
    }

    writer
        .write_all(&(payload.len() as u32).to_be_bytes())
        .await?;
    writer.write_all(&[kind]).await?;
    writer.write_all(payload).await?;
    writer.flush().await?;
    Ok(())
}

/// Send a typed control message as a JSON frame.
pub async fn write_message<W: AsyncWrite + Unpin>(
    writer: &mut W,
    msg: &Message,
) -> Result<(), ProtocolError> {
    let env = Envelope::wrap(msg, crate::new_request_id());
    let json =
        serde_json::to_vec(&env).map_err(|e| ProtocolError::MalformedEnvelope(e.to_string()))?;
    write_frame(writer, FRAME_KIND_CONTROL, &json).await
}

/// Read one frame and, if it is a control frame, decode it to a message.
///
/// Returns `(Some(msg), None)` for JSON frames, `(None, Some(bytes))` for
/// data frames. Callers that only expect control messages should treat
/// `Ok((None, Some(_)))` as a protocol violation.
pub async fn read_message<R: AsyncRead + Unpin>(
    reader: &mut R,
) -> Result<(Option<Message>, Option<Vec<u8>>), ProtocolError> {
    let (kind, payload) = read_frame(reader).await?;
    match kind {
        FRAME_KIND_CONTROL => {
            let json = std::str::from_utf8(&payload)
                .map_err(|_| ProtocolError::MalformedEnvelope("payload is not UTF-8".into()))?;
            let env = Envelope::parse(json)?;
            Ok((Some(env.into_message()?), None))
        }
        FRAME_KIND_DATA => Ok((None, Some(payload))),
        other => Err(ProtocolError::InvalidFrameKind(other)),
    }
}

/// Send a local failure to the peer as a `transfer.error` message.
///
/// Never fails silently: if even this cannot be written (peer gone), the
/// error is logged by the caller via `tracing` and dropped — the local
/// error is still the one the user sees.
pub async fn write_error<W: AsyncWrite + Unpin>(
    writer: &mut W,
    transfer_id: Option<&str>,
    code: crate::errors::ErrorCode,
    message: &str,
) -> Result<(), ProtocolError> {
    let msg = Message::Error(crate::messages::TransferError {
        transfer_id: transfer_id.map(str::to_string),
        code,
        message: Some(message.to_string()),
    });
    write_message(writer, &msg).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::messages::{Hello, Message};

    /// Round-trip a frame through an in-memory buffer.
    #[tokio::test]
    async fn frame_roundtrip() {
        let mut buf: Vec<u8> = Vec::new();
        write_frame(&mut buf, FRAME_KIND_DATA, b"hello bytes")
            .await
            .unwrap();
        let mut cursor = std::io::Cursor::new(buf);
        let (kind, payload) = read_frame(&mut cursor).await.unwrap();
        assert_eq!(kind, FRAME_KIND_DATA);
        assert_eq!(payload, b"hello bytes");
    }

    /// A hostile peer declaring a giant length must be rejected *before*
    /// we try to allocate that memory.
    #[tokio::test]
    async fn rejects_oversized_length_header() {
        // Declares a 4 GiB payload of kind DATA.
        let mut frame = Vec::new();
        frame.extend_from_slice(&u32::MAX.to_be_bytes());
        frame.push(FRAME_KIND_DATA);
        let mut cursor = std::io::Cursor::new(frame);
        let err = read_frame(&mut cursor).await.unwrap_err();
        assert!(matches!(err, ProtocolError::FrameTooLarge { .. }));
    }

    #[tokio::test]
    async fn rejects_unknown_kind() {
        let mut frame = Vec::new();
        frame.extend_from_slice(&4u32.to_be_bytes());
        frame.push(9); // not 0 or 1
        frame.extend_from_slice(&[0u8; 4]);
        let mut cursor = std::io::Cursor::new(frame);
        let err = read_frame(&mut cursor).await.unwrap_err();
        assert!(matches!(err, ProtocolError::InvalidFrameKind(9)));
    }

    /// A connection that dies mid-frame is a clean `Closed`, not a panic.
    #[tokio::test]
    async fn truncated_frame_is_closed() {
        let mut cursor = std::io::Cursor::new(vec![0u8, 0u8]); // half a length header
        let err = read_frame(&mut cursor).await.unwrap_err();
        assert!(matches!(err, ProtocolError::Closed));
    }

    /// Control messages survive the write→read→decode path.
    #[tokio::test]
    async fn message_frame_roundtrip() {
        let msg = Message::Hello(Hello {
            protocol_minor: 0,
            device_id: "b".repeat(64),
            display_name: "Test".into(),
            device_key: "AyD".into(),
            auth_sig: "sig".into(),
            capabilities: Default::default(),
        });
        let mut buf: Vec<u8> = Vec::new();
        write_message(&mut buf, &msg).await.unwrap();
        let mut cursor = std::io::Cursor::new(buf);
        let (decoded, data) = read_message(&mut cursor).await.unwrap();
        assert!(data.is_none());
        assert_eq!(decoded.unwrap(), msg);
    }
}
