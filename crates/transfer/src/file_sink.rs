//! The receiving-side file pipeline — spec FR-XFER-009, FR-XFER-010,
//! FR-XFER-012, §10.4.
//!
//! One [`IncomingFile`] per offered item. Lifecycle:
//!
//! 1. **create** — sanitize the sender's name, resolve collisions, then
//!    open a *randomly named* temp file (`.swiftdrop-<pid>-<rand>.part`)
//!    with `create_new` (exclusive) inside the destination directory.
//! 2. **write_chunk** — bounded 64 KiB chunks, hashed with SHA-256 as
//!    they arrive; a chunk that would exceed the promised size aborts the
//!    file (a lying sender cannot make us write more than declared).
//! 3. **finalize** — require exact byte count, compare the digest, flush
//!    to disk (`sync_all`), then rename temp → final. Only after that
//!    rename does the file become "real": until then it is visibly a
//!    `.part` file (spec NFR-REL-002: partial ≠ complete).
//! 4. **abort** (any failure/cancel) — delete the temp file.
//!
//! The receiver never opens, previews, or executes received content
//! (spec FR-XFER-018, NFR-SEC-011) — this module only writes bytes.

use crate::paths::{reserve_final_name, sanitize_file_name, temp_file_path, CollisionPolicy};
use sha2::{Digest, Sha256};
use std::io::Write;
use std::path::{Path, PathBuf};
use swiftdrop_protocol::errors::ErrorCode;

/// Failures while receiving a file. Each maps to a stable wire code so
/// the sender gets a machine-readable reason (spec §8.8).
#[derive(Debug, thiserror::Error)]
pub enum ReceiveError {
    /// Hostile/unusable filename (path traversal etc.).
    #[error("unsafe filename rejected: {0}")]
    UnsafeName(#[from] crate::paths::PathSafetyError),

    /// Local filesystem problem (permissions, disk full, device gone…).
    #[error("filesystem error: {0}")]
    Io(#[from] std::io::Error),

    /// Peer sent more bytes than it declared in the offer.
    #[error("peer sent more bytes than declared ({got} > {declared})")]
    TooManyBytes { declared: u64, got: u64 },

    /// Stream ended before the declared size arrived.
    #[error("stream ended early: got {got} of {expected} bytes")]
    ShortStream { expected: u64, got: u64 },

    /// Received content did not hash to the promised digest.
    #[error("integrity check failed (expected {expected}, computed {computed})")]
    DigestMismatch { expected: String, computed: String },

    /// Destination has less free space than the offer needs.
    #[error("not enough free space at destination ({needed} bytes required)")]
    InsufficientStorage { needed: u64 },

    /// Collision policy said "skip".
    #[error("destination already contains this file (policy: skip)")]
    CollisionSkip,

    /// A temp file could not be created because one already exists with
    /// the same random name (astronomically unlikely) or the directory
    /// is unusable.
    #[error("could not create a temporary file in the destination")]
    TempCreation,
}

impl ReceiveError {
    /// Stable code to report to the sender.
    pub fn code(&self) -> ErrorCode {
        match self {
            Self::UnsafeName(_) | Self::TooManyBytes { .. } | Self::ShortStream { .. } => {
                ErrorCode::InvalidMetadata
            }
            Self::Io(_) | Self::TempCreation => ErrorCode::DestinationUnavailable,
            Self::DigestMismatch { .. } => ErrorCode::IntegrityMismatch,
            Self::InsufficientStorage { .. } => ErrorCode::InsufficientStorage,
            Self::CollisionSkip => ErrorCode::TransferRejected,
        }
    }
}

/// One in-progress incoming file (see module docs).
#[derive(Debug)]
pub struct IncomingFile {
    /// Where the file will live once verified.
    final_path: PathBuf,
    /// Where bytes go *until* verified. Always `.part`-suffixed and
    /// hidden (leading dot on Unix) so users can't mistake it for done.
    temp_path: PathBuf,
    /// `Option` so `finalize` can take the handle out, close it, and then
    /// still delete the temp file on a failed verification.
    file: Option<std::fs::File>,
    expected_size: u64,
    written: u64,
    hasher: Sha256,
    policy: CollisionPolicy,
    /// The sanitized display name we negotiated for this item.
    safe_name: String,
}

/// A file that passed every check and has been renamed into place.
#[derive(Debug, Clone)]
pub struct FinalizedFile {
    /// Final absolute-ish path (as built from the local destination dir).
    pub path: PathBuf,
    /// Name as saved (post-sanitization, post-collision policy).
    pub saved_name: String,
    /// SHA-256 the receiver computed over the stored bytes.
    pub sha256: String,
    pub size_bytes: u64,
}

impl IncomingFile {
    /// Open the pipeline for one item (step 1 of the module docs).
    ///
    /// `dest_dir` must already exist and be writable; `raw_name` is the
    /// untrusted sender-supplied display name.
    pub fn create(
        dest_dir: &Path,
        raw_name: &str,
        expected_size: u64,
        policy: CollisionPolicy,
    ) -> Result<Self, ReceiveError> {
        let safe_name = sanitize_file_name(raw_name)?;

        // Disk-space check with a small margin for filesystem overhead.
        let free = fs2::available_space(dest_dir)?;
        let needed = expected_size.saturating_add(64 * 1024 * 1024);
        if free < needed {
            return Err(ReceiveError::InsufficientStorage {
                needed: expected_size,
            });
        }

        // Reserve the final name now so the user sees consistent naming,
        // while the actual write goes to the temp file.
        let final_name = match reserve_final_name(dest_dir, &safe_name, policy)? {
            crate::paths::ReserveResult::Use(name) => name,
            crate::paths::ReserveResult::Skip => return Err(ReceiveError::CollisionSkip),
        };

        let temp_path = temp_file_path(dest_dir);
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create_new(true); // exclusive: no symlink games
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o644); // received files: owner-writable, group/other-readable
        }
        let file = opts
            .open(&temp_path)
            .map_err(|_| ReceiveError::TempCreation)?;

        Ok(Self {
            final_path: dest_dir.join(&final_name),
            temp_path,
            file: Some(file),
            expected_size,
            written: 0,
            hasher: Sha256::new(),
            policy,
            safe_name: final_name,
        })
    }

