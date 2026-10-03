//! Import (Stage 2 UI spec §7) and Export (§6) windows and their workers.
//!
//! Import runs in three steps: choose a file, preview it (`packs::preview` on a
//! worker), then install on the serialized pack worker. The new profile is committed
//! on the UI thread with `instances::commit_new_instance`. Export picks the file with
//! the native Save-As dialog and writes it on the same worker.

use super::dialogs::{
    self, button_row, enter_pressed, escape_pressed, fact, muted, name_error_text, name_field,
    primary_button, quilt_badge,
};
use super::instances::loader_picker;
use super::startup::details;
use super::{Ferrite, PackImportOutcome, PackTaskEvent};
use crate::instances::InstanceProfile;
use crate::loaders::ModLoader;
use crate::packs::{
    ExportOptions, ImportBlocker, ImportOptions, PackError, PackFormat, PackPreview, PackTarget,
};
use eframe::egui::{self, RichText};
use ferrite_launcher::core::activity::{InstanceAction, disabled_reason};
use ferrite_launcher::core::instances as core_instances;
use ferrite_launcher::core::migration::format_size;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, TryRecvError};

/// The pack file types the Import file picker offers.
const PACK_EXTENSIONS: [&str; 4] = ["ferritepack", "mrpack", "zip", "lcpack"];

/// The previewed pack and the choices made on the preview step.
pub(super) struct ImportChoice {
    path: PathBuf,
    preview: PackPreview,
    name: String,
    /// Import-local pickers for a generic ZIP without a target.
    version: String,
    loader: String,
    include_optional: bool,
    first_frame: bool,
}

impl ImportChoice {
    /// The target this import installs: the pack's own, or the local pickers'.
    fn target(&self) -> Option<PackTarget> {
        if let Some(target) = &self.preview.info.target {
            return Some(target.clone());
        }
        Some(PackTarget {
            minecraft_version: self.version.clone(),
            loader: ModLoader::from_label(&self.loader)?,
            loader_version: None,
        })
    }

    fn pack_name(&self) -> String {
        let name = self.preview.info.name.trim();
        if name.is_empty() {
            file_stem(&self.path)
        } else {
            name.to_owned()
        }
    }
}

/// Where the Import window is.
#[derive(Default)]
pub(super) enum ImportStep {
    #[default]
    Choose,
    Reading {
        path: PathBuf,
        receiver: Receiver<Result<PackPreview, PackError>>,
    },
    Preview(Box<ImportChoice>),
    CantImport {
        reason: String,
        error: Option<String>,
    },
    Installing(Box<ImportChoice>),
    Done {
        name: String,
        warnings: Vec<String>,
    },
    Failed {
        choice: Box<ImportChoice>,
        error: String,
    },
}

/// The end of an export, shown in the Export window.
pub(super) enum ExportResult {
    Done(PathBuf),
    Failed { reason: String, error: String },
}

fn file_stem(path: &Path) -> String {
    path.file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_else(|| "Imported pack".into())
}

/// "Ferrite can't import this file." reason in plain words, plus the raw error for
/// Details when it adds something.
pub(super) fn inspect_failure(path: &Path, error: &PackError) -> (String, Option<String>) {
    let lcpack = path
        .extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("lcpack"));
    let raw = Some(error.to_string());
    if lcpack {
        return (
            "Lunar Client packs use a format Ferrite can't read. In Lunar, export the instance \
             as a Modrinth or CurseForge pack and import that file instead."
                .into(),
            None,
        );
    }
    match error {
        PackError::Zip(_) => ("It isn't a ZIP file, or it's damaged.".into(), raw),
        PackError::Json(_) | PackError::Invalid(_) => {
            ("Its pack information is missing or damaged.".into(), raw)
        }
        PackError::Security(_) => (
            "It contains file paths that could write outside the instance folder.".into(),
            raw,
        ),
        PackError::Limit(_) => ("It's bigger than Ferrite allows for a pack.".into(), raw),
        PackError::Unsupported(_) => ("Ferrite doesn't support this kind of pack.".into(), raw),
        PackError::Io(_) => ("Ferrite couldn't read the file.".into(), raw),
        _ => ("Something went wrong while reading it.".into(), raw),
    }
}

/// The NeedsCurseForgeKey text (spec §7; never mentions the environment variable).
pub(super) fn curseforge_key_text(mods: usize) -> String {
    format!(
        "This CurseForge pack lists {mods} mods that have to be downloaded from CurseForge, \
         which needs an API key Ferrite doesn't have. Ferrite can't import it yet."
    )
}

