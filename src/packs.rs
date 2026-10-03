//! Secure, blocking import, inspection, and export of launcher instance packs.
//!
//! Format detection is manifest-first: Ferrite, Modrinth, CurseForge, and Prism packs
//! are recognized by their documented root metadata, while an otherwise valid ZIP is
//! treated as a generic game directory. Known `.ferritepack` and `.mrpack` extensions
//! do not fall back to generic ZIP when their required manifest is absent, and Lunar
//! `.lcpack` is rejected because there is no supported public interchange schema.
//!
//! Import scans the whole central directory before parsing manifests or writing files.
//! The scan enforces configured entry and expanded-size limits, relative portable names,
//! unique case-folded paths, and regular-file/directory Unix modes. Extraction happens in
//! a new sibling staging directory and is renamed into place only after all archive reads,
//! downloads, hash/size checks, and overlays succeed. Ordinary errors remove that staging
//! tree, so they do not leave a partial destination; a process crash can leave the hidden
//! temporary tree. The final rename is not a transaction with other processes, and callers
//! should not permit hostile concurrent mutation of the archive, destination parent, or
//! instance tree.
//!
//! Modrinth imports download required client files (and optional files when requested),
//! verify declared size plus SHA-512 and SHA-1, then apply `overrides/` followed by
//! `client-overrides/`. CurseForge imports resolve required project files through its API,
//! skip optional entries, verify exact size and SHA-1 when supplied, then apply the named
//! overrides directory. Network access is HTTPS-only and rejects credentialed URLs,
//! localhost names, and literal private/local IP addresses; it does not resolve hostnames
//! itself to detect DNS rebinding. There are no retries, cancellation hooks, or resumable
//! downloads.
//!
//! Exports are self-contained override archives: Modrinth and CurseForge manifests contain
//! empty downloadable-file lists, with selected instance files embedded under their
//! overrides roots. Volatile/user-specific paths are excluded, worlds are opt-in, and
//! symlinks or special files abort export. The completed ZIP is published with a no-clobber
//! hard link, so the output filesystem must support hard links. Loader pins are metadata
//! only in every format; installing the requested loader remains the caller's responsibility.

use crate::instances::InstanceProfile;
use crate::loaders::ModLoader;
use ferrite_launcher::core::paths::AppPaths;
use serde::{Deserialize, Serialize};
use sha1::Sha1;
use sha2::{Digest, Sha512};
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::net::IpAddr;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use zip::write::FileOptions;
use zip::{CompressionMethod, ZipArchive, ZipWriter};

/// Pack layouts recognized by this module.
///
/// Presence in this enum does not imply bidirectional support: [`PackFormat::Lunar`] is
/// exposed for UI labeling/extension handling but inspection, import, and export reject it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PackFormat {
    Ferrite,
    Modrinth,
    GenericZip,
    Prism,
    CurseForge,
    Lunar,
}

impl PackFormat {
    /// Stable list used by format pickers; unsupported operations can still return
    /// [`PackError::Unsupported`] for a listed format.
    pub const ALL: [PackFormat; 6] = [
        PackFormat::Ferrite,
        PackFormat::Modrinth,
        PackFormat::GenericZip,
        PackFormat::Prism,
        PackFormat::CurseForge,
        PackFormat::Lunar,
    ];

    /// Human-readable label intended for launcher UI.
    pub fn label(self) -> &'static str {
        match self {
            Self::Ferrite => "Ferrite Pack",
            Self::Modrinth => "Modrinth",
            Self::GenericZip => "Generic ZIP",
            Self::Prism => "Prism / MultiMC",
            Self::CurseForge => "CurseForge",
            Self::Lunar => "Lunar Client",
        }
    }

    /// Conventional extension without a leading dot.
    pub fn extension(self) -> &'static str {
        match self {
            Self::Ferrite => "ferritepack",
            Self::Modrinth => "mrpack",
            Self::Lunar => "lcpack",
            Self::GenericZip | Self::Prism | Self::CurseForge => "zip",
        }
    }
}

/// Minecraft and loader metadata declared by a pack or supplied for a generic ZIP.
///
/// A missing `loader_version` is valid for vanilla and for inspection, but exports whose
/// manifest requires an exact non-vanilla loader pin reject it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackTarget {
    pub minecraft_version: String,
    pub loader: ModLoader,
    pub loader_version: Option<String>,
}

/// Metadata obtained without extracting or downloading pack payloads.
///
/// `warnings` describe format-level caveats discovered during inspection. Import starts
/// with these warnings and may append per-file skips or missing-hash notices.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackInfo {
    pub format: PackFormat,
    pub name: String,
    pub version: Option<String>,
    pub summary: Option<String>,
    pub target: Option<PackTarget>,
    pub warnings: Vec<String>,
}

/// Resource limits applied to both archive expansion and downloaded pack files.
///
/// Declared ZIP sizes are checked during the initial scan and actual copied byte counts
/// are checked again during extraction. The total budget is shared by extracted entries
/// and network downloads within an import.
#[derive(Debug, Clone, Copy)]
pub struct ArchiveLimits {
    pub max_entries: usize,
    pub max_file_bytes: u64,
    pub max_total_bytes: u64,
}

impl Default for ArchiveLimits {
    fn default() -> Self {
        Self {
            max_entries: 100_000,
            max_file_bytes: 2 * 1024 * 1024 * 1024,
            // Large world backups can legitimately expand far beyond their ZIP size.
            max_total_bytes: 32 * 1024 * 1024 * 1024,
        }
    }
}

/// Policy and resource controls for [`import`].
///
/// Generic ZIPs have no trustworthy target metadata, so `generic_target` is mandatory for
/// that fallback format. Optional Modrinth files are opt-in; optional CurseForge files are
/// always skipped by this implementation. A CurseForge API key is required only when its
/// manifest contains project files to resolve.
#[derive(Debug, Clone)]
pub struct ImportOptions {
    pub generic_target: Option<PackTarget>,
    pub include_optional_modrinth_files: bool,
    pub curseforge_api_key: Option<String>,
    pub limits: ArchiveLimits,
}

impl Default for ImportOptions {
    fn default() -> Self {
        Self {
            generic_target: None,
            include_optional_modrinth_files: false,
            curseforge_api_key: None,
            limits: ArchiveLimits::default(),
        }
    }
}

/// Outcome of a successful import.
///
/// Counts include files downloaded or extracted, including an overlay each time it is
/// written. They do not count directories or manifest entries that are only inspected.
#[derive(Debug, Clone)]
pub struct ImportReport {
    pub info: PackInfo,
    pub files_written: u64,
    pub bytes_written: u64,
    pub warnings: Vec<String>,
}

/// Metadata and file-selection policy for [`export`].
///
/// `include_worlds` controls the top-level `saves/` tree; volatile launcher/game paths are
/// excluded regardless. Modrinth and CurseForge exports default a missing pack version to
/// `1.0.0` in their manifests.
#[derive(Debug, Clone)]
pub struct ExportOptions {
    pub format: PackFormat,
    pub name: String,
    pub version: Option<String>,
    pub summary: Option<String>,
    /// Exact loader version for formats whose manifests require one.
    pub loader_version: Option<String>,
    pub include_worlds: bool,
}

/// Failures are classified so callers can distinguish malformed input, security policy,
/// configured resource limits, unsupported formats, and recoverable setup requirements.
/// I/O, network, ZIP, and JSON variants retain their typed errors for display or direct
/// pattern matching. This type does not override [`std::error::Error::source`], so those
/// values are not exposed through Rust's standard error-source chain.
#[derive(Debug)]
pub enum PackError {
    Io(io::Error),
    Zip(zip::result::ZipError),
    Json(serde_json::Error),
    Network(reqwest::Error),
    Invalid(String),
    Security(String),
    Limit(String),
    Unsupported(String),
    AlreadyExists(PathBuf),
    MissingApiKey(String),
}

impl fmt::Display for PackError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "pack filesystem error: {error}"),
            Self::Zip(error) => write!(f, "invalid ZIP archive: {error}"),
            Self::Json(error) => write!(f, "invalid pack manifest JSON: {error}"),
            Self::Network(error) => write!(f, "pack download failed: {error}"),
            Self::Invalid(message) => write!(f, "invalid pack: {message}"),
            Self::Security(message) => write!(f, "unsafe pack: {message}"),
            Self::Limit(message) => write!(f, "pack exceeds configured limits: {message}"),
            Self::Unsupported(message) => write!(f, "unsupported pack: {message}"),
            Self::AlreadyExists(path) => {
                write!(f, "destination already exists: {}", path.display())
            }
            Self::MissingApiKey(message) => write!(f, "CurseForge API key required: {message}"),
        }
    }
}

impl std::error::Error for PackError {}
impl From<io::Error> for PackError {
    fn from(value: io::Error) -> Self {
        Self::Io(value)
    }
}
impl From<zip::result::ZipError> for PackError {
    fn from(value: zip::result::ZipError) -> Self {
        Self::Zip(value)
    }
}
impl From<serde_json::Error> for PackError {
    fn from(value: serde_json::Error) -> Self {
        Self::Json(value)
    }
}
impl From<reqwest::Error> for PackError {
    fn from(value: reqwest::Error) -> Self {
        Self::Network(value)
    }
}

type Result<T> = std::result::Result<T, PackError>;

const MAX_METADATA_BYTES: u64 = 16 * 1024 * 1024;

#[derive(Debug, Clone)]
struct EntryMeta {
    index: usize,
    name: String,
    is_dir: bool,
    size: u64,
}

