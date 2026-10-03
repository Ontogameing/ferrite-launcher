//! Settings UI, live appearance helpers, update checks, and Discord synchronization.
//!
//! Normal controls mutate typed [`Config`] and save immediately; the advanced editor
//! deliberately keeps raw TOML separate until explicit apply. Network update checks use
//! a worker channel, while visual settings are read from central state every frame.

use super::{ACCENT, BACKGROUND, CARD, Ferrite, MUTED, Page, SIDEBAR, page_heading};
use crate::background::BackgroundStatus;
use crate::config::Config;
use crate::discord::DiscordPresence;
use crate::ui_settings::{
    BackgroundFit, BackgroundSettings, BackgroundSource, HorizontalAlignment, ThemePalette,
    ThemePreset, VerticalAlignment,
};
use crate::updates::UpdateCheck;
use eframe::egui::{self, Color32, RichText};
use std::path::PathBuf;
use std::sync::mpsc::{self, TryRecvError};

impl Ferrite {
    /// Starts a single blocking GitHub release check and records whether it was manual.
    pub(super) fn start_update_check(&mut self, manual: bool) {
        if self.update_task.is_some() {
            if manual {
                self.update_status = Some("An update check is already running.".into());
            }
            return;
        }
        self.update_status = Some("Checking for updates...".into());
        let (sender, receiver) = mpsc::channel();
        match std::thread::Builder::new()
            .name("github-update-check".to_owned())
            .spawn(move || {
                let result = crate::updates::check_for_update().map_err(|error| error.to_string());
                let _ = sender.send((manual, result));
            }) {
            Ok(_) => self.update_task = Some(receiver),
            Err(error) => {
                self.update_status = Some(format!("Could not start update check: {error}"));
            }
        }
    }

    /// Non-blockingly applies an update worker's terminal result to banner/status state.
    pub(super) fn poll_update_check(&mut self) {
        let Some(receiver) = &self.update_task else {
            return;
        };
        match receiver.try_recv() {
            Ok((_manual, Ok(UpdateCheck::Available(info)))) => {
                self.update_status = Some(format!(
                    "Update available: {} → {}",
                    info.current_version, info.latest_version
                ));
                self.update_info = Some(info);
                self.update_dismissed = false;
                self.update_task = None;
            }
            Ok((
                _manual,
                Ok(UpdateCheck::UpToDate {
                    current_version,
                    latest_version,
                }),
            )) => {
                self.update_status = Some(format!(
                    "Ferrite is up to date ({current_version}; latest release {latest_version})."
                ));
                self.update_info = None;
                self.update_task = None;
            }
            Ok((_manual, Err(error))) => {
                self.update_status = Some(format!("Could not check for updates: {error}"));
                self.update_task = None;
            }
            Err(TryRecvError::Disconnected) => {
                self.update_status = Some("Could not check for updates: worker stopped.".into());
                self.update_task = None;
            }
            Err(TryRecvError::Empty) => {}
        }
    }

    /// Draws a session-dismissible release banner without changing update preferences.
    pub(super) fn update_banner(&mut self, ui: &mut egui::Ui) {
        if self.update_dismissed {
            return;
        }
        let Some(info) = self.update_info.as_ref() else {
            return;
        };
        let current = info.current_version.to_string();
        let latest = info.latest_version.to_string();
        let release_url = info.release_url.clone();
        let release_name = info.release_name.clone();
        egui::Frame::new()
            .fill(self.card_color())
            .stroke(egui::Stroke::new(1.0, self.accent_color()))
            .corner_radius(self.ui_settings.appearance.corner_radius)
            .inner_margin(12.0)
            .show(ui, |ui| {
                ui.horizontal_wrapped(|ui| {
                    ui.label(
                        RichText::new("Update Available")
                            .strong()
                            .color(self.accent_color()),
                    );
                    ui.label(format!("Current {current} · Latest {latest}"));
                    if let Some(name) = release_name {
                        ui.label(format!("· {name}"));
                    }
                    if ui.button("Open Release").clicked() {
                        ui.ctx().open_url(egui::OpenUrl::new_tab(release_url));
                    }
                    if ui.button("Dismiss").clicked() {
                        self.update_dismissed = true;
                    }
                });
            });
        ui.add_space(8.0);
    }

