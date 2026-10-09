//! Editing instances: rename (display name only) and changing the Minecraft version or
//! loader, plus the version-order helper used to detect downgrades.
//!
//! Prepared updates install shared game files before manifest changes. The instance
//! folder never moves; failed saves restore the previous in-memory values. Version/loader
//! saves precede the separate rename, which can fail without undoing the version save.

use crate::core::instances::{self, InstanceDirName, InstanceError, InstanceProfile, SkippedEntry};
use crate::core::paths::AppPaths;
use std::cmp::Ordering;
use std::fmt;

/// Why an edit was not applied. Nothing changed in either case.
#[derive(Debug)]
pub enum EditError {
    /// The instance's game was found running at the moment of acting.
    NowRunning,
    Failed(InstanceError),
}

impl fmt::Display for EditError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NowRunning => write!(f, "the instance is running; close Minecraft first"),
            Self::Failed(error) => write!(f, "{error}"),
        }
    }
}

impl std::error::Error for EditError {}

impl From<InstanceError> for EditError {
    fn from(error: InstanceError) -> Self {
        Self::Failed(error)
    }
}

/// A selected update with stable instance identity, independent of UI/worker scheduling.
pub struct PreparedUpdate {
    original: InstanceProfile,
    new_name: String,
    version: String,
    loader: crate::loaders::ModLoader,
}

/// Version/loader were saved. A separate rename may fail without undoing that save.
pub struct UpdateOutcome {
    pub name: String,
    pub rename_error: Option<EditError>,
}

/// Captures the chosen target and rejects an already running instance before installation.
pub fn prepare_update(
    profiles: &[InstanceProfile],
    directory: &InstanceDirName,
    new_name: &str,
    version: &str,
    loader_label: &str,
    is_running: &dyn Fn(&InstanceDirName) -> bool,
) -> Result<PreparedUpdate, String> {
    let profile = profiles
        .iter()
        .find(|profile| profile.directory() == directory)
        .ok_or_else(|| "This instance is no longer in the list.".to_owned())?;
    if is_running(directory) {
        return Err(format!(
            "{} started while this was open. Close Minecraft, then try again.",
            profile.name
        ));
    }
    let loader = crate::loaders::ModLoader::from_label(loader_label)
        .ok_or_else(|| format!("Unknown mod loader: {loader_label}"))?;
    Ok(PreparedUpdate {
        original: profile.clone(),
        new_name: new_name.to_owned(),
        version: version.to_owned(),
        loader,
    })
}

impl PreparedUpdate {
    /// Installs only shared game files; does not alter the target profile or its folder.
    pub fn install(
        self,
        paths: &AppPaths,
        progress: impl Fn(crate::loaders::InstallProgress),
    ) -> Result<Self, String> {
        crate::loaders::install_game_files(paths, &self.version, self.loader, &progress)?;
        Ok(self)
    }

    /// Rechecks running state and saves version/loader before attempting the separate rename.
    pub fn commit(
        self,
        paths: &AppPaths,
        profiles: &mut [InstanceProfile],
        skipped: &[SkippedEntry],
        is_running: &dyn Fn(&InstanceDirName) -> bool,
    ) -> Result<UpdateOutcome, EditError> {
        let directory = self.original.directory();
        commit_version_loader(
            paths,
            profiles,
            skipped,
            directory,
            &self.version,
            self.loader.label(),
            is_running,
        )?;
        let mut outcome = UpdateOutcome {
            name: self.original.name.clone(),
            rename_error: None,
        };
        if self.new_name != self.original.name {
            match rename_instance(paths, profiles, skipped, directory, &self.new_name, &|_| {
                false
            }) {
                Ok(_) => outcome.name = self.new_name,
                Err(error) => outcome.rename_error = Some(error),
            }
        }
        Ok(outcome)
    }
}

/// Renames the instance stored in `directory`. Only the display name changes; the
/// folder stays. The new name is validated with
/// [`instances::validate_instance_name`] (a case-only change of its own name is fine).
///
/// `is_running` is checked right before saving; pass `|_| false` to allow renaming a
/// running instance (harmless for the game, which never reads the manifest).
/// Returns the previous name.
pub fn rename_instance(
    paths: &AppPaths,
    profiles: &mut [InstanceProfile],
    skipped: &[SkippedEntry],
    directory: &InstanceDirName,
    new_name: &str,
    is_running: &dyn Fn(&InstanceDirName) -> bool,
) -> Result<String, EditError> {
    let index = instances::find_profile(profiles, directory)?;
    let name = instances::validate_instance_name(new_name, profiles, Some(directory))
        .map_err(InstanceError::from)?;
    if is_running(directory) {
        return Err(EditError::NowRunning);
    }
    let previous = std::mem::replace(&mut profiles[index].name, name);
    if let Err(error) = instances::save(paths, profiles, skipped) {
        profiles[index].name = previous;
        return Err(error.into());
    }
    Ok(previous)
}

