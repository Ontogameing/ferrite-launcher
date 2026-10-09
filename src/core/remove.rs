//! Removing instances: move to the OS trash, delete permanently, or remove from the
//! list and keep the files.
//!
//! ## Guarantees
//!
//! * The only path ever acted on is `instances_dir/<dir>`, where `<dir>` is re-validated
//!   as a plain folder name right before acting. The instances root itself and anything
//!   outside it are never touched. A link at that path is removed (or trashed) as the
//!   link; its target is never followed.
//! * Trash and permanent delete are refused when any other profile *or* preserved
//!   skipped manifest entry uses the same folder (case-insensitively), when the
//!   instance's game is running (checked again at the moment of acting), and when the
//!   folder is missing. "Remove from list, keep files" only edits the manifest, so it
//!   works for shared and missing folders too (still refused while running).
//! * **Trash** moves the folder first and saves the manifest only after the folder is
//!   confirmed gone. A failed trash changes nothing. If the save fails after a
//!   successful trash, the entry stays listed and shows up as "folder missing".
//! * **Permanent** saves the manifest first and then deletes; a partial failure reports
//!   the leftover paths. No result ever falls through to permanent deletion by itself:
//!   the UI has to call [`remove_instance`] again with [`RemoveMode::Permanent`].
//!
//! ## Threading
//!
//! [`remove_instance`] runs every step on the calling thread. Moving a large instance
//! to the trash can take minutes (Linux cross-drive trash is a copy), so the UI should
//! use the split steps instead: [`prepare_removal`] and [`finish_trash`] /
//! [`commit_removal`] on the UI thread (they touch the profile list), and
//! [`trash_target`] / [`delete_target`] on a worker thread while showing
//! `Moving to <Trash>…`.

// Refusals are returned as a full `RemoveOutcome` so the UI matches one type; these are
// produced once per user action, so their size does not matter.
#![allow(clippy::result_large_err)]

use crate::core::instances::{self, InstanceDirName, InstanceError, InstanceProfile, SkippedEntry};
use crate::core::paths::AppPaths;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// Maximum number of leftover paths listed after a partial permanent delete.
const MAX_LEFTOVERS: usize = 100;

/// What to do with an instance's folder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoveMode {
    /// Move the folder to the OS trash, then remove the entry (default).
    Trash,
    /// Remove the entry, then delete the folder for good.
    Permanent,
    /// Remove the entry only; the folder stays on disk.
    KeepFiles,
}

/// Best-effort guess at why the OS trash refused a folder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrashFailureKind {
    /// Too big for the trash, or the disk holding the trash is full.
    TooLarge,
    /// The drive has no usable trash (read-only, network, some removable drives).
    NoTrashOnDrive,
    /// A file is in use (e.g. locked by another program on Windows).
    InUse,
    PermissionDenied,
    Other(String),
}

/// A failed trash call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrashError {
    pub kind: TrashFailureKind,
    /// Raw error text for a "Details" section.
    pub message: String,
}

impl fmt::Display for TrashError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

/// Moves one path to the OS trash. Abstracted so tests can simulate failures.
pub trait Trasher: Send + Sync {
    fn trash(&self, path: &Path) -> Result<(), TrashError>;
}

/// The real OS trash via the `trash` crate.
///
/// Each call runs on a fresh, dedicated thread: on Windows the crate initializes COM
/// apartment-threaded on the calling thread (and panics if that thread already chose
/// another COM mode), so a new thread is always safe. A panic in the backend becomes an
/// error. On macOS the `NSFileManager` method is used; the default Finder method runs
/// `osascript` and triggers an Automation permission prompt.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemTrash;

impl Trasher for SystemTrash {
    fn trash(&self, path: &Path) -> Result<(), TrashError> {
        let path = path.to_path_buf();
        let worker = std::thread::Builder::new()
            .name("trash".into())
            .spawn(move || {
                #[allow(unused_mut)]
                let mut context = trash::TrashContext::default();
                #[cfg(target_os = "macos")]
                {
                    use trash::macos::{DeleteMethod, TrashContextExtMacos};
                    context.set_delete_method(DeleteMethod::NsFileManager);
                }
                context.delete(&path).map_err(|error| TrashError {
                    kind: classify_trash_error(&error),
                    message: error.to_string(),
                })
            })
            .map_err(|error| TrashError {
                kind: TrashFailureKind::Other(error.to_string()),
                message: format!("could not start the trash worker: {error}"),
            })?;
        worker.join().unwrap_or_else(|_| {
            Err(TrashError {
                kind: TrashFailureKind::Other("the trash backend crashed".into()),
                message: "the trash backend crashed".into(),
            })
        })
    }
}

