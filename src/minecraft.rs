//! # Minecraft-specific backend logic for Ferrite Launcher.
//!
//! This module knows everything about Mojang's version manifest, version
//! metadata, libraries, natives, assets, and how to launch the game.
//! Nothing in here touches egui or any GUI code — `app.rs` only ever
//! calls the small public API below and only ever sees plain
//! `Result` / `Option` / `String` values, never Mojang's JSON shapes.
//!
//! Mojang has shipped two incompatible version-metadata shapes over the
//! years, and this module now understands both:
//!
//! - **Modern** (roughly 1.13+): a structured `arguments` object with
//!   separate `jvm` and `game` argument lists, each entry optionally
//!   gated by OS/feature rules; a `javaVersion` requirement; and
//!   natives shipped as ordinary `…:natives-<os>` library entries.
//! - **Legacy** (everything before that, e.g. 1.0): a single
//!   `minecraftArguments` string of game args with no rules at all and
//!   no JVM argument list — the launcher itself was expected to know
//!   the standard `-cp`/`-Djava.library.path` invocation; no
//!   `javaVersion` field; and natives shipped via the older
//!   `natives`/`classifiers` scheme (already handled below). Legacy
//!   asset indexes are also frequently "virtual", meaning the client
//!   reads assets straight off disk by their original filename rather
//!   than through the hash-object scheme.
//!
//! All of that is resolved internally; `install_version` /
//! `launch_version` behave identically from the caller's perspective
//! regardless of which shape a given version uses.
//!
//! ## Storage and process model
//!
//! Launcher-managed content is rooted at [`AppPaths::storage_root`], resolved once at
//! startup (never relative to the working directory) and passed into every function
//! here. Version metadata and client jars live under `versions/<id>/`, Maven artifacts
//! are shared under `libraries/`, content-addressed assets under `assets/`, and
//! extracted native libraries under `natives/<id>/`.
//! Existing libraries and assets are treated as a download cache; version
//! metadata and asset indexes are refreshed during installation.
//!
//! A process-wide `OnceLock<Mutex<Option<Child>>>` stores the one Minecraft
//! child started by this process. The mutex permits UI and worker threads to
//! inspect or kill the same owned `Child` handle without exposing it publicly;
//! it is not inter-process locking, so another Ferrite process is independent.
//!
//! ## Mod loaders (Fabric, Forge, NeoForge, Quilt)
//!
//! This module has no idea mod loaders exist. `crate::loaders` builds
//! ordinary, vanilla-shaped synthetic versions on top of what's here
//! (see `crate::loaders::fabric`) and then calls `install_version` /
//! `launch_version` / `is_version_installed` on those synthetic ids
//! exactly as if they were another Mojang release. The handful of
//! `pub(crate)` items below (`download_file`, `copy_natives`) exist only so
//! that module can reuse this one's download logic instead of duplicating it;
//! the filesystem layout itself comes from [`AppPaths`].

use ferrite_launcher::core::paths::AppPaths;
use reqwest::blocking::Client;
use serde::Deserialize;
use std::collections::HashMap;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::{Mutex, OnceLock};
use zip::ZipArchive;

// =====================================================================
// Error handling
// =====================================================================

/// Errors produced while discovering, installing, or launching Minecraft and
/// loader-backed synthetic versions.
#[derive(Debug)]
pub enum FerriteError {
    /// An HTTP request failed, including non-success status responses.
    Network(reqwest::Error),
    /// A launcher-managed file or child process operation failed.
    Io(std::io::Error),
    /// Mojang or loader metadata was not valid for the expected JSON shape.
    Json(serde_json::Error),
    /// A native or installer archive could not be read or written.
    Zip(zip::result::ZipError),
    /// The requested id was absent from the current Mojang manifest.
    VersionNotFound(String),
    /// Required local version metadata or `client.jar` is missing.
    NotInstalled,
    /// The `java` executable on `PATH` is absent, unparseable, or too old.
    JavaVersionMismatch { required: u32, found: Option<u32> },
    /// The process-local child slot still contains a running game.
    AlreadyRunning,
    /// The authenticated session expired and requires a fresh sign-in.
    AuthenticationExpired,
    /// A mod loader (Forge/NeoForge/Quilt) that isn't implemented yet.
    LoaderNotImplemented(&'static str),
    /// `crate::loaders` was asked to launch/check a mod-loader version
    /// for a Minecraft version that hasn't had that loader installed.
    LoaderNotInstalled(String),
    /// A loader's metadata API had nothing published for the requested
    /// Minecraft version (e.g. no Fabric loader builds for it).
    LoaderVersionUnavailable(String),
    /// A Forge/NeoForge installer JAR ran but exited with an error.
    InstallerFailed(String),
}

impl fmt::Display for FerriteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FerriteError::Network(e) => write!(f, "network error: {e}"),
            FerriteError::Io(e) => write!(f, "filesystem error: {e}"),
            FerriteError::Json(e) => write!(f, "failed to parse Mojang metadata: {e}"),
            FerriteError::Zip(e) => write!(f, "failed to extract native library: {e}"),
            FerriteError::VersionNotFound(id) => {
                write!(f, "version '{id}' was not found in Mojang's manifest")
            }
            FerriteError::NotInstalled => {
                write!(
                    f,
                    "that version isn't installed — call install_version() first"
                )
            }
            FerriteError::JavaVersionMismatch { required, found } => write!(
                f,
                "this Minecraft version needs Java {required}, but found {}",
                found
                    .map(|v| v.to_string())
                    .unwrap_or_else(|| "no working `java`".into())
            ),
            FerriteError::AlreadyRunning => write!(f, "Minecraft is already running"),
            FerriteError::AuthenticationExpired => {
                write!(
                    f,
                    "authentication expired — sign in again before launching Minecraft"
                )
            }
            FerriteError::LoaderNotImplemented(name) => {
                write!(f, "{name} support isn't implemented yet")
            }
            FerriteError::LoaderNotInstalled(mc_version) => write!(
                f,
                "no mod loader is installed for Minecraft {mc_version} — install it first"
            ),
            FerriteError::LoaderVersionUnavailable(mc_version) => write!(
                f,
                "no compatible loader build was found for Minecraft {mc_version}"
            ),
            FerriteError::InstallerFailed(detail) => {
                write!(f, "mod-loader installer failed: {detail}")
            }
        }
    }
}