/// Plain-words export failure and the raw error for Details (spec §6.5).
pub(super) fn export_failure(error: &PackError) -> (String, String) {
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
        ExportFailureKind::Other => "Ferrite couldn't export the pack.",
    };
    (plain.to_owned(), error.to_string())
}

/// Default export file name: replaces `< > : " / \ | ? *`, control characters, and
/// trailing dots or spaces with `-`; dots inside the name are kept (`Pack 1.20`).
pub(super) fn sanitize_file_name(name: &str) -> String {
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

/// Whether `format` records the exact loader version (spec §6.4).
fn needs_loader_version(format: PackFormat, loader: &str) -> bool {
    loader != "Vanilla"
        && matches!(
            format,
            PackFormat::Modrinth | PackFormat::Prism | PackFormat::CurseForge
        )
}

/// Why Export… is disabled, if it is.
pub(super) fn export_block(
    format: PackFormat,
    loader: &str,
    loader_version: &str,
) -> Option<String> {
    if format == PackFormat::Lunar {
        return Some("Ferrite can't write Lunar Client packs.".into());
    }
    if needs_loader_version(format, loader) && loader_version.trim().is_empty() {
        return Some(format!("Enter the {loader} version this pack needs."));
    }
    None
}

impl Ferrite {
    /// Starts reading `path` for the preview.
    fn start_reading_pack(&mut self, path: PathBuf) {
        let worker_path = path.clone();
        let key = std::env::var("FERRITE_CURSEFORGE_API_KEY").ok();
        let (sender, receiver) = mpsc::channel();
        let spawned = std::thread::Builder::new()
            .name("pack-preview".into())
            .spawn(move || {
                let _ = sender.send(crate::packs::preview(&worker_path, key.as_deref()));
            });
        self.import_step = match spawned {
            Ok(_) => ImportStep::Reading { path, receiver },
            Err(error) => ImportStep::CantImport {
                reason: "Ferrite couldn't start reading the file.".into(),
                error: Some(error.to_string()),
            },
        };
    }

    /// Opens the file picker; a chosen file goes straight to reading.
    fn choose_pack_file(&mut self) {
        if let Some(path) = rfd::FileDialog::new()
            .set_title("Choose a pack to import")
            .add_filter("Minecraft packs", &PACK_EXTENSIONS)
            .pick_file()
        {
            self.start_reading_pack(path);
        }
    }

    /// Polls the preview worker.
    pub(super) fn poll_import_preview(&mut self) {
        let ImportStep::Reading { path, receiver } = &self.import_step else {
            return;
        };
        let result = match receiver.try_recv() {
            Ok(result) => result,
            Err(TryRecvError::Empty) => return,
            Err(TryRecvError::Disconnected) => {
                self.import_step = ImportStep::CantImport {
                    reason: "Something went wrong while reading it.".into(),
                    error: Some("the preview worker stopped unexpectedly".into()),
                };
                return;
            }
        };
        let path = path.clone();
        self.import_step = match result {
            Ok(preview) => {
                let base = if preview.info.name.trim().is_empty() {
                    file_stem(&path)
                } else {
                    preview.info.name.trim().to_owned()
                };
                ImportStep::Preview(Box::new(ImportChoice {
                    name: core_instances::suggest_name(&self.instances, &base),
                    version: self.versions.first().cloned().unwrap_or_default(),
                    loader: "Vanilla".into(),
                    include_optional: false,
                    first_frame: true,
                    path,
                    preview,
                }))
            }
            Err(error) => {
                let (reason, error) = inspect_failure(&path, &error);
                ImportStep::CantImport { reason, error }
            }
        };
    }

    /// Whether the preview worker is running.
    pub(super) fn import_reading(&self) -> bool {
        matches!(self.import_step, ImportStep::Reading { .. })
    }

    /// Installs the previewed pack on the pack worker with the chosen name.
    fn start_pack_import(&mut self, choice: Box<ImportChoice>) {
        let lock = self.create_import_lock().or_else(|| {
            if self.instance_creation_task.is_some() {
                Some("Wait for the new instance to finish installing.".to_owned())
            } else if self.mod_task.is_some() || self.pending_uninstall.is_some() {
                Some("Wait for mod changes to finish.".to_owned())
            } else {
                None
            }
        });
        if let Some(reason) = lock {
            self.import_step = ImportStep::Failed {
                choice,
                error: reason,
            };
            return;
        }
        let Some(target) = choice.target() else {
            self.import_step = ImportStep::Failed {
                choice,
                error: "Choose a Minecraft version and loader.".into(),
            };
            return;
        };
        let name = match core_instances::validate_instance_name(&choice.name, &self.instances, None)
        {
            Ok(name) => name,
            Err(error) => {
                self.import_step = ImportStep::Failed {
                    error: name_error_text(&error),
                    choice,
                };
                return;
            }
        };
        let profile = match crate::instances::new_instance_profile(
            &self.paths,
            &name,
            &target.minecraft_version,
            target.loader.label(),
            &self.instances,
            &self.skipped_instances,
        ) {
            Ok(profile) => profile,
            Err(error) => {
                self.import_step = ImportStep::Failed {
                    choice,
                    error: format!("Cannot create instance: {error}"),
                };
                return;
            }
        };
        let source = choice.path.clone();
        let include_optional = choice.include_optional;
        let curseforge_api_key = std::env::var("FERRITE_CURSEFORGE_API_KEY").ok();
        let paths = self.paths.clone();
        let (sender, receiver) = mpsc::channel();
        let event_sender = sender.clone();
        let worker = std::thread::Builder::new()
            .name("pack-import".to_owned())
            .spawn(move || {
                let result = import_worker(
                    &paths,
                    &source,
                    profile,
                    target,
                    include_optional,
                    curseforge_api_key,
                    &|step| {
                        let _ = event_sender.send(PackTaskEvent::Progress(step.to_owned()));
                    },
                );
                let _ = sender.send(PackTaskEvent::Imported(result.map(Box::new)));
            });
        match worker {
            Ok(_) => {
                self.pack_task = Some(receiver);
                self.pack_status = Some(format!("Importing {}…", choice.pack_name()));
                self.import_step = ImportStep::Installing(choice);
            }
            Err(error) => {
                self.import_step = ImportStep::Failed {
                    choice,
                    error: format!("Failed to start import: {error}"),
                }
            }
        }
    }

    /// Commits a finished import (UI thread) and moves the window to its result.
    pub(super) fn finish_import(&mut self, result: Result<Box<PackImportOutcome>, String>) {
        let step = std::mem::take(&mut self.import_step);
        let ImportStep::Installing(choice) = step else {
            // No window state to update (should not happen); keep the status line.
            self.import_step = step;
            if let Err(error) = result {
                self.running_text = format!("Import failed: {error}");
            }
            return;
        };
        let mut outcome = match result {
            Ok(outcome) => outcome,
            Err(error) => {
                self.import_step = ImportStep::Failed { choice, error };
                self.import_pack_open = true;
                return;
            }
        };
        let name = outcome.profile.name.clone();
        // commit_new_instance removes the folder itself when it can't keep it (and
        // leaves it alone when another entry claims it), so the guard stands down.
        let committed = core_instances::commit_new_instance(
            &self.paths,
            &mut self.instances,
            &self.skipped_instances,
            outcome.profile.clone(),
        );
        outcome.committed = true;
        match committed {
            Ok(index) => {
                self.selected_instance = Some(index);
                self.scroll_to_instance = Some(outcome.profile.directory().clone());
                self.running_text = format!("Imported {name}");
                self.pack_status = None;
                if outcome.warnings.is_empty() {
                    self.import_step = ImportStep::Choose;
                    self.import_pack_open = false;
                } else {
                    self.import_step = ImportStep::Done {
                        name,
                        warnings: std::mem::take(&mut outcome.warnings),
                    };
                    self.import_pack_open = true;
                }
            }
            Err(error) => {
                self.import_step = ImportStep::Failed {
                    choice,
                    error: format!("Couldn't save the instance list: {error}"),
                };
                self.import_pack_open = true;
            }
        }
    }

    /// Draws the Import window.
    pub(super) fn import_pack_window(&mut self, context: &egui::Context) {
        if !self.import_pack_open {
            return;
        }
        let muted_color = self.muted_color();
        let accent = self.accent_color();
        let modal_open = self.remove_dialog.is_some()
            || self.edit_dialog.is_some()
            || self.duplicate_setup.is_some();
        let lock = self.create_import_lock();
        let instances = &self.instances;
        let versions = &self.versions;
        let mut open = true;
        let mut action = None;
        let installing = matches!(
            self.import_step,
            ImportStep::Installing(_) | ImportStep::Reading { .. }
        );
        egui::Window::new("Import pack")
            .id(egui::Id::new("import-pack"))
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .default_width(480.0)
            .show(context, |ui| {
                if !installing && !modal_open && escape_pressed(ui) {
                    action = Some(ImportAction::Close);
                }
                let step_action = match &mut self.import_step {
                    ImportStep::Choose => {
                        ui.label(
                            "Ferrite can import Ferrite, Modrinth, Prism/MultiMC and CurseForge \
                             packs, and plain ZIP files.",
                        );
                        ui.add_space(6.0);
                        let button =
                            primary_button(ui, "Choose pack file…", lock.is_none(), accent);
                        let clicked = button.clicked();
                        if let Some(reason) = &lock {
                            button.on_disabled_hover_text(reason);
                        }
                        clicked.then_some(ImportAction::ChooseFile)
                    }
                    ImportStep::Reading { .. } => {
                        ui.add(egui::ProgressBar::new(0.0).animate(true));
                        ui.label("Reading the pack…");
                        None
                    }
                    ImportStep::CantImport { reason, error } => {
                        ui.heading("Ferrite can't import this file.");
                        ui.add(egui::Label::new(reason.as_str()).wrap());
                        if let Some(error) = error {
                            details(ui, "import-inspect", std::slice::from_ref(error));
                        }
                        ui.add_space(6.0);
                        button_row(
                            ui,
                            |ui| ui.button("Close").clicked().then_some(ImportAction::Close),
                            |ui| {
                                ui.button("Choose another file")
                                    .clicked()
                                    .then_some(ImportAction::ChooseFile)
                            },
                        )
                    }
                    ImportStep::Preview(choice) => preview_ui(
                        ui,
                        choice,
                        instances,
                        versions,
                        lock.as_deref(),
                        accent,
                        muted_color,
                    ),
                    ImportStep::Installing(choice) => {
                        ui.heading(choice.pack_name());
                        ui.add(egui::ProgressBar::new(0.0).animate(true));
                        let status = self.pack_status.as_deref().unwrap_or("Importing…");
                        ui.add(egui::Label::new(status).wrap());
                        None
                    }
                    ImportStep::Done { name, warnings } => {
                        ui.heading(format!("Imported {name}"));
                        for warning in warnings.iter() {
                            muted(ui, format!("• {warning}"), muted_color);
                        }
                        ui.add_space(6.0);
                        button_row(
                            ui,
                            |_| None,
                            |ui| ui.button("Done").clicked().then_some(ImportAction::Close),
                        )
                    }
                    ImportStep::Failed { choice, error } => {
                        ui.heading(format!(
                            "Couldn't import {}. Nothing was added.",
                            choice.pack_name()
                        ));
                        details(ui, "import-error", std::slice::from_ref(error));
                        ui.add_space(6.0);
                        button_row(
                            ui,
                            |ui| ui.button("Close").clicked().then_some(ImportAction::Close),
                            |ui| {
                                ui.button("Try again")
                                    .clicked()
                                    .then_some(ImportAction::Install)
                            },
                        )
                    }
                };
                if step_action.is_some() {
                    action = step_action;
                }
            });
        if !open {
            action = Some(ImportAction::Close);
        }
        match action {
            Some(ImportAction::Close) => {
                // Closing keeps a running install going; the status line shows it.
                self.import_pack_open = false;
                if !matches!(self.import_step, ImportStep::Installing(_)) {
                    self.import_step = ImportStep::Choose;
                }
            }
            Some(ImportAction::ChooseFile) => self.choose_pack_file(),
            Some(ImportAction::Install) => match std::mem::take(&mut self.import_step) {
                ImportStep::Preview(choice) | ImportStep::Failed { choice, .. } => {
                    self.start_pack_import(choice)
                }
                other => self.import_step = other,
            },
            None => {}
        }
    }

    /// Drains the pack worker: progress, the import commit, and export results.
    pub(super) fn poll_pack_task(&mut self) {
        loop {
            let Some(receiver) = &self.pack_task else {
                return;
            };
            match receiver.try_recv() {
                Ok(PackTaskEvent::Progress(message)) => self.pack_status = Some(message),
                Ok(PackTaskEvent::Imported(result)) => {
                    self.pack_task = None;
                    self.finish_import(result);
                    return;
                }
                Ok(PackTaskEvent::Exported(result)) => {
                    self.pack_task = None;
                    self.pack_status = None;
                    self.finish_export(result);
                    return;
                }
                Err(TryRecvError::Disconnected) => {
                    self.pack_task = None;
                    let message = "The import/export worker stopped unexpectedly.".to_owned();
                    if matches!(self.import_step, ImportStep::Installing(_)) {
                        self.finish_import(Err(message));
                    } else {
                        self.finish_export(Err((message.clone(), message)));
                    }
                    return;
                }
                Err(TryRecvError::Empty) => return,
            }
        }
    }

    /// Records an export's end in the window and, on success, the folder.
    fn finish_export(&mut self, result: Result<PathBuf, (String, String)>) {
        match result {
            Ok(path) => {
                self.running_text = format!("Exported to {}", path.display());
                if let Some(folder) = path.parent() {
                    self.remember_export_dir(folder);
                }
                self.export_result = Some(ExportResult::Done(path));
            }
            Err((reason, error)) => {
                self.running_text = format!("Export failed: {reason}");
                self.export_result = Some(ExportResult::Failed { reason, error });
            }
        }
        self.export_pack_open = true;
    }

    /// Saves `folder` as `last_export_dir` in `ui.toml`.
    fn remember_export_dir(&mut self, folder: &Path) {
        if self.ui_settings.last_export_dir.as_deref() == Some(folder) {
            return;
        }
        self.ui_settings.last_export_dir = Some(folder.to_path_buf());
        if let Err(error) = crate::ui_settings::save(&self.paths, &self.ui_settings) {
            eprintln!("Ferrite: couldn't remember the export folder: {error}");
        }
    }

    /// Opens the native Save-As dialog and, unless cancelled, starts the export.
    ///
    /// The dialog itself asks "Replace?" for an existing file, so its answer is an
    /// explicit overwrite confirmation. If appending the extension produced a different
    /// name, that name was never confirmed and an existing file there is refused.
    fn choose_export_path_and_start(&mut self) {
        let Some(profile) = self.selected_instance().cloned() else {
            return;
        };
        let format = self.pack_format;
        let base = if self.pack_name.trim().is_empty() {
            profile.name.clone()
        } else {
            self.pack_name.trim().to_owned()
        };
        let default_name =
            crate::packs::with_pack_extension(Path::new(&sanitize_file_name(&base)), format);
        let mut dialog = rfd::FileDialog::new()
            .set_title(format!("Export {}", profile.name))
            .add_filter(format.label(), &[format.extension()])
            .set_file_name(default_name.to_string_lossy());
        let start = self
            .ui_settings
            .last_export_dir
            .clone()
            .filter(|folder| folder.is_dir())
            .or_else(|| {
                directories::UserDirs::new().map(|dirs| {
                    dirs.download_dir()
                        .unwrap_or_else(|| dirs.home_dir())
                        .to_path_buf()
                })
            });
        if let Some(folder) = start {
            dialog = dialog.set_directory(folder);
        }
        let Some(chosen) = dialog.save_file() else {
            return;
        };
        let output = crate::packs::with_pack_extension(&chosen, format);
        let confirmed_by_dialog = output == chosen;
        self.start_pack_export(profile, output, confirmed_by_dialog);
    }

    fn start_pack_export(&mut self, profile: InstanceProfile, output: PathBuf, replace: bool) {
        if self.pack_busy() {
            return;
        }
        // Only this instance's own state blocks its export (checked when it starts).
        if let Some(reason) = disabled_reason(
            InstanceAction::Export,
            &profile.name,
            &self.instance_status(&profile),
        ) {
            self.export_result = Some(ExportResult::Failed {
                reason: reason.clone(),
                error: reason,
            });
            return;
        }
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
            replace_existing: replace,
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
                .map(|()| output)
                .map_err(|error| export_failure(&error));
                let _ = sender.send(PackTaskEvent::Exported(result));
            });
        match worker {
            Ok(_) => {
                self.export_result = None;
                self.pack_task = Some(receiver);
                self.pack_status = Some("Preparing export…".into());
            }
            Err(error) => {
                self.export_result = Some(ExportResult::Failed {
                    reason: "Ferrite couldn't start the export.".into(),
                    error: error.to_string(),
                })
            }
        }
    }

    /// Draws the Export window (spec §6).
    pub(super) fn export_pack_window(&mut self, context: &egui::Context) {
        if !self.export_pack_open {
            return;
        }
        let Some(profile) = self.selected_instance().cloned() else {
            self.export_pack_open = false;
            return;
        };
        let muted_color = self.muted_color();
        let accent = self.accent_color();
        let busy = self.pack_busy();
        let mut open = true;
        let mut action = None;
        egui::Window::new(format!("Export {}", profile.name))
            .id(egui::Id::new("export-pack"))
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .default_width(480.0)
            .show(context, |ui| {
                if busy {
                    ui.add(egui::ProgressBar::new(0.0).animate(true));
                    ui.label(self.pack_status.as_deref().unwrap_or("Exporting…"));
                    return;
                }
                match &self.export_result {
                    Some(ExportResult::Done(path)) => {
                        ui.horizontal_wrapped(|ui| {
                            ui.label("Exported to");
                            ui.add(
                                egui::Label::new(
                                    RichText::new(path.display().to_string()).monospace(),
                                )
                                .selectable(true)
                                .wrap(),
                            );
                        });
                        ui.add_space(6.0);
                        action = button_row(
                            ui,
                            |ui| {
                                ui.button("Show in folder")
                                    .clicked()
                                    .then_some(ExportAction::ShowInFolder(path.clone()))
                            },
                            |ui| ui.button("Done").clicked().then_some(ExportAction::Close),
                        );
                        return;
                    }
                    Some(ExportResult::Failed { reason, error }) => {
                        ui.add(egui::Label::new(reason.as_str()).wrap());
                        details(ui, "export-error", std::slice::from_ref(error));
                        ui.add_space(6.0);
                        action = button_row(
                            ui,
                            |ui| ui.button("Close").clicked().then_some(ExportAction::Close),
                            |ui| {
                                ui.button("Choose another location")
                                    .clicked()
                                    .then_some(ExportAction::Export)
                            },
                        );
                        return;
                    }
                    None => {}
                }
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
                    muted(
                        ui,
                        "Ferrite can't write Lunar Client packs (Lunar doesn't publish the \
                         format). Lunar can import Modrinth and CurseForge packs.",
                        muted_color,
                    );
                }
                ui.label("Pack name");
                ui.text_edit_singleline(&mut self.pack_name);
                ui.label("Pack version");
                ui.text_edit_singleline(&mut self.pack_version);
                if needs_loader_version(self.pack_format, &profile.loader) {
                    ui.horizontal(|ui| {
                        ui.label(format!("{} version", profile.loader));
                        quilt_badge(ui, &profile.loader, muted_color);
                    });
                    ui.text_edit_singleline(&mut self.pack_loader_version);
                    // Only claim a prefill when there was one (unknown stays empty).
                    let help = if self.pack_loader_version.trim().is_empty() {
                        format!(
                            "These pack formats record the exact {} version.",
                            profile.loader
                        )
                    } else {
                        format!(
                            "These pack formats record the exact {} version. Ferrite filled \
                             in the one this instance uses.",
                            profile.loader
                        )
                    };
                    muted(ui, help, muted_color);
                }
                ui.checkbox(&mut self.pack_include_worlds, "Include worlds");
                if matches!(
                    self.pack_format,
                    PackFormat::Modrinth | PackFormat::Prism | PackFormat::CurseForge
                ) && self.pack_include_worlds
                {
                    muted(
                        ui,
                        "Packs you share usually leave out worlds, which can be private.",
                        muted_color,
                    );
                }
                if matches!(
                    self.pack_format,
                    PackFormat::Modrinth | PackFormat::CurseForge
                ) {
                    muted(
                        ui,
                        "Mods are stored in the pack as files; Ferrite doesn't link them to \
                         Modrinth or CurseForge projects.",
                        muted_color,
                    );
                }
                ui.add_space(6.0);
                let block =
                    export_block(self.pack_format, &profile.loader, &self.pack_loader_version);
                action = button_row(
                    ui,
                    |ui| ui.button("Cancel").clicked().then_some(ExportAction::Close),
                    |ui| {
                        let button = primary_button(ui, "Export…", block.is_none(), accent);
                        let clicked = button.clicked();
                        if let Some(reason) = &block {
                            button.on_disabled_hover_text(reason);
                        }
                        clicked.then_some(ExportAction::Export)
                    },
                );
            });
        if !open {
            action = Some(ExportAction::Close);
        }
        match action {
            Some(ExportAction::Close) => {
                // Closing during an export keeps it going; the result reopens the window.
                self.export_pack_open = false;
                self.export_result = None;
            }
            Some(ExportAction::Export) => {
                self.export_result = None;
                self.choose_export_path_and_start();
            }
            Some(ExportAction::ShowInFolder(path)) => {
                let folder = path.parent().map(Path::to_path_buf).unwrap_or_default();
                if let Some(message) = dialogs::open_folder(&folder) {
                    self.running_text = message;
                }
            }
            None => {}
        }
    }
}

