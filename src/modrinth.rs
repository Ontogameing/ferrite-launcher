//! Blocking, deliberately small Modrinth v2 API integration.
//!
//! The public response types mirror only fields consumed by the launcher. Serde ignores
//! additional API fields, while `#[serde(default)]` keeps optional/omitted fields from
//! making otherwise useful responses undecodable. Every network entry point constructs
//! the same HTTPS-only client and returns display-ready `String` errors with endpoint or
//! filesystem context. Calls are synchronous, so UI callers must move them to a worker
//! thread.
//!
//! `search` returns the first page (20 hits). `install` selects the first compatible
//! version in Modrinth's newest-first response, including prereleases, and installs
//! exactly one primary JAR per project plus all required dependencies. Optional,
//! embedded and incompatible dependency entries are not acted on. Exact dependency
//! pins must match the requested Minecraft version and loader; conflicting pins
//! fail rather than running a version-constraint solver. Loader names are exact
//! Modrinth slugs (e.g. `fabric`, `forge`, `neoforge`, `quilt`).
//!
//! `search_filtered` supports offset pagination; `details` loads project metadata,
//! all available version summaries, and team members.
//!
//! Limitations: no retries, cancellation, progress, cryptographic hash
//! verification, update/removal of older JARs, or installed-mod conflict detection.
//! Resolution is bounded to 256 projects. Downloads check the declared byte size.
//! Installation is atomic per file, not transactional across the dependency graph:
//! an error may leave already completed files, and a process crash may leave a
//! hidden staging directory. Identical existing files are reused; different files
//! and symlinks are rejected. Staged files are renamed before being published via
//! an atomic no-clobber hard link (plain rename would overwrite on Unix). The mods
//! filesystem must support hard links. The game directory and its ancestors must
//! be trusted, without concurrent hostile filesystem mutation. No other launcher
//! files need to be referenced by this module; declare `mod modrinth;` separately
//! when integrating it into the application.

use reqwest::blocking::Client;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

const API: &str = "https://api.modrinth.com/v2/";
const MAX_PROJECTS: usize = 256;
static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

/// A compact project record returned by Modrinth's search endpoint.
///
/// Unlike [`Project`], this is search-index data rather than authoritative project
/// metadata. Optional fields may be absent as the API/search index evolves; callers that
/// need the full description, team, or complete release list should use [`details`].
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct SearchHit {
    pub title: String,
    pub description: String,
    pub downloads: u64,
    pub project_id: String,
    #[serde(default)]
    pub icon_url: Option<String>,
    #[serde(default)]
    pub author: Option<String>,
    #[serde(default)]
    pub versions: Option<Vec<String>>,
    #[serde(default)]
    pub categories: Option<Vec<String>>,
}

/// Browser filters. `None` omits a facet; strings are exact Modrinth values.
/// Defaults search mods by relevance, with no compatibility or side restrictions.
/// Side values are `required`, `optional`, `unsupported`, or `unknown`.
/// Sort values are `relevance`, `downloads`, `follows`, `newest`, or `updated`.
/// Each category is ANDed with every other category and filter (not ORed).
/// Facets are JSON arrays of singleton arrays, then URL-encoded by reqwest;
/// query text and facet values must not be pre-escaped. Pages contain 20 hits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchFilters {
    pub query: String,
    pub game_version: Option<String>,
    pub loader: Option<String>,
    pub client_side: Option<String>,
    pub server_side: Option<String>,
    pub categories: Vec<String>,
    pub project_type: Option<String>,
    pub sort: String,
    pub offset: u64,
}

impl Default for SearchFilters {
    fn default() -> Self {
        Self {
            query: String::new(),
            game_version: None,
            loader: None,
            client_side: None,
            server_side: None,
            categories: Vec::new(),
            project_type: Some("mod".into()),
            sort: "relevance".into(),
            offset: 0,
        }
    }
}

/// One page of search results plus the server-reported pagination bounds.
///
/// `offset` and `limit` describe this response; `total_hits` allows callers to decide
/// whether requesting a later page through [`search_filtered`] is useful.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct SearchResponse {
    pub hits: Vec<SearchHit>,
    pub offset: u64,
    pub limit: u64,
    pub total_hits: u64,
}

/// Search one 20-hit page using the documented facet and pagination rules.
/// Invalid Modrinth filter values are reported by the API, not silently changed.
pub fn search_filtered(filters: &SearchFilters) -> Result<SearchResponse, String> {
    Api::new()?.get("search", &filtered_search_params(filters))
}

