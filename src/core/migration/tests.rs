//! Migration tests. Everything runs inside tempdirs; real user directories are never
//! touched.

use super::*;
use crate::core::paths::StorageMode;
use crate::core::paths::test_support::paths_in;
use std::cell::Cell;
use std::time::Duration;

const V0: &str =
    r#"[{"name":"Survival","version":"1.21.1","loader":"Fabric","directory":"survival"}]"#;

/// Creates `<base>/minecraft` with a valid v0 manifest and some game files.
fn legacy_tree(base: &Path) -> PathBuf {
    let root = base.join("minecraft");
    fs::create_dir_all(root.join("instances/survival/saves/world")).unwrap();
    fs::create_dir_all(root.join("versions/1.21.1")).unwrap();
    fs::create_dir_all(root.join("libraries/org/example")).unwrap();
    fs::create_dir_all(root.join("assets/objects/ab")).unwrap();
    fs::write(root.join("instances.json"), V0).unwrap();
    fs::write(
        root.join("instances/survival/saves/world/level.dat"),
        b"world-data",
    )
    .unwrap();
    fs::write(root.join("versions/1.21.1/client.jar"), vec![7_u8; 3000]).unwrap();
    fs::write(root.join("libraries/org/example/lib.jar"), b"library").unwrap();
    fs::write(root.join("assets/objects/ab/abcdef"), b"asset").unwrap();
    fs::canonicalize(root).unwrap()
}

fn snapshot(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    let scan = scan_tree(root).unwrap();
    scan.files
        .keys()
        .map(|relative| (relative.clone(), fs::read(root.join(relative)).unwrap()))
        .collect()
}

fn expect_migration(plan: StartupPlan) -> (AppPaths, Candidate, bool) {
    match plan {
        StartupPlan::NeedsMigration {
            paths,
            source,
            resuming,
            ..
        } => (paths, source, resuming),
        other => panic!("expected NeedsMigration, got {other:?}"),
    }
}

fn crash(message: &'static str) -> io::Result<()> {
    Err(io::Error::other(message))
}

