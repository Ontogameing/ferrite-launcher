//! Minimal delete flow wired to `core::remove` (placeholder until the Stage 2 UI spec
//! screens land): confirm (trash or remove-from-list), a worker for the slow file step,
//! the trash-failed choices, and a permanent-delete confirmation.

use super::Ferrite;
use eframe::egui::{self, RichText};
use ferrite_launcher::core::activity::BusyOperation;
use ferrite_launcher::core::instances::InstanceDirName;
use ferrite_launcher::core::remove::{self, RemovalTarget, RemoveMode, RemoveOutcome, SystemTrash};
use std::sync::mpsc::{self, Receiver, TryRecvError};

/// Where the delete dialog is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum RemoveStep {
    Confirm,
    Working,
    TrashFailed { error: String },
    ConfirmPermanent { acknowledged: bool },
}

/// State of the open delete dialog.
pub(super) struct RemoveDialog {
    directory: InstanceDirName,
    name: String,
    mode: RemoveMode,
    step: RemoveStep,
    /// Trash/permanent unavailable (shared or missing folder); only KeepFiles allowed.
    keep_files_only: bool,
    note: Option<String>,
}

/// The worker's result for the slow file step.
pub(super) struct RemoveTask {
    target: RemovalTarget,
    result: Receiver<Result<(), RemoveOutcome>>,
}

fn is_running(directory: &InstanceDirName) -> bool {
    crate::minecraft::is_instance_running(directory.as_str())
}

impl Ferrite {
    /// Opens the delete dialog for the instance in `directory`.
    pub(super) fn open_remove_dialog(&mut self, directory: InstanceDirName) {
        let Some(profile) = self
            .instances
            .iter()
            .find(|profile| profile.directory() == &directory)
        else {
            return;
        };
        // Dry-run the trash checks so a shared or missing folder is known up front.
        let precheck = remove::prepare_removal(
            &self.paths,
            &self.instances,
            &self.skipped_instances,
            &directory,
            RemoveMode::Trash,
            &|_| false,
        );
        let (keep_files_only, note) = match precheck {
            Err(RemoveOutcome::SharedFolder { .. }) => (
                true,
                Some(format!(
                    "Another entry in your instance list uses this same folder, so Ferrite \
                     won't delete it. You can still remove {} from the list.",
                    profile.name
                )),
            ),
            Err(RemoveOutcome::FolderMissing) => {
                (true, Some("This instance's folder is missing.".to_owned()))
            }
            _ => (false, None),
        };
        self.remove_dialog = Some(RemoveDialog {
            name: profile.name.clone(),
            directory,
            mode: if keep_files_only {
                RemoveMode::KeepFiles
            } else {
                RemoveMode::Trash
            },
            step: RemoveStep::Confirm,
            keep_files_only,
            note,
        });
    }

    /// Starts the chosen removal. KeepFiles finishes immediately; trash and permanent
    /// delete run their file step on a worker thread.
    fn start_removal(&mut self, mode: RemoveMode) {
        let Some(dialog) = self.remove_dialog.as_mut() else {
            return;
        };
        let directory = dialog.directory.clone();
        let target = match remove::prepare_removal(
            &self.paths,
            &self.instances,
            &self.skipped_instances,
            &directory,
            mode,
            &is_running,
        ) {
            Ok(target) => target,
            Err(outcome) => {
                self.apply_remove_outcome(outcome);
                return;
            }
        };
        match mode {
            RemoveMode::KeepFiles => {
                let outcome = match remove::commit_removal(
                    &self.paths,
                    &mut self.instances,
                    &self.skipped_instances,
                    &target,
                    &is_running,
                ) {
                    Ok(outcome) | Err(outcome) => outcome,
                };
                self.apply_remove_outcome(outcome);
            }
            RemoveMode::Trash | RemoveMode::Permanent => {
                if mode == RemoveMode::Permanent {
                    // Save the list first; the worker then deletes the folder.
                    if let Err(outcome) = remove::commit_removal(
                        &self.paths,
                        &mut self.instances,
                        &self.skipped_instances,
                        &target,
                        &is_running,
                    ) {
                        self.apply_remove_outcome(outcome);
                        return;
                    }
                    self.after_instance_removed(&directory);
                }
                let paths = self.paths.clone();
                let worker_target = target.clone();
                let (sender, result) = mpsc::channel();
                let spawned = std::thread::Builder::new()
                    .name("instance-remove".into())
                    .spawn(move || {
                        let outcome = match worker_target.mode() {
                            RemoveMode::Trash => remove::trash_target(
                                &paths,
                                &worker_target,
                                &SystemTrash,
                                &is_running,
                            ),
                            _ => match remove::delete_target(&paths, &worker_target) {
                                RemoveOutcome::Deleted { .. } => Ok(()),
                                other => Err(other),
                            },
                        };
                        let _ = sender.send(outcome);
                    });
                match spawned {
                    Ok(_) => {
                        self.activity
                            .begin(directory.as_str(), BusyOperation::Deleting);
                        self.remove_task = Some(RemoveTask { target, result });
                        if let Some(dialog) = self.remove_dialog.as_mut() {
                            dialog.step = RemoveStep::Working;
                        }
                    }
                    Err(error) => {
                        self.running_text = format!("Could not start removal: {error}");
                    }
                }
            }
        }
    }

