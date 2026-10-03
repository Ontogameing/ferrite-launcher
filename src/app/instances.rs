//! Instance lifecycle, launch controls, and pack import/export orchestration.
//!
//! Long installs and archive operations run on workers with owned profiles/options.
//! The UI thread polls progress and alone mutates the profile list or saves its index,
//! preserving a clear commit point between filesystem preparation and visible state.

use super::{Ferrite, InstanceCreationEvent, InstanceCreationStage, Page, page_heading};
use crate::instances::InstanceProfile;
use crate::loaders::ModLoader;
use crate::packs::PackFormat;
use eframe::egui::{self, Color32, RichText};
use ferrite_launcher::core::activity::{
    BusyOperation, GlobalBusy, InstanceAction, InstanceStatus, disabled_reason,
};
use std::path::PathBuf;
use std::sync::mpsc::{self, TryRecvError};

impl Ferrite {
    /// Parses the form's display label into the loader backend used by workers.
    pub(super) fn selected_loader(&self) -> Option<ModLoader> {
        ModLoader::from_label(&self.selected_loader)
    }

    /// Resolves the selected index defensively because removals can invalidate it.
    pub(super) fn selected_instance(&self) -> Option<&InstanceProfile> {
        self.selected_instance
            .and_then(|index| self.instances.get(index))
    }

    pub(super) fn selected_instance_label(&self) -> String {
        self.selected_instance()
            .map(|instance| instance.name.clone())
            .unwrap_or_else(|| "No instance selected".to_owned())
    }

    /// Validates form input and starts a worker that prepares one instance on disk.
    ///
    /// The new profile is moved into the worker; only a successful terminal event adds
    /// it to UI state and persists the profile index.
    pub(super) fn create_instance(&mut self) {
        if self.instance_creation_task.is_some() || self.create_import_lock().is_some() {
            return;
        }
        self.instance_creation_status = None;
        let name = self.instance_name.trim();
        if name.is_empty() {
            self.running_text = "An instance name is required.".to_owned();
            return;
        }
        if crate::instances::name_taken(&self.instances, name, None) {
            self.running_text = format!("An instance named '{name}' already exists.");
            return;
        }

        let Some(loader) = self.selected_loader() else {
            self.running_text = format!("Unknown mod loader: {}", self.selected_loader);
            return;
        };

        if self.selected_version.trim().is_empty()
            || (!self.versions.is_empty() && !self.versions.contains(&self.selected_version))
        {
            self.running_text =
                "Select a valid Minecraft version before creating an instance.".to_owned();
            return;
        }

        // The folder is allocated case-insensitively against loaded profiles, skipped
        // manifest entries, and folders already on disk; it is never adopted.
        let profile = match crate::instances::new_instance_profile(
            &self.paths,
            name,
            &self.selected_version,
            &self.selected_loader,
            &self.instances,
            &self.skipped_instances,
        ) {
            Ok(profile) => profile,
            Err(error) => {
                self.running_text = format!("Cannot create instance: {error}");
                return;
            }
        };
        let paths = self.paths.clone();
        let (sender, receiver) = mpsc::channel();
        let worker = std::thread::Builder::new()
            .name("instance-creation".to_owned())
            .spawn(move || {
                let result = (|| -> Result<InstanceProfile, String> {
                    let _ = sender.send(InstanceCreationEvent::Stage(
                        InstanceCreationStage::Preparing,
                    ));
                    // A failed install removes the folder this call created.
                    crate::instances::create_instance_files(&paths, &profile, || {
                        install_game_files(&paths, &profile.version, loader, &|event| {
                            let _ = sender.send(event);
                        })
                    })
                    .map_err(|error| match error {
                        crate::instances::InstanceError::Install(detail) => detail,
                        other => format!("Failed to create instance directory: {other}"),
                    })?;
                    let _ = sender.send(InstanceCreationEvent::Stage(
                        InstanceCreationStage::Finalizing,
                    ));
                    Ok(profile)
                })();
                let _ = sender.send(InstanceCreationEvent::Finished(result));
            });
        match worker {
            Ok(_) => {
                self.instance_creation_task = Some(receiver);
                self.instance_creation_status =
                    Some(InstanceCreationStage::Preparing.label().to_owned());
            }
            Err(error) => {
                self.instance_creation_failed(format!("Failed to start instance worker: {error}"))
            }
        }
    }

