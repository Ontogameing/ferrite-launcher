//! Persistent, non-secret launcher preferences.
//!
//! [`config_path`] is [`AppPaths::config_file`]: `config.toml` in the platform-specific
//! per-user configuration directory resolved once at startup. The entire [`Config`]
//! is serialized as human-readable TOML; missing tables and fields inherit defaults
//! through Serde's `default` handling, while known values are semantically validated.
//!
//! [`load`] is strict. Startup code can instead use [`load_or_create`], which creates
//! a default file when none exists and recovers from syntactically malformed or
//! type-invalid TOML by returning defaults plus a warning. Files that parse but fail
//! semantic validation and filesystem failures are still returned as errors so callers can decide whether an in-memory fallback is safe.
//! Locating the directory itself can no longer fail here; that happens once when
//! [`AppPaths`] is resolved.
//!
//! This module stores preferences only. Credentials and access tokens do not belong in
//! [`Config`] or in raw TOML supplied to [`save_toml`].

use ferrite_launcher::core::paths::AppPaths;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::fs;
use std::io;
use std::path::PathBuf;
use std::process::Command;

/// Complete on-disk configuration.
///
/// Deserializing a partial file fills absent sections and fields from [`Default`],
/// which allows newer versions to add settings without requiring a migration.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Config {
    pub appearance: AppearanceConfig,
    /// Opt-in per-page widget grid. Disabled by default to preserve the current shell.
    pub layout: LayoutConfig,
    pub launcher: LauncherConfig,
    pub discord: DiscordConfig,
    pub minecraft: MinecraftConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            appearance: AppearanceConfig::default(),
            layout: LayoutConfig::default(),
            launcher: LauncherConfig::default(),
            discord: DiscordConfig::default(),
            minecraft: MinecraftConfig::default(),
        }
    }
}

impl Config {
    fn validate(&self) -> Result<(), ConfigError> {
        if !matches!(self.appearance.theme.as_str(), "dark" | "light") {
            return Err(ConfigError::Validation(
                "appearance.theme must be 'dark' or 'light'".to_owned(),
            ));
        }
        let accent = self.appearance.accent.as_bytes();
        if accent.len() != 7 || accent[0] != b'#' || !accent[1..].iter().all(u8::is_ascii_hexdigit)
        {
            return Err(ConfigError::Validation(
                "appearance.accent must be a color in #RRGGBB format".to_owned(),
            ));
        }
        if !(0.5..=2.0).contains(&self.appearance.font_scale) {
            return Err(ConfigError::Validation(
                "appearance.font_scale must be between 0.5 and 2.0".to_owned(),
            ));
        }
        if self.appearance.corner_radius > 32 {
            return Err(ConfigError::Validation(
                "appearance.corner_radius must be between 0 and 32".to_owned(),
            ));
        }
        self.appearance.theme_config.validate()?;
        self.appearance.background.validate()?;
        self.layout.validate()?;
        if !(512..=32_768).contains(&self.minecraft.default_memory_mb) {
            return Err(ConfigError::Validation(
                "minecraft.default_memory_mb must be between 512 and 32768".to_owned(),
            ));
        }
        Ok(())
    }
}

/// Visual preferences applied by the launcher UI.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct AppearanceConfig {
    /// Legacy dark/light selector retained for existing UI and TOML compatibility.
    pub theme: String,
    /// Legacy accent retained for existing UI and TOML compatibility.
    pub accent: String,
    pub font_scale: f32,
    pub corner_radius: u8,
    /// Preset/custom palette selection for theme-aware UI modules.
    pub theme_config: ThemeConfig,
    /// Persisted image-background preferences.
    pub background: BackgroundConfig,
}

impl Default for AppearanceConfig {
    fn default() -> Self {
        Self {
            theme: "dark".to_owned(),
            accent: "#ff6600".to_owned(),
            font_scale: 1.0,
            corner_radius: 8,
            theme_config: ThemeConfig::default(),
            background: BackgroundConfig::default(),
        }
    }
}