#[derive(Debug)]
struct ArchiveCatalog {
    entries: Vec<EntryMeta>,
    names: HashMap<String, usize>,
}

#[derive(Default)]
struct WriteStats {
    files: u64,
    bytes: u64,
}

// RAII cleanup arms temporary paths immediately after creation. Successful publication
// disarms the guard; every ordinary early return removes the partial file/tree best-effort.
struct CleanupPath {
    path: PathBuf,
    directory: bool,
    armed: bool,
}

impl CleanupPath {
    fn new(path: PathBuf, directory: bool) -> Self {
        Self {
            path,
            directory,
            armed: true,
        }
    }
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for CleanupPath {
    fn drop(&mut self) {
        if self.armed {
            if self.directory {
                let _ = fs::remove_dir_all(&self.path);
            } else {
                let _ = fs::remove_file(&self.path);
            }
        }
    }
}

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

// A sibling temporary lives on the destination filesystem, allowing final rename/import
// or hard-link/export publication without crossing filesystem boundaries. The generated
// name is collision-resistant but creation still uses `create_new`/`create_dir` as the
// actual exclusivity check.
fn temporary_sibling(path: &Path, kind: &str) -> Result<PathBuf> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("pack");
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let counter = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    Ok(parent.join(format!(
        ".{name}.{kind}.{}.{}.tmp",
        std::process::id(),
        stamp + counter as u128
    )))
}

fn lunar_error() -> PackError {
    PackError::Unsupported(
        "Lunar .lcpack has no supported public interchange schema. Export the instance as a Modrinth or CurseForge pack from Lunar, then import that file instead.".to_owned(),
    )
}

/// Validate an archive and return its format/target metadata without extracting payloads.
///
/// `impl AsRef<Path>` accepts owned and borrowed path-like values. Inspection uses default
/// limits, performs no network requests, and can still fail for unsafe payload entry names
/// because the entire archive is catalogued before its manifest is trusted.
pub fn inspect(path: impl AsRef<Path>) -> Result<PackInfo> {
    inspect_with_limits(path.as_ref(), ArchiveLimits::default())
}

/// Import a pack into a destination that must not already exist.
///
/// Both paths accept owned or borrowed path-like values. `progress` is an `FnMut` because
/// callers commonly update captured UI/task state; it is invoked synchronously on this
/// blocking call's thread and messages are status snapshots, not a stable machine-readable
/// protocol. The closure may be called before an eventual error.
///
/// All writes occur in a sibling staging tree. On success that tree is renamed to the
/// destination; on an ordinary error it is removed. The destination parent must already
/// exist, and no attempt is made to install the loader recorded in [`PackInfo`].
pub fn import(
    path: impl AsRef<Path>,
    destination: impl AsRef<Path>,
    options: &ImportOptions,
    mut progress: impl FnMut(&str),
) -> Result<ImportReport> {
    let path = path.as_ref();
    let destination = destination.as_ref();
    if destination.exists() {
        return Err(PackError::AlreadyExists(destination.to_owned()));
    }
    let parent = destination.parent().unwrap_or_else(|| Path::new("."));
    if !parent.is_dir() {
        return Err(PackError::Invalid(format!(
            "destination parent does not exist: {}",
            parent.display()
        )));
    }

    progress("Inspecting and validating archive");
    let catalog = scan_archive(path, options.limits)?;
    let mut info = inspect_catalog(path, &catalog)?;
    if info.format == PackFormat::GenericZip {
        info.target = options.generic_target.clone();
        if info.target.is_none() {
            return Err(PackError::Invalid(
                "generic ZIP imports require ImportOptions::generic_target".to_owned(),
            ));
        }
    }

    let staging = temporary_sibling(destination, "import")?;
    fs::create_dir(&staging)?;
    let mut cleanup = CleanupPath::new(staging.clone(), true);
    let mut stats = WriteStats::default();
    let mut warnings = info.warnings.clone();

    // Keep format-specific extraction and final publication inside one fallible boundary;
    // the outer cleanup guard remains armed for every `?` and explicit error in this block.
    let result = (|| {
        match info.format {
            PackFormat::Ferrite => {
                progress("Extracting Ferrite overrides");
                extract_prefix(
                    path,
                    &catalog,
                    "overrides/",
                    &staging,
                    options.limits,
                    &mut stats,
                )?;
            }
            PackFormat::GenericZip => {
                progress("Extracting generic game directory");
                extract_generic(path, &catalog, &staging, options.limits, &mut stats)?;
            }
            PackFormat::Prism => {
                progress("Extracting Prism game directory");
                let modern = catalog
                    .entries
                    .iter()
                    .any(|entry| entry.name.starts_with("minecraft/"));
                let legacy = catalog
                    .entries
                    .iter()
                    .any(|entry| entry.name.starts_with(".minecraft/"));
                if modern && legacy {
                    return Err(PackError::Invalid(
                        "Prism archive contains both minecraft/ and .minecraft/ game roots"
                            .to_owned(),
                    ));
                }
                let prefix = if modern { "minecraft/" } else { ".minecraft/" };
                extract_prefix(path, &catalog, prefix, &staging, options.limits, &mut stats)?;
            }
            PackFormat::Modrinth => {
                progress("Downloading Modrinth files");
                import_modrinth(
                    path,
                    &catalog,
                    &staging,
                    options,
                    &mut stats,
                    &mut warnings,
                    &mut progress,
                )?;
            }
            PackFormat::CurseForge => {
                progress("Downloading CurseForge files");
                import_curseforge(
                    path,
                    &catalog,
                    &staging,
                    options,
                    &mut stats,
                    &mut warnings,
                    &mut progress,
                )?;
            }
            PackFormat::Lunar => return Err(lunar_error()),
        }
        // Recheck after potentially long downloads to catch ordinary races. This is not a
        // no-clobber primitive on every platform: callers must still serialize writers to
        // the destination parent.
        if destination.exists() {
            return Err(PackError::AlreadyExists(destination.to_owned()));
        }
        fs::rename(&staging, destination)?;
        cleanup.disarm();
        Ok(())
    })();
    result?;

    progress("Import complete");
    Ok(ImportReport {
        info,
        files_written: stats.files,
        bytes_written: stats.bytes,
        warnings,
    })
}

/// Export an instance to a newly created archive.
///
/// `progress` follows the same synchronous `FnMut` contract as [`import`]. An explicit,
/// non-empty loader version overrides the launcher's installed-version lookup; otherwise
/// that lookup is the only fallback. Formats requiring a non-vanilla loader pin fail if
/// neither source provides one. The output is never intentionally replaced.
pub fn export(
    paths: &AppPaths,
    profile: &InstanceProfile,
    output: impl AsRef<Path>,
    options: &ExportOptions,
    mut progress: impl FnMut(&str),
) -> Result<()> {
    let loader = ModLoader::from_label(&profile.loader).ok_or_else(|| {
        PackError::Invalid(format!(
            "instance has unknown loader label {:?}",
            profile.loader
        ))
    })?;
    let target = PackTarget {
        minecraft_version: profile.version.clone(),
        loader,
        loader_version: options
            .loader_version
            .as_deref()
            .map(str::trim)
            .filter(|version| !version.is_empty())
            .map(str::to_owned)
            .or_else(|| crate::loaders::installed_loader_version(paths, &profile.version, loader)),
    };
    export_directory(
        &profile.game_dir(paths),
        output.as_ref(),
        options,
        &target,
        &mut progress,
    )
}

fn inspect_with_limits(path: &Path, limits: ArchiveLimits) -> Result<PackInfo> {
    let catalog = scan_archive(path, limits)?;
    inspect_catalog(path, &catalog)
}

// Manifest names take precedence over the filename extension so ordinary `.zip` exports
// from third-party launchers are recognized. Conversely, known dedicated extensions are
// treated as promises and fail when their manifest is absent rather than silently becoming
// a generic archive.
fn inspect_catalog(path: &Path, catalog: &ArchiveCatalog) -> Result<PackInfo> {
    let extension = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    if extension == "lcpack" {
        return Err(lunar_error());
    }

    if catalog.names.contains_key("ferritepack.json") {
        return inspect_ferrite(path, catalog);
    }
    if catalog.names.contains_key("modrinth.index.json") {
        return inspect_modrinth(path, catalog);
    }
    if catalog.names.contains_key("manifest.json") {
        if let Ok(manifest) = read_json(path, catalog, "manifest.json") {
            if is_curseforge_manifest(&manifest) {
                return inspect_curseforge_value(manifest);
            }
        }
    }
    if catalog.names.contains_key("mmc-pack.json") || catalog.names.contains_key("instance.cfg") {
        return inspect_prism(path, catalog);
    }

    if extension == "ferritepack" {
        return Err(PackError::Invalid(
            ".ferritepack archive is missing ferritepack.json".to_owned(),
        ));
    }
    if extension == "mrpack" {
        return Err(PackError::Invalid(
            ".mrpack archive is missing modrinth.index.json".to_owned(),
        ));
    }

    Ok(PackInfo {
        format: PackFormat::GenericZip,
        name: path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("Imported pack")
            .to_owned(),
        version: None,
        summary: None,
        target: None,
        warnings: vec![
            "Generic ZIP metadata does not identify a Minecraft version or loader.".to_owned(),
        ],
    })
}