    /// Drains creation progress and commits a completed profile on the UI thread.
    ///
    /// If saving the profile index fails, the in-memory append is rolled back so memory
    /// continues to match the persisted index; the form remains available for retry.
    pub(super) fn poll_instance_creation(&mut self) {
        loop {
            let Some(receiver) = &self.instance_creation_task else {
                return;
            };
            match receiver.try_recv() {
                Ok(InstanceCreationEvent::Stage(stage)) => {
                    self.instance_creation_status = Some(stage.label().to_owned());
                }
                Ok(InstanceCreationEvent::DownloadProgress(message)) => {
                    self.instance_creation_status = Some(message);
                }
                Ok(InstanceCreationEvent::Finished(result)) => {
                    self.instance_creation_task = None;
                    match result {
                        Ok(profile) => {
                            let name = profile.name.clone();
                            // On failure the profile is not kept and its new folder is
                            // removed, so memory, manifest, and disk stay consistent.
                            let index = match crate::instances::commit_new_instance(
                                &self.paths,
                                &mut self.instances,
                                &self.skipped_instances,
                                profile,
                            ) {
                                Ok(index) => index,
                                Err(error) => {
                                    self.instance_creation_failed(format!(
                                        "Failed to save instance: {error}"
                                    ));
                                    return;
                                }
                            };
                            self.selected_instance = Some(index);
                            self.running_text = format!("Created instance '{name}'.");
                            self.instance_creation_status = None;
                            self.instance_name.clear();
                            self.create_instance_open = false;
                        }
                        Err(error) => self.instance_creation_failed(error),
                    }
                    return;
                }
                Err(TryRecvError::Disconnected) => {
                    self.instance_creation_failed(
                        "The instance creation worker stopped unexpectedly.".to_owned(),
                    );
                    return;
                }
                Err(TryRecvError::Empty) => return,
            }
        }
    }

    /// Clears the busy lock while preserving form input and reopening the failed dialog.
    pub(super) fn instance_creation_failed(&mut self, message: String) {
        self.instance_creation_task = None;
        self.running_text = message.clone();
        self.instance_creation_status = Some(message);
        self.create_instance_open = true;
    }

    /// Treats the single pack receiver as both task handle and serialization guard.
    pub(super) fn pack_busy(&self) -> bool {
        self.pack_task.is_some()
    }

