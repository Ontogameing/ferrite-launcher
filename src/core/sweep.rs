//! Startup clean-up of folders left behind in `instances/` by an import or duplicate
//! that was interrupted (crash, power loss, killed process).
//!
//! Only exact Ferrite temp names directly inside the instances folder are considered:
//! `.<dir>.import.<pid>.<n>.tmp` (pack import staging) and
//! `.<dir>.duplicate.<pid>.<n>.tmp` (duplicate staging), where `<pid>` and `<n>` are
//! decimal numbers. A candidate is removed only when all of these hold:
//!
//! * it is a real folder (a link or file with such a name is left alone; links are
//!   never followed),
//! * nothing in it (the folder, its subfolders, or files, checked without following
//!   links) was modified within `min_age`,
//! * on Unix, the process `<pid>` is not running (it could be another Ferrite window
//!   still working; a reused pid just postpones the clean-up),
//! * no manifest entry (profile or preserved skipped entry) names that folder,
//!   compared case-insensitively; an unreadable manifest skips the sweep entirely.
//!
//! It only logs; there is no UI.

use crate::core::fsutil;
use crate::core::instances::{self, InstanceProfile, SkippedEntry, directory_key};
use crate::core::paths::AppPaths;
use std::collections::HashSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// Default minimum age before a leftover is swept.
pub const DEFAULT_MIN_AGE: Duration = Duration::from_secs(5 * 60);

/// The temp-folder kinds Ferrite creates inside `instances/`.
const KINDS: [&str; 2] = ["import", crate::core::duplicate::STAGING_KIND];

/// What a sweep did.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct SweepReport {
    pub removed: Vec<PathBuf>,
    /// Candidates left alone because they were recent or their process is alive.
    pub kept: Vec<PathBuf>,
    /// Candidates that could not be removed, with the error.
    pub failed: Vec<(PathBuf, String)>,
}

/// Parses `.<dir>.<kind>.<pid>.<n>.tmp`; returns the pid when `name` matches exactly.
pub fn parse_temp_name(name: &str) -> Option<u32> {
    let inner = name.strip_prefix('.')?.strip_suffix(".tmp")?;
    let mut parts = inner.rsplitn(4, '.');
    let counter = parts.next()?;
    let pid = parts.next()?;
    let kind = parts.next()?;
    let directory = parts.next()?;
    let digits = |text: &str| !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit());
    if directory.is_empty() || !KINDS.contains(&kind) || !digits(pid) || !digits(counter) {
        return None;
    }
    pid.parse().ok()
}

/// Removes stale Ferrite temp folders directly inside the instances folder (see the
/// module docs). A missing instances folder is not an error.
///
/// Folders named by the manifest (any loaded profile or preserved skipped entry,
/// compared case-insensitively like folders) are never touched. If the manifest can't
/// be read, nothing is swept.
pub fn sweep_stale_temp_dirs(paths: &AppPaths, min_age: Duration) -> io::Result<SweepReport> {
    let loaded = instances::load(paths).map_err(|error| {
        io::Error::other(format!(
            "instance list unreadable, skipping the temp sweep: {error}"
        ))
    })?;
    let claimed = claimed_keys(&loaded.profiles, &loaded.skipped);
    sweep_with(paths, min_age, SystemTime::now(), &process_alive, &claimed)
}

/// Folder keys used by manifest entries.
fn claimed_keys(profiles: &[InstanceProfile], skipped: &[SkippedEntry]) -> HashSet<String> {
    profiles
        .iter()
        .map(|profile| directory_key(profile.directory().as_str()))
        .chain(
            skipped
                .iter()
                .filter_map(SkippedEntry::raw_directory)
                .map(directory_key),
        )
        .collect()
}

fn sweep_with(
    paths: &AppPaths,
    min_age: Duration,
    now: SystemTime,
    alive: &dyn Fn(u32) -> bool,
    claimed: &HashSet<String>,
) -> io::Result<SweepReport> {
    let root = paths.instances_dir();
    let mut report = SweepReport::default();
    let entries = match fs::read_dir(&root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(report),
        Err(error) => return Err(error),
    };
    for entry in entries {
        let entry = entry?;
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let Some(pid) = parse_temp_name(&name) else {
            continue;
        };
        if claimed.contains(&directory_key(&name)) {
            eprintln!("Ferrite: {name} looks like a temp folder but an instance uses it; kept");
            continue;
        }
        let path = root.join(&name);
        let Ok(metadata) = fs::symlink_metadata(&path) else {
            continue;
        };
        if !metadata.is_dir() || fsutil::is_link_like(&metadata) {
            continue;
        }
        if alive(pid) || recently_modified(&path, now, min_age) {
            report.kept.push(path);
            continue;
        }
        // `remove_dir_all` does not follow links inside the folder.
        match fs::remove_dir_all(&path) {
            Ok(()) => {
                eprintln!("Ferrite: removed leftover temp folder {}", path.display());
                report.removed.push(path);
            }
            Err(error) => {
                eprintln!(
                    "Ferrite: could not remove leftover temp folder {}: {error}",
                    path.display()
                );
                report.failed.push((path, error.to_string()));
            }
        }
    }
    Ok(report)
}

