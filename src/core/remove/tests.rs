//! Removal tests; everything runs inside tempdirs with a fake trash backend.

use super::*;
use crate::core::instances::{load, save};
use crate::core::paths::test_support::paths_in;
use std::cell::Cell;
use std::sync::Mutex;

/// Fake trash: records calls and either moves the folder away, fails, or lies.
struct FakeTrash {
    behavior: Behavior,
    calls: Mutex<Vec<PathBuf>>,
    bin: PathBuf,
}

#[derive(Clone, Copy)]
enum Behavior {
    Move,
    Fail,
    /// Returns Ok but leaves the folder where it is.
    OkButStays,
    /// Moves the folder away but still reports an error.
    MoveThenFail,
}

impl FakeTrash {
    fn new(bin: &Path, behavior: Behavior) -> Self {
        fs::create_dir_all(bin).unwrap();
        Self {
            behavior,
            calls: Mutex::new(Vec::new()),
            bin: bin.to_path_buf(),
        }
    }
    fn calls(&self) -> usize {
        self.calls.lock().unwrap().len()
    }
}

impl Trasher for FakeTrash {
    fn trash(&self, path: &Path) -> Result<(), TrashError> {
        self.calls.lock().unwrap().push(path.to_path_buf());
        let failure = TrashError {
            kind: TrashFailureKind::InUse,
            message: "file in use".into(),
        };
        match self.behavior {
            Behavior::Move => {
                fs::rename(path, self.bin.join(path.file_name().unwrap())).unwrap();
                Ok(())
            }
            Behavior::Fail => Err(failure),
            Behavior::OkButStays => Ok(()),
            Behavior::MoveThenFail => {
                fs::rename(path, self.bin.join(path.file_name().unwrap())).unwrap();
                Err(failure)
            }
        }
    }
}

fn not_running(_: &InstanceDirName) -> bool {
    false
}

struct Fixture {
    _dir: tempfile::TempDir,
    paths: AppPaths,
    profiles: Vec<InstanceProfile>,
    skipped: Vec<SkippedEntry>,
    bin: PathBuf,
}

/// Two instances (`alpha`, `beta`) with a world each, saved to the manifest.
fn fixture(extra_manifest_entries: &str) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let paths = paths_in(dir.path());
    fs::create_dir_all(paths.storage_root()).unwrap();
    for name in ["alpha", "beta"] {
        let world = paths.instances_dir().join(name).join("saves/World");
        fs::create_dir_all(&world).unwrap();
        fs::write(world.join("level.dat"), name).unwrap();
    }
    fs::write(
        paths.instances_manifest(),
        format!(
            r#"[{{"name":"Alpha","version":"1","loader":"Vanilla","directory":"alpha"}},
                {{"name":"Beta","version":"1","loader":"Vanilla","directory":"beta"}}{extra_manifest_entries}]"#
        ),
    )
    .unwrap();
    let loaded = load(&paths).unwrap();
    let bin = dir.path().join("trash-bin");
    Fixture {
        paths,
        profiles: loaded.profiles,
        skipped: loaded.skipped,
        bin,
        _dir: dir,
    }
}

fn dir(name: &str) -> InstanceDirName {
    InstanceDirName::parse(name).unwrap()
}

impl Fixture {
    fn manifest(&self) -> String {
        fs::read_to_string(self.paths.instances_manifest()).unwrap()
    }
    fn folder(&self, name: &str) -> PathBuf {
        self.paths.instances_dir().join(name)
    }
    fn remove(&mut self, name: &str, mode: RemoveMode, trash: &FakeTrash) -> RemoveOutcome {
        remove_instance(
            &self.paths,
            &mut self.profiles,
            &self.skipped,
            &dir(name),
            mode,
            trash,
            &not_running,
        )
    }
}

#[test]
fn trash_moves_folder_then_saves() {
    let mut f = fixture("");
    let trash = FakeTrash::new(&f.bin, Behavior::Move);
    let outcome = f.remove("alpha", RemoveMode::Trash, &trash);
    assert!(matches!(outcome, RemoveOutcome::Trashed { ref profile } if profile.name == "Alpha"));
    assert!(!f.folder("alpha").exists());
    assert!(f.bin.join("alpha/saves/World/level.dat").exists());
    assert!(f.folder("beta/saves/World/level.dat").exists());
    let names: Vec<_> = load(&f.paths)
        .unwrap()
        .profiles
        .into_iter()
        .map(|p| p.name)
        .collect();
    assert_eq!(names, ["Beta"]);
}

