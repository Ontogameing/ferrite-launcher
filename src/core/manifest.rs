//! Versioned on-disk model for the instance list.
//!
//! ## Formats
//!
//! * **Version 0** (pre-Stage-1, unversioned): a bare JSON array of profiles,
//!   `[{"name": ..., "version": ..., "loader": ..., "directory": ...}, ...]`.
//! * **Version 1** (current): an object with an explicit schema version,
//!   `{"schema_version": 1, "instances": [<profile>, ...]}`. Profile objects have the
//!   same four string fields as version 0.
//!
//! [`parse_manifest`] accepts both and upgrades version 0 in memory; the file is
//! rewritten as version 1 only on the next save. A `schema_version` greater than
//! [`CURRENT_SCHEMA_VERSION`] is refused with [`ManifestError::UnsupportedVersion`]
//! so a newer launcher's data is never silently downgraded or overwritten.
//!
//! ## Per-entry validation
//!
//! Each profile's `directory` must be a single plain folder name (see
//! [`InstanceDirName`]). Entries that are malformed or carry an unsafe directory are
//! not loaded; they are returned in [`InstanceManifest::skipped`] with a reason and
//! their raw JSON, so callers can report them and write them back untouched instead
//! of silently deleting user data. Duplicate directories are also skipped (after the
//! first), because two profiles sharing one game directory would let deleting one
//! destroy the other's files.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashSet;
use std::fmt;

/// Schema version written by this build.
pub const CURRENT_SCHEMA_VERSION: u32 = 1;

/// Maximum accepted directory name length in bytes (common filesystem limit).
const MAX_DIR_NAME_BYTES: usize = 255;
/// Length cap applied to generated slugs, leaving room for numeric suffixes.
const MAX_SLUG_CHARS: usize = 64;

// =====================================================================
// Directory names
// =====================================================================

/// Why a stored or proposed instance directory name was rejected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DirNameError {
    Empty,
    DotComponent,
    Separator,
    DrivePrefixOrColon,
    ControlCharacter,
    WindowsInvalidCharacter(char),
    TrailingDotOrSpace,
    WindowsReservedName,
    TooLong,
}

impl fmt::Display for DirNameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => write!(f, "directory name is empty"),
            Self::DotComponent => write!(f, "directory name must not be '.' or '..'"),
            Self::Separator => write!(f, "directory name must not contain '/' or '\\'"),
            Self::DrivePrefixOrColon => {
                write!(f, "directory name must not contain ':' (drive prefix)")
            }
            Self::ControlCharacter => write!(f, "directory name contains a control character"),
            Self::WindowsInvalidCharacter(c) => {
                write!(f, "directory name contains invalid character {c:?}")
            }
            Self::TrailingDotOrSpace => {
                write!(f, "directory name must not end with '.' or a space")
            }
            Self::WindowsReservedName => {
                write!(f, "directory name is a reserved Windows device name")
            }
            Self::TooLong => write!(f, "directory name exceeds {MAX_DIR_NAME_BYTES} bytes"),
        }
    }
}

impl std::error::Error for DirNameError {}

/// Returns whether `name` (case-insensitive, ignoring any extension and trailing
/// dots/spaces, as Windows does) is a reserved DOS device name.
fn is_windows_reserved(name: &str) -> bool {
    let stem = name
        .split('.')
        .next()
        .unwrap_or(name)
        .trim_end_matches([' ', '.'])
        .to_ascii_uppercase();
    if matches!(
        stem.as_str(),
        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$" | "CLOCK$"
    ) {
        return true;
    }
    ["COM", "LPT"].iter().any(|prefix| {
        stem.strip_prefix(prefix).is_some_and(|rest| {
            matches!(
                rest,
                "0" | "1"
                    | "2"
                    | "3"
                    | "4"
                    | "5"
                    | "6"
                    | "7"
                    | "8"
                    | "9"
                    | "\u{b9}"
                    | "\u{b2}"
                    | "\u{b3}"
            )
        })
    })
}