/// Full project metadata returned by `/project/{id}`.
///
/// `description` is the short summary while `body` is Modrinth's Markdown description.
/// Loader, game-version, and side fields describe project-level support and do not choose
/// an installable release; installation performs release-level compatibility checks.
#[derive(Debug, Clone, Deserialize)]
pub struct Project {
    pub id: String,
    pub slug: String,
    pub title: String,
    pub description: String,
    pub body: String,
    #[serde(default)]
    pub icon_url: Option<String>,
    pub team: String,
    pub downloads: u64,
    pub game_versions: Vec<String>,
    pub loaders: Vec<String>,
    pub client_side: String,
    pub server_side: String,
    #[serde(default)]
    pub source_url: Option<String>,
    #[serde(default)]
    pub issues_url: Option<String>,
}

/// Display metadata for a release in the unfiltered project-details version list.
///
/// This intentionally omits download-file data used by installation. Dependency entries
/// are exposed for the details UI only; [`install`] resolves its own fresh internal models
/// so it can validate pins, files, and compatibility before writing anything.
#[derive(Debug, Clone, Deserialize)]
pub struct VersionSummary {
    pub id: String,
    pub project_id: String,
    pub name: String,
    pub version_number: String,
    pub version_type: String,
    pub date_published: String,
    pub downloads: u64,
    pub game_versions: Vec<String>,
    pub loaders: Vec<String>,
    #[serde(default)]
    pub changelog: Option<String>,
    #[serde(default)]
    pub dependencies: Vec<DependencySummary>,
}

/// Dependency metadata as shown for a release.
///
/// Modrinth may identify a dependency by project, exact version, or only a filename.
/// Presence here does not mean this module will install it: installation follows only
/// `required` entries and requires a usable project or version identifier.
#[derive(Debug, Clone, Deserialize)]
pub struct DependencySummary {
    #[serde(default)]
    pub version_id: Option<String>,
    #[serde(default)]
    pub project_id: Option<String>,
    #[serde(default)]
    pub file_name: Option<String>,
    pub dependency_type: String,
}

/// Public user fields embedded in a Modrinth team-members response.
#[derive(Debug, Clone, Deserialize)]
pub struct TeamUser {
    pub id: String,
    pub username: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub avatar_url: Option<String>,
}

/// A project-team membership, including whether the invitation was accepted.
#[derive(Debug, Clone, Deserialize)]
pub struct TeamMember {
    pub user: TeamUser,
    pub role: String,
    pub accepted: bool,
}

/// Aggregate used by a details view.
///
/// Construction is all-or-nothing: [`details`] does not return project data if versions
/// or team membership could not also be fetched.
#[derive(Debug, Clone)]
pub struct ProjectDetails {
    pub project: Project,
    /// All versions, without game/loader filtering, in API (newest-first) order.
    pub versions: Vec<VersionSummary>,
    pub team_members: Vec<TeamMember>,
}

impl ProjectDetails {
    /// Accepted owner's username, if exposed by the team API. Other roles are
    /// available through `team_members`; no arbitrary member is called the author.
    pub fn author(&self) -> Option<&str> {
        self.team_members
            .iter()
            .find(|member| member.accepted && member.role.eq_ignore_ascii_case("owner"))
            .map(|member| member.user.username.as_str())
    }
}

/// Fetch `/project/{id}`, `/project/{id}/version`, and `/team/{team}/members`.
/// Accepts an ID or slug. Any request failure (including team lookup) is returned
/// with endpoint context; no partial result is presented as a complete detail view.
/// Blocking: invoke on a worker thread, as with search and install.
pub fn details(project_id: &str) -> Result<ProjectDetails, String> {
    valid_id(project_id)?;
    let api = Api::new()?;
    let project: Project = api.get(&format!("project/{project_id}"), &[])?;
    let versions = api.get(&format!("project/{project_id}/version"), &[])?;
    valid_id(&project.team)?;
    let team_members = api.get(&format!("team/{}/members", project.team), &[])?;
    Ok(ProjectDetails {
        project,
        versions,
        team_members,
    })
}

