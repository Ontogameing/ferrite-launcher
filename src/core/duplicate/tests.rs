//! Duplicate tests; everything runs inside tempdirs.

use super::*;
use crate::core::instances::{load, save};
use crate::core::paths::test_support::paths_in;
use std::cell::Cell;
use std::collections::BTreeMap;

struct Fixture {
    _temp: tempfile::TempDir,
    paths: AppPaths,
    profiles: Vec<InstanceProfile>,
    skipped: Vec<SkippedEntry>,
}

fn dir(name: &str) -> InstanceDirName {
    InstanceDirName::parse(name.to_owned()).unwrap()
}

/// One instance `Alpha` in folder `alpha` with a world, a mod and options.
fn fixture() -> Fixture {
    let temp = tempfile::tempdir().unwrap();
    let paths = paths_in(temp.path());
    let alpha = paths.instances_dir().join("alpha");
    fs::create_dir_all(alpha.join("saves/New World/region")).unwrap();
    fs::create_dir_all(alpha.join("mods")).unwrap();
    fs::create_dir_all(alpha.join("empty folder")).unwrap();
    fs::write(alpha.join("saves/New World/level.dat"), b"level").unwrap();
    fs::write(
        alpha.join("saves/New World/region/r.0.0.mca"),
        vec![7_u8; 4096],
    )
    .unwrap();
    fs::write(alpha.join("mods/a.jar"), b"jar").unwrap();
    fs::write(alpha.join("options.txt"), b"fov:70").unwrap();
    let profiles = vec![InstanceProfile::with_directory(
        "Alpha".into(),
        "1.20.1".into(),
        "Fabric".into(),
        dir("alpha"),
    )];
    save(&paths, &profiles, &[]).unwrap();
    Fixture {
        _temp: temp,
        paths,
        profiles,
        skipped: Vec::new(),
    }
}

impl Fixture {
    fn manifest(&self) -> String {
        fs::read_to_string(self.paths.instances_manifest()).unwrap()
    }
    fn instances_entries(&self) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(self.paths.instances_dir())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }
    fn plan(&self, name: &str) -> DuplicatePlan {
        prepare_duplicate(
            &self.paths,
            &self.profiles,
            &self.skipped,
            &dir("alpha"),
            name,
            &|_| false,
        )
        .unwrap()
    }
}

/// Relative path -> contents for every file under `root` (no links followed).
fn snapshot(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    let scan = copy::scan_tree(root).unwrap();
    scan.files
        .keys()
        .map(|relative| (relative.clone(), fs::read(root.join(relative)).unwrap()))
        .collect()
}

#[test]
fn duplicate_copies_everything_and_adds_the_entry() {
    let mut f = fixture();
    let source = f.paths.instances_dir().join("alpha");
    let before = snapshot(&source);
    let control = DuplicateControl::default();
    let skipped = f.skipped.clone();
    let report = duplicate_instance(
        &f.paths,
        &mut f.profiles,
        &skipped,
        &dir("alpha"),
        "  Alpha (copy) ",
        &control,
        &|_| false,
    )
    .unwrap();

    assert_eq!(report.profile.name, "Alpha (copy)");
    assert_eq!(report.profile.version, "1.20.1");
    assert_eq!(report.profile.loader, "Fabric");
    assert_eq!(report.index, 1);
    assert_eq!(report.files_copied, 4);
    assert!(report.skipped_links.is_empty());
    let copy = report.profile.game_dir(&f.paths);
    assert_ne!(copy, source);
    assert_eq!(snapshot(&copy), before);
    assert!(copy.join("empty folder").is_dir());
    // The source is unchanged and nothing temporary is left behind.
    assert_eq!(snapshot(&source), before);
    let entries = f.instances_entries();
    assert_eq!(entries.len(), 2, "{entries:?}");
    let loaded = load(&f.paths).unwrap();
    assert_eq!(loaded.profiles.len(), 2);
    assert_eq!(loaded.profiles[1].name, "Alpha (copy)");
    assert_eq!(loaded.profiles[1].directory(), report.profile.directory());
    assert_eq!(control.snapshot().step, DuplicateStep::Done);
    assert_eq!(control.snapshot().fraction(), Some(1.0));
}

