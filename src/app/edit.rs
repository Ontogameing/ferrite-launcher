//! Edit (Stage 2 UI spec §4): rename, and change the Minecraft version or loader.
//!
//! A rename is a manifest change only (`core::edit::rename_instance`). A version or
//! loader change installs the shared game files on a worker (the same installer
//! Create uses), then commits on the UI thread with `core::edit::commit_version_loader`;
//! nothing is committed unless the install succeeds.

use super::dialogs::{
    button_row, enter_pressed, escape_pressed, muted, name_error_text, name_field, primary_button,
    quilt_badge,
};
use super::instances::install_game_files;
use super::startup::details;
use super::{Ferrite, InstanceCreationEvent, InstanceCreationStage};
use crate::instances::InstanceProfile;
use crate::loaders::ModLoader;
use eframe::egui::{self, Color32, RichText};
use ferrite_launcher::core::activity::{BusyOperation, InstanceAction, disabled_reason};
use ferrite_launcher::core::edit::{self, EditError, VersionDirection};
use ferrite_launcher::core::instances::{self as core_instances, InstanceDirName};
use std::sync::mpsc::{self, Receiver, TryRecvError};

/// A warning shown above the Edit buttons (spec §4.3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum EditWarning {
    /// Needs [Duplicate first] and the "I understand" checkbox.
    Downgrade,
    /// Informational; also used when the order is unknown.
    Upgrade {
        current: String,
    },
    LoaderWithMods {
        old: String,
        new: String,
    },
    ToVanilla,
    Quilt,
}

/// The warnings for changing `original` to (`version`, `loader`).
pub(super) fn edit_warnings(
    original: &InstanceProfile,
    version: &str,
    loader: &str,
    versions: &[String],
    mods_present: bool,
) -> Vec<EditWarning> {
    let change = edit::assess_version_change(
        &original.version,
        &original.loader,
        version,
        loader,
        versions,
    );
    let mut warnings = Vec::new();
    match change.minecraft {
        VersionDirection::Downgrade => warnings.push(EditWarning::Downgrade),
        VersionDirection::Upgrade | VersionDirection::Unknown => {
            warnings.push(EditWarning::Upgrade {
                current: original.version.clone(),
            })
        }
        VersionDirection::Same => {}
    }
    if change.to_vanilla {
        warnings.push(EditWarning::ToVanilla);
    } else if change.loader_changed && mods_present && original.loader != "Vanilla" {
        warnings.push(EditWarning::LoaderWithMods {
            old: original.loader.clone(),
            new: loader.to_owned(),
        });
    }
    if change.loader_changed && loader == "Quilt" {
        warnings.push(EditWarning::Quilt);
    }
    warnings
}

/// The words of a warning (the downgrade one names the instance).
pub(super) fn warning_text(warning: &EditWarning, name: &str) -> String {
    match warning {
        EditWarning::Downgrade => format!(
            "Older versions of Minecraft can't always open worlds from newer ones, and can \
             damage them. Duplicate {name} first and try the older version on the copy."
        ),
        EditWarning::Upgrade { current } => format!(
            "Minecraft updates worlds when you open them. After that they won't open in \
             {current} anymore."
        ),
        EditWarning::LoaderWithMods { old, new } => format!(
            "The mods in this instance were made for {old}. They won't load with {new}. \
             Ferrite won't remove them."
        ),
        EditWarning::ToVanilla => "Mods are kept but won't be loaded.".into(),
        EditWarning::Quilt => "Quilt support is experimental.".into(),
    }
}

/// `"Minecraft 1.21.1 · Fabric"`.
pub(super) fn version_label(version: &str, loader: &str) -> String {
    format!("Minecraft {version} · {loader}")
}

/// The open Edit modal.
pub(super) struct EditDialog {
    original: InstanceProfile,
    name: String,
    version: String,
    loader: String,
    /// Whether `mods/` had anything in it when the dialog opened.
    mods_present: bool,
    downgrade_understood: bool,
    notice: Option<String>,
    first_frame: bool,
}

impl EditDialog {
    fn version_changed(&self) -> bool {
        self.version != self.original.version || self.loader != self.original.loader
    }
}

/// What an update shows in its window.
enum UpdateState {
    Installing { status: String },
    Failed { reason: String, error: String },
}

/// The single version/loader update in progress (or failed and still shown).
pub(super) struct EditTask {
    directory: InstanceDirName,
    name: String,
    new_name: String,
    old_version: String,
    old_loader: String,
    version: String,
    loader: String,
    events: Option<Receiver<InstanceCreationEvent>>,
    state: UpdateState,
    pub(super) window_open: bool,
}