// Catalog once before any extraction. Exact and case-folded duplicate rejection prevents
// archives from producing different results across filesystems; declared uncompressed
// sizes provide an early ZIP-bomb bound, while extraction later verifies actual counts.
fn scan_archive(path: &Path, limits: ArchiveLimits) -> Result<ArchiveCatalog> {
    let file = File::open(path)?;
    let mut archive = ZipArchive::new(file)?;
    if archive.len() > limits.max_entries {
        return Err(PackError::Limit(format!(
            "{} entries exceeds maximum {}",
            archive.len(),
            limits.max_entries
        )));
    }
    let mut entries = Vec::with_capacity(archive.len());
    let mut names = HashMap::new();
    let mut folded = HashSet::new();
    let mut total = 0u64;
    for index in 0..archive.len() {
        let entry = archive.by_index(index)?;
        let raw = entry.name().to_owned();
        let is_dir = entry.is_dir();
        let name = validate_zip_name(&raw, is_dir)?;
        validate_unix_mode(entry.unix_mode(), is_dir, &name)?;
        if names.insert(name.clone(), index).is_some() {
            return Err(PackError::Security(format!("duplicate ZIP entry {name:?}")));
        }
        if !folded.insert(name.to_lowercase()) {
            return Err(PackError::Security(format!(
                "case-folding ZIP path collision at {name:?}"
            )));
        }
        let size = entry.size();
        if size > limits.max_file_bytes {
            return Err(PackError::Limit(format!("entry {name:?} is {size} bytes")));
        }
        total = total
            .checked_add(size)
            .ok_or_else(|| PackError::Limit("uncompressed size overflow".to_owned()))?;
        if total > limits.max_total_bytes {
            return Err(PackError::Limit(format!(
                "total uncompressed size exceeds {} bytes",
                limits.max_total_bytes
            )));
        }

        entries.push(EntryMeta {
            index,
            name: name.clone(),
            is_dir,
            size,
        });
    }
    Ok(ArchiveCatalog { entries, names })
}

// Enforce a cross-platform relative-path subset rather than merely relying on `Path`.
// This blocks traversal/absolute paths and Windows aliases such as device names, drive
// prefixes, alternate-data-stream colons, and trailing dot/space normalization.
fn validate_zip_name(raw: &str, is_dir: bool) -> Result<String> {
    if raw.is_empty() || raw.chars().any(char::is_control) || raw.contains('\\') {
        return Err(PackError::Security(format!("invalid ZIP path {raw:?}")));
    }
    if raw.starts_with('/') || raw.starts_with("//") {
        return Err(PackError::Security(format!("absolute ZIP path {raw:?}")));
    }
    let trimmed = if is_dir {
        raw.trim_end_matches('/')
    } else {
        raw
    };
    if trimmed.is_empty() {
        return Err(PackError::Security("empty ZIP path".to_owned()));
    }
    let first = trimmed.split('/').next().unwrap_or("");
    if first.len() >= 2 && first.as_bytes()[1] == b':' && first.as_bytes()[0].is_ascii_alphabetic()
    {
        return Err(PackError::Security(format!(
            "Windows-prefixed ZIP path {raw:?}"
        )));
    }
    if trimmed.split('/').any(|part| {
        part.is_empty()
            || part == "."
            || part == ".."
            || part.contains(':')
            || part.ends_with('.')
            || part.ends_with(' ')
            || is_windows_reserved_component(part)
    }) {
        return Err(PackError::Security(format!(
            "unsafe or non-portable ZIP path {raw:?}"
        )));
    }
    let candidate = Path::new(trimmed);
    if candidate
        .components()
        .any(|part| !matches!(part, Component::Normal(_)))
    {
        return Err(PackError::Security(format!(
            "non-relative ZIP path {raw:?}"
        )));
    }
    Ok(if is_dir {
        format!("{trimmed}/")
    } else {
        trimmed.to_owned()
    })
}

fn is_windows_reserved_component(component: &str) -> bool {
    let stem = component
        .split('.')
        .next()
        .unwrap_or(component)
        .to_ascii_uppercase();
    matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || stem
            .strip_prefix("COM")
            .or_else(|| stem.strip_prefix("LPT"))
            .is_some_and(|number| {
                matches!(number, "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9")
            })
}

fn validate_unix_mode(mode: Option<u32>, is_dir: bool, name: &str) -> Result<()> {
    if let Some(mode) = mode {
        let kind = mode & 0o170000;
        let expected = if is_dir { 0o040000 } else { 0o100000 };
        if kind != 0 && kind != expected {
            return Err(PackError::Security(format!(
                "ZIP entry {name:?} is a symlink or special file"
            )));
        }
    }
    Ok(())
}

// Metadata is read into memory only after an independent 16 MiB cap. Reopening the archive
// avoids retaining a borrow from `ZipArchive`, and checking the copied length detects a
// changed/truncated entry relative to the catalog.
fn read_entry(path: &Path, catalog: &ArchiveCatalog, name: &str) -> Result<Vec<u8>> {
    let index = *catalog
        .names
        .get(name)
        .ok_or_else(|| PackError::Invalid(format!("missing {name}")))?;
    let meta = &catalog.entries[index];
    if meta.size > MAX_METADATA_BYTES {
        return Err(PackError::Limit(format!(
            "metadata entry {name:?} exceeds {MAX_METADATA_BYTES} bytes"
        )));
    }
    let capacity = usize::try_from(meta.size)
        .map_err(|_| PackError::Limit(format!("{name} is too large for this platform")))?;
    let mut archive = ZipArchive::new(File::open(path)?)?;
    let mut entry = archive.by_index(index)?;
    let mut bytes = Vec::with_capacity(capacity);
    entry.read_to_end(&mut bytes)?;
    if bytes.len() as u64 != meta.size {
        return Err(PackError::Invalid(format!(
            "size changed while reading {name}"
        )));
    }
    Ok(bytes)
}