/// Theme selection and the palette used when [`ThemePreset::Custom`] is selected.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct ThemeConfig {
    pub preset: ThemePreset,
    pub custom_palette: ThemePalette,
}

impl Default for ThemeConfig {
    fn default() -> Self {
        Self {
            preset: ThemePreset::Legacy,
            custom_palette: ThemePalette::default(),
        }
    }
}

impl ThemeConfig {
    fn validate(&self) -> Result<(), ConfigError> {
        self.custom_palette
            .validate("appearance.theme_config.custom_palette")
    }
}

/// Built-in theme or the persisted custom palette.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum ThemePreset {
    /// Honors the legacy `appearance.theme` and `appearance.accent` fields.
    #[default]
    Legacy,
    /// Ferrite's dark appearance.
    Dark,
    Light,
    Custom,
}

/// Colors available to a theme-aware launcher UI.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct ThemePalette {
    pub background: String,
    pub sidebar: String,
    pub card: String,
    pub accent: String,
    pub text: String,
    pub muted: String,
}

impl Default for ThemePalette {
    fn default() -> Self {
        Self {
            background: "#121418".to_owned(),
            sidebar: "#191C22".to_owned(),
            card: "#1F232A".to_owned(),
            accent: "#FF6600".to_owned(),
            text: "#FFFFFF".to_owned(),
            muted: "#969BA5".to_owned(),
        }
    }
}

impl ThemePalette {
    fn validate(&self, path: &str) -> Result<(), ConfigError> {
        for (name, color) in [
            ("background", &self.background),
            ("sidebar", &self.sidebar),
            ("card", &self.card),
            ("accent", &self.accent),
            ("text", &self.text),
            ("muted", &self.muted),
        ] {
            if !is_rgb_color(color) {
                return Err(ConfigError::Validation(format!(
                    "{path}.{name} must be a color in #RRGGBB format"
                )));
            }
        }
        Ok(())
    }
}

/// Selects whether one background is shared or each launcher page has its own.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct BackgroundConfig {
    /// When `false`, only [`Self::global`] is selected; when `true`, the
    /// configuration matching the current page is selected.
    pub use_per_page: bool,
    /// Background shared by all pages when [`Self::use_per_page`] is `false`.
    pub global: BackgroundSettings,
    /// Background for the Play page.
    pub play: BackgroundSettings,
    /// Background for the Instances page.
    pub instances: BackgroundSettings,
    /// Background for the Mods page.
    pub mods: BackgroundSettings,
    /// Background for the Settings page.
    pub settings: BackgroundSettings,
}

impl Default for BackgroundConfig {
    fn default() -> Self {
        Self {
            use_per_page: false,
            global: BackgroundSettings::default(),
            play: BackgroundSettings::default(),
            instances: BackgroundSettings::default(),
            mods: BackgroundSettings::default(),
            settings: BackgroundSettings::default(),
        }
    }
}

impl BackgroundConfig {
    fn validate(&self) -> Result<(), ConfigError> {
        for (name, settings) in [
            ("global", &self.global),
            ("play", &self.play),
            ("instances", &self.instances),
            ("mods", &self.mods),
            ("settings", &self.settings),
        ] {
            settings.validate(&format!("appearance.background.{name}"))?;
        }
        Ok(())
    }
}

/// Complete visual treatment for one launcher background.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct BackgroundSettings {
    /// Image source, or [`BackgroundSource::None`] for the existing solid background.
    pub source: BackgroundSource,
    /// How the image is sized within the page.
    pub fit: BackgroundFit,
    /// Horizontal placement used when the fitted image does not fill the page.
    pub horizontal_alignment: HorizontalAlignment,
    /// Vertical placement used when the fitted image does not fill the page.
    pub vertical_alignment: VerticalAlignment,
    /// Image opacity in the inclusive range `0.0..=1.0`.
    pub opacity: f32,
    /// Blur radius in pixels in the inclusive range `0.0..=100.0`.
    pub blur: f32,
    /// Overlay color in strict `#RRGGBB` form.
    pub overlay: String,
    /// Overlay opacity in the inclusive range `0.0..=1.0`.
    pub overlay_opacity: f32,
    /// Persisted generation used to retry a source or choose another folder image.
    pub reload_nonce: u64,
}

