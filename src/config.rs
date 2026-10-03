//! Persistent, non-secret core launcher preferences.
//!
//! [`config_path`] is [`AppPaths::config_file`]: `config.toml` in the platform-specific
//! per-user configuration directory resolved once at startup. The entire [`Config`]
//! is serialized as human-readable TOML; missing tables and fields inherit defaults
//! through Serde's `default` handling, while known values are semantically validated.
//!
//! Appearance and layout preferences are frontend-owned and live in `ui.toml`
//! (see [`crate::ui_settings`]). Older builds stored them as `[appearance]` and
//! `[layout]` tables in `config.toml`; this module ignores those tables when parsing,
//! but [`save`] carries them forward verbatim until `ui.toml` exists so that values
//! are never lost before the one-time migration has succeeded.
//!
//! [`load`] is strict. Startup code can instead use [`load_or_create`], which creates
//! a default file when none exists and recovers from syntactically malformed or
//! type-invalid TOML by returning defaults plus a warning. Files that parse but fail
//! semantic validation and filesystem failures are still returned as errors so callers
//! can decide whether an in-memory fallback is safe. Locating the directory itself can
//! no longer fail here; that happens once when [`AppPaths`] is resolved.
//!
//! This module stores preferences only. Credentials and access tokens do not belong in
//! [`Config`] or in raw TOML supplied to [`save_toml`].

use ferrite_launcher::core::paths::AppPaths;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Top-level `config.toml` tables that belong to the frontend (`ui.toml`) and are only
/// carried forward for legacy compatibility until they have been migrated.
pub const LEGACY_UI_TABLES: [&str; 2] = ["appearance", "layout"];

/// Complete on-disk core configuration.
///
/// Deserializing a partial file fills absent sections and fields from [`Default`],
/// which allows newer versions to add settings without requiring a migration.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(default)]
pub struct Config {
    pub launcher: LauncherConfig,
    pub discord: DiscordConfig,
    pub minecraft: MinecraftConfig,
}

impl Config {
    fn validate(&self) -> Result<(), ConfigError> {
        if !(512..=32_768).contains(&self.minecraft.default_memory_mb) {
            return Err(ConfigError::Validation(
                "minecraft.default_memory_mb must be between 512 and 32768".to_owned(),
            ));
        }
        Ok(())
    }
}

/// General launcher behavior and update-check preferences.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct LauncherConfig {
    pub close_on_launch: bool,
    pub show_snapshots: bool,
    pub check_for_updates: bool,
}

impl Default for LauncherConfig {
    fn default() -> Self {
        Self {
            close_on_launch: false,
            show_snapshots: false,
            check_for_updates: true,
        }
    }
}

/// Discord integration preferences; no Discord credentials are stored here.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct DiscordConfig {
    pub rich_presence: bool,
}

impl Default for DiscordConfig {
    fn default() -> Self {
        Self {
            rich_presence: true,
        }
    }
}

/// Defaults used when launching Minecraft instances.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct MinecraftConfig {
    pub default_memory_mb: u32,
}

impl Default for MinecraftConfig {
    fn default() -> Self {
        Self {
            default_memory_mb: 4096,
        }
    }
}

/// Failure while locating, decoding, validating, writing, or revealing configuration.
///
/// Parse and serialization variants retain their typed errors for formatting or direct
/// pattern matching. Because this type does not override [`std::error::Error::source`],
/// callers cannot traverse those values through Rust's standard error-source chain.
/// Displayed messages may include filesystem paths or TOML locations, but this module
/// never intentionally places configuration contents in an error.
#[derive(Debug)]
pub enum ConfigError {
    Io(io::Error),
    Deserialize(toml::de::Error),
    Serialize(toml::ser::Error),
    Validation(String),
    OpenFolder(String),
}

impl fmt::Display for ConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "configuration filesystem error: {error}"),
            Self::Deserialize(error) => write!(formatter, "invalid configuration TOML: {error}"),
            Self::Serialize(error) => {
                write!(formatter, "could not serialize configuration: {error}")
            }
            Self::Validation(error) => write!(formatter, "invalid configuration value: {error}"),
            Self::OpenFolder(error) => write!(formatter, "could not open config folder: {error}"),
        }
    }
}

impl std::error::Error for ConfigError {}

impl From<io::Error> for ConfigError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<toml::de::Error> for ConfigError {
    fn from(error: toml::de::Error) -> Self {
        Self::Deserialize(error)
    }
}

impl From<toml::ser::Error> for ConfigError {
    fn from(error: toml::ser::Error) -> Self {
        Self::Serialize(error)
    }
}

/// Result of startup-oriented loading.
///
/// `warning` is populated when an existing file could not be deserialized and the
/// returned configuration is therefore an in-memory default. The invalid file is
/// left untouched until the caller explicitly saves a setting.
pub struct ConfigLoad {
    /// Configuration to apply for this process.
    pub config: Config,
    /// User-facing explanation of a recoverable fallback, if one occurred.
    pub warning: Option<String>,
}

