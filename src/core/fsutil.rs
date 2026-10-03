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

/// Flushes directory metadata (new entries/renames) with a plain `fsync` on Unix.
/// Windows has no portable directory fsync; NTFS journals renames itself.
pub fn sync_dir(dir: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        sync_file_data(&File::open(dir)?)
    }
    #[cfg(not(unix))]
    {
        let _ = dir;
        Ok(())
    }
}

/// Per-file durability for bulk copies: a plain `fsync`.
///
/// On macOS Rust's `sync_all`/`sync_data` issue `F_FULLFSYNC` (a full drive-cache
/// flush) every time, which is far too slow per file; `libc::fsync` is used there
/// and a single [`full_flush`] is done once before committing. Elsewhere
/// `sync_data` is a plain `fdatasync`/`FlushFileBuffers`.
pub fn sync_file_data(file: &File) -> io::Result<()> {
    #[cfg(target_vendor = "apple")]
    {
        use std::os::fd::AsRawFd;
        // SAFETY: the descriptor is owned by `file` and valid for this call.
        if unsafe { libc::fsync(file.as_raw_fd()) } == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }
    #[cfg(not(target_vendor = "apple"))]
    {
        file.sync_data()
    }
}

/// One full flush of everything written under `dir`'s filesystem, done once before
/// a commit point:
/// - macOS: `F_FULLFSYNC` on `dir` (flushes the drive's write cache);
/// - Linux/Android: `syncfs` on `dir` (flushes the whole filesystem);
/// - other Unix: `fsync` on `dir`;
/// - Windows: best effort `FlushFileBuffers` on a directory handle (files were already
///   flushed individually); failure to open the handle is ignored.
pub fn full_flush(dir: &Path) -> io::Result<()> {
    #[cfg(target_vendor = "apple")]
    {
        use std::os::fd::AsRawFd;
        let handle = File::open(dir)?;
        // SAFETY: valid descriptor owned by `handle`; F_FULLFSYNC takes no argument.
        if unsafe { libc::fcntl(handle.as_raw_fd(), libc::F_FULLFSYNC) } == -1 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        use std::os::fd::AsRawFd;
        let handle = File::open(dir)?;
        // SAFETY: valid descriptor owned by `handle`.
        if unsafe { libc::syncfs(handle.as_raw_fd()) } == -1 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
    #[cfg(all(
        unix,
        not(target_vendor = "apple"),
        not(any(target_os = "linux", target_os = "android"))
    ))]
    {
        File::open(dir)?.sync_all()
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
        if let Ok(handle) = OpenOptions::new()
            .write(true)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
            .open(dir)
        {
            let _ = handle.sync_all();
        }
        Ok(())
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = dir;
        Ok(())
    }
}

/// Opens a source file for copying without following a final-component link, and
/// confirms the opened handle is the regular file that was inspected.
///
/// On Unix this uses `O_NOFOLLOW | O_NONBLOCK` (so a FIFO swapped in cannot block)
/// and compares the handle's device/inode with `expected`. On Windows the file is
/// opened normally: `FILE_FLAG_OPEN_REPARSE_POINT` would bypass the cloud-files and
/// dedup filters and read placeholder stubs instead of contents, and the standard
/// library does not expose a stable file ID to compare; the pre-open link check
/// remains the guard there.
pub fn open_regular_no_follow(path: &Path, expected: &fs::Metadata) -> io::Result<File> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(path)?;
        let opened = file.metadata()?;
        if !opened.is_file() || opened.dev() != expected.dev() || opened.ino() != expected.ino() {
            return Err(io::Error::other(
                "the file was replaced while Ferrite was copying it",
            ));
        }
        Ok(file)
    }
    #[cfg(not(unix))]
    {
        let file = File::open(path)?;
        if !file.metadata()?.is_file() || !expected.is_file() {
            return Err(io::Error::other("not a regular file"));
        }
        Ok(file)
    }
}

/// Bytes available to this user on the filesystem containing `path` (which must
/// exist): `statvfs` (`f_bavail * f_frsize`) on Unix, `GetDiskFreeSpaceExW` on Windows.
pub fn available_space(path: &Path) -> io::Result<u64> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        let c_path = std::ffi::CString::new(path.as_os_str().as_bytes())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains NUL"))?;
        let mut stats = std::mem::MaybeUninit::<libc::statvfs>::uninit();
        // SAFETY: valid NUL-terminated path and a writable statvfs buffer.
        if unsafe { libc::statvfs(c_path.as_ptr(), stats.as_mut_ptr()) } != 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: statvfs returned success, so the buffer is initialized.
        let stats = unsafe { stats.assume_init() };
        #[allow(clippy::unnecessary_cast)] // field widths differ between platforms
        Ok((stats.f_bavail as u64).saturating_mul(stats.f_frsize as u64))
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        let wide: Vec<u16> = path.as_os_str().encode_wide().chain([0]).collect();
        let mut available = 0_u64;
        // SAFETY: NUL-terminated UTF-16 path; the optional out-pointers may be null.
        let ok = unsafe {
            windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW(
                wide.as_ptr(),
                &mut available,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(available)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = path;
        Ok(u64::MAX)
    }
}

