//! Inspection and safe local mutation of installed mod JARs.
//!
//! This module performs no network access and never loads or executes JAR code.
//! Operations are confined to an instance's `game_dir/mods` directory. Listing a
//! missing directory yields an empty collection, while mutation requires an exact,
//! existing regular file whose case-sensitive basename ends in `.jar` or
//! `.jar.disabled`. Unsupported entries and symlinks are ignored while listing and
//! rejected for mutation; every existing directory component is also checked to
//! reject symlinked ancestors.
//!
//! Display metadata is best-effort and untrusted. The reader recognizes Fabric,
//! Quilt, and legacy Forge JSON descriptors inside the ZIP, applies strict read
//! limits, and never extracts archive entries. Invalid archives, malformed or
//! unsupported descriptors, unsafe icon paths, and oversized metadata degrade to
//! the filename-derived name with no version/icon rather than making listing fail.
//! Filesystem enumeration/inspection errors are still returned because they make
//! the directory result unreliable.
//!
//! Enabling and disabling changes only the suffix. It creates a hard link under
//! the destination name before unlinking the source, so a destination that already
//! exists is never overwritten, including when it appears concurrently. The tradeoff
//! is that hard links must be supported and the two-step operation is not atomic:
//! a crash or unlink failure can leave both names. Uninstall likewise removes only
//! the exact validated regular file; it does not infer or delete related files.
//!
//! These checks defend against accidental traversal and ordinary symlink mistakes,
//! not a hostile process racing filesystem components between checks and use.
//! Callers must serialize mutations and control the instance tree if it is a trust
//! boundary.
//!
//! Integration: declare `mod instance_mods;` in the consuming crate. Unit tests
//! below use only temporary directories and synthetic jars, and run offline with
//! `cargo test --offline instance_mods` once the module is declared.

use serde_json::Value;
use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Component, Path, PathBuf};
use zip::ZipArchive;

// Bounds apply to uncompressed ZIP entry sizes and to bytes actually read.
const METADATA_LIMIT: u64 = 256 * 1024;
const ICON_LIMIT: u64 = 1024 * 1024;

/// UI-facing description of one regular mod JAR found on disk.
///
/// Identity and mutations use [`Self::filename`], never the untrusted embedded
/// display fields. Metadata fields remain useful even when partial: each absent
/// value independently falls back or stays `None`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstalledMod {
    /// Exact on-disk basename, including `.disabled` when disabled.
    pub filename: String,
    /// Embedded display name, or the filename stem when no valid name was found.
    pub name: String,
    /// Embedded mod version when the recognized descriptor supplies one.
    pub version: Option<String>,
    /// Whether the exact filename has the active `.jar` suffix.
    pub enabled: bool,
    /// Untrusted encoded image data, at most 1 MiB.
    ///
    /// Consumers must still validate dimensions, format, and decoder allocation.
    pub icon: Option<Vec<u8>>,
}

/// Lists regular mod JARs in deterministic filename order.
///
/// Each candidate starts with filename-derived metadata, then recognized embedded
/// descriptors may replace its name and add a version/icon. Per-JAR archive and
/// descriptor errors preserve that fallback and do not abort the scan. Directory
/// access or entry-inspection errors are returned with the affected path.
pub fn list(game_dir: impl AsRef<Path>) -> Result<Vec<InstalledMod>, String> {
    let Some(mods) = mods_directory(game_dir.as_ref())? else {
        return Ok(Vec::new());
    };
    let entries = fs::read_dir(&mods).map_err(|e| error("Read directory", &mods, e))?;
    let mut result = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| error("Read directory entry", &mods, e))?;
        let Ok(filename) = entry.file_name().into_string() else {
            continue;
        };
        let Ok((fallback, enabled)) = parse_filename(&filename) else {
            continue;
        };
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path).map_err(|e| error("Inspect", &path, e))?;
        if !metadata.file_type().is_file() {
            continue;
        }
        let mut installed = InstalledMod {
            name: fallback.to_owned(),
            version: None,
            enabled,
            icon: None,
            filename,
        };
        read_metadata(&path, &mut installed);
        result.push(installed);
    }
    result.sort_by(|a, b| a.filename.cmp(&b.filename));
    Ok(result)
}

