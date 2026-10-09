//! Duplicating an instance: a full, independent copy of its folder under a new name.
//!
//! ## Guarantees
//!
//! * Files are copied with the migration copier ([`crate::core::copy`]): never
//!   hard-linked, written to `.ferrite-partial` names, timestamped, fsynced, then
//!   renamed. Genuine links in the source are not followed or copied; they are listed
//!   in [`CopiedDuplicate::skipped_links`]. Anything else that can't be copied
//!   (special files, unreadable files or folders) fails the duplicate with
//!   [`DuplicateError::UncopyableFiles`], so a copy is never missing files silently.
//! * The copy goes into a staging folder `instances/.<dir>.duplicate.<pid>.<n>.tmp`,
//!   created exclusively, on the same file system as the final folder. Cancel or any
//!   error removes only that staging folder. The source is only read.
//! * Free space is checked before copying ([`DuplicateError::NotEnoughSpace`]).
//! * The source must not be running: checked when the duplicate is prepared, again
//!   when the copy starts, and again before committing ([`DuplicateError::NowRunning`]).
//!   Right before committing the source is rescanned; if anything changed during the
//!   copy, nothing is committed ([`DuplicateError::SourceChanged`]).
//! * Commit is a rename that never replaces anything, not even an empty folder
//!   ([`fsutil::rename_no_replace`]), followed by the manifest save. If the save fails,
//!   only the new folder is removed.
//!
//! ## Threading
//!
//! [`prepare_duplicate`] and [`commit_duplicate`] run on the UI thread (they read or
//! change the profile list). [`copy_duplicate`] does the slow part; run it through
//! [`DuplicateTask::spawn`] and poll [`DuplicateTask::try_finish`].
//! [`duplicate_instance`] runs every step on the calling thread.

use crate::core::copy::{
    self, CopyFailure, CopyObserver, TreeScan, UncopyableFile, describe_change, with_margin,
};
use crate::core::fsutil;
use crate::core::instances::{self, InstanceDirName, InstanceError, InstanceProfile, SkippedEntry};
use crate::core::paths::AppPaths;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

/// The `<kind>` part of duplicate staging folder names (see [`staging_dir_name`]).
pub const STAGING_KIND: &str = "duplicate";

static STAGING_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Coarse step for UI labels (same shape as the migration's).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DuplicateStep {
    #[default]
    Starting,
    Scanning,
    Copying,
    Finalizing,
    Done,
}

/// Snapshot of duplicate progress.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DuplicateProgress {
    pub step: DuplicateStep,
    pub files_done: u64,
    pub files_total: u64,
    pub bytes_done: u64,
    pub bytes_total: u64,
}

impl DuplicateProgress {
    /// Fraction of bytes copied while copying, `Some(1.0)` when done, and `None` for
    /// steps without measurable progress (show an indeterminate bar, never 100% early).
    pub fn fraction(&self) -> Option<f32> {
        match self.step {
            DuplicateStep::Done => Some(1.0),
            DuplicateStep::Copying if self.bytes_total == 0 => Some(0.0),
            DuplicateStep::Copying => {
                Some((self.bytes_done as f64 / self.bytes_total as f64).clamp(0.0, 1.0) as f32)
            }
            _ => None,
        }
    }
}

/// Shared progress + cancellation handle between the worker and the UI.
#[derive(Debug, Clone, Default)]
pub struct DuplicateControl {
    progress: Arc<Mutex<DuplicateProgress>>,
    cancel: Arc<AtomicBool>,
}

impl DuplicateControl {
    /// Latest progress snapshot.
    pub fn snapshot(&self) -> DuplicateProgress {
        self.progress
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .clone()
    }
    /// Requests cancellation at the next file/chunk boundary. Ignored once the final
    /// rename has happened.
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
    /// Whether cancellation was requested.
    pub fn is_cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }
    fn update(&self, change: impl FnOnce(&mut DuplicateProgress)) {
        change(
            &mut self
                .progress
                .lock()
                .unwrap_or_else(|poison| poison.into_inner()),
        );
    }
    fn check_cancel(&self) -> Result<(), DuplicateError> {
        if self.is_cancelled() {
            Err(DuplicateError::Cancelled)
        } else {
            Ok(())
        }
    }
}

