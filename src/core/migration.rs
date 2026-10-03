//! Copy-first, non-destructive migration from the pre-Stage-1 CWD-relative
//! `minecraft/` directory into [`AppPaths::standard_storage_root`].
//!
//! ## Guarantees
//!
//! * **The old location is never modified or deleted.** It is only read.
//! * **Detection** ([`legacy_candidate_dirs`]): `<exe dir>/minecraft` and
//!   `<cwd>/minecraft`, canonicalized and de-duplicated. A candidate counts only if it
//!   contains a regular-file `instances.json` that parses as a Ferrite manifest
//!   ([`inspect_candidate`]). A random `minecraft` folder is ignored.
//! * **Planning** ([`plan`]) returns [`StartupPlan::Ready`] (fresh install, already
//!   migrated, or destination already in use), [`StartupPlan::NeedsMigration`] (one
//!   source, or an interrupted run to resume), or [`StartupPlan::NeedsUserChoice`] when
//!   two valid candidates differ. It never picks between differing candidates.
//! * **Copying** ([`run_migration`]) goes into `<data dir>/.migration-staging/minecraft`,
//!   which is on the same filesystem as the destination. Genuine links (symlinks;
//!   on Windows junctions and other name-surrogate reparse points) are never followed
//!   or copied; each one is logged and listed in the report and state file. Other
//!   reparse points (OneDrive placeholders, dedup) are read like regular files.
//!   Anything else that cannot be copied (special files, unreadable files or folders)
//!   **blocks the commit** with [`MigrationError::UncopyableFiles`] listing the paths,
//!   so a migration can never "succeed" with files missing. Every file is written to a
//!   `.ferrite-partial` name, fsynced, then renamed, so a staged file with its final
//!   name is always complete.
//! * **Verification** compares the staged tree against the source scan (the exact
//!   set of relative paths and every file size) and the manifest bytes. On failure
//!   only the staging directory is deleted.
//! * **Commit** is a single `rename(staging/minecraft -> <data dir>/minecraft)`, which
//!   is refused if the destination exists. Existing destinations are never overwritten.
//! * **Resumability**: `migration-state.json` records the phase (`copying`, `verified`,
//!   `completed`, ...). Re-running after an interruption at any point either resumes
//!   the copy (keeping completed staged files), finishes a commit whose rename already
//!   happened, or (when already completed) does nothing.
//! * **Rollback**: [`rollback_to_legacy`] switches back to the old location after a
//!   completed migration by rewriting only the state file; neither tree is touched.
//!
//! The only use of the process working directory in Ferrite is
//! [`default_legacy_candidate_dirs`], which reads it solely to *find* old data.

use crate::core::fsutil;
use crate::core::manifest::{self, InstanceManifest};
use crate::core::paths::{AppPaths, INSTANCES_MANIFEST_FILE, STORAGE_DIR_NAME};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

/// Schema version of `migration-state.json`.
pub const STATE_SCHEMA_VERSION: u32 = 1;
/// Suffix for in-flight staged files.
const PARTIAL_SUFFIX: &str = ".ferrite-partial";
const COPY_CHUNK: usize = 1024 * 1024;

// =====================================================================
// Errors
// =====================================================================

/// Migration failure. None of these leave the source modified.
#[derive(Debug)]
pub enum MigrationError {
    Io {
        context: String,
        error: io::Error,
    },
    /// The state marker exists but cannot be parsed.
    StateUnreadable(String),
    /// The state marker was written by a newer Ferrite.
    UnsupportedState {
        found: u32,
    },
    /// The chosen source is not Ferrite data.
    NotFerriteData {
        path: PathBuf,
        reason: String,
    },
    /// The chosen source is unusable (e.g. inside the destination).
    InvalidSource(String),
    /// The destination already exists; it is never overwritten.
    DestinationExists(PathBuf),
    /// The staged copy did not match the source; staging was removed.
    VerificationFailed(String),
    /// Some source entries could not be copied (not links: special files, unreadable
    /// files or folders). Nothing was committed; staged progress is kept.
    UncopyableFiles(Vec<UncopyableFile>),
    /// The user cancelled; staged progress is kept for resumption.
    Cancelled,
    /// A rolled-back legacy location is no longer usable.
    LegacyRootUnavailable {
        path: PathBuf,
        reason: String,
    },
    /// Rollback requested but no completed migration is recorded.
    NothingToRollBack,
    /// The user previously rolled back; migration will not run automatically.
    RolledBack,
    /// The background worker exited without reporting a result.
    WorkerStopped,
}

