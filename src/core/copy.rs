//! Link-safe tree scanning and copying shared by the storage migration and
//! instance duplication.
//!
//! * [`scan_tree`] lists a tree without following links. Genuine links (symlinks;
//!   on Windows junctions and other name-surrogate reparse points) are recorded in
//!   [`TreeScan::links`] and never traversed or copied. Entries that are not links but
//!   can't be copied (special files, unreadable folders) go in [`TreeScan::blocked`].
//! * [`copy_tree`] copies a scanned tree into a destination Ferrite owns. Every file
//!   is written to a `.ferrite-partial` name, given its original timestamp, fsynced,
//!   then renamed, so a file with its final name is always complete. Files are always
//!   copied, never hard-linked. Source files that can't be read are collected and
//!   reported together as [`CopyFailure::Uncopyable`].
//! * [`describe_change`] compares two scans so callers can refuse to commit a copy of
//!   a tree that changed while it was being copied.

use crate::core::fsutil;
use std::collections::BTreeMap;
use std::fmt;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// Suffix for in-flight copied files.
pub(crate) const PARTIAL_SUFFIX: &str = ".ferrite-partial";
const COPY_CHUNK: usize = 1024 * 1024;

/// Failure of a scan or copy. Callers map these onto their own error types.
#[derive(Debug)]
pub(crate) enum CopyFailure {
    Io {
        context: String,
        error: io::Error,
    },
    /// Source entries that could not be copied; nothing should be committed.
    Uncopyable(Vec<UncopyableFile>),
    /// [`CopyObserver::cancelled`] returned `true`.
    Cancelled,
    /// [`CopyObserver::file_done`] returned an error (used by test hooks).
    Interrupted(io::Error),
}

pub(crate) fn io_ctx(context: impl Into<String>) -> impl FnOnce(io::Error) -> CopyFailure {
    let context = context.into();
    move |error| CopyFailure::Io { context, error }
}

/// One regular file found by a scan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ScannedFile {
    pub(crate) len: u64,
    pub(crate) modified: Option<SystemTime>,
}

/// An entry that cannot be copied and is not a link.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UncopyableFile {
    /// Path relative to the copied folder.
    pub path: PathBuf,
    /// Plain-language reason (includes the OS error where there is one).
    pub reason: String,
}

impl fmt::Display for UncopyableFile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} ({})", self.path.display(), self.reason)
    }
}

/// Recursive listing of a tree, relative to its root.
#[derive(Debug, Clone, Default)]
pub(crate) struct TreeScan {
    pub(crate) dirs: Vec<PathBuf>,
    pub(crate) files: BTreeMap<PathBuf, ScannedFile>,
    /// Genuine links that are deliberately not traversed or copied.
    pub(crate) links: Vec<PathBuf>,
    /// Entries that are not links but cannot be copied; these block a commit.
    pub(crate) blocked: Vec<UncopyableFile>,
    pub(crate) total_bytes: u64,
}

impl TreeScan {
    pub(crate) fn last_modified(&self) -> Option<SystemTime> {
        self.files.values().filter_map(|file| file.modified).max()
    }

    /// Same relative paths and sizes.
    pub(crate) fn same_content_shape(&self, other: &Self) -> bool {
        self.files.len() == other.files.len()
            && self
                .files
                .iter()
                .zip(other.files.iter())
                .all(|((a, fa), (b, fb))| a == b && fa.len == fb.len)
    }
}

/// Progress, cancellation and test hooks for [`copy_tree`].
pub(crate) trait CopyObserver {
    /// Checked before every file and every chunk.
    fn cancelled(&self) -> bool;
    /// Bytes copied (or found already copied) since the last call.
    fn add_bytes(&self, bytes: u64);
    /// Called after each file with the number of files handled so far. An error
    /// aborts the copy with [`CopyFailure::Interrupted`].
    fn file_done(&self, files_done: u64) -> io::Result<()>;
    /// Called with each source file right before it is opened; an error marks the
    /// file as unreadable (test hook).
    fn before_open(&self, _source: &Path) -> io::Result<()> {
        Ok(())
    }
    /// Every file is copied; directory syncing follows.
    fn all_copied(&self) {}
}

fn check_cancel(observer: &dyn CopyObserver) -> Result<(), CopyFailure> {
    if observer.cancelled() {
        Err(CopyFailure::Cancelled)
    } else {
        Ok(())
    }
}

fn exists_no_follow(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok()
}