/// Renames `from` to `to`, failing with [`io::ErrorKind::AlreadyExists`] instead of
/// replacing anything at `to`, including an empty folder (which plain POSIX `rename`
/// would silently replace).
///
/// Linux uses `renameat2(RENAME_NOREPLACE)` (raw syscall, so old glibc and musl
/// work), macOS `renamex_np(RENAME_EXCL)`, Windows `MoveFileExW` without
/// `MOVEFILE_REPLACE_EXISTING`. Where the kernel or file system doesn't support the
/// exclusive form (`EINVAL`/`ENOSYS`/`ENOTSUP`, e.g. some network or FUSE mounts),
/// it falls back to checking that `to` doesn't exist right before a plain rename;
/// that leaves a narrow race, which callers accept for folders they just allocated.
pub fn rename_no_replace(from: &Path, to: &Path) -> io::Result<()> {
    match rename_exclusive(from, to) {
        Err(error) if exclusive_rename_unsupported(&error) => {
            if fs::symlink_metadata(to).is_ok() {
                return Err(already_exists(to));
            }
            fs::rename(from, to)
        }
        other => other,
    }
}

fn already_exists(to: &Path) -> io::Error {
    io::Error::new(
        io::ErrorKind::AlreadyExists,
        format!("{} already exists", to.display()),
    )
}

#[cfg(unix)]
fn exclusive_rename_unsupported(error: &io::Error) -> bool {
    matches!(
        error.raw_os_error(),
        Some(libc::EINVAL | libc::ENOSYS | libc::ENOTSUP)
    )
}

#[cfg(not(unix))]
fn exclusive_rename_unsupported(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::Unsupported
}

#[cfg(unix)]
fn c_path(path: &Path) -> io::Result<std::ffi::CString> {
    use std::os::unix::ffi::OsStrExt;
    std::ffi::CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains NUL"))
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn rename_exclusive(from: &Path, to: &Path) -> io::Result<()> {
    /// `RENAME_NOREPLACE` from `<linux/fs.h>`.
    const RENAME_NOREPLACE: libc::c_uint = 1;
    let (from_c, to_c) = (c_path(from)?, c_path(to)?);
    // SAFETY: valid NUL-terminated paths; AT_FDCWD is ignored for absolute paths.
    let result = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            libc::AT_FDCWD,
            from_c.as_ptr(),
            libc::AT_FDCWD,
            to_c.as_ptr(),
            RENAME_NOREPLACE,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(target_vendor = "apple")]
fn rename_exclusive(from: &Path, to: &Path) -> io::Result<()> {
    let (from_c, to_c) = (c_path(from)?, c_path(to)?);
    // SAFETY: valid NUL-terminated paths.
    if unsafe { libc::renamex_np(from_c.as_ptr(), to_c.as_ptr(), libc::RENAME_EXCL) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(windows)]
fn rename_exclusive(from: &Path, to: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    let wide = |path: &Path| -> Vec<u16> { path.as_os_str().encode_wide().chain([0]).collect() };
    let (from_w, to_w) = (wide(from), wide(to));
    // SAFETY: NUL-terminated UTF-16 paths. Flags 0: no replace, same volume only.
    let ok = unsafe {
        windows_sys::Win32::Storage::FileSystem::MoveFileExW(from_w.as_ptr(), to_w.as_ptr(), 0)
    };
    if ok != 0 {
        return Ok(());
    }
    let error = io::Error::last_os_error();
    // ERROR_FILE_EXISTS (80) / ERROR_ALREADY_EXISTS (183).
    if matches!(error.raw_os_error(), Some(80 | 183)) {
        return Err(already_exists(to));
    }
    Err(error)
}

#[cfg(not(any(
    target_os = "linux",
    target_os = "android",
    target_vendor = "apple",
    windows
)))]
fn rename_exclusive(_from: &Path, _to: &Path) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "no exclusive rename on this platform",
    ))
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
    fn rename_no_replace_moves_and_refuses_existing_targets() {
        let dir = tempfile::tempdir().unwrap();
        let from = dir.path().join("staging");
        fs::create_dir(&from).unwrap();
        fs::write(from.join("file"), b"data").unwrap();

        // An existing empty folder is not replaced (plain rename would replace it).
        let empty = dir.path().join("empty");
        fs::create_dir(&empty).unwrap();
        let error = rename_no_replace(&from, &empty).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert!(from.join("file").exists());
        assert_eq!(fs::read_dir(&empty).unwrap().count(), 0);

        // Nor is an existing file.
        let file = dir.path().join("file");
        fs::write(&file, b"keep").unwrap();
        let error = rename_no_replace(&from, &file).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read(&file).unwrap(), b"keep");

        let to = dir.path().join("final");
        rename_no_replace(&from, &to).unwrap();
        assert!(!from.exists());
        assert_eq!(fs::read(to.join("file")).unwrap(), b"data");
    }

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
    fn flush_helpers_work_on_real_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("data.bin");
        fs::write(&path, b"data").unwrap();
        sync_file_data(&File::options().write(true).open(&path).unwrap()).unwrap();
        sync_dir(dir.path()).unwrap();
        full_flush(dir.path()).unwrap();
        let metadata = fs::symlink_metadata(&path).unwrap();
        open_regular_no_follow(&path, &metadata).unwrap();
        assert!(available_space(dir.path()).unwrap() > 0);
    }

    #[cfg(unix)]
    #[test]
    fn open_regular_no_follow_rejects_links_and_swaps() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real");
        let other = dir.path().join("other");
        fs::write(&real, b"real").unwrap();
        fs::write(&other, b"other").unwrap();
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let real_meta = fs::symlink_metadata(&real).unwrap();
        // A link at the final component is never followed.
        assert!(open_regular_no_follow(&link, &real_meta).is_err());
        // A different file than the one inspected is refused.
        assert!(open_regular_no_follow(&other, &real_meta).is_err());
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