#[test]
fn candidate_detection_rejects_folders_without_valid_manifest() {
    let dir = tempfile::tempdir().unwrap();
    let paths = paths_in(&dir.path().join("ferrite"));
    // A `minecraft` folder with no instances.json (e.g. in $HOME or `/`).
    let home = dir.path().join("home");
    fs::create_dir_all(home.join("minecraft/saves")).unwrap();
    // A `minecraft` folder whose instances.json is not Ferrite's format.
    let other = dir.path().join("other");
    fs::create_dir_all(other.join("minecraft")).unwrap();
    fs::write(other.join("minecraft/instances.json"), r#"{"foo": 1}"#).unwrap();
    // ...or is a JSON array of unrelated objects.
    let array = dir.path().join("array");
    fs::create_dir_all(array.join("minecraft")).unwrap();
    fs::write(array.join("minecraft/instances.json"), r#"[{"id": 1}]"#).unwrap();
    // ...or is not even JSON.
    let text = dir.path().join("text");
    fs::create_dir_all(text.join("minecraft")).unwrap();
    fs::write(text.join("minecraft/instances.json"), "hello").unwrap();

    let dirs = legacy_candidate_dirs(&[
        Some(home.as_path()),
        Some(other.as_path()),
        Some(array.as_path()),
        Some(text.as_path()),
    ]);
    assert_eq!(dirs.len(), 4);
    for candidate in &dirs {
        assert!(
            inspect_candidate(candidate).is_err(),
            "{}",
            candidate.display()
        );
    }
    match plan(&paths, &dirs).unwrap() {
        StartupPlan::Ready { paths: ready, .. } => assert_eq!(ready, paths),
        other => panic!("expected fresh install, got {other:?}"),
    }
    assert!(!paths.storage_root().exists());
}

#[cfg(unix)]
#[test]
fn candidate_manifest_must_not_be_a_symlink() {
    let dir = tempfile::tempdir().unwrap();
    let real = legacy_tree(&dir.path().join("real"));
    let fake = dir.path().join("fake/minecraft");
    fs::create_dir_all(&fake).unwrap();
    std::os::unix::fs::symlink(real.join("instances.json"), fake.join("instances.json")).unwrap();
    assert!(inspect_candidate(&fake).is_err());
}

#[test]
fn candidates_are_canonicalized_and_deduplicated() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().join("base");
    legacy_tree(&base);
    let dotted = base.join("..").join("base");
    let dirs = legacy_candidate_dirs(&[Some(base.as_path()), Some(dotted.as_path()), None]);
    assert_eq!(dirs.len(), 1);
    assert!(inspect_candidate(&dirs[0]).is_ok());
    let missing = dir.path().join("missing");
    assert!(legacy_candidate_dirs(&[Some(missing.as_path())]).is_empty());
}

#[test]
fn no_candidates_is_a_fresh_install() {
    let dir = tempfile::tempdir().unwrap();
    let paths = paths_in(dir.path());
    assert!(matches!(
        plan(&paths, &[]).unwrap(),
        StartupPlan::Ready { .. }
    ));
    assert!(!paths.migration_state_file().exists());
}

#[test]
fn happy_path_copies_verifies_and_leaves_source_untouched() {
    let dir = tempfile::tempdir().unwrap();
    let source = legacy_tree(&dir.path().join("old"));
    let before = snapshot(&source);
    let paths = paths_in(&dir.path().join("ferrite"));

    let (paths, candidate, resuming) =
        expect_migration(plan(&paths, std::slice::from_ref(&source)).unwrap());
    assert!(!resuming);
    assert_eq!(candidate.instance_count, 1);
    assert_eq!(candidate.file_count, 5);
    assert!(candidate.total_bytes >= 3000);
    assert!(candidate.last_modified.is_some());

    let control = MigrationControl::default();
    let report = run_migration(&paths, &candidate.path, &control).unwrap();
    assert!(!report.already_complete);
    assert_eq!(report.files_copied, 5);
    let progress = control.snapshot();
    assert_eq!(progress.step, MigrationStep::Done);
    assert_eq!(progress.files_done, progress.files_total);
    assert_eq!(progress.bytes_done, progress.bytes_total);
    assert_eq!(progress.fraction(), 1.0);

    assert_eq!(snapshot(paths.storage_root()), before);
    assert_eq!(snapshot(&source), before, "source must be unchanged");
    assert!(!paths.migration_staging_dir().exists());
    let state = read_state(&paths).unwrap().unwrap();
    assert_eq!(state.phase, MigrationPhase::Completed);
    assert_eq!(state.source.as_deref(), Some(source.as_path()));

    // The migrated v0 manifest loads through the normal store.
    let loaded = crate::core::instances::load(&paths).unwrap();
    assert_eq!(loaded.profiles.len(), 1);
    assert_eq!(loaded.source_version, Some(0));

    // Subsequent startups are ready immediately.
    assert!(matches!(
        plan(&paths, &[source]).unwrap(),
        StartupPlan::Ready { .. }
    ));
}

#[test]
fn running_twice_is_a_noop() {
    let dir = tempfile::tempdir().unwrap();
    let source = legacy_tree(&dir.path().join("old"));
    let paths = paths_in(&dir.path().join("ferrite"));
    run_migration(&paths, &source, &MigrationControl::default()).unwrap();
    fs::write(paths.storage_root().join("marker-after-first-run"), b"x").unwrap();
    let state_before = fs::read(paths.migration_state_file()).unwrap();
    let new_tree_before = snapshot(paths.storage_root());

    let second = run_migration(&paths, &source, &MigrationControl::default()).unwrap();
    assert!(second.already_complete);
    assert_eq!(snapshot(paths.storage_root()), new_tree_before);
    assert_eq!(
        fs::read(paths.migration_state_file()).unwrap(),
        state_before
    );
}

#[test]
fn interrupted_copy_resumes_and_keeps_completed_files() {
    let dir = tempfile::tempdir().unwrap();
    let source = legacy_tree(&dir.path().join("old"));
    let before = snapshot(&source);
    let paths = paths_in(&dir.path().join("ferrite"));

    // Simulate a crash after two files were staged.
    let crash_after_two = |done: u64| {
        if done == 2 {
            crash("simulated crash")
        } else {
            Ok(())
        }
    };
    let hooks = Hooks {
        after_file: Some(&crash_after_two),
        ..Hooks::default()
    };
    assert!(run_with_hooks(&paths, &source, &MigrationControl::default(), &hooks).is_err());
    assert!(!paths.storage_root().exists());
    assert_eq!(
        read_state(&paths).unwrap().unwrap().phase,
        MigrationPhase::Copying
    );
    // Leave a torn partial file behind, as a real crash mid-write would.
    let staged = paths.migration_staging_dir().join("minecraft");
    fs::write(
        partial_name(&staged.join("versions/1.21.1/client.jar")),
        b"torn",
    )
    .unwrap();

    let (paths, candidate, resuming) = expect_migration(plan(&paths, &[]).unwrap());
    assert!(
        resuming,
        "the state file must drive resumption even with no candidates"
    );
    let counted = Cell::new(0_u64);
    let count = |_: u64| {
        counted.set(counted.get() + 1);
        Ok(())
    };
    let hooks = Hooks {
        after_file: Some(&count),
        ..Hooks::default()
    };
    let control = MigrationControl::default();
    let report = run_with_hooks(&paths, &candidate.path, &control, &hooks).unwrap();
    assert_eq!(report.files_copied, 5);
    assert_eq!(counted.get(), 5);
    assert_eq!(snapshot(paths.storage_root()), before);
    assert_eq!(snapshot(&source), before);
    assert_eq!(
        read_state(&paths).unwrap().unwrap().phase,
        MigrationPhase::Completed
    );
}

#[test]
fn resume_recopies_source_files_changed_between_attempts() {
    let dir = tempfile::tempdir().unwrap();
    let source = legacy_tree(&dir.path().join("old"));
    let paths = paths_in(&dir.path().join("ferrite"));
    // Interrupt after every file has been staged but before verification.
    let fail = |_: &Path| crash("simulated crash before verify");
    let hooks = Hooks {
        before_verify: Some(&fail),
        ..Hooks::default()
    };
    assert!(run_with_hooks(&paths, &source, &MigrationControl::default(), &hooks).is_err());

    // The source changes in between with the same size but different content/mtime.
    let level = source.join("instances/survival/saves/world/level.dat");
    fs::write(&level, b"WORLD-DATA").unwrap();
    let later = SystemTime::now() + Duration::from_secs(3600);
    File::options()
        .write(true)
        .open(&level)
        .unwrap()
        .set_modified(later)
        .unwrap();

    run_migration(&paths, &source, &MigrationControl::default()).unwrap();
    assert_eq!(
        fs::read(
            paths
                .storage_root()
                .join("instances/survival/saves/world/level.dat")
        )
        .unwrap(),
        b"WORLD-DATA"
    );
}

#[test]
fn interruption_before_rename_resumes() {
    let dir = tempfile::tempdir().unwrap();
    let source = legacy_tree(&dir.path().join("old"));
    let before = snapshot(&source);
    let paths = paths_in(&dir.path().join("ferrite"));
    let fail = || crash("simulated crash before rename");
    let hooks = Hooks {
        before_rename: Some(&fail),
        ..Hooks::default()
    };
    assert!(run_with_hooks(&paths, &source, &MigrationControl::default(), &hooks).is_err());
    assert!(!paths.storage_root().exists());
    let (paths, candidate, resuming) = expect_migration(plan(&paths, &[]).unwrap());
    assert!(resuming);
    run_migration(&paths, &candidate.path, &MigrationControl::default()).unwrap();
    assert_eq!(snapshot(paths.storage_root()), before);
}

#[test]
fn interruption_after_rename_before_marker_finishes_on_resume() {
    let dir = tempfile::tempdir().unwrap();
    let source = legacy_tree(&dir.path().join("old"));
    let before = snapshot(&source);
    let paths = paths_in(&dir.path().join("ferrite"));
    let fail = || crash("simulated crash after rename");
    let hooks = Hooks {
        after_rename: Some(&fail),
        ..Hooks::default()
    };
    assert!(run_with_hooks(&paths, &source, &MigrationControl::default(), &hooks).is_err());
    assert!(paths.storage_root().exists());
    assert_eq!(
        read_state(&paths).unwrap().unwrap().phase,
        MigrationPhase::Verified
    );

    let (paths, candidate, resuming) = expect_migration(plan(&paths, &[]).unwrap());
    assert!(resuming);
    let report = run_migration(&paths, &candidate.path, &MigrationControl::default()).unwrap();
    assert!(!report.already_complete);
    assert_eq!(snapshot(paths.storage_root()), before);
    assert_eq!(
        read_state(&paths).unwrap().unwrap().phase,
        MigrationPhase::Completed
    );
    assert!(!paths.migration_staging_dir().exists());
}

#[test]
fn cancellation_is_resumable() {
    let dir = tempfile::tempdir().unwrap();
    let source = legacy_tree(&dir.path().join("old"));
    let paths = paths_in(&dir.path().join("ferrite"));
    let control = MigrationControl::default();
    control.cancel();
    assert!(matches!(
        run_migration(&paths, &source, &control),
        Err(MigrationError::Cancelled)
    ));
    assert!(!paths.storage_root().exists());
    let (paths, candidate, resuming) = expect_migration(plan(&paths, &[]).unwrap());
    assert!(resuming);
    run_migration(&paths, &candidate.path, &MigrationControl::default()).unwrap();
    assert!(paths.instances_manifest().exists());
}

#[test]
fn background_task_reports_progress_and_result() {
    let dir = tempfile::tempdir().unwrap();
    let source = legacy_tree(&dir.path().join("old"));
    let paths = paths_in(&dir.path().join("ferrite"));
    let task = MigrationTask::spawn(paths.clone(), source).unwrap();
    let result = loop {
        if let Some(result) = task.try_finish() {
            break result;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    };
    assert_eq!(result.unwrap().files_copied, 5);
    assert_eq!(task.control().snapshot().step, MigrationStep::Done);
}

#[cfg(unix)]
#[test]
fn symlinks_inside_source_are_not_followed() {
    use std::os::unix::fs::symlink;
    let dir = tempfile::tempdir().unwrap();
    let source = legacy_tree(&dir.path().join("old"));
    let outside = dir.path().join("outside-secrets");
    fs::create_dir_all(&outside).unwrap();
    fs::write(outside.join("secret.txt"), b"do not copy").unwrap();
    symlink(&outside, source.join("instances/survival/linked-dir")).unwrap();
    symlink(outside.join("secret.txt"), source.join("linked-file.txt")).unwrap();
    symlink(dir.path().join("nowhere"), source.join("dangling")).unwrap();

    let paths = paths_in(&dir.path().join("ferrite"));
    let candidate = inspect_candidate(&source).unwrap();
    assert_eq!(candidate.skipped_links.len(), 3);
    assert_eq!(candidate.file_count, 5, "link targets are not counted");
    let report = run_migration(&paths, &source, &MigrationControl::default()).unwrap();
    assert_eq!(report.skipped_links.len(), 3);

    let root = paths.storage_root();
    for link in [
        "instances/survival/linked-dir",
        "linked-file.txt",
        "dangling",
    ] {
        assert!(fs::symlink_metadata(root.join(link)).is_err(), "{link}");
    }
    let copied = scan_tree(root).unwrap();
    for path in copied.files.keys() {
        let contents = fs::read(root.join(path)).unwrap();
        assert_ne!(contents, b"do not copy", "{}", path.display());
    }
    assert_eq!(copied.files.len(), 5);
    assert_eq!(
        fs::read(outside.join("secret.txt")).unwrap(),
        b"do not copy"
    );
    let state = read_state(&paths).unwrap().unwrap();
    assert_eq!(state.skipped_links.len(), 3);
}

#[test]
fn unreadable_source_file_blocks_the_commit() {
    let dir = tempfile::tempdir().unwrap();
    let source = legacy_tree(&dir.path().join("old"));
    let before = snapshot(&source);
    let paths = paths_in(&dir.path().join("ferrite"));
    // Simulates e.g. an online-only OneDrive file that cannot be hydrated.
    let fail_lib = |path: &Path| {
        if path.ends_with("libraries/org/example/lib.jar") {
            Err(io::Error::other("cloud file provider is not running"))
        } else {
            Ok(())
        }
    };
    let hooks = Hooks {
        before_open: Some(&fail_lib),
        ..Hooks::default()
    };
    let error = run_with_hooks(&paths, &source, &MigrationControl::default(), &hooks).unwrap_err();
    match &error {
        MigrationError::UncopyableFiles(files) => {
            assert_eq!(files.len(), 1);
            assert_eq!(files[0].path, Path::new("libraries/org/example/lib.jar"));
            assert!(files[0].reason.contains("cloud file provider"));
        }
        other => panic!("expected UncopyableFiles, got {other}"),
    }
    assert!(error.to_string().contains("libraries/org/example/lib.jar"));
    assert!(!paths.storage_root().exists(), "nothing may be committed");
    assert_eq!(snapshot(&source), before);
    assert_eq!(
        read_state(&paths).unwrap().unwrap().phase,
        MigrationPhase::Copying,
        "staged progress is kept for a retry"
    );

    // Once the file is readable again, a retry resumes and completes.
    let (paths, candidate, resuming) = expect_migration(plan(&paths, &[]).unwrap());
    assert!(resuming);
    run_migration(&paths, &candidate.path, &MigrationControl::default()).unwrap();
    assert_eq!(snapshot(paths.storage_root()), before);
}

#[cfg(unix)]
#[test]
fn special_file_blocks_the_commit_before_copying() {
    let dir = tempfile::tempdir().unwrap();
    let source = legacy_tree(&dir.path().join("old"));
    let fifo = source.join("instances/survival/pipe");
    let c_path = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).unwrap();
    // SAFETY: valid NUL-terminated path; mkfifo has no other preconditions.
    assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) }, 0);
    let paths = paths_in(&dir.path().join("ferrite"));

    let candidate = inspect_candidate(&source).unwrap();
    assert_eq!(candidate.uncopyable.len(), 1);
    assert!(candidate.skipped_links.is_empty(), "a FIFO is not a link");

    match run_migration(&paths, &source, &MigrationControl::default()) {
        Err(MigrationError::UncopyableFiles(files)) => {
            assert_eq!(files[0].path, Path::new("instances/survival/pipe"));
        }
        other => panic!("expected UncopyableFiles, got {other:?}"),
    }
    assert!(!paths.storage_root().exists());
    assert!(
        !paths.migration_staging_dir().exists(),
        "nothing was copied"
    );
}