fn read_json(path: &Path, catalog: &ArchiveCatalog, name: &str) -> Result<serde_json::Value> {
    Ok(serde_json::from_slice(&read_entry(path, catalog, name)?)?)
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct FerriteManifest {
    format_version: u32,
    name: String,
    #[serde(default)]
    version: Option<String>,
    #[serde(default)]
    summary: Option<String>,
    minecraft: FerriteMinecraft,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct FerriteMinecraft {
    version: String,
    loader: String,
    #[serde(default)]
    loader_version: Option<String>,
}

fn inspect_ferrite(path: &Path, catalog: &ArchiveCatalog) -> Result<PackInfo> {
    let manifest: FerriteManifest =
        serde_json::from_slice(&read_entry(path, catalog, "ferritepack.json")?)?;
    if manifest.format_version != 1 {
        return Err(PackError::Unsupported(format!(
            "Ferrite pack format version {} (only v1 is supported)",
            manifest.format_version
        )));
    }
    let loader = parse_loader(&manifest.minecraft.loader)?;
    require_nonempty("pack name", &manifest.name)?;
    require_nonempty("Minecraft version", &manifest.minecraft.version)?;
    Ok(PackInfo {
        format: PackFormat::Ferrite,
        name: manifest.name,
        version: manifest.version,
        summary: manifest.summary,
        target: Some(PackTarget {
            minecraft_version: manifest.minecraft.version,
            loader,
            loader_version: manifest
                .minecraft
                .loader_version
                .map(|version| normalize_loader_pin(loader, &version)),
        }),
        warnings: Vec::new(),
    })
}

fn inspect_modrinth(path: &Path, catalog: &ArchiveCatalog) -> Result<PackInfo> {
    let value = read_json(path, catalog, "modrinth.index.json")?;
    let format_version = value
        .get("formatVersion")
        .and_then(|v| v.as_u64())
        .ok_or_else(|| PackError::Invalid("Modrinth index lacks formatVersion".to_owned()))?;
    if format_version != 1 {
        return Err(PackError::Unsupported(format!(
            "Modrinth format version {format_version} (only v1 is supported)"
        )));
    }
    let deps = value
        .get("dependencies")
        .and_then(|v| v.as_object())
        .ok_or_else(|| PackError::Invalid("Modrinth index lacks dependencies".to_owned()))?;
    let target = target_from_modrinth_dependencies(deps)?;
    let name = json_string(&value, "name")?.to_owned();
    Ok(PackInfo {
        format: PackFormat::Modrinth,
        name,
        version: value.get("versionId").and_then(|v| v.as_str()).map(str::to_owned),
        summary: value.get("summary").and_then(|v| v.as_str()).map(str::to_owned),
        target: Some(target),
        warnings: vec!["Modrinth loader versions are metadata only; the launcher must install the requested loader separately.".to_owned()],
    })
}

fn target_from_modrinth_dependencies(
    deps: &serde_json::Map<String, serde_json::Value>,
) -> Result<PackTarget> {
    let minecraft = deps
        .get("minecraft")
        .and_then(|v| v.as_str())
        .ok_or_else(|| {
            PackError::Invalid(
                "Modrinth dependencies require a string minecraft version".to_owned(),
            )
        })?;
    require_nonempty("Minecraft version", minecraft)?;
    let loaders = [
        ("fabric-loader", ModLoader::Fabric),
        ("forge", ModLoader::Forge),
        ("neoforge", ModLoader::NeoForge),
        ("quilt-loader", ModLoader::Quilt),
    ];
    let found: Vec<_> = loaders
        .iter()
        .filter_map(|(key, loader)| deps.get(*key).map(|v| (*key, *loader, v)))
        .collect();
    if found.len() > 1 {
        return Err(PackError::Invalid(
            "Modrinth dependencies declare multiple mod loaders".to_owned(),
        ));
    }
    if let Some((key, loader, value)) = found.first() {
        let version = value.as_str().ok_or_else(|| {
            PackError::Invalid(format!("Modrinth dependency {key} must be a string"))
        })?;
        require_nonempty("loader version", version)?;
        Ok(PackTarget {
            minecraft_version: minecraft.to_owned(),
            loader: *loader,
            loader_version: Some(normalize_loader_pin(*loader, version)),
        })
    } else {
        Ok(PackTarget {
            minecraft_version: minecraft.to_owned(),
            loader: ModLoader::Vanilla,
            loader_version: None,
        })
    }
}

fn inspect_prism(path: &Path, catalog: &ArchiveCatalog) -> Result<PackInfo> {
    let mut name = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("Prism instance")
        .to_owned();
    if catalog.names.contains_key("instance.cfg") {
        let cfg = String::from_utf8(read_entry(path, catalog, "instance.cfg")?)
            .map_err(|_| PackError::Invalid("instance.cfg is not UTF-8".to_owned()))?;
        if let Some(value) = cfg.lines().find_map(|line| line.strip_prefix("name=")) {
            if !value.trim().is_empty() {
                name = value.trim().to_owned();
            }
        }
    }
    let value = read_json(path, catalog, "mmc-pack.json")?;
    let components = value
        .get("components")
        .and_then(|v| v.as_array())
        .ok_or_else(|| PackError::Invalid("mmc-pack.json lacks components".to_owned()))?;
    let mut minecraft = None;
    let mut loader: Option<(ModLoader, String)> = None;
    for component in components {
        let uid = component.get("uid").and_then(|v| v.as_str()).unwrap_or("");
        let version = component
            .get("version")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        match uid {
            "net.minecraft" => minecraft = Some(version.to_owned()),
            "net.fabricmc.fabric-loader" => {
                set_prism_loader(&mut loader, ModLoader::Fabric, version)?
            }
            "net.minecraftforge" => set_prism_loader(&mut loader, ModLoader::Forge, version)?,
            "net.neoforged" => set_prism_loader(&mut loader, ModLoader::NeoForge, version)?,
            "org.quiltmc.quilt-loader" => set_prism_loader(&mut loader, ModLoader::Quilt, version)?,
            _ => {}
        }
    }
    let minecraft_version = minecraft.filter(|v| !v.is_empty()).ok_or_else(|| {
        PackError::Invalid("Prism pack lacks a net.minecraft component version".to_owned())
    })?;
    let (loader, loader_version) = loader
        .map(|(l, v)| (l, Some(v)))
        .unwrap_or((ModLoader::Vanilla, None));
    Ok(PackInfo {
        format: PackFormat::Prism,
        name,
        version: None,
        summary: None,
        target: Some(PackTarget { minecraft_version, loader, loader_version }),
        warnings: vec!["Prism component versions are metadata only; extra Prism components are not installed by this importer.".to_owned()],
    })
}

fn set_prism_loader(
    slot: &mut Option<(ModLoader, String)>,
    loader: ModLoader,
    version: &str,
) -> Result<()> {
    if slot.is_some() {
        return Err(PackError::Invalid(
            "Prism pack declares multiple mod loaders".to_owned(),
        ));
    }
    require_nonempty("Prism loader version", version)?;
    *slot = Some((loader, normalize_loader_pin(loader, version)));
    Ok(())
}

fn is_curseforge_manifest(value: &serde_json::Value) -> bool {
    value.get("manifestType").and_then(|v| v.as_str()) == Some("minecraftModpack")
        && value.get("manifestVersion").is_some()
}

fn inspect_curseforge_value(value: serde_json::Value) -> Result<PackInfo> {
    let manifest_version = value
        .get("manifestVersion")
        .and_then(|v| v.as_u64())
        .ok_or_else(|| {
            PackError::Invalid("CurseForge manifest lacks manifestVersion".to_owned())
        })?;
    if manifest_version != 1 {
        return Err(PackError::Unsupported(format!(
            "CurseForge manifest version {manifest_version} (only v1 is supported)"
        )));
    }
    let minecraft = value
        .get("minecraft")
        .and_then(|v| v.as_object())
        .ok_or_else(|| {
            PackError::Invalid("CurseForge manifest lacks minecraft metadata".to_owned())
        })?;
    let mc_version = minecraft
        .get("version")
        .and_then(|v| v.as_str())
        .ok_or_else(|| {
            PackError::Invalid("CurseForge manifest lacks minecraft.version".to_owned())
        })?;
    let loaders = minecraft
        .get("modLoaders")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let primary = loaders
        .iter()
        .find(|v| v.get("primary").and_then(|p| p.as_bool()).unwrap_or(false))
        .or_else(|| loaders.first());
    let (loader, loader_version) = match primary.and_then(|v| v.get("id")).and_then(|v| v.as_str())
    {
        None => (ModLoader::Vanilla, None),
        Some(id) => parse_curse_loader(id)?,
    };
    Ok(PackInfo {
        format: PackFormat::CurseForge,
        name: json_string(&value, "name")?.to_owned(),
        version: value.get("version").and_then(|v| v.as_str()).map(str::to_owned),
        summary: value.get("author").and_then(|v| v.as_str()).map(|a| format!("CurseForge pack by {a}")),
        target: Some(PackTarget { minecraft_version: mc_version.to_owned(), loader, loader_version }),
        warnings: vec!["CurseForge project files require an API key and some authors restrict third-party downloads.".to_owned()],
    })
}

fn parse_curse_loader(id: &str) -> Result<(ModLoader, Option<String>)> {
    let lower = id.to_ascii_lowercase();
    for (prefix, loader) in [
        ("fabric-", ModLoader::Fabric),
        ("forge-", ModLoader::Forge),
        ("neoforge-", ModLoader::NeoForge),
        ("quilt-", ModLoader::Quilt),
    ] {
        if let Some(version) = lower.strip_prefix(prefix) {
            require_nonempty("CurseForge loader version", version)?;
            return Ok((
                loader,
                Some(normalize_loader_pin(loader, &id[prefix.len()..])),
            ));
        }
    }
    Err(PackError::Unsupported(format!(
        "unknown CurseForge mod loader id {id:?}"
    )))
}

fn normalize_loader_pin(loader: ModLoader, version: &str) -> String {
    if loader == ModLoader::Forge {
        version
            .strip_suffix(":universal")
            .unwrap_or(version)
            .to_owned()
    } else {
        version.to_owned()
    }
}

fn parse_loader(label: &str) -> Result<ModLoader> {
    ModLoader::from_label(label)
        .or_else(|| {
            ModLoader::ALL
                .into_iter()
                .find(|loader| loader.label().eq_ignore_ascii_case(label))
        })
        .ok_or_else(|| PackError::Unsupported(format!("unknown mod loader {label:?}")))
}

fn require_nonempty(field: &str, value: &str) -> Result<()> {
    if value.trim().is_empty() {
        Err(PackError::Invalid(format!("{field} must not be empty")))
    } else {
        Ok(())
    }
}

fn json_string<'a>(value: &'a serde_json::Value, key: &str) -> Result<&'a str> {
    value
        .get(key)
        .and_then(|v| v.as_str())
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| PackError::Invalid(format!("manifest requires non-empty string {key}")))
}

fn extract_prefix(
    path: &Path,
    catalog: &ArchiveCatalog,
    prefix: &str,
    destination: &Path,
    limits: ArchiveLimits,
    stats: &mut WriteStats,
) -> Result<()> {
    extract_prefix_with_mode(path, catalog, prefix, destination, limits, stats, false)
}

fn extract_prefix_overlay(
    path: &Path,
    catalog: &ArchiveCatalog,
    prefix: &str,
    destination: &Path,
    limits: ArchiveLimits,
    stats: &mut WriteStats,
) -> Result<()> {
    extract_prefix_with_mode(path, catalog, prefix, destination, limits, stats, true)
}

fn extract_prefix_with_mode(
    path: &Path,
    catalog: &ArchiveCatalog,
    prefix: &str,
    destination: &Path,
    limits: ArchiveLimits,
    stats: &mut WriteStats,
    overlay: bool,
) -> Result<()> {
    let selected: Vec<_> = catalog
        .entries
        .iter()
        .filter_map(|entry| {
            entry
                .name
                .strip_prefix(prefix)
                .filter(|relative| !relative.is_empty())
                .map(|relative| (entry, relative.to_owned()))
        })
        .collect();
    extract_entries(path, selected, destination, limits, stats, overlay)
}

// Generic archives have no standard root. Ignore common macOS metadata and strip exactly
// one common wrapper directory only when every meaningful file is beneath it; otherwise
// preserve paths as authored.
fn extract_generic(
    path: &Path,
    catalog: &ArchiveCatalog,
    destination: &Path,
    limits: ArchiveLimits,
    stats: &mut WriteStats,
) -> Result<()> {
    let meaningful: Vec<_> = catalog
        .entries
        .iter()
        .filter(|e| !e.name.ends_with('/') && !is_junk_path(&e.name))
        .collect();
    let wrapper = common_wrapper_root(&meaningful);
    let selected = catalog
        .entries
        .iter()
        .filter(|e| !is_junk_path(&e.name))
        .filter_map(|entry| {
            let relative = wrapper
                .as_ref()
                .and_then(|root| entry.name.strip_prefix(&format!("{root}/")))
                .unwrap_or(&entry.name);
            if relative.is_empty() {
                None
            } else {
                Some((entry, relative.to_owned()))
            }
        })
        .collect();
    extract_entries(path, selected, destination, limits, stats, false)
}