/// Validates that `name` is one plain, portable folder name.
///
/// Rejected on every platform (the data may move between OSes): empty, `.`/`..`,
/// any `/` or `\`, any `:` (drive prefixes like `C:x`, NTFS streams), control
/// characters, `<>"|?*`, trailing dots/spaces, Windows reserved device names
/// (`CON`, `nul.txt`, `COM1`, ...), and names longer than 255 bytes. Absolute paths
/// are necessarily rejected because they contain a separator or drive prefix.
pub fn validate_dir_name(name: &str) -> Result<(), DirNameError> {
    if name.is_empty() {
        return Err(DirNameError::Empty);
    }
    if name == "." || name == ".." {
        return Err(DirNameError::DotComponent);
    }
    if name.contains(['/', '\\']) {
        return Err(DirNameError::Separator);
    }
    if name.contains(':') {
        return Err(DirNameError::DrivePrefixOrColon);
    }
    if name.chars().any(char::is_control) {
        return Err(DirNameError::ControlCharacter);
    }
    if let Some(c) = name
        .chars()
        .find(|c| matches!(c, '<' | '>' | '"' | '|' | '?' | '*'))
    {
        return Err(DirNameError::WindowsInvalidCharacter(c));
    }
    if name.ends_with(['.', ' ']) {
        return Err(DirNameError::TrailingDotOrSpace);
    }
    if is_windows_reserved(name) {
        return Err(DirNameError::WindowsReservedName);
    }
    if name.len() > MAX_DIR_NAME_BYTES {
        return Err(DirNameError::TooLong);
    }
    Ok(())
}

/// A validated single-component instance directory name.
///
/// The only constructors validate, and deserialization goes through the same check,
/// so an `InstanceDirName` can always be joined onto the instances root without
/// escaping it.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct InstanceDirName(String);

impl InstanceDirName {
    /// Validates and wraps `name`.
    pub fn parse(name: impl Into<String>) -> Result<Self, DirNameError> {
        let name = name.into();
        validate_dir_name(&name)?;
        Ok(Self(name))
    }

    /// The folder name.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Test-only escape hatch for exercising defense-in-depth checks downstream.
    #[cfg(test)]
    pub(crate) fn unchecked_for_tests(name: &str) -> Self {
        Self(name.to_owned())
    }
}

impl TryFrom<String> for InstanceDirName {
    type Error = DirNameError;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::parse(value)
    }
}

impl From<InstanceDirName> for String {
    fn from(value: InstanceDirName) -> Self {
        value.0
    }
}

impl fmt::Display for InstanceDirName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Converts a display name into a portable ASCII directory slug.
///
/// ASCII letters are lowercased; digits, `-`, and `_` are preserved; everything else
/// becomes `-`. Leading/trailing dashes are trimmed, the result is capped at 64
/// characters, an empty result becomes `instance`, and a Windows reserved name gets an
/// `-instance` suffix so the slug always passes [`validate_dir_name`].
pub fn directory_slug(name: &str) -> String {
    let slug: String = name
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || character == '-' || character == '_' {
                character.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>()
        .trim_matches('-')
        .chars()
        .take(MAX_SLUG_CHARS)
        .collect::<String>()
        .trim_end_matches('-')
        .to_owned();

    if slug.is_empty() {
        "instance".to_owned()
    } else if validate_dir_name(&slug).is_err() {
        format!("{slug}-instance")
    } else {
        slug
    }
}

/// Comparison key for folder names: Unicode-lowercased with trailing dots and spaces
/// removed, because Windows and macOS folders are case-insensitive and Windows ignores
/// trailing dots/spaces (`Foo.` and `foo` are the same folder there).
///
/// This does not apply Unicode normalization (NFC/NFD); generated slugs are ASCII, so
/// only hand-edited legacy names could differ in normalization alone.
pub fn directory_key(name: &str) -> String {
    name.trim_end_matches(['.', ' ']).to_lowercase()
}

/// Comparison key for display names: trimmed and Unicode-lowercased.
pub fn name_key(name: &str) -> String {
    name.trim().to_lowercase()
}

// =====================================================================
// Profiles
// =====================================================================

/// A saved Minecraft profile and the directory containing its game data.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstanceProfile {
    /// The unique, user-facing profile name.
    pub name: String,
    /// The base Minecraft version, such as `1.21.1`.
    pub version: String,
    /// The serialized display label of the selected mod loader (stored opaquely).
    pub loader: String,
    /// Single folder name under the instances root. Private so callers cannot
    /// redirect an instance after creation; always validated.
    directory: InstanceDirName,
}