impl EditTask {
    /// The instance being updated while the install runs.
    pub(super) fn installing(&self) -> Option<&InstanceDirName> {
        self.events.as_ref().map(|_| &self.directory)
    }
}

fn is_running(directory: &InstanceDirName) -> bool {
    crate::minecraft::is_instance_running(directory.as_str())
}

fn has_mods(profile: &InstanceProfile, paths: &ferrite_launcher::core::paths::AppPaths) -> bool {
    std::fs::read_dir(profile.game_dir(paths).join("mods"))
        .map(|mut entries| entries.next().is_some())
        .unwrap_or(false)
}

impl Ferrite {
    /// Opens the Edit modal for the instance in `directory`.
    pub(super) fn open_edit_dialog(&mut self, directory: &InstanceDirName) {
        if self.edit_dialog.is_some() {
            return;
        }
        let Some(profile) = self
            .instances
            .iter()
            .find(|profile| profile.directory() == directory)
            .cloned()
        else {
            return;
        };
        self.edit_dialog = Some(EditDialog {
            name: profile.name.clone(),
            version: profile.version.clone(),
            loader: profile.loader.clone(),
            mods_present: has_mods(&profile, &self.paths),
            original: profile,
            downgrade_understood: false,
            notice: None,
            first_frame: true,
        });
    }

    /// Whether the update worker is running (keeps the frame loop polling).
    pub(super) fn edit_busy(&self) -> bool {
        self.edit_task
            .as_ref()
            .is_some_and(|task| task.events.is_some())
    }

    /// Starts installing `version`/`loader` for the instance; the rename (if any) and
    /// the version change are committed together when the install succeeds.
    fn start_update(
        &mut self,
        directory: InstanceDirName,
        new_name: String,
        version: String,
        loader_label: String,
    ) -> Result<(), String> {
        if self.edit_busy() {
            return Err("Ferrite is already updating an instance.".into());
        }
        let Some(profile) = self
            .instances
            .iter()
            .find(|profile| profile.directory() == &directory)
            .cloned()
        else {
            return Err("This instance is no longer in the list.".into());
        };
        if is_running(&directory) {
            return Err(format!(
                "{} started while this was open. Close Minecraft, then try again.",
                profile.name
            ));
        }
        let Some(loader) = ModLoader::from_label(&loader_label) else {
            return Err(format!("Unknown mod loader: {loader_label}"));
        };
        let paths = self.paths.clone();
        let worker_version = version.clone();
        let (sender, receiver) = mpsc::channel();
        std::thread::Builder::new()
            .name("instance-update".into())
            .spawn(move || {
                let result = install_game_files(&paths, &worker_version, loader, &|event| {
                    let _ = sender.send(event);
                })
                .map(|()| profile.clone());
                let _ = sender.send(InstanceCreationEvent::Finished(result));
            })
            .map_err(|error| format!("Couldn't start the update: {error}"))?;
        self.activity
            .begin(directory.as_str(), BusyOperation::Updating);
        let current = self
            .instances
            .iter()
            .find(|profile| profile.directory() == &directory)
            .expect("checked above");
        self.edit_task = Some(EditTask {
            name: current.name.clone(),
            old_version: current.version.clone(),
            old_loader: current.loader.clone(),
            directory,
            new_name,
            version,
            loader: loader_label,
            events: Some(receiver),
            state: UpdateState::Installing {
                status: InstanceCreationStage::Preparing.label().to_owned(),
            },
            window_open: true,
        });
        Ok(())
    }

