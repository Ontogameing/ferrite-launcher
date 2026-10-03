//! Ferrite's eframe application state and UI orchestration.
//!
//! [`Ferrite`] is the UI thread's single source of truth. Page modules render and
//! mutate portions of it during an egui frame; blocking network and filesystem work
//! instead receives owned snapshots on worker threads and reports typed events over
//! channels. The frame callback polls those channels before drawing, commits results
//! to UI/persistent state, and requests periodic repainting while work is outstanding.

mod auth;
mod dialogs;
mod duplicate;
mod edit;
mod instances;
mod layout;
mod mods;
mod packs_ui;
mod remove;
mod settings;
mod startup;
mod view;

use crate::auth::Account;
use crate::background::BackgroundRenderer;
use crate::config::Config;
use crate::discord::DiscordPresence;
use crate::icons::IconCache;
use crate::instance_mods::InstalledMod;
use crate::instances::{InstanceProfile, SkippedEntry};
use crate::modrinth::{ProjectDetails, SearchFilters, SearchResponse};
use crate::packs::PackFormat;
use crate::ui_settings::{BackgroundSource, UiSettings};
use crate::updates::{UpdateCheck, UpdateInfo};
use eframe::egui::{self, Color32};
use ferrite_launcher::core::paths::AppPaths;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Receiver;
use std::time::Duration;
use view::page_heading;

#[cfg(test)]
use settings::{color_to_hex, parse_hex_color};
#[cfg(test)]
use std::sync::mpsc;

const BACKGROUND: Color32 = Color32::from_rgb(18, 20, 24);
const SIDEBAR: Color32 = Color32::from_rgb(25, 28, 34);
const CARD: Color32 = Color32::from_rgb(31, 35, 42);
const ACCENT: Color32 = Color32::from_rgb(220, 55, 65);
const MUTED: Color32 = Color32::from_rgb(150, 155, 165);

/// Starts Ferrite Launcher in eframe's native window.
///
/// Storage locations are resolved exactly once here and passed down. Before the
/// launcher UI appears, [`startup::StartupApp`] runs the storage migration gate.
pub fn run() -> eframe::Result {
    let paths = match AppPaths::resolve() {
        Ok(paths) => paths,
        Err(error) => {
            eprintln!("Ferrite cannot determine where to store its data: {error}");
            std::process::exit(1);
        }
    };
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1_200.0, 800.0])
            .with_min_inner_size([760.0, 540.0]),
        ..Default::default()
    };

    eframe::run_native(
        "Ferrite Launcher",
        options,
        Box::new(|_cc| Ok(Box::new(startup::StartupApp::new(paths)))),
    )
}

/// A top-level destination in the launcher's sidebar.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum Page {
    Play,
    Instances,
    Mods,
    Settings,
}

/// A terminal result sent from the serialized mod worker to the UI thread.
///
/// Filters and profiles travel with results so the UI can identify their provenance
/// rather than applying them to form values or selections that may have since changed.
enum ModTaskResult {
    Search(SearchFilters, Result<SearchResponse, String>),
    Details(bool, Result<ProjectDetails, String>),
    Local {
        target: InstanceProfile,
        message: String,
        result: Result<Vec<InstalledMod>, String>,
    },
}

/// Actual operations performed during creation, without estimated percentages.
#[derive(Clone, Copy)]
enum InstanceCreationStage {
    Preparing,
    DownloadingMinecraft,
    InstallingLoader,
    Finalizing,
}

impl InstanceCreationStage {
    /// Returns the same progress label for the global footer and creation dialog.
    fn label(self) -> &'static str {
        match self {
            Self::Preparing => "Preparing instance...",
            Self::DownloadingMinecraft => "Downloading Minecraft (reusing cached files)...",
            Self::InstallingLoader => "Installing mod loader...",
            Self::Finalizing => "Finalizing instance...",
        }
    }
}

/// Ordered progress and terminal results from the instance-creation worker.
enum InstanceCreationEvent {
    Stage(InstanceCreationStage),
    DownloadProgress(String),
    Finished(Result<InstanceProfile, String>),
}

/// Progress and terminal results from the serialized import/export worker.
///
/// Workers may prepare files, but profile-list persistence remains on the UI thread so
/// [`Ferrite::instances`] and its on-disk index are committed together.
enum PackTaskEvent {
    Progress(String),
    Imported(Result<Box<PackImportOutcome>, String>),
    /// The written file, or (plain-words reason, raw error).
    Exported(Result<std::path::PathBuf, (String, String)>),
}