#[derive(Clone)]
enum ImportAction {
    Close,
    ChooseFile,
    Install,
}

enum ExportAction {
    Close,
    Export,
    ShowInFolder(PathBuf),
}

/// The preview step's body (spec §7 step 2).
fn preview_ui(
    ui: &mut egui::Ui,
    choice: &mut ImportChoice,
    instances: &[InstanceProfile],
    versions: &[String],
    lock: Option<&str>,
    accent: egui::Color32,
    muted_color: egui::Color32,
) -> Option<ImportAction> {
    let info = &choice.preview.info;
    ui.heading(choice.pack_name());
    fact(ui, "Format", info.format.label(), muted_color);
    if let Some(target) = &info.target {
        fact(
            ui,
            "Minecraft",
            target.minecraft_version.clone(),
            muted_color,
        );
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new("Loader").color(muted_color));
            let mut text = target.loader.label().to_owned();
            if let Some(version) = &target.loader_version {
                text.push(' ');
                text.push_str(version);
            }
            ui.label(text);
            quilt_badge(ui, target.loader.label(), muted_color);
        });
    }
    fact(
        ui,
        "Mods",
        choice.preview.mod_count.to_string(),
        muted_color,
    );
    fact(
        ui,
        "Size",
        format_size(choice.preview.archive_bytes),
        muted_color,
    );
    if choice.preview.has_worlds {
        fact(ui, "Worlds", "Includes worlds", muted_color);
    }
    if !info.warnings.is_empty() {
        for warning in &info.warnings {
            muted(ui, format!("• {warning}"), muted_color);
        }
    }
    ui.add_space(4.0);
    ui.label("Instance name");
    let first = std::mem::take(&mut choice.first_frame);
    let name_error = core_instances::validate_instance_name(&choice.name, instances, None)
        .err()
        .map(|error| name_error_text(&error));
    name_field(
        ui,
        "import-name",
        &mut choice.name,
        name_error.as_deref(),
        first,
    );
    if choice.preview.info.target.is_none() {
        ui.add(
            egui::Label::new("This file doesn't say which Minecraft version it's for. Choose one:")
                .wrap(),
        );
        egui::ComboBox::from_label("Minecraft version")
            .selected_text(choice.version.clone())
            .show_ui(ui, |ui| {
                for version in versions {
                    ui.selectable_value(&mut choice.version, version.clone(), version);
                }
            });
        loader_picker(ui, &mut choice.loader, muted_color);
    }
    if choice.preview.optional_files > 0 {
        ui.checkbox(
            &mut choice.include_optional,
            format!(
                "Also install {}",
                super::startup::plural(
                    choice.preview.optional_files as u64,
                    "optional file",
                    "optional files"
                )
            ),
        );
    }
    let mut block = lock.map(str::to_owned);
    if let Some(ImportBlocker::NeedsCurseForgeKey { mods }) = &choice.preview.blocker {
        let text = curseforge_key_text(*mods);
        ui.add(egui::Label::new(RichText::new(&text).strong()).wrap());
        block = Some(text);
    }
    if choice
        .target()
        .is_none_or(|target| target.minecraft_version.is_empty())
    {
        block.get_or_insert_with(|| "Choose a Minecraft version.".into());
    }
    if let Some(error) = &name_error {
        block.get_or_insert_with(|| error.clone());
    }
    ui.add_space(6.0);
    let ready = block.is_none();
    let mut action = button_row(
        ui,
        |ui| {
            ui.button("Choose another file")
                .clicked()
                .then_some(ImportAction::ChooseFile)
        },
        |ui| {
            let button = primary_button(ui, "Install", ready, accent);
            let clicked = button.clicked();
            if let Some(reason) = &block {
                button.on_disabled_hover_text(reason);
            }
            clicked.then_some(ImportAction::Install)
        },
    );
    if action.is_none() && ready && enter_pressed(ui) {
        action = Some(ImportAction::Install);
    }
    action
}