/// Lists `root` without following links. Only a failure to read `root` itself is an
/// error; problems further down are recorded in [`TreeScan::blocked`].
pub(crate) fn scan_tree(root: &Path) -> Result<TreeScan, CopyFailure> {
    let mut scan = TreeScan::default();
    fs::read_dir(root).map_err(io_ctx(format!("read {}", root.display())))?;
    let mut pending = vec![PathBuf::new()];
    while let Some(relative) = pending.pop() {
        let absolute = root.join(&relative);
        let entries = match fs::read_dir(&absolute) {
            Ok(entries) => entries,
            Err(error) => {
                scan.blocked.push(UncopyableFile {
                    path: relative,
                    reason: format!("folder could not be read: {error}"),
                });
                continue;
            }
        };
        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    scan.blocked.push(UncopyableFile {
                        path: relative.clone(),
                        reason: format!("folder could not be listed: {error}"),
                    });
                    break;
                }
            };
            let rel = relative.join(entry.file_name());
            let path = entry.path();
            let metadata = match fs::symlink_metadata(&path) {
                Ok(metadata) => metadata,
                Err(error) => {
                    scan.blocked.push(UncopyableFile {
                        path: rel,
                        reason: format!("could not be inspected: {error}"),
                    });
                    continue;
                }
            };
            if fsutil::is_link_like(&metadata) {
                eprintln!("Ferrite: not following link {}", path.display());
                scan.links.push(rel);
            } else if metadata.is_dir() {
                scan.dirs.push(rel.clone());
                pending.push(rel);
            } else if metadata.is_file() {
                scan.total_bytes += metadata.len();
                scan.files.insert(
                    rel,
                    ScannedFile {
                        len: metadata.len(),
                        modified: metadata.modified().ok(),
                    },
                );
            } else {
                eprintln!("Ferrite: cannot copy special file {}", path.display());
                scan.blocked.push(UncopyableFile {
                    path: rel,
                    reason: "not a regular file, folder, or link".into(),
                });
            }
        }
    }
    scan.dirs.sort();
    scan.links.sort();
    scan.blocked.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(scan)
}

/// Safety margin on top of the bytes to copy: 5%.
pub(crate) fn with_margin(bytes: u64) -> u64 {
    bytes.saturating_add(bytes / 20)
}

/// Compares two scans of a source (file set, sizes, mtimes, folders, links) and
/// describes the first difference.
pub(crate) fn describe_change(before: &TreeScan, after: &TreeScan) -> Option<String> {
    if let Some(blocked) = after.blocked.first() {
        return Some(format!("{blocked} can no longer be read"));
    }
    for (path, info) in &after.files {
        match before.files.get(path) {
            None => return Some(format!("{} was added", path.display())),
            Some(old) if old != info => {
                return Some(format!("{} was modified", path.display()));
            }
            Some(_) => {}
        }
    }
    if let Some(path) = before
        .files
        .keys()
        .find(|path| !after.files.contains_key(*path))
    {
        return Some(format!("{} was removed", path.display()));
    }
    if before.dirs != after.dirs {
        return Some("folders were added or removed".into());
    }
    if before.links != after.links {
        return Some("links were added or removed".into());
    }
    None
}

/// Removes entries in `staged` that are no longer in the source scan (left over from
/// an earlier attempt against a source that has since changed). Only ever touches the
/// staging directory, which belongs to Ferrite.
fn prune_staging(staged: &Path, scan: &TreeScan) -> Result<(), CopyFailure> {
    if !exists_no_follow(staged) {
        return Ok(());
    }
    let copy = scan_tree(staged)?;
    for (path, _) in copy
        .files
        .iter()
        .filter(|(path, _)| !scan.files.contains_key(*path))
    {
        let target = staged.join(path);
        fs::remove_file(&target).map_err(io_ctx(format!("remove stale {}", target.display())))?;
    }
    for link in &copy.links {
        let target = staged.join(link);
        fs::remove_file(&target).map_err(io_ctx(format!("remove stale {}", target.display())))?;
    }
    // Deepest first, so a removed parent never hides a child we still look at.
    for dir in copy.dirs.iter().rev() {
        if !scan.dirs.contains(dir) {
            let target = staged.join(dir);
            match fs::remove_dir_all(&target) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(io_ctx(format!("remove stale {}", target.display()))(error));
                }
            }
        }
    }
    Ok(())
}

pub(crate) fn partial_name(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(PARTIAL_SUFFIX);
    PathBuf::from(name)
}

/// Removes leftover `.ferrite-partial` files inside staging (only ever ours).
fn clear_partials(staged: &Path) -> Result<(), CopyFailure> {
    if !exists_no_follow(staged) {
        return Ok(());
    }
    let mut pending = vec![staged.to_path_buf()];
    while let Some(dir) = pending.pop() {
        for entry in fs::read_dir(&dir).map_err(io_ctx(format!("read {}", dir.display())))? {
            let entry = entry.map_err(io_ctx(format!("read {}", dir.display())))?;
            let path = entry.path();
            let metadata = fs::symlink_metadata(&path)
                .map_err(io_ctx(format!("inspect {}", path.display())))?;
            if metadata.is_dir() && !fsutil::is_link_like(&metadata) {
                pending.push(path);
            } else if entry
                .file_name()
                .to_string_lossy()
                .ends_with(PARTIAL_SUFFIX)
            {
                fs::remove_file(&path).map_err(io_ctx(format!("remove {}", path.display())))?;
            }
        }
    }
    Ok(())
}