impl InstanceProfile {
    /// Creates metadata for a new profile without writing it to disk.
    ///
    /// The directory is derived from the name and receives a numeric suffix if that
    /// directory is already used by `existing` (compared case-insensitively).
    ///
    /// This only considers `existing`; launcher code that creates real folders should
    /// use `core::instances::new_instance_profile`, which also checks skipped manifest
    /// entries and folders already on disk.
    pub fn new(name: String, version: String, loader: String, existing: &[Self]) -> Self {
        let base = directory_slug(&name);
        let mut directory = base.clone();
        let mut suffix = 2;
        while existing
            .iter()
            .any(|profile| directory_key(profile.directory.as_str()) == directory_key(&directory))
        {
            directory = format!("{base}-{suffix}");
            suffix += 1;
        }
        let directory = InstanceDirName::parse(directory)
            .expect("slugs with numeric suffixes are always valid directory names");
        Self::with_directory(name, version, loader, directory)
    }

    /// Creates metadata for a profile stored in an already chosen, validated folder.
    pub fn with_directory(
        name: String,
        version: String,
        loader: String,
        directory: InstanceDirName,
    ) -> Self {
        Self {
            name,
            version,
            loader,
            directory,
        }
    }

    /// The validated folder name under the instances root.
    pub fn directory(&self) -> &InstanceDirName {
        &self.directory
    }

    /// Test-only constructor that bypasses directory validation.
    #[cfg(test)]
    pub(crate) fn with_unchecked_directory_for_tests(name: &str, directory: &str) -> Self {
        Self {
            name: name.to_owned(),
            version: "1.20.1".to_owned(),
            loader: "Vanilla".to_owned(),
            directory: InstanceDirName::unchecked_for_tests(directory),
        }
    }
}

// =====================================================================
// Manifest
// =====================================================================

/// Errors that make the whole manifest unusable.
#[derive(Debug)]
pub enum ManifestError {
    /// Not valid JSON.
    Json(serde_json::Error),
    /// Valid JSON but not a recognizable Ferrite manifest shape.
    InvalidFormat(String),
    /// Written by a newer Ferrite; refusing to read or overwrite it.
    UnsupportedVersion { found: u64, supported: u32 },
}

impl fmt::Display for ManifestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Json(error) => write!(f, "instance manifest is not valid JSON: {error}"),
            Self::InvalidFormat(detail) => write!(f, "unrecognized instance manifest: {detail}"),
            Self::UnsupportedVersion { found, supported } => write!(
                f,
                "instance manifest uses schema_version {found}, but this Ferrite build only \
                 understands up to {supported}; it was written by a newer version and will \
                 not be modified"
            ),
        }
    }
}

impl std::error::Error for ManifestError {}

impl From<serde_json::Error> for ManifestError {
    fn from(error: serde_json::Error) -> Self {
        Self::Json(error)
    }
}

/// Why an individual entry was not loaded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkipReason {
    /// The entry is not an object with string `name`/`version`/`loader`/`directory`.
    Malformed(String),
    /// The entry is well-formed but its `directory` is unsafe.
    InvalidDirectory {
        directory: String,
        error: DirNameError,
    },
    /// Another earlier entry already uses this directory.
    DuplicateDirectory(String),
}

impl fmt::Display for SkipReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Malformed(detail) => write!(f, "malformed entry: {detail}"),
            Self::InvalidDirectory { directory, error } => {
                write!(f, "unsafe directory {directory:?}: {error}")
            }
            Self::DuplicateDirectory(directory) => {
                write!(
                    f,
                    "directory {directory:?} is already used by another instance"
                )
            }
        }
    }
}

/// An entry that was reported and skipped, kept verbatim so it can be preserved.
#[derive(Debug, Clone, PartialEq)]
pub struct SkippedEntry {
    /// Zero-based position in the on-disk array.
    pub index: usize,
    /// Profile name, when one could be read.
    pub name: Option<String>,
    pub reason: SkipReason,
    /// The original JSON value, written back unchanged on save.
    pub raw: Value,
}

impl SkippedEntry {
    /// The raw `directory` string of this entry, if it has one (it may be unsafe or
    /// shared with another entry; never join it onto a path without validation).
    pub fn raw_directory(&self) -> Option<&str> {
        self.raw.get("directory").and_then(Value::as_str)
    }
}

impl fmt::Display for SkippedEntry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.name {
            Some(name) => write!(f, "instance #{} ({name:?}): {}", self.index, self.reason),
            None => write!(f, "instance #{}: {}", self.index, self.reason),
        }
    }
}

