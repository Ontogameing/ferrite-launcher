//! Delete dialog (Stage 2 UI spec §3): confirm with size/worlds facts, move to the OS
//! trash on a worker, the trash-failed choices, and the permanent-delete confirmation.
//!
//! The screen flow is a small state machine ([`DeleteDialog`], [`DeleteStep`],
//! [`DeleteDialog::apply_outcome`]) kept free of egui so it can be tested; the file
//! work itself is `core::remove`.

use super::Ferrite;
use super::dialogs::{
    self, ScanJob, danger_button, escape_pressed, fact, muted, path_fact, size_line, trash_word,
    worlds_line,
};
use super::startup::{Segment, details, sentence};
use crate::instances::InstanceProfile;
use eframe::egui::{self, RichText};
use ferrite_launcher::core::activity::{BusyOperation, InstanceAction, OperationId};
use ferrite_launcher::core::instances::{InstanceDirName, InstanceError};
use ferrite_launcher::core::migration::format_size;
use ferrite_launcher::core::remove::{
    self, RemovalTarget, RemoveMode, RemoveOutcome, SystemTrash, TrashFailureKind,
};
use ferrite_launcher::core::scan::InstanceScan;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, TryRecvError};

/// Which screen of the delete dialog is showing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum DeleteStep {
    /// §3.1: facts and the Trash / Remove-from-Ferrite choice.
    Confirm,
    /// §3.2: the trash worker is running.
    Moving,
    /// §3.3: the OS trash refused; nothing was deleted by Ferrite.
    TrashFailed {
        kind: TrashFailureKind,
        error: String,
    },
    /// §3.4: permanent delete confirmation (after a rescan of what is left).
    ConfirmPermanent { acknowledged: bool },
    /// §3.4: the permanent delete worker is running.
    Deleting,
    /// §3.4 "Partly deleted": the entry is gone, some files are left.
    PartiallyDeleted {
        folder: PathBuf,
        failed: Vec<PathBuf>,
        error: String,
    },
}

impl DeleteStep {
    fn is_working(&self) -> bool {
        matches!(self, Self::Moving | Self::Deleting)
    }
}

/// State of the open delete dialog.
pub(super) struct DeleteDialog {
    profile: InstanceProfile,
    folder: PathBuf,
    /// The radio choice on the confirm screen: [`RemoveMode::Trash`] or
    /// [`RemoveMode::KeepFiles`].
    choice: RemoveMode,
    step: DeleteStep,
    /// Where to go back to if a started action is refused (e.g. `NowRunning`).
    return_step: DeleteStep,
    /// Another entry uses the same folder: trash/permanent are refused.
    shared: bool,
    /// The folder does not exist: only remove-from-list is possible.
    missing: bool,
    /// A line shown in the dialog (started while open, unexpected failure).
    notice: Option<String>,
    /// Size and worlds; rerun before the permanent step.
    scan: ScanJob,
    /// Give the safe button keyboard focus on the next frame.
    focus_safe_button: bool,
    /// The last trash-failed screen, where [Back] from the permanent step returns.
    trash_failure: Option<DeleteStep>,
}

/// What the app must do after [`DeleteDialog::apply_outcome`].
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct DeleteEffect {
    /// Close the dialog.
    pub close: bool,
    /// Status-line message.
    pub status: Option<String>,
    /// This instance is no longer in the list (fix the selection and mod target).
    pub removed: Option<InstanceDirName>,
}

impl DeleteDialog {
    /// A dialog on the confirm screen. `shared`/`missing` come from a dry-run check.
    pub(super) fn new(
        profile: InstanceProfile,
        folder: PathBuf,
        shared: bool,
        missing: bool,
        scan: ScanJob,
    ) -> Self {
        let keep_only = shared || missing;
        Self {
            profile,
            folder,
            choice: if keep_only {
                RemoveMode::KeepFiles
            } else {
                RemoveMode::Trash
            },
            step: DeleteStep::Confirm,
            return_step: DeleteStep::Confirm,
            shared,
            missing,
            notice: None,
            scan,
            focus_safe_button: true,
            trash_failure: None,
        }
    }

    fn name(&self) -> &str {
        &self.profile.name
    }

    pub(super) fn directory(&self) -> &InstanceDirName {
        self.profile.directory()
    }

    fn trash_allowed(&self) -> bool {
        !self.shared && !self.missing
    }