fn common_wrapper_root(entries: &[&EntryMeta]) -> Option<String> {
    let mut root: Option<&str> = None;
    for entry in entries {
        let (first, rest) = entry.name.split_once('/')?;
        if rest.is_empty() {
            return None;
        }
        match root {
            None => root = Some(first),
            Some(existing) if existing == first => {}
            Some(_) => return None,
        }
    }
    root.map(str::to_owned)
}

fn is_junk_path(path: &str) -> bool {
    path == ".DS_Store" || path.starts_with("__MACOSX/")
}

// `overlay` is reserved for format-defined overrides. Initial payload writes use
// `create_new`; overlays may replace an existing regular file via a completed sibling
// temporary, but never follow or replace a symlink/non-file target.
fn extract_entries(
    path: &Path,
    selected: Vec<(&EntryMeta, String)>,
    destination: &Path,
    limits: ArchiveLimits,
    stats: &mut WriteStats,
    overlay: bool,
) -> Result<()> {
    let mut outputs = HashSet::new();
    let mut archive = ZipArchive::new(File::open(path)?)?;
    for (meta, relative) in selected {
        let normalized = validate_zip_name(&relative, meta.is_dir)?;
        if !outputs.insert(normalized.to_lowercase()) {
            return Err(PackError::Security(format!(
                "extracted path collision at {relative:?}"
            )));
        }
        let output = destination.join(normalized.trim_end_matches('/'));
        ensure_beneath(destination, &output)?;
        if meta.is_dir {
            fs::create_dir_all(&output)?;
            continue;
        }
        if stats
            .bytes
            .checked_add(meta.size)
            .map_or(true, |n| n > limits.max_total_bytes)
        {
            return Err(PackError::Limit(
                "combined extracted/downloaded size exceeds total limit".to_owned(),
            ));
        }
        if let Some(parent) = output.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut source = archive.by_index(meta.index)?;
        let copied = if overlay && output.exists() {
            let existing = fs::symlink_metadata(&output)?;
            if existing.file_type().is_symlink() || !existing.is_file() {
                return Err(PackError::Security(format!(
                    "override target is not a regular file: {}",
                    output.display()
                )));
            }
            let temporary = temporary_sibling(&output, "overlay")?;
            let mut cleanup = CleanupPath::new(temporary.clone(), false);
            let mut target = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temporary)?;
            let copied = io::copy(
                &mut source.by_ref().take(limits.max_file_bytes + 1),
                &mut target,
            )?;
            target.sync_all()?;
            fs::rename(&temporary, &output)?;
            cleanup.disarm();
            copied
        } else {
            let mut target = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&output)?;
            io::copy(
                &mut source.by_ref().take(limits.max_file_bytes + 1),
                &mut target,
            )?
        };
        if copied != meta.size {
            return Err(PackError::Invalid(format!(
                "entry {:?} yielded {copied} bytes, expected {}",
                meta.name, meta.size
            )));
        }
        stats.files += 1;
        stats.bytes += copied;
    }
    Ok(())
}

fn ensure_beneath(root: &Path, output: &Path) -> Result<()> {
    if !output.starts_with(root) {
        Err(PackError::Security(format!(
            "output escaped destination: {}",
            output.display()
        )))
    } else {
        Ok(())
    }
}

// The pack's client environment controls inclusion because this launcher imports a client
// instance. Download mirrors are considered in manifest order and the first HTTPS URL is
// used; there is no retry/fallback to a later mirror after a transfer failure.
fn import_modrinth(
    path: &Path,
    catalog: &ArchiveCatalog,
    destination: &Path,
    options: &ImportOptions,
    stats: &mut WriteStats,
    warnings: &mut Vec<String>,
    progress: &mut impl FnMut(&str),
) -> Result<()> {
    let value = read_json(path, catalog, "modrinth.index.json")?;
    let files = value
        .get("files")
        .and_then(|v| v.as_array())
        .ok_or_else(|| PackError::Invalid("Modrinth index files must be an array".to_owned()))?;
    let client = safe_http_client()?;
    for file in files {
        let relative = file
            .get("path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| PackError::Invalid("Modrinth file lacks path".to_owned()))?;
        validate_zip_name(relative, false)?;
        let client_env = file
            .get("env")
            .and_then(|v| v.get("client"))
            .and_then(|v| v.as_str())
            .unwrap_or("required");
        match client_env {
            "unsupported" => {
                warnings.push(format!(
                    "Skipped client-unsupported Modrinth file {relative}"
                ));
                continue;
            }
            "optional" if !options.include_optional_modrinth_files => {
                warnings.push(format!("Skipped optional Modrinth file {relative}"));
                continue;
            }
            "required" | "optional" => {}
            other => {
                return Err(PackError::Invalid(format!(
                    "unknown Modrinth client environment {other:?}"
                )));
            }
        }
        let downloads = file
            .get("downloads")
            .and_then(|v| v.as_array())
            .ok_or_else(|| {
                PackError::Invalid(format!("Modrinth file {relative:?} lacks downloads"))
            })?;
        let url = downloads
            .iter()
            .filter_map(|v| v.as_str())
            .find(|url| url.starts_with("https://"))
            .ok_or_else(|| {
                PackError::Security(format!(
                    "Modrinth file {relative:?} has no HTTPS download URL"
                ))
            })?;
        let expected_size = file
            .get("fileSize")
            .and_then(|v| v.as_u64())
            .ok_or_else(|| {
                PackError::Invalid(format!("Modrinth file {relative:?} lacks fileSize"))
            })?;
        let hashes = file
            .get("hashes")
            .and_then(|v| v.as_object())
            .ok_or_else(|| {
                PackError::Invalid(format!("Modrinth file {relative:?} lacks hashes"))
            })?;
        let sha512 = hashes
            .get("sha512")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                PackError::Invalid(format!("Modrinth file {relative:?} lacks SHA-512"))
            })?;
        let sha1 = hashes
            .get("sha1")
            .and_then(|v| v.as_str())
            .ok_or_else(|| PackError::Invalid(format!("Modrinth file {relative:?} lacks SHA-1")))?;
        progress(&format!("Downloading {relative}"));
        download_file(
            &client,
            url,
            destination,
            relative,
            expected_size,
            Some((sha512, sha1)),
            options.limits,
            stats,
            &[],
        )?;
    }
    progress("Applying Modrinth overrides");
    extract_prefix_overlay(
        path,
        catalog,
        "overrides/",
        destination,
        options.limits,
        stats,
    )?;
    extract_prefix_overlay(
        path,
        catalog,
        "client-overrides/",
        destination,
        options.limits,
        stats,
    )?;
    Ok(())
}

// CurseForge manifests identify project/file IDs rather than direct trusted artifacts.
// Resolve each required item through the API so filename, URL, size, and optional SHA-1
// come from CurseForge; restricted third-party downloads remain an explicit error.
fn import_curseforge(
    path: &Path,
    catalog: &ArchiveCatalog,
    destination: &Path,
    options: &ImportOptions,
    stats: &mut WriteStats,
    warnings: &mut Vec<String>,
    progress: &mut impl FnMut(&str),
) -> Result<()> {
    let value = read_json(path, catalog, "manifest.json")?;
    let files = value
        .get("files")
        .and_then(|v| v.as_array())
        .ok_or_else(|| {
            PackError::Invalid("CurseForge manifest files must be an array".to_owned())
        })?;
    if !files.is_empty()
        && options
            .curseforge_api_key
            .as_deref()
            .filter(|k| !k.trim().is_empty())
            .is_none()
    {
        return Err(PackError::MissingApiKey(
            "set ImportOptions::curseforge_api_key to download project files".to_owned(),
        ));
    }
    let key = options.curseforge_api_key.as_deref().unwrap_or("");
    let client = safe_http_client()?;
    for item in files {
        if item.get("required").and_then(|value| value.as_bool()) == Some(false) {
            let project = item
                .get("projectID")
                .and_then(|value| value.as_u64())
                .unwrap_or(0);
            let file = item
                .get("fileID")
                .and_then(|value| value.as_u64())
                .unwrap_or(0);
            warnings.push(format!(
                "Skipped optional CurseForge project {project}, file {file}"
            ));
            continue;
        }
        let project = item
            .get("projectID")
            .and_then(|v| v.as_u64())
            .ok_or_else(|| PackError::Invalid("CurseForge file lacks projectID".to_owned()))?;
        let file_id = item
            .get("fileID")
            .and_then(|v| v.as_u64())
            .ok_or_else(|| PackError::Invalid("CurseForge file lacks fileID".to_owned()))?;
        progress(&format!(
            "Resolving CurseForge project {project}, file {file_id}"
        ));
        let endpoint = format!("https://api.curseforge.com/v1/mods/{project}/files/{file_id}");
        let response = client.get(&endpoint).header("x-api-key", key).send()?;
        if response.status() == reqwest::StatusCode::FORBIDDEN
            || response.status() == reqwest::StatusCode::UNAUTHORIZED
        {
            return Err(PackError::MissingApiKey(format!(
                "API rejected the key while resolving project {project}, file {file_id}"
            )));
        }
        if !response.status().is_success() {
            return Err(PackError::Invalid(format!(
                "CurseForge API returned {} for project {project}, file {file_id}",
                response.status()
            )));
        }
        let api: serde_json::Value = serde_json::from_reader(response)?;
        let data = api
            .get("data")
            .and_then(|v| v.as_object())
            .ok_or_else(|| PackError::Invalid("CurseForge API response lacks data".to_owned()))?;
        let filename = data
            .get("fileName")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                PackError::Invalid("CurseForge API response lacks fileName".to_owned())
            })?;
        validate_zip_name(filename, false)?;
        if filename.contains('/') {
            return Err(PackError::Security(format!(
                "CurseForge returned nested fileName {filename:?}"
            )));
        }
        let url = data.get("downloadUrl").and_then(|v| v.as_str()).filter(|u| u.starts_with("https://")).ok_or_else(|| PackError::Unsupported(format!("project {project}, file {file_id} has no HTTPS download URL; the author may restrict third-party downloads")))?;
        let size = data
            .get("fileLength")
            .and_then(|v| v.as_u64())
            .ok_or_else(|| {
                PackError::Invalid("CurseForge API response lacks fileLength".to_owned())
            })?;
        let relative = format!("mods/{filename}");
        let sha1 = data
            .get("hashes")
            .and_then(|v| v.as_array())
            .and_then(|hashes| {
                hashes
                    .iter()
                    .find(|h| h.get("algo").and_then(|v| v.as_u64()) == Some(1))
            })
            .and_then(|h| h.get("value"))
            .and_then(|v| v.as_str());
        if sha1.is_none() {
            warnings.push(format!(
                "CurseForge supplied no SHA-1 for {filename}; exact size was verified"
            ));
        }
        progress(&format!("Downloading {relative}"));
        download_file(
            &client,
            url,
            destination,
            &relative,
            size,
            None,
            options.limits,
            stats,
            sha1.into_iter().collect::<Vec<_>>().as_slice(),
        )?;
    }
    let overrides = value
        .get("overrides")
        .and_then(|v| v.as_str())
        .unwrap_or("overrides");
    let prefix = validate_override_directory(overrides)?;
    progress("Applying CurseForge overrides");
    extract_prefix_overlay(
        path,
        catalog,
        &format!("{prefix}/"),
        destination,
        options.limits,
        stats,
    )?;
    Ok(())
}