impl std::error::Error for FerriteError {}

impl From<reqwest::Error> for FerriteError {
    fn from(e: reqwest::Error) -> Self {
        FerriteError::Network(e)
    }
}
impl From<std::io::Error> for FerriteError {
    fn from(e: std::io::Error) -> Self {
        FerriteError::Io(e)
    }
}
impl From<serde_json::Error> for FerriteError {
    fn from(e: serde_json::Error) -> Self {
        FerriteError::Json(e)
    }
}
impl From<zip::result::ZipError> for FerriteError {
    fn from(e: zip::result::ZipError) -> Self {
        FerriteError::Zip(e)
    }
}

/// Result type shared by Minecraft and mod-loader operations.
pub type Result<T> = std::result::Result<T, FerriteError>;

// =====================================================================
// PUBLIC API — this is the only part `app.rs` should ever touch.
// =====================================================================

/// Fetches release ids from Mojang's current version manifest.
///
/// Results preserve manifest order (normally newest first). This always performs
/// a network request and does not inspect which versions are installed locally.
pub fn get_versions() -> Result<Vec<String>> {
    get_versions_with_snapshots(false)
}

/// Fetches selectable ids from Mojang's current version manifest.
///
/// When `show_snapshots` is `false`, only entries whose manifest type is exactly
/// `release` are returned. When it is `true`, every manifest entry is returned,
/// including snapshots and any other types Mojang publishes.
pub fn get_versions_with_snapshots(show_snapshots: bool) -> Result<Vec<String>> {
    let client = Client::new();
    let manifest = fetch_manifest(&client)?;

    Ok(manifest
        .versions
        .into_iter()
        .filter(|version| show_snapshots || version.version_type == "release")
        .map(|version| version.id)
        .collect())
}

/// Downloads and installs an official Minecraft Java version, printing phase
/// progress to stdout.
///
/// The pipeline stores metadata and the client jar under `versions/<id>/`,
/// libraries under the shared Maven-style `libraries/` tree, assets under the
/// shared content-addressed `assets/` tree, and extracted natives under
/// `natives/<id>/`. Existing libraries, native jars, and asset objects are
/// reused by path; the manifest, metadata, client jar, and asset index are
/// fetched again. Failures return immediately and may leave a partial install
/// that a later call can resume.
pub fn install_version(paths: &AppPaths, version: &str) -> Result<()> {
    install_version_with_progress(paths, version, |message| println!("{message}"))
}

/// Installs a version synchronously, reporting metadata, client, libraries/native,
/// and asset phases through `progress` before their existing operations run.
/// The success message is emitted only after all phases complete; errors return
/// immediately without reporting subsequent phases.
///
/// The callback runs synchronously on the calling thread. `FnMut` allows it to
/// update captured progress state, while the borrowed `&str` means Ferrite does
/// not transfer message ownership; callers that retain or send a message must
/// clone it. No `Send`, `Sync`, or `'static` bound is required because this
/// function neither stores the closure nor moves it to another thread. Per-file
/// messages from download helpers still go to stdout.
pub fn install_version_with_progress(
    paths: &AppPaths,
    version: &str,
    mut progress: impl FnMut(&str),
) -> Result<()> {
    let client = Client::new();

    progress("Fetching version manifest...");
    let manifest = fetch_manifest(&client)?;
    let entry = manifest
        .versions
        .iter()
        .find(|v| v.id == version)
        .ok_or_else(|| FerriteError::VersionNotFound(version.to_string()))?;

    progress(&format!("Fetching version metadata for {}...", entry.id));
    let (metadata, raw_json) = fetch_version_metadata(&client, &entry.url)?;

    let v_dir = paths.version_dir(&metadata.id);
    fs::create_dir_all(&v_dir)?;
    fs::write(v_dir.join(format!("{}.json", metadata.id)), &raw_json)?;
    progress(&format!("Saved version metadata for {}.", metadata.id));

    progress("Downloading client jar...");
    download_file(
        &client,
        &metadata.downloads.client.url,
        &v_dir.join("client.jar"),
        Some(metadata.downloads.client.size),
    )?;

    progress("Downloading libraries and natives...");
    download_libraries(
        &client,
        &metadata.libraries,
        &paths.libraries_dir(),
        &paths.natives_dir(&metadata.id),
    )?;

    progress("Downloading assets (this can take a while the first time)...");
    download_assets(
        &client,
        &metadata.asset_index.url,
        &metadata.assets,
        &paths.assets_dir(),
    )?;

    progress(&format!(
        "Minecraft {} installed successfully.",
        metadata.id
    ));
    Ok(())
}

/// Launches the given Minecraft version as a child process. Returns
/// `FerriteError::NotInstalled` if `install_version()` hasn't completed
/// successfully for this id yet, `FerriteError::AlreadyRunning` if a
/// previous launch is still alive, or `FerriteError::JavaVersionMismatch`
/// if the `java` on PATH is older than the version Mojang requires.
///
/// `version` doesn't have to be a real Mojang release id — anything
/// under `<storage root>/versions/<id>/<id>.json` with a vanilla-shaped
/// metadata file works, which is how `crate::loaders::fabric` piggybacks
/// on this function for modded launches.
///
/// Explicit offline compatibility wrapper: uses placeholder credentials for
/// singleplayer, not online-mode servers. Account-aware callers should use
/// `launch_authenticated` instead.
pub fn launch_version(paths: &AppPaths, version: &str) -> Result<()> {
    launch_version_in_directory(paths, version, paths.storage_root())
}

