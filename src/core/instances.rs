//! Instance storage: loading/saving the manifest and managing game directories.
//!
//! Every path comes from [`AppPaths`]: the manifest lives at
//! [`AppPaths::instances_manifest`] and each profile's isolated game directory at
//! `AppPaths::instances_dir()/<directory>`. Shared versions, libraries, and assets are
//! elsewhere under the storage root.
//!
//! * [`load`] reads either manifest version (see [`crate::core::manifest`]). A missing
//!   file means no profiles. Invalid entries are reported via
//!   [`LoadedInstances::skipped`] instead of failing the whole load.
//! * [`save`] writes the current schema atomically (temp file in the same directory,
//!   fsync, rename). It refuses to replace an existing manifest that this build cannot
//!   read (unknown future version or corrupt), so such a file is never silently lost.
//! * [`create_game_dir`]/[`delete_game_dir`] keep Stage 0's lexical containment gate as
//!   defense in depth on top of directory-name validation.

pub use crate::core::manifest::{InstanceDirName, InstanceProfile, SkippedEntry};
pub use crate::core::manifest::{directory_key, name_key};

use crate::core::fsutil;
use crate::core::manifest::{self, ManifestError};
use crate::core::paths::{AppPaths, StorageMode};
use std::fmt;
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};

/// Errors produced while reading or writing the instance store.
#[derive(Debug)]
pub enum InstanceError {
    /// Creating, reading, renaming, or deleting launcher files failed.
    Io(io::Error),
    /// The manifest could not be parsed or is from an unsupported version.
    Manifest(ManifestError),
    /// Serializing the manifest failed.
    Json(serde_json::Error),
    /// An existing manifest that this build cannot read would have been overwritten.
    RefusingToOverwrite(String),
    /// The requested display name is empty, taken, or otherwise unusable.
    Name(NameError),
    /// No loaded instance uses the given folder.
    NotFound(String),
    /// Another manifest entry (possibly a skipped one) uses the same folder, so its
    /// files cannot be touched without destroying that entry's data.
    FolderShared { directory: String, other: String },
    /// A folder that was expected to be new already exists on disk.
    FolderExists(PathBuf),
    /// No free folder name could be found for a new instance.
    NoFreeFolder(String),
    /// Installing game files for a new or changed instance failed.
    Install(String),
}

impl fmt::Display for InstanceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "instance filesystem error: {error}"),
            Self::Manifest(error) => write!(f, "{error}"),
            Self::Json(error) => write!(f, "could not serialize instance metadata: {error}"),
            Self::RefusingToOverwrite(detail) => write!(
                f,
                "refusing to overwrite the existing instance manifest: {detail}"
            ),
            Self::Name(error) => write!(f, "{error}"),
            Self::NotFound(directory) => {
                write!(f, "no instance uses the folder '{directory}'")
            }
            Self::FolderShared { directory, other } => write!(
                f,
                "the folder '{directory}' is also used by {other}; its files were left \
                 untouched"
            ),
            Self::FolderExists(path) => write!(
                f,
                "the instance folder {} already exists and will not be reused",
                path.display()
            ),
            Self::NoFreeFolder(base) => {
                write!(f, "could not find a free folder name based on '{base}'")
            }
            Self::Install(detail) => write!(f, "{detail}"),
        }
    }
}

impl std::error::Error for InstanceError {}

impl From<io::Error> for InstanceError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<ManifestError> for InstanceError {
    fn from(error: ManifestError) -> Self {
        Self::Manifest(error)
    }
}

impl From<NameError> for InstanceError {
    fn from(error: NameError) -> Self {
        Self::Name(error)
    }
}

/// Why a proposed display name was rejected by [`validate_instance_name`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NameError {
    /// Empty or only whitespace.
    Empty,
    /// Contains a control character (tabs, newlines, ...).
    ControlCharacter,
    /// Another loaded instance already uses this name, compared case-insensitively.
    /// Carries the existing instance's name as written (`foo` reports `Foo`).
    Taken(String),
}

