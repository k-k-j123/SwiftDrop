//! The trusted-device store — which peers we've confirmed and can revoke
//! (spec FR-PAIR-005, FR-PAIR-008, §11.2).
//!
//! Storage format: `trusted_devices.json` in the config directory.
//!
//! ```json
//! [
//!   {"device_id":"<hex sha256>","display_name":"Kitchen Laptop",
//!    "added_at":1765000000}
//! ]
//! ```
//!
//! The store contains **no secrets** — only public identities we have
//! chosen to trust — so plain JSON with `0600` permissions is acceptable
//! for the MVP (see ADR-0006: SQLite arrives with transfer history).
//! Writes are atomic (temp file + rename) so a crash cannot corrupt it.

use crate::error::SecurityError;
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::{Path, PathBuf};

/// File name of the store inside the config directory (as documented in
/// the crate docs) — a const so `swiftdrop-core` and the integration
/// tests agree without a magic string.
pub const TRUST_FILE: &str = "trusted_devices.json";

/// One confirmed peer identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrustedDevice {
    /// `SHA-256(public key)` — the stable id (spec FR-PAIR-004).
    pub device_id: String,
    /// Name shown at pairing time. **Cosmetic only** — a peer can change
    /// its display name at any time; we never trust by name.
    pub display_name: String,
    /// Unix timestamp (seconds) when the user confirmed the pairing code.
    pub added_at: u64,
}

/// Load/save handle for `trusted_devices.json`.
#[derive(Debug)]
pub struct TrustStore {
    path: PathBuf,
    devices: Vec<TrustedDevice>,
}

impl TrustStore {
    /// Open the store at `path`; a missing file means "no trusted devices".
    ///
    /// A *malformed* file is an error, not an empty store: silently
    /// forgetting every trusted device would look like "trust broke" with
    /// no explanation (and would force re-pairing by surprise).
    pub fn open(path: impl Into<PathBuf>) -> Result<Self, SecurityError> {
        let path = path.into();
        if !path.exists() {
            return Ok(Self {
                path,
                devices: Vec::new(),
            });
        }
        let bytes = std::fs::read(&path)
            .map_err(|e| SecurityError::TrustStore(format!("read {}: {e}", path.display())))?;
        let devices: Vec<TrustedDevice> = serde_json::from_slice(&bytes).map_err(|e| {
            SecurityError::TrustStore(format!(
                "parse {}: {e} (delete the file to reset trust, then re-pair)",
                path.display()
            ))
        })?;
        Ok(Self { path, devices })
    }

    /// Is this device id trusted?
    pub fn contains(&self, device_id: &str) -> bool {
        self.devices.iter().any(|d| d.device_id == device_id)
    }

    /// The stored record (name at time of pairing), if trusted.
    pub fn get(&self, device_id: &str) -> Option<&TrustedDevice> {
        self.devices.iter().find(|d| d.device_id == device_id)
    }

    /// All trusted devices (for `swiftdrop trust list`).
    pub fn list(&self) -> &[TrustedDevice] {
        &self.devices
    }