/// Strictly reads, parses, and validates Ferrite's configuration from disk.
///
/// Unlike [`load_or_create`], a missing or malformed file is returned as an error and
/// no filesystem state is changed.
pub fn load(paths: &AppPaths) -> Result<Config, ConfigError> {
    parse_toml(&read_toml(paths)?)
}

/// Reads the config file as UTF-8 text without parsing or validating it.
///
/// This performs filesystem I/O only and is useful for displaying the exact source
/// in an advanced editor.
pub fn read_toml(paths: &AppPaths) -> Result<String, ConfigError> {
    Ok(fs::read_to_string(config_path(paths))?)
}

/// Parses and validates TOML, applying defaults for missing sections and fields.
///
/// Unknown fields follow Serde's normal behavior and are ignored. No disk I/O occurs.
pub fn parse_toml(input: &str) -> Result<Config, ConfigError> {
    let config: Config = toml::from_str(input)?;
    config.validate()?;
    Ok(config)
}

/// Serializes a complete configuration as human-readable TOML without writing it.
///
/// This does not revalidate a programmatically constructed [`Config`].
pub fn to_toml(config: &Config) -> Result<String, ConfigError> {
    Ok(toml::to_string_pretty(config)?)
}

/// Parses, validates, and saves TOML, returning the normalized configuration.
///
/// Parsing and validation happen before any write, so those failures leave the
/// existing file untouched. Successful output is formatted by [`to_toml`] rather
/// than preserving the input's comments or whitespace.
pub fn save_toml(paths: &AppPaths, input: &str) -> Result<Config, ConfigError> {
    let config = parse_toml(input)?;
    save(paths, &config)?;
    Ok(config)
}

/// Loads configuration for startup, creating a default file when none exists.
///
/// Deserialization failures (including malformed TOML and field type mismatches)
/// return defaults with a warning and preserve the bad file for inspection. Semantic
/// validation and I/O failures remain errors. Creating a missing file also creates
/// its parent directory and can therefore fail.
pub fn load_or_create(paths: &AppPaths) -> Result<ConfigLoad, ConfigError> {
    let path = config_path(paths);
    match read_toml(paths) {
        Ok(input) => match parse_toml(&input) {
            Ok(config) => Ok(ConfigLoad {
                config,
                warning: None,
            }),
            Err(ConfigError::Deserialize(error)) => Ok(ConfigLoad {
                config: Config::default(),
                warning: Some(format!(
                    "Could not read {}: {error}. Using defaults; changing a setting will replace the invalid file.",
                    path.display()
                )),
            }),
            Err(error) => Err(error),
        },
        Err(ConfigError::Io(error)) if error.kind() == io::ErrorKind::NotFound => {
            let config = Config::default();
            save(paths, &config)?;
            Ok(ConfigLoad {
                config,
                warning: None,
            })
        }
        Err(error) => Err(error),
    }
}

/// Saves the complete configuration as human-readable TOML.
///
/// The parent directory is created as needed. Data is written atomically (unique
/// sibling temporary file, fsync, rename), so a failed write leaves the previous file.
///
/// This function serializes the supplied value but does not call semantic validation.
///
/// Until `ui.toml` exists, any legacy `[appearance]`/`[layout]` tables already present
/// in `config.toml` are copied into the new file unchanged, so saving a core setting
/// can never discard UI preferences that have not been migrated yet.
pub fn save(paths: &AppPaths, config: &Config) -> Result<(), ConfigError> {
    let mut text = to_toml(config)?;
    if !paths.ui_settings_file().exists()
        && let Some(legacy) = legacy_ui_tables(paths)
    {
        text.push('\n');
        text.push_str(&toml::to_string_pretty(&legacy)?);
    }
    ferrite_launcher::core::write_atomic(&config_path(paths), text.as_bytes())?;
    Ok(())
}

/// Returns the legacy UI tables currently stored in `config.toml`, if any.
///
/// Unreadable or unparseable files yield `None`; in that case there is nothing that
/// can be carried forward safely.
fn legacy_ui_tables(paths: &AppPaths) -> Option<toml::Table> {
    let text = fs::read_to_string(config_path(paths)).ok()?;
    let table: toml::Table = toml::from_str(&text).ok()?;
    let legacy: toml::Table = table
        .into_iter()
        .filter(|(key, _)| LEGACY_UI_TABLES.contains(&key.as_str()))
        .collect();
    (!legacy.is_empty()).then_some(legacy)
}

/// Opens the directory containing Ferrite's configuration file.
///
/// Creates the directory first, then spawns the platform file browser (`explorer`,
/// `open`, or `xdg-open`). Success means the process was launched; it does not wait
/// for the browser or prove that a window became visible.
pub fn open_config_folder(paths: &AppPaths) -> Result<(), ConfigError> {
    let folder = paths.config_dir();
    fs::create_dir_all(folder)?;
    open_folder(folder)
}