/// Maps an I/O error kind to a trash failure kind.
fn classify_io_kind(kind: io::ErrorKind, message: &str) -> TrashFailureKind {
    match kind {
        io::ErrorKind::PermissionDenied => TrashFailureKind::PermissionDenied,
        io::ErrorKind::ResourceBusy => TrashFailureKind::InUse,
        io::ErrorKind::ReadOnlyFilesystem | io::ErrorKind::CrossesDevices => {
            TrashFailureKind::NoTrashOnDrive
        }
        io::ErrorKind::StorageFull | io::ErrorKind::FileTooLarge | io::ErrorKind::QuotaExceeded => {
            TrashFailureKind::TooLarge
        }
        _ => TrashFailureKind::Other(message.to_owned()),
    }
}

fn classify_trash_error(error: &trash::Error) -> TrashFailureKind {
    match error {
        #[cfg(all(
            unix,
            not(target_os = "macos"),
            not(target_os = "ios"),
            not(target_os = "android")
        ))]
        trash::Error::FileSystem { source, .. } => {
            classify_io_kind(source.kind(), &source.to_string())
        }
        trash::Error::Os { code, description } => classify_os_code(*code, description),
        trash::Error::CouldNotAccess { .. } => TrashFailureKind::PermissionDenied,
        other => TrashFailureKind::Other(other.to_string()),
    }
}

/// Windows reports HRESULTs (`0x8007xxxx` wraps a Win32 error). Other platforms use
/// framework-specific codes, so only the description is kept there.
fn classify_os_code(code: i32, description: &str) -> TrashFailureKind {
    #[cfg(windows)]
    {
        let code = code as u32;
        let win32 = if code & 0xFFFF_0000 == 0x8007_0000 {
            code & 0xFFFF
        } else {
            code
        };
        match win32 {
            5 => return TrashFailureKind::PermissionDenied, // ERROR_ACCESS_DENIED
            32 | 33 => return TrashFailureKind::InUse,      // sharing / lock violation
            39 | 112 => return TrashFailureKind::TooLarge,  // disk full
            _ => {}
        }
    }
    #[cfg(not(windows))]
    let _ = code;
    TrashFailureKind::Other(description.to_owned())
}

/// Result of a removal step. Variants marked "nothing changed" leave the list, the
/// manifest, and the folder exactly as they were.
#[derive(Debug)]
pub enum RemoveOutcome {
    /// The folder is in the trash and the entry was removed and saved.
    Trashed { profile: InstanceProfile },
    /// The entry was removed and saved; the folder was not touched.
    RemovedFromList {
        profile: InstanceProfile,
        folder: PathBuf,
    },
    /// The entry was removed and saved, and the folder was deleted for good.
    Deleted { profile: InstanceProfile },
    /// The entry was removed and saved, but some files could not be deleted.
    PartiallyDeleted {
        profile: InstanceProfile,
        folder: PathBuf,
        /// Paths still on disk (at most `MAX_LEFTOVERS`).
        failed: Vec<PathBuf>,
        error: String,
    },
    /// Refused, nothing changed: another entry uses the same folder. Remove-from-list
    /// ([`RemoveMode::KeepFiles`]) is still possible.
    SharedFolder { other: String },
    /// Refused, nothing changed: the game of this instance is running right now.
    NowRunning,
    /// Refused, nothing changed: the folder does not exist, so there is nothing to trash
    /// or delete. Use [`RemoveMode::KeepFiles`] to remove the entry.
    FolderMissing,
    /// The OS trash refused; nothing changed (the entry is still listed). Never falls
    /// back to permanent deletion.
    TrashFailed {
        kind: TrashFailureKind,
        error: String,
        /// Whether the folder still exists (it always does when this is returned; a
        /// folder that vanished despite an error is treated as trashed).
        folder_still_present: bool,
    },
    /// The folder was trashed, but saving the manifest failed. The entry is still
    /// listed and now shows as "folder missing"; removing it with KeepFiles later works.
    TrashedButNotSaved {
        profile: InstanceProfile,
        error: String,
    },
    /// Refused or failed before anything changed (unknown instance, unsafe folder name,
    /// manifest could not be saved, ...).
    Failed(InstanceError),
}