/// Explicit offline compatibility wrapper for an installed vanilla or synthetic
/// loader version in `game_dir`. Use `launch_authenticated` for account launches.
/// Creates the directory if needed (callers pass absolute paths derived from
/// [`AppPaths`]). The canonical path is used for both
/// `${game_directory}` (normally `--gameDir`) and the child's working directory.
/// Assets, libraries, version files, and natives remain in shared storage.
/// Authentication, Java checks, and the single-running-process limit are the
/// same as in `launch_version`; this does not install or copy game content.
pub fn launch_version_in_directory(paths: &AppPaths, version: &str, game_dir: &Path) -> Result<()> {
    launch_with_auth(
        paths,
        version,
        game_dir,
        offline_auth_placeholders(),
        true,
        None,
    )
}

/// Offline launch using an explicit maximum Java heap size.
pub fn launch_version_in_directory_with_memory(
    paths: &AppPaths,
    version: &str,
    game_dir: &Path,
    memory_mb: u32,
) -> Result<()> {
    launch_with_auth(
        paths,
        version,
        game_dir,
        offline_auth_placeholders(),
        true,
        Some(memory_mb),
    )
}

/// Launches an installed vanilla or synthetic loader version with a Microsoft
/// account. Rejects expired sessions; the caller must sign in again (no refresh
/// or offline fallback). Directory, Java, and process handling match the offline
/// compatibility wrapper `launch_version_in_directory`.
pub fn launch_authenticated(
    paths: &AppPaths,
    version: &str,
    game_dir: &Path,
    account: &crate::auth::Account,
) -> Result<()> {
    if account.is_expired() {
        return Err(FerriteError::AuthenticationExpired);
    }
    let placeholders = auth_placeholders(
        &account.name,
        &account.uuid,
        &account.access_token,
        &account.client_id,
        &account.xuid,
        "msa",
    );
    launch_with_auth(paths, version, game_dir, placeholders, false, None)
}

/// Authenticated launch using an explicit maximum Java heap size.
pub fn launch_authenticated_with_memory(
    paths: &AppPaths,
    version: &str,
    game_dir: &Path,
    account: &crate::auth::Account,
    memory_mb: u32,
) -> Result<()> {
    if account.is_expired() {
        return Err(FerriteError::AuthenticationExpired);
    }
    let placeholders = auth_placeholders(
        &account.name,
        &account.uuid,
        &account.access_token,
        &account.client_id,
        &account.xuid,
        "msa",
    );
    launch_with_auth(
        paths,
        version,
        game_dir,
        placeholders,
        false,
        Some(memory_mb),
    )
}

// Launch preparation is deliberately centralized so authenticated and offline
// entry points cannot diverge. It rejects a live process and incomplete install,
// loads the installed JSON, validates Java, builds an OS-filtered classpath,
// fills authentication/filesystem placeholders, then transfers ownership of the
// spawned `Child` into the global process slot. Errors before `spawn` leave the
// slot untouched; a successful spawn is never silently downgraded to offline.
fn launch_with_auth(
    paths: &AppPaths,
    version: &str,
    game_dir: &Path,
    mut placeholders: HashMap<String, String>,
    offline: bool,
    memory_mb: Option<u32>,
) -> Result<()> {
    if is_running() {
        return Err(FerriteError::AlreadyRunning);
    }
    if !is_version_installed(paths, version) {
        return Err(FerriteError::NotInstalled);
    }

    let v_dir = paths.version_dir(version);
    let metadata_text = fs::read_to_string(v_dir.join(format!("{version}.json")))?;
    let metadata: VersionMetadata = serde_json::from_str(&metadata_text)?;

    // Legacy metadata predates the `javaVersion` field entirely, so fall
    // back to a safe default (Java 8, which every legacy-era version
    // runs on) rather than treating its absence as an error.
    let required_java = metadata
        .java_version
        .as_ref()
        .map(|v| v.major_version)
        .unwrap_or(LEGACY_DEFAULT_JAVA_MAJOR);
    check_java_version(required_java)?;

    let libs_dir = paths.libraries_dir();
    let natives = paths.natives_dir(version);
    let client_jar = v_dir.join("client.jar");

    let assets = paths.assets_dir();

    // 26.2 JVM args point at natives_directory/{java,jna,lwjgl,netty}.
    // LWJGL and jtracy unpack their own .so files into those dirs at runtime.
    ensure_native_workdirs(&natives)?;

    // Every allowed artifact goes on the classpath — including 1.19+ native
    // jars named `…:natives-<os>`. Those jars still have a `downloads.artifact`
    // (they are NOT the old `classifiers` format). LWJGL 3 and jtracy load
    // natives from the classpath themselves; stripping them here is what
    // produced UnsatisfiedLinkError.
    let mut classpath_paths: Vec<PathBuf> = metadata
        .libraries
        .iter()
        .filter(|lib| rules_allow(&lib.rules))
        .filter_map(|lib| lib.downloads.artifact.as_ref())
        .map(|artifact| libs_dir.join(&artifact.path))
        .collect();
    classpath_paths.push(client_jar);

    let mut classpath_absolute = Vec::with_capacity(classpath_paths.len());
    for path in &classpath_paths {
        classpath_absolute.push(abs(path)?.to_string_lossy().to_string());
    }
    let classpath = classpath_absolute.join(classpath_separator());

    if offline {
        println!("⚠️  Launching with explicit offline placeholder credentials.");
    }

    // Pre-1.7-ish clients that use a "virtual" asset index read assets
    // straight off disk by their original filename from
    // assets/virtual/<index-id>/... (populated by download_assets)
    // instead of going through the hashed object store. If that
    // directory exists for this version's asset index, legacy
    // `${game_assets}` should point there; otherwise it's the same as
    // the modern assets root.
    let legacy_assets_root = {
        let virtual_candidate = assets.join("virtual").join(&metadata.assets);
        if virtual_candidate.is_dir() {
            virtual_candidate
        } else {
            assets.clone()
        }
    };

    placeholders.insert("version_name".to_string(), metadata.id.clone());

    placeholders.insert(
        "assets_root".to_string(),
        abs(&assets)?.to_string_lossy().to_string(),
    );
    placeholders.insert("assets_index_name".to_string(), metadata.assets.clone());

    placeholders.insert("version_type".to_string(), metadata.version_type.clone());
    placeholders.insert(
        "natives_directory".to_string(),
        abs(&natives)?.to_string_lossy().to_string(),
    );
    placeholders.insert(
        "library_directory".to_string(),
        abs(&libs_dir)?.to_string_lossy().to_string(),
    );
    placeholders.insert("launcher_name".to_string(), "ferrite-launcher".to_string());
    placeholders.insert(
        "launcher_version".to_string(),
        env!("CARGO_PKG_VERSION").to_string(),
    );
    placeholders.insert("classpath".to_string(), classpath);
    // Legacy-only placeholders. Harmless to insert unconditionally: they
    // simply go unused by modern `${...}` templates that never reference
    // them.

    placeholders.insert("user_properties".to_string(), "{}".to_string());
    placeholders.insert(
        "game_assets".to_string(),
        abs(&legacy_assets_root)?.to_string_lossy().to_string(),
    );

    let mut command = game_command(&metadata, placeholders, game_dir, memory_mb)?;

    // Never log the command, resolved arguments, or placeholders: both modern
    // access tokens and legacy sessions contain credentials (including JVM args).
    println!("Launching Minecraft {}...", metadata.id);
    let child = command.spawn()?;

    *process_slot().lock().unwrap() = Some(child);
    Ok(())
}