impl fmt::Display for MigrationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { context, error } => write!(f, "{context}: {error}"),
            Self::StateUnreadable(detail) => {
                write!(f, "migration state file is unreadable: {detail}")
            }
            Self::UnsupportedState { found } => write!(
                f,
                "migration state uses schema_version {found}, newer than this build supports \
                 ({STATE_SCHEMA_VERSION}); refusing to continue"
            ),
            Self::NotFerriteData { path, reason } => {
                write!(f, "{} is not Ferrite data: {reason}", path.display())
            }
            Self::InvalidSource(detail) => write!(f, "invalid migration source: {detail}"),
            Self::DestinationExists(path) => write!(
                f,
                "destination {} already exists and will not be overwritten",
                path.display()
            ),
            Self::VerificationFailed(detail) => write!(
                f,
                "verification of the copied data failed ({detail}); the partial copy was \
                 removed and your original data was not changed"
            ),
            Self::UncopyableFiles(files) => {
                write!(
                    f,
                    "{} item(s) in the old folder could not be copied, so nothing was moved: ",
                    files.len()
                )?;
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
            Self::Cancelled => write!(
                f,
                "migration cancelled; progress was kept and will resume next time"
            ),
            Self::LegacyRootUnavailable { path, reason } => write!(
                f,
                "the old data location {} chosen earlier is unavailable: {reason}",
                path.display()
            ),
            Self::NothingToRollBack => write!(f, "no completed migration to roll back"),
            Self::RolledBack => write!(
                f,
                "storage was rolled back to the old location; migration will not run"
            ),
            Self::WorkerStopped => write!(f, "the migration worker stopped unexpectedly"),
        }
    }
}

impl std::error::Error for MigrationError {}

fn io_ctx(context: impl Into<String>) -> impl FnOnce(io::Error) -> MigrationError {
    let context = context.into();
    move |error| MigrationError::Io { context, error }
}

// =====================================================================
// Tree scanning (never follows links)
// =====================================================================

/// One regular file found by a scan.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ScannedFile {
    len: u64,
    modified: Option<SystemTime>,
}

/// An entry in the old folder that cannot be copied and is not a link.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UncopyableFile {
    /// Path relative to the old `minecraft` folder.
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
struct TreeScan {
    dirs: Vec<PathBuf>,
    files: BTreeMap<PathBuf, ScannedFile>,
    /// Genuine links that are deliberately not traversed or copied.
    links: Vec<PathBuf>,
    /// Entries that are not links but cannot be copied; these block the commit.
    blocked: Vec<UncopyableFile>,
    total_bytes: u64,
}

impl TreeScan {
    fn last_modified(&self) -> Option<SystemTime> {
        self.files.values().filter_map(|file| file.modified).max()
    }

    /// Same relative paths and sizes.
    fn same_content_shape(&self, other: &Self) -> bool {
        self.files.len() == other.files.len()
            && self
                .files
                .iter()
                .zip(other.files.iter())
                .all(|((a, fa), (b, fb))| a == b && fa.len == fb.len)
    }
}

/// Lists `root` without following links. Only a failure to read `root` itself is an
/// error; problems further down are recorded in [`TreeScan::blocked`].
fn scan_tree(root: &Path) -> Result<TreeScan, MigrationError> {
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
                eprintln!("Ferrite migration: not following link {}", path.display());
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
                eprintln!(
                    "Ferrite migration: cannot copy special file {}",
                    path.display()
                );
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

// =====================================================================
// Candidates
// =====================================================================

/// A legacy directory that holds Ferrite data.
#[derive(Debug, Clone, PartialEq)]
pub struct Candidate {
    /// Canonical path of the legacy `minecraft` directory.
    pub path: PathBuf,
    /// Number of valid instances in its manifest.
    pub instance_count: usize,
    /// Human-readable reasons for manifest entries that will be skipped.
    pub skipped_entries: Vec<String>,
    /// Regular files (links and special files excluded).
    pub file_count: u64,
    /// Total bytes of those files.
    pub total_bytes: u64,
    /// Newest file modification time in the tree.
    pub last_modified: Option<SystemTime>,
    /// Genuine links that will not be copied.
    pub skipped_links: Vec<PathBuf>,
    /// Non-link entries that cannot be copied; migrating this candidate will fail
    /// with [`MigrationError::UncopyableFiles`] until they are fixed.
    pub uncopyable: Vec<UncopyableFile>,
}

/// Why a candidate directory was not accepted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rejection {
    pub path: PathBuf,
    pub reason: String,
}

impl fmt::Display for Rejection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.path.display(), self.reason)
    }
}

/// `<base>/minecraft` for each base, canonicalized (non-existent ones dropped) and
/// de-duplicated in order.
pub fn legacy_candidate_dirs(bases: &[Option<&Path>]) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    for base in bases.iter().flatten() {
        if let Ok(canonical) = fs::canonicalize(base.join(STORAGE_DIR_NAME))
            && !out.contains(&canonical)
        {
            out.push(canonical);
        }
    }
    out
}

/// Candidate legacy directories for this process: `<exe dir>/minecraft` and
/// `<cwd>/minecraft`. This is the only place Ferrite reads the working directory, and
/// only to find old data, never to decide where data is stored.
pub fn default_legacy_candidate_dirs() -> Vec<PathBuf> {
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|exe| fs::canonicalize(exe).ok())
        .and_then(|exe| exe.parent().map(Path::to_path_buf));
    let cwd = std::env::current_dir().ok();
    legacy_candidate_dirs(&[exe_dir.as_deref(), cwd.as_deref()])
}