/// Why a duplicate did not happen. In every case the source is unchanged and no new
/// entry or folder was left behind.
#[derive(Debug)]
pub enum DuplicateError {
    /// The source instance's game is running (or started while the dialog was open).
    NowRunning,
    /// The source instance's folder is missing.
    SourceMissing,
    /// Not enough free space in the instances folder. `needed` includes a 5% margin.
    NotEnoughSpace {
        needed: u64,
        available: u64,
        volume: PathBuf,
    },
    /// Entries in the source that are not links but can't be copied.
    UncopyableFiles(Vec<UncopyableFile>),
    /// The source changed while it was being copied.
    SourceChanged(String),
    /// Something appeared at the new folder's path; it was not replaced.
    TargetExists(PathBuf),
    /// The user cancelled; the staging folder was removed.
    Cancelled,
    /// Name, list, or file system error.
    Failed(InstanceError),
    /// The background worker exited without reporting a result.
    WorkerStopped,
}

impl fmt::Display for DuplicateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NowRunning => write!(f, "the instance is running; close Minecraft first"),
            Self::SourceMissing => write!(f, "the instance's folder is missing"),
            Self::NotEnoughSpace {
                needed,
                available,
                volume,
            } => write!(
                f,
                "not enough free space on {}: needs {}, {} free",
                volume.display(),
                crate::core::migration::format_size(*needed),
                crate::core::migration::format_size(*available)
            ),
            Self::UncopyableFiles(files) => {
                write!(f, "{} item(s) could not be copied: ", files.len())?;
                for (index, file) in files.iter().take(20).enumerate() {
                    if index > 0 {
                        write!(f, "; ")?;
                    }
                    write!(f, "{file}")?;
                }
                if files.len() > 20 {
                    write!(f, "; and {} more", files.len() - 20)?;
                }
                Ok(())
            }
            Self::SourceChanged(detail) => {
                write!(
                    f,
                    "the instance changed while it was being copied ({detail})"
                )
            }
            Self::TargetExists(path) => write!(
                f,
                "{} appeared while copying and was not replaced",
                path.display()
            ),
            Self::Cancelled => write!(f, "duplicate cancelled"),
            Self::Failed(error) => write!(f, "{error}"),
            Self::WorkerStopped => write!(f, "the duplicate worker stopped unexpectedly"),
        }
    }
}

impl std::error::Error for DuplicateError {}

impl From<InstanceError> for DuplicateError {
    fn from(error: InstanceError) -> Self {
        Self::Failed(error)
    }
}

impl From<CopyFailure> for DuplicateError {
    fn from(failure: CopyFailure) -> Self {
        match failure {
            CopyFailure::Io { context, error } => Self::Failed(InstanceError::Io(io::Error::new(
                error.kind(),
                format!("{context}: {error}"),
            ))),
            CopyFailure::Uncopyable(files) => Self::UncopyableFiles(files),
            CopyFailure::Cancelled => Self::Cancelled,
            CopyFailure::Interrupted(error) => Self::Failed(InstanceError::Io(error)),
        }
    }
}

/// A checked duplicate request from [`prepare_duplicate`].
#[derive(Debug, Clone)]
pub struct DuplicatePlan {
    source: InstanceProfile,
    profile: InstanceProfile,
}

impl DuplicatePlan {
    /// The instance being copied.
    pub fn source(&self) -> &InstanceProfile {
        &self.source
    }
    /// The new instance (validated name, freshly allocated folder).
    pub fn profile(&self) -> &InstanceProfile {
        &self.profile
    }
}

/// A finished copy, removed on drop until its manifest commit takes ownership.
#[derive(Debug)]
pub struct CopiedDuplicate {
    paths: AppPaths,
    cleanup: Option<PathBuf>,
    pub profile: InstanceProfile,
    pub files_copied: u64,
    pub bytes_copied: u64,
    /// Links in the source (relative paths) that were not copied.
    pub skipped_links: Vec<PathBuf>,
}

impl Drop for CopiedDuplicate {
    fn drop(&mut self) {
        if let Some(path) = &self.cleanup
            && instances::ensure_game_dir_contained(&self.paths, path).is_ok()
            && let Err(error) = fs::remove_dir_all(path)
            && error.kind() != io::ErrorKind::NotFound
        {
            eprintln!(
                "Ferrite: could not remove uncommitted duplicate {}: {error}",
                path.display()
            );
        }
    }
}