#[cfg(unix)]
#[test]
fn duplicate_shares_no_inodes_with_the_source() {
    use std::os::unix::fs::MetadataExt;
    let mut f = fixture();
    let skipped = f.skipped.clone();
    let report = duplicate_instance(
        &f.paths,
        &mut f.profiles,
        &skipped,
        &dir("alpha"),
        "Copy",
        &DuplicateControl::default(),
        &|_| false,
    )
    .unwrap();
    let source = f.paths.instances_dir().join("alpha");
    let copy = report.profile.game_dir(&f.paths);
    let scan = copy::scan_tree(&source).unwrap();
    assert!(!scan.files.is_empty());
    for relative in scan.files.keys() {
        let original = fs::metadata(source.join(relative)).unwrap();
        let copied = fs::metadata(copy.join(relative)).unwrap();
        assert_ne!(
            (original.dev(), original.ino()),
            (copied.dev(), copied.ino()),
            "{} shares an inode",
            relative.display()
        );
        assert_eq!(original.nlink(), 1);
        assert_eq!(copied.nlink(), 1);
    }
}

#[cfg(unix)]
#[test]
fn links_are_not_followed_and_are_listed() {
    let mut f = fixture();
    let outside = f._temp.path().join("outside");
    fs::create_dir_all(&outside).unwrap();
    fs::write(outside.join("secret.txt"), b"outside").unwrap();
    let source = f.paths.instances_dir().join("alpha");
    std::os::unix::fs::symlink(&outside, source.join("linked-folder")).unwrap();
    std::os::unix::fs::symlink(outside.join("secret.txt"), source.join("mods/link.jar")).unwrap();
    let skipped = f.skipped.clone();
    let report = duplicate_instance(
        &f.paths,
        &mut f.profiles,
        &skipped,
        &dir("alpha"),
        "Copy",
        &DuplicateControl::default(),
        &|_| false,
    )
    .unwrap();
    assert_eq!(
        report.skipped_links,
        vec![
            PathBuf::from("linked-folder"),
            PathBuf::from("mods/link.jar")
        ]
    );
    let copy = report.profile.game_dir(&f.paths);
    assert!(fs::symlink_metadata(copy.join("linked-folder")).is_err());
    assert!(fs::symlink_metadata(copy.join("mods/link.jar")).is_err());
    assert!(copy.join("mods/a.jar").is_file());
    assert_eq!(fs::read(outside.join("secret.txt")).unwrap(), b"outside");
}

#[cfg(unix)]
#[test]
fn uncopyable_files_block_the_duplicate() {
    let f = fixture();
    let fifo = f.paths.instances_dir().join("alpha/pipe");
    let c_path = std::ffi::CString::new(fifo.to_str().unwrap()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) }, 0);
    let before_manifest = f.manifest();
    let before_entries = f.instances_entries();
    let plan = f.plan("Copy");
    match copy_duplicate(&f.paths, &plan, &DuplicateControl::default(), &|_| false) {
        Err(DuplicateError::UncopyableFiles(files)) => {
            assert_eq!(files.len(), 1);
            assert_eq!(files[0].path, PathBuf::from("pipe"));
        }
        other => panic!("expected UncopyableFiles, got {other:?}"),
    }
    assert_eq!(f.instances_entries(), before_entries);
    assert_eq!(f.manifest(), before_manifest);
}

#[test]
fn not_enough_space_fails_before_creating_anything() {
    let f = fixture();
    let before_entries = f.instances_entries();
    let plan = f.plan("Copy");
    let hooks = Hooks {
        available_space: Some(&|| 100),
        ..Hooks::default()
    };
    match copy_with_hooks(
        &f.paths,
        &plan,
        &DuplicateControl::default(),
        &|_| false,
        &hooks,
    ) {
        Err(DuplicateError::NotEnoughSpace {
            needed, available, ..
        }) => {
            assert_eq!(available, 100);
            assert!(needed > 4096, "needed {needed}");
        }
        other => panic!("expected NotEnoughSpace, got {other:?}"),
    }
    assert_eq!(f.instances_entries(), before_entries);
}

