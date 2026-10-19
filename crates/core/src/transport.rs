//! Transport abstraction (placeholder for now).
//!
//! The MVP uses `tokio::net::TcpStream` directly over TLS (already
//! handled by `swiftdrop-security` + `swiftdrop-protocol`). This module
//! exists to keep the API shape stable for a future QUIC/USB adapter
//! (spec §8.1, §13.1) without breaking `core`.

pub type TcpTransport = tokio::net::TcpStream;
