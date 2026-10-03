//! Canonical on-disk locations for everything Ferrite stores.
//!
//! [`AppPaths`] is resolved exactly once at startup and then passed by reference (or
//! cloned into worker threads). No other module computes launcher storage locations,
//! and no path produced here is ever relative: [`AppPaths::from_base_dirs`] rejects
//! relative roots, so the process working directory can never influence where data
//! is read or written.
//!
//! ## Layout
//!
//! | Purpose | Location |
//! |---|---|
//! | Core config (`config.toml`), UI settings (`ui.toml`) | `ProjectDirs::config_dir()` |
//! | Storage root (manifest, instances, versions, libraries, assets, natives) | `ProjectDirs::data_local_dir()/minecraft` |
//! | Migration state + staging | `ProjectDirs::data_local_dir()` |
//! | Downloads (loader installers) and temp files | `ProjectDirs::cache_dir()` |
//!
//! `data_local_dir` is used rather than `data_dir` so multi-gigabyte game files stay
//! out of roaming profiles on Windows.
//!
//! ## Storage modes
//!
//! Every mode funnels into the same [`AppPaths`] value, so consumers never branch on
//! the mode. Only [`StorageMode::Standard`] and the explicit post-migration rollback
//! ([`StorageMode::LegacyRollback`]) exist today. A future portable mode would be a
//! new variant chosen by an explicit opt-in (for example a marker file next to the
//! executable) that simply supplies different [`BaseDirs`]; it must never be inferred
//! from the executable directory merely being writable.

use directories::ProjectDirs;
use std::fmt;
use std::path::{Path, PathBuf};

/// `ProjectDirs` qualifier; changing it would move every user's data.
pub const QUALIFIER: &str = "io";
/// `ProjectDirs` organization; changing it would move every user's data.
pub const ORGANIZATION: &str = "Ferrite";
/// `ProjectDirs` application; changing it would move every user's data.
pub const APPLICATION: &str = "Ferrite Launcher";

/// Name of the storage root inside the data directory. It mirrors the pre-Stage-1
/// relative `minecraft/` tree so a legacy directory can be migrated by one rename.
pub const STORAGE_DIR_NAME: &str = "minecraft";
/// Instance manifest file name inside the storage root.
pub const INSTANCES_MANIFEST_FILE: &str = "instances.json";
/// Directory holding per-instance game directories inside the storage root.
pub const INSTANCES_DIR_NAME: &str = "instances";
/// Core configuration file name inside the config directory.
pub const CONFIG_FILE: &str = "config.toml";
/// Frontend-owned appearance/layout settings file inside the config directory.
pub const UI_SETTINGS_FILE: &str = "ui.toml";
/// Migration progress marker inside the data directory.
pub const MIGRATION_STATE_FILE: &str = "migration-state.json";
/// Migration staging directory inside the data directory (same filesystem as the
/// storage root so the final rename is atomic).
pub const MIGRATION_STAGING_DIR: &str = ".migration-staging";

/// Failure to determine usable storage locations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathsError {
    /// The OS did not provide a home/profile directory.
    PlatformDirsUnavailable,
    /// A supplied root was relative and would have been resolved against the CWD.
    RelativeRoot { role: &'static str, path: PathBuf },
}

impl fmt::Display for PathsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PlatformDirsUnavailable => write!(
                f,
                "the operating system did not provide user data/config directories"
            ),
            Self::RelativeRoot { role, path } => write!(
                f,
                "refusing relative {role} directory {} (paths must be absolute)",
                path.display()
            ),
        }
    }
}

impl std::error::Error for PathsError {}

/// Where the storage root came from. See the module docs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StorageMode {
    /// Per-user OS directories from `ProjectDirs` (or test-supplied roots).
    Standard,
    /// The user explicitly rolled back to a pre-Stage-1 `minecraft/` directory after
    /// a migration; the storage root points at that old directory.
    LegacyRollback,
}

/// The three OS-level roots everything else is derived from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BaseDirs {
    /// Small, user-editable configuration (may roam on Windows).
    pub config: PathBuf,
    /// Large, machine-local data (never roams).
    pub data_local: PathBuf,
    /// Re-creatable downloads and temporary files.
    pub cache: PathBuf,
}

impl BaseDirs {
    /// Reads Ferrite's directories from the platform conventions.
    pub fn from_platform() -> Result<Self, PathsError> {
        let dirs = ProjectDirs::from(QUALIFIER, ORGANIZATION, APPLICATION)
            .ok_or(PathsError::PlatformDirsUnavailable)?;
        Ok(Self {
            config: dirs.config_dir().to_path_buf(),
            data_local: dirs.data_local_dir().to_path_buf(),
            cache: dirs.cache_dir().to_path_buf(),
        })
    }
}

/// Every launcher storage location, resolved once at startup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppPaths {
    config_dir: PathBuf,
    data_dir: PathBuf,
    cache_dir: PathBuf,
    storage_root: PathBuf,
    mode: StorageMode,
}