#[test]
fn cancel_removes_only_the_staging_folder() {
    let f = fixture();
    // An unrelated folder that merely looks like staging must survive.
    let lookalike = f.paths.instances_dir().join(".beta.duplicate.999.1.tmp");
    fs::create_dir(&lookalike).unwrap();
    fs::write(lookalike.join("keep"), b"keep").unwrap();
    let source = f.paths.instances_dir().join("alpha");
    let before = snapshot(&source);
    let before_entries = f.instances_entries();
    let before_manifest = f.manifest();

    let plan = f.plan("Copy");
    let control = DuplicateControl::default();
    let staging_seen = Cell::new(false);
    let paths = f.paths.clone();
    let after_file = |files_done: u64| -> io::Result<()> {
        if files_done == 1 {
            // Mid-copy: staging exists with partial content.
            let staging: Vec<_> = fs::read_dir(paths.instances_dir())
                .unwrap()
                .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
                .filter(|name| name.starts_with(".copy.duplicate."))
                .collect();
            staging_seen.set(staging.len() == 1);
            control.cancel();
        }
        Ok(())
    };
    let hooks = Hooks {
        after_file: Some(&after_file),
        ..Hooks::default()
    };
    let result = copy_with_hooks(&f.paths, &plan, &control, &|_| false, &hooks);
    assert!(
        matches!(result, Err(DuplicateError::Cancelled)),
        "{result:?}"
    );
    assert!(staging_seen.get());
    assert_eq!(f.instances_entries(), before_entries);
    assert_eq!(fs::read(lookalike.join("keep")).unwrap(), b"keep");
    assert_eq!(snapshot(&source), before);
    assert_eq!(f.manifest(), before_manifest);
}

#[test]
fn final_rename_refuses_an_existing_empty_folder() {
    let f = fixture();
    let plan = f.plan("Copy");
    let target = plan.profile().game_dir(&f.paths);
    let create_target = || fs::create_dir(&target).unwrap();
    let hooks = Hooks {
        before_rename: Some(&create_target),
        ..Hooks::default()
    };
    let result = copy_with_hooks(
        &f.paths,
        &plan,
        &DuplicateControl::default(),
        &|_| false,
        &hooks,
    );
    match result {
        Err(DuplicateError::TargetExists(path)) => assert_eq!(path, target),
        other => panic!("expected TargetExists, got {other:?}"),
    }
    // The racing folder is untouched (still empty) and staging is gone.
    assert_eq!(fs::read_dir(&target).unwrap().count(), 0);
    let mut expected = vec!["alpha".to_owned(), "copy".to_owned()];
    expected.sort();
    assert_eq!(f.instances_entries(), expected);
}

#[test]
fn running_is_checked_when_preparing_and_when_the_copy_starts() {
    let f = fixture();
    let refused = prepare_duplicate(
        &f.paths,
        &f.profiles,
        &f.skipped,
        &dir("alpha"),
        "Copy",
        &|directory| directory.as_str() == "alpha",
    );
    assert!(matches!(refused, Err(DuplicateError::NowRunning)));

    // Idle when the dialog opened, started before the copy began.
    let plan = f.plan("Copy");
    let before_entries = f.instances_entries();
    let result = copy_duplicate(&f.paths, &plan, &DuplicateControl::default(), &|_| true);
    assert!(matches!(result, Err(DuplicateError::NowRunning)));
    assert_eq!(f.instances_entries(), before_entries);

    // Started during the copy: refused before committing, staging removed.
    let calls = Cell::new(0);
    let started_later = |_: &InstanceDirName| {
        calls.set(calls.get() + 1);
        calls.get() > 1
    };
    let result = copy_duplicate(
        &f.paths,
        &plan,
        &DuplicateControl::default(),
        &started_later,
    );
    assert!(matches!(result, Err(DuplicateError::NowRunning)));
    assert_eq!(calls.get(), 2);
    assert_eq!(f.instances_entries(), before_entries);
}