    /// Bytes accepted so far (drives the receiver's progress display).
    pub fn bytes_written(&self) -> u64 {
        self.written
    }

    /// The safe name this file will be saved as.
    pub fn safe_name(&self) -> &str {
        &self.safe_name
    }

    /// Append one chunk (step 2). Refuses to exceed the declared size.
    pub fn write_chunk(&mut self, chunk: &[u8]) -> Result<(), ReceiveError> {
        let new_total =
            self.written
                .checked_add(chunk.len() as u64)
                .ok_or(ReceiveError::TooManyBytes {
                    declared: self.expected_size,
                    got: u64::MAX,
                })?;
        if new_total > self.expected_size {
            return Err(ReceiveError::TooManyBytes {
                declared: self.expected_size,
                got: new_total,
            });
        }
        let file = self
            .file
            .as_mut()
            .expect("write_chunk called after finalize took the file handle");
        file.write_all(chunk)?;
        self.hasher.update(chunk);
        self.written = new_total;
        Ok(())
    }

    /// Verify + persist (step 3). Consumes `self`.
    ///
    /// Order matters and follows spec NFR-REL-001: exact size check →
    /// digest check → `sync_all` (durability) → rename (atomicity). A
    /// failure at any point removes the temp file so no half-file
    /// survives (spec §10.4: "on failure, remove temporary files").
    pub fn finalize(
        mut self,
        expected_digest: Option<&str>,
    ) -> Result<FinalizedFile, ReceiveError> {
        let result = self.finalize_inner(expected_digest);
        if result.is_err() {
            // Close the handle first so the delete can succeed on Windows.
            drop(self.file.take());
            let _ = std::fs::remove_file(&self.temp_path);
        }
        result
    }