/// Reads and validates `dir/instances.json` without scanning the tree.
fn inspect_manifest(dir: &Path) -> Result<(InstanceManifest, Vec<u8>), String> {
    let metadata = fs::symlink_metadata(dir).map_err(|error| error.to_string())?;
    if !metadata.is_dir() || fsutil::is_link_like(&metadata) {
        return Err("not a plain directory".into());
    }
    let manifest_path = dir.join(INSTANCES_MANIFEST_FILE);
    let metadata = match fs::symlink_metadata(&manifest_path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Err("no instances.json".into());
        }
        Err(error) => return Err(format!("cannot inspect instances.json: {error}")),
    };
    if !metadata.is_file() || fsutil::is_link_like(&metadata) {
        return Err("instances.json is not a regular file".into());
    }
    let bytes = fs::read(&manifest_path).map_err(|error| format!("cannot read: {error}"))?;
    let text = std::str::from_utf8(&bytes).map_err(|_| "instances.json is not UTF-8")?;
    let parsed = manifest::parse_manifest(text).map_err(|error| error.to_string())?;
    let total = parsed.instances.len() + parsed.skipped.len();
    if total > 0 && !parsed.has_ferrite_shaped_entries() {
        return Err("instances.json does not contain Ferrite instance entries".into());
    }
    Ok((parsed, bytes))
}

fn build_candidate(dir: &Path) -> Result<(Candidate, TreeScan, Vec<u8>), Rejection> {
    let reject = |reason: String| Rejection {
        path: dir.to_path_buf(),
        reason,
    };
    let (parsed, manifest_bytes) = inspect_manifest(dir).map_err(reject)?;
    let scan = scan_tree(dir).map_err(|error| reject(error.to_string()))?;
    let skipped_entries = parsed.skipped.iter().map(ToString::to_string).collect();
    Ok((
        Candidate {
            path: dir.to_path_buf(),
            instance_count: parsed.instances.len(),
            skipped_entries,
            file_count: scan.files.len() as u64,
            total_bytes: scan.total_bytes,
            last_modified: scan.last_modified(),
            skipped_links: scan.links.clone(),
            uncopyable: scan.blocked.clone(),
        },
        scan,
        manifest_bytes,
    ))
}

/// Validates one candidate directory and gathers the information the UI shows.
pub fn inspect_candidate(dir: &Path) -> Result<Candidate, Rejection> {
    build_candidate(dir).map(|(candidate, _, _)| candidate)
}

// =====================================================================
// State marker
// =====================================================================

/// Recorded migration phase.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MigrationPhase {
    /// Staging is being populated.
    Copying,
    /// Staging was verified; the commit rename may or may not have happened.
    Verified,
    /// The destination is live.
    Completed,
    /// The last attempt failed verification; staging was removed.
    VerificationFailed,
    /// The user chose to keep using the legacy location.
    RolledBackToLegacy,
}

/// Contents of `migration-state.json`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MigrationState {
    pub schema_version: u32,
    pub phase: MigrationPhase,
    pub source: Option<PathBuf>,
    #[serde(default)]
    pub files_total: u64,
    #[serde(default)]
    pub bytes_total: u64,
    /// Genuine links that were not copied (relative to source).
    #[serde(default)]
    pub skipped_links: Vec<PathBuf>,
    /// Manifest entries reported as invalid during migration.
    #[serde(default)]
    pub skipped_entries: Vec<String>,
    #[serde(default)]
    pub message: Option<String>,
    #[serde(default)]
    pub updated_unix_secs: u64,
}

impl MigrationState {
    fn new(phase: MigrationPhase, source: Option<PathBuf>) -> Self {
        Self {
            schema_version: STATE_SCHEMA_VERSION,
            phase,
            source,
            files_total: 0,
            bytes_total: 0,
            skipped_links: Vec::new(),
            skipped_entries: Vec::new(),
            message: None,
            updated_unix_secs: 0,
        }
    }
}

/// Reads the state marker (`None` when absent).
pub fn read_state(paths: &AppPaths) -> Result<Option<MigrationState>, MigrationError> {
    let path = paths.migration_state_file();
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(io_ctx(format!("read {}", path.display()))(error)),
    };
    let value: serde_json::Value = serde_json::from_str(&text)
        .map_err(|error| MigrationError::StateUnreadable(error.to_string()))?;
    let version = value
        .get("schema_version")
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(|| MigrationError::StateUnreadable("missing schema_version".into()))?;
    if version > u64::from(STATE_SCHEMA_VERSION) {
        return Err(MigrationError::UnsupportedState {
            found: u32::try_from(version).unwrap_or(u32::MAX),
        });
    }
    serde_json::from_value(value)
        .map(Some)
        .map_err(|error| MigrationError::StateUnreadable(error.to_string()))
}