impl Default for BackgroundSettings {
    fn default() -> Self {
        Self {
            source: BackgroundSource::None,
            fit: BackgroundFit::Cover,
            horizontal_alignment: HorizontalAlignment::Center,
            vertical_alignment: VerticalAlignment::Center,
            opacity: 1.0,
            blur: 0.0,
            overlay: "#000000".to_owned(),
            overlay_opacity: 0.0,
            reload_nonce: 0,
        }
    }
}

impl BackgroundSettings {
    fn validate(&self, path: &str) -> Result<(), ConfigError> {
        if !(0.0..=1.0).contains(&self.opacity) {
            return Err(ConfigError::Validation(format!(
                "{path}.opacity must be between 0 and 1"
            )));
        }
        if !(0.0..=100.0).contains(&self.blur) {
            return Err(ConfigError::Validation(format!(
                "{path}.blur must be between 0 and 100"
            )));
        }
        if !is_rgb_color(&self.overlay) {
            return Err(ConfigError::Validation(format!(
                "{path}.overlay must be a color in #RRGGBB format"
            )));
        }
        if !(0.0..=1.0).contains(&self.overlay_opacity) {
            return Err(ConfigError::Validation(format!(
                "{path}.overlay_opacity must be between 0 and 1"
            )));
        }
        Ok(())
    }
}

/// Persisted source for a background image.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BackgroundSource {
    /// Do not draw an image, leaving the launcher's existing solid background.
    #[default]
    None,
    /// Load one image from a local filesystem path.
    LocalFile { path: PathBuf },
    /// Download one image from an HTTPS URL.
    HttpsUrl { url: String },
    /// Choose an image from a local folder.
    RandomFolder { path: PathBuf },
}

/// Determines how a background image fills its available area.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(rename_all = "snake_case")]
pub enum BackgroundFit {
    /// Fill the area while preserving aspect ratio, cropping as needed.
    #[default]
    Cover,
    /// Show the entire image while preserving aspect ratio.
    Contain,
    /// Fill both dimensions without preserving aspect ratio.
    Stretch,
    /// Repeat the image at its natural size.
    Tile,
}

/// Horizontal placement of a fitted background image.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(rename_all = "snake_case")]
pub enum HorizontalAlignment {
    /// Align the image to the left edge.
    Left,
    /// Center the image horizontally.
    #[default]
    Center,
    /// Align the image to the right edge.
    Right,
}

/// Vertical placement of a fitted background image.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(rename_all = "snake_case")]
pub enum VerticalAlignment {
    /// Align the image to the top edge.
    Top,
    /// Center the image vertically.
    #[default]
    Center,
    /// Align the image to the bottom edge.
    Bottom,
}

fn is_rgb_color(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() == 7 && bytes[0] == b'#' && bytes[1..].iter().all(u8::is_ascii_hexdigit)
}

/// Maximum number of rows addressable by a custom page layout.
pub const MAX_LAYOUT_ROWS: u16 = 1_000;
/// Maximum number of widget placements accepted on one page.
pub const MAX_WIDGET_PLACEMENTS: usize = 256;
/// Maximum custom text length, measured in Unicode scalar values.
pub const MAX_WIDGET_TEXT_LENGTH: usize = 2_048;
/// Maximum action-button label length, measured in Unicode scalar values.
pub const MAX_WIDGET_LABEL_LENGTH: usize = 80;

/// Opt-in grid and ordered widget placements for each top-level launcher page.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct LayoutConfig {
    /// When `false`, consumers must render the existing built-in shell.
    pub enabled: bool,
    pub grid: GridConfig,
    pub play: PageLayout,
    pub instances: PageLayout,
    pub mods: PageLayout,
    pub settings: PageLayout,
}