// Keep argument substitution and the child's working directory tied to the
// same absolute path, without changing the launcher's own working directory.
fn game_command(
    metadata: &VersionMetadata,
    mut placeholders: HashMap<String, String>,
    game_dir: &Path,
    memory_mb: Option<u32>,
) -> Result<Command> {
    fs::create_dir_all(game_dir)?;
    let game_dir = abs(game_dir)?;
    placeholders.insert(
        "game_directory".to_string(),
        game_dir.to_string_lossy().to_string(),
    );
    let mut command = Command::new("java");
    if let Some(memory_mb) = memory_mb {
        command.arg(format!("-Xmx{memory_mb}M"));
    }
    command
        .args(resolve_launch_arguments(metadata, &placeholders))
        .current_dir(game_dir);
    Ok(command)
}

// `OnceLock` lazily creates one slot for the whole process. `Mutex` is sufficient
// because the slot itself has static shared ownership; an `Arc` is unnecessary
// unless the slot must be passed outside this module. Lock poisoning currently
// propagates as a panic through `unwrap`. The check in `launch_with_auth` and the
// later store are separate lock acquisitions, so this is process management for
// normal launcher use rather than a claim of atomic concurrent launch admission.
static RUNNING_PROCESS: OnceLock<Mutex<Option<Child>>> = OnceLock::new();

fn process_slot() -> &'static Mutex<Option<Child>> {
    RUNNING_PROCESS.get_or_init(|| Mutex::new(None))
}

/// Returns whether this Ferrite process owns a child that has not exited.
///
/// `try_wait` is non-blocking. An observed exit clears and drops the stored
/// handle. A polling error is treated as “not running” but leaves the handle in
/// the slot so [`kill`] can still attempt cleanup. This does not detect games
/// launched by another launcher process.
pub fn is_running() -> bool {
    let mut slot = process_slot().lock().unwrap();
    match slot.as_mut() {
        Some(child) => match child.try_wait() {
            Ok(Some(_status)) => {
                *slot = None; // it already exited on its own
                false
            }
            Ok(None) => true,
            Err(_) => false,
        },
        None => false,
    }
}

/// Forcibly kills and reaps the child owned by this launcher, if any.
///
/// The handle is removed from the global slot before signaling it. A kill error
/// is returned with the slot already empty; errors from the subsequent blocking
/// `wait` are intentionally ignored. Calling this with no stored child succeeds.
pub fn kill() -> Result<()> {
    let mut slot = process_slot().lock().unwrap();
    if let Some(mut child) = slot.take() {
        child.kill()?;
        let _ = child.wait(); // reap it so it doesn't linger as a zombie
    }
    Ok(())
}

/// Returns `true` if the given Minecraft version is already installed —
/// i.e. its metadata JSON and client jar are both present on disk.
///
/// As with `launch_version`, `version` may be a synthetic mod-loader id.
pub fn is_version_installed(paths: &AppPaths, version: &str) -> bool {
    let v_dir = paths.version_dir(version);
    v_dir.join(format!("{version}.json")).exists() && v_dir.join("client.jar").exists()
}

/// Copies an already-installed version's extracted native libraries
/// into another version's natives directory. Mod loaders that build a
/// synthetic vanilla-shaped version (Fabric, Quilt, Forge, NeoForge)
/// need this: `launch_version` looks for natives under
/// `natives/<that version's own id>`, but extraction only ever happens
/// for the underlying vanilla version during `install_version`.
/// Without this, `-Djava.library.path` points at a directory that was
/// never created.
pub(crate) fn copy_natives(paths: &AppPaths, from_version: &str, to_version: &str) -> Result<()> {
    let src = paths.natives_dir(from_version);
    let dst = paths.natives_dir(to_version);
    fs::create_dir_all(&dst)?;

    if src.exists() {
        copy_natives_tree(&src, &dst)?;
    }

    // Modern JVM args also expect these workdirs even when vanilla
    // extracted no files at install time.
    ensure_native_workdirs(&dst)?;
    Ok(())
}

fn copy_natives_tree(src: &Path, dst: &Path) -> Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let dest = dst.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_natives_tree(&entry.path(), &dest)?;
        } else if entry.file_type()?.is_file() {
            fs::copy(entry.path(), dest)?;
        }
    }
    Ok(())
}

// =====================================================================
// Filesystem layout
// =====================================================================