fn write_state(paths: &AppPaths, state: &MigrationState) -> Result<(), MigrationError> {
    let mut state = state.clone();
    state.updated_unix_secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or_default();
    let text = serde_json::to_string_pretty(&state).map_err(|error| MigrationError::Io {
        context: "serialize migration state".into(),
        error: io::Error::other(error),
    })?;
    let path = paths.migration_state_file();
    fsutil::write_atomic(&path, text.as_bytes())
        .map_err(io_ctx(format!("write {}", path.display())))
}

// =====================================================================
// Planning
// =====================================================================

/// What startup must do before the launcher can use its storage.
#[derive(Debug, Clone)]
pub enum StartupPlan {
    /// Use these paths now. `notes` explains anything noteworthy (ignored candidates...).
    Ready { paths: AppPaths, notes: Vec<String> },
    /// Copy `source` into the standard location (possibly resuming).
    NeedsMigration {
        paths: AppPaths,
        source: Candidate,
        resuming: bool,
        notes: Vec<String>,
    },
    /// Two or more valid candidates differ; the user must choose one. Nothing is
    /// preselected and merging is not offered.
    NeedsUserChoice {
        paths: AppPaths,
        candidates: Vec<Candidate>,
        notes: Vec<String>,
    },
}

fn exists_no_follow(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok()
}

/// Decides what startup must do. `candidate_dirs` usually comes from
/// [`default_legacy_candidate_dirs`]; tests pass explicit directories.
pub fn plan(paths: &AppPaths, candidate_dirs: &[PathBuf]) -> Result<StartupPlan, MigrationError> {
    let mut notes = Vec::new();
    let dest = paths.standard_storage_root();
    let staged = paths.migration_staging_dir().join(STORAGE_DIR_NAME);

    if let Some(state) = read_state(paths)? {
        match state.phase {
            MigrationPhase::RolledBackToLegacy => {
                let source = state.source.ok_or_else(|| {
                    MigrationError::StateUnreadable("rollback state without a source".into())
                })?;
                inspect_manifest(&source).map_err(|reason| {
                    MigrationError::LegacyRootUnavailable {
                        path: source.clone(),
                        reason,
                    }
                })?;
                let legacy = paths
                    .with_legacy_storage_root(source)
                    .map_err(|error| MigrationError::StateUnreadable(error.to_string()))?;
                return Ok(StartupPlan::Ready {
                    paths: legacy,
                    notes,
                });
            }
            MigrationPhase::Completed if exists_no_follow(&dest) => {
                return Ok(StartupPlan::Ready {
                    paths: paths.clone(),
                    notes,
                });
            }
            MigrationPhase::Completed => notes.push(format!(
                "A previous migration completed, but {} is missing; checking old locations again.",
                dest.display()
            )),
            MigrationPhase::Copying | MigrationPhase::Verified => {
                let resumable = state
                    .source
                    .as_deref()
                    .and_then(|source| match inspect_candidate(source) {
                        Ok(candidate) => Some(candidate),
                        Err(rejection) => {
                            notes.push(format!(
                                "An interrupted migration cannot resume: {rejection}"
                            ));
                            None
                        }
                    });
                let committed_rename = state.phase == MigrationPhase::Verified
                    && exists_no_follow(&dest)
                    && !exists_no_follow(&staged);
                if let Some(source) = resumable
                    && (committed_rename || !exists_no_follow(&dest))
                {
                    return Ok(StartupPlan::NeedsMigration {
                        paths: paths.clone(),
                        source,
                        resuming: true,
                        notes,
                    });
                }
            }
            MigrationPhase::VerificationFailed => notes.push(format!(
                "The previous migration attempt failed verification{}.",
                state
                    .message
                    .map(|message| format!(": {message}"))
                    .unwrap_or_default()
            )),
        }
    }

    if exists_no_follow(&dest) {
        let ignored: Vec<String> = candidate_dirs
            .iter()
            .filter(|dir| inspect_manifest(dir).is_ok())
            .map(|dir| dir.display().to_string())
            .collect();
        if !ignored.is_empty() {
            let note = format!(
                "{} already contains data, so old data at {} was not migrated.",
                dest.display(),
                ignored.join(", ")
            );
            eprintln!("Ferrite: {note}");
            notes.push(note);
        }
        return Ok(StartupPlan::Ready {
            paths: paths.clone(),
            notes,
        });
    }

    let data_dir = fs::canonicalize(paths.data_dir()).ok();
    let mut valid: Vec<(Candidate, TreeScan, Vec<u8>)> = Vec::new();
    for dir in candidate_dirs {
        if data_dir
            .as_deref()
            .is_some_and(|data| dir.starts_with(data))
        {
            notes.push(format!(
                "Ignored {}: it is inside Ferrite's data directory.",
                dir.display()
            ));
            continue;
        }
        match build_candidate(dir) {
            Ok(found) => valid.push(found),
            // A missing instances.json is the normal "not Ferrite data" case.
            Err(rejection) => eprintln!("Ferrite: not migrating {rejection}"),
        }
    }

    match valid.len() {
        0 => Ok(StartupPlan::Ready {
            paths: paths.clone(),
            notes,
        }),
        1 => Ok(StartupPlan::NeedsMigration {
            paths: paths.clone(),
            source: valid.remove(0).0,
            resuming: false,
            notes,
        }),
        _ => {
            let (first, rest) = valid.split_first().expect("at least two candidates");
            let identical = rest
                .iter()
                .all(|(_, scan, bytes)| bytes == &first.2 && scan.same_content_shape(&first.1));
            if identical {
                notes.push(format!(
                    "Found identical copies of old data; migrating {}.",
                    first.0.path.display()
                ));
                Ok(StartupPlan::NeedsMigration {
                    paths: paths.clone(),
                    source: valid.remove(0).0,
                    resuming: false,
                    notes,
                })
            } else {
                Ok(StartupPlan::NeedsUserChoice {
                    paths: paths.clone(),
                    candidates: valid
                        .into_iter()
                        .map(|(candidate, _, _)| candidate)
                        .collect(),
                    notes,
                })
            }
        }
    }
}

