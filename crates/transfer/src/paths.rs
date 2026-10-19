//! Safe handling of sender-supplied filenames — spec §10.4, FR-XFER-010,
//! FR-XFER-011, NFR-SEC-005.
//!
//! A remote peer is *hostile until proven otherwise*. It may send names
//! like `../../.bashrc`, `C:\Windows\System32\evil.dll`, `a/b/c`, `NUL`,
//! `file.txt:ads`, or names full of control characters. This module turns
//! any of those into a single, boring, safe filename that stays inside
//! the chosen destination directory.
//!
//! ## Policy (documented decisions)
//!
//! * Path separators (`/`, `\`) → the **last** component wins
//!   ("strip", not "reject"): senders frequently (and innocently) send
//!   relative paths for folders.
//! * `..`, `.`, empty, or names that sanitize to empty → **rejected**.
//! * Windows-reserved characters (`<>:"|?*`), control characters, NUL →
//!   replaced with `_`.
//! * Trailing dots/spaces (Windows silently strips them → collisions) →
//!   trimmed.
//! * Reserved Windows device names (`CON`, `NUL`, `COM1`…) → prefixed
//!   with `_`.
//! * Length capped at 200 UTF-8 bytes with truncation at a char boundary.
//! * Symlinks are never created and temp files are always created with
//!   `create_new` (exclusive) so a pre-planted symlink cannot redirect
//!   writes (spec §10.4).
//!
//! Note the *destination directory itself* always comes from local
//! settings — never from the network.

use std::path::{Path, PathBuf};

/// Longest allowed filename we produce (bytes of UTF-8).
const MAX_NAME_BYTES: usize = 200;

/// What to do when the target filename already exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CollisionPolicy {
    /// Append " (1)", " (2)", … before the extension. **Default** —
    /// never loses an existing file.
    Rename,
    /// Do not save this file; report it as skipped.
    Skip,
    /// Replace the existing file. Only when the user opted in.
    Overwrite,
}

impl std::str::FromStr for CollisionPolicy {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "rename" => Ok(Self::Rename),
            "skip" => Ok(Self::Skip),
            "overwrite" => Ok(Self::Overwrite),
            other => Err(format!(
                "unknown collision policy {other:?} (expected rename|skip|overwrite)"
            )),
        }
    }
}

/// Why a name was refused. Maps to `INVALID_METADATA` on the wire.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum PathSafetyError {
    #[error("filename is empty after sanitization")]
    Empty,
    #[error("filename is a traversal token ('.' or '..')")]
    Traversal,
    #[error("filename contains no usable characters")]
    Unusable,
}

