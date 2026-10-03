//! Background size and worlds scan of an instance folder, for the Delete and
//! Duplicate dialogs. Links are listed, never followed (same scanner as the
//! migration and duplicate). This reads the whole tree, so run it off the UI thread.

use crate::core::copy::{self, CopyFailure, UncopyableFile};
use crate::core::instances::{self, InstanceError, InstanceProfile};
use crate::core::paths::AppPaths;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// Result of [`scan_instance`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InstanceScan {
    /// Total size of the regular files.
    pub bytes: u64,
    /// Number of regular files.
    pub files: u64,
    /// Folder names directly under `saves/` (not links), sorted case-insensitively.
    pub worlds: Vec<String>,
    /// Links (relative paths) that were not followed or counted.
    pub skipped_links: Vec<PathBuf>,
    /// Entries that could not be read (they would block a duplicate).
    pub unreadable: Vec<UncopyableFile>,
}

/// Scans `profile`'s folder. A missing folder is [`InstanceError::Io`] with
/// [`io::ErrorKind::NotFound`]; a link at the folder's own path is refused.
pub fn scan_instance(
    paths: &AppPaths,
    profile: &InstanceProfile,
) -> Result<InstanceScan, InstanceError> {
    let folder = profile.game_dir(paths);
    instances::ensure_game_dir_contained(paths, &folder)?;
    let metadata = fs::symlink_metadata(&folder)?;
    if !metadata.is_dir() || crate::core::fsutil::is_link_like(&metadata) {
        return Err(InstanceError::Io(io::Error::other(format!(
            "{} is not a folder",
            folder.display()
        ))));
    }
    let scan = copy::scan_tree(&folder).map_err(|failure| match failure {
        CopyFailure::Io { context, error } => {
            InstanceError::Io(io::Error::new(error.kind(), format!("{context}: {error}")))
        }
        other => InstanceError::Io(io::Error::other(format!("{other:?}"))),
    })?;
    let saves = Path::new("saves");
    let mut worlds: Vec<String> = scan
        .dirs
        .iter()
        .filter(|dir| dir.parent() == Some(saves))
        .filter_map(|dir| dir.file_name())
        .map(|name| name.to_string_lossy().into_owned())
        .collect();
    worlds.sort_by_key(|name| name.to_lowercase());
    Ok(InstanceScan {
        bytes: scan.total_bytes,
        files: scan.files.len() as u64,
        worlds,
        skipped_links: scan.links,
        unreadable: scan.blocked,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::instances::InstanceDirName;
    use crate::core::paths::test_support::paths_in;

    fn profile(directory: &str) -> InstanceProfile {
        InstanceProfile::with_directory(
            "Pack".into(),
            "1.20.1".into(),
            "Vanilla".into(),
            InstanceDirName::parse(directory.to_owned()).unwrap(),
        )
    }

    #[test]
    fn counts_files_bytes_and_worlds() {
        let temp = tempfile::tempdir().unwrap();
        let paths = paths_in(temp.path());
        let root = paths.instances_dir().join("pack");
        for world in ["skyblock", "New World", "Hardcore 2"] {
            fs::create_dir_all(root.join("saves").join(world)).unwrap();
            fs::write(root.join("saves").join(world).join("level.dat"), b"12345").unwrap();
        }
        fs::create_dir_all(root.join("saves/New World/DIM-1")).unwrap();
        fs::create_dir_all(root.join("mods")).unwrap();
        fs::write(root.join("mods/a.jar"), vec![0_u8; 1000]).unwrap();
        fs::write(root.join("saves/readme.txt"), b"not a world").unwrap();

        let scan = scan_instance(&paths, &profile("pack")).unwrap();
        assert_eq!(scan.files, 5);
        assert_eq!(scan.bytes, 3 * 5 + 1000 + 11);
        assert_eq!(scan.worlds, ["Hardcore 2", "New World", "skyblock"]);
        assert!(scan.skipped_links.is_empty());
        assert!(scan.unreadable.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn links_are_listed_not_followed_or_counted() {
        let temp = tempfile::tempdir().unwrap();
        let paths = paths_in(temp.path());
        let root = paths.instances_dir().join("pack");
        fs::create_dir_all(root.join("saves")).unwrap();
        let outside = temp.path().join("outside");
        fs::create_dir_all(outside.join("world")).unwrap();
        fs::write(outside.join("world/level.dat"), vec![0_u8; 5000]).unwrap();
        std::os::unix::fs::symlink(outside.join("world"), root.join("saves/Linked")).unwrap();

        let scan = scan_instance(&paths, &profile("pack")).unwrap();
        assert_eq!(scan.bytes, 0);
        assert!(scan.worlds.is_empty());
        assert_eq!(scan.skipped_links, [PathBuf::from("saves/Linked")]);
    }

    #[test]
    fn missing_folder_is_not_found() {
        let temp = tempfile::tempdir().unwrap();
        let paths = paths_in(temp.path());
        match scan_instance(&paths, &profile("gone")) {
            Err(InstanceError::Io(error)) => assert_eq!(error.kind(), io::ErrorKind::NotFound),
            other => panic!("expected NotFound, got {other:?}"),
        }
    }
}