#[test]
fn source_changed_during_copy_is_not_committed() {
    let f = fixture();
    let plan = f.plan("Copy");
    let before_entries = f.instances_entries();
    let modify = |source: &Path| fs::write(source.join("options.txt"), b"fov:110 changed").unwrap();
    let hooks = Hooks {
        before_recheck: Some(&modify),
        ..Hooks::default()
    };
    let result = copy_with_hooks(
        &f.paths,
        &plan,
        &DuplicateControl::default(),
        &|_| false,
        &hooks,
    );
    match result {
        Err(DuplicateError::SourceChanged(detail)) => assert!(detail.contains("options.txt")),
        other => panic!("expected SourceChanged, got {other:?}"),
    }
    assert_eq!(f.instances_entries(), before_entries);
}

#[test]
fn name_is_validated_and_folder_missing_is_reported() {
    let f = fixture();
    let taken = prepare_duplicate(
        &f.paths,
        &f.profiles,
        &f.skipped,
        &dir("alpha"),
        "alpha",
        &|_| false,
    );
    assert!(matches!(
        taken,
        Err(DuplicateError::Failed(InstanceError::Name(
            instances::NameError::Taken(ref existing)
        ))) if existing == "Alpha"
    ));
    fs::remove_dir_all(f.paths.instances_dir().join("alpha")).unwrap();
    let missing = prepare_duplicate(
        &f.paths,
        &f.profiles,
        &f.skipped,
        &dir("alpha"),
        "Copy",
        &|_| false,
    );
    assert!(matches!(missing, Err(DuplicateError::SourceMissing)));
}

#[test]
fn folder_allocation_skips_folders_already_on_disk() {
    let f = fixture();
    fs::create_dir(f.paths.instances_dir().join("COPY")).unwrap();
    let plan = f.plan("Copy");
    assert_eq!(plan.profile().directory().as_str(), "copy-2");
}

#[test]
fn save_failure_removes_only_the_new_folder() {
    let mut f = fixture();
    let source = f.paths.instances_dir().join("alpha");
    let before = snapshot(&source);
    let plan = f.plan("Copy");
    let copied = copy_duplicate(&f.paths, &plan, &DuplicateControl::default(), &|_| false).unwrap();
    let new_folder = copied.profile.game_dir(&f.paths);
    assert!(new_folder.is_dir());
    // A manifest this build can't read is never overwritten.
    fs::write(f.paths.instances_manifest(), "{ corrupt").unwrap();
    let skipped = f.skipped.clone();
    let result = commit_duplicate(&f.paths, &mut f.profiles, &skipped, copied, None);
    assert!(matches!(
        result,
        Err(DuplicateError::Failed(InstanceError::RefusingToOverwrite(
            _
        )))
    ));
    assert!(!new_folder.exists());
    assert_eq!(snapshot(&source), before);
    assert_eq!(f.profiles.len(), 1);
}

#[test]
fn task_runs_in_the_background_and_commits() {
    let mut f = fixture();
    let plan = f.plan("Copy");
    let task = DuplicateTask::spawn(f.paths.clone(), plan, |_| false).unwrap();
    let copied = loop {
        if let Some(result) = task.try_finish() {
            break result.unwrap();
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    };
    let skipped = f.skipped.clone();
    let report = commit_duplicate(
        &f.paths,
        &mut f.profiles,
        &skipped,
        copied,
        Some(task.control()),
    )
    .unwrap();
    assert_eq!(report.profile.name, "Copy");
    assert_eq!(task.control().snapshot().step, DuplicateStep::Done);
    assert_eq!(load(&f.paths).unwrap().profiles.len(), 2);
}

#[test]
fn staging_names_match_the_sweep_pattern() {
    let name = staging_dir_name(&dir("my-pack"));
    assert!(name.starts_with(".my-pack.duplicate."), "{name}");
    assert!(name.ends_with(".tmp"));
    assert_ne!(name, staging_dir_name(&dir("my-pack")));
}