// The layout (`versions/<id>`, `libraries/`, `assets/`, `natives/<id>`) is owned by
// `AppPaths`; Forge and NeoForge installers receive `AppPaths::storage_root()` so they
// write the same layout as the official launcher.

fn abs(path: &Path) -> Result<PathBuf> {
    Ok(fs::canonicalize(path)?)
}

// =====================================================================
// Mojang JSON shapes — only the fields we actually use.
// =====================================================================

const VERSION_MANIFEST_URL: &str =
    "https://piston-meta.mojang.com/mc/game/version_manifest_v2.json";

/// Java major version assumed for metadata that predates the
/// `javaVersion` field (everything before roughly 1.17). Every
/// legacy-era release runs fine on Java 8.
const LEGACY_DEFAULT_JAVA_MAJOR: u32 = 8;

#[derive(Deserialize)]
struct Manifest {
    versions: Vec<VersionEntry>,
}

#[derive(Deserialize)]
struct VersionEntry {
    id: String,
    url: String,

    #[serde(rename = "type")]
    version_type: String,
}

#[derive(Deserialize)]
struct VersionMetadata {
    id: String,
    #[serde(rename = "type")]
    version_type: String,
    #[serde(rename = "mainClass")]
    main_class: String,
    assets: String,
    #[serde(rename = "assetIndex")]
    asset_index: AssetIndexRef,
    downloads: Downloads,
    libraries: Vec<LibraryEntry>,
    /// Modern (1.13+) shape: structured, rule-gated JVM + game argument
    /// lists. Absent on legacy metadata.
    #[serde(default)]
    arguments: Option<Arguments>,
    /// Legacy (pre-1.13) shape: one space-separated string of game
    /// arguments, no rules, no separate JVM list. Absent on modern
    /// metadata.
    #[serde(default, rename = "minecraftArguments")]
    minecraft_arguments: Option<String>,
    /// Absent on legacy metadata (added for 1.17). See
    /// `LEGACY_DEFAULT_JAVA_MAJOR` for the fallback.
    #[serde(default, rename = "javaVersion")]
    java_version: Option<JavaVersionReq>,
}

#[derive(Deserialize)]
struct JavaVersionReq {
    #[serde(rename = "majorVersion")]
    major_version: u32,
}

#[derive(Deserialize)]
struct AssetIndexRef {
    url: String,
}

#[derive(Deserialize)]
struct Downloads {
    client: DownloadInfo,
}

#[derive(Deserialize)]
struct DownloadInfo {
    url: String,
    size: u64,
}

#[derive(Deserialize)]
struct LibraryEntry {
    name: String,
    downloads: LibraryDownloads,
    /// Pre-1.19 format: maps Mojang OS names (`linux`/`windows`/`osx`) to
    /// classifier keys such as `natives-linux`. Absent on 26.2, which lists
    /// each platform as its own library named `…:natives-<os>` instead.
    #[serde(default)]
    natives: Option<HashMap<String, String>>,
    #[serde(default)]
    extract: Option<ExtractRules>,
    #[serde(default)]
    rules: Vec<Rule>,
}

#[derive(Deserialize)]
struct LibraryDownloads {
    #[serde(default)]
    artifact: Option<Artifact>,
    /// Pre-1.19 format only. 26.2 native jars live in `artifact`, not here.
    #[serde(default)]
    classifiers: Option<HashMap<String, Artifact>>,
}

#[derive(Deserialize)]
struct ExtractRules {
    #[serde(default)]
    exclude: Vec<String>,
}

#[derive(Deserialize)]
struct Artifact {
    path: String,
    url: String,
    size: u64,
}

#[derive(Deserialize)]
struct Rule {
    action: String,
    #[serde(default)]
    os: Option<OsRule>,
    #[serde(default)]
    features: Option<serde_json::Value>,
}

#[derive(Deserialize)]
struct OsRule {
    #[serde(default)]
    name: Option<String>,
}