    /// Switches to a worker step, remembering where a refusal should return to.
    pub(super) fn begin_work(&mut self, mode: RemoveMode) {
        self.return_step = self.step.clone();
        self.notice = None;
        self.step = if mode == RemoveMode::Permanent {
            DeleteStep::Deleting
        } else {
            DeleteStep::Moving
        };
    }

    /// Opens the permanent-delete step with a fresh scan of what is left (spec §3.3:
    /// a failed trash may have moved part of the folder already).
    pub(super) fn begin_permanent(&mut self, rescan: ScanJob) {
        self.notice = None;
        self.scan = rescan;
        self.step = DeleteStep::ConfirmPermanent {
            acknowledged: false,
        };
        self.focus_safe_button = true;
    }

    /// Goes back from the permanent step to the trash-failed choices.
    fn back_from_permanent(&mut self) {
        self.notice = None;
        self.step = self.trash_failure.clone().unwrap_or(DeleteStep::Confirm);
        self.focus_safe_button = true;
    }

    /// The permanent step's button is usable: box ticked and the rescan finished.
    fn permanent_ready(&self) -> bool {
        matches!(
            self.step,
            DeleteStep::ConfirmPermanent { acknowledged: true }
        ) && !self.scan.is_running()
    }

    /// Applies a `core::remove` result to the dialog.
    pub(super) fn apply_outcome(&mut self, outcome: RemoveOutcome) -> DeleteEffect {
        let name = self.name().to_owned();
        let refused_step = if self.step.is_working() {
            self.return_step.clone()
        } else {
            self.step.clone()
        };
        match outcome {
            RemoveOutcome::Trashed { profile } => DeleteEffect {
                close: true,
                status: Some(format!("Moved {} to the {}.", profile.name, trash_word())),
                removed: Some(profile.directory().clone()),
            },
            RemoveOutcome::RemovedFromList { profile, folder } => DeleteEffect {
                close: true,
                status: Some(format!(
                    "Removed {} from Ferrite. Its files are still in {}.",
                    profile.name,
                    folder.display()
                )),
                removed: Some(profile.directory().clone()),
            },
            RemoveOutcome::Deleted { profile } => DeleteEffect {
                close: true,
                status: Some(format!("Deleted {} permanently.", profile.name)),
                removed: Some(profile.directory().clone()),
            },
            RemoveOutcome::PartiallyDeleted {
                profile,
                folder,
                failed,
                error,
            } => {
                self.step = DeleteStep::PartiallyDeleted {
                    folder,
                    failed,
                    error,
                };
                self.focus_safe_button = true;
                DeleteEffect {
                    removed: Some(profile.directory().clone()),
                    ..Default::default()
                }
            }
            RemoveOutcome::SharedFolder { .. } => {
                self.shared = true;
                self.choice = RemoveMode::KeepFiles;
                self.step = DeleteStep::Confirm;
                self.notice = None;
                DeleteEffect::default()
            }
            RemoveOutcome::FolderMissing => {
                self.missing = true;
                self.choice = RemoveMode::KeepFiles;
                self.step = DeleteStep::Confirm;
                self.notice = None;
                self.scan = ScanJob::finished(Err("the folder is missing".into()));
                DeleteEffect::default()
            }
            RemoveOutcome::NowRunning => {
                self.step = refused_step;
                self.notice = Some(format!(
                    "{name} started while this was open. Close Minecraft, then try again."
                ));
                DeleteEffect::default()
            }
            RemoveOutcome::TrashFailed { kind, error, .. } => {
                self.step = DeleteStep::TrashFailed { kind, error };
                self.trash_failure = Some(self.step.clone());
                self.notice = None;
                self.focus_safe_button = true;
                DeleteEffect::default()
            }
            RemoveOutcome::TrashedButNotSaved { profile, error } => DeleteEffect {
                close: true,
                status: Some(format!(
                    "Moved {} to the {}, but the instance list couldn't be saved ({error}). \
                     It now shows as missing; remove it from Ferrite to finish.",
                    profile.name,
                    trash_word()
                )),
                removed: None,
            },
            RemoveOutcome::Failed(error) => {
                self.step = refused_step;
                self.notice = Some(format!("Couldn't remove {name}: {error}"));
                DeleteEffect::default()
            }
        }
    }
}