    /// Returns the user-actionable reason authenticated launch is currently unavailable.
    pub(super) fn launch_auth_error(&self) -> Option<&'static str> {
        if self.auth.offline_mode {
            return None;
        }
        match self.auth.account.as_ref() {
            None => Some("Sign in with Microsoft before launching, or select Offline mode."),
            Some(account) if account.is_expired() => {
                Some("Session expired. Please sign in again before launching.")
            }
            Some(_) => None,
        }
    }

    /// Validates cross-subsystem locks and launches the selected profile.
    ///
    /// Launch is delegated synchronously to the backend after copying profile fields,
    /// avoiding an outstanding borrow of `self.instances` while status is updated.
    pub(super) fn launch_selected(&mut self) {
        if let Some(message) = self.launch_auth_error() {
            self.running_text = message.into();
            self.auth.status = message.into();
            self.auth.open = true;
            return;
        }
        if self.mod_task.is_some() || self.pending_uninstall.is_some() || self.pack_busy() {
            self.running_text =
                "Wait for mod or instance import/export work to finish before launching.".into();
            return;
        }
        let Some(instance) = self.selected_instance() else {
            self.running_text = "Select an instance before launching.".to_owned();
            return;
        };
        let name = instance.name.clone();
        let version = instance.version.clone();
        let loader_name = instance.loader.clone();
        let game_dir = instance.game_dir(&self.paths);

        let Some(loader) = ModLoader::from_label(&loader_name) else {
            self.running_text = format!("Unknown mod loader: {loader_name}");
            return;
        };

        let memory_mb = self.config.minecraft.default_memory_mb;
        let result = if self.auth.offline_mode {
            crate::loaders::launch_in_directory_with_memory(
                &self.paths,
                &version,
                loader,
                &game_dir,
                memory_mb,
            )
        } else {
            crate::loaders::launch_authenticated_with_memory(
                &self.paths,
                &version,
                loader,
                &game_dir,
                self.auth.account.as_ref().expect("account checked above"),
                memory_mb,
            )
        };
        let launched = result.is_ok();
        self.running_text = match result {
            Ok(()) if self.auth.offline_mode => format!("Launched '{name}' in offline mode."),
            Ok(()) => format!("Launched '{name}'."),
            Err(error) => format!("Failed to launch '{name}': {error}"),
        };
        if launched && self.config.launcher.close_on_launch {
            self.close_requested = true;
        }
    }

    /// Why Create and Import are unavailable right now, if they are.
    pub(super) fn create_import_lock(&self) -> Option<String> {
        if self.pack_busy() {
            return Some("Wait for the import or export to finish.".into());
        }
        if let Some(task) = &self.edit_task
            && task.installing().is_some()
        {
            return Some("Wait for Ferrite to finish updating the instance.".into());
        }
        None
    }

    /// Draws profile cards and executes at most one deferred card action afterward.
    pub(super) fn instances_page(&mut self, ui: &mut egui::Ui) {
        let muted = self.muted_color();
        page_heading(ui, "Instances", "Manage your Minecraft profiles.");
        ui.add_space(8.0);
        let lock = self.create_import_lock();
        ui.horizontal(|ui| {
            let create = ui.add_enabled(
                lock.is_none(),
                egui::Button::new(RichText::new("＋ Create instance").color(Color32::WHITE))
                    .fill(self.accent_color()),
            );
            if create.clicked() {
                self.create_instance_open = true;
            }
            if let Some(reason) = &lock {
                create.on_disabled_hover_text(reason);
            }
            let import = ui.add_enabled(lock.is_none(), egui::Button::new("Import pack"));
            if import.clicked() {
                self.open_import_window();
            }
            if let Some(reason) = &lock {
                import.on_disabled_hover_text(reason);
            }
        });
        ui.add_space(20.0);

        if self.instances.is_empty() {
            ui.label(
                RichText::new("No instances created yet.")
                    .size(20.0)
                    .color(muted),
            );
            return;
        }

        // Defer mutations until iteration releases its immutable borrow of `instances`.
        let mut action = None;
        let cards: Vec<CardInfo> = self
            .instances
            .iter()
            .map(|instance| {
                let status = self.instance_status(instance);
                let chip = self.instance_chip(instance, &status);
                CardInfo {
                    status,
                    chip,
                    folder: instance.game_dir(&self.paths),
                }
            })
            .collect();
        let mod_idle =
            self.mod_task.is_none() && self.pending_uninstall.is_none() && !self.pack_busy();
        let scroll_to = self.scroll_to_instance.take();
        let light = self.ui_settings.appearance.theme == "light";
        let accent = self.accent_color();
        let card_color = self.card_color();
        let corner_radius = self.ui_settings.appearance.corner_radius;
        egui::ScrollArea::vertical().show(ui, |ui| {
            for (index, instance) in self.instances.iter().enumerate() {
                let selected = self.selected_instance == Some(index);
                let info = &cards[index];
                let card = ui.scope_builder(
                    egui::UiBuilder::new()
                        .id_salt(("instance-card", instance.directory().as_str()))
                        .sense(egui::Sense::click()),
                    |ui| {
                        egui::Frame::new()
                            .fill(match (selected, light) {
                                (true, true) => Color32::from_rgb(255, 240, 232),
                                (true, false) => Color32::from_rgb(43, 39, 44),
                                _ => card_color,
                            })
                            .stroke(egui::Stroke::new(
                                if selected { 1.5 } else { 1.0 },
                                if selected {
                                    accent
                                } else {
                                    Color32::from_rgb(50, 55, 64)
                                },
                            ))
                            .corner_radius(corner_radius)
                            .inner_margin(18.0)
                            .show(ui, |ui| {
                                ui.set_width(ui.available_width());
                                instance_card(ui, index, instance, info, mod_idle, accent, muted)
                            })
                            .inner
                    },
                );
                if let Some(card_action) = card.inner {
                    action = Some(card_action);
                }
                let response = card.response;
                if response.clicked() {
                    action = Some(CardAction::Select(index));
                }
                response.context_menu(|ui| {
                    if let Some(menu_action) = card_menu(ui, index, instance, info) {
                        action = Some(menu_action);
                    }
                });
                if scroll_to.as_ref() == Some(instance.directory()) {
                    response.scroll_to_me(Some(egui::Align::Center));
                }
                ui.add_space(10.0);
            }
        });
        if let Some(action) = action {
            self.run_card_action(action);
        }
    }

    /// The status chip for a card (spec §2.3), if any.
    fn instance_chip(&self, instance: &InstanceProfile, status: &InstanceStatus) -> Option<Chip> {
        use ferrite_launcher::core::activity::InstanceActivity;
        match status.activity {
            InstanceActivity::Busy(BusyOperation::Deleting) => Some(Chip::Busy {
                text: format!("Moving to {}…", super::dialogs::trash_word()),
                reopen: None,
            }),
            InstanceActivity::Busy(BusyOperation::Updating) => Some(Chip::Busy {
                text: "Updating…".into(),
                reopen: Some(Reopen::Update),
            }),
            InstanceActivity::Busy(BusyOperation::Duplicating) => Some(Chip::Busy {
                text: self
                    .duplicate_job_for(instance.directory())
                    .and_then(super::duplicate::DuplicateJob::progress)
                    .map(|progress| super::duplicate::chip_text(&progress))
                    .unwrap_or_else(|| "Copying…".into()),
                reopen: Some(Reopen::Duplicate(instance.directory().clone())),
            }),
            InstanceActivity::Running => Some(Chip::Running),
            InstanceActivity::Idle if status.folder_missing => Some(Chip::FolderMissing),
            InstanceActivity::Idle => None,
        }
    }

    /// Carries out one card or menu action after the cards were drawn.
    fn run_card_action(&mut self, action: CardAction) {
        match action {
            CardAction::Select(index) => self.selected_instance = Some(index),
            CardAction::Play(index) => {
                self.selected_instance = Some(index);
                self.launch_selected();
            }
            CardAction::Mods(index) => {
                if let Some(target) = self.instances.get(index).cloned() {
                    self.set_mod_target(target);
                    self.show_installed = true;
                    self.current_page = Page::Mods;
                    self.local_mod_task(None);
                }
            }
            CardAction::OpenFolder(folder) => {
                if let Some(message) = super::dialogs::open_folder(&folder) {
                    self.running_text = message;
                }
            }
            CardAction::Edit(index) => {
                self.selected_instance = Some(index);
                if let Some(directory) = self.instances.get(index).map(|p| p.directory().clone()) {
                    self.open_edit_dialog(&directory);
                }
            }
            CardAction::Duplicate(index) => {
                self.selected_instance = Some(index);
                if let Some(directory) = self.instances.get(index).map(|p| p.directory().clone()) {
                    self.open_duplicate_dialog(&directory);
                }
            }
            CardAction::Export(index) => {
                self.selected_instance = Some(index);
                self.open_export_window();
            }
            CardAction::Delete(index) => {
                self.selected_instance = Some(index);
                if let Some(directory) = self.instances.get(index).map(|p| p.directory().clone()) {
                    self.open_remove_dialog(directory);
                }
            }
            CardAction::Reopen(Reopen::Update) => self.show_edit_window(),
            CardAction::Reopen(Reopen::Duplicate(directory)) => {
                self.show_duplicate_window(&directory)
            }
        }
    }

    /// Opens the Export window for the selected instance (spec §6).
    pub(super) fn open_export_window(&mut self) {
        let Some(profile) = self.selected_instance().cloned() else {
            return;
        };
        self.pack_name = profile.name.clone();
        self.pack_format = PackFormat::Ferrite;
        self.pack_include_worlds = true;
        self.pack_loader_version = ModLoader::from_label(&profile.loader)
            .filter(|loader| *loader != ModLoader::Vanilla)
            .and_then(|loader| {
                crate::loaders::installed_loader_version(&self.paths, &profile.version, loader)
            })
            .unwrap_or_default();
        self.pack_status = None;
        self.export_pack_open = true;
    }

    /// Opens the Import window on its first step.
    pub(super) fn open_import_window(&mut self) {
        self.pack_path.clear();
        self.pack_name.clear();
        self.pack_status = None;
        self.import_pack_open = true;
    }

    /// Activity, folder presence, and global locks for one card (cheap: one mutex poll
    /// and one `symlink_metadata` call).
    pub(super) fn instance_status(&self, instance: &InstanceProfile) -> InstanceStatus {
        let running = crate::minecraft::is_instance_running(instance.directory().as_str());
        InstanceStatus {
            activity: self
                .activity
                .activity(instance.directory().as_str(), running),
            folder_missing: crate::instances::folder_missing(&self.paths, instance),
            global: GlobalBusy {
                pack_task: self.pack_busy(),
                mod_task: self.mod_task.is_some() || self.pending_uninstall.is_some(),
                creation_task: self.instance_creation_task.is_some(),
            },
        }
    }

    /// Keeps creation input visible during progress and preserves it when work fails.
    pub(super) fn create_instance_window(&mut self, context: &egui::Context) {
        if !self.create_instance_open {
            return;
        }

        let mut open = self.create_instance_open;
        let mut create_requested = false;
        egui::Window::new("Create instance")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .default_width(420.0)
            .show(context, |ui| {
                ui.label("Each instance keeps its own worlds, mods, and settings.");
                ui.add_space(10.0);
                ui.add_enabled_ui(self.instance_creation_task.is_none(), |ui| {
                    ui.label("Instance name");
                    ui.text_edit_singleline(&mut self.instance_name);
                    egui::ComboBox::from_label("Minecraft version")
                        .selected_text(&self.selected_version)
                        .show_ui(ui, |ui| {
                            for version in &self.versions {
                                ui.selectable_value(
                                    &mut self.selected_version,
                                    version.clone(),
                                    version,
                                );
                            }
                        });
                    let muted = self.muted_color();
                    loader_picker(ui, &mut self.selected_loader, muted);
                    ui.add_space(12.0);
                    create_requested = ui
                        .add_enabled(
                            !self.instance_name.trim().is_empty(),
                            egui::Button::new("Create instance").fill(self.accent_color()),
                        )
                        .clicked();
                });
                if let Some(status) = &self.instance_creation_status {
                    ui.label(status);
                }
            });
        self.create_instance_open = open;
        if create_requested {
            self.create_instance();
        }
    }
}