    fn finalize_inner(
        &mut self,
        expected_digest: Option<&str>,
    ) -> Result<FinalizedFile, ReceiveError> {
        // 1. Exact size.
        if self.written != self.expected_size {
            return Err(ReceiveError::ShortStream {
                expected: self.expected_size,
                got: self.written,
            });
        }

        // 2. Digest.
        let computed = {
            let hasher = self.hasher.clone();
            hex::encode(hasher.finalize())
        };
        if let Some(expected) = expected_digest {
            if !expected.eq_ignore_ascii_case(&computed) {
                return Err(ReceiveError::DigestMismatch {
                    expected: expected.to_string(),
                    computed,
                });
            }
        }

        // 3. Durability before rename: the data must be on disk before the
        //    name says "done".
        let file = self
            .file
            .as_mut()
            .expect("file handle present until success");
        file.sync_all()?;
        drop(self.file.take());

        // 4. Atomic-ish rename with a final collision re-check (small
        //    window; documented in docs/security.md as an accepted MVP
        //    limitation because std has no portable rename-if-absent).
        let final_path = self.resolve_rename_target()?;
        std::fs::rename(&self.temp_path, &final_path)?;

        // Best-effort directory sync so the rename survives power loss on
        // POSIX; unsupported platforms simply ignore it (documented).
        #[cfg(unix)]
        if let Some(parent) = final_path.parent() {
            if let Ok(dir) = std::fs::File::open(parent) {
                let _ = dir.sync_all();
            }
        }

        Ok(FinalizedFile {
            path: final_path,
            saved_name: self.safe_name.clone(),
            sha256: computed,
            size_bytes: self.written,
        })
    }

    /// Re-check for collisions immediately before renaming.
    fn resolve_rename_target(&self) -> Result<PathBuf, ReceiveError> {
        if self.policy == CollisionPolicy::Overwrite || !self.final_path.exists() {
            return Ok(self.final_path.clone());
        }
        // Something appeared since `create` — re-run the policy.
        match reserve_final_name(
            self.final_path.parent().unwrap_or(Path::new(".")),
            &self.safe_name,
            self.policy,
        )? {
            crate::paths::ReserveResult::Use(name) => Ok(self
                .final_path
                .parent()
                .unwrap_or(Path::new("."))
                .join(name)),
            crate::paths::ReserveResult::Skip => Err(ReceiveError::CollisionSkip),
        }
    }