/// Whether anything in `dir` (itself, subfolders, files; links not followed) was
/// modified within `min_age` of `now`, or can't be checked.
fn recently_modified(dir: &Path, now: SystemTime, min_age: Duration) -> bool {
    let recent = |metadata: &fs::Metadata| match metadata.modified() {
        Ok(modified) => now
            .duration_since(modified)
            .map_or(true, |age| age < min_age),
        Err(_) => true,
    };
    let mut pending = vec![dir.to_path_buf()];
    while let Some(current) = pending.pop() {
        let Ok(metadata) = fs::symlink_metadata(&current) else {
            return true;
        };
        if recent(&metadata) {
            return true;
        }
        let Ok(entries) = fs::read_dir(&current) else {
            return true;
        };
        for entry in entries {
            let Ok(entry) = entry else {
                return true;
            };
            let Ok(metadata) = fs::symlink_metadata(entry.path()) else {
                return true;
            };
            if metadata.is_dir() && !fsutil::is_link_like(&metadata) {
                pending.push(entry.path());
            } else if recent(&metadata) {
                return true;
            }
        }
    }
    false
}

#[cfg(unix)]
fn process_alive(pid: u32) -> bool {
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return false;
    };
    if pid <= 0 {
        return false;
    }
    // SAFETY: signal 0 only checks whether the process exists.
    if unsafe { libc::kill(pid, 0) } == 0 {
        return true;
    }
    io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(not(unix))]