impl fmt::Display for NameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => write!(f, "a name is required"),
            Self::ControlCharacter => write!(f, "names cannot contain control characters"),
            Self::Taken(existing) => write!(f, "an instance named '{existing}' already exists"),
        }
    }
}

impl std::error::Error for NameError {}

impl From<serde_json::Error> for InstanceError {
    fn from(error: serde_json::Error) -> Self {
        Self::Json(error)
    }
}

/// Result of [`load`].
#[derive(Debug, Clone, Default)]
pub struct LoadedInstances {
    /// Valid profiles in on-disk order.
    pub profiles: Vec<InstanceProfile>,
    /// Entries reported and skipped; pass back to [`save`] to preserve them.
    pub skipped: Vec<SkippedEntry>,
    /// Schema version found on disk (`None` when no manifest exists yet).
    pub source_version: Option<u32>,
}

impl InstanceProfile {
    /// Returns this profile's isolated game directory (constructed only).
    pub fn game_dir(&self, paths: &AppPaths) -> PathBuf {
        paths.instances_dir().join(self.directory().as_str())
    }
}

/// Loads every saved profile from the manifest.
pub fn load(paths: &AppPaths) -> Result<LoadedInstances, InstanceError> {
    load_from(&paths.instances_manifest())
}

/// Loads a manifest from an explicit file (used for migration candidates).
pub fn load_from(path: &Path) -> Result<LoadedInstances, InstanceError> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(LoadedInstances::default());
        }
        Err(error) => return Err(error.into()),
    };
    let parsed = manifest::parse_manifest(&text)?;
    for entry in &parsed.skipped {
        eprintln!(
            "Ferrite: skipped invalid entry in {}: {entry}",
            path.display()
        );
    }
    Ok(LoadedInstances {
        profiles: parsed.instances,
        skipped: parsed.skipped,
        source_version: Some(parsed.source_version),
    })
}

/// Saves all profiles (plus preserved skipped entries).
///
/// In the standard data dir this writes the current versioned schema; when the
/// storage root is a legacy `minecraft` folder it writes the plain v0 array.
pub fn save(
    paths: &AppPaths,
    profiles: &[InstanceProfile],
    preserved: &[SkippedEntry],
) -> Result<(), InstanceError> {
    let path = paths.instances_manifest();
    guard_existing_manifest(&path)?;
    // The new data dir uses the versioned format. A legacy `minecraft` folder keeps
    // the bare-array v0 format so older Ferrite builds can still read it.
    let text = match paths.mode() {
        StorageMode::Standard => manifest::serialize_manifest(profiles, preserved)?,
        StorageMode::LegacyRollback => manifest::serialize_manifest_v0(profiles, preserved)?,
    };
    fsutil::write_atomic(&path, text.as_bytes())?;
    Ok(())
}

/// Refuses to overwrite a manifest this build cannot interpret.
fn guard_existing_manifest(path: &Path) -> Result<(), InstanceError> {
    match fs::read_to_string(path) {
        Ok(text) => match manifest::parse_manifest(&text) {
            Ok(_) => Ok(()),
            Err(error) => Err(InstanceError::RefusingToOverwrite(format!(
                "{} ({error}); move or repair it first",
                path.display()
            ))),
        },
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::InvalidData => {
            Err(InstanceError::RefusingToOverwrite(format!(
                "{} is not valid UTF-8; move or repair it first",
                path.display()
            )))
        }
        Err(error) => Err(error.into()),
    }
}

// =====================================================================
// Names and folder allocation
// =====================================================================

/// Maximum numeric suffix tried when looking for a free folder name.
const MAX_FOLDER_SUFFIX: u32 = 10_000;