/// Per-card state computed before drawing.
struct CardInfo {
    status: InstanceStatus,
    chip: Option<Chip>,
    folder: PathBuf,
}

/// Which task window a chip reopens.
#[derive(Clone)]
enum Reopen {
    Update,
    Duplicate(crate::instances::InstanceDirName),
}

/// The status chip next to a card's name (spec §2.3).
enum Chip {
    FolderMissing,
    Running,
    Busy {
        text: String,
        reopen: Option<Reopen>,
    },
}

/// What the user did on a card.
enum CardAction {
    Select(usize),
    Play(usize),
    Mods(usize),
    OpenFolder(PathBuf),
    Edit(usize),
    Duplicate(usize),
    Export(usize),
    Delete(usize),
    Reopen(Reopen),
}

/// One action button, disabled with its reason when needed.
fn card_button(
    ui: &mut egui::Ui,
    button: egui::Button,
    reason: Option<String>,
    extra_enabled: bool,
) -> bool {
    let response = ui.add_enabled(extra_enabled && reason.is_none(), button);
    let clicked = response.clicked();
    if let Some(reason) = reason {
        response.on_disabled_hover_text(reason);
    }
    clicked
}

/// The card body: name row with chip and Quilt badge, the version line, and the
/// Play / Mods / ⋯ row (spec §2.1).
fn instance_card(
    ui: &mut egui::Ui,
    index: usize,
    instance: &InstanceProfile,
    info: &CardInfo,
    mod_idle: bool,
    accent: Color32,
    muted: Color32,
) -> Option<CardAction> {
    let mut action = None;
    let reason = |action| disabled_reason(action, &instance.name, &info.status);
    ui.horizontal(|ui| {
        ui.add(
            egui::Label::new(RichText::new(&instance.name).size(20.0).strong()).selectable(false),
        );
        if let Some(chip) = &info.chip
            && let Some(reopen) = chip_ui(ui, chip, muted)
        {
            action = Some(CardAction::Reopen(reopen));
        }
    });
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 6.0;
        super::dialogs::loader_text(
            ui,
            &instance.loader,
            RichText::new(format!(
                "Minecraft {}  •  {}",
                instance.version, instance.loader
            ))
            .color(muted),
            muted,
        );
    });
    ui.horizontal(|ui| {
        let play = egui::Button::new(RichText::new("Play").color(Color32::WHITE)).fill(accent);
        if card_button(ui, play, reason(InstanceAction::Play), mod_idle) {
            action = Some(CardAction::Play(index));
        }
        if card_button(
            ui,
            egui::Button::new("Mods"),
            reason(InstanceAction::Mods),
            mod_idle,
        ) {
            action = Some(CardAction::Mods(index));
        }
        let menu = ui.menu_button("⋯", |ui| card_menu(ui, index, instance, info));
        if let Some(Some(menu_action)) = menu.inner {
            action = Some(menu_action);
        }
        menu.response.on_hover_text("More actions");
    });
    action
}