    /// Reconciles the optional IPC owner with the persisted Rich Presence setting.
    pub(super) fn sync_discord_presence(&mut self) {
        if self.config.discord.rich_presence {
            if self.discord.is_none() {
                self.discord = DiscordPresence::new();
            }
        } else if let Some(mut presence) = self.discord.take() {
            presence.clear();
        }
    }

    /// Connects or disconnects Rich Presence immediately when its setting changes.
    pub(super) fn set_discord_enabled(&mut self, enabled: bool) {
        self.config.discord.rich_presence = enabled;
        self.sync_discord_presence();
        let message = if enabled && self.discord.is_some() {
            "Discord Rich Presence enabled."
        } else if enabled {
            "Discord Rich Presence is enabled, but Discord is unavailable."
        } else {
            "Discord Rich Presence disabled."
        };
        self.save_config_change(message);
    }

    /// Persists typed settings and refreshes the advanced editor only after success.
    ///
    /// Core settings go to `config.toml` and appearance/layout to `ui.toml`; both are
    /// written atomically. `ui.toml` is written first so a successful core save can
    /// safely drop any legacy UI tables from `config.toml`.
    pub(super) fn save_config_change(&mut self, success_message: &str) {
        let result = crate::ui_settings::save(&self.paths, &self.ui_settings)
            .and_then(|()| crate::config::save(&self.paths, &self.config));
        match result {
            Ok(()) => {
                self.running_text = success_message.to_owned();
                self.config_status = Some(success_message.to_owned());
                self.raw_config_toml = crate::config::to_toml(&self.config).unwrap_or_default();
            }
            Err(error) => {
                let message = format!("Failed to save configuration: {error}");
                self.running_text = message.clone();
                self.config_status = Some(message);
            }
        }
    }

    /// Replaces all live settings after a validated load/apply and reconciles dependents.
    pub(super) fn replace_config(&mut self, config: Config, raw_toml: String, message: &str) {
        self.config = config;
        self.raw_config_toml = raw_toml;
        self.sync_discord_presence();
        self.running_text = message.to_owned();
        self.config_status = Some(message.to_owned());
    }

    /// Reloads typed and raw representations together, leaving current state on failure.
    pub(super) fn reload_config(&mut self) {
        match crate::config::load(&self.paths) {
            Ok(config) => {
                let raw = crate::config::read_toml(&self.paths)
                    .or_else(|_| crate::config::to_toml(&config))
                    .unwrap_or_default();
                if let Ok(settings) = crate::ui_settings::load(&self.paths) {
                    self.ui_settings = settings;
                    self.accent_edit = self.ui_settings.appearance.accent.clone();
                }
                self.replace_config(config, raw, "Reloaded configuration from disk.");
            }
            Err(error) => {
                self.config_status = Some(format!("Reload failed: {error}"));
            }
        }
    }

    /// Validates and saves the editor buffer before replacing live configuration.
    pub(super) fn apply_raw_config(&mut self) {
        match crate::config::save_toml(&self.paths, &self.raw_config_toml) {
            Ok(config) => {
                let raw = crate::config::to_toml(&config).unwrap_or_default();
                self.replace_config(config, raw, "Applied and saved configuration TOML.");
            }
            Err(error) => {
                self.config_status = Some(format!("TOML was not saved: {error}"));
            }
        }
    }

    pub(super) fn is_light_theme(&self) -> bool {
        match self.ui_settings.appearance.theme_config.preset {
            ThemePreset::Legacy => self.ui_settings.appearance.theme == "light",
            ThemePreset::Dark => false,
            ThemePreset::Light => true,
            ThemePreset::Custom => {
                let color = self.background_color();
                (u32::from(color.r()) * 299
                    + u32::from(color.g()) * 587
                    + u32::from(color.b()) * 114)
                    > 128_000
            }
        }
    }

