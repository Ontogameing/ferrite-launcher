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

/// Whether `metadata` (from `symlink_metadata`) describes a link-like entry that
/// must never be traversed: a symlink on every platform, plus any reparse point
/// (junctions, mount points, OneDrive placeholders, ...) on Windows.
pub fn is_link_like(metadata: &fs::Metadata) -> bool {
    if metadata.file_type().is_symlink() {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
        if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return true;
        }
    }
    false
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
}