/// Imported files awaiting the UI thread's final profile-list commit.
///
/// The pack subsystem has already published the files at the profile's final game
/// directory. `committed` tracks whether the separate instance-index save succeeded.
struct PackImportOutcome {
    /// Storage locations used to roll back the published files.
    paths: AppPaths,
    profile: InstanceProfile,
    files: u64,
    bytes: u64,
    warnings: Vec<String>,
    committed: bool,
}

impl Drop for PackImportOutcome {
    /// Removes published files unless the UI persisted and accepted the profile metadata.
    fn drop(&mut self) {
        if !self.committed {
            let _ = crate::instances::delete_game_dir(&self.paths, &self.profile);
        }
    }
}

/// Messages from one sign-in attempt; only public device instructions reach the UI.
enum AuthEvent {
    Progress(String),
    Device {
        user_code: String,
        verification_uri: String,
    },
    Account(Result<Account, String>),
}

/// One cancellable worker. Dropping its receiver discards even queued credentials.
struct AuthTask {
    events: Receiver<AuthEvent>,
    cancel: Arc<AtomicBool>,
}

impl Drop for AuthTask {
    /// Signals cancellation without blocking the UI on an in-flight HTTP request.
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

/// Memory-only account and sign-in state; never serialized or logged.
#[derive(Default)]
struct AccountSession {
    account: Option<Account>,
    client_id: String,
    /// Explicit opt-in to placeholder credentials for single-player/offline servers.
    offline_mode: bool,
    open: bool,
    task: Option<AuthTask>,
    device: Option<(String, String)>,
    status: String,
}

/// Central application model owned exclusively by eframe's UI thread.
///
/// It combines persisted domain state, editable form state, dialog/page navigation,
/// and channel receivers representing in-flight work. A task receiver's presence is
/// also the busy lock for that subsystem, preventing overlapping operations without
/// sharing mutable application state across threads.
struct Ferrite {
    /// Every storage location, resolved once at startup.
    paths: AppPaths,
    /// Persistent non-secret launcher preferences.
    config: Config,
    /// Frontend-owned appearance and layout preferences persisted in `ui.toml`.
    ui_settings: UiSettings,
    /// Advanced editor state is separate so keystrokes never mutate live settings.
    raw_config_toml: String,
    config_status: Option<String>,
    accent_edit: String,
    /// Appearance editor target (`Global` or one of the top-level pages).
    background_edit_target: String,
    /// Loads and paints the selected background without blocking the UI thread.
    background: BackgroundRenderer,
    close_requested: bool,
    /// One non-blocking GitHub release check and its session-only notification state.
    update_task: Option<Receiver<(bool, Result<UpdateCheck, String>)>>,
    update_info: Option<UpdateInfo>,
    update_status: Option<String>,
    update_dismissed: bool,
    /// Session credentials and a worker cancelled automatically when the app drops.
    auth: AccountSession,
    /// A short result/status message displayed at the bottom of the window.
    running_text: String,
    /// One-time cards from the startup storage gate (data moved, old data not
    /// moved), shown above the page until dismissed.
    startup_cards: Vec<startup::StartupCard>,
    /// The page currently selected in the sidebar.
    current_page: Page,
    /// The label of the active settings subsection.
    current_settings_tab: String,
    /// Temporary state for the placeholder Global settings control.
    is_global_checked: bool,