/// Enables or disables an exact existing filename and returns its resulting basename.
///
/// A request matching the current state is a no-op only after validating that the
/// source is still a regular file. A state change uses hard-link-then-unlink rather
/// than rename because rename may replace an existing destination. If unlinking the
/// source fails, no rollback is attempted: the error identifies that both names may
/// now refer to the same file.
pub fn set_enabled(
    game_dir: impl AsRef<Path>,
    filename: &str,
    enabled: bool,
) -> Result<String, String> {
    let (stem, current) = parse_filename(filename)?;
    let source = existing_mod(game_dir.as_ref(), filename)?;
    if current == enabled {
        return Ok(filename.to_owned());
    }
    let target_name = format!("{stem}.jar{}", if enabled { "" } else { ".disabled" });
    let target = source.with_file_name(&target_name);
    // Establish the non-overwriting destination first; only then retire the old name.
    fs::hard_link(&source, &target).map_err(|e| error("Create mod destination", &target, e))?;
    fs::remove_file(&source).map_err(|e| {
        format!(
            "{}; destination {} also exists (no rollback attempted)",
            error("Remove original mod", &source, e),
            target.display()
        )
    })?;
    Ok(target_name)
}

/// Removes only the exact named regular JAR.
///
/// Missing files, symlinks, directories, unsafe basenames, and unsupported suffixes
/// are errors. No alternate enabled/disabled name is searched or removed.
pub fn uninstall(game_dir: impl AsRef<Path>, filename: &str) -> Result<(), String> {
    let path = existing_mod(game_dir.as_ref(), filename)?;
    fs::remove_file(&path).map_err(|e| error("Uninstall", &path, e))
}

fn error(action: &str, path: &Path, error: io::Error) -> String {
    format!("{action} {}: {error}", path.display())
}

/// Validates a caller-controlled basename and derives its stem and enabled state.
///
/// Requiring exactly one normal path component prevents traversal or absolute paths;
/// explicit separator/control checks keep behavior consistent across platforms.
fn parse_filename(filename: &str) -> Result<(&str, bool), String> {
    if filename.is_empty()
        || filename
            .chars()
            .any(|c| c.is_control() || matches!(c, '/' | '\\' | ':'))
        || Path::new(filename).components().count() != 1
        || !matches!(
            Path::new(filename).components().next(),
            Some(Component::Normal(_))
        )
    {
        return Err("Mod filename must be a safe basename".into());
    }
    let parsed = filename
        .strip_suffix(".jar.disabled")
        .map(|stem| (stem, false))
        .or_else(|| filename.strip_suffix(".jar").map(|stem| (stem, true)));
    match parsed {
        Some((stem, enabled)) if !stem.is_empty() => Ok((stem, enabled)),
        _ => Err("Mod filename must end in .jar or .jar.disabled with a nonempty name".into()),
    }
}