/// Install a mod and its required dependencies under `game_dir/mods`.
///
/// `game_dir: impl AsRef<Path>` accepts either an owned path or a borrowed path-like value
/// without forcing callers to allocate. Resolution finishes before the directory is
/// created, so metadata/compatibility failures write nothing. Downloads then proceed in
/// dependency-first order; each file is staged and published independently, so a later
/// failure does not roll back earlier successful files.
///
/// Returns dependency-first paths, including byte-identical files already present.
/// Errors have context and can be sent directly from a worker thread to the UI.
pub fn install(
    project_id: &str,
    version: &str,
    loader: &str,
    game_dir: impl AsRef<Path>,
) -> Result<Vec<PathBuf>, String> {
    validate_target(version, loader)?;
    let api = Api::new()?;
    let plan = resolve(&api, project_id, version, loader)?;
    let mods = game_dir.as_ref().join("mods");
    fs::create_dir_all(&mods).map_err(|e| format!("Create {}: {e}", mods.display()))?;
    if !fs::symlink_metadata(&mods)
        .map_err(|e| format!("Inspect {}: {e}", mods.display()))?
        .file_type()
        .is_dir()
    {
        return Err(format!("Not a real directory: {}", mods.display()));
    }
    let mut paths = Vec::new();
    for release in plan {
        let file = primary_file(&release)?;
        let mut response = api
            .client
            .get(&file.url)
            .send()
            .and_then(|r| r.error_for_status())
            .map_err(|e| format!("Download {}: {e}", file.filename))?;
        paths.push(store_file(&mods, file, &mut response)?);
    }
    Ok(paths)
}

fn validate_target(version: &str, loader: &str) -> Result<(), String> {
    if version.trim().is_empty() || loader.trim().is_empty() {
        return Err("Minecraft version and loader must not be empty".into());
    }
    Ok(())
}

fn filtered_search_params(filters: &SearchFilters) -> Vec<(String, String)> {
    let mut facets = Vec::new();
    for (key, value) in [
        ("project_type", &filters.project_type),
        ("versions", &filters.game_version),
        ("categories", &filters.loader),
        ("client_side", &filters.client_side),
        ("server_side", &filters.server_side),
    ] {
        if let Some(value) = value {
            facets.push(vec![format!("{key}:{value}")]);
        }
    }
    for category in &filters.categories {
        facets.push(vec![format!("categories:{category}")]);
    }
    vec![
        ("query".into(), filters.query.clone()),
        ("limit".into(), "20".into()),
        ("facets".into(), serde_json::json!(facets).to_string()),
        ("index".into(), filters.sort.clone()),
        ("offset".into(), filters.offset.to_string()),
    ]
}

fn version_params(version: &str, loader: &str) -> Vec<(String, String)> {
    vec![
        (
            "game_versions".into(),
            serde_json::json!([version]).to_string(),
        ),
        ("loaders".into(), serde_json::json!([loader]).to_string()),
    ]
}

struct Api {
    client: Client,
}