#[test]
fn verification_failure_rolls_back_and_leaves_source_untouched() {
    let dir = tempfile::tempdir().unwrap();
    let source = legacy_tree(&dir.path().join("old"));
    let before = snapshot(&source);
    let paths = paths_in(&dir.path().join("ferrite"));
    let corrupt = |staged: &Path| fs::write(staged.join("libraries/org/example/lib.jar"), b"x");
    let hooks = Hooks {
        before_verify: Some(&corrupt),
        ..Hooks::default()
    };
    let error = run_with_hooks(&paths, &source, &MigrationControl::default(), &hooks).unwrap_err();
    assert!(
        matches!(error, MigrationError::VerificationFailed(_)),
        "{error}"
    );
    assert!(
        !paths.migration_staging_dir().exists(),
        "staging must be removed"
    );
    assert!(!paths.storage_root().exists(), "no destination may appear");
    assert_eq!(snapshot(&source), before, "source must be unchanged");
    let state = read_state(&paths).unwrap().unwrap();
    assert_eq!(state.phase, MigrationPhase::VerificationFailed);

    // An extra file in staging is also caught.
    let add_extra = |staged: &Path| fs::write(staged.join("unexpected.bin"), b"extra");
    let hooks = Hooks {
        before_verify: Some(&add_extra),
        ..Hooks::default()
    };
    assert!(matches!(
        run_with_hooks(&paths, &source, &MigrationControl::default(), &hooks),
        Err(MigrationError::VerificationFailed(_))
    ));
    assert_eq!(snapshot(&source), before);

    // A retry after failure re-plans and succeeds.
    let (paths, candidate, _) =
        expect_migration(plan(&paths, std::slice::from_ref(&source)).unwrap());
    run_migration(&paths, &candidate.path, &MigrationControl::default()).unwrap();
    assert_eq!(snapshot(paths.storage_root()), before);
}