/// §3.3 first line, by failure kind.
pub(super) fn trash_failed_reason(kind: &TrashFailureKind) -> String {
    let word = trash_word();
    match kind {
        TrashFailureKind::TooLarge => format!("It's too big for the {word} on this drive."),
        TrashFailureKind::NoTrashOnDrive => {
            format!("The drive it's on doesn't have a {word} (network and some USB drives don't).")
        }
        TrashFailureKind::InUse => {
            "Some of its files are in use. Close any programs using them and try again.".into()
        }
        TrashFailureKind::PermissionDenied => "Ferrite isn't allowed to move this folder.".into(),
        TrashFailureKind::Other(_) => "Something went wrong.".into(),
    }
}

/// §3.3 second line, always the same wording (a partial move can't be told apart from
/// a clean failure). Returned as (text before the path, text after the path).
pub(super) fn trash_failed_leftover_text() -> (String, &'static str) {
    (
        format!(
            "Ferrite didn't delete anything, but some files may already be in the {}. \
             The rest are still in ",
            trash_word()
        ),
        ".",
    )
}

/// §3.4 body: "This can't be undone. 4.2 GiB and 3 worlds will be deleted for good."
pub(super) fn permanent_body(scan: Option<&InstanceScan>) -> String {
    match scan {
        Some(scan) if scan.worlds.is_empty() => format!(
            "This can't be undone. {} will be deleted for good.",
            format_size(scan.bytes)
        ),
        Some(scan) => format!(
            "This can't be undone. {} and {} will be deleted for good.",
            format_size(scan.bytes),
            super::startup::plural(scan.worlds.len() as u64, "world", "worlds")
        ),
        None => {
            "This can't be undone. Everything left in its folder will be deleted for good.".into()
        }
    }
}

/// §3.4 checkbox wording.
pub(super) fn permanent_checkbox(name: &str, has_worlds: bool) -> String {
    if has_worlds {
        format!("I understand {name} and its worlds can't be recovered.")
    } else {
        format!("I understand {name} can't be recovered.")
    }
}

/// The worker's result for the slow file step.
pub(super) struct RemoveTask {
    operation: OperationId,
    target: RemovalTarget,
    result: Receiver<Result<(), RemoveOutcome>>,
}

fn is_running(directory: &InstanceDirName) -> bool {
    crate::minecraft::is_instance_running(directory.as_str())
}

/// What the user clicked this frame.
enum DeleteAction {
    Cancel,
    Start(RemoveMode),
    ToPermanent,
    Back,
    OpenFolder(PathBuf),
}

impl Ferrite {
    /// Opens the delete dialog for the instance in `directory`.
    pub(super) fn open_remove_dialog(&mut self, directory: InstanceDirName) {
        if self.remove_dialog.is_some() {
            return;
        }
        let Some(profile) = self
            .instances
            .iter()
            .find(|profile| profile.directory() == &directory)
            .cloned()
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
        let shared = matches!(precheck, Err(RemoveOutcome::SharedFolder { .. }));
        let missing = matches!(precheck, Err(RemoveOutcome::FolderMissing));
        let scan = if missing {
            ScanJob::finished(Err("the folder is missing".into()))
        } else {
            ScanJob::start(&self.paths, &profile)
        };
        let folder = profile.game_dir(&self.paths);
        self.remove_dialog = Some(DeleteDialog::new(profile, folder, shared, missing, scan));
    }

    /// Whether the delete dialog has background work to watch (scan or removal).
    pub(super) fn remove_dialog_busy(&self) -> bool {
        self.remove_dialog
            .as_ref()
            .is_some_and(|dialog| dialog.scan.is_running())
    }