/// Direction of a Minecraft version change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VersionDirection {
    Same,
    Upgrade,
    Downgrade,
    /// At least one version is not in the known order; neither warning applies as a
    /// downgrade (the UI shows the upgrade note).
    Unknown,
}

/// Compares two Minecraft version ids by their position in the version manifest
/// (`newest_first`, as returned by Mojang's manifest). Identical ids are `Equal`
/// even when unknown; otherwise `None` if either id cannot be placed.
pub fn compare_versions(a: &str, b: &str, newest_first: &[String]) -> Option<Ordering> {
    if a == b {
        return Some(Ordering::Equal);
    }
    let position = |id: &str| newest_first.iter().position(|known| known == id);
    // A smaller index is newer, so compare in reverse.
    Some(position(b)?.cmp(&position(a)?))
}

/// What a version/loader edit means for the instance's worlds and mods.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VersionChange {
    pub minecraft: VersionDirection,
    pub loader_changed: bool,
    /// The new loader is Vanilla (mods are kept but not loaded).
    pub to_vanilla: bool,
}

impl VersionChange {
    /// Older Minecraft may not open (and can damage) newer worlds: warn and offer
    /// "Duplicate first".
    pub fn is_downgrade(&self) -> bool {
        self.minecraft == VersionDirection::Downgrade
    }
    pub fn is_noop(&self) -> bool {
        self.minecraft == VersionDirection::Same && !self.loader_changed
    }
}

/// Classifies changing (`old_version`, `old_loader`) to (`new_version`, `new_loader`).
/// Loader values are the stored labels (e.g. "Fabric").
pub fn assess_version_change(
    old_version: &str,
    old_loader: &str,
    new_version: &str,
    new_loader: &str,
    newest_first: &[String],
) -> VersionChange {
    let minecraft = match compare_versions(new_version, old_version, newest_first) {
        Some(Ordering::Equal) => VersionDirection::Same,
        Some(Ordering::Greater) => VersionDirection::Upgrade,
        Some(Ordering::Less) => VersionDirection::Downgrade,
        None => VersionDirection::Unknown,
    };
    VersionChange {
        minecraft,
        loader_changed: old_loader != new_loader,
        to_vanilla: old_loader != new_loader && new_loader == "Vanilla",
    }
}

/// Commits a new version/loader to the instance in `directory` (UI thread, after the
/// install succeeded). Re-checks `is_running`; restores the old values if the save
/// fails. Returns the previous `(version, loader)`.
pub fn commit_version_loader(
    paths: &AppPaths,
    profiles: &mut [InstanceProfile],
    skipped: &[SkippedEntry],
    directory: &InstanceDirName,
    version: &str,
    loader: &str,
    is_running: &dyn Fn(&InstanceDirName) -> bool,
) -> Result<(String, String), EditError> {
    let index = instances::find_profile(profiles, directory)?;
    if is_running(directory) {
        return Err(EditError::NowRunning);
    }
    let profile = &mut profiles[index];
    let previous_version = std::mem::replace(&mut profile.version, version.to_owned());
    let previous_loader = std::mem::replace(&mut profile.loader, loader.to_owned());
    if let Err(error) = instances::save(paths, profiles, skipped) {
        profiles[index].version = previous_version;
        profiles[index].loader = previous_loader;
        return Err(error.into());
    }
    Ok((previous_version, previous_loader))
}