/// Draws a chip; returns the window to reopen when it was clicked.
fn chip_ui(ui: &mut egui::Ui, chip: &Chip, muted: Color32) -> Option<Reopen> {
    let frame = egui::Frame::new()
        .stroke(egui::Stroke::new(1.0, muted.gamma_multiply(0.6)))
        .corner_radius(8.0)
        .inner_margin(egui::Margin::symmetric(6, 2));
    let small =
        |text: &str| egui::Label::new(RichText::new(text).small().color(muted)).selectable(false);
    match chip {
        Chip::FolderMissing => {
            frame
                .show(ui, |ui| ui.add(small("Folder missing")))
                .response
                .on_hover_text(
                    "Ferrite can't find this instance's folder. It may have been moved or deleted.",
                );
            None
        }
        Chip::Running => {
            frame.show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 4.0;
                    let (rect, _) =
                        ui.allocate_exact_size(egui::vec2(8.0, 8.0), egui::Sense::hover());
                    ui.painter()
                        .circle_filled(rect.center(), 4.0, super::dialogs::RUNNING_GREEN);
                    ui.add(small("Running"));
                });
            });
            None
        }
        Chip::Busy { text, reopen } => {
            let response = frame
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 4.0;
                        ui.add(egui::Spinner::new().size(10.0));
                        ui.add(small(text));
                    });
                })
                .response;
            let response = if reopen.is_some() {
                response
                    .interact(egui::Sense::click())
                    .on_hover_cursor(egui::CursorIcon::PointingHand)
                    .on_hover_text("Show progress")
            } else {
                response
            };
            if response.clicked() {
                reopen.clone()
            } else {
                None
            }
        }
    }
}