    /// Temporary state for the placeholder Appearance settings control.
    is_appearance_checked: bool,
    /// Whether the create-instance dialog should be drawn this frame.
    create_instance_open: bool,
    /// The name being entered in the create-instance dialog.
    instance_name: String,
    /// Receives creation progress/results; its presence prevents another creation.
    instance_creation_task: Option<Receiver<InstanceCreationEvent>>,
    /// Creation progress or failure retained independently of other UI messages.
    instance_creation_status: Option<String>,
    /// The Minecraft version selected for the instance being created.
    selected_version: String,
    /// The mod-loader label selected for the instance being created.
    selected_loader: String,
    /// Import/export dialogs and their single serialized background worker.
    import_pack_open: bool,
    export_pack_open: bool,
    pack_path: String,
    pack_name: String,
    pack_format: PackFormat,
    pack_version: String,
    pack_loader_version: String,
    pack_include_worlds: bool,
    pack_task: Option<Receiver<PackTaskEvent>>,
    pack_status: Option<String>,
    /// Which step the Import window shows (choose, reading, preview, ...).
    import_step: packs_ui::ImportStep,
    /// The last export's result, shown in the Export window.
    export_result: Option<packs_ui::ExportResult>,
    /// Ferrite prefilled the Export loader-version field (cleared when the user edits it).
    export_loader_prefilled: bool,
    /// Release versions fetched from Mojang.
    versions: Vec<String>,
    /// Profiles loaded from and saved to the persistent instance store.
    instances: Vec<InstanceProfile>,
    /// Manifest entries that failed validation; reported, never loaded, and written
    /// back unchanged on save so no data is silently dropped.
    skipped_instances: Vec<SkippedEntry>,
    /// Index into [`Self::instances`] for the active profile.
    selected_instance: Option<usize>,
    /// Explicit mod target, independent of Play selection and creation completion.
    mod_target: Option<InstanceProfile>,
    /// Editable filters and the immutable filters associated with displayed results.
    mod_filters: SearchFilters,
    result_filters: SearchFilters,
    mod_categories: String,
    mod_results: Option<SearchResponse>,
    mod_details: Option<(bool, ProjectDetails)>,
    installed_mods: Option<Vec<InstalledMod>>,
    show_installed: bool,
    /// Confirmation captures both the profile and exact filename, never an index.
    pending_uninstall: Option<(InstanceProfile, String)>,
    /// Receives the active search or installation result without blocking egui.
    mod_task: Option<Receiver<ModTaskResult>>,
    /// Bounded asynchronous icon decoding shared by browser and installed mods.
    icons: IconCache,
    /// Live IPC connection governed by `config.discord.rich_presence`.
    discord: Option<DiscordPresence>,
    /// Which instance is owned by which background operation (delete, ...).
    activity: ferrite_launcher::core::activity::ActivityTracker,
    /// The open delete dialog, if any.
    remove_dialog: Option<remove::DeleteDialog>,
    /// The trash/delete worker; its presence also defers closing the window.
    remove_task: Option<remove::RemoveTask>,
    /// The window was asked to close while a removal was running.
    close_after_remove: bool,
    /// The open Duplicate setup modal, if any.
    duplicate_setup: Option<duplicate::DuplicateSetup>,
    /// Running and finished-but-still-shown duplicates.
    duplicate_jobs: Vec<duplicate::DuplicateJob>,
    /// Scroll this instance's card into view on the next Instances frame.
    scroll_to_instance: Option<crate::instances::InstanceDirName>,
    /// Source of window ids for task windows.
    next_job_id: u64,
    /// The open Edit modal, if any.
    edit_dialog: Option<edit::EditDialog>,
    /// The single version/loader update (installing, or failed and still shown).
    edit_task: Option<edit::EditTask>,
}

/// Removes stale import/duplicate temp folders from `instances/` in the background.
/// Silent: the outcome is only logged.
fn spawn_temp_sweep(paths: &AppPaths) {
    use ferrite_launcher::core::sweep;
    let paths = paths.clone();
    let spawned = std::thread::Builder::new()
        .name("temp-sweep".into())
        .spawn(move || {
            if let Err(error) = sweep::sweep_stale_temp_dirs(&paths, sweep::DEFAULT_MIN_AGE) {
                eprintln!("Ferrite: could not check for leftover temp folders: {error}");
            }
        });
    if let Err(error) = spawned {
        eprintln!("Ferrite: could not start the temp folder sweep: {error}");
    }
}

impl Ferrite {
    /// Loads startup state, performs the initial version lookup, and optionally starts
    /// update checking. Recoverable config/instance failures become visible UI status.
    fn new(paths: AppPaths) -> Self {
        // Migrate frontend settings out of config.toml before anything can rewrite it.
        let loaded_ui = crate::ui_settings::load_or_migrate(&paths);
        let ui_settings = loaded_ui.settings;
        let loaded_config = crate::config::load_or_create(&paths);
        let (config, config_warning) = match loaded_config {
            Ok(loaded) => (loaded.config, loaded.warning),
            Err(error) => (
                Config::default(),
                Some(format!(
                    "Could not load Ferrite configuration: {error}. Using defaults."
                )),
            ),
        };
        let discord = if config.discord.rich_presence {
            DiscordPresence::new()
        } else {
            None
        };
        let raw_config_toml = crate::config::read_toml(&paths)
            .or_else(|_| crate::config::to_toml(&config))
            .unwrap_or_default();
        let accent_edit = ui_settings.appearance.accent.clone();

        let versions =
            match crate::minecraft::get_versions_with_snapshots(config.launcher.show_snapshots) {
                Ok(versions) => versions,
                Err(error) => {
                    eprintln!("Failed to fetch Minecraft versions: {error}");
                    Vec::new()
                }
            };

        let (instances, skipped_instances, mut running_text) = match crate::instances::load(&paths)
        {
            Ok(loaded) if loaded.skipped.is_empty() => (
                loaded.profiles,
                Vec::new(),
                String::from("Game not running."),
            ),
            Ok(loaded) => {
                let message = format!(
                    "Skipped {} invalid instance entr{} (kept on disk, not loaded): {}",
                    loaded.skipped.len(),
                    if loaded.skipped.len() == 1 {
                        "y"
                    } else {
                        "ies"
                    },
                    loaded
                        .skipped
                        .iter()
                        .map(ToString::to_string)
                        .collect::<Vec<_>>()
                        .join("; ")
                );
                (loaded.profiles, loaded.skipped, message)
            }
            Err(error) => (
                Vec::new(),
                Vec::new(),
                format!("Failed to load saved instances: {error}"),
            ),
        };
        if let Some(warning) = config_warning {
            running_text = warning;
        }
        if loaded_ui.migrated_from_legacy {
            running_text = format!(
                "Moved appearance and layout settings to {}.",
                paths.ui_settings_file().display()
            );
        }
        if let Some(warning) = loaded_ui.warning {
            running_text = warning;
        }
        let selected_instance = (!instances.is_empty()).then_some(0);
        spawn_temp_sweep(&paths);

        let mut app = Self {
            paths,
            config,
            ui_settings,
            raw_config_toml,
            config_status: None,
            accent_edit,
            background_edit_target: "Global".to_owned(),
            background: BackgroundRenderer::default(),
            close_requested: false,
            update_task: None,
            update_info: None,
            update_status: None,
            update_dismissed: false,
            auth: AccountSession {
                client_id: std::env::var("FERRITE_MICROSOFT_CLIENT_ID").unwrap_or_default(),
                ..Default::default()
            },
            icons: IconCache::default(),
            running_text,
            startup_cards: Vec::new(),
            current_page: Page::Play,
            current_settings_tab: String::from("Global"),
            is_global_checked: false,
            is_appearance_checked: false,
            create_instance_open: false,
            instance_name: String::new(),
            instance_creation_task: None,
            instance_creation_status: None,
            selected_version: versions
                .first()
                .cloned()
                .unwrap_or_else(|| String::from("26.2")),
            selected_loader: String::from("Vanilla"),
            import_pack_open: false,
            export_pack_open: false,
            pack_path: String::new(),
            pack_name: String::new(),
            pack_format: PackFormat::Ferrite,
            pack_version: String::from("1.0.0"),
            pack_loader_version: String::new(),
            pack_include_worlds: true,
            pack_task: None,
            pack_status: None,
            import_step: Default::default(),
            export_result: None,
            export_loader_prefilled: false,
            versions,
            instances,
            skipped_instances,
            selected_instance,
            mod_target: None,
            mod_filters: SearchFilters::default(),
            result_filters: SearchFilters::default(),
            mod_categories: String::new(),
            mod_results: None,
            mod_details: None,
            installed_mods: None,
            show_installed: false,
            pending_uninstall: None,
            mod_task: None,
            discord,
            activity: Default::default(),
            remove_dialog: None,
            remove_task: None,
            close_after_remove: false,
            duplicate_setup: None,
            duplicate_jobs: Vec::new(),
            scroll_to_instance: None,
            next_job_id: 0,
            edit_dialog: None,
            edit_task: None,
        };
        if app.config.launcher.check_for_updates {
            app.start_update_check(false);
        }
        app
    }
}

impl eframe::App for Ferrite {
    /// Runs one immediate-mode frame: incorporate worker events, apply current styling,
    /// draw the selected page and modal windows, then schedule the next needed repaint.
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.poll_instance_creation();
        self.poll_pack_task();
        self.poll_import_preview();
        self.poll_remove_task();
        self.poll_duplicates();
        self.poll_edit_task();
        self.intercept_close_while_removing(ui.ctx());
        self.poll_mod_task();
        self.poll_auth();
        self.poll_update_check();
        ui.ctx()
            .set_zoom_factor(self.ui_settings.appearance.font_scale.clamp(0.5, 2.0));
        let mut visuals = if self.is_light_theme() {
            egui::Visuals::light()
        } else {
            egui::Visuals::dark()
        };
        let background = self.background_color();
        visuals.panel_fill = background;
        visuals.selection.bg_fill = self.accent_color();
        visuals.override_text_color = Some(self.text_color());
        ui.style_mut().visuals = visuals;
        ui.style_mut().spacing.item_spacing = egui::vec2(10.0, 10.0);
        let viewport = ui.max_rect();
        ui.painter().rect_filled(viewport, 0.0, background);
        let background_settings = self.active_background_settings().clone();
        let background_enabled = !matches!(background_settings.source, BackgroundSource::None);
        let background_scope = self
            .ui_settings
            .appearance
            .background
            .use_per_page
            .then_some(self.current_page);
        self.background.paint(
            ui.ctx(),
            ui.painter(),
            viewport,
            &background_settings,
            &background_scope,
        );