/// Resolves `game_dir/mods` while rejecting traversal and non-directory ancestors.
///
/// Game directories must be absolute (they come from `AppPaths`); relative paths are
/// rejected rather than resolved against the working directory, so every component
/// can be inspected from a concrete root. `symlink_metadata` deliberately observes
/// links themselves instead of following them. The first missing component means the
/// mods directory is absent; callers decide whether absence is empty or an error.
fn mods_directory(game_dir: &Path) -> Result<Option<PathBuf>, String> {
    if !game_dir.is_absolute() {
        return Err("Game directory must be an absolute path".into());
    }
    let path = game_dir.join("mods");
    // Validate the whole lexical path before an early return on a missing component.
    if path.components().any(|c| matches!(c, Component::ParentDir)) {
        return Err("Game directory must not contain parent traversal".into());
    }
    let mut checked = PathBuf::new();
    for component in path.components() {
        checked.push(component.as_os_str());
        match fs::symlink_metadata(&checked) {
            Ok(metadata) if metadata.file_type().is_dir() => {}
            Ok(_) => {
                return Err(format!(
                    "Refusing non-directory or symlink: {}",
                    checked.display()
                ));
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(error("Inspect directory", &checked, e)),
        }
    }
    Ok(Some(path))
}

/// Returns the path only when both its directory chain and leaf type are acceptable.
///
/// This check does not open or lock the file, so callers still rely on exclusive
/// control of the instance directory between validation and mutation.
fn existing_mod(game_dir: &Path, filename: &str) -> Result<PathBuf, String> {
    parse_filename(filename)?;
    let mods = mods_directory(game_dir)?.ok_or("Mods directory does not exist")?;
    let path = mods.join(filename);
    let metadata = fs::symlink_metadata(&path).map_err(|e| error("Inspect mod", &path, e))?;
    if !metadata.file_type().is_file() {
        return Err(format!(
            "Refusing non-regular mod or symlink: {}",
            path.display()
        ));
    }
    Ok(path)
}

fn text(value: &Value) -> Option<String> {
    value
        .as_str()
        .filter(|s| !s.trim().is_empty())
        .map(str::to_owned)
}

/// Accepts only relative, normalized, slash-separated archive entry names.
fn safe_zip_path(path: &str) -> bool {
    !path.is_empty()
        && !path
            .chars()
            .any(|c| c.is_control() || matches!(c, '\\' | ':'))
        && path
            .split('/')
            .all(|part| !part.is_empty() && part != "." && part != "..")
}

/// Reads one regular ZIP entry without extraction and within `limit` bytes.
///
/// Both the declared uncompressed size and the streamed result are checked. Reading
/// one extra byte catches archives whose actual output exceeds their header claim.
/// Unix mode checks reject entries explicitly marked as links or special files.
fn zip_bytes(archive: &mut ZipArchive<File>, path: &str, limit: u64) -> Option<Vec<u8>> {
    if !safe_zip_path(path) {
        return None;
    }
    let entry = archive.by_name(path).ok()?;
    if entry.is_dir() || entry.size() > limit {
        return None;
    }
    if let Some(mode) = entry.unix_mode() {
        let kind = mode & 0o170000;
        if kind != 0 && kind != 0o100000 {
            return None;
        }
    }
    let mut bytes = Vec::new();
    entry.take(limit + 1).read_to_end(&mut bytes).ok()?;
    (bytes.len() as u64 <= limit).then_some(bytes)
}

/// Enriches filename-derived metadata from the first useful recognized descriptor.
///
/// Descriptor precedence is Fabric, Quilt, then legacy Forge. Their schemas place
/// fields at different nesting levels, so the selected `details` object normalizes
/// subsequent name/version/icon handling. A descriptor counts as useful when any
/// supported field is valid; otherwise scanning continues to the next format.
fn read_metadata(path: &Path, installed: &mut InstalledMod) {
    let Ok(file) = File::open(path) else { return };
    let Ok(mut archive) = ZipArchive::new(file) else {
        return;
    };
    for kind in ["fabric.mod.json", "quilt.mod.json", "mcmod.info"] {
        let Some(bytes) = zip_bytes(&mut archive, kind, METADATA_LIMIT) else {
            continue;
        };
        let Ok(root) = serde_json::from_slice::<Value>(&bytes) else {
            continue;
        };
        let (details, version, icon_key) = match kind {
            "quilt.mod.json" => (
                &root["quilt_loader"]["metadata"],
                &root["quilt_loader"]["version"],
                "icon",
            ),
            "mcmod.info" => {
                let details = root
                    .as_array()
                    .and_then(|a| a.first())
                    .or_else(|| {
                        root.get("modList")
                            .and_then(Value::as_array)
                            .and_then(|a| a.first())
                    })
                    .unwrap_or(&root);
                (details, &details["version"], "logoFile")
            }
            _ => (&root, &root["version"], "icon"),
        };
        let name = text(&details["name"]);
        let version = text(version);
        let icon_value = &details[icon_key];
        let icon_path = text(icon_value).or_else(|| {
            // Fabric may map nominal pixel sizes to paths; choose the largest valid key.
            icon_value
                .as_object()?
                .iter()
                .filter_map(|(size, path)| Some((size.parse::<u32>().ok()?, path.as_str()?)))
                .max_by_key(|(size, _)| *size)
                .map(|(_, path)| path.to_owned())
        });
        let icon = icon_path.and_then(|path| zip_bytes(&mut archive, &path, ICON_LIMIT));
        if name.is_some() || version.is_some() || icon.is_some() {
            if let Some(name) = name {
                installed.name = name;
            }
            installed.version = version;
            installed.icon = icon;
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::sync::atomic::{AtomicU64, Ordering};
    use zip::{ZipWriter, write::FileOptions};

    struct Temp(PathBuf);
    impl Temp {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            loop {
                let path = std::env::temp_dir().join(format!(
                    "ferrite-instance-mods-{}-{}",
                    std::process::id(),
                    NEXT.fetch_add(1, Ordering::Relaxed)
                ));
                match fs::create_dir(&path) {
                    Ok(()) => {
                        let path = fs::canonicalize(path).unwrap();
                        fs::create_dir(path.join("mods")).unwrap();
                        return Self(path);
                    }
                    Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                    Err(e) => panic!("Create test directory: {e}"),
                }
            }
        }
        fn mod_path(&self, name: &str) -> PathBuf {
            self.0.join("mods").join(name)
        }
        fn jar(&self, name: &str, entries: &[(&str, &[u8])]) {
            let mut writer = ZipWriter::new(File::create(self.mod_path(name)).unwrap());
            for (path, bytes) in entries {
                writer.start_file(*path, FileOptions::default()).unwrap();
                writer.write_all(bytes).unwrap();
            }
            writer.finish().unwrap();
        }
    }
    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn lists_metadata_and_manual_jars_in_order() {
        let dir = Temp::new();
        dir.jar(
            "a.jar",
            &[
                (
                    "fabric.mod.json",
                    br#"{"name":"Fabric","version":"1.2","icon":{"16":"icon.png"}}"#,
                ),
                ("icon.png", b"image"),
            ],
        );
        dir.jar(
            "b.jar.disabled",
            &[(
                "quilt.mod.json",
                br#"{"quilt_loader":{"version":"2","metadata":{"name":"Quilt"}}}"#,
            )],
        );
        dir.jar(
            "c.jar",
            &[("mcmod.info", br#"[{"name":"Forge","version":"3"}]"#)],
        );
        dir.jar("d.jar", &[("fabric.mod.json", b"bad json")]);
        fs::write(dir.mod_path("e.jar"), b"not a zip").unwrap();
        fs::write(dir.mod_path("ignored.txt"), b"ignored").unwrap();
        fs::create_dir(dir.mod_path("directory.jar")).unwrap();
        let mods = list(&dir.0).unwrap();
        assert_eq!(
            mods.iter().map(|m| m.name.as_str()).collect::<Vec<_>>(),
            ["Fabric", "Quilt", "Forge", "d", "e"]
        );
        assert_eq!(mods[0].version.as_deref(), Some("1.2"));
        assert_eq!(mods[0].icon.as_deref(), Some(b"image".as_slice()));
        assert!(!mods[1].enabled);
        assert_eq!(mods[1].filename, "b.jar.disabled");
        assert_eq!(mods[2].version.as_deref(), Some("3"));
        assert_eq!(mods[4].version, None);
    }

    #[test]
    fn toggle_conflicts_uninstall_and_isolation() {
        let dir = Temp::new();
        let other = Temp::new();
        fs::write(dir.mod_path("test.jar"), b"original").unwrap();
        fs::write(other.mod_path("test.jar"), b"other").unwrap();
        assert_eq!(set_enabled(&dir.0, "test.jar", true).unwrap(), "test.jar");
        assert_eq!(
            set_enabled(&dir.0, "test.jar", false).unwrap(),
            "test.jar.disabled"
        );
        fs::write(dir.mod_path("test.jar"), b"conflict").unwrap();
        assert!(set_enabled(&dir.0, "test.jar.disabled", true).is_err());
        assert_eq!(fs::read(dir.mod_path("test.jar")).unwrap(), b"conflict");
        assert_eq!(
            fs::read(dir.mod_path("test.jar.disabled")).unwrap(),
            b"original"
        );
        uninstall(&dir.0, "test.jar").unwrap();
        set_enabled(&dir.0, "test.jar.disabled", true).unwrap();
        uninstall(&dir.0, "test.jar").unwrap();
        assert!(list(&dir.0).unwrap().is_empty());
        assert!(uninstall(&dir.0, "test.jar").is_err());
        assert_eq!(fs::read(other.mod_path("test.jar")).unwrap(), b"other");
    }

    #[test]
    fn validates_names_and_directories() {
        let dir = Temp::new();
        for name in [
            "",
            "..",
            "../x.jar",
            "/x.jar",
            "sub/x.jar",
            "sub\\x.jar",
            "C:x.jar",
            "x.jar\0",
            "x.txt",
            ".jar",
        ] {
            assert!(set_enabled(&dir.0, name, false).is_err(), "{name:?}");
            assert!(uninstall(&dir.0, name).is_err(), "{name:?}");
        }
        fs::create_dir(dir.mod_path("directory.jar")).unwrap();
        assert!(uninstall(&dir.0, "directory.jar").is_err());
        assert!(set_enabled(&dir.0, "directory.jar", true).is_err());
        assert!(list(dir.0.join("missing")).unwrap().is_empty());
        assert!(list(dir.0.join("missing/../other")).is_err());
        fs::write(dir.0.join("file"), b"x").unwrap();
        assert!(list(dir.0.join("file")).is_err());
    }

    #[test]
    fn metadata_and_icon_limits_and_legacy_wrapper() {
        let dir = Temp::new();
        let huge = vec![b' '; METADATA_LIMIT as usize + 1];
        let huge_icon = vec![0; ICON_LIMIT as usize + 1];
        dir.jar(
            "a.jar",
            &[
                ("fabric.mod.json", &huge),
                (
                    "mcmod.info",
                    br#"{"modList":[{"name":"Legacy","version":"4"}]}"#,
                ),
            ],
        );
        dir.jar(
            "b.jar",
            &[
                ("fabric.mod.json", br#"{"name":"Bounded","icon":"big.png"}"#),
                ("big.png", &huge_icon),
            ],
        );
        dir.jar(
            "c.jar",
            &[
                ("fabric.mod.json", br#"{"icon":"../outside.png"}"#),
                ("../outside.png", b"no"),
            ],
        );
        dir.jar(
            "d.jar",
            &[(
                "fabric.mod.json",
                br#"{"name":"Missing","icon":"absent.png"}"#,
            )],
        );
        let mods = list(&dir.0).unwrap();
        assert_eq!(mods[0].name, "Legacy");
        assert_eq!(mods[0].version.as_deref(), Some("4"));
        assert!(mods.iter().all(|m| m.icon.is_none()));
        assert_eq!(mods[2].name, "c");
        for path in ["/absolute", "../x", "a/../x", "a\\x", "C:x", "a//b", "./x"] {
            assert!(!safe_zip_path(path));
        }
    }

    #[cfg(unix)]
    #[test]
    fn refuses_symlink_files_destinations_and_directories() {
        use std::os::unix::fs::symlink;
        let dir = Temp::new();
        let outside = Temp::new();
        fs::write(outside.mod_path("real.jar"), b"safe").unwrap();
        symlink(outside.mod_path("real.jar"), dir.mod_path("link.jar")).unwrap();
        assert!(list(&dir.0).unwrap().is_empty());
        assert!(uninstall(&dir.0, "link.jar").is_err());
        assert!(set_enabled(&dir.0, "link.jar", true).is_err());
        fs::write(dir.mod_path("test.jar"), b"original").unwrap();
        symlink(outside.0.join("missing"), dir.mod_path("test.jar.disabled")).unwrap();
        assert!(set_enabled(&dir.0, "test.jar", false).is_err());
        assert_eq!(fs::read(dir.mod_path("test.jar")).unwrap(), b"original");
        symlink(&outside.0, dir.0.join("linked-game")).unwrap();
        assert!(list(dir.0.join("linked-game")).is_err());
        fs::create_dir(dir.0.join("nested")).unwrap();
        symlink(outside.0.join("mods"), dir.0.join("nested/mods")).unwrap();
        assert!(list(dir.0.join("nested")).is_err());
        assert!(uninstall(dir.0.join("nested"), "real.jar").is_err());
        assert_eq!(fs::read(outside.mod_path("real.jar")).unwrap(), b"safe");
    }
}
