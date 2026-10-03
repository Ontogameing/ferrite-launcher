//! Small filesystem helpers shared by the core storage modules.
//!
//! Everything here operates on caller-supplied absolute paths; nothing consults the
//! process working directory.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// Monotonic per-process counter so concurrent writers never share a temp name.
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Returns a unique sibling temporary path for `target` (same directory, so the
/// final rename never crosses a filesystem boundary).
pub(crate) fn sibling_temp_path(target: &Path) -> io::Result<PathBuf> {
    let parent = target.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{} has no parent directory", target.display()),
        )
    })?;
    let name = target
        .file_name()
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{} has no file name", target.display()),
            )
        })?
        .to_string_lossy();
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    let counter = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    Ok(parent.join(format!(
        ".{name}.tmp-{}-{nanos}-{counter}",
        std::process::id()
    )))
}

/// Atomically replaces `target` with `contents`.
///
/// The data is written to a uniquely named temporary file in the same directory,
/// flushed with `fsync`, and renamed over the target. On Unix the parent directory
/// is fsynced afterwards so the rename itself is durable. If any step fails the
/// temporary file is removed and the previous `target` (if any) is left untouched.
pub fn write_atomic(target: &Path, contents: &[u8]) -> io::Result<()> {
    let parent = target.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{} has no parent directory", target.display()),
        )
    })?;
    fs::create_dir_all(parent)?;
    let temporary = sibling_temp_path(target)?;
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        file.write_all(contents)?;
        file.sync_all()?;
        drop(file);
        // `std::fs::rename` replaces an existing file on every supported platform
        // (MoveFileExW with MOVEFILE_REPLACE_EXISTING on Windows).
        fs::rename(&temporary, target)?;
        sync_dir(parent)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

/// Flushes directory metadata (new entries/renames) to stable storage on Unix.
/// Windows has no portable directory fsync; NTFS journals renames itself.
pub fn sync_dir(dir: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        File::open(dir)?.sync_all()
    }
    #[cfg(not(unix))]
    {
        let _ = dir;
        Ok(())
    }
}

/// Windows reparse tag of a symbolic link (`IO_REPARSE_TAG_SYMLINK`).
pub const IO_REPARSE_TAG_SYMLINK: u32 = 0xA000_000C;
/// Windows reparse tag of a junction / mount point (`IO_REPARSE_TAG_MOUNT_POINT`).
pub const IO_REPARSE_TAG_MOUNT_POINT: u32 = 0xA000_0003;
/// `IsReparseTagNameSurrogate`: the tag redirects to another named entity.
const REPARSE_TAG_NAME_SURROGATE_BIT: u32 = 0x2000_0000;

/// Whether a Windows reparse tag denotes a link (a *name surrogate*: symlinks,
/// junctions/mount points, and any other tag with the `IsReparseTagNameSurrogate`
/// bit). Data-carrying reparse points such as OneDrive/cloud-files placeholders
/// (`0x9000001A` and friends) or deduplicated files (`0x80000013`) are *not*
/// links: their contents are read and copied like any regular file.
///
/// This is a pure function so the classification can be tested on every platform.
/// It is the same rule Rust's standard library applies on Windows in
/// [`fs::FileType::is_symlink`], which [`is_link_like`] relies on.
pub fn reparse_tag_is_link(tag: u32) -> bool {
    tag & REPARSE_TAG_NAME_SURROGATE_BIT != 0
}

/// Whether `metadata` (from `symlink_metadata`) describes a link that must never be
/// traversed or copied: a symlink on Unix; on Windows a symlink, junction, or other
/// name-surrogate reparse point (see [`reparse_tag_is_link`]).
///
/// Non-surrogate reparse points (OneDrive placeholders, dedup) report as regular
/// files/directories here and are copied normally. The standard library classifies
/// them with exactly the [`reparse_tag_is_link`] rule; it does not expose the raw tag.
pub fn is_link_like(metadata: &fs::Metadata) -> bool {
    metadata.file_type().is_symlink()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_atomic_replaces_and_leaves_no_temporaries() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("nested").join("file.json");
        write_atomic(&target, b"one").unwrap();
        write_atomic(&target, b"two").unwrap();
        assert_eq!(fs::read(&target).unwrap(), b"two");
        let entries: Vec<_> = fs::read_dir(target.parent().unwrap())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(entries, vec![std::ffi::OsString::from("file.json")]);
    }

    #[test]
    fn only_name_surrogate_reparse_tags_are_links() {
        // Links: never followed.
        assert!(reparse_tag_is_link(IO_REPARSE_TAG_SYMLINK));
        assert!(reparse_tag_is_link(IO_REPARSE_TAG_MOUNT_POINT));
        assert!(reparse_tag_is_link(0xA000_001D)); // IO_REPARSE_TAG_LX_SYMLINK (WSL)
        // Any tag with the name-surrogate bit counts, even ones we don't know.
        assert!(reparse_tag_is_link(0x2000_1234));
        // Data reparse points: read and copied normally.
        for tag in [
            0x9000_001A_u32, // IO_REPARSE_TAG_CLOUD (OneDrive Files On-Demand)
            0x9000_101A,     // IO_REPARSE_TAG_CLOUD_1
            0x9000_F01A,     // IO_REPARSE_TAG_CLOUD_F
            0x8000_0013,     // IO_REPARSE_TAG_DEDUP
            0x8000_0017,     // IO_REPARSE_TAG_WOF (compressed system files)
            0x8000_001B,     // IO_REPARSE_TAG_APPEXECLINK
        ] {
            assert!(!reparse_tag_is_link(tag), "{tag:#x}");
        }
    }
}