    /// Drains the removal worker and finishes on the UI thread.
    pub(super) fn poll_remove_task(&mut self) {
        let Some(task) = &self.remove_task else {
            return;
        };
        let result = match task.result.try_recv() {
            Ok(result) => result,
            Err(TryRecvError::Empty) => return,
            Err(TryRecvError::Disconnected) => Err(RemoveOutcome::Failed(
                ferrite_launcher::core::instances::InstanceError::Install(
                    "the removal worker stopped unexpectedly".into(),
                ),
            )),
        };
        let task = self.remove_task.take().expect("checked above");
        self.activity
            .end(task.target.profile().directory().as_str());
        let outcome = match (task.target.mode(), result) {
            (RemoveMode::Trash, Ok(())) => remove::finish_trash(
                &self.paths,
                &mut self.instances,
                &self.skipped_instances,
                task.target,
            ),
            (_, Ok(())) => RemoveOutcome::Deleted {
                profile: task.target.profile().clone(),
            },
            (_, Err(outcome)) => outcome,
        };
        self.apply_remove_outcome(outcome);
        if self.close_after_remove {
            self.close_requested = true;
        }
    }

    /// Clears selection/mod state that pointed at a removed instance.
    fn after_instance_removed(&mut self, directory: &InstanceDirName) {
        if self
            .mod_target
            .as_ref()
            .is_some_and(|target| target.directory() == directory)
        {
            self.mod_target = None;
            self.installed_mods = None;
        }
        let len = self.instances.len();
        self.selected_instance = match self.selected_instance {
            _ if len == 0 => None,
            Some(index) if index >= len => Some(len - 1),
            other => other,
        };
    }

    /// Shows the outcome in the dialog or the status line.
    fn apply_remove_outcome(&mut self, outcome: RemoveOutcome) {
        let name = self
            .remove_dialog
            .as_ref()
            .map(|dialog| dialog.name.clone())
            .unwrap_or_default();
        let set_note = |this: &mut Self, step: RemoveStep, note: String| {
            if let Some(dialog) = this.remove_dialog.as_mut() {
                dialog.step = step;
                dialog.note = Some(note);
            }
        };
        match outcome {
            RemoveOutcome::Trashed { profile } => {
                self.after_instance_removed(profile.directory());
                self.remove_dialog = None;
                self.running_text = format!("Moved {} to the {}.", profile.name, trash_word());
            }
            RemoveOutcome::RemovedFromList { profile, folder } => {
                self.after_instance_removed(profile.directory());
                self.remove_dialog = None;
                self.running_text = format!(
                    "Removed {} from Ferrite. Its files are still in {}.",
                    profile.name,
                    folder.display()
                );
            }
            RemoveOutcome::Deleted { profile } => {
                self.after_instance_removed(profile.directory());
                self.remove_dialog = None;
                self.running_text = format!("Deleted {} permanently.", profile.name);
            }
            RemoveOutcome::PartiallyDeleted {
                profile,
                folder,
                failed,
                error,
            } => {
                self.after_instance_removed(profile.directory());
                self.remove_dialog = None;
                self.running_text = format!(
                    "Most of {} was deleted, but {} item(s) in {} couldn't be removed: {error}",
                    profile.name,
                    failed.len(),
                    folder.display()
                );
            }
            RemoveOutcome::SharedFolder { .. } => {
                if let Some(dialog) = self.remove_dialog.as_mut() {
                    dialog.keep_files_only = true;
                    dialog.mode = RemoveMode::KeepFiles;
                }
                set_note(
                    self,
                    RemoveStep::Confirm,
                    format!(
                        "Another entry in your instance list uses this same folder, so \
                         Ferrite won't delete it. You can still remove {name} from the list."
                    ),
                );
            }
            RemoveOutcome::NowRunning => set_note(
                self,
                RemoveStep::Confirm,
                format!("{name} started while this was open. Close Minecraft, then try again."),
            ),
            RemoveOutcome::FolderMissing => {
                if let Some(dialog) = self.remove_dialog.as_mut() {
                    dialog.keep_files_only = true;
                    dialog.mode = RemoveMode::KeepFiles;
                }
                set_note(
                    self,
                    RemoveStep::Confirm,
                    "This instance's folder is missing.".into(),
                );
            }
            RemoveOutcome::TrashFailed { error, .. } => set_note(
                self,
                RemoveStep::TrashFailed { error },
                "Nothing has been deleted.".into(),
            ),
            RemoveOutcome::TrashedButNotSaved { profile, error } => {
                self.remove_dialog = None;
                self.running_text = format!(
                    "Moved {} to the {}, but the instance list couldn't be saved ({error}). \
                     It now shows as missing; remove it from Ferrite to finish.",
                    profile.name,
                    trash_word()
                );
            }
            RemoveOutcome::Failed(error) => set_note(
                self,
                RemoveStep::Confirm,
                format!("Couldn't remove {name}: {error}"),
            ),
        }
    }