/// A committed duplicate.
#[derive(Debug, Clone)]
pub struct DuplicateReport {
    /// Index of the new profile in the list.
    pub index: usize,
    pub profile: InstanceProfile,
    pub files_copied: u64,
    pub bytes_copied: u64,
    /// Links in the source (relative paths) that were not copied.
    pub skipped_links: Vec<PathBuf>,
}

/// Step 1 (UI thread): validates the new name, checks the source is idle and present,
/// and allocates a free folder for the copy. Nothing is created.
pub fn prepare_duplicate(
    paths: &AppPaths,
    profiles: &[InstanceProfile],
    skipped: &[SkippedEntry],
    source: &InstanceDirName,
    new_name: &str,
    is_running: &dyn Fn(&InstanceDirName) -> bool,
) -> Result<DuplicatePlan, DuplicateError> {
    let index = instances::find_profile(profiles, source)?;
    let source = profiles[index].clone();
    let name =
        instances::validate_instance_name(new_name, profiles, None).map_err(InstanceError::from)?;
    if is_running(source.directory()) {
        return Err(DuplicateError::NowRunning);
    }
    source_folder(paths, &source)?;
    let directory = instances::allocate_directory(paths, &name, profiles, skipped)?;
    let profile = InstanceProfile::with_directory(
        name,
        source.version.clone(),
        source.loader.clone(),
        directory,
    );
    Ok(DuplicatePlan { source, profile })
}

/// The source's folder, which must be a real folder (not a link) inside the
/// instances root.
fn source_folder(paths: &AppPaths, source: &InstanceProfile) -> Result<PathBuf, DuplicateError> {
    let folder = source.game_dir(paths);
    instances::ensure_game_dir_contained(paths, &folder)?;
    match fs::symlink_metadata(&folder) {
        Ok(metadata) if metadata.is_dir() && !fsutil::is_link_like(&metadata) => Ok(folder),
        Ok(_) => Err(DuplicateError::Failed(InstanceError::Io(io::Error::other(
            format!("{} is not a folder", folder.display()),
        )))),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Err(DuplicateError::SourceMissing),
        Err(error) => Err(DuplicateError::Failed(error.into())),
    }
}

/// Name of a duplicate staging folder for the new folder `directory`:
/// `.<directory>.duplicate.<pid>.<n>.tmp`.
pub fn staging_dir_name(directory: &InstanceDirName) -> String {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let counter = STAGING_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!(
        ".{}.{STAGING_KIND}.{}.{}.tmp",
        directory.as_str(),
        std::process::id(),
        stamp + u128::from(counter)
    )
}

/// Removes the staging folder on drop unless the copy was committed.
struct StagingGuard {
    path: Option<PathBuf>,
}

impl StagingGuard {
    fn disarm(&mut self) {
        self.path = None;
    }
}

impl Drop for StagingGuard {
    fn drop(&mut self) {
        if let Some(path) = &self.path
            && let Err(error) = fs::remove_dir_all(path)
            && error.kind() != io::ErrorKind::NotFound
        {
            eprintln!(
                "Ferrite: could not remove duplicate staging folder {}: {error}",
                path.display()
            );
        }
    }
}

type FileHook<'a> = &'a dyn Fn(u64) -> io::Result<()>;
type PathHook<'a> = &'a dyn Fn(&Path);
type StepHook<'a> = &'a dyn Fn();

/// Test hooks for simulating races at precise points.
#[derive(Default)]
struct Hooks<'a> {
    available_space: Option<&'a dyn Fn() -> u64>,
    after_file: Option<FileHook<'a>>,
    before_recheck: Option<PathHook<'a>>,
    before_rename: Option<StepHook<'a>>,
}

struct DuplicateCopy<'a> {
    control: &'a DuplicateControl,
    hooks: &'a Hooks<'a>,
}

impl CopyObserver for DuplicateCopy<'_> {
    fn cancelled(&self) -> bool {
        self.control.is_cancelled()
    }
    fn add_bytes(&self, bytes: u64) {
        self.control.update(|progress| progress.bytes_done += bytes);
    }
    fn file_done(&self, files_done: u64) -> io::Result<()> {
        self.control
            .update(|progress| progress.files_done = files_done);
        match self.hooks.after_file {
            Some(hook) => hook(files_done),
            None => Ok(()),
        }
    }
    fn all_copied(&self) {
        self.control
            .update(|progress| progress.step = DuplicateStep::Finalizing);
    }
}

