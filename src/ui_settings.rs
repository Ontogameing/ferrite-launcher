//! Frontend-owned appearance and layout preferences, persisted in `ui.toml`.
//!
//! These settings describe how the egui frontend looks and is arranged; they are not
//! part of the core launcher configuration (`config.toml`) or the instance manifest.
//! [`AppPaths::ui_settings_file`] places `ui.toml` next to `config.toml` in the
//! per-user configuration directory.
//!
//! # Migration from `config.toml`
//!
//! Builds before Stage 1 stored the same `[appearance]` and `[layout]` tables inside
//! `config.toml`. [`load_or_migrate`] copies them into `ui.toml` the first time it
//! runs (validating them first) and leaves `config.toml` untouched; the legacy tables
//! stay readable there until the next core-config save after `ui.toml` exists (see
//! [`crate::config::save`]). Because the table names are unchanged, `ui.toml` is a
//! drop-in subset of the old file.

use crate::config::{ConfigError, LEGACY_UI_TABLES};
use ferrite_launcher::core::paths::AppPaths;
use serde::{Deserialize, Serialize};
use std::fs;
use std::io;
use std::path::PathBuf;

/// Complete frontend settings stored in `ui.toml`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(default)]
pub struct UiSettings {
    /// Folder of the last successful export; the Save-As dialog starts there.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_export_dir: Option<PathBuf>,
    pub appearance: AppearanceConfig,
    /// Opt-in per-page widget grid. Disabled by default to preserve the current shell.
    pub layout: LayoutConfig,
}

impl UiSettings {
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
        Ok(())
    }
}

/// Parses and validates `ui.toml` text, applying defaults for missing fields.
pub fn parse_toml(input: &str) -> Result<UiSettings, ConfigError> {
    let settings: UiSettings = toml::from_str(input)?;
    settings.validate()?;
    Ok(settings)
}

/// Serializes UI settings as human-readable TOML without writing them.
pub fn to_toml(settings: &UiSettings) -> Result<String, ConfigError> {
    Ok(toml::to_string_pretty(settings)?)
}

/// Strictly reads and validates `ui.toml`.
pub fn load(paths: &AppPaths) -> Result<UiSettings, ConfigError> {
    parse_toml(&fs::read_to_string(paths.ui_settings_file())?)
}

/// Atomically writes `ui.toml` (unique sibling temp file, fsync, rename).
pub fn save(paths: &AppPaths, settings: &UiSettings) -> Result<(), ConfigError> {
    let text = to_toml(settings)?;
    ferrite_launcher::core::write_atomic(&paths.ui_settings_file(), text.as_bytes())?;
    Ok(())
}

/// Outcome of startup loading.
pub struct UiSettingsLoad {
    /// Settings to apply for this process.
    pub settings: UiSettings,
    /// User-facing explanation of a recoverable fallback, if one occurred.
    pub warning: Option<String>,
    /// `true` when legacy values were copied from `config.toml` into `ui.toml`.
    pub migrated_from_legacy: bool,
}