#[test]
fn trash_failure_leaves_list_manifest_and_folder_unchanged() {
    let mut f = fixture("");
    let before = f.manifest();
    let trash = FakeTrash::new(&f.bin, Behavior::Fail);
    let outcome = f.remove("alpha", RemoveMode::Trash, &trash);
    match outcome {
        RemoveOutcome::TrashFailed {
            kind,
            folder_still_present,
            ..
        } => {
            assert_eq!(kind, TrashFailureKind::InUse);
            assert!(folder_still_present);
        }
        other => panic!("expected TrashFailed, got {other:?}"),
    }
    assert_eq!(f.profiles.len(), 2);
    assert_eq!(f.manifest(), before);
    assert!(
        f.folder("alpha/saves/World/level.dat").exists(),
        "never falls back to delete"
    );
}

#[test]
fn trash_reporting_ok_but_leaving_the_folder_is_a_failure() {
    let mut f = fixture("");
    let before = f.manifest();
    let trash = FakeTrash::new(&f.bin, Behavior::OkButStays);
    let outcome = f.remove("alpha", RemoveMode::Trash, &trash);
    assert!(
        matches!(
            outcome,
            RemoveOutcome::TrashFailed {
                folder_still_present: true,
                ..
            }
        ),
        "{outcome:?}"
    );
    assert_eq!(f.profiles.len(), 2);
    assert_eq!(f.manifest(), before);
    assert!(f.folder("alpha").exists());
}

#[test]
fn trash_error_with_folder_gone_counts_as_trashed() {
    let mut f = fixture("");
    let trash = FakeTrash::new(&f.bin, Behavior::MoveThenFail);
    let outcome = f.remove("alpha", RemoveMode::Trash, &trash);
    assert!(
        matches!(outcome, RemoveOutcome::Trashed { .. }),
        "{outcome:?}"
    );
    assert_eq!(load(&f.paths).unwrap().profiles.len(), 1);
}

#[test]
fn save_failure_after_trash_keeps_entry_as_folder_missing() {
    let mut f = fixture("");
    let trash = FakeTrash::new(&f.bin, Behavior::Move);
    let target = prepare_removal(
        &f.paths,
        &f.profiles,
        &f.skipped,
        &dir("alpha"),
        RemoveMode::Trash,
        &not_running,
    )
    .unwrap();
    trash_target(&f.paths, &target, &trash, &not_running).unwrap();
    // The manifest becomes unwritable (unreadable by this build) before the save.
    fs::write(f.paths.instances_manifest(), "{ corrupt").unwrap();
    let outcome = finish_trash(&f.paths, &mut f.profiles, &f.skipped, target);
    assert!(
        matches!(outcome, RemoveOutcome::TrashedButNotSaved { .. }),
        "{outcome:?}"
    );
    assert_eq!(f.profiles.len(), 2);
    assert!(instances::folder_missing(&f.paths, &f.profiles[0]));
    // Trash/Permanent now report the missing folder; KeepFiles can still remove it.
    fs::remove_file(f.paths.instances_manifest()).unwrap();
    let outcome = f.remove("alpha", RemoveMode::Trash, &trash);
    assert!(
        matches!(outcome, RemoveOutcome::FolderMissing),
        "{outcome:?}"
    );
    let outcome = f.remove("alpha", RemoveMode::Permanent, &trash);
    assert!(
        matches!(outcome, RemoveOutcome::FolderMissing),
        "{outcome:?}"
    );
    let outcome = f.remove("alpha", RemoveMode::KeepFiles, &trash);
    assert!(
        matches!(outcome, RemoveOutcome::RemovedFromList { .. }),
        "{outcome:?}"
    );
    assert_eq!(load(&f.paths).unwrap().profiles.len(), 1);
}