/// A parsed manifest, upgraded in memory to the current schema.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct InstanceManifest {
    /// Schema version found on disk (0 for the legacy bare array).
    pub source_version: u32,
    /// Valid profiles, in on-disk order.
    pub instances: Vec<InstanceProfile>,
    /// Entries that were reported and skipped.
    pub skipped: Vec<SkippedEntry>,
}

impl InstanceManifest {
    /// Whether at least one entry had the shape of a Ferrite profile (regardless of
    /// directory validity). Used to decide whether a legacy folder is Ferrite data.
    pub fn has_ferrite_shaped_entries(&self) -> bool {
        !self.instances.is_empty()
            || self
                .skipped
                .iter()
                .any(|entry| !matches!(entry.reason, SkipReason::Malformed(_)))
    }
}

/// Shape check that ignores directory validity.
#[derive(Deserialize)]
struct RawProfile {
    name: String,
    #[allow(dead_code)]
    version: String,
    #[allow(dead_code)]
    loader: String,
    directory: String,
}

/// Parses either supported manifest version. See the module docs.
pub fn parse_manifest(text: &str) -> Result<InstanceManifest, ManifestError> {
    let value: Value = serde_json::from_str(text)?;
    let (source_version, entries) = match value {
        Value::Array(entries) => (0, entries),
        Value::Object(mut object) => {
            let version = object.get("schema_version").ok_or_else(|| {
                ManifestError::InvalidFormat("object without a schema_version field".into())
            })?;
            let version = version.as_u64().ok_or_else(|| {
                ManifestError::InvalidFormat("schema_version must be a non-negative integer".into())
            })?;
            if version > u64::from(CURRENT_SCHEMA_VERSION) {
                return Err(ManifestError::UnsupportedVersion {
                    found: version,
                    supported: CURRENT_SCHEMA_VERSION,
                });
            }
            if version == 0 {
                return Err(ManifestError::InvalidFormat(
                    "schema_version 0 is only valid as the legacy bare-array format".into(),
                ));
            }
            match object.remove("instances") {
                Some(Value::Array(entries)) => (version as u32, entries),
                Some(_) => {
                    return Err(ManifestError::InvalidFormat(
                        "instances must be an array".into(),
                    ));
                }
                None => {
                    return Err(ManifestError::InvalidFormat(
                        "missing instances array".into(),
                    ));
                }
            }
        }
        _ => {
            return Err(ManifestError::InvalidFormat(
                "expected a JSON array (v0) or object (v1+)".into(),
            ));
        }
    };

    let mut manifest = InstanceManifest {
        source_version,
        ..InstanceManifest::default()
    };
    let mut seen = HashSet::new();
    for (index, raw) in entries.into_iter().enumerate() {
        let shaped = match serde_json::from_value::<RawProfile>(raw.clone()) {
            Ok(shaped) => shaped,
            Err(error) => {
                let name = raw.get("name").and_then(Value::as_str).map(str::to_owned);
                manifest.skipped.push(SkippedEntry {
                    index,
                    name,
                    reason: SkipReason::Malformed(error.to_string()),
                    raw,
                });
                continue;
            }
        };
        if let Err(error) = validate_dir_name(&shaped.directory) {
            manifest.skipped.push(SkippedEntry {
                index,
                name: Some(shaped.name),
                reason: SkipReason::InvalidDirectory {
                    directory: shaped.directory,
                    error,
                },
                raw,
            });
            continue;
        }
        // Compare case-insensitively: Windows and macOS folders are case-insensitive.
        if !seen.insert(shaped.directory.to_ascii_lowercase()) {
            manifest.skipped.push(SkippedEntry {
                index,
                name: Some(shaped.name),
                reason: SkipReason::DuplicateDirectory(shaped.directory),
                raw,
            });
            continue;
        }
        match serde_json::from_value::<InstanceProfile>(raw.clone()) {
            Ok(profile) => manifest.instances.push(profile),
            Err(error) => manifest.skipped.push(SkippedEntry {
                index,
                name: Some(shaped.name),
                reason: SkipReason::Malformed(error.to_string()),
                raw,
            }),
        }
    }
    Ok(manifest)
}

/// Serializes the current schema. `preserved` entries (previously skipped) are
/// appended verbatim so saving never drops data this build could not load.
pub fn serialize_manifest(
    instances: &[InstanceProfile],
    preserved: &[SkippedEntry],
) -> Result<String, serde_json::Error> {
    let mut entries = Vec::with_capacity(instances.len() + preserved.len());
    for profile in instances {
        entries.push(serde_json::to_value(profile)?);
    }
    entries.extend(preserved.iter().map(|entry| entry.raw.clone()));
    let document = serde_json::json!({
        "schema_version": CURRENT_SCHEMA_VERSION,
        "instances": entries,
    });
    let mut text = serde_json::to_string_pretty(&document)?;
    text.push('\n');
    Ok(text)
}