/// Step 2 (worker thread): copies the source into staging and renames it into the new
/// folder. Does not touch the list; call [`commit_duplicate`] with the result.
pub fn copy_duplicate(
    paths: &AppPaths,
    plan: &DuplicatePlan,
    control: &DuplicateControl,
    is_running: &dyn Fn(&InstanceDirName) -> bool,
) -> Result<CopiedDuplicate, DuplicateError> {
    copy_with_hooks(paths, plan, control, is_running, &Hooks::default())
}

fn copy_with_hooks(
    paths: &AppPaths,
    plan: &DuplicatePlan,
    control: &DuplicateControl,
    is_running: &dyn Fn(&InstanceDirName) -> bool,
    hooks: &Hooks<'_>,
) -> Result<CopiedDuplicate, DuplicateError> {
    let instances_dir = paths.instances_dir();
    let target = plan.profile.game_dir(paths);
    instances::ensure_game_dir_contained(paths, &target)?;
    if is_running(plan.source.directory()) {
        return Err(DuplicateError::NowRunning);
    }
    let source = source_folder(paths, &plan.source)?;
    if fs::symlink_metadata(&target).is_ok() {
        return Err(DuplicateError::TargetExists(target));
    }

    control.update(|progress| progress.step = DuplicateStep::Scanning);
    let scan = copy::scan_tree(&source)?;
    if !scan.blocked.is_empty() {
        return Err(DuplicateError::UncopyableFiles(scan.blocked));
    }
    check_free_space(&instances_dir, &scan, hooks)?;
    control.check_cancel()?;

    let staging = instances_dir.join(staging_dir_name(plan.profile.directory()));
    instances::ensure_game_dir_contained(paths, &staging)?;
    // Exclusive: never adopt an existing folder as staging.
    fs::create_dir(&staging).map_err(InstanceError::from)?;
    let mut guard = StagingGuard {
        path: Some(staging.clone()),
    };

    control.update(|progress| {
        progress.step = DuplicateStep::Copying;
        progress.files_total = scan.files.len() as u64;
        progress.bytes_total = scan.total_bytes;
        progress.files_done = 0;
        progress.bytes_done = 0;
    });
    copy::copy_tree(&source, &staging, &scan, &DuplicateCopy { control, hooks })?;
    control.update(|progress| progress.step = DuplicateStep::Finalizing);

    if let Some(hook) = hooks.before_recheck {
        hook(&source);
    }
    if is_running(plan.source.directory()) {
        return Err(DuplicateError::NowRunning);
    }
    let recheck = copy::scan_tree(&source)?;
    if let Some(change) = describe_change(&scan, &recheck) {
        eprintln!("Ferrite: instance changed while duplicating: {change}");
        return Err(DuplicateError::SourceChanged(change));
    }
    fsutil::full_flush(&staging).map_err(InstanceError::from)?;
    control.check_cancel()?;

    if let Some(hook) = hooks.before_rename {
        hook();
    }
    match fsutil::rename_no_replace_with_retry(&staging, &target) {
        Ok(()) => guard.path = Some(target.clone()),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            return Err(DuplicateError::TargetExists(target));
        }
        Err(error) => return Err(InstanceError::from(error).into()),
    }
    if let Err(error) = fsutil::sync_dir(&instances_dir) {
        eprintln!(
            "Ferrite: could not sync {} after duplicating: {error}",
            instances_dir.display()
        );
    }
    control.update(|progress| {
        progress.files_done = progress.files_total;
        progress.bytes_done = progress.bytes_total;
    });
    let copied = CopiedDuplicate {
        paths: paths.clone(),
        cleanup: Some(target),
        profile: plan.profile.clone(),
        files_copied: scan.files.len() as u64,
        bytes_copied: scan.total_bytes,
        skipped_links: scan.links,
    };
    guard.disarm();
    Ok(copied)
}

fn check_free_space(
    instances_dir: &Path,
    scan: &TreeScan,
    hooks: &Hooks<'_>,
) -> Result<(), DuplicateError> {
    let available = match hooks.available_space {
        Some(query) => query(),
        None => fsutil::available_space(instances_dir).map_err(InstanceError::from)?,
    };
    space_check(instances_dir, scan.total_bytes, available)
}