/// Copies every file in `scan` from `source` into `staged`, creating `staged` and its
/// folders. Files already in `staged` with the scanned size and timestamp are kept
/// (resume). Leftover partial files and entries no longer in the scan are removed
/// from `staged` first.
pub(crate) fn copy_tree(
    source: &Path,
    staged: &Path,
    scan: &TreeScan,
    observer: &dyn CopyObserver,
) -> Result<(), CopyFailure> {
    clear_partials(staged)?;
    prune_staging(staged, scan)?;
    fs::create_dir_all(staged).map_err(io_ctx(format!("create {}", staged.display())))?;
    for dir in &scan.dirs {
        let target = staged.join(dir);
        fs::create_dir_all(&target).map_err(io_ctx(format!("create {}", target.display())))?;
    }
    let mut buffer = vec![0_u8; COPY_CHUNK];
    let mut files_done = 0_u64;
    let mut uncopyable = Vec::new();
    for (relative, info) in &scan.files {
        check_cancel(observer)?;
        let from = source.join(relative);
        let to = staged.join(relative);
        // Resume: a final-named staged file is complete by construction. Size and
        // modification time (preserved on copy) must still match the source scan, so a
        // source file that changed between attempts is copied again.
        let already = fs::symlink_metadata(&to).is_ok_and(|metadata| {
            metadata.is_file()
                && metadata.len() == info.len
                && info.modified.is_some()
                && metadata.modified().ok() == info.modified
        });
        if already {
            observer.add_bytes(info.len);
        } else {
            match copy_one(&from, &to, info, observer, &mut buffer) {
                Ok(()) => {}
                // A source-side problem: record it, keep copying the rest so the user
                // sees the complete list, and refuse to commit afterwards.
                Err(FileError::Source(reason)) => {
                    eprintln!("Ferrite: cannot copy {}: {reason}", from.display());
                    uncopyable.push(UncopyableFile {
                        path: relative.clone(),
                        reason,
                    });
                }
                Err(FileError::Fatal(error)) => return Err(error),
            }
        }
        files_done += 1;
        observer
            .file_done(files_done)
            .map_err(CopyFailure::Interrupted)?;
    }
    if !uncopyable.is_empty() {
        return Err(CopyFailure::Uncopyable(uncopyable));
    }
    observer.all_copied();
    // Persist the partial->final renames: sync every staged directory.
    for dir in scan.dirs.iter().rev() {
        let target = staged.join(dir);
        fsutil::sync_dir(&target).map_err(io_ctx(format!("sync {}", target.display())))?;
    }
    fsutil::sync_dir(staged).map_err(io_ctx(format!("sync {}", staged.display())))?;
    Ok(())
}

/// Why one file could not be copied.
enum FileError {
    /// The source could not be read (recorded; blocks the commit).
    Source(String),
    /// Destination-side failure or cancellation (aborts immediately).
    Fatal(CopyFailure),
}

impl From<CopyFailure> for FileError {
    fn from(error: CopyFailure) -> Self {
        Self::Fatal(error)
    }
}

fn copy_one(
    from: &Path,
    to: &Path,
    info: &ScannedFile,
    observer: &dyn CopyObserver,
    buffer: &mut [u8],
) -> Result<(), FileError> {
    // Re-check right before opening so a file swapped for a link after the scan is
    // not followed (a narrow race remains between this check and `open`).
    let metadata = fs::symlink_metadata(from)
        .map_err(|error| FileError::Source(format!("could not be inspected: {error}")))?;
    if !metadata.is_file() || fsutil::is_link_like(&metadata) {
        return Err(FileError::Source(
            "changed from a regular file while Ferrite was copying".into(),
        ));
    }
    observer
        .before_open(from)
        .map_err(|error| FileError::Source(format!("could not be opened: {error}")))?;
    let mut input = fsutil::open_regular_no_follow(from, &metadata)
        .map_err(|error| FileError::Source(format!("could not be opened: {error}")))?;
    let partial = partial_name(to);
    let mut output =
        File::create(&partial).map_err(io_ctx(format!("create {}", partial.display())))?;
    let copied = (|| -> Result<(), FileError> {
        loop {
            check_cancel(observer)?;
            let read = input
                .read(buffer)
                .map_err(|error| FileError::Source(format!("could not be read: {error}")))?;
            if read == 0 {
                break;
            }
            output
                .write_all(&buffer[..read])
                .map_err(io_ctx(format!("write {}", partial.display())))?;
            observer.add_bytes(read as u64);
        }
        Ok(())
    })();
    if let Err(error) = copied {
        drop(output);
        let _ = fs::remove_file(&partial);
        return Err(error);
    }
    // Set the timestamp before the flush so it is part of what gets synced.
    if let Some(modified) = info.modified {
        let _ = output.set_modified(modified);
    }
    // Plain per-file fsync; callers do one full flush before committing.
    fsutil::sync_file_data(&output).map_err(io_ctx(format!("sync {}", partial.display())))?;
    drop(output);
    fs::rename(&partial, to).map_err(io_ctx(format!("finish {}", to.display())))?;
    Ok(())
}