#[test]
fn verification_failure_leaves_existing_destination_untouched() {
    let dir = tempfile::tempdir().unwrap();
    let source = legacy_tree(&dir.path().join("old"));
    let before = snapshot(&source);
    let paths = paths_in(&dir.path().join("ferrite"));
    // A destination appears while copying (e.g. another process) and the copy is bad.
    let create_dest_and_corrupt = |staged: &Path| {
        fs::create_dir_all(paths.storage_root())?;
        fs::write(paths.storage_root().join("existing.txt"), b"existing")?;
        fs::write(staged.join("assets/objects/ab/abcdef"), b"")
    };
    let hooks = Hooks {
        before_verify: Some(&create_dest_and_corrupt),
        ..Hooks::default()
    };
    assert!(matches!(
        run_with_hooks(&paths, &source, &MigrationControl::default(), &hooks),
        Err(MigrationError::VerificationFailed(_))
    ));
    assert_eq!(
        fs::read(paths.storage_root().join("existing.txt")).unwrap(),
        b"existing"
    );
    assert!(!paths.migration_staging_dir().exists());
    assert_eq!(snapshot(&source), before);
}

#[test]
fn existing_destination_is_never_overwritten() {
    let dir = tempfile::tempdir().unwrap();
    let source = legacy_tree(&dir.path().join("old"));
    let paths = paths_in(&dir.path().join("ferrite"));
    fs::create_dir_all(paths.storage_root()).unwrap();
    fs::write(paths.storage_root().join("mine.txt"), b"mine").unwrap();
    match plan(&paths, std::slice::from_ref(&source)).unwrap() {
        StartupPlan::Ready { notes, .. } => assert!(!notes.is_empty()),
        other => panic!("expected Ready, got {other:?}"),
    }
    assert!(matches!(
        run_migration(&paths, &source, &MigrationControl::default()),
        Err(MigrationError::DestinationExists(_))
    ));
    assert_eq!(
        fs::read(paths.storage_root().join("mine.txt")).unwrap(),
        b"mine"
    );
}

