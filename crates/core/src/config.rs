//! Runtime and connection configuration defaults.
//!
//! The values here are sensible defaults for the MVP. CLI flags override
//! settings, and environment variables override CLI/impl defaults only
//! where explicitly documented (e.g. `SWIFTDROP_CONFIG_DIR`,
//! `SWIFTDROP_DEST_DIR`).
//!
//! Nothing network-related or file-list-related is exposed to discovery
//! (see `swiftdrop-discovery` docs).

/// Default TCP port for a `receive` session listener when the user does
/// not specify one. Use the discovery default for consistency.
pub const DEFAULT_LISTEN_PORT: u16 = swiftdrop_discovery::DEFAULT_PORT;

/// Default destination directory for received files (runtime-resolved):
/// platform Downloads if available, else the user's home dir.
pub fn default_dest_dir() -> std::path::PathBuf {
    if let Some(mut d) = dirs::download_dir() {
        d = d.canonicalize().unwrap_or(d);
        d
    } else {
        dirs::home_dir().unwrap_or_else(|| std::path::PathBuf::from("."))
    }
}

/// Default collision policy (spec FR-XFER-011): never overwrite silently.
pub fn default_collision_policy() -> swiftdrop_transfer::paths::CollisionPolicy {
    swiftdrop_transfer::paths::CollisionPolicy::Rename
}

/// Timeout for the initial TLS+handshake before giving up (milliseconds).
pub const HANDSHAKE_TIMEOUT_MS: u64 = 10_000;

/// Timeout for auth/SAS confirmation steps (milliseconds).
pub const PAIRING_TIMEOUT_MS: u64 = 30_000;

/// Send/receive chunk deadline (milliseconds) — prevents a stalled
/// peer from hanging the entire transfer.
pub const STREAM_IO_TIMEOUT_MS: u64 = 30_000;

/// Maximum number of files per offer (spec §8.2). Capped to prevent
/// O(N) exhaustion in one offer.
pub const MAX_ITEMS_PER_OFFER: usize = 1000;