impl RemoveOutcome {
    /// Whether the instance is no longer in the saved list.
    pub fn removed_from_list(&self) -> bool {
        matches!(
            self,
            Self::Trashed { .. }
                | Self::RemovedFromList { .. }
                | Self::Deleted { .. }
                | Self::PartiallyDeleted { .. }
        )
    }
}

/// A checked removal, produced by [`prepare_removal`] and consumed by the later steps.
#[derive(Debug, Clone)]
pub struct RemovalTarget {
    profile: InstanceProfile,
    mode: RemoveMode,
}

impl RemovalTarget {
    pub fn profile(&self) -> &InstanceProfile {
        &self.profile
    }
    pub fn mode(&self) -> RemoveMode {
        self.mode
    }
}

/// Re-validates `profile`'s folder name and returns `instances_dir/<dir>`, refusing
/// anything that is not a strict, direct child of the instances root.
fn checked_folder(paths: &AppPaths, profile: &InstanceProfile) -> Result<PathBuf, InstanceError> {
    let name = InstanceDirName::parse(profile.directory().as_str()).map_err(|error| {
        InstanceError::Io(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("refusing to touch unsafe instance folder name: {error}"),
        ))
    })?;
    let root = paths.instances_dir();
    let path = root.join(name.as_str());
    instances::ensure_game_dir_contained(paths, &path)?;
    if path.parent() != Some(root.as_path()) {
        return Err(InstanceError::Io(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("refusing to touch {}", path.display()),
        )));
    }
    Ok(path)
}

fn exists_no_follow(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok()
}

/// Step 1 (UI thread): finds the instance and runs every check that does not need the
/// file system to change. `is_running` is asked now; later steps ask again.
pub fn prepare_removal(
    paths: &AppPaths,
    profiles: &[InstanceProfile],
    skipped: &[SkippedEntry],
    directory: &InstanceDirName,
    mode: RemoveMode,
    is_running: &dyn Fn(&InstanceDirName) -> bool,
) -> Result<RemovalTarget, RemoveOutcome> {
    let index = instances::find_profile(profiles, directory).map_err(RemoveOutcome::Failed)?;
    let profile = profiles[index].clone();
    let folder = checked_folder(paths, &profile).map_err(RemoveOutcome::Failed)?;
    if is_running(profile.directory()) {
        return Err(RemoveOutcome::NowRunning);
    }
    if mode != RemoveMode::KeepFiles {
        if let Some(other) =
            instances::folder_user(profiles, skipped, profile.directory(), Some(index))
        {
            return Err(RemoveOutcome::SharedFolder { other });
        }
        if !exists_no_follow(&folder) {
            return Err(RemoveOutcome::FolderMissing);
        }
    }
    Ok(RemovalTarget { profile, mode })
}

/// Step 2 for [`RemoveMode::Trash`] (worker thread): moves the folder to the trash and
/// confirms it is gone. Does not touch the list; call [`finish_trash`] on success.
pub fn trash_target(
    paths: &AppPaths,
    target: &RemovalTarget,
    trasher: &dyn Trasher,
    is_running: &dyn Fn(&InstanceDirName) -> bool,
) -> Result<(), RemoveOutcome> {
    let folder = checked_folder(paths, &target.profile).map_err(RemoveOutcome::Failed)?;
    if is_running(target.profile.directory()) {
        return Err(RemoveOutcome::NowRunning);
    }
    if !exists_no_follow(&folder) {
        return Err(RemoveOutcome::FolderMissing);
    }
    let result = trasher.trash(&folder);
    // Trust the file system, not the backend's return value.
    let still_present = exists_no_follow(&folder);
    match result {
        Ok(()) if !still_present => Ok(()),
        Ok(()) => Err(RemoveOutcome::TrashFailed {
            kind: TrashFailureKind::Other("the folder is still there".into()),
            error: format!(
                "the trash reported success, but {} still exists",
                folder.display()
            ),
            folder_still_present: true,
        }),
        Err(error) if !still_present => {
            eprintln!(
                "Ferrite: trash reported an error but {} is gone; treating it as trashed: {error}",
                folder.display()
            );
            Ok(())
        }
        Err(error) => Err(RemoveOutcome::TrashFailed {
            kind: error.kind,
            error: error.message,
            folder_still_present: true,
        }),
    }
}

