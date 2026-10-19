//! # SwiftDrop transfer — state machine, safe file handling, streaming
//!
//! This crate owns everything about *moving bytes safely*:
//!
//! * [`state`] — the transfer state machine from spec §9.1/9.2. Every
//!   state change goes through [`state::TransferStateTracker::transition`];
//!   ad-hoc state mutation in UI callbacks is forbidden by the spec.
//! * [`paths`] — turning hostile sender-supplied names into safe local
//!   filenames, plus the collision policy (spec FR-XFER-010/011, §10.4).
//! * [`file_sink`] — the receiving-side pipeline: temp file → bounded
//!   chunk writes → SHA-256 → fsync → atomic rename (spec FR-XFER-009/012).
//! * [`file_source`] — the sending-side pipeline: size-capped chunked
//!   reads and whole-file pre-hashing (ADR-0007).
//! * [`progress`] — smoothed speed/ETA display helpers (spec NFR-PERF-007).
//! * [`limits`] — disk-space checks (spec FR-XFER-020).
//!
//! It knows nothing about sockets or UIs: `swiftdrop-core` drives these
//! pieces over an authenticated connection, and the CLI renders them.
//!
//! Re-exported for convenience: [`CancellationToken`] — the one flag that
//! propagates a user's Cancel to every task involved in a transfer.

pub mod file_sink;
pub mod file_source;
pub mod limits;
pub mod paths;
pub mod progress;
pub mod state;

pub use file_sink::{FinalizedFile, IncomingFile, ReceiveError};
pub use file_source::{sha256_file, OutgoingFile, PrehashError};
pub use state::{StateError, TransferState, TransferStateTracker};
pub use tokio_util::sync::CancellationToken;

/// Chunk size used when streaming file content (must equal the protocol's
/// `MAX_DATA_FRAME_BYTES` so one chunk == one frame).
pub const CHUNK_SIZE: usize = swiftdrop_protocol::MAX_DATA_FRAME_BYTES;