    pub(super) fn accent_color(&self) -> Color32 {
        let value = match self.ui_settings.appearance.theme_config.preset {
            ThemePreset::Custom => {
                &self
                    .ui_settings
                    .appearance
                    .theme_config
                    .custom_palette
                    .accent
            }
            ThemePreset::Legacy | ThemePreset::Dark | ThemePreset::Light => {
                &self.ui_settings.appearance.accent
            }
        };
        parse_hex_color(value).unwrap_or(ACCENT)
    }

    pub(super) fn background_color(&self) -> Color32 {
        match self.ui_settings.appearance.theme_config.preset {
            ThemePreset::Legacy => {
                if self.ui_settings.appearance.theme == "light" {
                    Color32::from_rgb(242, 244, 248)
                } else {
                    BACKGROUND
                }
            }
            ThemePreset::Dark => BACKGROUND,
            ThemePreset::Light => Color32::from_rgb(242, 244, 248),
            ThemePreset::Custom => parse_hex_color(
                &self
                    .ui_settings
                    .appearance
                    .theme_config
                    .custom_palette
                    .background,
            )
            .unwrap_or(BACKGROUND),
        }
    }

    pub(super) fn sidebar_color(&self) -> Color32 {
        match self.ui_settings.appearance.theme_config.preset {
            ThemePreset::Legacy => {
                if self.ui_settings.appearance.theme == "light" {
                    Color32::from_rgb(226, 230, 237)
                } else {
                    SIDEBAR
                }
            }
            ThemePreset::Dark => SIDEBAR,
            ThemePreset::Light => Color32::from_rgb(226, 230, 237),
            ThemePreset::Custom => parse_hex_color(
                &self
                    .ui_settings
                    .appearance
                    .theme_config
                    .custom_palette
                    .sidebar,
            )
            .unwrap_or(SIDEBAR),
        }
    }

    pub(super) fn card_color(&self) -> Color32 {
        match self.ui_settings.appearance.theme_config.preset {
            ThemePreset::Legacy => {
                if self.ui_settings.appearance.theme == "light" {
                    Color32::WHITE
                } else {
                    CARD
                }
            }
            ThemePreset::Dark => CARD,
            ThemePreset::Light => Color32::WHITE,
            ThemePreset::Custom => {
                parse_hex_color(&self.ui_settings.appearance.theme_config.custom_palette.card)
                    .unwrap_or(CARD)
            }
        }
    }

    pub(super) fn text_color(&self) -> Color32 {
        match self.ui_settings.appearance.theme_config.preset {
            ThemePreset::Legacy => {
                if self.ui_settings.appearance.theme == "light" {
                    Color32::BLACK
                } else {
                    Color32::WHITE
                }
            }
            ThemePreset::Dark => Color32::WHITE,
            ThemePreset::Light => Color32::BLACK,
            ThemePreset::Custom => {
                parse_hex_color(&self.ui_settings.appearance.theme_config.custom_palette.text)
                    .unwrap_or(Color32::WHITE)
            }
        }
    }

    pub(super) fn muted_color(&self) -> Color32 {
        match self.ui_settings.appearance.theme_config.preset {
            ThemePreset::Custom => parse_hex_color(
                &self
                    .ui_settings
                    .appearance
                    .theme_config
                    .custom_palette
                    .muted,
            )
            .unwrap_or(MUTED),
            ThemePreset::Legacy | ThemePreset::Dark | ThemePreset::Light => MUTED,
        }
    }

    /// Resolves the global or current-page background selected by appearance settings.
    pub(super) fn active_background_settings(&self) -> &BackgroundSettings {
        let backgrounds = &self.ui_settings.appearance.background;
        if !backgrounds.use_per_page {
            return &backgrounds.global;
        }
        match self.current_page {
            Page::Play => &backgrounds.play,
            Page::Instances => &backgrounds.instances,
            Page::Mods => &backgrounds.mods,
            Page::Settings => &backgrounds.settings,
        }
    }