/// Step 3 for [`RemoveMode::Trash`] (UI thread): removes the entry and saves. If the
/// save fails the entry is put back and [`RemoveOutcome::TrashedButNotSaved`] returned.
pub fn finish_trash(
    paths: &AppPaths,
    profiles: &mut Vec<InstanceProfile>,
    skipped: &[SkippedEntry],
    target: RemovalTarget,
) -> RemoveOutcome {
    let Ok(index) = instances::find_profile(profiles, target.profile.directory()) else {
        return RemoveOutcome::Trashed {
            profile: target.profile,
        };
    };
    let removed = profiles.remove(index);
    match instances::save(paths, profiles, skipped) {
        Ok(()) => RemoveOutcome::Trashed { profile: removed },
        Err(error) => {
            profiles.insert(index, removed);
            RemoveOutcome::TrashedButNotSaved {
                profile: target.profile,
                error: error.to_string(),
            }
        }
    }
}

/// Step 2 for [`RemoveMode::Permanent`] and [`RemoveMode::KeepFiles`] (UI thread):
/// re-checks that the game is not running, removes the entry and saves. On failure the
/// list is restored and nothing changed. For KeepFiles this is the last step.
pub fn commit_removal(
    paths: &AppPaths,
    profiles: &mut Vec<InstanceProfile>,
    skipped: &[SkippedEntry],
    target: &RemovalTarget,
    is_running: &dyn Fn(&InstanceDirName) -> bool,
) -> Result<RemoveOutcome, RemoveOutcome> {
    let folder = checked_folder(paths, &target.profile).map_err(RemoveOutcome::Failed)?;
    if is_running(target.profile.directory()) {
        return Err(RemoveOutcome::NowRunning);
    }
    let index = instances::find_profile(profiles, target.profile.directory())
        .map_err(RemoveOutcome::Failed)?;
    let removed = profiles.remove(index);
    if let Err(error) = instances::save(paths, profiles, skipped) {
        profiles.insert(index, removed);
        return Err(RemoveOutcome::Failed(error));
    }
    Ok(RemoveOutcome::RemovedFromList {
        profile: removed,
        folder,
    })
}

/// Whether removal finished synchronously or its file worker was scheduled.
#[derive(Debug)]
pub enum RemovalStart {
    Completed(RemoveOutcome),
    Scheduled,
}

/// Orders the manifest step around frontend-owned worker scheduling.
/// A failed permanent-delete spawn restores the entry before returning.
pub fn start_prepared_removal(
    paths: &AppPaths,
    profiles: &mut Vec<InstanceProfile>,
    skipped: &[SkippedEntry],
    target: &RemovalTarget,
    is_running: &dyn Fn(&InstanceDirName) -> bool,
    schedule: impl FnOnce() -> io::Result<()>,
) -> Result<RemovalStart, RemoveOutcome> {
    if target.mode == RemoveMode::KeepFiles {
        return commit_removal(paths, profiles, skipped, target, is_running)
            .map(RemovalStart::Completed);
    }
    let removed = if target.mode == RemoveMode::Permanent {
        let index = instances::find_profile(profiles, target.profile.directory())
            .map_err(RemoveOutcome::Failed)?;
        let profile = profiles[index].clone();
        commit_removal(paths, profiles, skipped, target, is_running)?;
        Some((index, profile))
    } else {
        None
    };
    if let Err(error) = schedule() {
        if let Some((index, profile)) = removed {
            profiles.insert(index, profile);
            if let Err(restore_error) = instances::save(paths, profiles, skipped) {
                return Err(RemoveOutcome::Failed(InstanceError::Install(format!(
                    "could not start removal: {error}; restored the instance in memory, but could not restore its manifest: {restore_error}"
                ))));
            }
        }
        return Err(RemoveOutcome::Failed(InstanceError::Io(io::Error::new(
            error.kind(),
            format!("could not start removal: {error}"),
        ))));
    }
    Ok(RemovalStart::Scheduled)
}