    /// Abort (step 4): delete the temp file and forget the pipeline.
    /// Called on cancel, peer disconnect, integrity failure, disk errors —
    /// any path that must not leave a half-file lying around (spec §9.2:
    /// cancellation reaches temp-file cleanup).
    pub fn abort(self) {
        if let Err(e) = std::fs::remove_file(&self.temp_path) {
            if e.kind() != std::io::ErrorKind::NotFound {
                tracing::warn!(
                    error = %e,
                    "failed to remove temp file during abort (it remains a .part file)"
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn incoming(dir: &Path, name: &str, size: u64) -> IncomingFile {
        IncomingFile::create(dir, name, size, CollisionPolicy::Rename).unwrap()
    }

    #[test]
    fn happy_path_finalizes_with_correct_digest() {
        let dir = tempfile::tempdir().unwrap();
        let content = b"hello SwiftDrop";
        let digest = hex::encode(Sha256::digest(content));

        let mut f = incoming(dir.path(), "greeting.txt", content.len() as u64);
        f.write_chunk(content).unwrap();
        let done = f.finalize(Some(&digest)).unwrap();

        assert_eq!(std::fs::read(&done.path).unwrap(), content);
        assert_eq!(done.saved_name, "greeting.txt");
        assert_eq!(done.sha256, digest);
        assert_eq!(done.size_bytes, content.len() as u64);
        // No temp files left behind.
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    /// Empty files must work (spec FR-XFER-017).
    #[test]
    fn empty_file_finalizes() {
        let dir = tempfile::tempdir().unwrap();
        let digest = hex::encode(Sha256::digest(b""));
        let f = incoming(dir.path(), "empty.dat", 0);
        let done = f.finalize(Some(&digest)).unwrap();
        assert_eq!(done.size_bytes, 0);
        assert_eq!(std::fs::read(&done.path).unwrap(), b"");
    }

    /// A wrong digest must fail AND leave no final file (FR-XFER-012).
    #[test]
    fn digest_mismatch_leaves_no_file() {
        let dir = tempfile::tempdir().unwrap();
        let mut f = incoming(dir.path(), "tampered.bin", 5);
        f.write_chunk(b"evil!").unwrap();
        let err = f
            .finalize(Some(
                "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
            ))
            .unwrap_err();
        assert!(matches!(err, ReceiveError::DigestMismatch { .. }));
        assert_eq!(err.code(), ErrorCode::IntegrityMismatch);
        // Directory must be empty: temp was cleaned during failure.
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    /// Short streams are refused at finalize (partial ≠ complete).
    #[test]
    fn short_stream_fails() {
        let dir = tempfile::tempdir().unwrap();
        let mut f = incoming(dir.path(), "partial.txt", 100);
        f.write_chunk(b"only ten").unwrap();
        let err = f.finalize(None).unwrap_err();
        assert!(matches!(
            err,
            ReceiveError::ShortStream {
                expected: 100,
                got: 8
            }
        ));
        // Cleanup happens on abort paths; finalize's size failure keeps the
        // temp for the caller to abort — simulate that here.
        // (Re-create to verify abort works:)
        let f = incoming(dir.path(), "partial.txt", 100);
        f.abort();
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    /// A sender exceeding the declared size is stopped mid-write.
    #[test]
    fn excess_bytes_rejected_immediately() {
        let dir = tempfile::tempdir().unwrap();
        let mut f = incoming(dir.path(), "liar.bin", 4);
        f.write_chunk(b"1234").unwrap();
        let err = f.write_chunk(b"5").unwrap_err();
        assert!(matches!(
            err,
            ReceiveError::TooManyBytes {
                declared: 4,
                got: 5
            }
        ));
        f.abort();
    }

    /// Hostile names are refused before anything touches disk.
    #[test]
    fn hostile_name_refused() {
        let dir = tempfile::tempdir().unwrap();
        let err = IncomingFile::create(dir.path(), "..", 1, CollisionPolicy::Rename).unwrap_err();
        assert!(matches!(err, ReceiveError::UnsafeName(_)));
        assert_eq!(err.code(), ErrorCode::InvalidMetadata);
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    /// Existing files are renamed, never silently overwritten (FR-XFER-011).
    #[test]
    fn collision_renames_and_preserves_original() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("notes.txt"), b"ORIGINAL").unwrap();

        let content = b"new content";
        let digest = hex::encode(Sha256::digest(content));
        let mut f = incoming(dir.path(), "notes.txt", content.len() as u64);
        f.write_chunk(content).unwrap();
        let done = f.finalize(Some(&digest)).unwrap();

        assert_eq!(done.saved_name, "notes (1).txt");
        assert_eq!(
            std::fs::read(dir.path().join("notes.txt")).unwrap(),
            b"ORIGINAL"
        );
        assert_eq!(std::fs::read(&done.path).unwrap(), content);
    }

    /// Skip policy: no file written at all.
    #[test]
    fn skip_policy_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("dup.bin"), b"keep me").unwrap();
        let err =
            IncomingFile::create(dir.path(), "dup.bin", 3, CollisionPolicy::Skip).unwrap_err();
        assert!(matches!(err, ReceiveError::CollisionSkip));
        assert_eq!(
            std::fs::read(dir.path().join("dup.bin")).unwrap(),
            b"keep me"
        );
    }

    /// Overwrite policy replaces only when explicitly chosen.
    #[test]
    fn overwrite_policy_replaces() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("swap.bin"), b"old").unwrap();
        let content = b"new!";
        let digest = hex::encode(Sha256::digest(content));
        let mut f = IncomingFile::create(
            dir.path(),
            "swap.bin",
            content.len() as u64,
            CollisionPolicy::Overwrite,
        )
        .unwrap();
        f.write_chunk(content).unwrap();
        let done = f.finalize(Some(&digest)).unwrap();
        assert_eq!(std::fs::read(&done.path).unwrap(), content);
    }

    /// Unicode names pass through sanitization intact.
    #[test]
    fn unicode_name_preserved() {
        let dir = tempfile::tempdir().unwrap();
        let content = "こんにちは".as_bytes();
        let digest = hex::encode(Sha256::digest(content));
        let mut f = incoming(dir.path(), "挨拶 🎌.txt", content.len() as u64);
        f.write_chunk(content).unwrap();
        let done = f.finalize(Some(&digest)).unwrap();
        assert_eq!(done.saved_name, "挨拶 🎌.txt");
        assert!(done.path.exists());
    }

    /// Aborting mid-transfer leaves nothing but the destination dir itself.
    #[test]
    fn abort_cleans_temp() {
        let dir = tempfile::tempdir().unwrap();
        let mut f = incoming(dir.path(), "cancelled.txt", 10);
        f.write_chunk(b"partial").unwrap();
        f.abort();
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    /// Missing destination directory is a clean error, not a panic.
    #[test]
    fn missing_dest_is_clean_error() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("nope");
        let err = IncomingFile::create(&missing, "x.txt", 1, CollisionPolicy::Rename).unwrap_err();
        assert!(matches!(err, ReceiveError::Io(_)));
        assert_eq!(err.code(), ErrorCode::DestinationUnavailable);
    }
}