impl Default for LayoutConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            grid: GridConfig::default(),
            play: PageLayout::default(),
            instances: PageLayout::default(),
            mods: PageLayout::default(),
            settings: PageLayout::default(),
        }
    }
}

impl LayoutConfig {
    fn validate(&self) -> Result<(), ConfigError> {
        self.grid.validate()?;
        for (name, page) in [
            ("play", &self.play),
            ("instances", &self.instances),
            ("mods", &self.mods),
            ("settings", &self.settings),
        ] {
            page.validate(&format!("layout.{name}"), &self.grid)?;
        }
        Ok(())
    }
}

/// Shared dimensions for all custom page grids.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct GridConfig {
    /// Number of columns in the inclusive range `1..=12`.
    pub columns: u8,
    /// Height of one row in logical pixels, in the range `24..=512`.
    pub row_height: u16,
    /// Gap between cells in logical pixels, in the range `0..=128`.
    pub gap: u16,
}

impl Default for GridConfig {
    fn default() -> Self {
        Self {
            columns: 12,
            row_height: 64,
            gap: 12,
        }
    }
}

impl GridConfig {
    fn validate(&self) -> Result<(), ConfigError> {
        if !(1..=12).contains(&self.columns) {
            return Err(ConfigError::Validation(
                "layout.grid.columns must be between 1 and 12".to_owned(),
            ));
        }
        if !(24..=512).contains(&self.row_height) {
            return Err(ConfigError::Validation(
                "layout.grid.row_height must be between 24 and 512".to_owned(),
            ));
        }
        if self.gap > 128 {
            return Err(ConfigError::Validation(
                "layout.grid.gap must be between 0 and 128".to_owned(),
            ));
        }
        Ok(())
    }
}

/// Ordered widget placements for one page.
///
/// Placements may overlap intentionally; later entries paint above earlier entries.
/// Enabled and disabled placements must still remain within the configured grid bounds.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct PageLayout {
    pub placements: Vec<WidgetPlacement>,
}

impl Default for PageLayout {
    fn default() -> Self {
        Self {
            placements: vec![
                WidgetPlacement {
                    widget: Widget::TopBar,
                    column: 1,
                    row: 1,
                    width: 12,
                    height: 1,
                    enabled: true,
                },
                WidgetPlacement {
                    widget: Widget::PageBody,
                    column: 1,
                    row: 2,
                    width: 12,
                    height: 10,
                    enabled: true,
                },
                WidgetPlacement {
                    widget: Widget::StatusBar,
                    column: 1,
                    row: 12,
                    width: 12,
                    height: 1,
                    enabled: true,
                },
            ],
        }
    }
}

impl PageLayout {
    fn validate(&self, path: &str, grid: &GridConfig) -> Result<(), ConfigError> {
        if self.placements.len() > MAX_WIDGET_PLACEMENTS {
            return Err(ConfigError::Validation(format!(
                "{path}.placements must contain at most {MAX_WIDGET_PLACEMENTS} widgets"
            )));
        }

        for (index, placement) in self.placements.iter().enumerate() {
            placement.validate(&format!("{path}.placements[{index}]"), grid)?;
        }
        Ok(())
    }
}

/// One widget's 1-based position and span in a page grid.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct WidgetPlacement {
    pub widget: Widget,
    pub column: u8,
    pub row: u16,
    pub width: u8,
    pub height: u16,
    pub enabled: bool,
}

impl Default for WidgetPlacement {
    fn default() -> Self {
        Self {
            widget: Widget::default(),
            column: 1,
            row: 1,
            width: 1,
            height: 1,
            enabled: true,
        }
    }
}