/// Runs the file step selected by a checked removal target.
pub fn run_removal_files(
    paths: &AppPaths,
    target: &RemovalTarget,
    trasher: &dyn Trasher,
    is_running: &dyn Fn(&InstanceDirName) -> bool,
) -> Result<(), RemoveOutcome> {
    match target.mode {
        RemoveMode::Trash => trash_target(paths, target, trasher, is_running),
        RemoveMode::Permanent => match delete_target(paths, target) {
            RemoveOutcome::Deleted { .. } => Ok(()),
            outcome => Err(outcome),
        },
        RemoveMode::KeepFiles => Ok(()),
    }
}

/// Accepts a file worker result and performs any remaining manifest commit.
pub fn finish_removal(
    paths: &AppPaths,
    profiles: &mut Vec<InstanceProfile>,
    skipped: &[SkippedEntry],
    target: RemovalTarget,
    result: Result<(), RemoveOutcome>,
) -> RemoveOutcome {
    match (target.mode, result) {
        (RemoveMode::Trash, Ok(())) => finish_trash(paths, profiles, skipped, target),
        (_, Ok(())) => RemoveOutcome::Deleted {
            profile: target.profile,
        },
        (_, Err(outcome)) => outcome,
    }
}

/// Step 3 for [`RemoveMode::Permanent`] (worker thread, after [`commit_removal`]):
/// deletes the folder. `remove_dir_all` does not follow links (a link at the folder
/// path is removed as the link). On failure, lists what is left.
pub fn delete_target(paths: &AppPaths, target: &RemovalTarget) -> RemoveOutcome {
    let folder = match checked_folder(paths, &target.profile) {
        Ok(folder) => folder,
        Err(error) => return RemoveOutcome::Failed(error),
    };
    if !exists_no_follow(&folder) {
        return RemoveOutcome::Deleted {
            profile: target.profile.clone(),
        };
    }
    match fs::remove_dir_all(&folder) {
        Ok(()) => RemoveOutcome::Deleted {
            profile: target.profile.clone(),
        },
        Err(error) => RemoveOutcome::PartiallyDeleted {
            profile: target.profile.clone(),
            failed: leftovers(&folder),
            folder,
            error: error.to_string(),
        },
    }
}

/// Read-only listing of what is left under `root` (no links followed), deepest
/// entries first, capped at `MAX_LEFTOVERS`.
fn leftovers(root: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if found.len() >= MAX_LEFTOVERS {
                return found;
            }
            match fs::symlink_metadata(&path) {
                Ok(metadata)
                    if metadata.is_dir() && !crate::core::fsutil::is_link_like(&metadata) =>
                {
                    pending.push(path.clone());
                    found.push(path);
                }
                _ => found.push(path),
            }
        }
    }
    if found.is_empty() && exists_no_follow(root) {
        found.push(root.to_path_buf());
    }
    found
}