    /// Draws launcher settings as page content rather than a popup window.
    pub(super) fn settings_page(&mut self, ui: &mut egui::Ui) {
        page_heading(ui, "Settings", "Configure Ferrite Launcher.");
        self.account_section(ui);
        ui.add_space(20.0);
        ui.horizontal(|ui| {
            for tab in ["Global", "Launcher", "Appearance", "Layout", "Advanced"] {
                if ui
                    .selectable_label(self.current_settings_tab == tab, tab)
                    .clicked()
                {
                    self.current_settings_tab = tab.to_owned();
                }
            }
        });
        ui.separator();
        ui.add_space(12.0);

        egui::ScrollArea::vertical()
            .id_salt("settings_content")
            .show(ui, |ui| match self.current_settings_tab.as_str() {
            "Global" => {
                ui.heading("Global settings");
                ui.label("Minecraft memory");
                let changed = ui
                    .add(
                        egui::Slider::new(
                            &mut self.config.minecraft.default_memory_mb,
                            512..=32_768,
                        )
                        .step_by(256.0)
                        .suffix(" MB"),
                    )
                    .changed();
                ui.label("Applied to the next Minecraft launch.");
                ui.separator();
                ui.checkbox(&mut self.is_global_checked, "Enable global defaults");
                if changed {
                    self.save_config_change("Saved default Minecraft memory.");
                }
            }
            "Launcher" => {
                ui.heading("Launcher settings");
                let mut changed = false;
                let mut keep_open = !self.config.launcher.close_on_launch;
                if ui
                    .checkbox(&mut keep_open, "Keep launcher open while playing")
                    .changed()
                {
                    self.config.launcher.close_on_launch = !keep_open;
                    changed = true;
                }
                changed |= ui
                    .checkbox(
                        &mut self.config.launcher.show_snapshots,
                        "Show Minecraft snapshots",
                    )
                    .changed();
                ui.label("Snapshot visibility is refreshed when Ferrite restarts.");
                changed |= ui
                    .checkbox(
                        &mut self.config.launcher.check_for_updates,
                        "Check for launcher updates automatically",
                    )
                    .changed();
                if changed {
                    self.save_config_change("Saved launcher settings.");
                }
                if ui
                    .add_enabled(
                        self.update_task.is_none(),
                        egui::Button::new(if self.update_task.is_some() {
                            "Checking..."
                        } else {
                            "Check for Updates"
                        }),
                    )
                    .clicked()
                {
                    self.start_update_check(true);
                }
                if let Some(status) = &self.update_status {
                    ui.label(status);
                }

                ui.separator();
                let mut discord_enabled = self.config.discord.rich_presence;
                if ui
                    .checkbox(&mut discord_enabled, "Enable Discord Rich Presence")
                    .changed()
                {
                    self.set_discord_enabled(discord_enabled);
                }
                if self.config.discord.rich_presence && self.discord.is_none() {
                    ui.label(
                        RichText::new(
                            "Enabled, but Discord is not currently available. Toggle off and on to retry.",
                        )
                        .color(MUTED),
                    );
                }
            }
            "Appearance" => {
                ui.heading("Appearance");
                let mut changed = false;
                let previous_theme = self.ui_settings.appearance.theme_config.preset;
                egui::ComboBox::from_label("Theme")
                    .selected_text(theme_preset_label(self.ui_settings.appearance.theme_config.preset))
                    .show_ui(ui, |ui| {
                        ui.selectable_value(
                            &mut self.ui_settings.appearance.theme_config.preset,
                            ThemePreset::Legacy,
                            "Classic / existing settings",
                        );
                        ui.selectable_value(
                            &mut self.ui_settings.appearance.theme_config.preset,
                            ThemePreset::Dark,
                            "Ferrite Dark",
                        );
                        ui.selectable_value(
                            &mut self.ui_settings.appearance.theme_config.preset,
                            ThemePreset::Light,
                            "Ferrite Light",
                        );
                        ui.selectable_value(
                            &mut self.ui_settings.appearance.theme_config.preset,
                            ThemePreset::Custom,
                            "Custom palette",
                        );
                    });
                if previous_theme != self.ui_settings.appearance.theme_config.preset {
                    if !matches!(
                        self.ui_settings.appearance.theme_config.preset,
                        ThemePreset::Legacy | ThemePreset::Custom
                    ) {
                        self.ui_settings.appearance.theme = if matches!(
                            self.ui_settings.appearance.theme_config.preset,
                            ThemePreset::Light
                        ) {
                            "light".to_owned()
                        } else {
                            "dark".to_owned()
                        };
                    }
                    changed = true;
                }
                changed |= ui
                    .add(
                        egui::Slider::new(&mut self.ui_settings.appearance.font_scale, 0.5..=2.0)
                            .step_by(0.05)
                            .text("Font scale"),
                    )
                    .changed();
                changed |= ui
                    .add(
                        egui::Slider::new(&mut self.ui_settings.appearance.corner_radius, 0..=32)
                            .text("Corner radius"),
                    )
                    .changed();

                if matches!(
                    self.ui_settings.appearance.theme_config.preset,
                    ThemePreset::Custom
                ) {
                    ui.label("Custom theme palette");
                    changed |= theme_palette_ui(
                        ui,
                        &mut self.ui_settings.appearance.theme_config.custom_palette,
                    );
                } else {
                    ui.label("Accent color");
                    ui.horizontal(|ui| {
                        let mut color = self.accent_color();
                        if ui.color_edit_button_srgba(&mut color).changed() {
                            self.ui_settings.appearance.accent = color_to_hex(color);
                            self.accent_edit = self.ui_settings.appearance.accent.clone();
                            changed = true;
                        }
                        ui.text_edit_singleline(&mut self.accent_edit);
                        if ui.button("Apply color").clicked() {
                            if let Some(color) = parse_hex_color(self.accent_edit.trim()) {
                                self.ui_settings.appearance.accent = color_to_hex(color);
                                self.accent_edit = self.ui_settings.appearance.accent.clone();
                                changed = true;
                            } else {
                                self.config_status =
                                    Some("Accent must use #RRGGBB format.".to_owned());
                            }
                        }
                    });
                }
                ui.separator();
                ui.heading("Background image");
                ui.label("The solid theme background remains active when the source is None.");

                changed |= ui
                    .checkbox(
                        &mut self.ui_settings.appearance.background.use_per_page,
                        "Use different backgrounds for each page",
                    )
                    .changed();

                if self.ui_settings.appearance.background.use_per_page {
                    if ui.button("Copy global background to every page").clicked() {
                        let global = self.ui_settings.appearance.background.global.clone();
                        self.ui_settings.appearance.background.play = global.clone();
                        self.ui_settings.appearance.background.instances = global.clone();
                        self.ui_settings.appearance.background.mods = global.clone();
                        self.ui_settings.appearance.background.settings = global;
                        changed = true;
                    }
                    egui::ComboBox::from_label("Background to edit")
                        .selected_text(&self.background_edit_target)
                        .show_ui(ui, |ui| {
                            for target in ["Play", "Instances", "Mods", "Settings"] {
                                ui.selectable_value(
                                    &mut self.background_edit_target,
                                    target.to_owned(),
                                    target,
                                );
                            }
                        });
                    if self.background_edit_target == "Global" {
                        self.background_edit_target = "Play".to_owned();
                    }
                } else {
                    self.background_edit_target = "Global".to_owned();
                }

                let target = self.background_edit_target.clone();
                let background_changed = {
                    let backgrounds = &mut self.ui_settings.appearance.background;
                    let settings = match target.as_str() {
                        "Play" => &mut backgrounds.play,
                        "Instances" => &mut backgrounds.instances,
                        "Mods" => &mut backgrounds.mods,
                        "Settings" => &mut backgrounds.settings,
                        _ => &mut backgrounds.global,
                    };
                    background_settings_ui(ui, settings)
                };
                changed |= background_changed;

                let editing_visible_background =
                    !self.ui_settings.appearance.background.use_per_page || target == "Settings";
                if !editing_visible_background {
                    ui.label(RichText::new(format!(
                        "Open the {target} page to preview this background."
                    )).color(MUTED));
                } else {
                    match self.background.status() {
                    BackgroundStatus::Idle | BackgroundStatus::Disabled => {}
                    BackgroundStatus::Loading => {
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.label("Loading background image...");
                        });
                    }
                    BackgroundStatus::Ready => {
                        ui.label(
                            RichText::new("Background image ready.").color(self.accent_color()),
                        );
                    }
                    BackgroundStatus::Failed(error) => {
                        ui.label(RichText::new(format!("Background failed: {error}")).color(MUTED));
                    }
                }
                }

                ui.separator();
                ui.checkbox(
                    &mut self.is_appearance_checked,
                    "Use compact instance cards",
                );
                if changed {
                    self.save_config_change("Saved appearance settings.");
                }
            }
            "Layout" => {
                ui.heading("Layout and widgets");
                self.layout_settings_ui(ui);
            }
            "Advanced" => {
                ui.heading("Advanced configuration");
                ui.horizontal(|ui| {
                    if ui.button("Open Config Folder").clicked() {
                        self.config_status = Some(match crate::config::open_config_folder(&self.paths) {
                            Ok(()) => "Opened the config folder.".to_owned(),
                            Err(error) => format!("Could not open config folder: {error}"),
                        });
                    }
                    if ui.button("Reload Config").clicked() {
                        self.reload_config();
                    }
                });
                ui.label(
                    RichText::new(
                        crate::config::config_path(&self.paths)
                            .display()
                            .to_string(),
                    )
                    .small()
                    .color(MUTED),
                );
                ui.label("Raw TOML is only parsed and saved when Apply / Save is pressed.");
                ui.add(
                    egui::TextEdit::multiline(&mut self.raw_config_toml)
                        .code_editor()
                        .desired_rows(18)
                        .desired_width(f32::INFINITY),
                );
                if ui.button("Apply / Save").clicked() {
                    self.apply_raw_config();
                }
            }
            _ => {}
        });

        if let Some(status) = &self.config_status {
            ui.add_space(8.0);
            ui.label(status);
        }
    }
}