/// Loads `ui.toml`, migrating legacy values from `config.toml` the first time.
///
/// - `ui.toml` exists: it is authoritative. If it is invalid, defaults are used with a
///   warning and the file is left untouched until the user saves a setting.
/// - `ui.toml` is missing: legacy `[appearance]`/`[layout]` tables are read from
///   `config.toml`, validated, and written to `ui.toml`. Invalid legacy values produce
///   defaults plus a warning and nothing is written, so the legacy tables remain the
///   only copy and are preserved by later core-config saves.
/// - Neither exists: defaults are written to `ui.toml`.
///
/// This never fails: every error becomes a warning so the UI can still start.
pub fn load_or_migrate(paths: &AppPaths) -> UiSettingsLoad {
    let ui_path = paths.ui_settings_file();
    match fs::read_to_string(&ui_path) {
        Ok(text) => {
            return match parse_toml(&text) {
                Ok(settings) => UiSettingsLoad {
                    settings,
                    warning: None,
                    migrated_from_legacy: false,
                },
                Err(error) => fallback(format!(
                    "Could not read {}: {error}. Using default appearance; changing a setting will replace the invalid file.",
                    ui_path.display()
                )),
            };
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => {
            return fallback(format!(
                "Could not read {}: {error}. Using default appearance.",
                ui_path.display()
            ));
        }
    }

    let (settings, migrated) = match legacy_settings(paths) {
        Ok(Some(settings)) => (settings, true),
        Ok(None) => (UiSettings::default(), false),
        Err(error) => {
            return fallback(format!(
                "Could not migrate appearance settings from {}: {error}. Using defaults; the old values are kept in that file.",
                paths.config_file().display()
            ));
        }
    };
    let warning = save(paths, &settings).err().map(|error| {
        format!(
            "Could not write {}: {error}. Settings are applied for this session only.",
            ui_path.display()
        )
    });
    UiSettingsLoad {
        settings,
        warning,
        migrated_from_legacy: migrated,
    }
}

fn fallback(warning: String) -> UiSettingsLoad {
    UiSettingsLoad {
        settings: UiSettings::default(),
        warning: Some(warning),
        migrated_from_legacy: false,
    }
}

/// Extracts and validates legacy UI tables from `config.toml`.
///
/// Returns `Ok(None)` when `config.toml` is missing or contains no UI tables.
fn legacy_settings(paths: &AppPaths) -> Result<Option<UiSettings>, ConfigError> {
    let text = match fs::read_to_string(paths.config_file()) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let table: toml::Table = toml::from_str(&text)?;
    let legacy: toml::Table = table
        .into_iter()
        .filter(|(key, _)| LEGACY_UI_TABLES.contains(&key.as_str()))
        .collect();
    if legacy.is_empty() {
        return Ok(None);
    }
    let settings: UiSettings = toml::Value::Table(legacy).try_into()?;
    settings.validate()?;
    Ok(Some(settings))
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
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
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
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
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
#[cfg(test)]
mod tests {
    use super::*;
    use ferrite_launcher::core::paths::BaseDirs;
    use std::path::Path;

    /// Paths under a temporary root; tests never touch the real config directory.
    fn temp_paths(root: &Path) -> AppPaths {
        AppPaths::from_base_dirs(BaseDirs {
            config: root.join("config"),
            data_local: root.join("data"),
            cache: root.join("cache"),
        })
        .unwrap()
    }

    #[test]
    fn last_export_dir_round_trips_and_is_optional() {
        let mut settings = UiSettings::default();
        assert!(!to_toml(&settings).unwrap().contains("last_export_dir"));
        settings.last_export_dir = Some(PathBuf::from("/home/me/Packs"));
        let text = to_toml(&settings).unwrap();
        assert_eq!(parse_toml(&text).unwrap(), settings);
        assert_eq!(
            parse_toml("").unwrap().last_export_dir,
            None,
            "older ui.toml files load unchanged"
        );
    }

    const LEGACY_CONFIG: &str = "[appearance]\ntheme = 'light'\naccent = '#123abc'\nfont_scale = 1.25\ncorner_radius = 4\n\n[appearance.theme_config]\npreset = 'custom'\n\n[appearance.theme_config.custom_palette]\nbackground = '#010203'\n\n[appearance.background]\nuse_per_page = true\n\n[appearance.background.play]\nopacity = 0.4\n\n[appearance.background.play.source]\nkind = 'local_file'\npath = '/pictures/play.png'\n\n[layout]\nenabled = true\n\n[layout.grid]\ngap = 20\n\n[launcher]\nclose_on_launch = true\n\n[minecraft]\ndefault_memory_mb = 6144\n";

    fn write_legacy(paths: &AppPaths) {
        fs::create_dir_all(paths.config_dir()).unwrap();
        fs::write(paths.config_file(), LEGACY_CONFIG).unwrap();
    }

    fn assert_legacy_values(settings: &UiSettings) {
        assert_eq!(settings.appearance.theme, "light");
        assert_eq!(settings.appearance.accent, "#123abc");
        assert_eq!(settings.appearance.font_scale, 1.25);
        assert_eq!(settings.appearance.corner_radius, 4);
        assert_eq!(settings.appearance.theme_config.preset, ThemePreset::Custom);
        assert_eq!(
            settings.appearance.theme_config.custom_palette.background,
            "#010203"
        );
        assert!(settings.appearance.background.use_per_page);
        assert_eq!(settings.appearance.background.play.opacity, 0.4);
        assert_eq!(
            settings.appearance.background.play.source,
            BackgroundSource::LocalFile {
                path: PathBuf::from("/pictures/play.png")
            }
        );
        assert!(settings.layout.enabled);
        assert_eq!(settings.layout.grid.gap, 20);
    }

    #[test]
    fn defaults_match_the_public_appearance_contract() {
        let settings = UiSettings::default();
        assert_eq!(settings.appearance.theme, "dark");
        assert_eq!(settings.appearance.accent, "#ff6600");
        assert_eq!(settings.appearance.font_scale, 1.0);
        assert_eq!(settings.appearance.corner_radius, 8);
        assert_eq!(settings.appearance.theme_config, ThemeConfig::default());
        assert!(!settings.appearance.background.use_per_page);
        assert_eq!(
            settings.appearance.background.global.source,
            BackgroundSource::None
        );
        assert_eq!(
            settings.appearance.background.global.fit,
            BackgroundFit::Cover
        );
        assert_eq!(settings.appearance.background.global.opacity, 1.0);
        assert_eq!(settings.appearance.background.global.overlay, "#000000");
        assert!(!settings.layout.enabled);
        assert_eq!(settings.layout.grid, GridConfig::default());
        assert_eq!(settings.layout.play.placements.len(), 3);
        assert_eq!(settings.layout.play.placements[0].widget, Widget::TopBar);
        assert_eq!(settings.layout.play.placements[1].widget, Widget::PageBody);
        assert_eq!(settings.layout.play.placements[2].widget, Widget::StatusBar);
        assert_eq!(settings.layout.instances, PageLayout::default());
        assert_eq!(parse_toml("").unwrap(), UiSettings::default());
    }

    #[test]
    fn default_settings_round_trip_through_toml() {
        let encoded = to_toml(&UiSettings::default()).unwrap();
        assert_eq!(parse_toml(&encoded).unwrap(), UiSettings::default());
    }

    #[test]
    fn semantic_validation_rejects_unsupported_values() {
        for invalid in [
            "[appearance]\ntheme = 'neon'\n",
            "[appearance]\naccent = 'orange'\n",
            "[appearance]\nfont_scale = 9.0\n",
            "[appearance]\ncorner_radius = 33\n",
        ] {
            assert!(matches!(
                parse_toml(invalid),
                Err(ConfigError::Validation(_))
            ));
        }
    }

    #[test]
    fn migration_preserves_legacy_values_and_writes_ui_toml() {
        let root = tempfile::tempdir().unwrap();
        let paths = temp_paths(root.path());
        write_legacy(&paths);

        let loaded = load_or_migrate(&paths);
        assert!(loaded.warning.is_none(), "{:?}", loaded.warning);
        assert!(loaded.migrated_from_legacy);
        assert_legacy_values(&loaded.settings);

        // ui.toml now holds the same values.
        let on_disk = load(&paths).unwrap();
        assert_eq!(on_disk, loaded.settings);
        let ui_text = fs::read_to_string(paths.ui_settings_file()).unwrap();
        assert!(!ui_text.contains("launcher"));
        assert!(!ui_text.contains("default_memory_mb"));

        // The legacy file is left exactly as it was (old keys still readable).
        assert_eq!(
            fs::read_to_string(paths.config_file()).unwrap(),
            LEGACY_CONFIG
        );
        // Core values are still read from config.toml.
        let core = crate::config::load(&paths).unwrap();
        assert!(core.launcher.close_on_launch);
        assert_eq!(core.minecraft.default_memory_mb, 6144);
    }

    #[test]
    fn second_load_uses_ui_toml_and_does_not_remigrate() {
        let root = tempfile::tempdir().unwrap();
        let paths = temp_paths(root.path());
        write_legacy(&paths);
        assert!(load_or_migrate(&paths).migrated_from_legacy);

        let mut changed = load(&paths).unwrap();
        changed.appearance.theme = "dark".to_owned();
        save(&paths, &changed).unwrap();

        let again = load_or_migrate(&paths);
        assert!(!again.migrated_from_legacy);
        assert_eq!(again.settings.appearance.theme, "dark");
    }

    #[test]
    fn legacy_keys_removed_from_config_only_after_a_later_core_save() {
        let root = tempfile::tempdir().unwrap();
        let paths = temp_paths(root.path());
        write_legacy(&paths);
        load_or_migrate(&paths);

        let core = crate::config::load(&paths).unwrap();
        crate::config::save(&paths, &core).unwrap();
        let text = fs::read_to_string(paths.config_file()).unwrap();
        assert!(!text.contains("[appearance"));
        assert!(!text.contains("[layout"));
        assert_eq!(crate::config::load(&paths).unwrap(), core);
        assert_legacy_values(&load(&paths).unwrap());
    }

    #[test]
    fn invalid_legacy_values_fall_back_without_writing_or_losing_them() {
        let root = tempfile::tempdir().unwrap();
        let paths = temp_paths(root.path());
        fs::create_dir_all(paths.config_dir()).unwrap();
        let legacy = "[appearance]\ntheme = 'neon'\nfont_scale = 1.5\n";
        fs::write(paths.config_file(), legacy).unwrap();

        let loaded = load_or_migrate(&paths);
        assert!(loaded.warning.is_some());
        assert_eq!(loaded.settings, UiSettings::default());
        assert!(!paths.ui_settings_file().exists());

        // A core save keeps the unmigrated values.
        crate::config::save(&paths, &crate::config::Config::default()).unwrap();
        let text = fs::read_to_string(paths.config_file()).unwrap();
        assert!(text.contains("neon"));
    }

    #[test]
    fn invalid_ui_toml_is_not_overwritten() {
        let root = tempfile::tempdir().unwrap();
        let paths = temp_paths(root.path());
        fs::create_dir_all(paths.config_dir()).unwrap();
        fs::write(paths.ui_settings_file(), "[appearance\n").unwrap();

        let loaded = load_or_migrate(&paths);
        assert!(loaded.warning.is_some());
        assert_eq!(loaded.settings, UiSettings::default());
        assert_eq!(
            fs::read_to_string(paths.ui_settings_file()).unwrap(),
            "[appearance\n"
        );
    }

    #[test]
    fn fresh_install_writes_default_ui_toml() {
        let root = tempfile::tempdir().unwrap();
        let paths = temp_paths(root.path());
        let loaded = load_or_migrate(&paths);
        assert!(loaded.warning.is_none());
        assert!(!loaded.migrated_from_legacy);
        assert_eq!(load(&paths).unwrap(), UiSettings::default());
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
    fn background_models_round_trip_through_toml() {
        let mut config = UiSettings::default();
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
    fn theme_and_layout_models_round_trip_through_toml() {
        let mut config = UiSettings::default();
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
        let mut config = UiSettings::default();
        config.layout.play.placements[0].column = 0;
        assert!(matches!(config.validate(), Err(ConfigError::Validation(_))));

        let mut config = UiSettings::default();
        config.layout.play.placements[0].width = 13;
        assert!(matches!(config.validate(), Err(ConfigError::Validation(_))));

        let mut config = UiSettings::default();
        config.layout.play.placements[0].row = MAX_LAYOUT_ROWS;
        config.layout.play.placements[0].height = 2;
        assert!(matches!(config.validate(), Err(ConfigError::Validation(_))));

        let mut config = UiSettings::default();
        config.layout.grid.columns = 0;
        assert!(matches!(config.validate(), Err(ConfigError::Validation(_))));

        let mut config = UiSettings::default();
        config.layout.grid.row_height = 23;
        assert!(matches!(config.validate(), Err(ConfigError::Validation(_))));

        let mut config = UiSettings::default();
        config.layout.grid.gap = 129;
        assert!(matches!(config.validate(), Err(ConfigError::Validation(_))));

        let mut config = UiSettings::default();
        config.layout.play.placements = vec![placement(
            Widget::Text {
                text: " ".to_owned(),
            },
            1,
        )];
        assert!(matches!(config.validate(), Err(ConfigError::Validation(_))));

        let mut config = UiSettings::default();
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
        let mut config = UiSettings::default();
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