/// The card menu (⋯ button and right-click, spec §2.2). Disabled items stay visible
/// with their reason.
fn card_menu(
    ui: &mut egui::Ui,
    index: usize,
    instance: &InstanceProfile,
    info: &CardInfo,
) -> Option<CardAction> {
    let status = &info.status;
    let reason = |action| disabled_reason(action, &instance.name, status);
    let mut action = None;
    let mut item = |ui: &mut egui::Ui, text: RichText, why: Option<String>, chosen: CardAction| {
        let response = ui.add_enabled(why.is_none(), egui::Button::new(text));
        if response.clicked() {
            action = Some(chosen);
            ui.close();
        }
        if let Some(why) = why {
            response.on_disabled_hover_text(why);
        }
    };
    item(
        ui,
        RichText::new("Open folder"),
        reason(InstanceAction::OpenFolder),
        CardAction::OpenFolder(info.folder.clone()),
    );
    item(
        ui,
        RichText::new("Edit…"),
        reason(InstanceAction::Rename),
        CardAction::Edit(index),
    );
    item(
        ui,
        RichText::new("Duplicate…"),
        reason(InstanceAction::Duplicate),
        CardAction::Duplicate(index),
    );
    item(
        ui,
        RichText::new("Export…"),
        reason(InstanceAction::Export),
        CardAction::Export(index),
    );
    ui.separator();
    let delete_action = if status.folder_missing {
        InstanceAction::RemoveFromList
    } else {
        InstanceAction::Delete
    };
    item(
        ui,
        RichText::new("Delete…").color(super::dialogs::DANGER),
        reason(delete_action),
        CardAction::Delete(index),
    );
    action
}