impl Api {
    fn new() -> Result<Self, String> {
        Client::builder()
            .user_agent(concat!("ferrite-launcher/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(Duration::from_secs(15))
            .timeout(Duration::from_secs(120))
            .https_only(true)
            .build()
            .map(|client| Self { client })
            .map_err(|e| format!("Build Modrinth client: {e}"))
    }

    // `DeserializeOwned` is required because the decoded value outlives the response
    // reader; endpoint helpers can therefore request any owned Serde model without
    // coupling this transport wrapper to a particular API response.
    fn get<T: DeserializeOwned>(
        &self,
        path: &str,
        params: &[(String, String)],
    ) -> Result<T, String> {
        let response = self
            .client
            .get(format!("{API}{path}"))
            .query(params)
            .send()
            .and_then(|r| r.error_for_status())
            .map_err(|e| format!("Modrinth {path}: {e}"))?;
        // reqwest's optional `json` feature is not required.
        serde_json::from_reader(response).map_err(|e| format!("Decode Modrinth {path}: {e}"))
    }
}

#[derive(Debug, Clone, Deserialize)]
struct Version {
    id: String,
    project_id: String,
    game_versions: Vec<String>,
    loaders: Vec<String>,
    files: Vec<VersionFile>,
    #[serde(default)]
    dependencies: Vec<Dependency>,
}

#[derive(Debug, Clone, Deserialize)]
struct VersionFile {
    url: String,
    filename: String,
    primary: bool,
    size: u64,
}

#[derive(Debug, Clone, Deserialize)]
struct Dependency {
    version_id: Option<String>,
    project_id: Option<String>,
    dependency_type: String,
}

// Resolution depends only on these two lookups. Keeping that boundary as a trait makes
// graph ordering, cycles, and pin conflicts testable without weakening the production
// HTTP validation. Callers use `&impl VersionSource`, so this remains statically
// dispatched and does not require trait objects or lifetimes on returned releases.
trait VersionSource {
    fn versions(&self, project: &str, game: &str, loader: &str) -> Result<Vec<Version>, String>;
    fn version(&self, id: &str) -> Result<Version, String>;
}

fn valid_id(id: &str) -> Result<(), String> {
    if id.is_empty()
        || !id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return Err(format!("Invalid Modrinth ID or slug: {id:?}"));
    }
    Ok(())
}

impl VersionSource for Api {
    fn versions(&self, project: &str, game: &str, loader: &str) -> Result<Vec<Version>, String> {
        valid_id(project)?;
        self.get(
            &format!("project/{project}/version"),
            &version_params(game, loader),
        )
    }
    fn version(&self, id: &str) -> Result<Version, String> {
        valid_id(id)?;
        self.get(&format!("version/{id}"), &[])
    }
}

// Compatibility is an exact membership test. There is intentionally no normalization,
// semantic-version interpretation, or loader aliasing: these values are Modrinth slugs
// and Minecraft version identifiers supplied to the API.
fn compatible(release: &Version, game: &str, loader: &str) -> bool {
    release.game_versions.iter().any(|v| v == game) && release.loaders.iter().any(|v| v == loader)
}

fn select(
    source: &impl VersionSource,
    project: &str,
    game: &str,
    loader: &str,
) -> Result<Version, String> {
    valid_id(project)?;
    // Modrinth returns project versions newest first. Preserving that order makes the
    // first exact match the selected release; release type is not used to exclude beta
    // or alpha versions.
    source
        .versions(project, game, loader)?
        .into_iter()
        .find(|v| compatible(v, game, loader))
        .ok_or_else(|| format!("No compatible version for {project} ({game}, {loader})"))
}

fn resolve(
    source: &impl VersionSource,
    project: &str,
    game: &str,
    loader: &str,
) -> Result<Vec<Version>, String> {
    let root = select(source, project, game, loader)?;
    // `selected` is keyed by project rather than version so two exact pins for one
    // project become a conflict instead of silently installing duplicate releases.
    let mut selected = HashMap::new();
    let mut plan = Vec::new();
    visit(source, root, game, loader, &mut selected, &mut plan)?;
    // Detect case-insensitive destination collisions before downloading. This is stricter
    // than some Unix filesystems but keeps plans portable to case-insensitive systems and
    // prevents one project from occupying another project's intended path.
    let mut filenames = HashMap::new();
    for release in &plan {
        let filename = &primary_file(release)?.filename;
        if let Some(other) = filenames.insert(filename.to_ascii_lowercase(), &release.project_id) {
            return Err(format!(
                "Projects {other} and {} use the same filename: {filename}",
                release.project_id
            ));
        }
    }
    Ok(plan)
}

fn visit(
    source: &impl VersionSource,
    release: Version,
    game: &str,
    loader: &str,
    selected: &mut HashMap<String, String>,
    plan: &mut Vec<Version>,
) -> Result<(), String> {
    if !compatible(&release, game, loader) {
        return Err(format!(
            "Dependency version {} is incompatible with {game} / {loader}",
            release.id
        ));
    }
    if let Some(id) = selected.get(&release.project_id) {
        return if id == &release.id {
            Ok(())
        } else {
            Err(format!(
                "Conflicting versions for project {}: {id} and {}",
                release.project_id, release.id
            ))
        };
    }
    if selected.len() >= MAX_PROJECTS {
        return Err(format!("Dependency graph exceeds {MAX_PROJECTS} projects"));
    }
    primary_file(&release)?;
    // Mark before recursion to terminate cycles and deduplicate shared dependencies.
    selected.insert(release.project_id.clone(), release.id.clone());
    for dep in &release.dependencies {
        if dep.dependency_type != "required" {
            continue;
        }
        // An exact version pin wins over a project-level dependency. Its returned IDs
        // are cross-checked because accepting mismatched metadata could install a
        // different project than the parent declared.
        let child = if let Some(id) = &dep.version_id {
            valid_id(id)?;
            let child = source.version(id)?;
            if child.id != *id
                || dep
                    .project_id
                    .as_ref()
                    .is_some_and(|p| p != &child.project_id)
            {
                return Err(format!(
                    "Dependency {id} returned mismatched version/project metadata"
                ));
            }
            child
        } else if let Some(project) = &dep.project_id {
            if selected.contains_key(project) {
                continue;
            }
            select(source, project, game, loader)?
        } else {
            return Err(format!(
                "Required dependency of {} has no project_id or version_id",
                release.id
            ));
        };
        visit(source, child, game, loader, selected, plan)?;
    }
    // Post-order insertion is what makes the eventual download list dependency-first.
    plan.push(release);
    Ok(())
}

// Restrict API-controlled names to one portable JAR basename. Besides blocking path
// traversal, rejecting hidden names and Windows device names avoids platform-dependent
// destinations and makes the earlier case-folded collision check meaningful.
fn safe_filename(name: &str) -> bool {
    let stem = name.split('.').next().unwrap_or("").to_ascii_uppercase();
    let reserved = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || (stem.len() == 4
            && (stem.starts_with("COM") || stem.starts_with("LPT"))
            && matches!(stem.as_bytes()[3], b'1'..=b'9'));
    !reserved
        && !name.starts_with('.')
        && name.len() <= 255
        && name.ends_with(".jar")
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'+'))
}

