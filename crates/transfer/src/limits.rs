//! Resource limits — spec FR-XFER-020, NFR-SEC-006/007.
//!
//! Receiver-side limits are **local policy**: they come from the user's
//! settings, never from the peer. The offer validator
//! (`swiftdrop-protocol::validate::OfferLimits`) applies the message-level
//! caps; this module adds *environment* checks (disk space).

/// Bytes of headspace required above the incoming payload size before we
/// accept a transfer — filesystem metadata, journal, and slack.
pub const DISK_MARGIN_BYTES: u64 = 64 * 1024 * 1024; // 64 MiB

/// Free bytes available at `dir` (whole filesystem containing it).
///
/// Errors are surfaced to the caller — a failed space check must never be
/// treated as "plenty of room" (fail closed, spec FR-XFER-020).
pub fn free_space(dir: &std::path::Path) -> std::io::Result<u64> {
    fs2::available_space(dir)
}

/// Can `dir` accept `needed_bytes` more data (plus margin)?
pub fn has_space_for(dir: &std::path::Path, needed_bytes: u64) -> std::io::Result<bool> {
    let free = free_space(dir)?;
    Ok(free >= needed_bytes.saturating_add(DISK_MARGIN_BYTES))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn temp_dir_has_space_and_answers_sensibly() {
        let dir = tempfile::tempdir().unwrap();
        let free = free_space(dir.path()).unwrap();
        // Any real filesystem reports *some* space; if this machine has
        // <64 MiB free the "needs huge amount" check below still works.
        assert!(free > 0);

        // One byte + margin: passes on any non-full disk.
        let tiny = has_space_for(dir.path(), 1).unwrap();
        // Requesting more than any disk could have: fails unless the
        // machine truly has exabytes free (it does not).
        let impossible = has_space_for(dir.path(), u64::MAX / 2).unwrap();
        if free > DISK_MARGIN_BYTES + 1 {
            assert!(tiny, "should have space for 1 byte on a non-full disk");
            assert!(!impossible, "cannot fit half the address space");
        }
    }

    #[test]
    fn missing_directory_is_an_error_not_a_yes() {
        let err = free_space(std::path::Path::new("/definitely/not/a/dir/12345")).unwrap_err();
        // Either the OS errors outright or reports something; an error is
        // the expected response on Linux/Windows for a missing path.
        assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
    }
}