#[test]
fn two_differing_candidates_require_a_user_choice() {
    let dir = tempfile::tempdir().unwrap();
    let exe = legacy_tree(&dir.path().join("exe-dir"));
    let cwd = legacy_tree(&dir.path().join("cwd"));
    fs::write(
        cwd.join("instances.json"),
        r#"[{"name":"A","version":"1","loader":"Vanilla","directory":"a"},
            {"name":"B","version":"1","loader":"Vanilla","directory":"b"}]"#,
    )
    .unwrap();
    let paths = paths_in(&dir.path().join("ferrite"));
    match plan(&paths, &[exe.clone(), cwd.clone()]).unwrap() {
        StartupPlan::NeedsUserChoice { candidates, .. } => {
            assert_eq!(candidates.len(), 2);
            assert_eq!(candidates[0].path, exe);
            assert_eq!(candidates[0].instance_count, 1);
            assert_eq!(candidates[1].path, cwd);
            assert_eq!(candidates[1].instance_count, 2);
            for candidate in &candidates {
                assert!(candidate.total_bytes > 0);
                assert!(candidate.file_count > 0);
                assert!(candidate.last_modified.is_some());
            }
        }
        other => panic!("expected NeedsUserChoice, got {other:?}"),
    }
    assert!(
        !paths.storage_root().exists(),
        "nothing may be migrated yet"
    );
    assert!(!paths.migration_state_file().exists());

    // After the user picks one, the other is ignored on later startups.
    run_migration(&paths, &cwd, &MigrationControl::default()).unwrap();
    assert!(matches!(
        plan(&paths, &[exe, cwd]).unwrap(),
        StartupPlan::Ready { .. }
    ));
    assert_eq!(
        crate::core::instances::load(&paths).unwrap().profiles.len(),
        2
    );
}