#[derive(Deserialize)]
struct Arguments {
    game: Vec<ArgumentEntry>,
    jvm: Vec<ArgumentEntry>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum ArgumentEntry {
    Plain(String),
    Conditional {
        #[serde(default)]
        rules: Vec<Rule>,
        value: ArgValue,
    },
}

#[derive(Deserialize)]
#[serde(untagged)]
enum ArgValue {
    One(String),
    Many(Vec<String>),
}

#[derive(Deserialize)]
struct AssetIndexFile {
    objects: HashMap<String, AssetObject>,
    /// Legacy indexes (pre-1.7-ish) set this to `true`, meaning the
    /// client reads assets off disk by their original name rather than
    /// through the hashed object store. See `download_assets`.
    #[serde(default, rename = "virtual")]
    is_virtual: bool,
}

#[derive(Deserialize)]
struct AssetObject {
    hash: String,
    size: u64,
}

// =====================================================================
// Install pipeline (private)
// =====================================================================

/// Fetches the manifest without a disk fallback; transport, status, and parse
/// failures are returned to the caller.
fn fetch_manifest(client: &Client) -> Result<Manifest> {
    let text = client
        .get(VERSION_MANIFEST_URL)
        .send()?
        .error_for_status()?
        .text()?;
    Ok(serde_json::from_str(&text)?)
}

fn fetch_version_metadata(client: &Client, url: &str) -> Result<(VersionMetadata, String)> {
    let text = client.get(url).send()?.error_for_status()?.text()?;
    let metadata = serde_json::from_str(&text)?;
    Ok((metadata, text))
}

/// Downloads `url` into `dest` and replaces any existing file.
///
/// The caller must create the destination's parent directory. HTTP and write
/// failures are fatal, but an `expected_size` mismatch only emits a warning;
/// hashes are not verified. Passing `None` disables the size check, as required
/// for loader repositories that do not publish sizes in profile metadata.
pub(crate) fn download_file(
    client: &Client,
    url: &str,
    dest: &Path,
    expected_size: Option<u64>,
) -> Result<()> {
    let bytes = client.get(url).send()?.error_for_status()?.bytes()?;
    if let Some(expected) = expected_size {
        if bytes.len() as u64 != expected {
            eprintln!(
                "⚠️  warning: {url} downloaded as {} bytes, expected {expected}",
                bytes.len()
            );
        }
    }
    fs::write(dest, &bytes)?;
    Ok(())
}

// Walk metadata in order, applying Mojang's rules before touching a library.
// Regular artifacts are cached in the shared Maven tree. Legacy classifier-based
// native jars are cached there too, then extracted on every install so a missing
// or partial per-version native directory is repaired; modern native artifacts
// remain packed on the classpath for their libraries to unpack at runtime.
fn download_libraries(
    client: &Client,
    libraries: &[LibraryEntry],
    libs_dir: &Path,
    natives_dir: &Path,
) -> Result<()> {
    fs::create_dir_all(libs_dir)?;
    ensure_native_workdirs(natives_dir)?;

    for lib in libraries {
        if !rules_allow(&lib.rules) {
            continue; // e.g. a Windows-only native lib, and we're on Linux
        }

        // Regular classpath jar — this includes 1.19+ `…:natives-<os>` jars.
        // Those stay packed; LWJGL 3 / jtracy unpack them at runtime.
        if let Some(artifact) = &lib.downloads.artifact {
            let dest = libs_dir.join(&artifact.path);
            if let Some(parent) = dest.parent() {
                fs::create_dir_all(parent)?;
            }
            if !dest.exists() {
                download_file(client, &artifact.url, &dest, Some(artifact.size))?;
                println!("  library: {}", lib.name);
            }
        }

        // Pre-1.19 format: natives live under `downloads.classifiers` and
        // must be extracted by the launcher into `natives_dir`.
        if let Some(classifier_key) = native_classifier_for_current_os(lib) {
            if let Some(artifact) = lib
                .downloads
                .classifiers
                .as_ref()
                .and_then(|c| c.get(&classifier_key))
            {
                let dest = libs_dir.join(&artifact.path);
                if let Some(parent) = dest.parent() {
                    fs::create_dir_all(parent)?;
                }
                if !dest.exists() {
                    download_file(client, &artifact.url, &dest, Some(artifact.size))?;
                }

                let default_exclude = ["META-INF/".to_string()];
                let exclude: &[String] = lib
                    .extract
                    .as_ref()
                    .map(|e| e.exclude.as_slice())
                    .unwrap_or(default_exclude.as_slice());

                extract_natives(&dest, natives_dir, exclude)?;
                println!("  extracted natives: {}", artifact.path);
            }
        }
    }

    Ok(())
}

/// If this library ships a native (platform-specific) jar for the OS
/// we're running on via the pre-1.19 `natives`/`classifiers` object,
/// returns the classifier key to fetch it under (e.g. `"natives-linux"`).
/// Some very old libraries embed `"${arch}"` in the classifier name for
/// 32/64-bit variants, so that gets resolved too.
///
/// 26.2 does not use this field at all — its natives are separate library
/// entries named `org.lwjgl:lwjgl:3.4.1:natives-linux` with a normal
/// `downloads.artifact`.
fn native_classifier_for_current_os(lib: &LibraryEntry) -> Option<String> {
    let raw = lib.natives.as_ref()?.get(current_os_name())?;
    let arch = if cfg!(target_pointer_width = "64") {
        "64"
    } else {
        "32"
    };
    Some(raw.replace("${arch}", arch))
}

/// Extracts files from a legacy native JAR, preserving paths except for archive
/// directories, `META-INF/`, and metadata-specified excluded prefixes. Existing
/// destination files are replaced, allowing reinstalls to repair extracted data.
fn extract_natives(jar_path: &Path, dest_dir: &Path, exclude: &[String]) -> Result<()> {
    let file = fs::File::open(jar_path)?;
    let mut archive = ZipArchive::new(file)?;

    for i in 0..archive.len() {
        let mut entry = archive.by_index(i)?;
        let name = entry.name().to_string();

        if name.ends_with('/')
            || name.starts_with("META-INF/")
            || exclude
                .iter()
                .any(|prefix| name.starts_with(prefix.as_str()))
        {
            continue;
        }

        let out_path = dest_dir.join(&name);
        if let Some(parent) = out_path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut out_file = fs::File::create(&out_path)?;
        std::io::copy(&mut entry, &mut out_file)?;
    }

    Ok(())
}

/// 26.2 (and other recent versions) pass these subdirectories to the JVM:
/// `-Djava.library.path=${natives_directory}/java`,
/// `-Djna.tmpdir=${natives_directory}/jna`,
/// `-Dorg.lwjgl.system.SharedLibraryExtractPath=${natives_directory}/lwjgl`,
/// `-Dio.netty.native.workdir=${natives_directory}/netty`.
fn ensure_native_workdirs(natives_dir: &Path) -> Result<()> {
    fs::create_dir_all(natives_dir)?;
    for sub in ["java", "jna", "lwjgl", "netty"] {
        fs::create_dir_all(natives_dir.join(sub))?;
    }
    Ok(())
}

// Refresh the named index, then materialize each object at
// `objects/<first-two-hash-chars>/<full-hash>`. Existing objects are trusted by
// path and skipped. For a legacy virtual index, every object is additionally
// copied to its original logical path under `virtual/<index>/`; those copies are
// also skipped when present. Any network, parse, or filesystem error aborts the
// remaining iteration while preserving completed cache entries.
fn download_assets(
    client: &Client,
    index_url: &str,
    index_name: &str,
    assets_dir: &Path,
) -> Result<()> {
    let indexes_dir = assets_dir.join("indexes");
    fs::create_dir_all(&indexes_dir)?;

    let text = client.get(index_url).send()?.error_for_status()?.text()?;
    fs::write(indexes_dir.join(format!("{index_name}.json")), &text)?;

    let index: AssetIndexFile = serde_json::from_str(&text)?;
    let objects_dir = assets_dir.join("objects");
    fs::create_dir_all(&objects_dir)?;

    // Legacy ("virtual") indexes are additionally mirrored under
    // assets/virtual/<index_name>/<original-path>, using each object's
    // real filename instead of its hash — that's how pre-1.7-ish clients
    // expect to find them. Modern clients never look here.
    let virtual_dir = if index.is_virtual {
        let dir = assets_dir.join("virtual").join(index_name);
        fs::create_dir_all(&dir)?;
        Some(dir)
    } else {
        None
    };

    let total = index.objects.len();
    for (i, (path, object)) in index.objects.iter().enumerate() {
        let prefix = &object.hash[0..2];
        let dir = objects_dir.join(prefix);
        fs::create_dir_all(&dir)?;

        let dest = dir.join(&object.hash);
        if !dest.exists() {
            let url = format!(
                "https://resources.download.minecraft.net/{prefix}/{}",
                object.hash
            );
            download_file(client, &url, &dest, Some(object.size))?;
        }

        if let Some(virtual_root) = &virtual_dir {
            let virtual_dest = virtual_root.join(path);
            if !virtual_dest.exists() {
                if let Some(parent) = virtual_dest.parent() {
                    fs::create_dir_all(parent)?;
                }
                fs::copy(&dest, &virtual_dest)?;
            }
        }

        if (i + 1) % 200 == 0 {
            println!("  downloaded {}/{total} assets...", i + 1);
        }
    }
    println!("  downloaded {total} assets total.");

    Ok(())
}

// =====================================================================
// Rule evaluation (shared by libraries and launch arguments)
// =====================================================================

fn current_os_name() -> &'static str {
    match std::env::consts::OS {
        "macos" => "osx", // Mojang calls it "osx", Rust calls it "macos"
        other => other,   // "windows" and "linux" already match
    }
}