/// The import worker: re-checks the pack, extracts it into the new folder, installs
/// the game files, and returns the uncommitted outcome. Any failure removes the
/// folder (the outcome's guard does the same if the UI can't commit it).
fn import_worker(
    paths: &ferrite_launcher::core::paths::AppPaths,
    source: &Path,
    profile: InstanceProfile,
    target: PackTarget,
    include_optional: bool,
    curseforge_api_key: Option<String>,
    progress: &dyn Fn(&str),
) -> Result<PackImportOutcome, String> {
    let preview = crate::packs::preview(source, curseforge_api_key.as_deref())
        .map_err(|error| error.to_string())?;
    if let Some(ImportBlocker::NeedsCurseForgeKey { mods }) = preview.blocker {
        return Err(curseforge_key_text(mods));
    }
    if preview.info.target.is_some() && preview.info.target.as_ref() != Some(&target) {
        return Err("The pack changed after it was previewed; no instance was kept.".into());
    }
    std::fs::create_dir_all(paths.instances_dir())
        .map_err(|error| format!("Failed to prepare instance storage: {error}"))?;
    let options = ImportOptions {
        generic_target: Some(target.clone()),
        include_optional_modrinth_files: include_optional,
        curseforge_api_key,
        ..ImportOptions::default()
    };
    let report = crate::packs::import(source, profile.game_dir(paths), &options, progress)
        .map_err(|error| error.to_string())?;
    // From here on the folder exists; the guard removes it on any failure.
    let mut outcome = PackImportOutcome {
        paths: paths.clone(),
        profile,
        files: report.files_written,
        bytes: report.bytes_written,
        warnings: report.warnings.clone(),
        committed: false,
    };
    if report.info.target.as_ref() != Some(&target) {
        return Err("The pack changed while it was being imported; no instance was kept.".into());
    }
    progress("Installing Minecraft files…");
    crate::minecraft::install_version_with_progress(paths, &target.minecraft_version, |message| {
        progress(message)
    })
    .map_err(|error| format!("Failed to install Minecraft: {error}"))?;
    if target.loader != ModLoader::Vanilla {
        progress(&format!("Installing {}…", target.loader.label()));
        crate::loaders::install_version(
            paths,
            &target.minecraft_version,
            target.loader,
            target.loader_version.as_deref(),
        )
        .map_err(|error| format!("Failed to install {}: {error}", target.loader.label()))?;
        if let Some(requested) = target.loader_version.as_deref() {
            let installed = crate::loaders::installed_loader_version(
                paths,
                &target.minecraft_version,
                target.loader,
            );
            if installed.as_deref() != Some(requested) {
                return Err(format!(
                    "The pack needs {} {requested}, but Ferrite installed {}.",
                    target.loader.label(),
                    installed.as_deref().unwrap_or("an unknown version")
                ));
            }
        }
    }
    eprintln!(
        "Ferrite: imported {} ({} files, {})",
        source.display(),
        outcome.files,
        format_size(outcome.bytes)
    );
    outcome.warnings.dedup();
    Ok(outcome)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn export_file_names_keep_dots_and_drop_unsafe_characters() {
        assert_eq!(sanitize_file_name("Pack 1.20"), "Pack 1.20");
        assert_eq!(sanitize_file_name("a/b:c?"), "a-b-c-");
        assert_eq!(sanitize_file_name("Trailing."), "Trailing-");
        assert_eq!(sanitize_file_name("   "), "pack");
    }

    #[test]
    fn export_needs_the_loader_version_only_for_formats_that_record_it() {
        assert_eq!(
            export_block(PackFormat::Modrinth, "Fabric", " ").as_deref(),
            Some("Enter the Fabric version this pack needs.")
        );
        assert!(export_block(PackFormat::Modrinth, "Fabric", "0.16.5").is_none());
        assert!(export_block(PackFormat::Modrinth, "Vanilla", "").is_none());
        assert!(export_block(PackFormat::Ferrite, "Fabric", "").is_none());
        assert!(export_block(PackFormat::Lunar, "Vanilla", "").is_some());
    }

    #[test]
    fn inspect_failures_are_plain_words() {
        let (reason, details) = inspect_failure(
            Path::new("/x/pack.lcpack"),
            &PackError::Unsupported("lunar".into()),
        );
        assert!(reason.starts_with("Lunar Client packs"));
        assert!(details.is_none());
        let (reason, details) = inspect_failure(
            Path::new("/x/pack.zip"),
            &PackError::Invalid("no manifest".into()),
        );
        assert_eq!(reason, "Its pack information is missing or damaged.");
        assert!(details.unwrap().contains("no manifest"));
        assert!(!curseforge_key_text(84).contains("FERRITE_"));
    }

    #[test]
    fn export_failures_keep_the_raw_error_for_details() {
        let (plain, raw) = export_failure(&PackError::Invalid("bad".into()));
        assert_eq!(plain, "Ferrite couldn't export the pack.");
        assert!(raw.contains("bad"));
    }
}