        if self.custom_layout_enabled() {
            egui::ScrollArea::vertical()
                .id_salt("custom_layout_canvas")
                .show(ui, |ui| self.render_layout(ui));
        } else {
            let available_width = ui.available_width();
            let available_height = ui.available_height();
            ui.vertical(|ui| {
                self.top_bar(ui);
                ui.add_space(14.0);
                egui::Frame::new()
                    .fill(if background_enabled {
                        Color32::TRANSPARENT
                    } else {
                        background
                    })
                    .inner_margin(egui::Margin::symmetric(28, 18))
                    .show(ui, |ui| {
                        let content_width = (available_width - 56.0).max(0.0);
                        let content_height = (available_height - 112.0).max(0.0);
                        ui.set_width(content_width);
                        ui.set_min_height(content_height);
                        self.update_banner(ui);
                        self.startup_cards_ui(ui);
                        let page_height = (ui.available_height() - 34.0).max(0.0);
                        ui.allocate_ui_with_layout(
                            egui::vec2(content_width, page_height),
                            egui::Layout::top_down(egui::Align::LEFT),
                            |ui| match self.current_page {
                                Page::Play => self.play_page(ui),
                                Page::Instances => self.instances_page(ui),
                                Page::Mods => self.mods_page(ui),
                                Page::Settings => self.settings_page(ui),
                            },
                        );
                        self.layout_status_bar(ui);
                    });
            });
        }

