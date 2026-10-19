//! User settings, persisted as JSON (spec §8.10).
//!
//! Location: `$SWIFTDROP_CONFIG_DIR/swiftdrop.json` or, if unset,
//! platform config dir: `~/.config/swiftdrop/` (Linux), `%APPDATA%\swiftdrop\`
//! (Windows), `~/Library/Application Support/swiftdrop/` (macOS). When
//! running tests, we should override via environment or a temp dir to
//! avoid polluting the developer's real config — the caller is expected
//! to set `SWIFTDROP_CONFIG_DIR` for test fixtures.
//!
//! All paths are sanitized via `swiftdrop-transfer::paths` where files
//! hit disk; here we only persist simple values.

use crate::config::{config_dir, default_collision_policy, default_dest_dir, DEFAULT_LISTEN_PORT};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use swiftdrop_transfer::paths::CollisionPolicy;

/// JSON-serializable settings persisted on disk.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "snake_case")]
pub struct Settings {
    /// Human-readable device display name, shown in peer lists.
    pub device_name: String,
    /// Directory to save received files into by default.
    pub dest_dir: PathBuf,
    /// Collision policy when a file already exists at the destination.
    pub collision_policy: CollisionPolicy,
    /// Default port to listen on for `receive` when none specified.
    pub listen_port: u16,
    /// Whether to auto-accept transfers from *trusted* peers only (P1
    /// feature). For MVP the default remains `false` (receiver must
    /// approve, spec FR-PAIR-006); trusted-only auto-accept is opt-in.
    pub auto_accept_trusted_only: bool,
    /// If true, suppress the SAS confirmation prompt for peers already
    /// trusted (the connection is still authenticated). Still logged.
    pub skip_sas_for_trusted: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            device_name: default_device_name(),
            dest_dir: default_dest_dir(),
            collision_policy: default_collision_policy(),
            listen_port: DEFAULT_LISTEN_PORT,
            auto_accept_trusted_only: false,
            skip_sas_for_trusted: false,
        }
    }
}

impl Settings {
    /// Load from disk, falling back to defaults if missing or unreadable.
    /// Does not validate that paths exist — callers ensure dest_dir.
    pub fn load() -> Self {
        config_path()
            .and_then(|p| std::fs::read_to_string(&p).ok())
            .and_then(|s| serde_json::from_str::<Self>(&s).ok())
            .unwrap_or_default()
    }

    /// Persist settings atomically (write to `.tmp` then rename) with
    /// restrictive permissions (0600) on Unix. Creates parent dirs.
    pub fn save(&self) -> std::io::Result<()> {
        let path = config_path().ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "could not determine config dir",
            )
        })?;
        let dir = path.parent().unwrap_or_else(|| std::path::Path::new("."));
        std::fs::create_dir_all(dir)?;
        let json = serde_json::to_string_pretty(self)?;
        let tmp = dir.join(format!(".swiftdrop-settings-{}.tmp", std::process::id()));
        std::fs::write(&tmp, json)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
        }
        std::fs::rename(&tmp, &path)?;
        Ok(())
    }

    /// Override the destination directory in-memory (used by CLI/tests).
    pub fn with_dest_dir(mut self, d: PathBuf) -> Self {
        self.dest_dir = d;
        self
    }
}

/// Resolve the JSON settings path. Honors `SWIFTDROP_CONFIG_DIR` override.
fn config_path() -> Option<PathBuf> {
    Some(config_dir().join("swiftdrop.json"))
}

/// Human-readable default device name (hostname or fallback).
fn default_device_name() -> String {
    hostname::get()
        .ok()
        .and_then(|h| h.into_string().ok())
        .map(|h| h.trim().to_string())
        .filter(|h| !h.is_empty())
        .unwrap_or_else(|| "SwiftDrop".to_string())
}