// =====================================================================
// Progress and cancellation
// =====================================================================

/// Coarse step for UI labels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MigrationStep {
    #[default]
    Starting,
    Scanning,
    Copying,
    Verifying,
    Finalizing,
    Done,
}

/// Snapshot of migration progress.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MigrationProgress {
    pub step: MigrationStep,
    pub files_done: u64,
    pub files_total: u64,
    pub bytes_done: u64,
    pub bytes_total: u64,
}

impl MigrationProgress {
    /// Fraction of bytes copied (0.0..=1.0).
    pub fn fraction(&self) -> f32 {
        match self.step {
            MigrationStep::Done | MigrationStep::Finalizing | MigrationStep::Verifying => 1.0,
            _ if self.bytes_total == 0 => 0.0,
            _ => (self.bytes_done as f64 / self.bytes_total as f64).clamp(0.0, 1.0) as f32,
        }
    }
}

/// Shared progress + cancellation handle between the worker and the UI.
#[derive(Debug, Clone, Default)]
pub struct MigrationControl {
    progress: Arc<Mutex<MigrationProgress>>,
    cancel: Arc<AtomicBool>,
}

impl MigrationControl {
    /// Latest progress snapshot.
    pub fn snapshot(&self) -> MigrationProgress {
        self.progress
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .clone()
    }
    /// Requests cancellation at the next file/chunk boundary.
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
    /// Whether cancellation was requested.
    pub fn is_cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }
    fn update(&self, change: impl FnOnce(&mut MigrationProgress)) {
        change(
            &mut self
                .progress
                .lock()
                .unwrap_or_else(|poison| poison.into_inner()),
        );
    }
    fn check_cancel(&self) -> Result<(), MigrationError> {
        if self.is_cancelled() {
            Err(MigrationError::Cancelled)
        } else {
            Ok(())
        }
    }
}

/// A migration running on a background thread.
pub struct MigrationTask {
    control: MigrationControl,
    result: Receiver<Result<MigrationReport, MigrationError>>,
}

impl MigrationTask {
    /// Starts [`run_migration`] on a worker thread.
    pub fn spawn(paths: AppPaths, source: PathBuf) -> io::Result<Self> {
        let control = MigrationControl::default();
        let worker_control = control.clone();
        let (sender, result) = mpsc::channel();
        std::thread::Builder::new()
            .name("storage-migration".into())
            .spawn(move || {
                let _ = sender.send(run_migration(&paths, &source, &worker_control));
            })?;
        Ok(Self { control, result })
    }
    /// Progress/cancellation handle.
    pub fn control(&self) -> &MigrationControl {
        &self.control
    }
    /// Non-blocking: the terminal result once the worker has finished.
    pub fn try_finish(&self) -> Option<Result<MigrationReport, MigrationError>> {
        match self.result.try_recv() {
            Ok(result) => Some(result),
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => Some(Err(MigrationError::WorkerStopped)),
        }
    }
}

// =====================================================================
// Running
// =====================================================================

/// Outcome of a successful (or already-complete) migration.
#[derive(Debug, Clone, PartialEq)]
pub struct MigrationReport {
    pub source: PathBuf,
    pub destination: PathBuf,
    pub files_copied: u64,
    pub bytes_copied: u64,
    pub skipped_links: Vec<PathBuf>,
    pub skipped_entries: Vec<String>,
    /// `true` when nothing had to be done.
    pub already_complete: bool,
}

type FileHook<'a> = &'a dyn Fn(u64) -> io::Result<()>;
type PathHook<'a> = &'a dyn Fn(&Path) -> io::Result<()>;
type StepHook<'a> = &'a dyn Fn() -> io::Result<()>;