fn process_alive(_pid: u32) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::paths::test_support::paths_in;

    const LATER: Duration = Duration::from_secs(3600);

    #[test]
    fn only_exact_temp_names_match() {
        assert_eq!(parse_temp_name(".pack.import.123.456.tmp"), Some(123));
        assert_eq!(parse_temp_name(".my.pack.duplicate.9.17.tmp"), Some(9));
        for name in [
            "pack",
            ".pack",
            ".pack.import.123.456",
            "pack.import.123.456.tmp",
            "..import.1.2.tmp",
            ".pack.export.1.2.tmp",
            ".pack.import.x1.2.tmp",
            ".pack.import.1.2a.tmp",
            ".pack.import..2.tmp",
            ".pack.import.1..tmp",
            ".import.1.2.tmp",
            ".pack.IMPORT.1.2.tmp",
        ] {
            assert_eq!(parse_temp_name(name), None, "{name}");
        }
    }

    fn setup() -> (tempfile::TempDir, AppPaths) {
        let temp = tempfile::tempdir().unwrap();
        let paths = paths_in(temp.path());
        fs::create_dir_all(paths.instances_dir()).unwrap();
        (temp, paths)
    }

    #[test]
    fn removes_only_stale_ferrite_temp_folders() {
        let (_temp, paths) = setup();
        let root = paths.instances_dir();
        let stale_import = root.join(".pack.import.4000001.1.tmp");
        let stale_duplicate = root.join(".copy.duplicate.4000001.2.tmp");
        fs::create_dir_all(stale_import.join("mods")).unwrap();
        fs::write(stale_import.join("mods/a.jar"), b"a").unwrap();
        fs::create_dir(&stale_duplicate).unwrap();
        // Look-alikes that must survive.
        let instance = root.join("pack");
        let other_kind = root.join(".pack.export.4000001.1.tmp");
        let file = root.join(".file.import.4000001.3.tmp");
        fs::create_dir(&instance).unwrap();
        fs::create_dir(&other_kind).unwrap();
        fs::write(&file, b"x").unwrap();

        let report = sweep_with(
            &paths,
            DEFAULT_MIN_AGE,
            SystemTime::now() + LATER,
            &|_| false,
            &HashSet::new(),
        )
        .unwrap();
        let mut removed = report.removed.clone();
        removed.sort();
        let mut expected = vec![stale_import.clone(), stale_duplicate.clone()];
        expected.sort();
        assert_eq!(removed, expected);
        assert!(!stale_import.exists() && !stale_duplicate.exists());
        assert!(instance.is_dir() && other_kind.is_dir() && file.is_file());
    }

    #[test]
    fn recent_or_live_folders_are_kept() {
        let (_temp, paths) = setup();
        let root = paths.instances_dir();
        let recent = root.join(".pack.import.4000001.1.tmp");
        fs::create_dir(&recent).unwrap();
        let report = sweep_with(
            &paths,
            DEFAULT_MIN_AGE,
            SystemTime::now(),
            &|_| false,
            &HashSet::new(),
        )
        .unwrap();
        assert_eq!(report.kept, std::slice::from_ref(&recent));
        assert!(recent.is_dir());

        // Old, but its process is still running.
        let report = sweep_with(
            &paths,
            DEFAULT_MIN_AGE,
            SystemTime::now() + LATER,
            &|pid| pid == 4000001,
            &HashSet::new(),
        )
        .unwrap();
        assert_eq!(report.kept, std::slice::from_ref(&recent));
        assert!(recent.is_dir());
    }

    #[test]
    fn a_recent_file_deep_inside_keeps_the_folder() {
        let (_temp, paths) = setup();
        let root = paths.instances_dir();
        let staging = root.join(".pack.duplicate.4000001.1.tmp");
        fs::create_dir_all(staging.join("saves/world")).unwrap();
        let file = staging.join("saves/world/level.dat");
        fs::write(&file, b"x").unwrap();
        // Run the sweep "later" so every folder looks old, and make only the
        // deepest file recent. (Setting a folder's mtime needs special open
        // flags on Windows, so the test moves the clock instead.)
        let now = SystemTime::now() + LATER;
        fs::File::options()
            .write(true)
            .open(&file)
            .unwrap()
            .set_modified(now)
            .unwrap();
        let report = sweep_with(&paths, DEFAULT_MIN_AGE, now, &|_| false, &HashSet::new()).unwrap();
        assert_eq!(report.kept, std::slice::from_ref(&staging));
        assert!(file.exists());
    }

    #[cfg(unix)]
    #[test]
    fn links_with_temp_names_are_not_followed_or_removed() {
        let (temp, paths) = setup();
        let target = temp.path().join("precious");
        fs::create_dir(&target).unwrap();
        fs::write(target.join("data"), b"keep").unwrap();
        let link = paths.instances_dir().join(".pack.import.4000001.1.tmp");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let report = sweep_with(
            &paths,
            DEFAULT_MIN_AGE,
            SystemTime::now() + LATER,
            &|_| false,
            &HashSet::new(),
        )
        .unwrap();
        assert!(report.removed.is_empty() && report.kept.is_empty());
        assert!(fs::symlink_metadata(&link).is_ok());
        assert_eq!(fs::read(target.join("data")).unwrap(), b"keep");
    }

    #[cfg(unix)]
    #[test]
    fn links_inside_a_stale_folder_are_removed_as_links() {
        let (temp, paths) = setup();
        let target = temp.path().join("precious");
        fs::create_dir(&target).unwrap();
        fs::write(target.join("data"), b"keep").unwrap();
        let staging = paths.instances_dir().join(".pack.duplicate.4000001.1.tmp");
        fs::create_dir(&staging).unwrap();
        std::os::unix::fs::symlink(&target, staging.join("link")).unwrap();
        let report = sweep_with(
            &paths,
            DEFAULT_MIN_AGE,
            SystemTime::now() + LATER,
            &|_| false,
            &HashSet::new(),
        )
        .unwrap();
        assert_eq!(report.removed, std::slice::from_ref(&staging));
        assert_eq!(fs::read(target.join("data")).unwrap(), b"keep");
    }

    #[test]
    fn folders_named_by_the_manifest_are_never_swept() {
        let (_temp, paths) = setup();
        let root = paths.instances_dir();
        let used = root.join(".x.import.123.1.tmp");
        let used_by_skipped = root.join(".y.duplicate.123.2.tmp");
        fs::create_dir(&used).unwrap();
        fs::write(used.join("options.txt"), b"keep").unwrap();
        fs::create_dir(&used_by_skipped).unwrap();
        // A loaded profile (different case) and a preserved invalid entry claim them.
        fs::write(
            paths.instances_manifest(),
            r#"[{"name":"Odd","version":"1.20.1","loader":"Vanilla","directory":".X.Import.123.1.tmp"},
               {"name":"Y","version":1,"directory":".y.duplicate.123.2.tmp"}]"#,
        )
        .unwrap();
        let loaded = instances::load(&paths).unwrap();
        assert_eq!(loaded.profiles.len(), 1, "{:?}", loaded.skipped);
        assert_eq!(loaded.skipped.len(), 1);
        let claimed = claimed_keys(&loaded.profiles, &loaded.skipped);
        let report = sweep_with(
            &paths,
            DEFAULT_MIN_AGE,
            SystemTime::now() + LATER,
            &|_| false,
            &claimed,
        )
        .unwrap();
        assert!(report.removed.is_empty(), "{report:?}");
        assert_eq!(fs::read(used.join("options.txt")).unwrap(), b"keep");
        assert!(used_by_skipped.is_dir());
    }

    #[test]
    fn unreadable_manifest_skips_the_sweep() {
        let (_temp, paths) = setup();
        let stale = paths.instances_dir().join(".pack.import.4000001.1.tmp");
        fs::create_dir(&stale).unwrap();
        fs::write(paths.instances_manifest(), "{ corrupt").unwrap();
        assert!(sweep_stale_temp_dirs(&paths, Duration::ZERO).is_err());
        assert!(stale.is_dir());
    }

    #[test]
    fn missing_instances_folder_is_fine() {
        let temp = tempfile::tempdir().unwrap();
        let paths = paths_in(temp.path());
        assert_eq!(
            sweep_stale_temp_dirs(&paths, DEFAULT_MIN_AGE).unwrap(),
            SweepReport::default()
        );
    }

    #[cfg(unix)]
    #[test]
    fn this_process_counts_as_alive() {
        assert!(process_alive(std::process::id()));
    }
}
