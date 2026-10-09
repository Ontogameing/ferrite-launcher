//! Exercises the library as a frontend would, using disposable offline storage.
use ferrite_launcher::{
    config,
    core::paths::{AppPaths, BaseDirs},
    instance_mods,
};

#[test]
fn configuration_and_local_mods_are_available_to_library_callers() {
    let temp = tempfile::tempdir().unwrap();
    let paths = AppPaths::from_base_dirs(BaseDirs {
        config: temp.path().join("config"),
        data_local: temp.path().join("data"),
        cache: temp.path().join("cache"),
    })
    .unwrap();
    let loaded = config::load_or_create(&paths).unwrap();
    assert_eq!(config::load(&paths).unwrap(), loaded.config);
    let game_dir = temp.path().join("game with spaces");
    assert!(instance_mods::list(&game_dir).unwrap().is_empty());
}

#[test]
fn instance_launch_uses_the_library_backend_and_keeps_missing_install_behavior() {
    use ferrite_launcher::{
        loaders::{self, ModLoader},
        minecraft::FerriteError,
    };
    let temp = tempfile::tempdir().unwrap();
    let paths = AppPaths::from_base_dirs(BaseDirs {
        config: temp.path().join("config"),
        data_local: temp.path().join("data"),
        cache: temp.path().join("cache"),
    })
    .unwrap();
    let game_dir = temp.path().join("game with spaces");
    assert!(matches!(
        loaders::launch_in_directory_with_memory(
            &paths,
            "missing",
            ModLoader::Vanilla,
            &game_dir,
            2048,
        ),
        Err(FerriteError::NotInstalled)
    ));
    assert!(!game_dir.exists());
}

#[test]
fn prepared_import_rechecks_the_preview_target_before_writes_or_downloads() {
    use ferrite_launcher::{
        core::instances,
        loaders::ModLoader,
        packs::{self, PackTarget},
    };
    use std::io::Write;
    let temp = tempfile::tempdir().unwrap();
    let paths = AppPaths::from_base_dirs(BaseDirs {
        config: temp.path().join("config"),
        data_local: temp.path().join("data"),
        cache: temp.path().join("cache"),
    })
    .unwrap();
    let archive = temp.path().join("changed.mrpack");
    let mut zip = zip::ZipWriter::new(std::fs::File::create(&archive).unwrap());
    zip.start_file("modrinth.index.json", zip::write::FileOptions::default())
        .unwrap();
    zip.write_all(
        br#"{"formatVersion":1,"game":"minecraft","versionId":"1","name":"Changed",
        "dependencies":{"minecraft":"1.20.1"},"files":[]}"#,
    )
    .unwrap();
    zip.finish().unwrap();
    let profile =
        instances::new_instance_profile(&paths, "Imported", "1.21", "Vanilla", &[], &[]).unwrap();
    let directory = profile.game_dir(&paths);
    let target = PackTarget {
        minecraft_version: "1.21".into(),
        loader: ModLoader::Vanilla,
        loader_version: None,
    };
    let result =
        packs::prepare_instance_import(&paths, &archive, profile, target, false, None, &|_| {
            panic!("changed target should be rejected before preparation")
        });
    assert_eq!(
        result.err().unwrap(),
        "The pack changed after it was previewed; no instance was kept."
    );
    assert!(!directory.exists());
    assert!(!paths.instances_manifest().exists());
}

#[test]
fn coordinator_preserves_launch_error_priority_without_frontend() {
    use ferrite_launcher::core::{activity::WorkflowCoordinator, instances};
    let temp = tempfile::tempdir().unwrap();
    let paths = AppPaths::from_base_dirs(BaseDirs {
        config: temp.path().join("config"),
        data_local: temp.path().join("data"),
        cache: temp.path().join("cache"),
    })
    .unwrap();
    let mut coordinator = WorkflowCoordinator::default();
    let mods = coordinator.begin_mod(false).unwrap();
    assert_eq!(
        coordinator
            .launch(&paths, None, None, false, 2048, false)
            .unwrap_err(),
        "Sign in with Microsoft before launching, or select Offline mode."
    );
    assert_eq!(
        coordinator
            .launch(&paths, None, None, true, 2048, false)
            .unwrap_err(),
        "Wait for mod or instance import/export work to finish before launching."
    );
    coordinator.complete(mods);
    assert_eq!(
        coordinator
            .launch(&paths, None, None, true, 2048, false)
            .unwrap_err(),
        "Select an instance before launching."
    );
    let profile =
        instances::new_instance_profile(&paths, "Offline", "missing", "Vanilla", &[], &[]).unwrap();
    assert_eq!(
        coordinator
            .launch(&paths, Some(&profile), None, true, 2048, false)
            .unwrap_err(),
        "This instance's folder is missing."
    );
    instances::create_new_game_dir(&paths, &profile).unwrap();
    assert!(
        coordinator
            .launch(&paths, Some(&profile), None, true, 2048, false)
            .unwrap_err()
            .contains("Failed to launch 'Offline':")
    );
    assert!(!ferrite_launcher::minecraft::is_running());
}

#[test]
fn prepared_creation_commits_and_abandonment_cleans_without_frontend() {
    use ferrite_launcher::core::{activity::WorkflowCoordinator, instances};
    let temp = tempfile::tempdir().unwrap();
    let paths = AppPaths::from_base_dirs(BaseDirs {
        config: temp.path().join("config"),
        data_local: temp.path().join("data"),
        cache: temp.path().join("cache"),
    })
    .unwrap();
    let mut coordinator = WorkflowCoordinator::default();
    let owner = coordinator.begin_create().unwrap();
    let profile =
        instances::new_instance_profile(&paths, "Created", "1.21", "Vanilla", &[], &[]).unwrap();
    let prepared = instances::prepare_create(&paths, &profile, || Ok(())).unwrap();
    let mut profiles = vec![];
    assert_eq!(prepared.commit(&mut profiles, &[]).unwrap(), 0);
    coordinator.complete(owner);
    assert_eq!(instances::load(&paths).unwrap().profiles, profiles);
    assert!(profile.game_dir(&paths).exists());
    let abandoned =
        instances::new_instance_profile(&paths, "Abandoned", "1.21", "Vanilla", &profiles, &[])
            .unwrap();
    drop(instances::prepare_create(&paths, &abandoned, || Ok(())).unwrap());
    assert!(!abandoned.game_dir(&paths).exists());
    assert!(profile.game_dir(&paths).exists());
}