fn theme_preset_label(preset: ThemePreset) -> &'static str {
    match preset {
        ThemePreset::Legacy => "Classic / existing settings",
        ThemePreset::Dark => "Ferrite Dark",
        ThemePreset::Light => "Ferrite Light",
        ThemePreset::Custom => "Custom palette",
    }
}

fn theme_palette_ui(ui: &mut egui::Ui, palette: &mut ThemePalette) -> bool {
    let mut changed = false;
    changed |= theme_color_row(ui, "Background", &mut palette.background);
    changed |= theme_color_row(ui, "Sidebar", &mut palette.sidebar);
    changed |= theme_color_row(ui, "Cards", &mut palette.card);
    changed |= theme_color_row(ui, "Accent", &mut palette.accent);
    changed |= theme_color_row(ui, "Text", &mut palette.text);
    changed |= theme_color_row(ui, "Muted text", &mut palette.muted);
    changed
}

fn theme_color_row(ui: &mut egui::Ui, label: &str, value: &mut String) -> bool {
    let mut changed = false;
    ui.horizontal(|ui| {
        ui.label(label);
        let mut color = parse_hex_color(value).unwrap_or(Color32::BLACK);
        if ui.color_edit_button_srgba(&mut color).changed() {
            *value = color_to_hex(color);
            changed = true;
        }
        ui.monospace(value.as_str());
    });
    changed
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum BackgroundSourceKind {
    None,
    LocalFile,
    HttpsUrl,
    RandomFolder,
}

/// Edits one global/page background and reports whether persisted state changed.
fn background_settings_ui(ui: &mut egui::Ui, settings: &mut BackgroundSettings) -> bool {
    let mut changed = false;
    let mut source_kind = match settings.source {
        BackgroundSource::None => BackgroundSourceKind::None,
        BackgroundSource::LocalFile { .. } => BackgroundSourceKind::LocalFile,
        BackgroundSource::HttpsUrl { .. } => BackgroundSourceKind::HttpsUrl,
        BackgroundSource::RandomFolder { .. } => BackgroundSourceKind::RandomFolder,
    };
    let previous_kind = source_kind;
    egui::ComboBox::from_label("Source")
        .selected_text(background_source_label(source_kind))
        .show_ui(ui, |ui| {
            ui.selectable_value(&mut source_kind, BackgroundSourceKind::None, "None");
            ui.selectable_value(
                &mut source_kind,
                BackgroundSourceKind::LocalFile,
                "Local image",
            );
            ui.selectable_value(
                &mut source_kind,
                BackgroundSourceKind::HttpsUrl,
                "HTTPS URL",
            );
            ui.selectable_value(
                &mut source_kind,
                BackgroundSourceKind::RandomFolder,
                "Random image from folder",
            );
        });
    if source_kind != previous_kind {
        settings.source = match source_kind {
            BackgroundSourceKind::None => BackgroundSource::None,
            BackgroundSourceKind::LocalFile => BackgroundSource::LocalFile {
                path: PathBuf::new(),
            },
            BackgroundSourceKind::HttpsUrl => BackgroundSource::HttpsUrl { url: String::new() },
            BackgroundSourceKind::RandomFolder => BackgroundSource::RandomFolder {
                path: PathBuf::new(),
            },
        };
        changed = true;
    }

    match &mut settings.source {
        BackgroundSource::None => {}
        BackgroundSource::LocalFile { path } => {
            ui.horizontal(|ui| {
                let mut display = path.display().to_string();
                if ui.text_edit_singleline(&mut display).changed() {
                    *path = PathBuf::from(display);
                    changed = true;
                }
                if ui.button("Choose Image...").clicked()
                    && let Some(selected) = rfd::FileDialog::new()
                        .set_title("Choose a background image")
                        .add_filter("Images", &["png", "jpg", "jpeg", "webp"])
                        .pick_file()
                {
                    *path = selected;
                    changed = true;
                }
            });
        }
        BackgroundSource::HttpsUrl { url } => {
            ui.label("Only HTTPS URLs are loaded.");
            changed |= ui.text_edit_singleline(url).changed();
        }
        BackgroundSource::RandomFolder { path } => {
            ui.horizontal(|ui| {
                let mut display = path.display().to_string();
                if ui.text_edit_singleline(&mut display).changed() {
                    *path = PathBuf::from(display);
                    changed = true;
                }
                if ui.button("Choose Folder...").clicked()
                    && let Some(selected) = rfd::FileDialog::new()
                        .set_title("Choose a background image folder")
                        .pick_folder()
                {
                    *path = selected;
                    changed = true;
                }
            });
            ui.label("A supported image is chosen at random and stays active until reloaded.");
        }
    }

    if !matches!(settings.source, BackgroundSource::None) {
        egui::ComboBox::from_label("Display mode")
            .selected_text(background_fit_label(&settings.fit))
            .show_ui(ui, |ui| {
                changed |= ui
                    .selectable_value(&mut settings.fit, BackgroundFit::Cover, "Cover")
                    .changed();
                changed |= ui
                    .selectable_value(&mut settings.fit, BackgroundFit::Contain, "Contain")
                    .changed();
                changed |= ui
                    .selectable_value(&mut settings.fit, BackgroundFit::Stretch, "Stretch")
                    .changed();
                changed |= ui
                    .selectable_value(&mut settings.fit, BackgroundFit::Tile, "Tile")
                    .changed();
            });
        ui.horizontal(|ui| {
            egui::ComboBox::from_label("Horizontal")
                .selected_text(horizontal_alignment_label(&settings.horizontal_alignment))
                .show_ui(ui, |ui| {
                    changed |= ui
                        .selectable_value(
                            &mut settings.horizontal_alignment,
                            HorizontalAlignment::Left,
                            "Left",
                        )
                        .changed();
                    changed |= ui
                        .selectable_value(
                            &mut settings.horizontal_alignment,
                            HorizontalAlignment::Center,
                            "Center",
                        )
                        .changed();
                    changed |= ui
                        .selectable_value(
                            &mut settings.horizontal_alignment,
                            HorizontalAlignment::Right,
                            "Right",
                        )
                        .changed();
                });
            egui::ComboBox::from_label("Vertical")
                .selected_text(vertical_alignment_label(&settings.vertical_alignment))
                .show_ui(ui, |ui| {
                    changed |= ui
                        .selectable_value(
                            &mut settings.vertical_alignment,
                            VerticalAlignment::Top,
                            "Top",
                        )
                        .changed();
                    changed |= ui
                        .selectable_value(
                            &mut settings.vertical_alignment,
                            VerticalAlignment::Center,
                            "Center",
                        )
                        .changed();
                    changed |= ui
                        .selectable_value(
                            &mut settings.vertical_alignment,
                            VerticalAlignment::Bottom,
                            "Bottom",
                        )
                        .changed();
                });
        });
        changed |= ui
            .add(egui::Slider::new(&mut settings.opacity, 0.0..=1.0).text("Image opacity"))
            .changed();
        changed |= ui
            .add(
                egui::Slider::new(&mut settings.blur, 0.0..=100.0)
                    .step_by(0.5)
                    .text("Blur"),
            )
            .changed();

        ui.label("Overlay color and opacity");
        ui.horizontal(|ui| {
            let mut overlay = parse_hex_color(&settings.overlay).unwrap_or(Color32::BLACK);
            if ui.color_edit_button_srgba(&mut overlay).changed() {
                settings.overlay = color_to_hex(overlay);
                changed = true;
            }
            ui.monospace(&settings.overlay);
        });
        changed |= ui
            .add(
                egui::Slider::new(&mut settings.overlay_opacity, 0.0..=1.0).text("Overlay opacity"),
            )
            .changed();

        ui.horizontal(|ui| {
            if ui.button("Reload / choose another").clicked() {
                settings.reload_nonce = settings.reload_nonce.wrapping_add(1);
                changed = true;
            }
            if ui.button("Reset background").clicked() {
                *settings = BackgroundSettings::default();
                changed = true;
            }
        });
    }
    changed
}

fn background_source_label(kind: BackgroundSourceKind) -> &'static str {
    match kind {
        BackgroundSourceKind::None => "None",
        BackgroundSourceKind::LocalFile => "Local image",
        BackgroundSourceKind::HttpsUrl => "HTTPS URL",
        BackgroundSourceKind::RandomFolder => "Random image from folder",
    }
}