/// Test hooks for simulating crashes and corruption at precise points.
#[derive(Default)]
struct Hooks<'a> {
    after_file: Option<FileHook<'a>>,
    before_verify: Option<PathHook<'a>>,
    /// Called with each source file right before it is opened; an error simulates
    /// an unreadable source file.
    before_open: Option<PathHook<'a>>,
    before_rename: Option<StepHook<'a>>,
    after_rename: Option<StepHook<'a>>,
}

fn interrupted(error: io::Error) -> MigrationError {
    MigrationError::Io {
        context: "migration interrupted".into(),
        error,
    }
}

/// Copies `source` (a validated legacy `minecraft` dir) into the standard storage
/// root. Safe to call repeatedly; see the module docs for every guarantee.
pub fn run_migration(
    paths: &AppPaths,
    source: &Path,
    control: &MigrationControl,
) -> Result<MigrationReport, MigrationError> {
    run_with_hooks(paths, source, control, &Hooks::default())
}

fn run_with_hooks(
    paths: &AppPaths,
    source: &Path,
    control: &MigrationControl,
    hooks: &Hooks<'_>,
) -> Result<MigrationReport, MigrationError> {
    let dest = paths.standard_storage_root();
    let staging_root = paths.migration_staging_dir();
    let staged = staging_root.join(STORAGE_DIR_NAME);
    let source = fs::canonicalize(source).map_err(io_ctx(format!(
        "locate migration source {}",
        source.display()
    )))?;
    let data_dir = paths.data_dir();
    if let Ok(canonical_data) = fs::canonicalize(data_dir)
        && (source.starts_with(&canonical_data) || canonical_data.starts_with(&source))
    {
        return Err(MigrationError::InvalidSource(format!(
            "{} overlaps Ferrite's data directory {}",
            source.display(),
            canonical_data.display()
        )));
    }

    let state = read_state(paths)?;
    let mut report = MigrationReport {
        source: source.clone(),
        destination: dest.clone(),
        files_copied: 0,
        bytes_copied: 0,
        skipped_links: Vec::new(),
        skipped_entries: Vec::new(),
        already_complete: false,
    };
    let phase = state.as_ref().map(|state| state.phase);
    match phase {
        Some(MigrationPhase::Completed) if exists_no_follow(&dest) => {
            report.already_complete = true;
            control.update(|progress| progress.step = MigrationStep::Done);
            return Ok(report);
        }
        Some(MigrationPhase::RolledBackToLegacy) => return Err(MigrationError::RolledBack),
        _ => {}
    }
    let same_source = state
        .as_ref()
        .is_some_and(|state| state.source.as_deref() == Some(source.as_path()));
    let resuming_copy = same_source
        && matches!(
            phase,
            Some(MigrationPhase::Copying | MigrationPhase::Verified)
        );

    // Interrupted after the commit rename but before the Completed marker.
    if resuming_copy
        && phase == Some(MigrationPhase::Verified)
        && exists_no_follow(&dest)
        && !exists_no_follow(&staged)
    {
        let previous = state.expect("checked above");
        report.files_copied = previous.files_total;
        report.bytes_copied = previous.bytes_total;
        report.skipped_links = previous.skipped_links.clone();
        report.skipped_entries = previous.skipped_entries.clone();
        return finalize(paths, previous, control, &staging_root, report);
    }
    if exists_no_follow(&dest) {
        return Err(MigrationError::DestinationExists(dest));
    }

    // Re-validate the source (manifest + directory names) at migration time.
    let (parsed, manifest_bytes) =
        inspect_manifest(&source).map_err(|reason| MigrationError::NotFerriteData {
            path: source.clone(),
            reason,
        })?;
    report.skipped_entries = parsed.skipped.iter().map(ToString::to_string).collect();
    for entry in &report.skipped_entries {
        eprintln!("Ferrite migration: invalid manifest entry will not be loaded: {entry}");
    }

    if !resuming_copy && exists_no_follow(&staging_root) {
        eprintln!(
            "Ferrite migration: discarding stale staging directory {}",
            staging_root.display()
        );
        fs::remove_dir_all(&staging_root)
            .map_err(io_ctx(format!("remove stale {}", staging_root.display())))?;
    }

    control.update(|progress| progress.step = MigrationStep::Scanning);
    let scan = scan_tree(&source)?;
    if !scan.blocked.is_empty() {
        // Fail before copying anything: these would be missing from the copy.
        return Err(MigrationError::UncopyableFiles(scan.blocked));
    }
    report.skipped_links = scan.links.clone();
    let mut state = MigrationState::new(MigrationPhase::Copying, Some(source.clone()));
    state.files_total = scan.files.len() as u64;
    state.bytes_total = scan.total_bytes;
    state.skipped_links = scan.links.clone();
    state.skipped_entries = report.skipped_entries.clone();
    fs::create_dir_all(data_dir).map_err(io_ctx(format!("create {}", data_dir.display())))?;
    write_state(paths, &state)?;

    control.update(|progress| {
        progress.step = MigrationStep::Copying;
        progress.files_total = state.files_total;
        progress.bytes_total = state.bytes_total;
        progress.files_done = 0;
        progress.bytes_done = 0;
    });
    copy_tree(&source, &staged, &scan, control, hooks)?;
    report.files_copied = state.files_total;
    report.bytes_copied = state.bytes_total;

    if let Some(hook) = hooks.before_verify {
        hook(&staged).map_err(interrupted)?;
    }
    control.update(|progress| progress.step = MigrationStep::Verifying);
    if let Err(detail) = verify(&staged, &scan, &manifest_bytes) {
        eprintln!("Ferrite migration: verification failed: {detail}");
        // Roll back: remove only our staging directory.
        fs::remove_dir_all(&staging_root)
            .map_err(io_ctx(format!("remove {}", staging_root.display())))?;
        let mut failed = state.clone();
        failed.phase = MigrationPhase::VerificationFailed;
        failed.message = Some(detail.clone());
        write_state(paths, &failed)?;
        return Err(MigrationError::VerificationFailed(detail));
    }
    state.phase = MigrationPhase::Verified;
    write_state(paths, &state)?;

    if let Some(hook) = hooks.before_rename {
        hook().map_err(interrupted)?;
    }
    control.update(|progress| progress.step = MigrationStep::Finalizing);
    if exists_no_follow(&dest) {
        return Err(MigrationError::DestinationExists(dest));
    }
    fs::rename(&staged, &dest).map_err(io_ctx(format!(
        "move {} to {}",
        staged.display(),
        dest.display()
    )))?;
    fsutil::sync_dir(data_dir).map_err(io_ctx(format!("sync {}", data_dir.display())))?;
    if let Some(hook) = hooks.after_rename {
        hook().map_err(interrupted)?;
    }
    finalize(paths, state, control, &staging_root, report)
}

