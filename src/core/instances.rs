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
}