fn primary_file(release: &Version) -> Result<&VersionFile, String> {
    let mut files = release.files.iter().filter(|f| f.primary);
    let file = files
        .next()
        .ok_or_else(|| format!("Version {} has no primary file", release.id))?;
    if files.next().is_some() {
        return Err(format!("Version {} has multiple primary files", release.id));
    }
    if !safe_filename(&file.filename) {
        return Err(format!("Unsafe or non-JAR filename: {:?}", file.filename));
    }
    let url = reqwest::Url::parse(&file.url).map_err(|e| format!("Invalid download URL: {e}"))?;
    if url.scheme() != "https"
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err(format!(
            "Download URL must use HTTPS without credentials: {}",
            file.url
        ));
    }
    Ok(file)
}

// Per-file staging keeps incomplete bytes out of `mods`. Drop is best-effort rollback for
// ordinary errors and unwinding; a process abort/crash can still leave the hidden folder.
struct Staging(PathBuf);
impl Drop for Staging {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn staging(mods: &Path) -> Result<Staging, String> {
    for _ in 0..100 {
        let id = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
        let path = mods.join(format!(".ferrite-modrinth-{}-{id}", std::process::id()));
        match fs::create_dir(&path) {
            Ok(()) => return Ok(Staging(path)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(format!("Create staging directory: {e}")),
        }
    }
    Err("Unable to reserve a staging directory".into())
}

// Existing destinations are reused only after a full byte comparison. `symlink_metadata`
// deliberately rejects symlinks and other non-regular nodes before either path is opened.
fn same_file(existing: &Path, staged: &Path) -> Result<bool, String> {
    let metadata = fs::symlink_metadata(existing)
        .map_err(|e| format!("Inspect {}: {e}", existing.display()))?;
    if !metadata.file_type().is_file() {
        return Err(format!(
            "Refusing non-regular existing file: {}",
            existing.display()
        ));
    }
    let mut left = File::open(existing).map_err(|e| e.to_string())?;
    let mut right = File::open(staged).map_err(|e| e.to_string())?;
    if metadata.len() != right.metadata().map_err(|e| e.to_string())?.len() {
        return Ok(false);
    }
    let mut remaining = metadata.len();
    let mut a = [0; 8192];
    let mut b = [0; 8192];
    while remaining > 0 {
        let len = remaining.min(a.len() as u64) as usize;
        left.read_exact(&mut a[..len]).map_err(|e| e.to_string())?;
        right.read_exact(&mut b[..len]).map_err(|e| e.to_string())?;
        if a[..len] != b[..len] {
            return Ok(false);
        }
        remaining -= len as u64;
    }
    Ok(true)
}

// `Read` keeps storage independent of reqwest and permits bounded in-memory readers in
// tests. The caller supplies a mutable reader because copying advances its stream.
fn store_file(mods: &Path, file: &VersionFile, input: &mut impl Read) -> Result<PathBuf, String> {
    if !safe_filename(&file.filename) {
        return Err(format!("Unsafe filename: {:?}", file.filename));
    }
    let stage = staging(mods)?;
    let partial = stage.0.join("download.part");
    let ready = stage.0.join("download.ready");
    let dest = mods.join(&file.filename);
    // The closure gives all write/publish failures one contextual error boundary. It is
    // called once and must be mutable because it captures and advances `input`.
    let operation = || -> Result<(), String> {
        let mut out = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&partial)
            .map_err(|e| e.to_string())?;
        // Read at most one extra byte so an incorrect Content-Length/API size cannot
        // cause an unbounded download to disk.
        let copied = std::io::copy(&mut input.take(file.size.saturating_add(1)), &mut out)
            .map_err(|e| e.to_string())?;
        if copied != file.size {
            return Err(format!("Expected {} bytes, received {copied}", file.size));
        }
        out.flush()
            .and_then(|()| out.sync_all())
            .map_err(|e| e.to_string())?;
        drop(out);
        // Rename marks a fully flushed staged file as ready. Publishing with a hard link
        // is atomic and no-clobber on the same filesystem, unlike Unix `rename`, which
        // would replace a concurrently created destination.
        fs::rename(&partial, &ready).map_err(|e| e.to_string())?;
        match fs::hard_link(&ready, &dest) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                if same_file(&dest, &ready)? {
                    Ok(())
                } else {
                    Err("Existing file has different contents; refusing to overwrite".into())
                }
            }
            Err(e) => Err(format!("Publish staged file (hard links required): {e}")),
        }
    };
    let mut operation = operation;
    operation().map_err(|e| format!("Install {}: {e}", dest.display()))?;
    Ok(dest)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn release(project: &str, id: &str, dependencies: Vec<Dependency>) -> Version {
        Version {
            id: id.into(),
            project_id: project.into(),
            game_versions: vec!["1.21.1".into()],
            loaders: vec!["fabric".into()],
            dependencies,
            files: vec![VersionFile {
                url: "https://cdn.modrinth.com/test.jar".into(),
                filename: format!("{project}.jar"),
                primary: true,
                size: 3,
            }],
        }
    }
    fn dep(project: Option<&str>, version: Option<&str>, kind: &str) -> Dependency {
        Dependency {
            project_id: project.map(str::to_owned),
            version_id: version.map(str::to_owned),
            dependency_type: kind.into(),
        }
    }
    struct Offline(Vec<Version>);
    impl VersionSource for Offline {
        fn versions(&self, project: &str, _: &str, _: &str) -> Result<Vec<Version>, String> {
            Ok(self
                .0
                .iter()
                .filter(|v| v.project_id == project)
                .cloned()
                .collect())
        }
        fn version(&self, id: &str) -> Result<Version, String> {
            self.0
                .iter()
                .find(|v| v.id == id)
                .cloned()
                .ok_or_else(|| format!("Missing {id}"))
        }
    }
    fn plan(source: &Offline) -> Result<Vec<Version>, String> {
        resolve(source, "root", "1.21.1", "fabric")
    }

    #[test]
    fn search_models_and_json_filters() {
        let result: SearchResponse = serde_json::from_str(r#"{"hits":[{"title":"Test","description":"A mod","downloads":9000000000,"project_id":"abc","ignored":true}],"offset":0,"limit":20,"total_hits":1}"#).unwrap();
        assert_eq!(result.hits[0].downloads, 9_000_000_000);
        assert_eq!(result.hits[0].icon_url, None);
        assert_eq!(result.hits[0].author, None);
        assert_eq!(result.hits[0].versions, None);
        assert_eq!(result.hits[0].categories, None);
        let params = filtered_search_params(&SearchFilters {
            query: "a & b".into(),
            game_version: Some("1.21\"test".into()),
            loader: Some("fabric".into()),
            ..SearchFilters::default()
        });
        assert_eq!(params[0].1, "a & b");
        let facets: serde_json::Value = serde_json::from_str(&params[2].1).unwrap();
        assert_eq!(
            facets,
            serde_json::json!([
                ["project_type:mod"],
                ["versions:1.21\"test"],
                ["categories:fabric"]
            ])
        );
        let params = version_params("1.21.1", "fabric");
        assert_eq!(params[0], ("game_versions".into(), "[\"1.21.1\"]".into()));
        assert_eq!(params[1], ("loaders".into(), "[\"fabric\"]".into()));
    }

    #[test]
    fn browser_filter_encoding_and_pagination() {
        let filters = SearchFilters {
            query: "a & b + 雪".into(),
            game_version: Some("1.21\"test".into()),
            loader: Some("fabric".into()),
            client_side: Some("required".into()),
            server_side: Some("optional".into()),
            categories: vec!["adventure".into(), "a\\b\"&+".into()],
            project_type: Some("modpack".into()),
            sort: "downloads".into(),
            offset: 40,
        };
        let params = filtered_search_params(&filters);
        let request = Client::new()
            .get(format!("{API}search"))
            .query(&params)
            .build()
            .unwrap();
        // Decode the actual request URL, not just the intermediate JSON. This
        // catches double escaping and query separators leaking out of values.
        let decoded: Vec<(String, String)> = request.url().query_pairs().into_owned().collect();
        assert_eq!(decoded, params);
        let values: HashMap<_, _> = decoded.into_iter().collect();
        assert_eq!(values["query"], filters.query);
        assert_eq!(values["limit"], "20");
        assert_eq!(values["index"], "downloads");
        assert_eq!(values["offset"], "40");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&values["facets"]).unwrap(),
            serde_json::json!([
                ["project_type:modpack"],
                ["versions:1.21\"test"],
                ["categories:fabric"],
                ["client_side:required"],
                ["server_side:optional"],
                ["categories:adventure"],
                ["categories:a\\b\"&+"]
            ])
        );
        let defaults = filtered_search_params(&SearchFilters::default());
        assert_eq!(defaults[2].1, r#"[["project_type:mod"]]"#);
        assert_eq!(defaults[3].1, "relevance");
        assert_eq!(defaults[4].1, "0");
        let unrestricted = filtered_search_params(&SearchFilters {
            project_type: None,
            ..SearchFilters::default()
        });
        assert_eq!(unrestricted[2].1, "[]");
    }

    #[test]
    fn browser_models_and_author() {
        let hit: SearchHit = serde_json::from_value(serde_json::json!({
            "title": "Test", "description": "Short", "downloads": 42,
            "project_id": "abc", "icon_url": "https://example.com/icon.png",
            "author": "alice", "versions": ["1.21.1"], "categories": ["fabric"]
        }))
        .unwrap();
        assert_eq!(hit.author.as_deref(), Some("alice"));
        assert!(hit.icon_url.is_some());
        assert_eq!(hit.versions.unwrap(), ["1.21.1"]);
        assert_eq!(hit.categories.unwrap(), ["fabric"]);
        let project: Project = serde_json::from_value(serde_json::json!({
            "id": "abc", "slug": "test", "title": "Test", "description": "Short",
            "body": "# Markdown", "team": "team1", "downloads": 9000000000u64,
            "game_versions": ["1.21.1"], "loaders": ["fabric"],
            "client_side": "required", "server_side": "optional", "icon_url": null
        }))
        .unwrap();
        assert_eq!(project.source_url, None);
        assert_eq!(project.issues_url, None);
        assert_eq!(project.body, "# Markdown");
        let version: VersionSummary = serde_json::from_value(serde_json::json!({
            "id": "v1", "project_id": "abc", "name": "First", "version_number": "1.0",
            "version_type": "release", "date_published": "2026-01-01T00:00:00Z",
            "downloads": 42, "game_versions": ["1.21.1"], "loaders": ["fabric"],
            "dependencies": [{"project_id": "dep", "dependency_type": "required"}]
        }))
        .unwrap();
        assert_eq!(version.dependencies[0].project_id.as_deref(), Some("dep"));
        assert_eq!(version.dependencies[0].version_id, None);
        assert_eq!(version.dependencies[0].file_name, None);
        let member: TeamMember = serde_json::from_value(serde_json::json!({
            "user": {"id": "u1", "username": "alice"},
            "role": "Owner", "accepted": true
        }))
        .unwrap();
        let mut details = ProjectDetails {
            project,
            versions: vec![version],
            team_members: vec![member],
        };
        assert_eq!(details.author(), Some("alice"));
        details.team_members[0].accepted = false;
        assert_eq!(details.author(), None);
        details.team_members[0].accepted = true;
        details.team_members[0].role = "Developer".into();
        assert_eq!(details.author(), None);
        assert!(super::details("../invalid").is_err());
    }

    #[test]
    fn recursive_dependencies_cycles_and_optional() {
        let source = Offline(vec![
            release(
                "root",
                "r",
                vec![
                    dep(Some("a"), None, "required"),
                    dep(None, Some("b1"), "required"),
                    dep(Some("missing"), None, "optional"),
                ],
            ),
            release("a", "a1", vec![dep(Some("b"), Some("b1"), "required")]),
            release("b", "b1", vec![dep(Some("root"), None, "required")]),
        ]);
        assert_eq!(
            plan(&source)
                .unwrap()
                .iter()
                .map(|v| v.id.as_str())
                .collect::<Vec<_>>(),
            ["b1", "a1", "r"]
        );
    }

    #[test]
    fn rejects_incompatible_pins_and_conflicts() {
        for wrong_loader in [false, true] {
            let mut child = release("a", "a1", vec![]);
            if wrong_loader {
                child.loaders = vec!["forge".into()];
            } else {
                child.game_versions = vec!["1.20.1".into()];
            }
            let source = Offline(vec![
                release("root", "r", vec![dep(None, Some("a1"), "required")]),
                child,
            ]);
            assert!(plan(&source).unwrap_err().contains("incompatible"));
        }
        let source = Offline(vec![
            release(
                "root",
                "r",
                vec![
                    dep(None, Some("a1"), "required"),
                    dep(None, Some("a2"), "required"),
                ],
            ),
            release("a", "a1", vec![]),
            release("a", "a2", vec![]),
        ]);
        assert!(plan(&source).unwrap_err().contains("Conflicting"));
    }

    #[test]
    fn selection_and_invalid_dependencies() {
        let mut wrong = release("root", "wrong", vec![]);
        wrong.loaders.clear();
        let source = Offline(vec![wrong, release("root", "right", vec![])]);
        assert_eq!(plan(&source).unwrap()[0].id, "right");
        assert!(
            plan(&Offline(vec![]))
                .unwrap_err()
                .contains("No compatible")
        );
        let source = Offline(vec![release(
            "root",
            "r",
            vec![dep(None, None, "required")],
        )]);
        assert!(plan(&source).unwrap_err().contains("no project_id"));
        let source = Offline(vec![
            release(
                "root",
                "r",
                vec![dep(Some("other"), Some("a1"), "required")],
            ),
            release("a", "a1", vec![]),
        ]);
        assert!(plan(&source).unwrap_err().contains("mismatched"));
    }

    #[test]
    fn filenames_primary_files_and_collisions() {
        for name in [
            "../evil.jar",
            "/evil.jar",
            "a\\evil.jar",
            "..",
            ".hidden.jar",
            "C:evil.jar",
            "CON.jar",
            "lpt1.jar",
            "a.jar ",
            "not.zip",
            "a\n.jar",
        ] {
            assert!(!safe_filename(name), "{name:?}");
        }
        assert!(safe_filename("example-1.0+fabric.jar"));
        let mut v = release("root", "r", vec![]);
        v.files[0].primary = false;
        assert!(primary_file(&v).is_err());
        v.files[0].primary = true;
        v.files.push(v.files[0].clone());
        assert!(primary_file(&v).is_err());
        let root = release("root", "r", vec![dep(Some("a"), None, "required")]);
        let mut child = release("a", "a1", vec![]);
        child.files[0].filename = "ROOT.jar".into();
        assert!(
            plan(&Offline(vec![root, child]))
                .unwrap_err()
                .contains("same filename")
        );
    }

    #[test]
    fn atomic_files_identical_conflicting_and_truncated() {
        let dir = staging(&std::env::temp_dir()).unwrap();
        let file = release("test", "v", vec![]).files.remove(0);
        let path = store_file(&dir.0, &file, &mut Cursor::new(b"abc")).unwrap();
        store_file(&dir.0, &file, &mut Cursor::new(b"abc")).unwrap();
        assert!(
            store_file(&dir.0, &file, &mut Cursor::new(b"xyz"))
                .unwrap_err()
                .contains("different contents")
        );
        assert_eq!(fs::read(&path).unwrap(), b"abc");
        fs::remove_file(&path).unwrap();
        for bytes in [b"ab".as_slice(), b"abcd".as_slice()] {
            assert!(store_file(&dir.0, &file, &mut Cursor::new(bytes)).is_err());
            assert!(!path.exists());
            assert_eq!(fs::read_dir(&dir.0).unwrap().count(), 0);
        }
    }

    #[cfg(unix)]
    #[test]
    fn rejects_existing_symlink() {
        let dir = staging(&std::env::temp_dir()).unwrap();
        let target = dir.0.join("target");
        fs::write(&target, b"abc").unwrap();
        std::os::unix::fs::symlink(&target, dir.0.join("test.jar")).unwrap();
        let file = release("test", "v", vec![]).files.remove(0);
        assert!(
            store_file(&dir.0, &file, &mut Cursor::new(b"abc"))
                .unwrap_err()
                .contains("non-regular")
        );
        assert_eq!(fs::read(target).unwrap(), b"abc");
    }
}