#[test]
fn shared_folder_is_refused_for_trash_and_permanent() {
    // A preserved duplicate entry and a valid profile share `alpha` (case-insensitive).
    let mut f = fixture(r#",{"name":"Copy","version":"1","loader":"Vanilla","directory":"ALPHA"}"#);
    assert_eq!(f.skipped.len(), 1);
    let before = f.manifest();
    let trash = FakeTrash::new(&f.bin, Behavior::Move);
    for mode in [RemoveMode::Trash, RemoveMode::Permanent] {
        let outcome = f.remove("alpha", mode, &trash);
        assert!(
            matches!(outcome, RemoveOutcome::SharedFolder { .. }),
            "{outcome:?}"
        );
    }
    assert_eq!(trash.calls(), 0);
    assert_eq!(f.manifest(), before);
    assert!(f.folder("alpha/saves/World/level.dat").exists());

    // Remove-from-list keeps the files and the preserved entry.
    let outcome = f.remove("alpha", RemoveMode::KeepFiles, &trash);
    assert!(
        matches!(outcome, RemoveOutcome::RemovedFromList { .. }),
        "{outcome:?}"
    );
    assert_eq!(f.profiles.len(), 1);
    assert!(f.folder("alpha/saves/World/level.dat").exists());
    // The preserved entry is written back; with `Alpha` gone it now loads normally
    // and still finds its files.
    let names: Vec<_> = load(&f.paths)
        .unwrap()
        .profiles
        .into_iter()
        .map(|p| p.name)
        .collect();
    assert_eq!(names, ["Beta", "Copy"]);
}

#[test]
fn shared_folder_between_two_profiles_is_refused() {
    let mut f = fixture("");
    // Simulate an in-memory list where two profiles claim one folder.
    f.profiles.push(InstanceProfile::with_directory(
        "Twin".into(),
        "1".into(),
        "Vanilla".into(),
        dir("Beta"),
    ));
    let trash = FakeTrash::new(&f.bin, Behavior::Move);
    let outcome = f.remove("beta", RemoveMode::Trash, &trash);
    assert!(
        matches!(outcome, RemoveOutcome::SharedFolder { .. }),
        "{outcome:?}"
    );
    assert_eq!(trash.calls(), 0);
}

#[test]
fn running_check_happens_at_the_moment_of_acting() {
    let mut f = fixture("");
    let before = f.manifest();
    let trash = FakeTrash::new(&f.bin, Behavior::Move);
    // Not running while the dialog is prepared, running by the time the user confirms.
    let checks = Cell::new(0);
    let starts_later = |_: &InstanceDirName| {
        checks.set(checks.get() + 1);
        checks.get() > 1
    };
    for mode in [
        RemoveMode::Trash,
        RemoveMode::Permanent,
        RemoveMode::KeepFiles,
    ] {
        checks.set(0);
        let outcome = remove_instance(
            &f.paths,
            &mut f.profiles,
            &f.skipped,
            &dir("alpha"),
            mode,
            &trash,
            &starts_later,
        );
        assert!(
            matches!(outcome, RemoveOutcome::NowRunning),
            "{mode:?}: {outcome:?}"
        );
        assert_eq!(
            checks.get(),
            2,
            "checked when preparing and again when acting"
        );
    }
    assert_eq!(trash.calls(), 0);
    assert_eq!(f.profiles.len(), 2);
    assert_eq!(f.manifest(), before);
    assert!(f.folder("alpha/saves/World/level.dat").exists());

    // Running from the start is refused up front.
    let outcome = remove_instance(
        &f.paths,
        &mut f.profiles,
        &f.skipped,
        &dir("alpha"),
        RemoveMode::Trash,
        &trash,
        &|_| true,
    );
    assert!(matches!(outcome, RemoveOutcome::NowRunning));
}

#[test]
fn permanent_saves_first_and_deletes_nothing_if_save_fails() {
    let mut f = fixture("");
    let trash = FakeTrash::new(&f.bin, Behavior::Move);
    fs::write(
        f.paths.instances_manifest(),
        r#"{"schema_version": 99, "instances": []}"#,
    )
    .unwrap();
    let outcome = f.remove("alpha", RemoveMode::Permanent, &trash);
    assert!(
        matches!(
            outcome,
            RemoveOutcome::Failed(InstanceError::RefusingToOverwrite(_))
        ),
        "{outcome:?}"
    );
    assert_eq!(f.profiles.len(), 2);
    assert!(f.folder("alpha/saves/World/level.dat").exists());

    fs::remove_file(f.paths.instances_manifest()).unwrap();
    save(&f.paths, &f.profiles, &f.skipped).unwrap();
    let outcome = f.remove("alpha", RemoveMode::Permanent, &trash);
    assert!(
        matches!(outcome, RemoveOutcome::Deleted { .. }),
        "{outcome:?}"
    );
    assert!(!f.folder("alpha").exists());
    assert!(f.folder("beta/saves/World/level.dat").exists());
    assert_eq!(trash.calls(), 0, "permanent delete never uses the trash");
    assert_eq!(load(&f.paths).unwrap().profiles.len(), 1);
}

#[test]
fn keep_files_leaves_folder_and_blocks_its_reuse() {
    let mut f = fixture("");
    let trash = FakeTrash::new(&f.bin, Behavior::Move);
    let outcome = f.remove("alpha", RemoveMode::KeepFiles, &trash);
    match outcome {
        RemoveOutcome::RemovedFromList { folder, .. } => assert_eq!(folder, f.folder("alpha")),
        other => panic!("{other:?}"),
    }
    assert!(f.folder("alpha/saves/World/level.dat").exists());
    assert_eq!(trash.calls(), 0);
    // A new instance with the same name never adopts the kept folder.
    let new =
        instances::new_instance_profile(&f.paths, "Alpha", "1", "Vanilla", &f.profiles, &f.skipped)
            .unwrap();
    assert_eq!(new.directory().as_str(), "alpha-2");
}

#[test]
fn containment_refuses_escaping_folders() {
    let mut f = fixture("");
    let outside = f._dir.path().join("outside");
    fs::create_dir_all(&outside).unwrap();
    fs::write(outside.join("keep.txt"), b"keep").unwrap();
    f.profiles
        .push(InstanceProfile::with_unchecked_directory_for_tests(
            "Evil",
            "../../../outside",
        ));
    let trash = FakeTrash::new(&f.bin, Behavior::Move);
    for mode in [
        RemoveMode::Trash,
        RemoveMode::Permanent,
        RemoveMode::KeepFiles,
    ] {
        let outcome = remove_instance(
            &f.paths,
            &mut f.profiles,
            &f.skipped,
            &InstanceDirName::unchecked_for_tests("../../../outside"),
            mode,
            &trash,
            &not_running,
        );
        assert!(matches!(outcome, RemoveOutcome::Failed(_)), "{outcome:?}");
    }
    assert_eq!(trash.calls(), 0);
    assert!(outside.join("keep.txt").exists());
    // The instances root itself can never be a target.
    f.profiles
        .push(InstanceProfile::with_unchecked_directory_for_tests(
            "Root", ".",
        ));
    let outcome = remove_instance(
        &f.paths,
        &mut f.profiles,
        &f.skipped,
        &InstanceDirName::unchecked_for_tests("."),
        RemoveMode::Permanent,
        &trash,
        &not_running,
    );
    assert!(matches!(outcome, RemoveOutcome::Failed(_)), "{outcome:?}");
    assert!(f.folder("beta").exists());
}

#[test]
fn unknown_instance_is_reported() {
    let mut f = fixture("");
    let trash = FakeTrash::new(&f.bin, Behavior::Move);
    let outcome = f.remove("gamma", RemoveMode::Trash, &trash);
    assert!(matches!(
        outcome,
        RemoveOutcome::Failed(InstanceError::NotFound(_))
    ));
}

#[cfg(unix)]
#[test]
fn a_link_at_the_folder_path_is_removed_as_the_link() {
    let mut f = fixture("");
    let target = f._dir.path().join("real-data");
    fs::create_dir_all(&target).unwrap();
    fs::write(target.join("keep.txt"), b"keep").unwrap();
    fs::remove_dir_all(f.folder("alpha")).unwrap();
    std::os::unix::fs::symlink(&target, f.folder("alpha")).unwrap();
    let trash = FakeTrash::new(&f.bin, Behavior::Move);
    let outcome = f.remove("alpha", RemoveMode::Permanent, &trash);
    assert!(
        matches!(outcome, RemoveOutcome::Deleted { .. }),
        "{outcome:?}"
    );
    assert!(fs::symlink_metadata(f.folder("alpha")).is_err());
    assert!(target.join("keep.txt").exists());
}

#[cfg(unix)]
#[test]
fn partial_permanent_delete_lists_leftovers() {
    use std::os::unix::fs::PermissionsExt;
    let mut f = fixture("");
    let locked = f.folder("alpha/locked");
    fs::create_dir_all(&locked).unwrap();
    fs::write(locked.join("stuck.txt"), b"x").unwrap();
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o555)).unwrap();
    let trash = FakeTrash::new(&f.bin, Behavior::Move);
    let outcome = f.remove("alpha", RemoveMode::Permanent, &trash);
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
    match outcome {
        RemoveOutcome::PartiallyDeleted { failed, folder, .. } => {
            assert_eq!(folder, f.folder("alpha"));
            assert!(failed.contains(&locked.join("stuck.txt")), "{failed:?}");
        }
        other => panic!("expected PartiallyDeleted, got {other:?}"),
    }
    assert_eq!(
        load(&f.paths).unwrap().profiles.len(),
        1,
        "the entry is off the list"
    );
}

#[test]
fn io_errors_map_to_trash_failure_kinds() {
    assert_eq!(
        classify_io_kind(io::ErrorKind::PermissionDenied, "x"),
        TrashFailureKind::PermissionDenied
    );
    assert_eq!(
        classify_io_kind(io::ErrorKind::ResourceBusy, "x"),
        TrashFailureKind::InUse
    );
    assert_eq!(
        classify_io_kind(io::ErrorKind::ReadOnlyFilesystem, "x"),
        TrashFailureKind::NoTrashOnDrive
    );
    assert_eq!(
        classify_io_kind(io::ErrorKind::StorageFull, "x"),
        TrashFailureKind::TooLarge
    );
    assert_eq!(
        classify_io_kind(io::ErrorKind::NotFound, "gone"),
        TrashFailureKind::Other("gone".into())
    );
}