fn rule_matches(rule: &Rule) -> bool {
    // We don't support any optional features yet (demo mode, custom
    // resolution, quick play, etc.), so any feature-gated rule never
    // matches — the argument it guards is simply omitted.
    if rule.features.is_some() {
        return false;
    }
    if let Some(os) = &rule.os {
        if let Some(name) = &os.name {
            if name != current_os_name() {
                return false;
            }
        }
    }
    true
}

/// Mirrors Mojang's own rule evaluation: default to disallowed, then
/// apply each matching rule in order (last match wins). Most real
/// entries start with an unconditional "allow" followed by an
/// OS-specific "disallow", which this correctly handles.
///
/// An empty rule list (the case for every mod-loader library, which
/// never carries OS/feature gating) is always allowed.
fn rules_allow(rules: &[Rule]) -> bool {
    if rules.is_empty() {
        return true;
    }
    let mut allowed = false;
    for rule in rules {
        if rule_matches(rule) {
            allowed = rule.action == "allow";
        }
    }
    allowed
}

// =====================================================================
// Java version check
// =====================================================================

fn detect_java_major_version() -> Option<u32> {
    // `java -version` prints to stderr, e.g.: openjdk version "21.0.3" 2024-04-16
    let output = std::process::Command::new("java")
        .arg("-version")
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&output.stderr);
    let quoted = text.split('"').nth(1)?; // grabs "21.0.3"
    let major_str = quoted.split('.').next()?;
    // Old versions look like "1.8.0" — Java 8 reports major as "1"
    let major: u32 = major_str.parse().ok()?;
    if major == 1 {
        quoted.split('.').nth(1)?.parse().ok()
    } else {
        Some(major)
    }
}

fn check_java_version(required: u32) -> Result<()> {
    let found = detect_java_major_version();
    match found {
        Some(v) if v >= required => Ok(()),
        other => Err(FerriteError::JavaVersionMismatch {
            required,
            found: other,
        }),
    }
}

// =====================================================================
// Launch argument templating
// =====================================================================

fn classpath_separator() -> &'static str {
    if cfg!(target_os = "windows") {
        ";"
    } else {
        ":"
    }
}

/// Replaces every known `${name}` occurrence without shell parsing or escaping.
/// Unknown placeholders remain literal so metadata can pass them through rather
/// than being silently erased. Each returned argument is later passed directly
/// to `Command`, so spaces in replacement values remain part of one argument.
fn substitute(template: &str, placeholders: &HashMap<String, String>) -> String {
    let mut result = template.to_string();
    for (key, value) in placeholders {
        result = result.replace(&format!("${{{key}}}"), value);
    }
    result
}

/// Flattens Mojang's plain and conditional argument entries in source order,
/// omitting rule-disallowed entries and substituting each emitted string.
fn resolve_arguments(
    entries: &[ArgumentEntry],
    placeholders: &HashMap<String, String>,
) -> Vec<String> {
    let mut resolved = Vec::new();
    for entry in entries {
        match entry {
            ArgumentEntry::Plain(value) => resolved.push(substitute(value, placeholders)),
            ArgumentEntry::Conditional { rules, value } => {
                if !rules_allow(rules) {
                    continue;
                }
                match value {
                    ArgValue::One(v) => resolved.push(substitute(v, placeholders)),
                    ArgValue::Many(values) => {
                        for v in values {
                            resolved.push(substitute(v, placeholders));
                        }
                    }
                }
            }
        }
    }
    resolved
}