fn require_absolute(role: &'static str, path: &Path) -> Result<(), PathsError> {
    // `has_root` additionally rejects Windows drive-relative forms such as `C:foo`.
    if path.is_absolute() && path.has_root() {
        Ok(())
    } else {
        Err(PathsError::RelativeRoot {
            role,
            path: path.to_path_buf(),
        })
    }
}

impl AppPaths {
    /// Resolves the standard per-user locations. Never consults the working directory.
    pub fn resolve() -> Result<Self, PathsError> {
        Self::from_base_dirs(BaseDirs::from_platform()?)
    }

    /// Builds paths from explicit roots (used by [`Self::resolve`] and by tests).
    /// Every root must be absolute.
    pub fn from_base_dirs(base: BaseDirs) -> Result<Self, PathsError> {
        require_absolute("config", &base.config)?;
        require_absolute("data", &base.data_local)?;
        require_absolute("cache", &base.cache)?;
        Ok(Self {
            storage_root: base.data_local.join(STORAGE_DIR_NAME),
            config_dir: base.config,
            data_dir: base.data_local,
            cache_dir: base.cache,
            mode: StorageMode::Standard,
        })
    }

    /// Returns a copy whose storage root is an explicitly chosen legacy directory
    /// (rollback after migration). Config, cache, and migration-state locations do not
    /// change. The root must be absolute.
    pub fn with_legacy_storage_root(&self, root: PathBuf) -> Result<Self, PathsError> {
        require_absolute("legacy storage", &root)?;
        Ok(Self {
            storage_root: root,
            mode: StorageMode::LegacyRollback,
            ..self.clone()
        })
    }

    /// How the storage root was chosen.
    pub fn mode(&self) -> &StorageMode {
        &self.mode
    }

    // ----- config -----

    /// Directory for configuration files.
    pub fn config_dir(&self) -> &Path {
        &self.config_dir
    }
    /// Core launcher configuration (`config.toml`).
    pub fn config_file(&self) -> PathBuf {
        self.config_dir.join(CONFIG_FILE)
    }
    /// Frontend-owned appearance/layout settings (`ui.toml`).
    pub fn ui_settings_file(&self) -> PathBuf {
        self.config_dir.join(UI_SETTINGS_FILE)
    }

    // ----- data -----