/// Turn an arbitrary sender-supplied name into a safe local filename.
///
/// This is a *pure* function — it never touches the filesystem, so it can
/// be unit-tested exhaustively (spec §14.1).
pub fn sanitize_file_name(raw: &str) -> Result<String, PathSafetyError> {
    // 1. Keep only the last path component (handles / and \ alike).
    let last = raw
        .split(['/', '\\'])
        .rfind(|s| !s.is_empty())
        .unwrap_or("");

    // 2. Colon handling. Two distinct hostile/odd cases:
    //    * Drive-relative name ("C:evil.txt") — on Windows, joining this
    //      to the destination dir would escape it entirely (a drive prefix
    //      replaces the base path), so strip the "X:" drive prefix and
    //      keep the actual filename. Only recognized when there is exactly
    //      one colon and a single letter before it; anything else falls
    //      through to the general rule below.
    //    * Alternate-data-stream / other colon syntax ("file.txt:stream",
    //      "a:b:c.txt") — cut at the first ':' so nothing after it can
    //      influence the name. (Also why ":" is later replaced with '_':
    //      a colon is never valid in a portable filename.)
    let last = {
        let mut chars = last.chars();
        let drive_letter = chars.next();
        let rest = chars.as_str();
        match drive_letter {
            Some(letter)
                if letter.is_ascii_alphabetic()
                    && rest.starts_with(':')
                    && !rest[1..].contains(':') =>
            {
                &rest[1..]
            }
            _ => match last.find(':') {
                Some(pos) => &last[..pos],
                None => last,
            },
        }
    };

    // 3. Traversal/empty checks FIRST — before any trimming that could
    //    disguise ".." as something else.
    let trimmed = last.trim();
    if trimmed == ".." || trimmed == "." {
        return Err(PathSafetyError::Traversal);
    }
    if trimmed.is_empty() {
        return Err(PathSafetyError::Empty);
    }

    // 4. Replace characters that are illegal or dangerous on any platform
    //    we target (Windows set is the superset) plus control chars.
    let cleaned: String = trimmed
        .chars()
        .map(|c| {
            if c == '\0' || c < '\u{20}' || c == '\u{7f}' || "<>:\"|?*".contains(c) {
                '_'
            } else {
                c
            }
        })
        .collect();

    // 5. Trim trailing dots/spaces (Windows drops them silently, which
    //    would let "evil." collide with "evil"). This can only *shrink*
    //    the name; ".."/"." were already rejected above.
    let cleaned = cleaned.trim_end_matches(['.', ' ']).trim();

    if cleaned.is_empty() || cleaned == "." || cleaned == ".." {
        return Err(PathSafetyError::Empty);
    }

    // 6. Windows reserved device names (CON, PRN, AUX, NUL, COM1-9, LPT1-9),
    //    with or without an extension.
    let stem = cleaned
        .split('.')
        .next()
        .unwrap_or(cleaned)
        .to_ascii_uppercase();
    let reserved = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || (stem.len() == 4
            && (stem.starts_with("COM") || stem.starts_with("LPT"))
            && matches!(stem.as_bytes()[3], b'1'..=b'9'));
    let cleaned = if reserved {
        format!("_{cleaned}")
    } else {
        cleaned.to_string()
    };

    // 7. Cap length, truncating only at UTF-8 char boundaries.
    let mut out = cleaned;
    if out.len() > MAX_NAME_BYTES {
        let mut end = MAX_NAME_BYTES;
        while end > 0 && !out.is_char_boundary(end) {
            end -= 1;
        }
        out.truncate(end);
        // Truncation may re-introduce trailing dots/spaces.
        let out_trimmed = out.trim_end_matches(['.', ' ']).to_string();
        out = if out_trimmed.is_empty() {
            "_".into()
        } else {
            out_trimmed
        };
    }

    if out.is_empty() {
        return Err(PathSafetyError::Unusable);
    }
    Ok(out)
}

/// Outcome of a collision check (spec FR-XFER-011).
#[derive(Debug, PartialEq, Eq)]
pub enum ReserveResult {
    /// Use this filename (possibly renamed).
    Use(String),
    /// Policy says skip: caller should drop the transfer of this item.
    Skip,
}

/// Find a usable final name in `dest_dir` under `policy`.
///
/// Pure existence checks — the actual move happens in
/// [`crate::file_sink::IncomingFile::finalize`], which re-checks
/// immediately before renaming to keep the overwrite window as small as
/// a single syscall pair (documented MVP limitation: no portable atomic
/// "rename if absent" exists in `std`; see docs/security.md).
pub fn reserve_final_name(
    dest_dir: &Path,
    safe_name: &str,
    policy: CollisionPolicy,
) -> std::io::Result<ReserveResult> {
    let candidate = dest_dir.join(safe_name);
    if !candidate.exists() {
        return Ok(ReserveResult::Use(safe_name.to_string()));
    }
    match policy {
        CollisionPolicy::Overwrite => Ok(ReserveResult::Use(safe_name.to_string())),
        CollisionPolicy::Skip => Ok(ReserveResult::Skip),
        CollisionPolicy::Rename => {
            // "report.mp4" -> "report (1).mp4" -> "report (2).mp4" ...
            let (stem, ext) = match safe_name.rfind('.') {
                Some(pos) if pos > 0 => (&safe_name[..pos], &safe_name[pos..]),
                _ => (safe_name, ""),
            };
            for i in 1..10_000 {
                let candidate_name = format!("{stem} ({i}){ext}");
                if !dest_dir.join(&candidate_name).exists() {
                    return Ok(ReserveResult::Use(candidate_name));
                }
            }
            // Pathologically full directory: let the caller fail with a
            // clear I/O error rather than looping forever.
            Err(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                "could not find an unused filename after 10000 attempts",
            ))
        }
    }
}