/// Serializes the legacy version-0 format: a plain JSON array of instance objects,
/// with preserved (skipped/invalid) entries appended verbatim.
///
/// Only used when the storage root is a pre-Stage-1 `minecraft` folder (rollback or
/// "use old data this time"), so older Ferrite builds can still read that folder.
pub fn serialize_manifest_v0(
    instances: &[InstanceProfile],
    preserved: &[SkippedEntry],
) -> Result<String, serde_json::Error> {
    let mut entries = Vec::with_capacity(instances.len() + preserved.len());
    for profile in instances {
        entries.push(serde_json::to_value(profile)?);
    }
    entries.extend(preserved.iter().map(|entry| entry.raw.clone()));
    let mut text = serde_json::to_string_pretty(&Value::Array(entries))?;
    text.push('\n');
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    const V0: &str = r#"[
  {"name": "Fabric 1.21", "version": "1.21.1", "loader": "Fabric", "directory": "fabric-1-21"},
  {"name": "Vanilla", "version": "1.20.1", "loader": "Vanilla", "directory": "vanilla"}
]"#;

    #[test]
    fn loads_v0_bare_array_and_upgrades_in_memory() {
        let manifest = parse_manifest(V0).unwrap();
        assert_eq!(manifest.source_version, 0);
        assert!(manifest.skipped.is_empty());
        assert_eq!(manifest.instances.len(), 2);
        assert_eq!(manifest.instances[0].name, "Fabric 1.21");
        assert_eq!(manifest.instances[0].directory().as_str(), "fabric-1-21");
        assert_eq!(manifest.instances[1].loader, "Vanilla");

        let upgraded = serialize_manifest(&manifest.instances, &[]).unwrap();
        let value: Value = serde_json::from_str(&upgraded).unwrap();
        assert_eq!(value["schema_version"], 1);
        assert_eq!(value["instances"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn v1_round_trips() {
        let original = parse_manifest(V0).unwrap();
        let text = serialize_manifest(&original.instances, &[]).unwrap();
        let reparsed = parse_manifest(&text).unwrap();
        assert_eq!(reparsed.source_version, 1);
        assert_eq!(reparsed.instances, original.instances);
        assert_eq!(serialize_manifest(&reparsed.instances, &[]).unwrap(), text);
    }

    #[test]
    fn unknown_future_version_is_refused() {
        let text = r#"{"schema_version": 2, "instances": [], "something_new": true}"#;
        match parse_manifest(text) {
            Err(ManifestError::UnsupportedVersion { found, supported }) => {
                assert_eq!(found, 2);
                assert_eq!(supported, CURRENT_SCHEMA_VERSION);
            }
            other => panic!("expected UnsupportedVersion, got {other:?}"),
        }
        let message = parse_manifest(text).unwrap_err().to_string();
        assert!(message.contains("newer"), "{message}");
    }

    #[test]
    fn unrecognized_shapes_are_errors() {
        for text in [
            "42",
            "\"text\"",
            "{}",
            r#"{"schema_version": "1", "instances": []}"#,
            r#"{"schema_version": 1}"#,
            r#"{"schema_version": 1, "instances": {}}"#,
            r#"{"schema_version": 0, "instances": []}"#,
        ] {
            assert!(
                matches!(parse_manifest(text), Err(ManifestError::InvalidFormat(_))),
                "{text}"
            );
        }
        assert!(matches!(
            parse_manifest("[not json"),
            Err(ManifestError::Json(_))
        ));
    }

    #[test]
    fn directory_name_validation() {
        let rejected = [
            ("..", DirNameError::DotComponent),
            (".", DirNameError::DotComponent),
            ("", DirNameError::Empty),
            ("a/b", DirNameError::Separator),
            ("a\\b", DirNameError::Separator),
            ("/etc", DirNameError::Separator),
            ("/", DirNameError::Separator),
            ("C:\\Windows", DirNameError::Separator),
            ("\\\\server\\share", DirNameError::Separator),
            ("C:x", DirNameError::DrivePrefixOrColon),
            ("C:", DirNameError::DrivePrefixOrColon),
            ("name:stream", DirNameError::DrivePrefixOrColon),
            ("CON", DirNameError::WindowsReservedName),
            ("con", DirNameError::WindowsReservedName),
            ("nul.txt", DirNameError::WindowsReservedName),
            ("Com1", DirNameError::WindowsReservedName),
            ("LPT9.log", DirNameError::WindowsReservedName),
            ("aux.tar.gz", DirNameError::WindowsReservedName),
            ("COM\u{b9}", DirNameError::WindowsReservedName),
            ("CONIN$", DirNameError::WindowsReservedName),
            ("name.", DirNameError::TrailingDotOrSpace),
            ("name ", DirNameError::TrailingDotOrSpace),
            ("tab\there", DirNameError::ControlCharacter),
            ("nul\0byte", DirNameError::ControlCharacter),
            ("what?", DirNameError::WindowsInvalidCharacter('?')),
        ];
        for (name, expected) in rejected {
            assert_eq!(validate_dir_name(name), Err(expected), "{name:?}");
            assert!(InstanceDirName::parse(name).is_err(), "{name:?}");
        }
        assert_eq!(
            validate_dir_name(&"a".repeat(256)),
            Err(DirNameError::TooLong)
        );
        for accepted in [
            "fabric-1-21",
            "my_pack",
            "console",
            "com10",
            "COM0x",
            "Älteres Profil",
            "a.b",
            ".hidden",
            "con-instance",
        ] {
            assert_eq!(validate_dir_name(accepted), Ok(()), "{accepted:?}");
        }
    }

    #[test]
    fn invalid_entries_are_reported_skipped_and_preserved() {
        let text = r#"[
  {"name": "ok", "version": "1", "loader": "Vanilla", "directory": "ok"},
  {"name": "escape", "version": "1", "loader": "Vanilla", "directory": "../../etc"},
  {"name": "abs", "version": "1", "loader": "Vanilla", "directory": "/tmp/evil"},
  {"name": "drive", "version": "1", "loader": "Vanilla", "directory": "C:x"},
  {"name": "dup", "version": "1", "loader": "Vanilla", "directory": "OK"},
  {"name": 5},
  "garbage"
]"#;
        let manifest = parse_manifest(text).unwrap();
        assert_eq!(manifest.instances.len(), 1);
        assert_eq!(manifest.instances[0].name, "ok");
        assert_eq!(manifest.skipped.len(), 6);
        assert!(matches!(
            manifest.skipped[0].reason,
            SkipReason::InvalidDirectory { .. }
        ));
        assert!(matches!(
            manifest.skipped[3].reason,
            SkipReason::DuplicateDirectory(_)
        ));
        assert!(matches!(
            manifest.skipped[4].reason,
            SkipReason::Malformed(_)
        ));
        assert!(manifest.has_ferrite_shaped_entries());
        for entry in &manifest.skipped {
            assert!(!entry.to_string().is_empty());
        }

        // Saving keeps the skipped entries verbatim, and they are skipped again on load.
        let saved = serialize_manifest(&manifest.instances, &manifest.skipped).unwrap();
        let reloaded = parse_manifest(&saved).unwrap();
        assert_eq!(reloaded.instances, manifest.instances);
        assert_eq!(reloaded.skipped.len(), 6);
        assert_eq!(reloaded.skipped[0].raw, manifest.skipped[0].raw);
    }

    #[test]
    fn slugs_are_always_valid_directory_names() {
        assert_eq!(
            directory_slug("My ../ Fabric Profile"),
            "my-----fabric-profile"
        );
        assert_eq!(directory_slug("测试"), "instance");
        assert_eq!(directory_slug("CON"), "con-instance");
        assert_eq!(directory_slug("nul"), "nul-instance");
        assert!(directory_slug(&"x".repeat(500)).len() <= MAX_SLUG_CHARS);
        for name in ["CON", "..", "C:x", "a/b", "", ".", "LPT1", "   "] {
            assert_eq!(validate_dir_name(&directory_slug(name)), Ok(()), "{name:?}");
        }
        let first = InstanceProfile::new("CON".into(), "1".into(), "Vanilla".into(), &[]);
        let second = InstanceProfile::new(
            "con".into(),
            "1".into(),
            "Vanilla".into(),
            std::slice::from_ref(&first),
        );
        assert_eq!(first.directory().as_str(), "con-instance");
        assert_eq!(second.directory().as_str(), "con-instance-2");
    }
}