/// The same free-space rule [`copy_duplicate`] applies, for the setup dialog: given the
/// source size (e.g. from [`crate::core::scan::scan_instance`]), returns
/// [`DuplicateError::NotEnoughSpace`] when the instances folder's volume can't hold a
/// copy plus a 5% margin. Run it off the UI thread with the scan.
pub fn preflight_space(paths: &AppPaths, source_bytes: u64) -> Result<(), DuplicateError> {
    let instances_dir = paths.instances_dir();
    // The instances folder may not exist yet; its parent is on the same volume.
    let probe = if instances_dir.exists() {
        instances_dir.clone()
    } else {
        paths.storage_root().to_path_buf()
    };
    let available = fsutil::available_space(&probe).map_err(InstanceError::from)?;
    space_check(&instances_dir, source_bytes, available)
}

fn space_check(volume: &Path, source_bytes: u64, available: u64) -> Result<(), DuplicateError> {
    let needed = with_margin(source_bytes);
    if needed > available {
        return Err(DuplicateError::NotEnoughSpace {
            needed,
            available,
            volume: volume.to_path_buf(),
        });
    }
    Ok(())
}

/// Step 3 (UI thread): adds the copy to the list and saves the manifest. If the name
/// was taken meanwhile or the save fails, only the new folder is removed.
pub fn commit_duplicate(
    paths: &AppPaths,
    profiles: &mut Vec<InstanceProfile>,
    skipped: &[SkippedEntry],
    mut copied: CopiedDuplicate,
    control: Option<&DuplicateControl>,
) -> Result<DuplicateReport, DuplicateError> {
    let result = instances::commit_new_instance(paths, profiles, skipped, copied.profile.clone());
    copied.cleanup = None;
    let index = result?;
    if let Some(control) = control {
        control.update(|progress| progress.step = DuplicateStep::Done);
    }
    Ok(DuplicateReport {
        index,
        profile: copied.profile.clone(),
        files_copied: copied.files_copied,
        bytes_copied: copied.bytes_copied,
        skipped_links: std::mem::take(&mut copied.skipped_links),
    })
}

/// Runs every step on the calling thread.
pub fn duplicate_instance(
    paths: &AppPaths,
    profiles: &mut Vec<InstanceProfile>,
    skipped: &[SkippedEntry],
    source: &InstanceDirName,
    new_name: &str,
    control: &DuplicateControl,
    is_running: &dyn Fn(&InstanceDirName) -> bool,
) -> Result<DuplicateReport, DuplicateError> {
    let plan = prepare_duplicate(paths, profiles, skipped, source, new_name, is_running)?;
    let copied = copy_duplicate(paths, &plan, control, is_running)?;
    commit_duplicate(paths, profiles, skipped, copied, Some(control))
}

/// A [`copy_duplicate`] running on a background thread.
pub struct DuplicateTask {
    control: DuplicateControl,
    plan: DuplicatePlan,
    result: Receiver<Result<CopiedDuplicate, DuplicateError>>,
}

impl DuplicateTask {
    /// Starts [`copy_duplicate`] on a worker thread. `is_running` is asked when the
    /// copy starts and before it is committed.
    pub fn spawn(
        paths: AppPaths,
        plan: DuplicatePlan,
        is_running: impl Fn(&InstanceDirName) -> bool + Send + 'static,
    ) -> io::Result<Self> {
        let control = DuplicateControl::default();
        let worker_control = control.clone();
        let worker_plan = plan.clone();
        let (sender, result) = mpsc::channel();
        std::thread::Builder::new()
            .name("instance-duplicate".into())
            .spawn(move || {
                let _ = sender.send(copy_duplicate(
                    &paths,
                    &worker_plan,
                    &worker_control,
                    &is_running,
                ));
            })?;
        Ok(Self {
            control,
            plan,
            result,
        })
    }
    /// Progress/cancellation handle.
    pub fn control(&self) -> &DuplicateControl {
        &self.control
    }
    /// The request being carried out.
    pub fn plan(&self) -> &DuplicatePlan {
        &self.plan
    }
    /// Non-blocking: the copy result once the worker has finished. Pass a success to
    /// [`commit_duplicate`].
    pub fn try_finish(&self) -> Option<Result<CopiedDuplicate, DuplicateError>> {
        match self.result.try_recv() {
            Ok(result) => Some(result),
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => Some(Err(DuplicateError::WorkerStopped)),
        }
    }
}

#[cfg(test)]
mod tests;