    /// Machine-local data directory (parent of the standard storage root).
    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }
    /// Root containing the instance manifest, instances, and shared game files.
    /// Also the `--installClient` target for Forge/NeoForge installers.
    pub fn storage_root(&self) -> &Path {
        &self.storage_root
    }
    /// Where the standard (non-rollback) storage root lives, regardless of mode.
    pub fn standard_storage_root(&self) -> PathBuf {
        self.data_dir.join(STORAGE_DIR_NAME)
    }
    /// Versioned instance manifest.
    pub fn instances_manifest(&self) -> PathBuf {
        self.storage_root.join(INSTANCES_MANIFEST_FILE)
    }
    /// Parent of every per-instance game directory.
    pub fn instances_dir(&self) -> PathBuf {
        self.storage_root.join(INSTANCES_DIR_NAME)
    }
    /// Shared `versions/` tree.
    pub fn versions_dir(&self) -> PathBuf {
        self.storage_root.join("versions")
    }
    /// `versions/<id>` for a Mojang or synthetic loader version id.
    pub fn version_dir(&self, id: &str) -> PathBuf {
        self.versions_dir().join(id)
    }
    /// Shared Maven-style `libraries/` tree.
    pub fn libraries_dir(&self) -> PathBuf {
        self.storage_root.join("libraries")
    }
    /// Shared content-addressed `assets/` tree.
    pub fn assets_dir(&self) -> PathBuf {
        self.storage_root.join("assets")
    }
    /// Per-version native extraction directory `natives/<id>`.
    pub fn natives_dir(&self, id: &str) -> PathBuf {
        self.storage_root.join("natives").join(id)
    }
    /// Migration progress marker.
    pub fn migration_state_file(&self) -> PathBuf {
        self.data_dir.join(MIGRATION_STATE_FILE)
    }
    /// Migration staging directory (inside the data dir, same filesystem).
    pub fn migration_staging_dir(&self) -> PathBuf {
        self.data_dir.join(MIGRATION_STAGING_DIR)
    }

    // ----- cache -----

    /// Cache directory for re-creatable files.
    pub fn cache_dir(&self) -> &Path {
        &self.cache_dir
    }
    /// Downloaded installers and similar transient artifacts.
    pub fn downloads_dir(&self) -> PathBuf {
        self.cache_dir.join("downloads")
    }
    /// Scratch space for temporary files.
    pub fn temp_dir(&self) -> PathBuf {
        self.cache_dir.join("tmp")
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;

    /// AppPaths rooted entirely inside `root` (a tempdir).
    pub fn paths_in(root: &Path) -> AppPaths {
        AppPaths::from_base_dirs(BaseDirs {
            config: root.join("config"),
            data_local: root.join("data"),
            cache: root.join("cache"),
        })
        .unwrap()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Serializes tests that change the process working directory.
    static CWD_LOCK: Mutex<()> = Mutex::new(());

    fn all_paths(paths: &AppPaths) -> Vec<PathBuf> {
        vec![
            paths.config_dir().to_path_buf(),
            paths.config_file(),
            paths.ui_settings_file(),
            paths.data_dir().to_path_buf(),
            paths.storage_root().to_path_buf(),
            paths.instances_manifest(),
            paths.instances_dir(),
            paths.versions_dir(),
            paths.version_dir("1.21.1"),
            paths.libraries_dir(),
            paths.assets_dir(),
            paths.natives_dir("1.21.1"),
            paths.migration_state_file(),
            paths.migration_staging_dir(),
            paths.cache_dir().to_path_buf(),
            paths.downloads_dir(),
            paths.temp_dir(),
        ]
    }

    #[test]
    fn relative_roots_are_rejected() {
        for base in [
            BaseDirs {
                config: PathBuf::from("config"),
                data_local: PathBuf::from("/abs/data"),
                cache: PathBuf::from("/abs/cache"),
            },
            BaseDirs {
                config: PathBuf::from("/abs/config"),
                data_local: PathBuf::from("minecraft"),
                cache: PathBuf::from("/abs/cache"),
            },
            BaseDirs {
                config: PathBuf::from("/abs/config"),
                data_local: PathBuf::from("/abs/data"),
                cache: PathBuf::from("./cache"),
            },
        ] {
            assert!(matches!(
                AppPaths::from_base_dirs(base),
                Err(PathsError::RelativeRoot { .. })
            ));
        }
        let dir = tempfile::tempdir().unwrap();
        let paths = test_support::paths_in(dir.path());
        assert!(
            paths
                .with_legacy_storage_root(PathBuf::from("minecraft"))
                .is_err()
        );
    }

    #[test]
    fn layout_is_derived_from_roots() {
        let dir = tempfile::tempdir().unwrap();
        let paths = test_support::paths_in(dir.path());
        let data = dir.path().join("data");
        assert_eq!(paths.storage_root(), data.join("minecraft"));
        assert_eq!(
            paths.instances_manifest(),
            data.join("minecraft").join("instances.json")
        );
        assert_eq!(
            paths.instances_dir(),
            data.join("minecraft").join("instances")
        );
        assert_eq!(
            paths.libraries_dir(),
            data.join("minecraft").join("libraries")
        );
        assert_eq!(
            paths.config_file(),
            dir.path().join("config").join("config.toml")
        );
        assert_eq!(
            paths.ui_settings_file(),
            dir.path().join("config").join("ui.toml")
        );
        assert!(paths.downloads_dir().starts_with(dir.path().join("cache")));
        assert!(paths.temp_dir().starts_with(dir.path().join("cache")));
        for path in all_paths(&paths) {
            assert!(path.starts_with(dir.path()), "{}", path.display());
        }
    }

    #[test]
    fn legacy_rollback_only_moves_the_storage_root() {
        let dir = tempfile::tempdir().unwrap();
        let paths = test_support::paths_in(dir.path());
        let legacy = dir.path().join("old").join("minecraft");
        let rolled = paths.with_legacy_storage_root(legacy.clone()).unwrap();
        assert_eq!(rolled.mode(), &StorageMode::LegacyRollback);
        assert_eq!(rolled.storage_root(), legacy);
        assert_eq!(rolled.instances_dir(), legacy.join("instances"));
        assert_eq!(rolled.config_file(), paths.config_file());
        assert_eq!(rolled.migration_state_file(), paths.migration_state_file());
        assert_eq!(rolled.standard_storage_root(), paths.storage_root());
    }

    /// AppPaths must never depend on the working directory: resolving from two very
    /// different CWDs yields identical, absolute paths outside both. This only computes
    /// paths; nothing is created in the real user directories.
    #[test]
    fn resolve_never_uses_the_working_directory() {
        let _guard = CWD_LOCK.lock().unwrap_or_else(|poison| poison.into_inner());
        let original = std::env::current_dir().unwrap();
        let first_cwd = tempfile::tempdir().unwrap();
        let second_cwd = tempfile::tempdir().unwrap();

        std::env::set_current_dir(first_cwd.path()).unwrap();
        let first = AppPaths::resolve();
        std::env::set_current_dir(second_cwd.path()).unwrap();
        let second = AppPaths::resolve();
        std::env::set_current_dir(&original).unwrap();

        let (Ok(first), Ok(second)) = (first, second) else {
            // Platforms without a home directory cannot resolve at all, which is
            // still never a CWD fallback.
            return;
        };
        assert_eq!(first, second);
        for path in all_paths(&first) {
            assert!(path.is_absolute(), "{}", path.display());
            assert!(!path.starts_with(first_cwd.path()), "{}", path.display());
            assert!(!path.starts_with(second_cwd.path()), "{}", path.display());
        }
        assert!(first.storage_root().starts_with(first.data_dir()));
    }
}