/// Opens `folder` in the platform file manager (Explorer, Finder, or `xdg-open`).
///
/// The path is passed as a single argument, never through a shell, so spaces and
/// special characters are safe. The folder is not created.
pub fn open_folder(folder: &Path) -> Result<(), ConfigError> {
    #[cfg(target_os = "windows")]
    let program = "explorer";
    #[cfg(target_os = "macos")]
    let program = "open";
    #[cfg(all(unix, not(target_os = "macos")))]
    let program = "xdg-open";
    #[cfg(not(any(target_os = "windows", unix)))]
    return Err(ConfigError::OpenFolder(
        "opening folders is unsupported on this platform".to_owned(),
    ));

    Command::new(program)
        .arg(folder)
        .spawn()
        .map_err(|error| ConfigError::OpenFolder(format!("failed to launch {program}: {error}")))?;
    Ok(())
}

/// Returns the path to Ferrite's `config.toml` ([`AppPaths::config_file`]).
///
/// This is a pure path lookup: it neither creates the directory nor checks that the
/// file exists.
pub fn config_path(paths: &AppPaths) -> PathBuf {
    paths.config_file()
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferrite_launcher::core::paths::BaseDirs;

    /// Paths under a temporary root; tests never touch the real config directory.
    fn temp_paths(root: &std::path::Path) -> AppPaths {
        AppPaths::from_base_dirs(BaseDirs {
            config: root.join("config"),
            data_local: root.join("data"),
            cache: root.join("cache"),
        })
        .unwrap()
    }

    #[test]
    fn defaults_match_the_public_configuration_contract() {
        let config = Config::default();
        assert!(!config.launcher.close_on_launch);
        assert!(!config.launcher.show_snapshots);
        assert!(config.launcher.check_for_updates);
        assert!(config.discord.rich_presence);
        assert_eq!(config.minecraft.default_memory_mb, 4096);
    }

    #[test]
    fn missing_fields_receive_defaults() {
        let config = parse_toml("[discord]\nrich_presence = false\n").unwrap();
        assert!(!config.discord.rich_presence);
        assert_eq!(config.launcher, LauncherConfig::default());
        assert_eq!(config.minecraft, MinecraftConfig::default());
    }

    #[test]
    fn invalid_toml_does_not_produce_a_config() {
        assert!(matches!(
            parse_toml("[appearance\ntheme = 42"),
            Err(ConfigError::Deserialize(_))
        ));
    }

    #[test]
    fn raw_toml_round_trips() {
        let input = "[launcher]\nclose_on_launch = true\n\n[minecraft]\ndefault_memory_mb = 6144\n";
        let config = parse_toml(input).unwrap();
        let encoded = to_toml(&config).unwrap();
        let decoded = parse_toml(&encoded).unwrap();

        assert_eq!(decoded, config);
        assert!(decoded.launcher.close_on_launch);
        assert_eq!(decoded.minecraft.default_memory_mb, 6144);
    }

    #[test]
    fn default_config_round_trips_through_toml() {
        let encoded = to_toml(&Config::default()).unwrap();
        let decoded = parse_toml(&encoded).unwrap();
        assert_eq!(decoded, Config::default());
    }

    #[test]
    fn semantic_validation_rejects_unsupported_values() {
        assert!(matches!(
            parse_toml("[minecraft]\ndefault_memory_mb = 64\n"),
            Err(ConfigError::Validation(_))
        ));
    }

    #[test]
    fn core_config_ignores_and_never_serializes_ui_tables() {
        // Legacy files still parse; their UI tables are not part of the core model.
        let config =
            parse_toml("[appearance]\ntheme = 'neon'\n\n[layout]\nenabled = true\n").unwrap();
        assert_eq!(config, Config::default());
        let encoded = to_toml(&config).unwrap();
        assert!(!encoded.contains("appearance"));
        assert!(!encoded.contains("layout"));
    }

    #[test]
    fn save_carries_legacy_ui_tables_forward_until_ui_toml_exists() {
        let root = tempfile::tempdir().unwrap();
        let paths = temp_paths(root.path());
        fs::create_dir_all(paths.config_dir()).unwrap();
        fs::write(
            paths.config_file(),
            "[appearance]\ntheme = 'light'\nfont_scale = 1.25\n\n[launcher]\nclose_on_launch = false\n",
        )
        .unwrap();

        let mut config = load(&paths).unwrap();
        config.launcher.close_on_launch = true;
        save(&paths, &config).unwrap();

        let text = fs::read_to_string(paths.config_file()).unwrap();
        let table: toml::Table = toml::from_str(&text).unwrap();
        assert_eq!(table["appearance"]["theme"].as_str(), Some("light"));
        assert_eq!(table["appearance"]["font_scale"].as_float(), Some(1.25));
        assert!(load(&paths).unwrap().launcher.close_on_launch);

        // Once ui.toml exists the legacy tables are dropped on the next save.
        fs::write(paths.ui_settings_file(), "").unwrap();
        save(&paths, &config).unwrap();
        let text = fs::read_to_string(paths.config_file()).unwrap();
        assert!(!text.contains("appearance"));
        assert!(load(&paths).unwrap().launcher.close_on_launch);
    }
}