    /// Starts the chosen removal. KeepFiles finishes immediately; trash and permanent
    /// delete run their file step on a worker thread.
    fn start_removal(&mut self, mode: RemoveMode) {
        if self.remove_task.is_some() {
            return;
        }
        let Some(dialog) = self.remove_dialog.as_ref() else {
            return;
        };
        let directory = dialog.directory().clone();
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
        let status = self.instance_status(target.profile());
        let action = if mode == RemoveMode::KeepFiles {
            InstanceAction::RemoveFromList
        } else {
            InstanceAction::Delete
        };
        let operation = match self.activity.begin_instance(
            action,
            target.profile(),
            &status,
            BusyOperation::Deleting,
        ) {
            Ok(operation) => operation,
            Err(reason) => {
                if let Some(dialog) = self.remove_dialog.as_mut() {
                    dialog.notice = Some(reason);
                }
                return;
            }
        };
        let paths = self.paths.clone();
        let worker_target = target.clone();
        let (sender, result) = mpsc::channel();
        let started = remove::start_prepared_removal(
            &self.paths,
            &mut self.instances,
            &self.skipped_instances,
            &target,
            &is_running,
            || {
                std::thread::Builder::new()
                    .name("instance-remove".into())
                    .spawn(move || {
                        let outcome = remove::run_removal_files(
                            &paths,
                            &worker_target,
                            &SystemTrash,
                            &is_running,
                        );
                        let _ = sender.send(outcome);
                    })
                    .map(|_| ())
            },
        );
        match started {
            Ok(remove::RemovalStart::Completed(outcome)) | Err(outcome) => {
                self.apply_remove_outcome(outcome);
                self.activity.complete(operation);
            }
            Ok(remove::RemovalStart::Scheduled) => {
                if mode == RemoveMode::Permanent {
                    self.after_instance_removed(&directory);
                }
                self.remove_task = Some(RemoveTask {
                    operation,
                    target,
                    result,
                });
                if let Some(dialog) = self.remove_dialog.as_mut() {
                    dialog.begin_work(mode);
                }
            }
        }
    }

    /// Drains the removal worker and finishes on the UI thread.
    pub(super) fn poll_remove_task(&mut self) {
        if let Some(dialog) = self.remove_dialog.as_mut() {
            dialog.scan.poll();
        }
        let Some(task) = &self.remove_task else {
            return;
        };
        let result = match task.result.try_recv() {
            Ok(result) => result,
            Err(TryRecvError::Empty) => return,
            Err(TryRecvError::Disconnected) => Err(RemoveOutcome::Failed(InstanceError::Install(
                "the removal worker stopped unexpectedly".into(),
            ))),
        };
        let task = self.remove_task.take().expect("checked above");
        let operation = task.operation;
        let outcome = remove::finish_removal(
            &self.paths,
            &mut self.instances,
            &self.skipped_instances,
            task.target,
            result,
        );
        self.apply_remove_outcome(outcome);
        self.activity.complete(operation);
        if self.close_after_remove {
            self.close_requested = true;
        }
    }