fn validate_override_directory(value: &str) -> Result<String> {
    let normalized = validate_zip_name(value.trim_end_matches('/'), false)?;
    if normalized.contains('/') {
        return Err(PackError::Security(
            "CurseForge overrides must name one top-level directory".to_owned(),
        ));
    }
    Ok(normalized)
}

// Redirects are revalidated under the same URL policy and capped at ten hops. DNS names
// are not resolved here, so only literal private/local addresses and localhost names can
// be rejected before reqwest connects.
fn safe_http_client() -> Result<reqwest::blocking::Client> {
    let redirects = reqwest::redirect::Policy::custom(|attempt| {
        if attempt.previous().len() >= 10 || validate_download_url(attempt.url()).is_err() {
            attempt.stop()
        } else {
            attempt.follow()
        }
    });
    Ok(reqwest::blocking::Client::builder()
        .user_agent("Ferrite-Launcher pack importer")
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(120))
        .redirect(redirects)
        .build()?)
}

fn validate_download_url(url: &reqwest::Url) -> Result<()> {
    if url.scheme() != "https" || !url.username().is_empty() || url.password().is_some() {
        return Err(PackError::Security(format!(
            "download URL must be HTTPS without embedded credentials: {url}"
        )));
    }
    let host = url
        .host_str()
        .ok_or_else(|| PackError::Security(format!("download URL has no host: {url}")))?;
    if host.eq_ignore_ascii_case("localhost") || host.ends_with(".localhost") {
        return Err(PackError::Security(format!(
            "download URL targets a local host: {url}"
        )));
    }
    if let Ok(address) = host.parse::<IpAddr>() {
        let unsafe_address = match address {
            IpAddr::V4(ip) => {
                ip.is_private()
                    || ip.is_loopback()
                    || ip.is_link_local()
                    || ip.is_unspecified()
                    || ip.is_broadcast()
            }
            IpAddr::V6(ip) => {
                ip.is_loopback()
                    || ip.is_unspecified()
                    || ip.is_unique_local()
                    || ip.is_unicast_link_local()
            }
        };
        if unsafe_address {
            return Err(PackError::Security(format!(
                "download URL targets a private or local address: {url}"
            )));
        }
    }
    Ok(())
}

// Stream directly into the import staging tree while hashing. Declared size is checked
// both before the request (budget reservation) and during/after transfer; a failed partial
// file is harmless to the final destination because the whole staging tree is rolled back.
fn download_file(
    client: &reqwest::blocking::Client,
    url: &str,
    destination: &Path,
    relative: &str,
    expected_size: u64,
    expected_modrinth: Option<(&str, &str)>,
    limits: ArchiveLimits,
    stats: &mut WriteStats,
    expected_sha1_only: &[&str],
) -> Result<()> {
    if expected_size > limits.max_file_bytes
        || stats
            .bytes
            .checked_add(expected_size)
            .map_or(true, |n| n > limits.max_total_bytes)
    {
        return Err(PackError::Limit(format!(
            "download {relative:?} exceeds size limits"
        )));
    }
    let parsed_url = reqwest::Url::parse(url)
        .map_err(|error| PackError::Invalid(format!("invalid download URL: {error}")))?;
    validate_download_url(&parsed_url)?;
    let output = destination.join(relative);
    ensure_beneath(destination, &output)?;
    if let Some(parent) = output.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut response = client.get(parsed_url).send()?;
    if !response.status().is_success() {
        return Err(PackError::Invalid(format!(
            "download for {relative:?} returned HTTP {}",
            response.status()
        )));
    }
    let mut target = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&output)?;
    let mut sha512_hasher = Sha512::new();
    let mut sha1_hasher = Sha1::new();
    let mut total = 0u64;
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let count = response.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        total = total
            .checked_add(count as u64)
            .ok_or_else(|| PackError::Limit("download size overflow".to_owned()))?;
        if total > expected_size || total > limits.max_file_bytes {
            return Err(PackError::Limit(format!(
                "download {relative:?} exceeded declared size"
            )));
        }
        sha512_hasher.update(&buffer[..count]);
        sha1_hasher.update(&buffer[..count]);
        target.write_all(&buffer[..count])?;
    }
    if total != expected_size {
        return Err(PackError::Invalid(format!(
            "download {relative:?} was {total} bytes, expected {expected_size}"
        )));
    }
    let actual512 = hex::encode(sha512_hasher.finalize());
    let actual1 = hex::encode(sha1_hasher.finalize());
    if let Some((wanted512, wanted1)) = expected_modrinth {
        if !actual512.eq_ignore_ascii_case(wanted512) || !actual1.eq_ignore_ascii_case(wanted1) {
            return Err(PackError::Security(format!(
                "hash mismatch for downloaded file {relative:?}"
            )));
        }
    }
    if let Some(wanted) = expected_sha1_only.first() {
        if !actual1.eq_ignore_ascii_case(wanted) {
            return Err(PackError::Security(format!(
                "SHA-1 mismatch for downloaded file {relative:?}"
            )));
        }
    }
    stats.files += 1;
    stats.bytes += total;
    Ok(())
}

// Internal form takes an explicit target, which keeps format serialization testable apart
// from `InstanceProfile` and the installed-loader lookup used by the public wrapper.
fn export_directory(
    source: &Path,
    output: &Path,
    options: &ExportOptions,
    target: &PackTarget,
    progress: &mut impl FnMut(&str),
) -> Result<()> {
    if output.exists() {
        return Err(PackError::AlreadyExists(output.to_owned()));
    }
    if !source.is_dir() {
        return Err(PackError::Invalid(format!(
            "instance game directory does not exist: {}",
            source.display()
        )));
    }
    if options.format == PackFormat::Lunar {
        return Err(lunar_error());
    }
    require_nonempty("pack name", &options.name)?;

    let files = collect_export_files(source, options.include_worlds)?;
    let temporary = temporary_sibling(output, "export")?;
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)?;
    let mut cleanup = CleanupPath::new(temporary.clone(), false);
    let mut zip = ZipWriter::new(file);
    let zip_options = FileOptions::default()
        .compression_method(CompressionMethod::Deflated)
        .unix_permissions(0o644);

    progress("Writing pack manifest");
    match options.format {
        PackFormat::Ferrite => {
            let manifest = FerriteManifest {
                format_version: 1,
                name: options.name.clone(),
                version: options.version.clone(),
                summary: options.summary.clone(),
                minecraft: FerriteMinecraft {
                    version: target.minecraft_version.clone(),
                    loader: target.loader.label().to_owned(),
                    loader_version: target.loader_version.clone(),
                },
            };
            write_json_to_zip(&mut zip, "ferritepack.json", &manifest, zip_options)?;
            write_selected_files(
                &mut zip,
                source,
                &files,
                "overrides/",
                zip_options,
                progress,
            )?;
        }
        PackFormat::GenericZip => {
            write_selected_files(&mut zip, source, &files, "", zip_options, progress)?
        }
        PackFormat::Prism => {
            let components = prism_components(target)?;
            let manifest = serde_json::json!({ "formatVersion": 1, "components": components });
            write_json_to_zip(&mut zip, "mmc-pack.json", &manifest, zip_options)?;
            zip.start_file("instance.cfg", zip_options)?;
            writeln!(zip, "[General]")?;
            writeln!(zip, "ConfigVersion=1.2")?;
            writeln!(zip, "InstanceType=OneSix")?;
            writeln!(zip, "name={}", cfg_value(&options.name))?;
            write_selected_files(
                &mut zip,
                source,
                &files,
                ".minecraft/",
                zip_options,
                progress,
            )?;
        }
        PackFormat::Modrinth => {
            let mut dependencies = serde_json::Map::new();
            dependencies.insert(
                "minecraft".to_owned(),
                serde_json::Value::String(target.minecraft_version.clone()),
            );
            if target.loader != ModLoader::Vanilla {
                let key = modrinth_loader_key(target.loader)?;
                let pin = target.loader_version.clone().ok_or_else(|| {
                    PackError::Invalid(format!(
                        "{} export requires a loader version for Modrinth metadata",
                        target.loader.label()
                    ))
                })?;
                dependencies.insert(key.to_owned(), serde_json::Value::String(pin));
            }
            let mut manifest = serde_json::json!({
                "formatVersion": 1, "game": "minecraft", "versionId": options.version.clone().unwrap_or_else(|| "1.0.0".to_owned()),
                "name": options.name, "files": [], "dependencies": dependencies
            });
            if let Some(summary) = &options.summary {
                manifest["summary"] = serde_json::Value::String(summary.clone());
            }
            write_json_to_zip(&mut zip, "modrinth.index.json", &manifest, zip_options)?;
            write_selected_files(
                &mut zip,
                source,
                &files,
                "overrides/",
                zip_options,
                progress,
            )?;
        }
        PackFormat::CurseForge => {
            let mod_loaders: Vec<serde_json::Value> = if target.loader == ModLoader::Vanilla {
                Vec::new()
            } else {
                let pin = target.loader_version.clone().ok_or_else(|| {
                    PackError::Invalid(format!(
                        "{} export requires a loader version for CurseForge metadata",
                        target.loader.label()
                    ))
                })?;
                vec![
                    serde_json::json!({ "id": format!("{}-{pin}", curse_loader_prefix(target.loader)?), "primary": true }),
                ]
            };
            let manifest = serde_json::json!({
                "minecraft": { "version": target.minecraft_version, "modLoaders": mod_loaders },
                "manifestType": "minecraftModpack", "manifestVersion": 1,
                "name": options.name, "version": options.version.clone().unwrap_or_else(|| "1.0.0".to_owned()),
                "author": "Ferrite Launcher export", "files": [], "overrides": "overrides"
            });
            write_json_to_zip(&mut zip, "manifest.json", &manifest, zip_options)?;
            write_selected_files(
                &mut zip,
                source,
                &files,
                "overrides/",
                zip_options,
                progress,
            )?;
        }
        PackFormat::Lunar => unreachable!(),
    }
    zip.finish()?.sync_all()?;
    if output.exists() {
        return Err(PackError::AlreadyExists(output.to_owned()));
    }
    // A hard link publishes the completed sibling atomically and fails rather
    // than replacing an output created concurrently.
    fs::hard_link(&temporary, output)?;
    fs::remove_file(&temporary)?;
    cleanup.disarm();
    progress("Export complete");
    Ok(())
}