/// Runs a whole removal on the calling thread (see the module docs for ordering).
///
/// `is_running` is consulted when checking and again right before acting.
pub fn remove_instance(
    paths: &AppPaths,
    profiles: &mut Vec<InstanceProfile>,
    skipped: &[SkippedEntry],
    directory: &InstanceDirName,
    mode: RemoveMode,
    trasher: &dyn Trasher,
    is_running: &dyn Fn(&InstanceDirName) -> bool,
) -> RemoveOutcome {
    let target = match prepare_removal(paths, profiles, skipped, directory, mode, is_running) {
        Ok(target) => target,
        Err(outcome) => return outcome,
    };
    match mode {
        RemoveMode::Trash => match trash_target(paths, &target, trasher, is_running) {
            Ok(()) => finish_trash(paths, profiles, skipped, target),
            Err(outcome) => outcome,
        },
        RemoveMode::KeepFiles => {
            match commit_removal(paths, profiles, skipped, &target, is_running) {
                Ok(outcome) | Err(outcome) => outcome,
            }
        }
        RemoveMode::Permanent => {
            match commit_removal(paths, profiles, skipped, &target, is_running) {
                Ok(_) => delete_target(paths, &target),
                Err(outcome) => outcome,
            }
        }
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod workflow_tests {
    use super::*;
    use crate::core::paths::test_support::paths_in;
    use std::cell::Cell;

    #[test]
    fn permanent_spawn_failure_restores_original_manifest_position() {
        let temp = tempfile::tempdir().unwrap();
        let paths = paths_in(temp.path());
        let mut profiles = Vec::new();
        for name in ["Before", "Delete", "After"] {
            let profile =
                instances::new_instance_profile(&paths, name, "1", "Vanilla", &profiles, &[])
                    .unwrap();
            instances::create_new_game_dir(&paths, &profile).unwrap();
            profiles.push(profile);
        }
        instances::save(&paths, &profiles, &[]).unwrap();
        let before = profiles.clone();
        let target = prepare_removal(
            &paths,
            &profiles,
            &[],
            before[1].directory(),
            RemoveMode::Permanent,
            &|_| false,
        )
        .unwrap();
        let called = Cell::new(false);
        let started =
            start_prepared_removal(&paths, &mut profiles, &[], &target, &|_| false, || {
                called.set(true);
                assert_eq!(
                    instances::load(&paths).unwrap().profiles,
                    vec![before[0].clone(), before[2].clone()]
                );
                assert!(before[1].game_dir(&paths).is_dir());
                Err(io::Error::other("spawn failed"))
            });
        assert!(called.get());
        assert!(matches!(started, Err(RemoveOutcome::Failed(_))));
        assert_eq!(profiles, before);
        assert_eq!(instances::load(&paths).unwrap().profiles, before);
        assert!(before[1].game_dir(&paths).is_dir());
    }
    #[test]
    fn scheduling_preserves_trash_permanent_and_keep_files_ordering() {
        for mode in [
            RemoveMode::Trash,
            RemoveMode::Permanent,
            RemoveMode::KeepFiles,
        ] {
            let temp = tempfile::tempdir().unwrap();
            let paths = paths_in(temp.path());
            let profile =
                instances::new_instance_profile(&paths, "Delete", "1", "Vanilla", &[], &[])
                    .unwrap();
            instances::create_new_game_dir(&paths, &profile).unwrap();
            let mut profiles = vec![profile.clone()];
            instances::save(&paths, &profiles, &[]).unwrap();
            let target =
                prepare_removal(&paths, &profiles, &[], profile.directory(), mode, &|_| {
                    false
                })
                .unwrap();
            let scheduled = Cell::new(false);
            let result =
                start_prepared_removal(&paths, &mut profiles, &[], &target, &|_| false, || {
                    scheduled.set(true);
                    assert!(profile.game_dir(&paths).is_dir());
                    let loaded = instances::load(&paths).unwrap();
                    assert_eq!(loaded.profiles.is_empty(), mode == RemoveMode::Permanent);
                    Ok(())
                })
                .unwrap();
            assert_eq!(scheduled.get(), mode != RemoveMode::KeepFiles);
            assert_eq!(profiles.is_empty(), mode != RemoveMode::Trash);
            assert_eq!(
                matches!(result, RemovalStart::Completed(_)),
                mode == RemoveMode::KeepFiles
            );
            assert!(profile.game_dir(&paths).is_dir());
        }
    }

    #[test]
    fn failed_spawn_reports_manifest_restoration_failure_without_overwriting_corruption() {
        let temp = tempfile::tempdir().unwrap();
        let paths = paths_in(temp.path());
        let profile =
            instances::new_instance_profile(&paths, "Delete", "1", "Vanilla", &[], &[]).unwrap();
        instances::create_new_game_dir(&paths, &profile).unwrap();
        let mut profiles = vec![profile.clone()];
        instances::save(&paths, &profiles, &[]).unwrap();
        let target = prepare_removal(
            &paths,
            &profiles,
            &[],
            profile.directory(),
            RemoveMode::Permanent,
            &|_| false,
        )
        .unwrap();
        let result =
            start_prepared_removal(&paths, &mut profiles, &[], &target, &|_| false, || {
                fs::write(paths.instances_manifest(), "{ corrupt").unwrap();
                Err(io::Error::other("spawn failed"))
            });
        assert!(
            matches!(result, Err(RemoveOutcome::Failed(InstanceError::Install(ref message)))
            if message.contains("could not restore its manifest"))
        );
        assert_eq!(profiles, vec![profile.clone()]);
        assert!(profile.game_dir(&paths).is_dir());
        assert_eq!(
            fs::read_to_string(paths.instances_manifest()).unwrap(),
            "{ corrupt"
        );
    }
}