    /// Drains update progress and commits a finished install on the UI thread.
    pub(super) fn poll_edit_task(&mut self) {
        loop {
            let Some(task) = self.edit_task.as_mut() else {
                return;
            };
            let Some(events) = &task.events else {
                return;
            };
            let result = match events.try_recv() {
                Ok(InstanceCreationEvent::Stage(stage)) => {
                    task.state = UpdateState::Installing {
                        status: stage.label().to_owned(),
                    };
                    continue;
                }
                Ok(InstanceCreationEvent::DownloadProgress(message)) => {
                    task.state = UpdateState::Installing { status: message };
                    continue;
                }
                Ok(InstanceCreationEvent::Finished(result)) => result.map(|_| ()),
                Err(TryRecvError::Empty) => return,
                Err(TryRecvError::Disconnected) => {
                    Err("The update worker stopped unexpectedly.".to_owned())
                }
            };
            task.events = None;
            let directory = task.directory.clone();
            self.activity.end(directory.as_str());
            let task = self.edit_task.as_ref().expect("checked above");
            let (name, new_name) = (task.name.clone(), task.new_name.clone());
            let (version, loader) = (task.version.clone(), task.loader.clone());
            let failure = match result {
                Err(error) => Some(error),
                Ok(()) => match edit::commit_version_loader(
                    &self.paths,
                    &mut self.instances,
                    &self.skipped_instances,
                    &directory,
                    &version,
                    &loader,
                    &is_running,
                ) {
                    Ok(_) => None,
                    Err(EditError::NowRunning) => Some(format!(
                        "{name} started while Ferrite was updating it. Close Minecraft, then \
                         try again."
                    )),
                    Err(EditError::Failed(error)) => {
                        Some(format!("Couldn't save the change: {error}"))
                    }
                },
            };
            match failure {
                None => {
                    let mut shown_name = name.clone();
                    if new_name != name {
                        match edit::rename_instance(
                            &self.paths,
                            &mut self.instances,
                            &self.skipped_instances,
                            &directory,
                            &new_name,
                            &|_| false,
                        ) {
                            Ok(_) => shown_name = new_name,
                            Err(error) => {
                                self.running_text = format!(
                                    "Updated {name} to {}, but couldn't rename it: {error}",
                                    version_label(&version, &loader)
                                );
                                self.edit_task = None;
                                return;
                            }
                        }
                    }
                    self.running_text = format!(
                        "Updated {shown_name} to {}.",
                        version_label(&version, &loader)
                    );
                    self.edit_task = None;
                }
                Some(error) => {
                    let task = self.edit_task.as_mut().expect("checked above");
                    task.state = UpdateState::Failed {
                        reason: format!(
                            "Couldn't switch {name} to {}. It still uses {}.",
                            version_label(&version, &loader),
                            version_label(&task.old_version, &task.old_loader)
                        ),
                        error,
                    };
                    task.window_open = true;
                }
            }
            return;
        }
    }