        self.account_window(ui.ctx());
        self.create_instance_window(ui.ctx());
        self.import_pack_window(ui.ctx());
        self.export_pack_window(ui.ctx());
        self.uninstall_window(ui.ctx());
        self.remove_window(ui.ctx());
        self.duplicate_setup_window(ui.ctx());
        self.duplicate_windows(ui.ctx());
        self.edit_window(ui.ctx());
        self.edit_task_window(ui.ctx());
        // Channels do not wake egui directly, so poll promptly while workers can send.
        if self.instance_creation_task.is_some()
            || self.pack_task.is_some()
            || self.remove_task.is_some()
            || self.remove_dialog_busy()
            || self.duplicate_busy()
            || self.edit_busy()
            || self.import_reading()
            || self.mod_task.is_some()
            || self.auth.task.is_some()
            || self.update_task.is_some()
        {
            ui.ctx().request_repaint_after(Duration::from_millis(100));
        } else if self.auth.account.is_some() {
            ui.ctx().request_repaint_after(Duration::from_secs(1));
        }

        if std::mem::take(&mut self.close_requested) {
            ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Avoids the startup network request while testing UI state transitions.
    /// Paths under a never-created temp directory; tests must not touch real user dirs.
    fn test_paths() -> AppPaths {
        let root = std::env::temp_dir().join(format!("ferrite-app-tests-{}", std::process::id()));
        AppPaths::from_base_dirs(ferrite_launcher::core::paths::BaseDirs {
            config: root.join("config"),
            data_local: root.join("data"),
            cache: root.join("cache"),
        })
        .unwrap()
    }

    fn app() -> Ferrite {
        Ferrite {
            paths: test_paths(),
            config: Config::default(),
            ui_settings: UiSettings::default(),
            raw_config_toml: crate::config::to_toml(&Config::default()).unwrap(),
            config_status: None,
            accent_edit: "#ff6600".into(),
            background_edit_target: "Global".into(),
            background: BackgroundRenderer::default(),
            close_requested: false,
            update_task: None,
            update_info: None,
            update_status: None,
            update_dismissed: false,
            auth: AccountSession::default(),
            icons: IconCache::default(),
            running_text: String::new(),
            startup_cards: Vec::new(),
            current_page: Page::Mods,
            current_settings_tab: "Global".into(),
            is_global_checked: false,
            is_appearance_checked: false,
            create_instance_open: false,
            instance_name: String::new(),
            instance_creation_task: None,
            instance_creation_status: None,
            selected_version: "1.21.1".into(),
            selected_loader: "Fabric".into(),
            import_pack_open: false,
            export_pack_open: false,
            pack_path: String::new(),
            pack_name: String::new(),
            pack_format: PackFormat::Ferrite,
            pack_version: "1.0.0".into(),
            pack_loader_version: String::new(),
            pack_include_worlds: true,
            pack_task: None,
            pack_status: None,
            import_step: Default::default(),
            export_result: None,
            export_loader_prefilled: false,
            versions: Vec::new(),
            instances: Vec::new(),
            skipped_instances: Vec::new(),
            selected_instance: None,
            mod_target: None,
            mod_filters: SearchFilters::default(),
            result_filters: SearchFilters::default(),
            mod_categories: String::new(),
            mod_results: None,
            mod_details: None,
            installed_mods: None,
            show_installed: false,
            pending_uninstall: None,
            mod_task: None,
            discord: None,
            activity: Default::default(),
            remove_dialog: None,
            remove_task: None,
            close_after_remove: false,
            duplicate_setup: None,
            duplicate_jobs: Vec::new(),
            scroll_to_instance: None,
            next_job_id: 0,
            edit_dialog: None,
            edit_task: None,
        }
    }

    #[test]
    fn background_selection_uses_global_or_current_page() {
        let mut app = app();
        app.ui_settings.appearance.background.global.opacity = 0.25;
        app.ui_settings.appearance.background.play.opacity = 0.5;
        app.ui_settings.appearance.background.mods.opacity = 0.75;

        assert_eq!(app.active_background_settings().opacity, 0.25);
        app.ui_settings.appearance.background.use_per_page = true;
        assert_eq!(app.active_background_settings().opacity, 0.75);
        app.current_page = Page::Play;
        assert_eq!(app.active_background_settings().opacity, 0.5);
    }

    #[test]
    fn update_results_are_polled_without_networking() {
        let mut app = app();
        let (sender, receiver) = mpsc::channel();
        app.update_task = Some(receiver);
        sender
            .send((
                true,
                Ok(UpdateCheck::Available(UpdateInfo {
                    current_version: semver::Version::parse("0.1.0-alpha").unwrap(),
                    latest_version: semver::Version::parse("0.1.0").unwrap(),
                    release_url: "https://github.com/Ontogameing/ferrite-launcher/releases/tag/v0.1.0".into(),
                    release_name: Some("Ferrite 0.1.0".into()),
                })),
            ))
            .unwrap();
        app.poll_update_check();
        assert!(app.update_task.is_none());
        assert_eq!(
            app.update_info.as_ref().unwrap().latest_version,
            semver::Version::new(0, 1, 0)
        );
        assert!(
            app.update_status
                .as_deref()
                .unwrap()
                .contains("Update available")
        );
    }

    #[test]
    fn appearance_color_helpers_validate_and_round_trip() {
        let color = parse_hex_color("#ff6600").unwrap();
        assert_eq!(color_to_hex(color), "#ff6600");
        assert!(parse_hex_color("orange").is_none());
        assert!(parse_hex_color("#12345").is_none());
    }

    /// Installs a fake worker channel without contacting Microsoft.
    fn auth_worker(app: &mut Ferrite) -> (mpsc::Sender<AuthEvent>, Arc<AtomicBool>) {
        let (sender, events) = mpsc::channel();
        let cancel = Arc::new(AtomicBool::new(false));
        app.auth.task = Some(AuthTask {
            events,
            cancel: Arc::clone(&cancel),
        });
        (sender, cancel)
    }

    #[test]
    fn launch_requires_account_and_opens_sign_in() {
        let mut app = app();
        app.instances.push(profile("Authenticated only"));
        app.selected_instance = Some(0);
        app.launch_selected();
        assert!(app.auth.open);
        assert!(app.running_text.contains("Sign in with Microsoft"));
        assert!(app.running_text.contains("Offline mode"));
        assert!(app.auth.task.is_none());
    }

    #[test]
    fn offline_mode_bypasses_account_requirement() {
        let mut app = app();
        assert!(app.launch_auth_error().is_some());
        app.auth.offline_mode = true;
        assert!(app.launch_auth_error().is_none());
    }

    #[test]
    fn empty_client_id_never_starts_worker() {
        let mut app = app();
        app.auth.client_id = "  ".into();
        app.start_sign_in();
        assert!(app.auth.task.is_none());
        assert!(app.auth.status.contains("application ID"));
    }

    #[test]
    fn auth_events_are_polled_on_other_pages_and_errors_clear_device() {
        let mut app = app();
        let (sender, cancel) = auth_worker(&mut app);
        sender
            .send(AuthEvent::Device {
                user_code: "TEST-CODE".into(),
                verification_uri: "https://microsoft.com/link".into(),
            })
            .unwrap();
        sender
            .send(AuthEvent::Progress("Waiting for Microsoft".into()))
            .unwrap();
        app.poll_auth();
        assert!(app.auth.device.is_some());
        assert_eq!(app.auth.status, "Waiting for Microsoft");
        assert_eq!(app.running_text, app.auth.status);
        sender
            .send(AuthEvent::Account(Err("Access denied".into())))
            .unwrap();
        app.poll_auth();
        assert!(app.auth.device.is_none());
        assert!(app.auth.task.is_none());
        assert!(cancel.load(Ordering::Relaxed));
        assert!(app.auth.status.contains("Access denied"));
    }

    #[test]
    fn cancel_and_sign_out_discard_queued_and_late_results() {
        let mut app = app();
        for sign_out in [false, true] {
            let (sender, cancel) = auth_worker(&mut app);
            sender
                .send(AuthEvent::Progress("Stale progress".into()))
                .unwrap();
            sender
                .send(AuthEvent::Account(Err("Stale result".into())))
                .unwrap();
            app.auth.device = Some(("TEST".into(), "https://microsoft.com/link".into()));
            if sign_out {
                app.sign_out();
            } else {
                app.cancel_sign_in();
            }
            assert!(cancel.load(Ordering::Relaxed));
            assert!(app.auth.device.is_none());
            assert!(
                sender
                    .send(AuthEvent::Progress("Late progress".into()))
                    .is_err()
            );
            let status = app.auth.status.clone();
            let (_new_sender, _) = auth_worker(&mut app);
            app.poll_auth();
            assert_eq!(app.auth.status, status);
            assert!(app.auth.account.is_none());
        }
    }

    #[test]
    fn dropped_app_cancels_worker_and_disconnect_is_reported() {
        let mut app = app();
        let (sender, cancel) = auth_worker(&mut app);
        drop(sender);
        app.poll_auth();
        assert!(app.auth.status.contains("stopped unexpectedly"));
        assert!(cancel.load(Ordering::Relaxed));
        let (sender, cancel) = auth_worker(&mut app);
        drop(app);
        assert!(cancel.load(Ordering::Relaxed));
        assert!(sender.send(AuthEvent::Progress("Late".into())).is_err());
    }

    fn profile(name: &str) -> InstanceProfile {
        InstanceProfile::new(name.into(), "1.21.1".into(), "Fabric".into(), &[])
    }

    #[test]
    fn instance_progress_and_failure_preserve_form_inputs() {
        let mut app = app();
        app.instance_name = "My new instance".into();
        let (sender, receiver) = mpsc::channel();
        app.instance_creation_task = Some(receiver);
        sender
            .send(InstanceCreationEvent::DownloadProgress(
                "Downloading assets".into(),
            ))
            .unwrap();
        app.poll_instance_creation();
        assert_eq!(
            app.instance_creation_status.as_deref(),
            Some("Downloading assets")
        );
        assert!(app.instance_creation_task.is_some());
        sender
            .send(InstanceCreationEvent::Finished(Err(
                "Network interrupted".into()
            )))
            .unwrap();
        app.poll_instance_creation();
        assert!(app.instance_creation_task.is_none());
        assert!(app.create_instance_open);
        assert_eq!(app.instance_name, "My new instance");
        assert!(app.running_text.contains("Network interrupted"));
        assert!(app.instances.is_empty());
    }

    #[test]
    fn pack_worker_progress_and_failure_are_polled_globally() {
        let mut app = app();
        let (sender, receiver) = mpsc::channel();
        app.pack_task = Some(receiver);
        sender
            .send(PackTaskEvent::Progress("Validating archive".into()))
            .unwrap();
        app.poll_pack_task();
        assert_eq!(app.pack_status.as_deref(), Some("Validating archive"));
        assert!(app.pack_busy());
        sender
            .send(PackTaskEvent::Exported(Err((
                "There isn't enough space on that drive.".into(),
                "disk full".into(),
            ))))
            .unwrap();
        app.poll_pack_task();
        assert!(!app.pack_busy());
        assert!(app.running_text.contains("enough space"));
        assert!(matches!(
            app.export_result,
            Some(packs_ui::ExportResult::Failed { ref error, .. }) if error == "disk full"
        ));
    }

    #[test]
    fn active_worker_locks_target_and_rejects_second_task() {
        let mut app = app();
        app.set_mod_target(profile("first"));
        let (sender, receiver) = mpsc::channel();
        app.mod_task = Some(receiver);
        app.set_mod_target(profile("second"));
        app.start_mod_task(|| panic!("must not start another worker"));
        assert_eq!(app.mod_target.as_ref().unwrap().name, "first");
        sender
            .send(ModTaskResult::Search(
                SearchFilters::default(),
                Err("original worker".into()),
            ))
            .unwrap();
        app.poll_mod_task();
        assert!(app.running_text.contains("original worker"));
        assert!(app.mod_task.is_none());
    }

    #[test]
    fn local_results_never_follow_play_selection_or_stale_targets() {
        let mut app = app();
        let first = profile("first");
        let second = profile("second");
        app.instances = vec![first.clone(), second.clone()];
        app.set_mod_target(first.clone());
        app.selected_instance = Some(1); // Creation completion or a Play selection.
        let (sender, receiver) = mpsc::channel();
        app.mod_task = Some(receiver);
        sender
            .send(ModTaskResult::Local {
                target: first,
                message: "first finished".into(),
                result: Ok(vec![]),
            })
            .unwrap();
        app.poll_mod_task();
        assert_eq!(app.installed_mods, Some(vec![]));
        app.installed_mods = None;
        let (sender, receiver) = mpsc::channel();
        app.mod_task = Some(receiver);
        sender
            .send(ModTaskResult::Local {
                target: second,
                message: "stale".into(),
                result: Ok(vec![]),
            })
            .unwrap();
        app.poll_mod_task();
        assert!(app.installed_mods.is_none());
    }

    #[test]
    fn result_filters_survive_form_edits_and_non_mod_install_is_rejected() {
        let mut app = app();
        let target = profile("target");
        app.instances.push(target.clone());
        app.set_mod_target(target);
        let mut submitted = SearchFilters::default();
        submitted.project_type = Some("shader".into());
        submitted.offset = 20;
        let (sender, receiver) = mpsc::channel();
        app.mod_task = Some(receiver);
        sender
            .send(ModTaskResult::Search(
                submitted.clone(),
                Ok(SearchResponse {
                    hits: vec![],
                    offset: 20,
                    limit: 20,
                    total_hits: 100,
                }),
            ))
            .unwrap();
        app.mod_filters.project_type = Some("mod".into());
        app.poll_mod_task();
        assert_eq!(app.result_filters, submitted);
        assert!(!app.can_install(false));
        app.install_mod("unused".into(), "shader".into(), false);
        assert!(app.mod_task.is_none());
        app.mod_target.as_mut().unwrap().loader = "Vanilla".into();
        assert!(!app.can_install(true));
    }
}