/// Installs the shared Minecraft files for `version` and, unless Vanilla, the loader:
/// the same steps Create runs, reused by Edit's "Save and install". Never touches an
/// instance folder. Progress goes to `events` as creation stages and messages.
pub(super) fn install_game_files(
    paths: &ferrite_launcher::core::paths::AppPaths,
    version: &str,
    loader: ModLoader,
    events: &dyn Fn(InstanceCreationEvent),
) -> Result<(), String> {
    events(InstanceCreationEvent::Stage(
        InstanceCreationStage::DownloadingMinecraft,
    ));
    crate::minecraft::install_version_with_progress(paths, version, |message| {
        events(InstanceCreationEvent::DownloadProgress(message.into()));
    })
    .map_err(|error| format!("Failed to download Minecraft: {error}"))?;
    if loader != ModLoader::Vanilla {
        events(InstanceCreationEvent::Stage(
            InstanceCreationStage::InstallingLoader,
        ));
        // Loader backends repeat the vanilla install, reusing cached downloads.
        crate::loaders::install(paths, version, loader)
            .map_err(|error| format!("Failed to install {}: {error}", loader.label()))?;
    }
    Ok(())
}

/// The "Mod loader" ComboBox used by Create and generic Import. Quilt reads
/// "Quilt (experimental)" with a muted note once picked; it's never blocked and the
/// stored value stays the plain label.
pub(super) fn loader_picker(ui: &mut egui::Ui, selected_loader: &mut String, muted: Color32) {
    let selected_text = ModLoader::from_label(selected_loader)
        .map(ModLoader::picker_label)
        .unwrap_or(selected_loader.as_str())
        .to_owned();
    egui::ComboBox::from_label("Mod loader")
        .selected_text(selected_text)
        .show_ui(ui, |ui| {
            for loader in ModLoader::ALL {
                ui.selectable_value(
                    selected_loader,
                    loader.label().to_owned(),
                    loader.picker_label(),
                );
            }
        });
    if ModLoader::from_label(selected_loader) == Some(ModLoader::Quilt) {
        ui.label(RichText::new("Quilt support is experimental.").color(muted));
    }
}