/// Validate an entire *relative* path (future folder transfers, P1).
/// Present now so the rules are tested before folder support lands
/// (spec §10.4: reject absolute paths, `..`, drive prefixes).
pub fn sanitize_relative_path(raw: &str) -> Result<String, PathSafetyError> {
    if raw.starts_with('/') || raw.starts_with('\\') {
        return Err(PathSafetyError::Traversal);
    }
    if raw.contains(':') {
        return Err(PathSafetyError::Traversal);
    }
    let mut safe_parts = Vec::new();
    for part in raw.split(['/', '\\']) {
        if part.is_empty() || part == "." {
            continue;
        }
        if part == ".." {
            return Err(PathSafetyError::Traversal);
        }
        safe_parts.push(sanitize_file_name(part)?);
    }
    if safe_parts.is_empty() {
        return Err(PathSafetyError::Empty);
    }
    Ok(safe_parts.join("/"))
}

/// Generate the random temp-file name used *during* transfer.
///
/// Lives in the destination directory so the final rename stays on the
/// same filesystem (a cross-device rename would not be atomic).
pub fn temp_file_path(dest_dir: &Path) -> PathBuf {
    use rand::RngCore;
    let mut bytes = [0u8; 16];
    rand::rng().fill_bytes(&mut bytes);
    dest_dir.join(format!(
        ".swiftdrop-{}-{}.part",
        std::process::id(),
        hex::encode(bytes)
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(raw: &str) -> String {
        sanitize_file_name(raw).expect("should sanitize")
    }

    #[test]
    fn strips_path_components() {
        assert_eq!(ok("../../etc/passwd"), "passwd");
        assert_eq!(ok("a/b/c.txt"), "c.txt");
        assert_eq!(ok(r"C:\Users\me\report.pdf"), "report.pdf");
        assert_eq!(ok("/absolute/name.bin"), "name.bin");
        assert_eq!(ok(r"..\..\evil.dll"), "evil.dll");
    }

    /// Path traversal must be impossible (spec FR-XFER-010, §10.4).
    #[test]
    fn never_yields_traversal() {
        for hostile in [
            "..", "../..", "../../", "..\\..\\", ".", "...", "....", ".. ", " .. ",
        ] {
            if let Ok(name) = sanitize_file_name(hostile) {
                assert_ne!(name, "..");
                assert!(
                    !name.contains(".."),
                    "still traversal-ish: {hostile:?} -> {name:?}"
                );
            }
        }
    }

    #[test]
    fn rejects_pure_traversal() {
        assert_eq!(sanitize_file_name(".."), Err(PathSafetyError::Traversal));
        assert_eq!(sanitize_file_name("."), Err(PathSafetyError::Traversal));
        assert_eq!(sanitize_file_name("../.."), Err(PathSafetyError::Traversal));
        assert_eq!(sanitize_file_name("/.."), Err(PathSafetyError::Traversal));
        assert_eq!(sanitize_file_name(""), Err(PathSafetyError::Empty));
        assert_eq!(sanitize_file_name("////"), Err(PathSafetyError::Empty));
    }

    #[test]
    fn strips_windows_dangerous_bits() {
        assert_eq!(ok("file<name>.txt"), "file_name_.txt");
        assert_eq!(ok("report|name.doc"), "report_name.doc");
        assert_eq!(ok("what?.txt"), "what_.txt");
        // Colon handling cuts everything from the first ':' onwards.
        assert_eq!(ok("a:b:c.txt"), "a");
    }

    #[test]
    fn windows_drive_and_ads_prefix_removed() {
        // Drive-prefix form is cut at ':' before replacement.
        assert_eq!(ok("C:evil.txt"), "evil.txt");
        // ADS form on an actual filename is cut too.
        assert_eq!(ok("report.txt:secret"), "report.txt");
    }

    #[test]
    fn control_and_nul_chars_replaced() {
        assert_eq!(ok("bad\u{0000}name.txt"), "bad_name.txt");
        assert_eq!(ok("line\nbreak.txt"), "line_break.txt");
        assert_eq!(ok("bell\u{7}ring.txt"), "bell_ring.txt");
    }

    #[test]
    fn trailing_dots_and_spaces_trimmed() {
        assert_eq!(ok("evil."), "evil");
        assert_eq!(ok("evil   "), "evil");
        assert_eq!(ok("name. "), "name");
    }

    #[test]
    fn reserved_device_names_prefixed() {
        assert_eq!(ok("CON"), "_CON");
        assert_eq!(ok("con.txt"), "_con.txt");
        assert_eq!(ok("NUL"), "_NUL");
        assert_eq!(ok("COM1.log"), "_COM1.log");
        assert_eq!(ok("COM10.log"), "COM10.log", "COM10 is a normal name");
        // "console" is fine.
        assert_eq!(ok("console.txt"), "console.txt");
    }

    #[test]
    fn unicode_names_survive() {
        assert_eq!(ok("Бюджет ☃ 2026.xlsx"), "Бюджет ☃ 2026.xlsx");
        assert_eq!(ok("日本語のファイル.pdf"), "日本語のファイル.pdf");
        assert_eq!(ok("emoji 🎉 party.txt"), "emoji 🎉 party.txt");
    }

    /// Long names are capped without splitting a UTF-8 character.
    #[test]
    fn long_names_truncated_on_char_boundary() {
        let long = format!("{}.txt", "☃".repeat(200)); // way over 200 bytes
        let out = sanitize_file_name(&long).unwrap();
        assert!(out.len() <= MAX_NAME_BYTES);
        assert!(std::str::from_utf8(out.as_bytes()).is_ok());
    }

    #[test]
    fn empty_after_sanitization_rejected() {
        assert!(sanitize_file_name("/").is_err());
        assert!(sanitize_file_name("::").is_err());
        assert!(sanitize_file_name("   ").is_err());
        // Control chars are *replaced*, so this yields a usable name.
        assert_eq!(sanitize_file_name("\u{0}\u{0}").unwrap(), "__");
    }

    #[test]
    fn collision_rename_policy() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("report.mp4"), b"existing").unwrap();

        match reserve_final_name(dir.path(), "report.mp4", CollisionPolicy::Rename).unwrap() {
            ReserveResult::Use(name) => assert_eq!(name, "report (1).mp4"),
            other => panic!("expected rename, got {other:?}"),
        }

        std::fs::write(dir.path().join("report (1).mp4"), b"existing too").unwrap();
        match reserve_final_name(dir.path(), "report.mp4", CollisionPolicy::Rename).unwrap() {
            ReserveResult::Use(name) => assert_eq!(name, "report (2).mp4"),
            other => panic!("expected (2), got {other:?}"),
        }

        // Fresh names are used as-is.
        match reserve_final_name(dir.path(), "fresh.txt", CollisionPolicy::Rename).unwrap() {
            ReserveResult::Use(name) => assert_eq!(name, "fresh.txt"),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn collision_skip_and_overwrite_policies() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), b"x").unwrap();
        assert_eq!(
            reserve_final_name(dir.path(), "a.txt", CollisionPolicy::Skip).unwrap(),
            ReserveResult::Skip
        );
        assert_eq!(
            reserve_final_name(dir.path(), "a.txt", CollisionPolicy::Overwrite).unwrap(),
            ReserveResult::Use("a.txt".into())
        );
        assert_eq!(
            reserve_final_name(dir.path(), "a.txt", CollisionPolicy::Rename).unwrap(),
            ReserveResult::Use("a (1).txt".into())
        );
    }

    #[test]
    fn relative_path_validation() {
        assert_eq!(
            sanitize_relative_path("folder/sub/file.txt").unwrap(),
            "folder/sub/file.txt"
        );
        assert!(sanitize_relative_path("/abs/path").is_err());
        assert!(sanitize_relative_path("../up").is_err());
        assert!(sanitize_relative_path("a/../../b").is_err());
        assert!(sanitize_relative_path("C:/x").is_err());
        assert!(sanitize_relative_path("").is_err());
    }

    #[test]
    fn temp_paths_stay_in_dest_dir() {
        let dir = tempfile::tempdir().unwrap();
        let t1 = temp_file_path(dir.path());
        let t2 = temp_file_path(dir.path());
        assert_eq!(t1.parent().unwrap(), dir.path());
        assert_ne!(t1, t2, "temp names must be random");
        assert!(t1.file_name().unwrap().to_str().unwrap().ends_with(".part"));
    }
}