#[test]
fn identical_candidates_do_not_prompt() {
    let dir = tempfile::tempdir().unwrap();
    let first = legacy_tree(&dir.path().join("one"));
    let second = legacy_tree(&dir.path().join("two"));
    let paths = paths_in(&dir.path().join("ferrite"));
    let (_, candidate, _) = expect_migration(plan(&paths, &[first.clone(), second]).unwrap());
    assert_eq!(candidate.path, first);
}

#[test]
fn invalid_manifest_entries_are_reported_during_migration() {
    let dir = tempfile::tempdir().unwrap();
    let source = legacy_tree(&dir.path().join("old"));
    fs::write(
        source.join("instances.json"),
        r#"[{"name":"ok","version":"1","loader":"Vanilla","directory":"survival"},
            {"name":"bad","version":"1","loader":"Vanilla","directory":"..\\..\\Windows"}]"#,
    )
    .unwrap();
    let paths = paths_in(&dir.path().join("ferrite"));
    let candidate = inspect_candidate(&source).unwrap();
    assert_eq!(candidate.instance_count, 1);
    assert_eq!(candidate.skipped_entries.len(), 1);
    let report = run_migration(&paths, &source, &MigrationControl::default()).unwrap();
    assert_eq!(report.skipped_entries.len(), 1);
    let loaded = crate::core::instances::load(&paths).unwrap();
    assert_eq!(loaded.profiles.len(), 1);
    assert_eq!(loaded.skipped.len(), 1);
}

