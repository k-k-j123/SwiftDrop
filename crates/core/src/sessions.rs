//! Session orchestration (MVP skeleton).
//!
//! This module owns the in-process state for a single transfer session:
//! for `receive`, accept a TCP+TLS connection, run the hello exchange,
//! authenticate via the application-layer signature bound to the TLS
//! exporter (RFC 5705), derive SAS, handle pairing/trust, wait for offer,
//! accept/reject, stream files. For `send`, discover/connect, propose
//! offer, wait for accept, stream.
//!
//! Each session holds its own cancellation token so `cancel()` propagates
//! cleanly to `tokio_util::sync::CancellationToken` (spec §9.2). All
//! long-running operations respect timeouts from `crate::config`.
//!
//! Implementation will be filled incrementally; this skeleton exists so
//! `lib.rs` compiles and the structure matches the design.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionKind {
    Send,
    Receive,
}

#[derive(Debug, Clone)]
pub struct SessionMeta {
    pub id: String,
    pub kind: SessionKind,
}