impl WidgetPlacement {
    fn validate(&self, path: &str, grid: &GridConfig) -> Result<(), ConfigError> {
        if self.column == 0 || self.row == 0 || self.width == 0 || self.height == 0 {
            return Err(ConfigError::Validation(format!(
                "{path} column, row, width, and height must be greater than zero"
            )));
        }
        let column_end = u16::from(self.column) + u16::from(self.width) - 1;
        if column_end > u16::from(grid.columns) {
            return Err(ConfigError::Validation(format!(
                "{path} extends beyond the {}-column grid",
                grid.columns
            )));
        }
        let row_end = u32::from(self.row) + u32::from(self.height) - 1;
        if row_end > u32::from(MAX_LAYOUT_ROWS) {
            return Err(ConfigError::Validation(format!(
                "{path} extends beyond maximum row {MAX_LAYOUT_ROWS}"
            )));
        }
        self.widget.validate(path)
    }
}

/// Content rendered by a custom layout placement.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Widget {
    TopBar,
    #[default]
    PageBody,
    StatusBar,
    SelectedInstance,
    AccountSummary,
    LauncherStatus,
    Text {
        text: String,
    },
    ActionButton {
        label: String,
        action: WidgetAction,
    },
}

impl Widget {
    fn validate(&self, path: &str) -> Result<(), ConfigError> {
        match self {
            Self::Text { text } => {
                validate_display_text(text, MAX_WIDGET_TEXT_LENGTH, &format!("{path}.widget.text"))
            }
            Self::ActionButton { label, .. } => validate_display_text(
                label,
                MAX_WIDGET_LABEL_LENGTH,
                &format!("{path}.widget.label"),
            ),
            _ => Ok(()),
        }
    }
}

fn validate_display_text(value: &str, maximum: usize, path: &str) -> Result<(), ConfigError> {
    let length = value.chars().count();
    if value.trim().is_empty() || length > maximum {
        return Err(ConfigError::Validation(format!(
            "{path} must contain between 1 and {maximum} characters"
        )));
    }
    Ok(())
}

/// Top-level page addressable by a navigation action.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum LayoutPage {
    #[default]
    Play,
    Instances,
    Mods,
    Settings,
}

/// Operation performed by a built-in action-button widget.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WidgetAction {
    #[default]
    Launch,
    Navigate {
        page: LayoutPage,
    },
    StopGame,
    CreateInstance,
    ImportPack,
    OpenAccount,
    Settings,
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
pub fn save(paths: &AppPaths, config: &Config) -> Result<(), ConfigError> {
    let text = to_toml(config)?;
    ferrite_launcher::core::write_atomic(&config_path(paths), text.as_bytes())?;
    Ok(())
}