/// The shared, side-effect-free display-name check used by create, rename, duplicate,
/// and the import preview. Returns the trimmed name.
///
/// Names are compared case-insensitively against loaded profiles other than `except`
/// (the instance being renamed). Skipped manifest entries are invisible in the UI and
/// do not block a name; their folders are protected by [`allocate_directory`] instead.
pub fn validate_instance_name(
    name: &str,
    profiles: &[InstanceProfile],
    except: Option<&InstanceDirName>,
) -> Result<String, NameError> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Err(NameError::Empty);
    }
    if trimmed.chars().any(char::is_control) {
        return Err(NameError::ControlCharacter);
    }
    let key = name_key(trimmed);
    if let Some(existing) = profiles
        .iter()
        .filter(|profile| except.is_none_or(|except| profile.directory() != except))
        .find(|profile| name_key(&profile.name) == key)
    {
        return Err(NameError::Taken(existing.name.clone()));
    }
    Ok(trimmed.to_owned())
}

/// Whether a loaded profile other than `except` already uses `name`, compared
/// case-insensitively (`Foo` and `foo` collide).
pub fn name_taken(
    profiles: &[InstanceProfile],
    name: &str,
    except: Option<&InstanceDirName>,
) -> bool {
    matches!(
        validate_instance_name(name, profiles, except),
        Err(NameError::Taken(_))
    )
}

/// Returns `base`, or `base (2)`, `base (3)`, ... — the first name no loaded profile
/// uses (case-insensitively). Import, duplicate, and the import preview use this.
pub fn suggest_name(profiles: &[InstanceProfile], base: &str) -> String {
    let base = base.trim();
    let base = if base.is_empty() { "Instance" } else { base };
    let mut name = base.to_owned();
    let mut suffix = 2;
    while name_taken(profiles, &name, None) {
        name = format!("{base} ({suffix})");
        suffix += 1;
    }
    name
}