    /// Defers closing the window while a trash or delete is running.
    pub(super) fn intercept_close_while_removing(&mut self, ctx: &egui::Context) {
        if self.remove_task.is_some() && ctx.input(|input| input.viewport().close_requested()) {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.close_after_remove = true;
        }
    }

    /// Draws the delete dialog.
    pub(super) fn remove_window(&mut self, context: &egui::Context) {
        let Some(dialog) = self.remove_dialog.as_mut() else {
            return;
        };
        let mut action: Option<RemoveMode> = None;
        let mut close = false;
        let working = dialog.step == RemoveStep::Working;
        let title = match &dialog.step {
            RemoveStep::Confirm => format!("Delete {}?", dialog.name),
            RemoveStep::Working => format!("Removing {}…", dialog.name),
            RemoveStep::TrashFailed { .. } => {
                format!("Couldn't move {} to the {}", dialog.name, trash_word())
            }
            RemoveStep::ConfirmPermanent { .. } => format!("Delete {} permanently?", dialog.name),
        };
        egui::Window::new(title)
            .id(egui::Id::new("remove-instance"))
            .collapsible(false)
            .resizable(false)
            .default_width(460.0)
            .show(context, |ui| {
                if let Some(note) = &dialog.note {
                    ui.label(RichText::new(note).strong());
                }
                match &mut dialog.step {
                    RemoveStep::Confirm => {
                        ui.add_enabled_ui(!dialog.keep_files_only, |ui| {
                            ui.radio_value(
                                &mut dialog.mode,
                                RemoveMode::Trash,
                                format!("Move to {}", trash_word()),
                            );
                        });
                        ui.radio_value(
                            &mut dialog.mode,
                            RemoveMode::KeepFiles,
                            "Remove from Ferrite, keep the files.",
                        );
                        ui.horizontal(|ui| {
                            close = ui.button("Cancel").clicked();
                            let label = match dialog.mode {
                                RemoveMode::Trash => format!("Move to {}", trash_word()),
                                _ => "Remove from Ferrite".to_owned(),
                            };
                            if ui.button(label).clicked() {
                                action = Some(dialog.mode);
                            }
                        });
                    }
                    RemoveStep::Working => {
                        ui.spinner();
                        ui.label(
                            "This can take a few minutes if the instance is on another drive.",
                        );
                    }
                    RemoveStep::TrashFailed { error } => {
                        ui.collapsing("Details", |ui| {
                            ui.label(RichText::new(error.as_str()).monospace());
                        });
                        ui.horizontal(|ui| {
                            close = ui.button(format!("Keep {}", dialog.name)).clicked();
                            if ui.button("Remove from Ferrite only").clicked() {
                                action = Some(RemoveMode::KeepFiles);
                            }
                            if ui.button("Delete permanently…").clicked() {
                                dialog.note = None;
                                dialog.step = RemoveStep::ConfirmPermanent {
                                    acknowledged: false,
                                };
                            }
                        });
                    }
                    RemoveStep::ConfirmPermanent { acknowledged } => {
                        ui.label("This can't be undone.");
                        ui.checkbox(
                            acknowledged,
                            format!("I understand {} can't be recovered.", dialog.name),
                        );
                        ui.horizontal(|ui| {
                            close = ui.button("Back").clicked();
                            if ui
                                .add_enabled(*acknowledged, egui::Button::new("Delete permanently"))
                                .clicked()
                            {
                                action = Some(RemoveMode::Permanent);
                            }
                        });
                    }
                }
            });
        if close && !working {
            self.remove_dialog = None;
        } else if let Some(mode) = action {
            self.start_removal(mode);
        }
    }
}

/// The OS word for the trash.
fn trash_word() -> &'static str {
    if cfg!(windows) {
        "Recycle Bin"
    } else {
        "Trash"
    }
}