/// Builds the final `java ...` argument vector (JVM args, then main
/// class, then game args), handling both metadata shapes.
///
/// Modern metadata supplies its own rule-gated JVM argument list, which
/// we resolve as-is — this is also the path every mod-loader version
/// takes, since a Fabric/Forge/etc. synthetic version's metadata reuses
/// its parent vanilla version's `arguments.jvm` verbatim (see
/// `crate::loaders::fabric`). Legacy metadata has no JVM list at all —
/// Mojang's old launcher hard-coded the classpath/native-library-path
/// invocation — so we synthesize the equivalent here before falling
/// back to splitting `minecraftArguments` on whitespace for the game
/// args (that string has no rule syntax; every token is unconditional).
fn resolve_launch_arguments(
    metadata: &VersionMetadata,
    placeholders: &HashMap<String, String>,
) -> Vec<String> {
    if let Some(arguments) = &metadata.arguments {
        let mut args = resolve_arguments(&arguments.jvm, placeholders);
        args.push(metadata.main_class.clone());
        args.extend(resolve_arguments(&arguments.game, placeholders));
        args
    } else {
        let mut args = vec![
            format!(
                "-Djava.library.path={}",
                placeholders
                    .get("natives_directory")
                    .cloned()
                    .unwrap_or_default()
            ),
            "-cp".to_string(),
            placeholders.get("classpath").cloned().unwrap_or_default(),
        ];
        args.push(metadata.main_class.clone());

        let raw = metadata.minecraft_arguments.as_deref().unwrap_or_default();
        args.extend(
            raw.split_whitespace()
                .map(|token| substitute(token, placeholders)),
        );
        args
    }
}

// =====================================================================
// Authentication argument construction (never log these maps)
// =====================================================================

/// Builds owned placeholder values so the account borrows need only last for
/// this call. The resulting map contains credentials and must never be logged.
fn auth_placeholders(
    player_name: &str,
    uuid: &str,
    access_token: &str,
    client_id: &str,
    xuid: &str,
    user_type: &str,
) -> HashMap<String, String> {
    HashMap::from([
        ("auth_player_name".into(), player_name.into()),
        ("auth_uuid".into(), uuid.into()),
        ("auth_access_token".into(), access_token.into()),
        ("clientid".into(), client_id.into()),
        ("auth_xuid".into(), xuid.into()),
        ("user_type".into(), user_type.into()),
        (
            "auth_session".into(),
            format!("token:{access_token}:{uuid}"),
        ),
    ])
}

/// Only the explicit offline compatibility APIs use these fake credentials.
fn offline_auth_placeholders() -> HashMap<String, String> {
    auth_placeholders(
        "Player",
        "00000000-0000-0000-0000-000000000000",
        "0",
        "",
        "",
        "legacy",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn metadata_with_game_dir_argument() -> VersionMetadata {
        serde_json::from_str(
            r#"{
                "id": "test",
                "type": "release",
                "mainClass": "example.Main",
                "assets": "test-assets",
                "assetIndex": { "url": "https://example.invalid/assets.json" },
                "downloads": {
                    "client": { "url": "https://example.invalid/client.jar", "size": 0 }
                },
                "libraries": [],
                "arguments": {
                    "jvm": ["-cp", "${classpath}"],
                    "game": ["--gameDir", "${game_directory}"]
                }
            }"#,
        )
        .unwrap()
    }

    #[test]
    fn authenticated_placeholders_resolve_modern_and_legacy_arguments() {
        // Synthetic values only; no account construction or network access needed.
        let placeholders = auth_placeholders(
            "TestPlayer",
            "test-uuid",
            "fake-token",
            "test-client",
            "12345",
            "msa",
        );
        let templates = [
            "${auth_player_name}",
            "${auth_uuid}",
            "${auth_access_token}",
            "${clientid}",
            "${auth_xuid}",
            "${user_type}",
            "${auth_session}",
        ];
        let expected = vec![
            "TestPlayer",
            "test-uuid",
            "fake-token",
            "test-client",
            "12345",
            "msa",
            "token:fake-token:test-uuid",
        ];
        let mut metadata = metadata_with_game_dir_argument();
        metadata.arguments = Some(Arguments {
            jvm: vec![ArgumentEntry::Plain("-Dsession=${auth_session}".into())],
            game: templates
                .iter()
                .map(|s| ArgumentEntry::Plain((*s).into()))
                .collect(),
        });
        let modern = resolve_launch_arguments(&metadata, &placeholders);
        assert_eq!(modern[0], "-Dsession=token:fake-token:test-uuid");
        assert_eq!(&modern[2..], expected.as_slice());

        metadata.arguments = None;
        metadata.minecraft_arguments = Some(templates.join(" "));
        let legacy = resolve_launch_arguments(&metadata, &placeholders);
        assert_eq!(&legacy[4..], expected.as_slice());
    }

    #[test]
    fn offline_compatibility_placeholders_remain_unchanged() {
        let placeholders = offline_auth_placeholders();
        assert_eq!(placeholders["auth_player_name"], "Player");
        assert_eq!(
            placeholders["auth_uuid"],
            "00000000-0000-0000-0000-000000000000"
        );
        assert_eq!(placeholders["auth_access_token"], "0");
        assert_eq!(placeholders["clientid"], "");
        assert_eq!(placeholders["auth_xuid"], "");
        assert_eq!(placeholders["user_type"], "legacy");
        assert_eq!(
            placeholders["auth_session"],
            "token:0:00000000-0000-0000-0000-000000000000"
        );
    }

    #[test]
    fn game_command_uses_explicit_directory_for_argument_and_cwd() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let game_dir = std::env::temp_dir().join(format!(
            "ferrite-launcher-game-dir-{}-{unique}",
            std::process::id()
        ));
        let metadata = metadata_with_game_dir_argument();
        let mut placeholders = HashMap::new();
        placeholders.insert(
            "classpath".to_string(),
            "/shared/libraries/client.jar".to_string(),
        );

        let command = game_command(&metadata, placeholders, &game_dir, Some(6144)).unwrap();
        let canonical_game_dir = fs::canonicalize(&game_dir).unwrap();
        let args: Vec<String> = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();

        assert_eq!(
            command.get_current_dir(),
            Some(canonical_game_dir.as_path())
        );
        assert_eq!(
            args,
            vec![
                "-Xmx6144M",
                "-cp",
                "/shared/libraries/client.jar",
                "example.Main",
                "--gameDir",
                canonical_game_dir.to_string_lossy().as_ref(),
            ]
        );

        fs::remove_dir(&game_dir).unwrap();
    }
}