fn finalize(
    paths: &AppPaths,
    mut state: MigrationState,
    control: &MigrationControl,
    staging_root: &Path,
    report: MigrationReport,
) -> Result<MigrationReport, MigrationError> {
    state.phase = MigrationPhase::Completed;
    state.message = None;
    write_state(paths, &state)?;
    // Only removes the now-empty staging parent; never anything with content.
    let _ = fs::remove_dir(staging_root);
    control.update(|progress| {
        progress.step = MigrationStep::Done;
        progress.files_done = progress.files_total;
        progress.bytes_done = progress.bytes_total;
    });
    eprintln!(
        "Ferrite migration: copied {} files ({} bytes) from {} to {}; the old location was left unchanged.",
        report.files_copied,
        report.bytes_copied,
        report.source.display(),
        report.destination.display()
    );
    Ok(report)
}

fn partial_name(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(PARTIAL_SUFFIX);
    PathBuf::from(name)
}

/// Removes leftover `.ferrite-partial` files inside staging (only ever ours).
fn clear_partials(staged: &Path) -> Result<(), MigrationError> {
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

fn copy_tree(
    source: &Path,
    staged: &Path,
    scan: &TreeScan,
    control: &MigrationControl,
    hooks: &Hooks<'_>,
) -> Result<(), MigrationError> {
    clear_partials(staged)?;
    fs::create_dir_all(staged).map_err(io_ctx(format!("create {}", staged.display())))?;
    for dir in &scan.dirs {
        let target = staged.join(dir);
        fs::create_dir_all(&target).map_err(io_ctx(format!("create {}", target.display())))?;
    }
    let mut buffer = vec![0_u8; COPY_CHUNK];
    let mut files_done = 0_u64;
    let mut uncopyable = Vec::new();
    for (relative, info) in &scan.files {
        control.check_cancel()?;
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
            control.update(|progress| progress.bytes_done += info.len);
        } else {
            match copy_one(&from, &to, info, control, &mut buffer, hooks) {
                Ok(()) => {}
                // A source-side problem: record it, keep copying the rest so the user
                // sees the complete list, and refuse to commit afterwards.
                Err(CopyError::Source(reason)) => {
                    eprintln!(
                        "Ferrite migration: cannot copy {}: {reason}",
                        from.display()
                    );
                    uncopyable.push(UncopyableFile {
                        path: relative.clone(),
                        reason,
                    });
                }
                Err(CopyError::Fatal(error)) => return Err(error),
            }
        }
        files_done += 1;
        control.update(|progress| progress.files_done = files_done);
        if let Some(hook) = hooks.after_file {
            hook(files_done).map_err(interrupted)?;
        }
    }
    if !uncopyable.is_empty() {
        return Err(MigrationError::UncopyableFiles(uncopyable));
    }
    fsutil::sync_dir(staged).map_err(io_ctx(format!("sync {}", staged.display())))?;
    Ok(())
}

/// Why one file could not be copied.
enum CopyError {
    /// The source could not be read (recorded; blocks the commit).
    Source(String),
    /// Destination-side failure or cancellation (aborts immediately).
    Fatal(MigrationError),
}

