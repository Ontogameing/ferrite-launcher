//! Instance lifecycle, launch controls, and pack import/export orchestration.
//!
//! Long installs and archive operations run on workers with owned profiles/options.
//! The UI thread polls progress and alone mutates the profile list or saves its index,
//! preserving a clear commit point between filesystem preparation and visible state.

use super::{
    Ferrite, InstanceCreationEvent, InstanceCreationStage, PackImportOutcome, PackTaskEvent, Page,
    page_heading,
};
use crate::instances::InstanceProfile;
use crate::loaders::ModLoader;
use crate::packs::{ExportOptions, ImportOptions, PackFormat, PackTarget};
use eframe::egui::{self, Color32, RichText};
use ferrite_launcher::core::activity::{
    GlobalBusy, InstanceAction, InstanceStatus, disabled_reason,
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
        if self.instance_creation_task.is_some() || self.pack_busy() {
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
                        let _ = sender.send(InstanceCreationEvent::Stage(
                            InstanceCreationStage::DownloadingMinecraft,
                        ));
                        crate::minecraft::install_version_with_progress(
                            &paths,
                            &profile.version,
                            |message| {
                                let _ = sender
                                    .send(InstanceCreationEvent::DownloadProgress(message.into()));
                            },
                        )
                        .map_err(|error| format!("Failed to download Minecraft: {error}"))?;
                        if loader != ModLoader::Vanilla {
                            let _ = sender.send(InstanceCreationEvent::Stage(
                                InstanceCreationStage::InstallingLoader,
                            ));
                            // Loader backends repeat the vanilla install, reusing cached downloads.
                            crate::loaders::install(&paths, &profile.version, loader).map_err(
                                |error| format!("Failed to install {}: {error}", loader.label()),
                            )?;
                        }
                        Ok(())
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

    /// Imports and installs an archive on a worker, returning an uncommitted profile.
    ///
    /// Existing profiles and form options are cloned into the `'static` worker so it
    /// never borrows [`Ferrite`]. The pack subsystem stages validation internally, then
    /// publishes files to the final game directory before returning. If the UI cannot
    /// persist the profile index, dropping the outcome removes those published files.
    pub(super) fn start_pack_import(&mut self) {
        if self.pack_busy()
            || self.instance_creation_task.is_some()
            || self.mod_task.is_some()
            || self.pending_uninstall.is_some()
            || crate::minecraft::is_running()
        {
            self.pack_status = Some("Stop Minecraft and finish other instance work first.".into());
            return;
        }
        let source = PathBuf::from(self.pack_path.trim());
        if self.pack_path.trim().is_empty() || !source.is_file() {
            self.pack_status = Some("Choose an existing pack archive path.".into());
            return;
        }
        let Some(generic_loader) = self.selected_loader() else {
            self.pack_status = Some("Select a valid loader for generic ZIP imports.".into());
            return;
        };
        let generic_target = PackTarget {
            minecraft_version: self.selected_version.clone(),
            loader: generic_loader,
            loader_version: None,
        };
        let requested_name = self.pack_name.trim().to_owned();
        // The snapshot makes naming deterministic without sharing the live profile list.
        let existing = self.instances.clone();
        let skipped = self.skipped_instances.clone();
        let include_optional = self.pack_include_optional;
        let curseforge_api_key = std::env::var("FERRITE_CURSEFORGE_API_KEY").ok();
        let paths = self.paths.clone();
        let (sender, receiver) = mpsc::channel();
        let event_sender = sender.clone();
        let worker = std::thread::Builder::new()
            .name("pack-import".to_owned())
            .spawn(move || {
                let result = (|| -> Result<PackImportOutcome, String> {
                    let preview = crate::packs::preview(&source, curseforge_api_key.as_deref())
                        .map_err(|error| error.to_string())?;
                    if let Some(crate::packs::ImportBlocker::NeedsCurseForgeKey { mods }) =
                        preview.blocker
                    {
                        return Err(format!(
                            "This CurseForge pack lists {mods} mods that have to be downloaded \
                             from CurseForge, which needs an API key Ferrite doesn't have. \
                             Ferrite can't import it yet."
                        ));
                    }
                    eprintln!(
                        "Ferrite: importing {} ({} files, {} mods, {} optional files, worlds: {})",
                        source.display(),
                        preview.archive_files,
                        preview.mod_count,
                        preview.optional_files,
                        preview.has_worlds
                    );
                    let info = preview.info;
                    let target = info.target.clone().unwrap_or(generic_target);
                    let base_name = if requested_name.is_empty() {
                        info.name.trim().to_owned()
                    } else {
                        requested_name
                    };
                    if base_name.is_empty() {
                        return Err("The imported instance needs a name.".into());
                    }
                    // Names are unique case-insensitively; the folder is allocated
                    // separately (skipped entries and folders on disk are never reused).
                    let name = crate::instances::suggest_name(&existing, &base_name);
                    let profile = crate::instances::new_instance_profile(
                        &paths,
                        &name,
                        &target.minecraft_version,
                        target.loader.label(),
                        &existing,
                        &skipped,
                    )
                    .map_err(|error| format!("Cannot create instance: {error}"))?;
                    std::fs::create_dir_all(paths.instances_dir())
                        .map_err(|error| format!("Failed to prepare instance storage: {error}"))?;
                    let options = ImportOptions {
                        generic_target: Some(target.clone()),
                        include_optional_modrinth_files: include_optional,
                        curseforge_api_key,
                        ..ImportOptions::default()
                    };
                    let report = crate::packs::import(&source, profile.game_dir(&paths), &options, |step| {
                        let _ = event_sender.send(PackTaskEvent::Progress(step.to_owned()));
                    })
                    .map_err(|error| error.to_string())?;
                    if report.info.target.as_ref() != Some(&target) {
                        let _ = crate::instances::delete_game_dir(&paths, &profile);
                        return Err("The pack changed while it was being imported; no instance was kept.".into());
                    }

                    let install = (|| -> Result<(), String> {
                        let _ = event_sender.send(PackTaskEvent::Progress(
                            "Installing Minecraft files...".into(),
                        ));
                        crate::minecraft::install_version_with_progress(
                            &paths,
                            &target.minecraft_version,
                            |message| {
                                let _ = event_sender
                                    .send(PackTaskEvent::Progress(message.to_owned()));
                            },
                        )
                        .map_err(|error| format!("Failed to install Minecraft: {error}"))?;
                        if target.loader != ModLoader::Vanilla {
                            let _ = event_sender.send(PackTaskEvent::Progress(format!(
                                "Installing {}...",
                                target.loader.label()
                            )));
                            crate::loaders::install_version(
                                &paths,
                                &target.minecraft_version,
                                target.loader,
                                target.loader_version.as_deref(),
                            )
                            .map_err(|error| {
                                format!("Failed to install {}: {error}", target.loader.label())
                            })?;
                            if let Some(requested) = target.loader_version.as_deref() {
                                let installed = crate::loaders::installed_loader_version(
                                    &paths,
                                    &target.minecraft_version,
                                    target.loader,
                                );
                                if installed.as_deref() != Some(requested) {
                                    return Err(format!(
                                        "Pack requires {} {requested}, but Ferrite installed {}. Exact loader-version installation is required for this pack.",
                                        target.loader.label(),
                                        installed.as_deref().unwrap_or("an unknown version")
                                    ));
                                }
                            }
                        }
                        Ok(())
                    })();
                    if let Err(error) = install {
                        let _ = crate::instances::delete_game_dir(&paths, &profile);
                        return Err(error);
                    }
                    Ok(PackImportOutcome {
                        paths: paths.clone(),
                        profile,
                        files: report.files_written,
                        bytes: report.bytes_written,
                        warnings: report.warnings,
                        committed: false,
                    })
                })();
                let _ = sender.send(PackTaskEvent::Imported(result.map(Box::new)));
            });
        match worker {
            Ok(_) => {
                self.pack_task = Some(receiver);
                self.pack_status = Some("Inspecting pack...".into());
            }
            Err(error) => self.pack_status = Some(format!("Failed to start import: {error}")),
        }
    }

    /// Captures the selected profile and export options for one background archive write.
    /// Opens the native Save-As dialog and, unless cancelled, starts the export.
    ///
    /// The dialog itself asks "Replace?" for an existing file, so its answer is an
    /// explicit overwrite confirmation. If appending the extension produced a different
    /// name, that name was never confirmed and an existing file there is refused.
    pub(super) fn choose_export_path_and_start(&mut self) {
        let Some(profile) = self.selected_instance().cloned() else {
            self.pack_status = Some("Select an instance to export.".into());
            return;
        };
        let format = self.pack_format;
        let base = if self.pack_name.trim().is_empty() {
            profile.name.clone()
        } else {
            self.pack_name.trim().to_owned()
        };
        let default_name = crate::packs::with_pack_extension(
            std::path::Path::new(&sanitize_file_name(&base)),
            format,
        );
        let mut dialog = rfd::FileDialog::new()
            .set_title(format!("Export {}", profile.name))
            .add_filter(format.label(), &[format.extension()])
            .set_file_name(default_name.to_string_lossy());
        if let Some(folder) = directories::UserDirs::new().and_then(|dirs| {
            dirs.download_dir()
                .map(std::path::Path::to_path_buf)
                .or_else(|| Some(dirs.home_dir().to_path_buf()))
        }) {
            dialog = dialog.set_directory(folder);
        }
        let Some(chosen) = dialog.save_file() else {
            return;
        };
        let output = crate::packs::with_pack_extension(&chosen, format);
        let confirmed_by_dialog = output == chosen;
        self.start_pack_export(output, confirmed_by_dialog);
    }

    pub(super) fn start_pack_export(&mut self, output: PathBuf, replace_existing: bool) {
        if self.pack_busy()
            || self.instance_creation_task.is_some()
            || self.mod_task.is_some()
            || self.pending_uninstall.is_some()
        {
            self.pack_status = Some("Finish other instance work first.".into());
            return;
        }
        let Some(profile) = self.selected_instance().cloned() else {
            self.pack_status = Some("Select an instance to export.".into());
            return;
        };
        // Only this instance's own game blocks its export (checked when it starts).
        if let Some(reason) = disabled_reason(
            InstanceAction::Export,
            &profile.name,
            &self.instance_status(&profile),
        ) {
            self.pack_status = Some(reason);
            return;
        }
        if !output.is_absolute() {
            self.pack_status = Some("Choose where to save the pack.".into());
            return;
        }
        let output_display = output.display().to_string();
        let options = ExportOptions {
            format: self.pack_format,
            name: if self.pack_name.trim().is_empty() {
                profile.name.clone()
            } else {
                self.pack_name.trim().to_owned()
            },
            version: (!self.pack_version.trim().is_empty())
                .then(|| self.pack_version.trim().to_owned()),
            summary: None,
            loader_version: (!self.pack_loader_version.trim().is_empty())
                .then(|| self.pack_loader_version.trim().to_owned()),
            include_worlds: self.pack_include_worlds,
            replace_existing,
        };
        let paths = self.paths.clone();
        let (sender, receiver) = mpsc::channel();
        let event_sender = sender.clone();
        let worker = std::thread::Builder::new()
            .name("pack-export".to_owned())
            .spawn(move || {
                let result = crate::packs::export(&paths, &profile, &output, &options, |step| {
                    let _ = event_sender.send(PackTaskEvent::Progress(step.to_owned()));
                })
                .map(|()| output_display)
                .map_err(|error| export_error_message(&error));
                let _ = sender.send(PackTaskEvent::Exported(result));
            });
        match worker {
            Ok(_) => {
                self.pack_task = Some(receiver);
                self.pack_status = Some("Preparing export...".into());
            }
            Err(error) => self.pack_status = Some(format!("Failed to start export: {error}")),
        }
    }

    /// Drains pack progress and performs the import's final profile-list commit.
    ///
    /// Marking an outcome committed only after `instances::save` succeeds transfers
    /// cleanup responsibility away from its [`Drop`] rollback guard.
    pub(super) fn poll_pack_task(&mut self) {
        loop {
            let Some(receiver) = &self.pack_task else {
                return;
            };
            match receiver.try_recv() {
                Ok(PackTaskEvent::Progress(message)) => self.pack_status = Some(message),
                Ok(PackTaskEvent::Imported(result)) => {
                    self.pack_task = None;
                    match result {
                        Ok(mut outcome) => {
                            let name = outcome.profile.name.clone();
                            self.instances.push(outcome.profile.clone());
                            if let Err(error) = crate::instances::save(
                                &self.paths,
                                &self.instances,
                                &self.skipped_instances,
                            ) {
                                self.instances.pop();
                                self.pack_status = Some(format!(
                                    "Imported files but could not save the instance: {error}"
                                ));
                                return;
                            }
                            outcome.committed = true;
                            self.selected_instance = Some(self.instances.len() - 1);
                            let mib = outcome.bytes as f64 / (1024.0 * 1024.0);
                            let warning = if outcome.warnings.is_empty() {
                                String::new()
                            } else {
                                format!(" Warnings: {}", outcome.warnings.join(" "))
                            };
                            let message = format!(
                                "Imported '{name}' ({} files, {mib:.1} MiB).{warning}",
                                outcome.files
                            );
                            self.pack_status = Some(message.clone());
                            self.running_text = message;
                            self.import_pack_open = false;
                        }
                        Err(error) => {
                            self.pack_status = Some(format!("Import failed: {error}"));
                            self.running_text = format!("Import failed: {error}");
                        }
                    }
                    return;
                }
                Ok(PackTaskEvent::Exported(result)) => {
                    self.pack_task = None;
                    let message = match result {
                        Ok(path) => {
                            self.export_pack_open = false;
                            format!("Exported instance to {path}.")
                        }
                        Err(error) => format!("Export failed: {error}"),
                    };
                    self.pack_status = Some(message.clone());
                    self.running_text = message;
                    return;
                }
                Err(TryRecvError::Disconnected) => {
                    self.pack_task = None;
                    let message = "The import/export worker stopped unexpectedly.".to_owned();
                    self.pack_status = Some(message.clone());
                    self.running_text = message;
                    return;
                }
                Err(TryRecvError::Empty) => return,
            }
        }
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

    /// Draws profile cards and executes at most one deferred card action afterward.
    pub(super) fn instances_page(&mut self, ui: &mut egui::Ui) {
        let muted = self.muted_color();
        page_heading(ui, "Instances", "Manage your Minecraft profiles.");
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            if ui
                .add_enabled(
                    !self.pack_busy(),
                    egui::Button::new("＋ Create instance").fill(self.accent_color()),
                )
                .clicked()
            {
                self.create_instance_open = true;
            }
            if ui
                .add_enabled(!self.pack_busy(), egui::Button::new("Import pack"))
                .clicked()
            {
                self.pack_path.clear();
                self.pack_name.clear();
                self.pack_status = None;
                self.import_pack_open = true;
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
        let mut launch = false;
        let mut remove = None;
        let mut export = false;
        let mut open_folder = None;
        let statuses: Vec<InstanceStatus> = self
            .instances
            .iter()
            .map(|instance| self.instance_status(instance))
            .collect();
        let mut mods = None;
        let mod_idle =
            self.mod_task.is_none() && self.pending_uninstall.is_none() && !self.pack_busy();
        egui::ScrollArea::vertical().show(ui, |ui| {
            for (index, instance) in self.instances.iter().enumerate() {
                let selected = self.selected_instance == Some(index);
                egui::Frame::new()
                    .fill(if selected {
                        if self.ui_settings.appearance.theme == "light" {
                            Color32::from_rgb(255, 240, 232)
                        } else {
                            Color32::from_rgb(43, 39, 44)
                        }
                    } else {
                        self.card_color()
                    })
                    .stroke(egui::Stroke::new(
                        if selected { 1.5 } else { 1.0 },
                        if selected {
                            self.accent_color()
                        } else {
                            Color32::from_rgb(50, 55, 64)
                        },
                    ))
                    .corner_radius(self.ui_settings.appearance.corner_radius)
                    .inner_margin(18.0)
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        // Metadata and actions use separate rows so long names do
                        // not push buttons beyond a narrow viewport.
                        if ui
                            .selectable_label(
                                selected,
                                RichText::new(&instance.name).size(20.0).strong(),
                            )
                            .clicked()
                        {
                            self.selected_instance = Some(index);
                        }
                        ui.label(
                            RichText::new(format!(
                                "Minecraft {}  •  {}",
                                instance.version, instance.loader
                            ))
                            .color(muted),
                        );
                        let status = statuses[index];
                        let reason = |action| disabled_reason(action, &instance.name, &status);
                        // Draws one action button, disabled with its reason when needed.
                        let button = |ui: &mut egui::Ui, label: &str, action, extra: bool| {
                            let reason = reason(action);
                            let response =
                                ui.add_enabled(extra && reason.is_none(), egui::Button::new(label));
                            let clicked = response.clicked();
                            if let Some(reason) = reason {
                                response.on_disabled_hover_text(reason);
                            }
                            clicked
                        };
                        ui.horizontal(|ui| {
                            if button(ui, "Play", InstanceAction::Play, mod_idle) {
                                self.selected_instance = Some(index);
                                launch = true;
                            }
                            if button(ui, "Mods", InstanceAction::Mods, mod_idle) {
                                mods = Some(instance.clone());
                            }
                            if button(ui, "Open folder", InstanceAction::OpenFolder, true) {
                                open_folder = Some(instance.game_dir(&self.paths));
                            }
                            if button(ui, "Export", InstanceAction::Export, true) {
                                self.selected_instance = Some(index);
                                export = true;
                            }
                            let delete_action = if status.folder_missing {
                                InstanceAction::RemoveFromList
                            } else {
                                InstanceAction::Delete
                            };
                            if button(ui, "Remove", delete_action, true) {
                                self.selected_instance = Some(index);
                                remove = Some(instance.directory().clone());
                            }
                        });
                    });
                ui.add_space(10.0);
            }
        });
        if let Some(target) = mods {
            self.set_mod_target(target);
            self.show_installed = true;
            self.current_page = Page::Mods;
            self.local_mod_task(None);
        }
        if launch {
            self.launch_selected();
        } else if export {
            if let Some(name) = self
                .selected_instance()
                .map(|instance| instance.name.clone())
            {
                self.pack_name = name;
            }
            self.pack_format = PackFormat::Ferrite;
            self.pack_include_worlds = true;
            self.pack_loader_version.clear();
            self.pack_status = None;
            self.export_pack_open = true;
        } else if let Some(directory) = remove {
            self.open_remove_dialog(directory);
        } else if let Some(folder) = open_folder
            && let Err(error) = crate::config::open_folder(&folder)
        {
            self.running_text = format!("Couldn't open {}: {error}", folder.display());
        }
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

    /// Draws import form/progress and starts requested work after the window closure.
    pub(super) fn import_pack_window(&mut self, context: &egui::Context) {
        if !self.import_pack_open {
            return;
        }
        let mut open = self.import_pack_open;
        let mut import_requested = false;
        egui::Window::new("Import instance pack")
            .open(&mut open)
            .collapsible(false)
            .default_width(500.0)
            .show(context, |ui| {
                ui.label("Supported: Ferrite, Modrinth, Prism/MultiMC, CurseForge, and generic ZIP archives.");
                ui.label("Archive contents are validated and staged before the instance is committed.");
                ui.add_space(8.0);
                ui.add_enabled_ui(!self.pack_busy(), |ui| {
                    ui.label("Pack archive");
                    ui.horizontal(|ui| {
                        if ui.button("Choose Pack File…").clicked()
                            && let Some(path) = rfd::FileDialog::new()
                                .set_title("Choose a Minecraft instance pack")
                                .add_filter(
                                    "Minecraft instance packs",
                                    &["ferritepack", "mrpack", "zip", "lcpack"],
                                )
                                .pick_file()
                        {
                            self.pack_path = path.display().to_string();
                            self.pack_status = None;
                        }
                        if !self.pack_path.is_empty() && ui.button("Clear").clicked() {
                            self.pack_path.clear();
                        }
                    });
                    if self.pack_path.is_empty() {
                        ui.label(RichText::new("No pack selected.").color(self.muted_color()));
                    } else {
                        ui.label(RichText::new(&self.pack_path).monospace());
                    }
                    ui.label("Instance name override (optional)");
                    ui.text_edit_singleline(&mut self.pack_name);
                    ui.separator();
                    ui.label("Fallback metadata for generic ZIP files");
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
                    ui.checkbox(
                        &mut self.pack_include_optional,
                        "Install optional client files from Modrinth packs",
                    );
                    ui.label("CurseForge packs containing indexed mods require FERRITE_CURSEFORGE_API_KEY.");
                    ui.add_space(8.0);
                    import_requested = ui
                        .add_enabled(
                            !self.pack_path.trim().is_empty(),
                            egui::Button::new("Import and install").fill(self.accent_color()),
                        )
                        .clicked();
                });
                if self.pack_busy() {
                    ui.spinner();
                }
                if let Some(status) = &self.pack_status {
                    ui.label(status);
                }
            });
        self.import_pack_open = open;
        if import_requested {
            self.start_pack_import();
        }
    }

    /// Draws export options, including format-dependent safety and metadata controls.
    pub(super) fn export_pack_window(&mut self, context: &egui::Context) {
        if !self.export_pack_open {
            return;
        }
        let mut open = self.export_pack_open;
        let mut export_requested = false;
        egui::Window::new("Export instance")
            .open(&mut open)
            .collapsible(false)
            .default_width(500.0)
            .show(context, |ui| {
                ui.add_enabled_ui(!self.pack_busy(), |ui| {
                    ui.label("Pack name");
                    ui.text_edit_singleline(&mut self.pack_name);
                    ui.label("Pack version");
                    ui.text_edit_singleline(&mut self.pack_version);
                    let previous = self.pack_format;
                    egui::ComboBox::from_label("Format")
                        .selected_text(self.pack_format.label())
                        .show_ui(ui, |ui| {
                            for format in PackFormat::ALL {
                                ui.selectable_value(&mut self.pack_format, format, format.label());
                            }
                        });
                    if previous != self.pack_format {
                        self.pack_include_worlds = matches!(
                            self.pack_format,
                            PackFormat::Ferrite | PackFormat::GenericZip
                        );
                    }
                    if self.pack_format == PackFormat::Lunar {
                        ui.label(RichText::new("Direct .lcpack export is unavailable because Lunar does not publish its schema. Lunar can import the Modrinth and CurseForge formats.").color(self.muted_color()));
                    }
                    let modded = self
                        .selected_instance()
                        .is_some_and(|profile| profile.loader != "Vanilla");
                    if modded
                        && matches!(
                            self.pack_format,
                            PackFormat::Modrinth | PackFormat::Prism | PackFormat::CurseForge
                        )
                    {
                        ui.label("Exact loader version required by this format");
                        ui.text_edit_singleline(&mut self.pack_loader_version);
                    }
                    ui.checkbox(&mut self.pack_include_worlds, "Include worlds/saves");
                    if matches!(
                        self.pack_format,
                        PackFormat::Modrinth | PackFormat::Prism | PackFormat::CurseForge
                    ) && self.pack_include_worlds
                    {
                        ui.label(RichText::new("Warning: distributable packs normally exclude private worlds.").color(self.muted_color()));
                    }
                    if matches!(self.pack_format, PackFormat::Modrinth | PackFormat::CurseForge) {
                        ui.label("This is an override-based export: Ferrite does not invent provider project IDs or download provenance for local JARs.");
                    }
                    ui.add_space(8.0);
                    export_requested = ui
                        .add_enabled(
                            self.pack_format != PackFormat::Lunar,
                            egui::Button::new("Export…").fill(self.accent_color()),
                        )
                        .clicked();
                });
                if self.pack_busy() {
                    ui.spinner();
                }
                if let Some(status) = &self.pack_status {
                    ui.label(status);
                }
            });
        self.export_pack_open = open;
        if export_requested {
            self.choose_export_path_and_start();
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

/// The "Mod loader" ComboBox used by Create and generic Import. Quilt reads
/// "Quilt (experimental)" with a muted note once picked; it's never blocked and the
/// stored value stays the plain label.
fn loader_picker(ui: &mut egui::Ui, selected_loader: &mut String, muted: Color32) {
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

/// Plain-words export failure, with the raw error appended for details.
fn export_error_message(error: &crate::packs::PackError) -> String {
    use crate::packs::ExportFailureKind;
    let plain = match error.export_failure_kind() {
        ExportFailureKind::FileTooLarge => {
            "The pack is larger than 4 GiB, which this drive's format (FAT32) can't store in \
             one file. Save it to another drive, or reformat this one as exFAT."
        }
        ExportFailureKind::NoSpace => "There isn't enough space on that drive.",
        ExportFailureKind::PermissionDenied => {
            "Ferrite can't save to that folder. Choose another one."
        }
        ExportFailureKind::AlreadyExists => {
            "A file with that name already exists. Choose another name."
        }
        ExportFailureKind::Other => return error.to_string(),
    };
    format!("{plain} ({error})")
}

/// Default export file name: replaces `< > : " / \ | ? *`, control characters, and
/// trailing dots or spaces with `-`; dots inside the name are kept (`Pack 1.20`).
fn sanitize_file_name(name: &str) -> String {
    let replaced: String = name
        .trim()
        .chars()
        .map(|c| {
            if matches!(c, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*') || c.is_control() {
                '-'
            } else {
                c
            }
        })
        .collect();
    let kept = replaced.trim_end_matches(['.', ' ']);
    let mut result = kept.to_owned();
    result.extend(std::iter::repeat_n('-', replaced.len() - kept.len()));
    if result.is_empty() {
        "pack".to_owned()
    } else {
        result
    }
}

#[cfg(test)]
mod tests {
    use super::sanitize_file_name;

    #[test]
    fn export_file_names_keep_dots_and_drop_unsafe_characters() {
        assert_eq!(sanitize_file_name("Pack 1.20"), "Pack 1.20");
        assert_eq!(sanitize_file_name("a/b:c?"), "a-b-c-");
        assert_eq!(sanitize_file_name("Trailing."), "Trailing-");
        assert_eq!(sanitize_file_name("   "), "pack");
    }
}