// The generic `Serialize` bound lets typed Ferrite manifests and ad-hoc JSON values share
// identical ZIP-writing behavior without converting every model through `Value`.
fn write_json_to_zip<T: Serialize>(
    zip: &mut ZipWriter<File>,
    name: &str,
    value: &T,
    options: FileOptions,
) -> Result<()> {
    zip.start_file(name, options)?;
    serde_json::to_writer_pretty(zip, value)?;
    Ok(())
}

// Collection is separate from writing so a symlink/special-file failure is discovered
// before the archive receives payload entries. Sorting produces deterministic entry order,
// though ZIP metadata/compression does not promise byte-for-byte reproducible archives.
fn collect_export_files(root: &Path, include_worlds: bool) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    collect_export_files_at(root, root, include_worlds, &mut files)?;
    files.sort();
    Ok(files)
}

fn collect_export_files_at(
    root: &Path,
    directory: &Path,
    include_worlds: bool,
    files: &mut Vec<PathBuf>,
) -> Result<()> {
    let mut entries: Vec<_> =
        fs::read_dir(directory)?.collect::<std::result::Result<Vec<_>, _>>()?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path)?;
        if metadata.file_type().is_symlink() {
            return Err(PackError::Security(format!(
                "cannot export symlink {}",
                path.display()
            )));
        }
        if !metadata.is_file() && !metadata.is_dir() {
            return Err(PackError::Security(format!(
                "cannot export special file {}",
                path.display()
            )));
        }
        let relative = path
            .strip_prefix(root)
            .map_err(|_| PackError::Security("export path escaped game directory".to_owned()))?;
        if excluded_export_path(relative, include_worlds) {
            continue;
        }
        if metadata.is_dir() {
            collect_export_files_at(root, &path, include_worlds, files)?;
        } else {
            files.push(relative.to_owned());
        }
    }
    Ok(())
}

// Exclusions remove volatile diagnostics, caches, screenshots, and account-adjacent state;
// worlds are large and potentially private, so `saves/` requires explicit opt-in.
fn excluded_export_path(path: &Path, include_worlds: bool) -> bool {
    let first = match path.components().next() {
        Some(Component::Normal(value)) => value.to_string_lossy().to_ascii_lowercase(),
        _ => return true,
    };
    matches!(
        first.as_str(),
        "logs"
            | "crash-reports"
            | "backups"
            | "screenshots"
            | "server-resource-packs"
            | "usercache.json"
            | "usernamecache.json"
            | "realms_persistence.json"
            | "launcher_log.txt"
    ) || (!include_worlds && first == "saves")
}

// `FnMut` permits stateful progress sinks and `&mut impl FnMut` threads one sink through
// nested helpers without moving it. Messages are emitted before opening/copying each file,
// so the last message is not proof that the corresponding write succeeded.
fn write_selected_files(
    zip: &mut ZipWriter<File>,
    source: &Path,
    files: &[PathBuf],
    prefix: &str,
    options: FileOptions,
    progress: &mut impl FnMut(&str),
) -> Result<()> {
    for relative in files {
        let slash = path_to_zip(relative)?;
        let name = format!("{prefix}{slash}");
        progress(&format!("Adding {slash}"));
        zip.start_file(name, options)?;
        let mut input = File::open(source.join(relative))?;
        io::copy(&mut input, zip)?;
    }
    Ok(())
}

fn path_to_zip(path: &Path) -> Result<String> {
    let mut parts = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(value) => parts.push(value.to_str().ok_or_else(|| {
                PackError::Unsupported(format!(
                    "non-UTF-8 path cannot be packed: {}",
                    path.display()
                ))
            })?),
            _ => {
                return Err(PackError::Security(format!(
                    "unsafe export path {}",
                    path.display()
                )));
            }
        }
    }
    validate_zip_name(&parts.join("/"), false)
}

fn prism_components(target: &PackTarget) -> Result<Vec<serde_json::Value>> {
    let mut components = vec![
        serde_json::json!({ "important": true, "uid": "net.minecraft", "version": target.minecraft_version }),
    ];
    if target.loader != ModLoader::Vanilla {
        let uid =
            prism_loader_uid(target.loader).expect("every non-vanilla loader has a Prism UID");
        let version = target.loader_version.as_ref().ok_or_else(|| {
            PackError::Invalid(format!(
                "{} export requires a loader version for Prism metadata",
                target.loader.label()
            ))
        })?;
        components.push(serde_json::json!({ "uid": uid, "version": version }));
    }
    Ok(components)
}

fn prism_loader_uid(loader: ModLoader) -> Option<&'static str> {
    match loader {
        ModLoader::Vanilla => None,
        ModLoader::Fabric => Some("net.fabricmc.fabric-loader"),
        ModLoader::Forge => Some("net.minecraftforge"),
        ModLoader::NeoForge => Some("net.neoforged"),
        ModLoader::Quilt => Some("org.quiltmc.quilt-loader"),
    }
}

fn modrinth_loader_key(loader: ModLoader) -> Result<&'static str> {
    match loader {
        ModLoader::Fabric => Ok("fabric-loader"),
        ModLoader::Forge => Ok("forge"),
        ModLoader::NeoForge => Ok("neoforge"),
        ModLoader::Quilt => Ok("quilt-loader"),
        ModLoader::Vanilla => Err(PackError::Invalid(
            "vanilla has no Modrinth loader key".to_owned(),
        )),
    }
}

fn curse_loader_prefix(loader: ModLoader) -> Result<&'static str> {
    match loader {
        ModLoader::Fabric => Ok("fabric"),
        ModLoader::Forge => Ok("forge"),
        ModLoader::NeoForge => Ok("neoforge"),
        ModLoader::Quilt => Ok("quilt"),
        ModLoader::Vanilla => Err(PackError::Invalid(
            "vanilla has no CurseForge loader prefix".to_owned(),
        )),
    }
}