    /// Clears selection/mod state that pointed at a removed instance.
    pub(super) fn after_instance_removed(&mut self, directory: &InstanceDirName) {
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

    /// Applies an outcome to the dialog, the status line, and the selection.
    fn apply_remove_outcome(&mut self, outcome: RemoveOutcome) {
        let effect = match self.remove_dialog.as_mut() {
            Some(dialog) => dialog.apply_outcome(outcome),
            // The dialog is always open while a removal runs; be safe anyway.
            None => DeleteDialog::new(
                InstanceProfile::new(String::new(), String::new(), String::new(), &[]),
                PathBuf::new(),
                false,
                false,
                ScanJob::finished(Ok(InstanceScan::default())),
            )
            .apply_outcome(outcome),
        };
        if let Some(directory) = &effect.removed {
            self.after_instance_removed(directory);
        }
        if let Some(status) = effect.status {
            self.running_text = status;
        }
        if effect.close {
            self.remove_dialog = None;
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
        let muted_color = self.muted_color();
        let paths = self.paths.clone();
        let Some(dialog) = self.remove_dialog.as_mut() else {
            return;
        };
        let mut action = None;
        egui::Modal::new(egui::Id::new("delete-instance")).show(context, |ui| {
            ui.set_width(460.0);
            action = delete_dialog_ui(ui, dialog, muted_color);
        });
        match action {
            Some(DeleteAction::Cancel) => self.remove_dialog = None,
            Some(DeleteAction::Start(mode)) => self.start_removal(mode),
            Some(DeleteAction::ToPermanent) => {
                if let Some(dialog) = self.remove_dialog.as_mut() {
                    let rescan = ScanJob::start(&paths, &dialog.profile);
                    dialog.return_step = dialog.step.clone();
                    dialog.begin_permanent(rescan);
                }
            }
            Some(DeleteAction::Back) => {
                if let Some(dialog) = self.remove_dialog.as_mut() {
                    dialog.back_from_permanent();
                }
            }
            Some(DeleteAction::OpenFolder(folder)) => {
                if let Some(message) = dialogs::open_folder(&folder) {
                    self.running_text = message;
                }
            }
            None => {}
        }
    }
}

/// Draws one frame of the delete dialog and returns the user's action.
fn delete_dialog_ui(
    ui: &mut egui::Ui,
    dialog: &mut DeleteDialog,
    muted_color: egui::Color32,
) -> Option<DeleteAction> {
    let name = dialog.name().to_owned();
    let word = trash_word();
    let mut action = None;
    let focus = std::mem::take(&mut dialog.focus_safe_button);
    if !dialog.step.is_working()
        && !matches!(dialog.step, DeleteStep::PartiallyDeleted { .. })
        && escape_pressed(ui)
    {
        return Some(match dialog.step {
            DeleteStep::ConfirmPermanent { .. } => DeleteAction::Back,
            _ => DeleteAction::Cancel,
        });
    }
    let notice = |ui: &mut egui::Ui, dialog: &DeleteDialog| {
        if let Some(notice) = &dialog.notice {
            ui.add(egui::Label::new(RichText::new(notice).strong()).wrap());
        }
    };
    match dialog.step.clone() {
        DeleteStep::Confirm => {
            ui.heading(format!("Delete {name}?"));
            if dialog.choice == RemoveMode::Trash {
                ui.add(
                    egui::Label::new(format!(
                        "{name} will be moved to the {word} and removed from Ferrite. You can \
                         restore the folder from the {word} later."
                    ))
                    .wrap(),
                );
            } else if dialog.missing {
                ui.add(egui::Label::new(format!("{name} will be removed from Ferrite.")).wrap());
            } else {
                ui.add(
                    egui::Label::new(format!(
                        "{name} will be removed from Ferrite. Its folder stays where it is."
                    ))
                    .wrap(),
                );
            }
            ui.add_space(4.0);
            if path_fact(ui, "Folder", &dialog.folder, !dialog.missing, muted_color) {
                action = Some(DeleteAction::OpenFolder(dialog.folder.clone()));
            }
            if dialog.missing {
                muted(
                    ui,
                    "Ferrite can't find this instance's folder. It may have been moved or deleted.",
                    muted_color,
                );
            } else {
                match dialog.scan.result() {
                    None => fact(ui, "Size", "Calculating…", muted_color),
                    Some(Ok(scan)) => {
                        fact(ui, "Size", size_line(scan), muted_color);
                        if let Some(worlds) = worlds_line(&scan.worlds) {
                            fact(ui, "Worlds", worlds, muted_color);
                            ui.label(RichText::new("Its worlds go with it.").strong());
                        }
                    }
                    Some(Err(_)) => fact(ui, "Size", "Unknown", muted_color),
                }
            }
            ui.add_space(4.0);
            if dialog.shared {
                muted(
                    ui,
                    format!(
                        "Another entry in your instance list uses this same folder, so Ferrite \
                         won't delete it. You can still remove {name} from the list."
                    ),
                    muted_color,
                );
            }
            let trash_allowed = dialog.trash_allowed();
            ui.add_enabled_ui(trash_allowed, |ui| {
                ui.radio_value(
                    &mut dialog.choice,
                    RemoveMode::Trash,
                    format!("Move to {word}"),
                );
            });
            ui.radio_value(
                &mut dialog.choice,
                RemoveMode::KeepFiles,
                "Remove from Ferrite, keep the files.",
            );
            ui.indent("keep-files-help", |ui| {
                // With the folder missing there's nothing to "stay where it is".
                let help = if dialog.missing {
                    "Ferrite won't list it anymore."
                } else {
                    "The folder stays where it is. Ferrite won't list it anymore."
                };
                muted(ui, help, muted_color);
            });
            if !trash_allowed {
                dialog.choice = RemoveMode::KeepFiles;
            }
            notice(ui, dialog);
            ui.add_space(6.0);
            let label = if dialog.choice == RemoveMode::Trash {
                format!("Move to {word}")
            } else {
                "Remove from Ferrite".to_owned()
            };
            let choice = dialog.choice;
            if let Some(clicked) = dialogs::button_row(
                ui,
                |ui| {
                    let cancel = ui.button("Cancel");
                    if focus {
                        cancel.request_focus();
                    }
                    if cancel.clicked() {
                        return Some(DeleteAction::Cancel);
                    }
                    None
                },
                |ui| {
                    if danger_button(ui, &label, true) {
                        return Some(DeleteAction::Start(choice));
                    }
                    None
                },
            ) {
                action = Some(clicked);
            }
        }
        DeleteStep::Moving | DeleteStep::Deleting => {
            let moving = dialog.step == DeleteStep::Moving;
            ui.heading(if moving {
                format!("Moving {name} to the {word}…")
            } else {
                format!("Deleting {name}…")
            });
            ui.add(egui::ProgressBar::new(0.0).animate(true));
            if moving {
                muted(
                    ui,
                    "This can take a few minutes if the instance is on another drive.",
                    muted_color,
                );
            }
        }
        DeleteStep::TrashFailed { kind, error } => {
            ui.heading(format!("Couldn't move {name} to the {word}"));
            ui.add(egui::Label::new(trash_failed_reason(&kind)).wrap());
            let (before, after) = trash_failed_leftover_text();
            sentence(
                ui,
                &[
                    Segment::text(before),
                    Segment::path(&dialog.folder),
                    Segment::text(after),
                ],
            );
            if matches!(kind, TrashFailureKind::Other(_)) {
                details(ui, "delete-trash-error", &[error]);
            }
            notice(ui, dialog);
            ui.add_space(6.0);
            let in_use = kind == TrashFailureKind::InUse;
            if let Some(clicked) = dialogs::button_row(
                ui,
                |ui| {
                    let keep = ui.button(format!("Keep {name}"));
                    if focus {
                        keep.request_focus();
                    }
                    if keep.clicked() {
                        return Some(DeleteAction::Cancel);
                    }
                    None
                },
                |ui| {
                    if in_use {
                        if ui.button("Try again").clicked() {
                            return Some(DeleteAction::Start(RemoveMode::Trash));
                        }
                    } else if danger_button(ui, "Delete permanently…", true) {
                        return Some(DeleteAction::ToPermanent);
                    }
                    if ui.button("Remove from Ferrite only").clicked() {
                        return Some(DeleteAction::Start(RemoveMode::KeepFiles));
                    }
                    None
                },
            ) {
                action = Some(clicked);
            }
        }
        DeleteStep::ConfirmPermanent { mut acknowledged } => {
            ui.heading(format!("Delete {name} permanently?"));
            let scanning = dialog.scan.is_running();
            if scanning {
                ui.horizontal(|ui| {
                    ui.spinner();
                    muted(ui, "Checking what's left in the folder…", muted_color);
                });
            } else {
                ui.add(egui::Label::new(permanent_body(dialog.scan.scan())).wrap());
            }
            let has_worlds = dialog
                .scan
                .scan()
                .is_none_or(|scan| !scan.worlds.is_empty());
            ui.add_enabled_ui(!scanning, |ui| {
                ui.checkbox(&mut acknowledged, permanent_checkbox(&name, has_worlds));
            });
            dialog.step = DeleteStep::ConfirmPermanent { acknowledged };
            notice(ui, dialog);
            ui.add_space(6.0);
            let ready = dialog.permanent_ready();
            if let Some(clicked) = dialogs::button_row(
                ui,
                |ui| {
                    let back = ui.button("Back");
                    if focus {
                        back.request_focus();
                    }
                    if back.clicked() {
                        return Some(DeleteAction::Back);
                    }
                    None
                },
                |ui| {
                    if danger_button(ui, "Delete permanently", ready) {
                        return Some(DeleteAction::Start(RemoveMode::Permanent));
                    }
                    None
                },
            ) {
                action = Some(clicked);
            }
        }
        DeleteStep::PartiallyDeleted {
            folder,
            failed,
            error,
        } => {
            ui.heading(format!("Couldn't delete all of {name}"));
            ui.add(
                egui::Label::new(format!(
                    "Most of {name} was deleted, but some files couldn't be removed."
                ))
                .wrap(),
            );
            let mut lines: Vec<String> = failed
                .iter()
                .map(|path| path.display().to_string())
                .collect();
            lines.push(error);
            details(ui, "delete-partial", &lines);
            ui.add_space(6.0);
            if let Some(clicked) = dialogs::button_row(
                ui,
                |ui| {
                    let close = ui.button("Close");
                    if focus {
                        close.request_focus();
                    }
                    if close.clicked() || escape_pressed(ui) {
                        return Some(DeleteAction::Cancel);
                    }
                    None
                },
                |ui| {
                    if ui.button("Open folder").clicked() {
                        return Some(DeleteAction::OpenFolder(folder.clone()));
                    }
                    None
                },
            ) {
                action = Some(clicked);
            }
        }
    }
    action
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocked_remove_from_list_preserves_manifest_and_existing_owner() {
        let mut app = crate::app::tests::app();
        let profile = profile();
        app.instances.push(profile.clone());
        app.remove_dialog = Some(dialog(false, true));
        let owner = app.activity.begin_create().unwrap();
        app.start_removal(RemoveMode::KeepFiles);
        assert_eq!(app.instances, vec![profile]);
        assert!(app.remove_task.is_none());
        assert!(app.activity.is_active(owner));
        assert!(app.remove_dialog.as_ref().unwrap().notice.is_some());
        assert_eq!(
            app.activity.busy(app.instances[0].directory().as_str()),
            None
        );
    }

    fn profile() -> InstanceProfile {
        InstanceProfile::new("Survival".into(), "1.21.1".into(), "Fabric".into(), &[])
    }

    fn dialog(shared: bool, missing: bool) -> DeleteDialog {
        DeleteDialog::new(
            profile(),
            PathBuf::from("/data/instances/survival"),
            shared,
            missing,
            ScanJob::finished(Ok(InstanceScan::default())),
        )
    }

    fn trash_failed(kind: TrashFailureKind) -> RemoveOutcome {
        RemoveOutcome::TrashFailed {
            kind,
            error: "raw".into(),
            folder_still_present: true,
        }
    }

    #[test]
    fn shared_or_missing_folders_offer_only_remove_from_list() {
        let open = dialog(false, false);
        assert_eq!(open.choice, RemoveMode::Trash);
        assert!(open.trash_allowed());
        for (shared, missing) in [(true, false), (false, true)] {
            let dialog = dialog(shared, missing);
            assert_eq!(dialog.choice, RemoveMode::KeepFiles);
            assert!(!dialog.trash_allowed());
        }
        // Learned only after clicking: same result.
        let mut late = dialog(false, false);
        late.begin_work(RemoveMode::Trash);
        let effect = late.apply_outcome(RemoveOutcome::SharedFolder {
            other: "Other".into(),
        });
        assert_eq!(effect, DeleteEffect::default());
        assert_eq!(late.step, DeleteStep::Confirm);
        assert_eq!(late.choice, RemoveMode::KeepFiles);
        assert!(!late.trash_allowed());
    }

    #[test]
    fn trash_success_closes_with_a_status_and_removes_the_entry() {
        let mut dialog = dialog(false, false);
        dialog.begin_work(RemoveMode::Trash);
        assert_eq!(dialog.step, DeleteStep::Moving);
        let effect = dialog.apply_outcome(RemoveOutcome::Trashed { profile: profile() });
        assert!(effect.close);
        assert_eq!(
            effect.status.unwrap(),
            format!("Moved Survival to the {}.", trash_word())
        );
        assert_eq!(effect.removed.as_ref(), Some(profile().directory()));
    }

    #[test]
    fn now_running_returns_to_the_screen_the_action_started_from() {
        let mut dialog = dialog(false, false);
        dialog.apply_outcome(trash_failed(TrashFailureKind::InUse));
        // [Try again] from the trash-failed screen.
        dialog.begin_work(RemoveMode::Trash);
        let effect = dialog.apply_outcome(RemoveOutcome::NowRunning);
        assert!(!effect.close && effect.removed.is_none());
        assert!(matches!(
            dialog.step,
            DeleteStep::TrashFailed {
                kind: TrashFailureKind::InUse,
                ..
            }
        ));
        assert_eq!(
            dialog.notice.as_deref(),
            Some("Survival started while this was open. Close Minecraft, then try again.")
        );
        // Refused before any worker started (confirm screen).
        let mut fresh = self::dialog(false, false);
        fresh.apply_outcome(RemoveOutcome::NowRunning);
        assert_eq!(fresh.step, DeleteStep::Confirm);
        assert!(fresh.notice.is_some());
        // A new attempt clears the line.
        fresh.begin_work(RemoveMode::Trash);
        assert!(fresh.notice.is_none());
    }

    #[test]
    fn trash_failure_then_permanent_flow_rescans_and_needs_the_checkbox() {
        let mut dialog = dialog(false, false);
        dialog.begin_work(RemoveMode::Trash);
        let effect = dialog.apply_outcome(trash_failed(TrashFailureKind::TooLarge));
        assert_eq!(effect, DeleteEffect::default());
        assert!(matches!(dialog.step, DeleteStep::TrashFailed { .. }));

        // Entering the permanent step: the fresh scan is still running.
        let (_sender, receiver) = mpsc::channel();
        dialog.return_step = dialog.step.clone();
        dialog.begin_permanent(ScanJob::pending(receiver));
        dialog.step = DeleteStep::ConfirmPermanent { acknowledged: true };
        assert!(!dialog.permanent_ready(), "wait for the rescan");

        // What's left after the partial move.
        let left = InstanceScan {
            bytes: 1024 * 1024 * 1024,
            files: 10,
            worlds: vec!["Skyblock".into()],
            ..Default::default()
        };
        dialog.scan = ScanJob::finished(Ok(left.clone()));
        dialog.step = DeleteStep::ConfirmPermanent {
            acknowledged: false,
        };
        assert!(!dialog.permanent_ready(), "box not ticked");
        dialog.step = DeleteStep::ConfirmPermanent { acknowledged: true };
        assert!(dialog.permanent_ready());
        assert_eq!(
            permanent_body(dialog.scan.scan()),
            "This can't be undone. 1.0 GiB and 1 world will be deleted for good."
        );

        // Refused at the moment of deleting: stay on the permanent step...
        dialog.begin_work(RemoveMode::Permanent);
        dialog.apply_outcome(RemoveOutcome::NowRunning);
        assert_eq!(
            dialog.step,
            DeleteStep::ConfirmPermanent { acknowledged: true }
        );
        // ...and [Back] still returns to the trash-failed choices.
        dialog.back_from_permanent();
        assert!(matches!(dialog.step, DeleteStep::TrashFailed { .. }));
    }

    #[test]
    fn partial_delete_stays_open_but_reports_the_removed_entry() {
        let mut dialog = dialog(false, false);
        dialog.step = DeleteStep::ConfirmPermanent { acknowledged: true };
        dialog.begin_work(RemoveMode::Permanent);
        assert_eq!(dialog.step, DeleteStep::Deleting);
        let effect = dialog.apply_outcome(RemoveOutcome::PartiallyDeleted {
            profile: profile(),
            folder: PathBuf::from("/x"),
            failed: vec![PathBuf::from("/x/locked.dat")],
            error: "denied".into(),
        });
        assert!(!effect.close);
        assert!(effect.removed.is_some());
        assert!(matches!(dialog.step, DeleteStep::PartiallyDeleted { .. }));
    }

    #[test]
    fn trashed_but_not_saved_closes_without_removing_the_entry() {
        let mut dialog = dialog(false, false);
        dialog.begin_work(RemoveMode::Trash);
        let effect = dialog.apply_outcome(RemoveOutcome::TrashedButNotSaved {
            profile: profile(),
            error: "disk full".into(),
        });
        assert!(effect.close);
        assert!(effect.removed.is_none());
        assert!(effect.status.unwrap().contains("shows as missing"));
    }

    #[test]
    fn trash_failed_copy_matches_the_spec() {
        let word = trash_word();
        assert_eq!(
            trash_failed_reason(&TrashFailureKind::TooLarge),
            format!("It's too big for the {word} on this drive.")
        );
        assert_eq!(
            trash_failed_reason(&TrashFailureKind::Other("x".into())),
            "Something went wrong."
        );
        let (before, after) = trash_failed_leftover_text();
        assert_eq!(
            format!("{before}/data/instances/survival{after}"),
            format!(
                "Ferrite didn't delete anything, but some files may already be in the {word}. \
                 The rest are still in /data/instances/survival."
            )
        );
        assert_eq!(
            permanent_checkbox("Survival", false),
            "I understand Survival can't be recovered."
        );
        assert_eq!(
            permanent_checkbox("Survival", true),
            "I understand Survival and its worlds can't be recovered."
        );
        assert_eq!(
            permanent_body(Some(&InstanceScan {
                bytes: 2048,
                ..Default::default()
            })),
            "This can't be undone. 2.0 KiB will be deleted for good."
        );
    }
}