    /// Draws the Edit modal.
    pub(super) fn edit_window(&mut self, context: &egui::Context) {
        let muted_color = self.muted_color();
        let accent = self.accent_color();
        let Some(mut dialog) = self.edit_dialog.take() else {
            return;
        };
        let directory = dialog.original.directory().clone();
        let Some(current) = self
            .instances
            .iter()
            .find(|profile| profile.directory() == &directory)
        else {
            return;
        };
        let status = self.instance_status(current);
        let running = is_running(&directory);
        let version_lock = if running {
            Some("Close Minecraft to change the version or loader.".to_owned())
        } else if self.edit_busy() {
            Some("Ferrite is already updating an instance.".to_owned())
        } else {
            disabled_reason(InstanceAction::ChangeVersion, &current.name, &status)
        };
        let rename_lock = disabled_reason(InstanceAction::Rename, &current.name, &status);
        let name_error =
            core_instances::validate_instance_name(&dialog.name, &self.instances, Some(&directory))
                .err()
                .map(|error| name_error_text(&error));
        let warnings = if dialog.version_changed() {
            edit_warnings(
                &dialog.original,
                &dialog.version,
                &dialog.loader,
                &self.versions,
                dialog.mods_present,
            )
        } else {
            Vec::new()
        };
        let mut versions = self.versions.clone();
        if !versions.contains(&dialog.original.version) {
            versions.insert(0, dialog.original.version.clone());
        }
        let first = std::mem::take(&mut dialog.first_frame);
        let mut action = None;
        egui::Modal::new(egui::Id::new("edit-instance")).show(context, |ui| {
            ui.set_width(440.0);
            if escape_pressed(ui) {
                action = Some(EditAction::Cancel);
            }
            ui.heading(format!("Edit {}", dialog.original.name));
            ui.label("Name");
            ui.add_enabled_ui(rename_lock.is_none(), |ui| {
                name_field(
                    ui,
                    "edit-name",
                    &mut dialog.name,
                    name_error.as_deref(),
                    first,
                );
            });
            muted(
                ui,
                format!(
                    "Only the name changes. The folder stays {}.",
                    dialog.original.directory().as_str()
                ),
                muted_color,
            );
            ui.add_space(6.0);
            let response = ui
                .add_enabled_ui(version_lock.is_none(), |ui| {
                    version_picker(ui, &mut dialog.version, &dialog.original.version, &versions);
                    edit_loader_picker(
                        ui,
                        &mut dialog.loader,
                        &dialog.original.loader,
                        muted_color,
                    );
                })
                .response;
            if let Some(reason) = &version_lock {
                response.on_disabled_hover_text(reason);
                muted(ui, reason.as_str(), muted_color);
            }
            let downgrade = warnings.contains(&EditWarning::Downgrade);
            if !warnings.is_empty() {
                egui::Frame::group(ui.style()).show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    for warning in &warnings {
                        let text = warning_text(warning, &dialog.original.name);
                        match warning {
                            EditWarning::Downgrade => {
                                ui.add(egui::Label::new(RichText::new(text).strong()).wrap());
                                if ui.button("Duplicate first").clicked() {
                                    action = Some(EditAction::DuplicateFirst);
                                }
                                ui.checkbox(
                                    &mut dialog.downgrade_understood,
                                    "I understand my worlds might not open.",
                                );
                            }
                            _ => muted(ui, text, muted_color),
                        }
                    }
                });
            }
            if let Some(notice) = &dialog.notice {
                ui.add(egui::Label::new(RichText::new(notice).strong()).wrap());
            }
            ui.add_space(6.0);
            let changed = dialog.version_changed();
            let ready = name_error.is_none()
                && (!changed
                    || (version_lock.is_none() && (!downgrade || dialog.downgrade_understood)));
            let label = if changed { "Save and install" } else { "Save" };
            let clicked = button_row(
                ui,
                |ui| ui.button("Cancel").clicked().then_some(EditAction::Cancel),
                |ui| {
                    primary_button(ui, label, ready, accent)
                        .clicked()
                        .then_some(EditAction::Save)
                },
            );
            if clicked.is_some() {
                action = clicked;
            }
            if action.is_none() && ready && enter_pressed(ui) {
                action = Some(EditAction::Save);
            }
        });
        self.edit_dialog = Some(dialog);
        match action {
            Some(EditAction::Cancel) => self.edit_dialog = None,
            Some(EditAction::DuplicateFirst) => {
                self.edit_dialog = None;
                self.open_duplicate_dialog(&directory);
            }
            Some(EditAction::Save) => self.save_edit(),
            None => {}
        }
    }

    /// Applies the Edit dialog: an instant rename, or the install-then-commit update.
    fn save_edit(&mut self) {
        let Some(dialog) = self.edit_dialog.as_ref() else {
            return;
        };
        let directory = dialog.original.directory().clone();
        let new_name = dialog.name.trim().to_owned();
        if dialog.version_changed() {
            let (version, loader) = (dialog.version.clone(), dialog.loader.clone());
            match self.start_update(directory, new_name, version, loader) {
                Ok(()) => self.edit_dialog = None,
                Err(message) => {
                    if let Some(dialog) = self.edit_dialog.as_mut() {
                        dialog.notice = Some(message);
                    }
                }
            }
            return;
        }
        let old_name = dialog.original.name.clone();
        if new_name == old_name {
            self.edit_dialog = None;
            return;
        }
        match edit::rename_instance(
            &self.paths,
            &mut self.instances,
            &self.skipped_instances,
            &directory,
            &new_name,
            &|_| false,
        ) {
            Ok(_) => {
                self.running_text = format!("Renamed {old_name} to {new_name}.");
                self.edit_dialog = None;
            }
            Err(error) => {
                let message = match &error {
                    EditError::Failed(core_instances::InstanceError::Name(name_error)) => {
                        name_error_text(name_error)
                    }
                    other => format!("Couldn't rename {old_name}: {other}"),
                };
                if let Some(dialog) = self.edit_dialog.as_mut() {
                    dialog.notice = Some(message);
                }
            }
        }
    }

    /// Draws the "Updating <name>" window while installing or after a failure.
    pub(super) fn edit_task_window(&mut self, context: &egui::Context) {
        let Some(task) = self.edit_task.as_mut() else {
            return;
        };
        if !task.window_open {
            if task.events.is_none() {
                self.edit_task = None;
            }
            return;
        }
        let mut open = true;
        let mut action = None;
        egui::Window::new(format!("Updating {}", task.name))
            .id(egui::Id::new("edit-update"))
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .default_width(440.0)
            .show(context, |ui| match &task.state {
                UpdateState::Installing { status } => {
                    ui.add(egui::ProgressBar::new(0.0).animate(true));
                    ui.add(egui::Label::new(status.as_str()).wrap());
                }
                UpdateState::Failed { reason, error } => {
                    ui.add(egui::Label::new(reason.as_str()).wrap());
                    details(ui, "edit-update-error", std::slice::from_ref(error));
                    ui.add_space(6.0);
                    action = button_row(
                        ui,
                        |ui| ui.button("Close").clicked().then_some(false),
                        |ui| ui.button("Try again").clicked().then_some(true),
                    );
                }
            });
        let retry = action == Some(true);
        if !open || action.is_some() {
            task.window_open = false;
        }
        if retry {
            let task = self.edit_task.take().expect("open");
            let name = task.name.clone();
            if let Err(message) = self.start_update(
                task.directory.clone(),
                task.new_name.clone(),
                task.version.clone(),
                task.loader.clone(),
            ) {
                self.edit_task = Some(EditTask {
                    state: UpdateState::Failed {
                        reason: format!(
                            "Couldn't switch {name} to {}. It still uses {}.",
                            version_label(&task.version, &task.loader),
                            version_label(&task.old_version, &task.old_loader)
                        ),
                        error: message,
                    },
                    window_open: true,
                    ..task
                });
            }
        } else if self
            .edit_task
            .as_ref()
            .is_some_and(|task| !task.window_open && task.events.is_none())
        {
            self.edit_task = None;
        }
    }

    /// Reopens the update window (Updating… chip click).
    pub(super) fn show_edit_window(&mut self) {
        if let Some(task) = self.edit_task.as_mut() {
            task.window_open = true;
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum EditAction {
    Cancel,
    Save,
    DuplicateFirst,
}

/// Minecraft version ComboBox with the current value marked "(current)".
fn version_picker(ui: &mut egui::Ui, selected: &mut String, current: &str, versions: &[String]) {
    let mark = |version: &str| {
        if version == current {
            format!("{version} (current)")
        } else {
            version.to_owned()
        }
    };
    egui::ComboBox::from_label("Minecraft version")
        .selected_text(mark(selected))
        .show_ui(ui, |ui| {
            for version in versions {
                ui.selectable_value(selected, version.clone(), mark(version));
            }
        });
}

/// Loader ComboBox for Edit: "(current)" marker, Quilt as experimental (spec §8).
fn edit_loader_picker(
    ui: &mut egui::Ui,
    selected: &mut String,
    current: &str,
    muted_color: Color32,
) {
    let mark = |label: &str| {
        let shown = ModLoader::from_label(label)
            .map(ModLoader::picker_label)
            .unwrap_or(label);
        if label == current {
            format!("{shown} (current)")
        } else {
            shown.to_owned()
        }
    };
    ui.horizontal(|ui| {
        egui::ComboBox::from_label("Mod loader")
            .selected_text(mark(selected))
            .show_ui(ui, |ui| {
                for loader in ModLoader::ALL {
                    ui.selectable_value(selected, loader.label().to_owned(), mark(loader.label()));
                }
            });
        quilt_badge(ui, selected, muted_color);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn versions() -> Vec<String> {
        ["1.21.1", "1.20.1", "1.19.4"].map(String::from).to_vec()
    }

    fn original(loader: &str) -> InstanceProfile {
        InstanceProfile::new("Survival".into(), "1.20.1".into(), loader.into(), &[])
    }

    #[test]
    fn downgrade_and_upgrade_warnings_follow_the_version_order() {
        let fabric = original("Fabric");
        assert_eq!(
            edit_warnings(&fabric, "1.19.4", "Fabric", &versions(), false),
            vec![EditWarning::Downgrade]
        );
        assert_eq!(
            edit_warnings(&fabric, "1.21.1", "Fabric", &versions(), false),
            vec![EditWarning::Upgrade {
                current: "1.20.1".into()
            }]
        );
        // Unknown order: no downgrade warning, the upgrade note instead.
        assert_eq!(
            edit_warnings(&fabric, "24w14a", "Fabric", &versions(), false),
            vec![EditWarning::Upgrade {
                current: "1.20.1".into()
            }]
        );
        assert!(edit_warnings(&fabric, "1.20.1", "Fabric", &versions(), true).is_empty());
    }

    #[test]
    fn loader_warnings_depend_on_mods_and_target() {
        let fabric = original("Fabric");
        assert_eq!(
            edit_warnings(&fabric, "1.20.1", "Forge", &versions(), true),
            vec![EditWarning::LoaderWithMods {
                old: "Fabric".into(),
                new: "Forge".into()
            }]
        );
        assert!(edit_warnings(&fabric, "1.20.1", "Forge", &versions(), false).is_empty());
        assert_eq!(
            edit_warnings(&fabric, "1.20.1", "Vanilla", &versions(), true),
            vec![EditWarning::ToVanilla]
        );
        assert_eq!(
            edit_warnings(&original("Vanilla"), "1.20.1", "Quilt", &versions(), false),
            vec![EditWarning::Quilt]
        );
        assert_eq!(
            warning_text(
                &EditWarning::LoaderWithMods {
                    old: "Fabric".into(),
                    new: "Forge".into()
                },
                "Survival"
            ),
            "The mods in this instance were made for Fabric. They won't load with Forge. \
             Ferrite won't remove them."
        );
    }
}