impl From<MigrationError> for CopyError {
    fn from(error: MigrationError) -> Self {
        Self::Fatal(error)
    }
}

fn copy_one(
    from: &Path,
    to: &Path,
    info: &ScannedFile,
    control: &MigrationControl,
    buffer: &mut [u8],
    hooks: &Hooks<'_>,
) -> Result<(), CopyError> {
    // Re-check right before opening so a file swapped for a link after the scan is
    // not followed (a narrow race remains between this check and `open`).
    let metadata = fs::symlink_metadata(from)
        .map_err(|error| CopyError::Source(format!("could not be inspected: {error}")))?;
    if !metadata.is_file() || fsutil::is_link_like(&metadata) {
        return Err(CopyError::Source(
            "changed from a regular file while Ferrite was copying".into(),
        ));
    }
    if let Some(hook) = hooks.before_open {
        hook(from).map_err(|error| CopyError::Source(format!("could not be opened: {error}")))?;
    }
    let mut input = File::open(from)
        .map_err(|error| CopyError::Source(format!("could not be opened: {error}")))?;
    let partial = partial_name(to);
    let mut output =
        File::create(&partial).map_err(io_ctx(format!("create {}", partial.display())))?;
    let copied = (|| -> Result<(), CopyError> {
        loop {
            control.check_cancel()?;
            let read = input
                .read(buffer)
                .map_err(|error| CopyError::Source(format!("could not be read: {error}")))?;
            if read == 0 {
                break;
            }
            output
                .write_all(&buffer[..read])
                .map_err(io_ctx(format!("write {}", partial.display())))?;
            control.update(|progress| progress.bytes_done += read as u64);
        }
        Ok(())
    })();
    if let Err(error) = copied {
        drop(output);
        let _ = fs::remove_file(&partial);
        return Err(error);
    }
    output
        .sync_all()
        .map_err(io_ctx(format!("sync {}", partial.display())))?;
    if let Some(modified) = info.modified {
        let _ = output.set_modified(modified);
    }
    drop(output);
    fs::rename(&partial, to).map_err(io_ctx(format!("finish {}", to.display())))?;
    Ok(())
}

/// Checks the staged tree against the source scan: identical relative file set,
/// identical sizes, no extra entries, and an identical manifest.
fn verify(staged: &Path, scan: &TreeScan, manifest_bytes: &[u8]) -> Result<(), String> {
    let copy = scan_tree(staged).map_err(|error| error.to_string())?;
    if let Some(link) = copy.links.first() {
        return Err(format!("unexpected link in staging: {}", link.display()));
    }
    if let Some(blocked) = copy.blocked.first() {
        return Err(format!("unreadable entry in staging: {blocked}"));
    }
    if copy.files.len() != scan.files.len() {
        return Err(format!(
            "file count mismatch: expected {}, found {}",
            scan.files.len(),
            copy.files.len()
        ));
    }
    for (relative, expected) in &scan.files {
        match copy.files.get(relative) {
            Some(found) if found.len == expected.len => {}
            Some(found) => {
                return Err(format!(
                    "size mismatch for {}: expected {}, found {}",
                    relative.display(),
                    expected.len,
                    found.len
                ));
            }
            None => return Err(format!("missing {}", relative.display())),
        }
    }
    if copy.total_bytes != scan.total_bytes {
        return Err("total size mismatch".into());
    }
    let staged_manifest = fs::read(staged.join(INSTANCES_MANIFEST_FILE))
        .map_err(|error| format!("cannot read staged manifest: {error}"))?;
    if staged_manifest != manifest_bytes {
        return Err("staged instances.json differs from the source".into());
    }
    Ok(())
}

// =====================================================================
// Rollback
// =====================================================================

/// After a completed migration, switch back to using the old location.
///
/// Only the state marker is rewritten; neither the old nor the new tree is modified.
/// Returns paths whose storage root is the old location. Changes made in either
/// location afterwards are not synchronized with the other. Not wired to any UI yet.
pub fn rollback_to_legacy(paths: &AppPaths) -> Result<AppPaths, MigrationError> {
    let mut state = read_state(paths)?.ok_or(MigrationError::NothingToRollBack)?;
    if state.phase != MigrationPhase::Completed {
        return Err(MigrationError::NothingToRollBack);
    }
    let source = state
        .source
        .clone()
        .ok_or(MigrationError::NothingToRollBack)?;
    inspect_manifest(&source).map_err(|reason| MigrationError::LegacyRootUnavailable {
        path: source.clone(),
        reason,
    })?;
    let legacy = paths
        .with_legacy_storage_root(source)
        .map_err(|error| MigrationError::StateUnreadable(error.to_string()))?;
    state.phase = MigrationPhase::RolledBackToLegacy;
    write_state(paths, &state)?;
    Ok(legacy)
}

#[cfg(test)]
mod tests;