/// Folder keys (see [`directory_key`]) that are already claimed: by loaded profiles,
/// by skipped manifest entries (whatever their raw `directory` says), and by every
/// entry already present in the instances directory on disk (listed without following
/// links, so a dangling link or a file also blocks its name).
fn claimed_folder_keys(
    paths: &AppPaths,
    profiles: &[InstanceProfile],
    skipped: &[SkippedEntry],
) -> Result<std::collections::HashSet<String>, InstanceError> {
    let mut claimed: std::collections::HashSet<String> = profiles
        .iter()
        .map(|profile| directory_key(profile.directory().as_str()))
        .collect();
    claimed.extend(
        skipped
            .iter()
            .filter_map(SkippedEntry::raw_directory)
            .map(directory_key),
    );
    match fs::read_dir(paths.instances_dir()) {
        Ok(entries) => {
            for entry in entries {
                claimed.insert(directory_key(&entry?.file_name().to_string_lossy()));
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    Ok(claimed)
}

/// Chooses a free folder for a new instance named `name`.
///
/// The folder is the name's slug, with `-2`, `-3`, ... appended until it is free
/// case-insensitively against loaded profiles, skipped manifest entries, and folders
/// already on disk. Nothing is created; [`create_new_game_dir`] claims the folder
/// exclusively, so a racing creator makes that step fail instead of sharing a folder.
pub fn allocate_directory(
    paths: &AppPaths,
    name: &str,
    profiles: &[InstanceProfile],
    skipped: &[SkippedEntry],
) -> Result<InstanceDirName, InstanceError> {
    let claimed = claimed_folder_keys(paths, profiles, skipped)?;
    let base = manifest::directory_slug(name);
    let mut candidate = base.clone();
    for suffix in 2..=MAX_FOLDER_SUFFIX + 1 {
        if !claimed.contains(&directory_key(&candidate))
            && let Ok(directory) = InstanceDirName::parse(candidate.clone())
        {
            return Ok(directory);
        }
        candidate = format!("{base}-{suffix}");
    }
    Err(InstanceError::NoFreeFolder(base))
}

/// Validates `name` (non-empty, unique case-insensitively) and builds the metadata for
/// a new instance in a freshly allocated folder (see [`allocate_directory`]).
pub fn new_instance_profile(
    paths: &AppPaths,
    name: &str,
    version: &str,
    loader: &str,
    profiles: &[InstanceProfile],
    skipped: &[SkippedEntry],
) -> Result<InstanceProfile, InstanceError> {
    let name = validate_instance_name(name, profiles, None)?;
    let directory = allocate_directory(paths, &name, profiles, skipped)?;
    Ok(InstanceProfile::with_directory(
        name,
        version.to_owned(),
        loader.to_owned(),
        directory,
    ))
}

/// Creates `profile`'s folder exclusively: fails with [`InstanceError::FolderExists`]
/// instead of adopting a folder that already exists.
pub fn create_new_game_dir(
    paths: &AppPaths,
    profile: &InstanceProfile,
) -> Result<(), InstanceError> {
    let path = profile.game_dir(paths);
    ensure_game_dir_contained(paths, &path)?;
    fs::create_dir_all(paths.instances_dir())?;
    match fs::create_dir(&path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            Err(InstanceError::FolderExists(path))
        }
        Err(error) => Err(error.into()),
    }
}

/// Creates a new instance's folder, then runs `install` (game/loader downloads).
///
/// If `install` fails, the folder created here is removed again so no orphan is left
/// behind. The profile is not added to the manifest; call [`commit_new_instance`] on
/// success.
pub fn create_instance_files(
    paths: &AppPaths,
    profile: &InstanceProfile,
    install: impl FnOnce() -> Result<(), String>,
) -> Result<(), InstanceError> {
    create_new_game_dir(paths, profile)?;
    if let Err(detail) = install() {
        if let Err(error) = delete_game_dir(paths, profile) {
            eprintln!(
                "Ferrite: could not clean up {} after a failed create: {error}",
                profile.game_dir(paths).display()
            );
        }
        return Err(InstanceError::Install(detail));
    }
    Ok(())
}

/// Adds a newly created instance (whose folder this launcher just created) to the
/// list and saves the manifest. Returns the new index.
///
/// Name and folder are re-checked because the list may have changed while files were
/// being prepared. If the name is now taken or saving fails, the profile is not kept
/// and its folder is removed. If another entry now claims the same folder, nothing is
/// deleted (the folder might be that entry's) and [`InstanceError::FolderShared`] is
/// returned.
pub fn commit_new_instance(
    paths: &AppPaths,
    profiles: &mut Vec<InstanceProfile>,
    skipped: &[SkippedEntry],
    profile: InstanceProfile,
) -> Result<usize, InstanceError> {
    if let Some(other) = folder_user(profiles, skipped, profile.directory(), None) {
        return Err(InstanceError::FolderShared {
            directory: profile.directory().to_string(),
            other,
        });
    }
    let discard = |profile: &InstanceProfile| {
        if let Err(error) = delete_game_dir(paths, profile) {
            eprintln!(
                "Ferrite: could not remove uncommitted instance folder {}: {error}",
                profile.game_dir(paths).display()
            );
        }
    };
    if let Err(error) = validate_instance_name(&profile.name, profiles, None) {
        discard(&profile);
        return Err(error.into());
    }
    profiles.push(profile);
    if let Err(error) = save(paths, profiles, skipped) {
        let profile = profiles.pop().expect("pushed above");
        discard(&profile);
        return Err(error);
    }
    Ok(profiles.len() - 1)
}

/// Describes the first manifest entry other than `except` whose folder matches
/// `directory` case-insensitively: a loaded profile or a skipped entry.
fn folder_user(
    profiles: &[InstanceProfile],
    skipped: &[SkippedEntry],
    directory: &InstanceDirName,
    except: Option<usize>,
) -> Option<String> {
    let key = directory_key(directory.as_str());
    if let Some(profile) = profiles
        .iter()
        .enumerate()
        .filter(|(index, _)| Some(*index) != except)
        .map(|(_, profile)| profile)
        .find(|profile| directory_key(profile.directory().as_str()) == key)
    {
        return Some(format!("the instance '{}'", profile.name));
    }
    skipped
        .iter()
        .find(|entry| entry.raw_directory().map(directory_key).as_deref() == Some(key.as_str()))
        .map(|entry| format!("a skipped manifest entry ({entry})"))
}

/// Creates the game directory belonging to `profile` and any missing parents.
pub fn create_game_dir(paths: &AppPaths, profile: &InstanceProfile) -> Result<(), InstanceError> {
    let path = profile.game_dir(paths);
    ensure_game_dir_contained(paths, &path)?;
    fs::create_dir_all(&path)?;
    Ok(())
}

/// Recursively deletes an instance's game directory if it exists.
///
/// Call only after the profile has been removed from the saved manifest. Refuses to act
/// outside the instances root. `remove_dir_all` does not follow a top-level symlink.
pub fn delete_game_dir(paths: &AppPaths, profile: &InstanceProfile) -> Result<(), InstanceError> {
    let path = profile.game_dir(paths);
    ensure_game_dir_contained(paths, &path)?;
    if fs::symlink_metadata(&path).is_ok() {
        fs::remove_dir_all(&path)?;
    }
    Ok(())
}

/// Rejects paths that escape the instances root (defense in depth; validated
/// directory names already cannot).
fn ensure_game_dir_contained(paths: &AppPaths, path: &Path) -> Result<(), InstanceError> {
    let root = paths.instances_dir();
    if is_contained_instance_path(path, &root) {
        Ok(())
    } else {
        Err(InstanceError::Io(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "refusing to modify path outside instances root ({}): {}",
                root.display(),
                path.display()
            ),
        )))
    }
}

