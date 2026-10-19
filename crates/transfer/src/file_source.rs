//! The sending-side file pipeline: size-capped chunked reads and
//! whole-file pre-hashing (ADR-0007).
//!
//! The sender opens a file once, records its *declared* size from disk
//! metadata, and streams at most that many bytes. If the file changes
//! mid-transfer the receiver's digest check catches it and reports
//! `INTEGRITY_MISMATCH` — we never silently send "whatever is there now".

use sha2::{Digest, Sha256};
use std::path::Path;
use swiftdrop_protocol::errors::ErrorCode;
use tokio::io::AsyncReadExt;

/// A file opened for sending.
#[derive(Debug)]
pub struct OutgoingFile {
    file: tokio::fs::File,
    /// Bytes still to be read (counts down as we stream).
    remaining: u64,
    /// Size captured at open time — this is what the offer declares.
    declared_size: u64,
}

/// Problems preparing or streaming a local file for send.
#[derive(Debug, thiserror::Error)]
pub enum PrehashError {
    #[error("cannot read {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("{path} is larger than the configured maximum file size ({size} > {limit})")]
    TooLarge { path: String, size: u64, limit: u64 },
    #[error("the file changed size while being prepared for sending")]
    ChangedUnderneath,
}

impl PrehashError {
    pub fn code(&self) -> ErrorCode {
        match self {
            Self::Io { .. } => ErrorCode::DestinationUnavailable,
            Self::TooLarge { .. } => ErrorCode::FileTooLarge,
            Self::ChangedUnderneath => ErrorCode::InternalError,
        }
    }

    /// Short, safe, user-facing text (path reduced to its file name so we
    /// never echo absolute local paths back through the app's UI layer).
    pub fn user_message(&self) -> String {
        match self {
            Self::Io { path, .. } => {
                format!("cannot read {}", basename(path))
            }
            Self::TooLarge { path, size, limit } => format!(
                "{} is {} bytes which exceeds the {} byte limit",
                basename(path),
                size,
                limit
            ),
            Self::ChangedUnderneath => "the file changed while it was being sent".to_string(),
        }
    }
}

fn basename(path: &str) -> &str {
    path.rsplit(['/', '\\']).next().unwrap_or(path)
}

impl OutgoingFile {
    /// Open `path` for sending, refusing anything over `max_bytes`.
    pub async fn open(path: &Path, max_bytes: u64) -> Result<Self, PrehashError> {
        let meta = tokio::fs::metadata(path)
            .await
            .map_err(|source| PrehashError::Io {
                path: path.display().to_string(),
                source,
            })?;
        if !meta.is_file() {
            return Err(PrehashError::Io {
                path: path.display().to_string(),
                source: std::io::Error::new(std::io::ErrorKind::InvalidInput, "not a regular file"),
            });
        }
        let size = meta.len();
        if size > max_bytes {
            return Err(PrehashError::TooLarge {
                path: path.display().to_string(),
                size,
                limit: max_bytes,
            });
        }
        let file = tokio::fs::File::open(path)
            .await
            .map_err(|source| PrehashError::Io {
                path: path.display().to_string(),
                source,
            })?;
        Ok(Self {
            file,
            remaining: size,
            declared_size: size,
        })
    }

    /// Size declared in the offer (from metadata at open time).
    pub fn declared_size(&self) -> u64 {
        self.declared_size
    }

    /// Display name for the offer: the local file's base name. The
    /// *receiver* re-sanitizes whatever we send (it trusts nothing).
    pub fn display_name(path: &Path) -> String {
        path.file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "file".to_string())
    }

    /// Fill `buf` with up to one chunk of remaining bytes.
    ///
    /// Returns `Ok(0)` when the declared size has been fully sent.
    /// Reading is capped at `remaining`, so a file that *grows* mid-send
    /// is truncated to the offered size (and a file that *shrinks* ends
    /// short → the receiver reports a size/integrity error).
    pub async fn next_chunk(&mut self, buf: &mut [u8]) -> Result<usize, PrehashError> {
        if self.remaining == 0 {
            return Ok(0);
        }
        let want = buf.len().min(self.remaining as usize);
        let read = self
            .file
            .read(&mut buf[..want])
            .await
            .map_err(|source| PrehashError::Io {
                path: "<in transfer>".to_string(),
                source,
            })?;
        self.remaining -= read as u64;
        Ok(read)
    }

    /// True when every declared byte has been handed to the writer.
    pub fn is_done(&self) -> bool {
        self.remaining == 0
    }
}