fn cfg_value(value: &str) -> String {
    value.replace(['\r', '\n'], " ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    struct TempDir(PathBuf);
    impl TempDir {
        fn new(label: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "ferrite-packs-{label}-{}-{}",
                std::process::id(),
                TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn make_zip(path: &Path, entries: &[(&str, &[u8])]) {
        let file = File::create(path).unwrap();
        let mut zip = ZipWriter::new(file);
        for (name, bytes) in entries {
            zip.start_file(*name, FileOptions::default()).unwrap();
            zip.write_all(bytes).unwrap();
        }
        zip.finish().unwrap();
    }

    fn target() -> PackTarget {
        PackTarget {
            minecraft_version: "1.20.1".to_owned(),
            loader: ModLoader::Fabric,
            loader_version: Some("0.15.0".to_owned()),
        }
    }

    #[test]
    fn ferrite_round_trip() {
        let temp = TempDir::new("roundtrip");
        let game = temp.0.join("game");
        fs::create_dir_all(game.join("config")).unwrap();
        fs::create_dir_all(game.join("saves/world")).unwrap();
        fs::write(game.join("config/test.toml"), b"enabled=true").unwrap();
        fs::write(game.join("saves/world/level.dat"), b"world").unwrap();
        let pack = temp.0.join("test.ferritepack");
        let options = ExportOptions {
            format: PackFormat::Ferrite,
            name: "Test".to_owned(),
            version: Some("2".to_owned()),
            summary: None,
            loader_version: None,
            include_worlds: false,
        };
        export_directory(&game, &pack, &options, &target(), &mut |_| {}).unwrap();
        let destination = temp.0.join("imported");
        let report = import(&pack, &destination, &ImportOptions::default(), |_| {}).unwrap();
        assert_eq!(report.info.format, PackFormat::Ferrite);
        assert_eq!(
            fs::read(destination.join("config/test.toml")).unwrap(),
            b"enabled=true"
        );
        assert!(!destination.join("saves").exists());
    }

    #[test]
    fn highly_compressible_minecraft_files_are_allowed() {
        let temp = TempDir::new("compressible");
        let pack = temp.0.join("world.zip");
        let file = File::create(&pack).unwrap();
        let mut zip = ZipWriter::new(file);
        zip.start_file(
            "minecraft/saves/city/entities/r.-3.1.mca",
            FileOptions::default().compression_method(CompressionMethod::Deflated),
        )
        .unwrap();
        zip.write_all(&vec![0; 1024 * 1024]).unwrap();
        zip.finish().unwrap();

        assert_eq!(inspect(pack).unwrap().format, PackFormat::GenericZip);
    }

    #[test]
    fn traversal_is_rejected() {
        let temp = TempDir::new("traversal");
        let pack = temp.0.join("bad.zip");
        make_zip(&pack, &[("../escape", b"bad")]);
        assert!(matches!(inspect(&pack), Err(PackError::Security(_))));
    }

    #[test]
    fn portable_paths_and_local_download_urls_are_rejected() {
        let temp = TempDir::new("portable-paths");
        for name in ["mods/CON.jar", "mods/file.jar:stream", "mods/trailing. "] {
            let pack = temp.0.join(format!(
                "{}.zip",
                TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
            ));
            make_zip(&pack, &[(name, b"bad")]);
            assert!(matches!(inspect(&pack), Err(PackError::Security(_))));
        }
        for url in [
            "http://example.com/file",
            "https://localhost/file",
            "https://127.0.0.1/file",
        ] {
            let url = reqwest::Url::parse(url).unwrap();
            assert!(matches!(
                validate_download_url(&url),
                Err(PackError::Security(_))
            ));
        }
    }

    #[test]
    fn modrinth_client_overrides_replace_base_overrides() {
        let temp = TempDir::new("mr-overlays");
        let pack = temp.0.join("overlays.mrpack");
        let manifest = br#"{"formatVersion":1,"game":"minecraft","versionId":"1","name":"Overlay","files":[],"dependencies":{"minecraft":"1.20.1","forge":"47.2.0:universal"}}"#;
        make_zip(
            &pack,
            &[
                ("modrinth.index.json", manifest),
                ("overrides/config/value.txt", b"base"),
                ("client-overrides/config/value.txt", b"client"),
            ],
        );
        let destination = temp.0.join("imported");
        let report = import(&pack, &destination, &ImportOptions::default(), |_| {}).unwrap();
        assert_eq!(
            report.info.target.unwrap().loader_version.as_deref(),
            Some("47.2.0")
        );
        assert_eq!(
            fs::read(destination.join("config/value.txt")).unwrap(),
            b"client"
        );
    }

    #[test]
    fn symlink_entry_is_rejected() {
        let temp = TempDir::new("symlink");
        let pack = temp.0.join("bad.zip");
        let file = File::create(&pack).unwrap();
        let mut zip = ZipWriter::new(file);
        zip.add_symlink("link", "target", FileOptions::default())
            .unwrap();
        zip.finish().unwrap();
        assert!(matches!(inspect(&pack), Err(PackError::Security(_))));
    }

    #[test]
    fn import_and_export_do_not_overwrite() {
        let temp = TempDir::new("overwrite");
        let pack = temp.0.join("generic.zip");
        make_zip(&pack, &[("options.txt", b"x")]);
        let destination = temp.0.join("existing");
        fs::create_dir(&destination).unwrap();
        let mut options = ImportOptions::default();
        options.generic_target = Some(target());
        assert!(matches!(
            import(&pack, &destination, &options, |_| {}),
            Err(PackError::AlreadyExists(_))
        ));
        let output = temp.0.join("existing-output.zip");
        fs::write(&output, b"keep").unwrap();
        let export_options = ExportOptions {
            format: PackFormat::GenericZip,
            name: "x".to_owned(),
            version: None,
            summary: None,
            loader_version: None,
            include_worlds: false,
        };
        assert!(matches!(
            export_directory(
                &destination,
                &output,
                &export_options,
                &target(),
                &mut |_| {}
            ),
            Err(PackError::AlreadyExists(_))
        ));
        assert_eq!(fs::read(output).unwrap(), b"keep");
    }

    #[test]
    fn generic_import_requires_target() {
        let temp = TempDir::new("generic");
        let pack = temp.0.join("generic.zip");
        make_zip(&pack, &[("mods/a.jar", b"x")]);
        assert!(matches!(
            import(&pack, temp.0.join("out"), &ImportOptions::default(), |_| {}),
            Err(PackError::Invalid(_))
        ));
    }

    #[test]
    fn prism_exports_standard_dot_minecraft_root() {
        let temp = TempDir::new("prism-export");
        let game = temp.0.join("game");
        fs::create_dir_all(game.join("mods")).unwrap();
        fs::write(game.join("mods/example.jar"), b"jar").unwrap();
        let output = temp.0.join("instance.zip");
        let options = ExportOptions {
            format: PackFormat::Prism,
            name: "Prism Test".to_owned(),
            version: None,
            summary: None,
            loader_version: Some("0.15.0".to_owned()),
            include_worlds: false,
        };
        export_directory(&game, &output, &options, &target(), &mut |_| {}).unwrap();
        let mut archive = ZipArchive::new(File::open(output).unwrap()).unwrap();
        assert!(archive.by_name(".minecraft/mods/example.jar").is_ok());
    }

    #[test]
    fn detects_prism_name_and_target() {
        let temp = TempDir::new("prism");
        let pack = temp.0.join("instance.zip");
        let manifest = br#"{"formatVersion":1,"components":[{"uid":"net.minecraft","version":"1.20.1"},{"uid":"net.fabricmc.fabric-loader","version":"0.15.0"}]}"#;
        make_zip(
            &pack,
            &[
                ("mmc-pack.json", manifest),
                ("instance.cfg", b"name=Prism Test\n"),
                ("minecraft/options.txt", b"x"),
            ],
        );
        let info = inspect(&pack).unwrap();
        assert_eq!(info.format, PackFormat::Prism);
        assert_eq!(info.name, "Prism Test");
        assert_eq!(info.target.unwrap(), target());
    }

    #[test]
    fn known_extension_requires_manifest() {
        let temp = TempDir::new("extension");
        let pack = temp.0.join("fake.mrpack");
        make_zip(&pack, &[("options.txt", b"x")]);
        assert!(matches!(inspect(&pack), Err(PackError::Invalid(_))));
    }

    #[test]
    fn lunar_is_actionably_rejected() {
        let temp = TempDir::new("lunar");
        let pack = temp.0.join("anything.lcpack");
        make_zip(&pack, &[("anything", b"x")]);
        let error = inspect(&pack).unwrap_err().to_string();
        assert!(error.contains("Modrinth or CurseForge"));
    }

    #[test]
    fn absolute_and_dotdot_entry_names_are_rejected() {
        let temp = TempDir::new("abs-paths");
        for name in [
            "/etc/passwd",
            "C:/Windows/system32/evil.dll",
            "mods/../../escape.txt",
        ] {
            let pack = temp.0.join(format!(
                "bad-{}.zip",
                TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
            ));
            make_zip(&pack, &[(name, b"x")]);
            assert!(
                matches!(inspect(&pack), Err(PackError::Security(_))),
                "expected Security for {name:?}"
            );
        }
    }

    #[test]
    fn archive_entry_count_limit_is_enforced() {
        let temp = TempDir::new("entry-limit");
        let pack = temp.0.join("many.zip");
        let file = File::create(&pack).unwrap();
        let mut zip = ZipWriter::new(file);
        for i in 0..5 {
            zip.start_file(format!("f{i}.txt"), FileOptions::default())
                .unwrap();
            zip.write_all(b"x").unwrap();
        }
        zip.finish().unwrap();
        let limits = ArchiveLimits {
            max_entries: 3,
            max_file_bytes: 1024 * 1024,
            max_total_bytes: 1024 * 1024,
        };
        assert!(matches!(
            inspect_with_limits(&pack, limits),
            Err(PackError::Limit(_))
        ));
    }

    #[test]
    fn archive_file_and_total_size_limits_are_enforced() {
        let temp = TempDir::new("size-limit");
        let pack = temp.0.join("big.zip");
        make_zip(&pack, &[("mods/big.jar", &[0u8; 64])]);
        let per_file = ArchiveLimits {
            max_entries: 100,
            max_file_bytes: 16,
            max_total_bytes: 1024 * 1024,
        };
        assert!(matches!(
            inspect_with_limits(&pack, per_file),
            Err(PackError::Limit(_))
        ));
        let total = ArchiveLimits {
            max_entries: 100,
            max_file_bytes: 1024 * 1024,
            max_total_bytes: 32,
        };
        assert!(matches!(
            inspect_with_limits(&pack, total),
            Err(PackError::Limit(_))
        ));
    }
}