/// Lexical containment: `path` must normalize to a strict child of `root`.
fn is_contained_instance_path(path: &Path, root: &Path) -> bool {
    let path = lexical_normalize(path);
    let root = lexical_normalize(root);
    if path.is_absolute() != root.is_absolute() {
        return false;
    }
    path.starts_with(&root) && path.as_os_str() != root.as_os_str()
}

fn lexical_normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => out.push(prefix.as_os_str()),
            Component::RootDir => out.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                let _ = out.pop();
            }
            Component::Normal(part) => out.push(part),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::paths::test_support::paths_in;

    fn profile(name: &str, existing: &[InstanceProfile]) -> InstanceProfile {
        InstanceProfile::new(name.into(), "1.21.1".into(), "Fabric".into(), existing)
    }

    #[test]
    fn missing_manifest_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let loaded = load(&paths_in(dir.path())).unwrap();
        assert!(loaded.profiles.is_empty());
        assert_eq!(loaded.source_version, None);
    }

    #[test]
    fn v0_file_loads_and_saves_as_v1() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths_in(dir.path());
        fs::create_dir_all(paths.storage_root()).unwrap();
        fs::write(
            paths.instances_manifest(),
            r#"[{"name":"A","version":"1.21.1","loader":"Vanilla","directory":"a"}]"#,
        )
        .unwrap();
        let loaded = load(&paths).unwrap();
        assert_eq!(loaded.source_version, Some(0));
        assert_eq!(loaded.profiles.len(), 1);
        assert_eq!(
            loaded.profiles[0].game_dir(&paths),
            paths.instances_dir().join("a")
        );

        save(&paths, &loaded.profiles, &loaded.skipped).unwrap();
        let reloaded = load(&paths).unwrap();
        assert_eq!(reloaded.source_version, Some(1));
        assert_eq!(reloaded.profiles, loaded.profiles);
    }

    #[test]
    fn legacy_storage_root_is_saved_as_v0_including_invalid_entries() {
        let dir = tempfile::tempdir().unwrap();
        let legacy_root = dir.path().join("old").join("minecraft");
        fs::create_dir_all(&legacy_root).unwrap();
        let invalid = r#"{"name":"Evil","version":"1","loader":"Vanilla","directory":"../escape"}"#;
        fs::write(
            legacy_root.join("instances.json"),
            format!(
                r#"[{{"name":"A","version":"1.21.1","loader":"Vanilla","directory":"a"}},{invalid}]"#
            ),
        )
        .unwrap();
        let paths = paths_in(&dir.path().join("ferrite"))
            .with_legacy_storage_root(legacy_root.clone())
            .unwrap();

        let mut loaded = load(&paths).unwrap();
        assert_eq!(loaded.skipped.len(), 1);
        loaded.profiles.push(InstanceProfile::new(
            "New".into(),
            "1.20.1".into(),
            "Fabric".into(),
            &loaded.profiles,
        ));
        save(&paths, &loaded.profiles, &loaded.skipped).unwrap();

        let text = fs::read_to_string(legacy_root.join("instances.json")).unwrap();
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        let array = value
            .as_array()
            .expect("legacy folder must keep a bare v0 array");
        assert_eq!(array.len(), 3);
        assert!(!text.contains("schema_version"));
        assert_eq!(array[1]["name"], "New");
        assert_eq!(
            array[2],
            serde_json::from_str::<serde_json::Value>(invalid).unwrap(),
            "invalid entries are written back unchanged"
        );
        // Every valid entry has exactly the fields older builds expect.
        for entry in &array[..2] {
            let mut keys: Vec<_> = entry.as_object().unwrap().keys().cloned().collect();
            keys.sort();
            assert_eq!(keys, ["directory", "loader", "name", "version"]);
        }
        let reloaded = load(&paths).unwrap();
        assert_eq!(reloaded.source_version, Some(0));
        assert_eq!(reloaded.profiles.len(), 2);
        assert_eq!(reloaded.skipped.len(), 1);

        // The standard data dir still gets v1.
        let standard = paths_in(&dir.path().join("ferrite"));
        save(&standard, &reloaded.profiles, &reloaded.skipped).unwrap();
        assert_eq!(load(&standard).unwrap().source_version, Some(1));
    }

    #[test]
    fn save_refuses_to_overwrite_future_or_corrupt_manifest() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths_in(dir.path());
        fs::create_dir_all(paths.storage_root()).unwrap();
        for contents in [r#"{"schema_version": 99, "instances": []}"#, "{ corrupt"] {
            fs::write(paths.instances_manifest(), contents).unwrap();
            assert!(load(&paths).is_err());
            let error = save(&paths, &[profile("new", &[])], &[]).unwrap_err();
            assert!(
                matches!(error, InstanceError::RefusingToOverwrite(_)),
                "{error}"
            );
            assert_eq!(
                fs::read_to_string(paths.instances_manifest()).unwrap(),
                contents
            );
        }
    }

    #[test]
    fn skipped_entries_are_preserved_on_save_and_never_touch_outside_paths() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths_in(dir.path());
        fs::create_dir_all(paths.storage_root()).unwrap();
        let outside = dir.path().join("outside");
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("keep.txt"), b"keep").unwrap();
        fs::write(
            paths.instances_manifest(),
            r#"[{"name":"ok","version":"1","loader":"Vanilla","directory":"ok"},
                {"name":"evil","version":"1","loader":"Vanilla","directory":"../../outside"}]"#,
        )
        .unwrap();
        let loaded = load(&paths).unwrap();
        assert_eq!(loaded.profiles.len(), 1);
        assert_eq!(loaded.skipped.len(), 1);
        for profile in &loaded.profiles {
            delete_game_dir(&paths, profile).unwrap();
        }
        assert!(outside.join("keep.txt").exists());
        save(&paths, &loaded.profiles, &loaded.skipped).unwrap();
        assert_eq!(load(&paths).unwrap().skipped.len(), 1);
    }

    #[test]
    fn create_and_delete_game_dir() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths_in(dir.path());
        let profile = profile("My Profile", &[]);
        create_game_dir(&paths, &profile).unwrap();
        assert!(paths.instances_dir().join("my-profile").is_dir());
        delete_game_dir(&paths, &profile).unwrap();
        assert!(!paths.instances_dir().join("my-profile").exists());
    }

    #[test]
    fn containment_rejects_traversal_and_absolute_escapes() {
        let root = Path::new("/data/minecraft/instances");
        assert!(is_contained_instance_path(&root.join("my-fabric"), root));
        assert!(!is_contained_instance_path(
            &root.join("..").join("..").join("etc"),
            root
        ));
        assert!(!is_contained_instance_path(root, root));
        assert!(!is_contained_instance_path(Path::new("relative/x"), root));
        assert!(!is_contained_instance_path(Path::new("/tmp/evil"), root));
    }

    #[test]
    fn create_and_delete_refuse_unvalidated_escapes() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths_in(dir.path());
        let target = dir.path().join("outside-ferrite-target");
        fs::create_dir_all(&target).unwrap();
        let escaped = InstanceProfile::with_unchecked_directory_for_tests(
            "escape",
            "../../outside-ferrite-target",
        );
        let message = delete_game_dir(&paths, &escaped).unwrap_err().to_string();
        assert!(message.contains("outside instances root"), "{message}");
        let message = create_game_dir(&paths, &escaped).unwrap_err().to_string();
        assert!(message.contains("refusing to modify"), "{message}");
        assert!(target.exists());
    }

    // ----- Stage 2: names and folder allocation -----

    fn manifest_with(paths: &AppPaths, text: &str) -> LoadedInstances {
        fs::create_dir_all(paths.storage_root()).unwrap();
        fs::write(paths.instances_manifest(), text).unwrap();
        load(paths).unwrap()
    }

    #[test]
    fn names_are_unique_case_insensitively() {
        let existing = vec![profile("Foo", &[])];
        assert_eq!(
            validate_instance_name("  foo ", &existing, None),
            Err(NameError::Taken("Foo".into()))
        );
        assert_eq!(
            validate_instance_name("   ", &existing, None),
            Err(NameError::Empty)
        );
        assert_eq!(
            validate_instance_name("a\tb", &existing, None),
            Err(NameError::ControlCharacter)
        );
        assert_eq!(
            validate_instance_name(" Bar ", &existing, None).unwrap(),
            "Bar"
        );
        // Renaming an instance to a different casing of its own name is allowed.
        let own = existing[0].directory().clone();
        assert_eq!(
            validate_instance_name("FOO", &existing, Some(&own)).unwrap(),
            "FOO"
        );
        assert_eq!(suggest_name(&existing, "foo"), "foo (2)");
        assert_eq!(suggest_name(&existing, "Other"), "Other");
    }

    #[test]
    fn allocation_is_case_insensitive_against_profiles() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths_in(dir.path());
        // A legacy entry whose folder differs from the new slug only by case.
        let loaded = manifest_with(
            &paths,
            r#"[{"name":"Old","version":"1","loader":"Vanilla","directory":"Foo"}]"#,
        );
        let new =
            new_instance_profile(&paths, "foo", "1.21.1", "Fabric", &loaded.profiles, &[]).unwrap();
        assert_eq!(new.directory().as_str(), "foo-2");
        let error =
            new_instance_profile(&paths, "OLD", "1", "Vanilla", &loaded.profiles, &[]).unwrap_err();
        assert!(matches!(error, InstanceError::Name(NameError::Taken(ref n)) if n == "Old"));
    }

    #[test]
    fn allocation_skips_leftover_folders_on_disk() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths_in(dir.path());
        // Left behind by a crash, a failed delete, or "remove from list, keep files".
        fs::create_dir_all(paths.instances_dir().join("My-Pack")).unwrap();
        fs::write(paths.instances_dir().join("My-Pack/keep.txt"), b"user data").unwrap();
        let new = new_instance_profile(&paths, "My Pack", "1", "Vanilla", &[], &[]).unwrap();
        assert_eq!(new.directory().as_str(), "my-pack-2");
        create_new_game_dir(&paths, &new).unwrap();
        assert_eq!(
            fs::read(paths.instances_dir().join("My-Pack/keep.txt")).unwrap(),
            b"user data"
        );
    }

    #[test]
    fn allocation_skips_folders_of_skipped_entries() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths_in(dir.path());
        let loaded = manifest_with(
            &paths,
            r#"[{"name":"A","version":"1","loader":"Vanilla","directory":"alpha"},
                {"name":"B","version":"1","loader":"Vanilla","directory":"ALPHA"},
                {"name":"C","version":"1","loader":"Vanilla","directory":"beta."}]"#,
        );
        assert_eq!(loaded.skipped.len(), 2);
        // `beta.` is invalid, but on Windows it names the same folder as `beta`.
        let beta =
            new_instance_profile(&paths, "Beta", "1", "Vanilla", &[], &loaded.skipped).unwrap();
        assert_eq!(beta.directory().as_str(), "beta-2");
        let alpha =
            new_instance_profile(&paths, "alpha", "1", "Vanilla", &[], &loaded.skipped).unwrap();
        assert_eq!(alpha.directory().as_str(), "alpha-2");
    }

    #[test]
    fn create_never_adopts_an_existing_folder() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths_in(dir.path());
        let first = profile("Same", &[]);
        fs::create_dir_all(first.game_dir(&paths)).unwrap();
        let error = create_new_game_dir(&paths, &first).unwrap_err();
        assert!(matches!(error, InstanceError::FolderExists(_)), "{error}");
    }

    #[test]
    fn failed_create_removes_its_folder() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths_in(dir.path());
        let new = new_instance_profile(&paths, "Broken", "1", "Vanilla", &[], &[]).unwrap();
        let error = create_instance_files(&paths, &new, || {
            fs::write(new.game_dir(&paths).join("partial.txt"), b"x").unwrap();
            Err("download failed".into())
        })
        .unwrap_err();
        assert!(matches!(error, InstanceError::Install(ref d) if d == "download failed"));
        assert!(!new.game_dir(&paths).exists());
        assert!(
            paths.instances_dir().is_dir(),
            "only the new folder is removed"
        );
    }

    #[test]
    fn commit_rolls_back_and_cleans_up_when_save_fails() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths_in(dir.path());
        let new = new_instance_profile(&paths, "New", "1", "Vanilla", &[], &[]).unwrap();
        create_instance_files(&paths, &new, || Ok(())).unwrap();
        // A manifest this build cannot read makes `save` refuse.
        fs::write(paths.instances_manifest(), "{ corrupt").unwrap();
        let mut profiles = Vec::new();
        let error = commit_new_instance(&paths, &mut profiles, &[], new.clone()).unwrap_err();
        assert!(matches!(error, InstanceError::RefusingToOverwrite(_)));
        assert!(profiles.is_empty());
        assert!(!new.game_dir(&paths).exists());

        // Success path.
        fs::remove_file(paths.instances_manifest()).unwrap();
        let new = new_instance_profile(&paths, "New", "1", "Vanilla", &[], &[]).unwrap();
        create_instance_files(&paths, &new, || Ok(())).unwrap();
        assert_eq!(
            commit_new_instance(&paths, &mut profiles, &[], new).unwrap(),
            0
        );
        assert_eq!(load(&paths).unwrap().profiles, profiles);
    }

    #[test]
    fn commit_refuses_a_name_taken_meanwhile() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths_in(dir.path());
        let new = new_instance_profile(&paths, "Dup", "1", "Vanilla", &[], &[]).unwrap();
        create_instance_files(&paths, &new, || Ok(())).unwrap();
        let mut profiles = vec![InstanceProfile::with_directory(
            "DUP".into(),
            "1".into(),
            "Vanilla".into(),
            InstanceDirName::parse("other").unwrap(),
        )];
        let error = commit_new_instance(&paths, &mut profiles, &[], new.clone()).unwrap_err();
        assert!(matches!(error, InstanceError::Name(NameError::Taken(_))));
        assert_eq!(profiles.len(), 1);
        assert!(!new.game_dir(&paths).exists());
    }
}