#[test]
fn rollback_switches_back_to_the_untouched_legacy_location() {
    let dir = tempfile::tempdir().unwrap();
    let source = legacy_tree(&dir.path().join("old"));
    let before = snapshot(&source);
    let paths = paths_in(&dir.path().join("ferrite"));
    assert!(matches!(
        rollback_to_legacy(&paths),
        Err(MigrationError::NothingToRollBack)
    ));
    run_migration(&paths, &source, &MigrationControl::default()).unwrap();

    let legacy = rollback_to_legacy(&paths).unwrap();
    assert_eq!(legacy.mode(), &StorageMode::LegacyRollback);
    assert_eq!(legacy.storage_root(), source);
    assert_eq!(snapshot(&source), before);
    assert!(paths.storage_root().exists(), "the new copy is kept too");

    match plan(&paths, &[]).unwrap() {
        StartupPlan::Ready { paths: ready, .. } => assert_eq!(ready, legacy),
        other => panic!("expected Ready(legacy), got {other:?}"),
    }
    assert!(matches!(
        run_migration(&paths, &source, &MigrationControl::default()),
        Err(MigrationError::RolledBack)
    ));
}

#[test]
fn future_state_versions_are_refused() {
    let dir = tempfile::tempdir().unwrap();
    let paths = paths_in(dir.path());
    fs::create_dir_all(paths.data_dir()).unwrap();
    fs::write(
        paths.migration_state_file(),
        r#"{"schema_version": 7, "phase": "teleporting"}"#,
    )
    .unwrap();
    assert!(matches!(
        plan(&paths, &[]),
        Err(MigrationError::UnsupportedState { found: 7 })
    ));
}

#[test]
fn source_overlapping_data_dir_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let paths = paths_in(dir.path());
    fs::create_dir_all(paths.data_dir()).unwrap();
    // e.g. CWD == data dir would make `<cwd>/minecraft` a candidate inside it.
    let inside = legacy_tree(&paths.data_dir().join("nested"));
    assert!(matches!(
        run_migration(&paths, &inside, &MigrationControl::default()),
        Err(MigrationError::InvalidSource(_))
    ));
    match plan(&paths, &[inside]).unwrap() {
        StartupPlan::Ready { notes, .. } => assert!(!notes.is_empty()),
        other => panic!("expected Ready, got {other:?}"),
    }
}