    /// Add (or refresh the stored name of) a trusted device, then persist.
    ///
    /// Idempotent: pairing with an already-trusted device just updates the
    /// stored display name and keeps the original `added_at`.
    pub fn add(&mut self, device_id: &str, display_name: &str) -> Result<(), SecurityError> {
        match self.devices.iter_mut().find(|d| d.device_id == device_id) {
            Some(existing) => existing.display_name = display_name.to_string(),
            None => self.devices.push(TrustedDevice {
                device_id: device_id.to_string(),
                display_name: display_name.to_string(),
                added_at: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0),
            }),
        }
        self.save()
    }

    /// Remove trust. Future sessions with this peer require fresh pairing
    /// (spec FR-PAIR-008). Returns `true` if something was removed.
    pub fn remove(&mut self, device_id: &str) -> Result<bool, SecurityError> {
        let before = self.devices.len();
        self.devices.retain(|d| d.device_id != device_id);
        let removed = self.devices.len() != before;
        if removed {
            self.save()?;
        }
        Ok(removed)
    }

    /// Persist atomically: write a temp file, then rename over the old one
    /// (a crash mid-write leaves either the old or the new store, never a
    /// half-file).
    fn save(&self) -> Result<(), SecurityError> {
        let json = serde_json::to_vec_pretty(&self.devices)
            .map_err(|e| SecurityError::TrustStore(format!("serialize: {e}")))?;

        let tmp = self.path.with_extension("json.tmp");
        {
            let mut opts = std::fs::OpenOptions::new();
            opts.write(true).create(true).truncate(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                opts.mode(0o600);
            }
            let mut file = opts
                .open(&tmp)
                .map_err(|e| SecurityError::TrustStore(format!("write {}: {e}", tmp.display())))?;
            file.write_all(&json)
                .map_err(|e| SecurityError::TrustStore(format!("write {}: {e}", tmp.display())))?;
            file.sync_all()
                .map_err(|e| SecurityError::TrustStore(format!("sync {}: {e}", tmp.display())))?;
        }
        std::fs::rename(&tmp, &self.path).map_err(|e| {
            SecurityError::TrustStore(format!("rename {}: {e}", self.path.display()))
        })?;
        Ok(())
    }

    /// Path backing this store (used by the CLI for status output).
    pub fn path(&self) -> &Path {
        &self.path
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store_in(dir: &tempfile::TempDir) -> TrustStore {
        TrustStore::open(dir.path().join("trusted_devices.json")).unwrap()
    }

    #[test]
    fn missing_file_is_empty_store() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_in(&dir);
        assert!(store.list().is_empty());
        assert!(!store.contains("some_id"));
    }

    #[test]
    fn add_contains_remove_persist() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = store_in(&dir);
        store.add("id_aaaaaaaaaaaaaaaa", "Laptop").unwrap();
        store.add("id_bbbbbbbbbbbbbbbb", "Phone").unwrap();
        assert!(store.contains("id_aaaaaaaaaaaaaaaa"));

        // Survives reopening the file (app restart).
        let reopened = store_in(&dir);
        assert_eq!(reopened.list().len(), 2);
        assert_eq!(
            reopened.get("id_aaaaaaaaaaaaaaaa").unwrap().display_name,
            "Laptop"
        );

        // Revocation persists too (spec FR-PAIR-008).
        let mut reopened = reopened;
        assert!(reopened.remove("id_aaaaaaaaaaaaaaaa").unwrap());
        assert!(
            !reopened.remove("id_aaaaaaaaaaaaaaaa").unwrap(),
            "second remove is a no-op"
        );
        let again = store_in(&dir);
        assert!(!again.contains("id_aaaaaaaaaaaaaaaa"));
        assert!(again.contains("id_bbbbbbbbbbbbbbbb"));
    }

    #[test]
    fn re_add_updates_name_keeps_timestamp() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = store_in(&dir);
        store.add("id_xxxxxxxxxxxxxxxx", "Old Name").unwrap();
        let added_at = store.get("id_xxxxxxxxxxxxxxxx").unwrap().added_at;
        store.add("id_xxxxxxxxxxxxxxxx", "New Name").unwrap();
        let rec = store.get("id_xxxxxxxxxxxxxxxx").unwrap();
        assert_eq!(rec.display_name, "New Name");
        assert_eq!(rec.added_at, added_at);
        assert_eq!(store.list().len(), 1, "no duplicate entries");
    }

    #[test]
    fn corrupt_store_is_an_explicit_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("trusted_devices.json");
        std::fs::write(&path, b"{ not json").unwrap();
        let err = TrustStore::open(&path).unwrap_err();
        assert!(err.to_string().contains("delete the file to reset"));
    }

    #[cfg(unix)]
    #[test]
    fn store_file_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let mut store = store_in(&dir);
        store.add("id_aaaaaaaaaaaaaaaa", "X").unwrap();
        let mode = std::fs::metadata(store.path())
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }
}