/// Changes the version/loader on the calling thread: checks running, runs `install`
/// (the same installer create uses; it only writes shared version/library files, never
/// the instance folder), and commits only if it succeeds. Upgrades and downgrades are
/// both allowed; use [`assess_version_change`] first to warn about downgrades.
#[allow(clippy::too_many_arguments)]
pub fn change_version_loader(
    paths: &AppPaths,
    profiles: &mut [InstanceProfile],
    skipped: &[SkippedEntry],
    directory: &InstanceDirName,
    version: &str,
    loader: &str,
    install: impl FnOnce(&str, &str) -> Result<(), String>,
    is_running: &dyn Fn(&InstanceDirName) -> bool,
) -> Result<(String, String), EditError> {
    instances::find_profile(profiles, directory)?;
    if is_running(directory) {
        return Err(EditError::NowRunning);
    }
    install(version, loader).map_err(InstanceError::Install)?;
    commit_version_loader(
        paths, profiles, skipped, directory, version, loader, is_running,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::instances::{NameError, load, save};
    use crate::core::paths::test_support::paths_in;
    use std::cell::Cell;
    use std::fs;

    fn setup() -> (tempfile::TempDir, AppPaths, Vec<InstanceProfile>) {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths_in(dir.path());
        let profiles = vec![
            InstanceProfile::new("Foo".into(), "1.20.1".into(), "Fabric".into(), &[]),
            InstanceProfile::new("Bar".into(), "1.20.1".into(), "Vanilla".into(), &[]),
        ];
        save(&paths, &profiles, &[]).unwrap();
        (dir, paths, profiles)
    }

    fn not_running(_: &InstanceDirName) -> bool {
        false
    }

    #[test]
    fn prepared_update_rechecks_running_before_commit() {
        let (_dir, paths, mut profiles) = setup();
        let folder = profiles[0].directory().clone();
        let update =
            prepare_update(&profiles, &folder, "New", "1.21.1", "Vanilla", &not_running).unwrap();
        assert!(matches!(
            update.commit(&paths, &mut profiles, &[], &|_| true),
            Err(EditError::NowRunning)
        ));
        assert_eq!(profiles[0].version, "1.20.1");
        assert_eq!(profiles[0].name, "Foo");
    }

    #[test]
    fn prepared_update_keeps_saved_version_when_rename_fails() {
        let (_dir, paths, mut profiles) = setup();
        let folder = profiles[0].directory().clone();
        let update =
            prepare_update(&profiles, &folder, "Bar", "1.21.1", "Vanilla", &not_running).unwrap();
        let outcome = update
            .commit(&paths, &mut profiles, &[], &not_running)
            .unwrap();
        assert_eq!(outcome.name, "Foo");
        assert!(outcome.rename_error.is_some());
        let saved = load(&paths).unwrap().profiles;
        assert_eq!(saved[0].name, "Foo");
        assert_eq!(saved[0].version, "1.21.1");
        assert_eq!(saved[0].loader, "Vanilla");
    }

    #[test]
    fn prepared_update_saves_version_then_rename_and_rolls_back_failed_version_save() {
        let (_dir, paths, mut profiles) = setup();
        let folder = profiles[0].directory().clone();
        let update =
            prepare_update(&profiles, &folder, "New", "1.21.1", "Vanilla", &not_running).unwrap();
        fs::write(paths.instances_manifest(), "{ corrupt").unwrap();
        assert!(matches!(
            update.commit(&paths, &mut profiles, &[], &not_running),
            Err(EditError::Failed(_))
        ));
        assert_eq!(profiles[0].version, "1.20.1");
        assert_eq!(profiles[0].name, "Foo");
        fs::remove_file(paths.instances_manifest()).unwrap();
        let update =
            prepare_update(&profiles, &folder, "New", "1.21.1", "Vanilla", &not_running).unwrap();
        let outcome = update
            .commit(&paths, &mut profiles, &[], &not_running)
            .unwrap();
        assert_eq!(outcome.name, "New");
        assert!(outcome.rename_error.is_none());
        assert_eq!(load(&paths).unwrap().profiles[0].name, "New");
    }

    #[test]
    fn rename_changes_only_the_name() {
        let (_dir, paths, mut profiles) = setup();
        let folder = profiles[0].directory().clone();
        let previous = rename_instance(
            &paths,
            &mut profiles,
            &[],
            &folder,
            " Survival ",
            &not_running,
        )
        .unwrap();
        assert_eq!(previous, "Foo");
        let reloaded = load(&paths).unwrap().profiles;
        assert_eq!(reloaded[0].name, "Survival");
        assert_eq!(reloaded[0].directory(), &folder, "the folder never changes");
    }

    #[test]
    fn rename_refuses_case_insensitive_collision() {
        let (_dir, paths, mut profiles) = setup();
        let folder = profiles[0].directory().clone();
        let error =
            rename_instance(&paths, &mut profiles, &[], &folder, "bar", &not_running).unwrap_err();
        assert!(
            matches!(error, EditError::Failed(InstanceError::Name(NameError::Taken(ref n))) if n == "Bar")
        );
        // A case-only change of its own name is fine.
        rename_instance(&paths, &mut profiles, &[], &folder, "FOO", &not_running).unwrap();
        assert_eq!(profiles[0].name, "FOO");
    }

    #[test]
    fn rename_rolls_back_when_save_fails() {
        let (_dir, paths, mut profiles) = setup();
        let folder = profiles[0].directory().clone();
        fs::write(paths.instances_manifest(), "{ corrupt").unwrap();
        let error =
            rename_instance(&paths, &mut profiles, &[], &folder, "New", &not_running).unwrap_err();
        assert!(matches!(
            error,
            EditError::Failed(InstanceError::RefusingToOverwrite(_))
        ));
        assert_eq!(profiles[0].name, "Foo");
        assert_eq!(
            fs::read_to_string(paths.instances_manifest()).unwrap(),
            "{ corrupt"
        );
    }

    #[test]
    fn edits_report_now_running() {
        let (_dir, paths, mut profiles) = setup();
        let folder = profiles[0].directory().clone();
        let error =
            rename_instance(&paths, &mut profiles, &[], &folder, "New", &|_| true).unwrap_err();
        assert!(matches!(error, EditError::NowRunning));
        // Starts while the installer runs: the install happens, the commit is refused.
        let checks = Cell::new(0);
        let starts_later = |_: &InstanceDirName| {
            checks.set(checks.get() + 1);
            checks.get() > 1
        };
        let error = change_version_loader(
            &paths,
            &mut profiles,
            &[],
            &folder,
            "1.21.1",
            "Fabric",
            |_, _| Ok(()),
            &starts_later,
        )
        .unwrap_err();
        assert!(matches!(error, EditError::NowRunning));
        assert_eq!(profiles[0].version, "1.20.1");
        assert_eq!(load(&paths).unwrap().profiles[0].version, "1.20.1");
    }

    #[test]
    fn version_change_commits_only_after_install_succeeds() {
        let (_dir, paths, mut profiles) = setup();
        let folder = profiles[0].directory().clone();
        let error = change_version_loader(
            &paths,
            &mut profiles,
            &[],
            &folder,
            "1.21.1",
            "NeoForge",
            |_, _| Err("download failed".into()),
            &not_running,
        )
        .unwrap_err();
        assert!(matches!(
            error,
            EditError::Failed(InstanceError::Install(_))
        ));
        assert_eq!(load(&paths).unwrap().profiles[0].version, "1.20.1");

        let installed = Cell::new(false);
        let previous = change_version_loader(
            &paths,
            &mut profiles,
            &[],
            &folder,
            "1.21.1",
            "NeoForge",
            |version, loader| {
                assert_eq!((version, loader), ("1.21.1", "NeoForge"));
                installed.set(true);
                Ok(())
            },
            &not_running,
        )
        .unwrap();
        assert!(installed.get());
        assert_eq!(previous, ("1.20.1".into(), "Fabric".into()));
        let reloaded = &load(&paths).unwrap().profiles[0];
        assert_eq!(
            (reloaded.version.as_str(), reloaded.loader.as_str()),
            ("1.21.1", "NeoForge")
        );
        assert_eq!(reloaded.directory(), &folder);
    }

    #[test]
    fn commit_rolls_back_when_save_fails() {
        let (_dir, paths, mut profiles) = setup();
        let folder = profiles[0].directory().clone();
        fs::write(paths.instances_manifest(), "{ corrupt").unwrap();
        let error = commit_version_loader(
            &paths,
            &mut profiles,
            &[],
            &folder,
            "1.21.1",
            "Vanilla",
            &not_running,
        )
        .unwrap_err();
        assert!(matches!(error, EditError::Failed(_)));
        assert_eq!(
            (profiles[0].version.as_str(), profiles[0].loader.as_str()),
            ("1.20.1", "Fabric")
        );
    }

    #[test]
    fn downgrades_are_detected_from_manifest_order() {
        let order: Vec<String> = ["26.2", "1.21.1", "1.20.1", "1.8.9"]
            .into_iter()
            .map(String::from)
            .collect();
        let change = assess_version_change("1.21.1", "Fabric", "1.20.1", "Fabric", &order);
        assert!(change.is_downgrade());
        assert!(!change.loader_changed);
        let change = assess_version_change("1.20.1", "Fabric", "26.2", "Vanilla", &order);
        assert_eq!(change.minecraft, VersionDirection::Upgrade);
        assert!(change.loader_changed && change.to_vanilla);
        let change = assess_version_change("1.20.1", "Fabric", "24w14a", "Fabric", &order);
        assert_eq!(change.minecraft, VersionDirection::Unknown);
        assert!(!change.is_downgrade());
        assert!(assess_version_change("x", "Quilt", "x", "Quilt", &[]).is_noop());
        assert_eq!(
            compare_versions("1.8.9", "1.20.1", &order),
            Some(Ordering::Less)
        );
    }
}