/// Opens the directory containing Ferrite's configuration file.
///
/// Creates the directory first, then spawns the platform file browser (`explorer`,
/// `open`, or `xdg-open`). Success means the process was launched; it does not wait
/// for the browser or prove that a window became visible.
pub fn open_config_folder(paths: &AppPaths) -> Result<(), ConfigError> {
    let folder = paths.config_dir();
    fs::create_dir_all(folder)?;

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

    #[test]
    fn defaults_match_the_public_configuration_contract() {
        let config = Config::default();
        assert_eq!(config.appearance.theme, "dark");
        assert_eq!(config.appearance.accent, "#ff6600");
        assert_eq!(config.appearance.font_scale, 1.0);
        assert_eq!(config.appearance.corner_radius, 8);
        assert_eq!(config.appearance.theme_config, ThemeConfig::default());
        assert_eq!(config.appearance.background, BackgroundConfig::default());
        assert!(!config.appearance.background.use_per_page);
        assert_eq!(
            config.appearance.background.global.source,
            BackgroundSource::None
        );
        assert_eq!(
            config.appearance.background.global.fit,
            BackgroundFit::Cover
        );
        assert_eq!(config.appearance.background.global.opacity, 1.0);
        assert_eq!(config.appearance.background.global.blur, 0.0);
        assert_eq!(config.appearance.background.global.overlay, "#000000");
        assert_eq!(config.appearance.background.global.overlay_opacity, 0.0);
        assert!(!config.layout.enabled);
        assert_eq!(config.layout.grid, GridConfig::default());
        assert_eq!(config.layout.play.placements.len(), 3);
        assert_eq!(config.layout.play.placements[0].widget, Widget::TopBar);
        assert_eq!(config.layout.play.placements[1].widget, Widget::PageBody);
        assert_eq!(config.layout.play.placements[2].widget, Widget::StatusBar);
        assert_eq!(config.layout.instances, PageLayout::default());
        assert_eq!(config.layout.mods, PageLayout::default());
        assert_eq!(config.layout.settings, PageLayout::default());
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
        assert_eq!(config.appearance, AppearanceConfig::default());
        assert_eq!(config.layout, LayoutConfig::default());
        assert_eq!(config.launcher, LauncherConfig::default());
        assert_eq!(config.minecraft, MinecraftConfig::default());
    }

    #[test]
    fn partial_background_toml_receives_nested_defaults() {
        let config = parse_toml(
            "[appearance.background]\nuse_per_page = true\n\n[appearance.background.play]\nopacity = 0.4\n\n[appearance.background.play.source]\nkind = 'local_file'\npath = '/pictures/play.png'\n",
        )
        .unwrap();

        assert!(config.appearance.background.use_per_page);
        assert_eq!(
            config.appearance.background.global,
            BackgroundSettings::default()
        );
        assert_eq!(config.appearance.background.play.opacity, 0.4);
        assert_eq!(config.appearance.background.play.fit, BackgroundFit::Cover);
        assert_eq!(
            config.appearance.background.play.source,
            BackgroundSource::LocalFile {
                path: PathBuf::from("/pictures/play.png")
            }
        );
        assert_eq!(
            config.appearance.background.instances,
            BackgroundSettings::default()
        );
        assert_eq!(
            config.appearance.background.mods,
            BackgroundSettings::default()
        );
        assert_eq!(
            config.appearance.background.settings,
            BackgroundSettings::default()
        );
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
        let input = "[appearance]\ntheme = \"light\"\n\n[launcher]\nclose_on_launch = true\n";
        let config = parse_toml(input).unwrap();
        let encoded = to_toml(&config).unwrap();
        let decoded = parse_toml(&encoded).unwrap();

        assert_eq!(decoded, config);
        assert_eq!(decoded.appearance.theme, "light");
        assert!(decoded.launcher.close_on_launch);
    }

    #[test]
    fn default_config_round_trips_through_toml() {
        let encoded = to_toml(&Config::default()).unwrap();
        let decoded = parse_toml(&encoded).unwrap();
        assert_eq!(decoded, Config::default());
    }

    #[test]
    fn background_models_round_trip_through_toml() {
        let mut config = Config::default();
        config.appearance.background.use_per_page = true;
        config.appearance.background.global.source = BackgroundSource::LocalFile {
            path: PathBuf::from("/pictures/global.png"),
        };
        config.appearance.background.global.fit = BackgroundFit::Contain;
        config.appearance.background.global.horizontal_alignment = HorizontalAlignment::Left;
        config.appearance.background.global.vertical_alignment = VerticalAlignment::Top;
        config.appearance.background.play.source = BackgroundSource::HttpsUrl {
            url: "https://example.com/play.png".to_owned(),
        };
        config.appearance.background.play.fit = BackgroundFit::Stretch;
        config.appearance.background.play.horizontal_alignment = HorizontalAlignment::Right;
        config.appearance.background.play.vertical_alignment = VerticalAlignment::Bottom;
        config.appearance.background.instances.source = BackgroundSource::RandomFolder {
            path: PathBuf::from("/pictures/instances"),
        };
        config.appearance.background.instances.fit = BackgroundFit::Tile;
        config.appearance.background.mods.opacity = 0.25;
        config.appearance.background.mods.blur = 12.5;
        config.appearance.background.mods.overlay = "#A1b2C3".to_owned();
        config.appearance.background.mods.overlay_opacity = 0.75;

        let encoded = to_toml(&config).unwrap();
        let decoded = parse_toml(&encoded).unwrap();
        assert_eq!(decoded, config);
    }

    #[test]
    fn background_numeric_boundaries_are_valid() {
        let config = parse_toml(
            "[appearance.background.global]\nopacity = 0.0\nblur = 100.0\noverlay_opacity = 1.0\n",
        )
        .unwrap();

        assert_eq!(config.appearance.background.global.opacity, 0.0);
        assert_eq!(config.appearance.background.global.blur, 100.0);
        assert_eq!(config.appearance.background.global.overlay_opacity, 1.0);
    }

    #[test]
    fn every_background_configuration_is_validated() {
        for name in ["global", "play", "instances", "mods", "settings"] {
            let invalid = format!("[appearance.background.{name}]\nopacity = 1.01\n");
            assert!(matches!(
                parse_toml(&invalid),
                Err(ConfigError::Validation(message))
                    if message == format!("appearance.background.{name}.opacity must be between 0 and 1")
            ));
        }
    }

    #[test]
    fn background_validation_rejects_invalid_numeric_values_and_colors() {
        for invalid in [
            "[appearance.background.global]\nopacity = -0.01\n",
            "[appearance.background.global]\nopacity = 1.01\n",
            "[appearance.background.global]\nblur = -0.01\n",
            "[appearance.background.global]\nblur = 100.01\n",
            "[appearance.background.global]\noverlay_opacity = -0.01\n",
            "[appearance.background.global]\noverlay_opacity = 1.01\n",
            "[appearance.background.global]\noverlay = '112233'\n",
            "[appearance.background.global]\noverlay = '#12345g'\n",
            "[appearance.background.global]\noverlay = '#1234567'\n",
        ] {
            assert!(matches!(
                parse_toml(invalid),
                Err(ConfigError::Validation(_))
            ));
        }
    }

    #[test]
    fn semantic_validation_rejects_unsupported_values() {
        for invalid in [
            "[appearance]\ntheme = 'neon'\n",
            "[appearance]\naccent = 'orange'\n",
            "[appearance]\nfont_scale = 9.0\n",
            "[minecraft]\ndefault_memory_mb = 64\n",
        ] {
            assert!(matches!(
                parse_toml(invalid),
                Err(ConfigError::Validation(_))
            ));
        }
    }

    #[test]
    fn theme_and_layout_models_round_trip_through_toml() {
        let mut config = Config::default();
        config.appearance.theme_config = ThemeConfig {
            preset: ThemePreset::Custom,
            custom_palette: ThemePalette {
                background: "#010203".to_owned(),
                sidebar: "#111213".to_owned(),
                card: "#212223".to_owned(),
                accent: "#AABBCC".to_owned(),
                text: "#F0F1F2".to_owned(),
                muted: "#777879".to_owned(),
            },
        };
        config.layout.enabled = true;
        config.layout.play.placements = vec![
            placement(Widget::TopBar, 1),
            placement(Widget::PageBody, 2),
            placement(Widget::StatusBar, 3),
            placement(Widget::SelectedInstance, 4),
            placement(Widget::AccountSummary, 5),
            placement(Widget::LauncherStatus, 6),
            placement(
                Widget::Text {
                    text: "Welcome to Ferrite".to_owned(),
                },
                7,
            ),
            placement(
                Widget::ActionButton {
                    label: "Play".to_owned(),
                    action: WidgetAction::Launch,
                },
                8,
            ),
            placement(
                Widget::ActionButton {
                    label: "Play page".to_owned(),
                    action: WidgetAction::Navigate {
                        page: LayoutPage::Play,
                    },
                },
                9,
            ),
            placement(
                Widget::ActionButton {
                    label: "Instances page".to_owned(),
                    action: WidgetAction::Navigate {
                        page: LayoutPage::Instances,
                    },
                },
                10,
            ),
            placement(
                Widget::ActionButton {
                    label: "Mods page".to_owned(),
                    action: WidgetAction::Navigate {
                        page: LayoutPage::Mods,
                    },
                },
                11,
            ),
            placement(
                Widget::ActionButton {
                    label: "Settings page".to_owned(),
                    action: WidgetAction::Navigate {
                        page: LayoutPage::Settings,
                    },
                },
                12,
            ),
            placement(
                Widget::ActionButton {
                    label: "Stop".to_owned(),
                    action: WidgetAction::StopGame,
                },
                13,
            ),
            placement(
                Widget::ActionButton {
                    label: "Create".to_owned(),
                    action: WidgetAction::CreateInstance,
                },
                14,
            ),
            placement(
                Widget::ActionButton {
                    label: "Import".to_owned(),
                    action: WidgetAction::ImportPack,
                },
                15,
            ),
            placement(
                Widget::ActionButton {
                    label: "Account".to_owned(),
                    action: WidgetAction::OpenAccount,
                },
                16,
            ),
            placement(
                Widget::ActionButton {
                    label: "Settings".to_owned(),
                    action: WidgetAction::Settings,
                },
                17,
            ),
        ];

        let encoded = to_toml(&config).unwrap();
        let decoded = parse_toml(&encoded).unwrap();
        assert_eq!(decoded, config);
    }

    #[test]
    fn partial_theme_and_layout_toml_receives_nested_defaults() {
        let config =
            parse_toml("[appearance.theme_config]\npreset = 'light'\n\n[layout]\nenabled = true\n")
                .unwrap();

        assert_eq!(config.appearance.theme_config.preset, ThemePreset::Light);
        assert_eq!(
            config.appearance.theme_config.custom_palette,
            ThemePalette::default()
        );
        assert!(config.layout.enabled);
        assert_eq!(config.layout.grid, GridConfig::default());
        assert_eq!(config.layout.play, PageLayout::default());
    }

    #[test]
    fn theme_validation_rejects_non_strict_rgb_colors() {
        for color in ["112233", "#12345G", "#1234567"] {
            let input =
                format!("[appearance.theme_config.custom_palette]\nbackground = '{color}'\n");
            assert!(matches!(
                parse_toml(&input),
                Err(ConfigError::Validation(_))
            ));
        }
    }

    #[test]
    fn layout_validation_rejects_invalid_grid_placements_and_content() {
        let mut config = Config::default();
        config.layout.play.placements[0].column = 0;
        assert!(matches!(config.validate(), Err(ConfigError::Validation(_))));

        let mut config = Config::default();
        config.layout.play.placements[0].width = 13;
        assert!(matches!(config.validate(), Err(ConfigError::Validation(_))));

        let mut config = Config::default();
        config.layout.play.placements[0].row = MAX_LAYOUT_ROWS;
        config.layout.play.placements[0].height = 2;
        assert!(matches!(config.validate(), Err(ConfigError::Validation(_))));

        let mut config = Config::default();
        config.layout.grid.columns = 0;
        assert!(matches!(config.validate(), Err(ConfigError::Validation(_))));

        let mut config = Config::default();
        config.layout.grid.row_height = 23;
        assert!(matches!(config.validate(), Err(ConfigError::Validation(_))));

        let mut config = Config::default();
        config.layout.grid.gap = 129;
        assert!(matches!(config.validate(), Err(ConfigError::Validation(_))));

        let mut config = Config::default();
        config.layout.play.placements = vec![placement(
            Widget::Text {
                text: " ".to_owned(),
            },
            1,
        )];
        assert!(matches!(config.validate(), Err(ConfigError::Validation(_))));

        let mut config = Config::default();
        config.layout.play.placements = vec![placement(
            Widget::ActionButton {
                label: "x".repeat(MAX_WIDGET_LABEL_LENGTH + 1),
                action: WidgetAction::Launch,
            },
            1,
        )];
        assert!(matches!(config.validate(), Err(ConfigError::Validation(_))));
    }

    #[test]
    fn placements_may_overlap_but_still_require_valid_bounds() {
        let mut config = Config::default();
        config.layout.play.placements[1].row = 1;
        assert!(config.validate().is_ok());

        config.layout.play.placements[1].column = 0;
        assert!(matches!(config.validate(), Err(ConfigError::Validation(_))));
    }

    fn placement(widget: Widget, row: u16) -> WidgetPlacement {
        WidgetPlacement {
            widget,
            column: 1,
            row,
            width: 12,
            height: 1,
            enabled: true,
        }
    }
}