fn background_fit_label(fit: &BackgroundFit) -> &'static str {
    match fit {
        BackgroundFit::Cover => "Cover",
        BackgroundFit::Contain => "Contain",
        BackgroundFit::Stretch => "Stretch",
        BackgroundFit::Tile => "Tile",
    }
}

fn horizontal_alignment_label(alignment: &HorizontalAlignment) -> &'static str {
    match alignment {
        HorizontalAlignment::Left => "Left",
        HorizontalAlignment::Center => "Center",
        HorizontalAlignment::Right => "Right",
    }
}

fn vertical_alignment_label(alignment: &VerticalAlignment) -> &'static str {
    match alignment {
        VerticalAlignment::Top => "Top",
        VerticalAlignment::Center => "Center",
        VerticalAlignment::Bottom => "Bottom",
    }
}

/// Parses strict `#RRGGBB` input; invalid text leaves the active accent unchanged.
pub(super) fn parse_hex_color(value: &str) -> Option<Color32> {
    let hex = value.strip_prefix('#')?;
    if hex.len() != 6 {
        return None;
    }
    let red = u8::from_str_radix(&hex[0..2], 16).ok()?;
    let green = u8::from_str_radix(&hex[2..4], 16).ok()?;
    let blue = u8::from_str_radix(&hex[4..6], 16).ok()?;
    Some(Color32::from_rgb(red, green, blue))
}

/// Serializes an egui color to the configuration's lowercase `#rrggbb` form.
pub(super) fn color_to_hex(color: Color32) -> String {
    format!("#{:02x}{:02x}{:02x}", color.r(), color.g(), color.b())
}