/// Compute SHA-256 of a whole file (spec FR-XFER-012 + ADR-0007).
///
/// Run on the blocking pool because `sha2` is synchronous CPU work over
/// possibly multi-GiB files. The digest goes into the offer so the
/// receiver can verify independently.
pub async fn sha256_file(path: std::path::PathBuf) -> Result<String, PrehashError> {
    tokio::task::spawn_blocking(move || -> Result<String, PrehashError> {
        use std::io::Read;
        let mut file = std::fs::File::open(&path).map_err(|source| PrehashError::Io {
            path: path.display().to_string(),
            source,
        })?;
        let mut hasher = Sha256::new();
        // 1 MiB buffer: hashing is CPU-bound, larger reads barely help.
        let mut buf = vec![0u8; 1024 * 1024];
        loop {
            let n = file.read(&mut buf).map_err(|source| PrehashError::Io {
                path: path.display().to_string(),
                source,
            })?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
        }
        Ok(hex::encode(hasher.finalize()))
    })
    .await
    .map_err(|join| PrehashError::Io {
        path: "hash task".to_string(),
        source: std::io::Error::new(std::io::ErrorKind::Other, join.to_string()),
    })?
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[tokio::test]
    async fn streams_a_file_in_chunks_then_stops() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("data.bin");
        let content: Vec<u8> = (0..100_000u32).map(|i| (i % 251) as u8).collect();
        std::fs::write(&path, &content).unwrap();

        let mut out = OutgoingFile::open(&path, u64::MAX).await.unwrap();
        assert_eq!(out.declared_size(), content.len() as u64);

        let mut sent = Vec::new();
        let mut buf = [0u8; 64 * 1024];
        loop {
            let n = out.next_chunk(&mut buf).await.unwrap();
            if n == 0 {
                break;
            }
            sent.extend_from_slice(&buf[..n]);
        }
        assert_eq!(sent, content);
        assert!(out.is_done());
        // A second call stays at 0 (idempotent end-of-stream).
        assert_eq!(out.next_chunk(&mut buf).await.unwrap(), 0);
    }

    /// Empty files stream as exactly zero bytes.
    #[tokio::test]
    async fn empty_file_is_immediately_done() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("empty");
        std::fs::File::create(&path).unwrap();
        let mut out = OutgoingFile::open(&path, u64::MAX).await.unwrap();
        let mut buf = [0u8; 16];
        assert_eq!(out.next_chunk(&mut buf).await.unwrap(), 0);
        assert!(out.is_done());
    }

    /// Files over the configured limit are refused *before* opening.
    #[tokio::test]
    async fn size_limit_enforced() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("big.bin");
        std::fs::write(&path, vec![0u8; 1024]).unwrap();
        let err = OutgoingFile::open(&path, 512).await.unwrap_err();
        assert!(matches!(
            err,
            PrehashError::TooLarge {
                size: 1024,
                limit: 512,
                ..
            }
        ));
        assert_eq!(err.code(), ErrorCode::FileTooLarge);
    }

    /// Directories are not sendable (spec FR-XFER-001 is files-only MVP).
    #[tokio::test]
    async fn directories_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let err = OutgoingFile::open(dir.path(), u64::MAX).await.unwrap_err();
        assert!(matches!(err, PrehashError::Io { .. }));
    }

    /// Missing files produce a clean error.
    #[tokio::test]
    async fn missing_file_is_clean_error() {
        let err = OutgoingFile::open(Path::new("/no/such/file.txt"), u64::MAX)
            .await
            .unwrap_err();
        assert!(matches!(err, PrehashError::Io { .. }));
        assert!(err.user_message().contains("cannot read"));
    }

    /// Pre-hashing produces the same digest as an independent computation.
    #[tokio::test]
    async fn prehash_matches_reference() {
        let dir = tempfile::tempdir().unwrap();
        let mut path = dir.path().join("h.txt");
        {
            let mut f = std::fs::File::create(&path).unwrap();
            f.write_all(b"hash me please").unwrap();
        }
        let expected = hex::encode(Sha256::digest(b"hash me please"));
        let got = sha256_file(path.clone()).await.unwrap();
        assert_eq!(got, expected);

        path.pop(); // just to ensure no accidental reliance on mut
    }

    #[tokio::test]
    async fn prehash_empty_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("e");
        std::fs::File::create(&path).unwrap();
        let expected = hex::encode(Sha256::digest(b""));
        assert_eq!(sha256_file(path).await.unwrap(), expected);
    }
}
