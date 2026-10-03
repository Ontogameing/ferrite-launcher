//! Duplicate (Stage 2 UI spec §5): the setup modal with a live name check and a
//! background size/space check, the progress window, and the results.
//!
//! The copy itself is `core::duplicate` (staging folder + no-replace rename); this
//! module only starts it, shows its progress, and commits the result on the UI thread.

use super::Ferrite;
use super::dialogs::{
    self, button_row, enter_pressed, escape_pressed, muted, name_error_text, name_field,
    primary_button,
};
use super::startup::{details, group_thousands, plural, size_pair};
use crate::instances::InstanceProfile;
use eframe::egui::{self, RichText};
use ferrite_launcher::core::activity::BusyOperation;
use ferrite_launcher::core::duplicate::{
    self, DuplicateError, DuplicateProgress, DuplicateStep, DuplicateTask,
};
use ferrite_launcher::core::instances::{self as core_instances, InstanceDirName};
use ferrite_launcher::core::migration::format_size;
use ferrite_launcher::core::paths::AppPaths;
use ferrite_launcher::core::scan::{self, InstanceScan};
use std::path::{Component, Path, PathBuf};
use std::sync::mpsc::{self, Receiver, TryRecvError};

/// Not enough room for the copy (from the preflight).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct SpaceShortfall {
    needed: u64,
    available: u64,
    volume: PathBuf,
}

type SizeCheckResult = Result<(InstanceScan, Option<SpaceShortfall>), String>;

/// The open setup modal.
pub(super) struct DuplicateSetup {
    source: InstanceProfile,
    name: String,
    size: Option<SizeCheckResult>,
    size_job: Option<Receiver<SizeCheckResult>>,
    /// Shown in the dialog (started while open, couldn't start).
    notice: Option<String>,
    first_frame: bool,
}

/// What a running or finished duplicate shows in its window.
pub(super) enum JobState {
    Copying,
    /// Committed, but some links weren't copied.
    DoneWithSkippedLinks {
        new_name: String,
        links: Vec<PathBuf>,
    },
    Failed {
        reason: String,
        details: Vec<String>,
    },
}

/// One duplicate started from the setup modal.
pub(super) struct DuplicateJob {
    id: u64,
    source: InstanceProfile,
    new_name: String,
    task: Option<DuplicateTask>,
    state: JobState,
    pub(super) window_open: bool,
}

impl DuplicateJob {
    /// The source folder this job holds busy while copying.
    pub(super) fn source_directory(&self) -> &InstanceDirName {
        self.source.directory()
    }

    /// Copy progress, while copying.
    pub(super) fn progress(&self) -> Option<DuplicateProgress> {
        self.task.as_ref().map(|task| task.control().snapshot())
    }
}

/// `"Not enough space. Needs 4.2 GiB, 1.1 GiB free on C:."` (spec §5.1).
pub(super) fn space_message(shortfall: &SpaceShortfall) -> String {
    format!(
        "Not enough space. Needs {}, {} free on {}.",
        format_size(shortfall.needed),
        format_size(shortfall.available),
        drive_label(&shortfall.volume)
    )
}

/// The drive letter on Windows (`C:`), otherwise the folder itself.
fn drive_label(volume: &Path) -> String {
    match volume.components().next() {
        Some(Component::Prefix(prefix)) => prefix.as_os_str().to_string_lossy().into_owned(),
        _ => volume.display().to_string(),
    }
}

/// Progress label and bar fraction (`None` = indeterminate), matching Stage 1.
pub(super) fn progress_text(progress: &DuplicateProgress) -> (String, Option<f32>) {
    match progress.step {
        DuplicateStep::Starting | DuplicateStep::Scanning => ("Checking files…".into(), None),
        DuplicateStep::Copying => (
            format!(
                "Copying {} of {} · {}",
                group_thousands(progress.files_done),
                plural(progress.files_total, "file", "files"),
                size_pair(progress.bytes_done, progress.bytes_total)
            ),
            progress.fraction(),
        ),
        DuplicateStep::Finalizing | DuplicateStep::Done => ("Finishing up…".into(), None),
    }
}

/// `"Copying 42%"` for the source card's chip.
pub(super) fn chip_text(progress: &DuplicateProgress) -> String {
    match progress.step {
        DuplicateStep::Copying => {
            let percent = (progress.fraction().unwrap_or(0.0) * 100.0).floor() as u32;
            format!("Copying {}%", percent.min(99))
        }
        _ => "Copying…".into(),
    }
}

/// Plain words and Details for a failed duplicate of `name`.
pub(super) fn failure_text(name: &str, error: &DuplicateError) -> (String, Vec<String>) {
    let raw = vec![error.to_string()];
    match error {
        DuplicateError::UncopyableFiles(files) => (
            format!("Couldn't duplicate {name}. Some files couldn't be read:"),
            files.iter().map(ToString::to_string).collect(),
        ),
        DuplicateError::NowRunning => (
            format!("{name} started while this was open. Close Minecraft, then try again."),
            Vec::new(),
        ),
        DuplicateError::SourceMissing => (
            format!("Couldn't duplicate {name}. Ferrite can't find its folder."),
            raw,
        ),
        DuplicateError::NotEnoughSpace {
            needed,
            available,
            volume,
        } => (
            space_message(&SpaceShortfall {
                needed: *needed,
                available: *available,
                volume: volume.clone(),
            }),
            Vec::new(),
        ),
        DuplicateError::SourceChanged(_) => (
            format!("Couldn't duplicate {name}. Its files changed while they were being copied."),
            raw,
        ),
        DuplicateError::TargetExists(_) => (
            format!(
                "Couldn't duplicate {name}. Something appeared where the copy was going, so \
                 Ferrite stopped instead of replacing it."
            ),
            raw,
        ),
        DuplicateError::Cancelled => (format!("Stopped duplicating {name}."), Vec::new()),
        DuplicateError::Failed(_) | DuplicateError::WorkerStopped => (
            format!("Couldn't duplicate {name}. Something went wrong."),
            raw,
        ),
    }
}

/// Starts the scan + free-space preflight for the setup dialog.
fn start_size_check(
    paths: &AppPaths,
    source: &InstanceProfile,
) -> Option<Receiver<SizeCheckResult>> {
    let paths = paths.clone();
    let source = source.clone();
    let (sender, receiver) = mpsc::channel();
    std::thread::Builder::new()
        .name("duplicate-size".into())
        .spawn(move || {
            let result = scan::scan_instance(&paths, &source)
                .map_err(|error| error.to_string())
                .map(|scan| {
                    let shortfall = match duplicate::preflight_space(&paths, scan.bytes) {
                        Err(DuplicateError::NotEnoughSpace {
                            needed,
                            available,
                            volume,
                        }) => Some(SpaceShortfall {
                            needed,
                            available,
                            volume,
                        }),
                        // An unknown free space is checked again when the copy starts.
                        _ => None,
                    };
                    (scan, shortfall)
                });
            let _ = sender.send(result);
        })
        .ok()?;
    Some(receiver)
}

fn is_running(directory: &InstanceDirName) -> bool {
    crate::minecraft::is_instance_running(directory.as_str())
}

impl Ferrite {
    /// Opens the setup modal for the instance in `directory`.
    pub(super) fn open_duplicate_dialog(&mut self, directory: &InstanceDirName) {
        if self.duplicate_setup.is_some() {
            return;
        }
        let Some(source) = self
            .instances
            .iter()
            .find(|profile| profile.directory() == directory)
            .cloned()
        else {
            return;
        };
        let name =
            core_instances::suggest_name(&self.instances, &format!("{} (copy)", source.name));
        let size_job = start_size_check(&self.paths, &source);
        let size = size_job
            .is_none()
            .then(|| Err("couldn't start the size check".to_owned()));
        self.duplicate_setup = Some(DuplicateSetup {
            source,
            name,
            size,
            size_job,
            notice: None,
            first_frame: true,
        });
    }

    /// Background work the frame loop must keep polling.
    pub(super) fn duplicate_busy(&self) -> bool {
        self.duplicate_setup
            .as_ref()
            .is_some_and(|setup| setup.size_job.is_some())
            || self.duplicate_jobs.iter().any(|job| job.task.is_some())
    }

    /// The job copying `directory`, if any.
    pub(super) fn duplicate_job_for(&self, directory: &InstanceDirName) -> Option<&DuplicateJob> {
        self.duplicate_jobs
            .iter()
            .find(|job| job.task.is_some() && job.source_directory() == directory)
    }

    /// Reopens the progress window of the copy of `directory` (chip click).
    pub(super) fn show_duplicate_window(&mut self, directory: &InstanceDirName) {
        if let Some(job) = self
            .duplicate_jobs
            .iter_mut()
            .find(|job| job.task.is_some() && job.source_directory() == directory)
        {
            job.window_open = true;
        }
    }

    /// Starts copying `source` as `new_name`. Errors are plain-words messages.
    fn start_duplicate(&mut self, source: &InstanceDirName, new_name: &str) -> Result<u64, String> {
        let source_name = self
            .instances
            .iter()
            .find(|profile| profile.directory() == source)
            .map(|profile| profile.name.clone())
            .unwrap_or_default();
        if let Some(reason) = self
            .instances
            .iter()
            .find(|profile| profile.directory() == source)
            .and_then(|profile| {
                ferrite_launcher::core::activity::disabled_reason(
                    ferrite_launcher::core::activity::InstanceAction::Duplicate,
                    &profile.name,
                    &self.instance_status(profile),
                )
            })
        {
            // A running game is reported with the "started while open" wording.
            if !is_running(source) {
                return Err(reason);
            }
        }
        let plan = duplicate::prepare_duplicate(
            &self.paths,
            &self.instances,
            &self.skipped_instances,
            source,
            new_name,
            &is_running,
        )
        .map_err(|error| match &error {
            DuplicateError::Failed(core_instances::InstanceError::Name(name_error)) => {
                name_error_text(name_error)
            }
            _ => failure_text(&source_name, &error).0,
        })?;
        let source_profile = plan.source().clone();
        let new_name = plan.profile().name.clone();
        let task = DuplicateTask::spawn(self.paths.clone(), plan, |directory| {
            crate::minecraft::is_instance_running(directory.as_str())
        })
        .map_err(|error| format!("Couldn't start the copy: {error}"))?;
        self.activity
            .begin(source.as_str(), BusyOperation::Duplicating);
        self.next_job_id += 1;
        let id = self.next_job_id;
        self.duplicate_jobs.push(DuplicateJob {
            id,
            source: source_profile,
            new_name,
            task: Some(task),
            state: JobState::Copying,
            window_open: true,
        });
        Ok(id)
    }

    /// Polls the setup size check and finishes completed copies on the UI thread.
    pub(super) fn poll_duplicates(&mut self) {
        if let Some(setup) = self.duplicate_setup.as_mut()
            && let Some(receiver) = &setup.size_job
        {
            match receiver.try_recv() {
                Ok(result) => {
                    setup.size = Some(result);
                    setup.size_job = None;
                }
                Err(TryRecvError::Empty) => {}
                Err(TryRecvError::Disconnected) => {
                    setup.size = Some(Err("the size check stopped unexpectedly".into()));
                    setup.size_job = None;
                }
            }
        }
        for index in 0..self.duplicate_jobs.len() {
            let Some(result) = self.duplicate_jobs[index]
                .task
                .as_ref()
                .and_then(DuplicateTask::try_finish)
            else {
                continue;
            };
            let task = self.duplicate_jobs[index]
                .task
                .take()
                .expect("checked above");
            let source = self.duplicate_jobs[index].source.clone();
            self.activity.end(source.directory().as_str());
            let committed = result.and_then(|copied| {
                duplicate::commit_duplicate(
                    &self.paths,
                    &mut self.instances,
                    &self.skipped_instances,
                    copied,
                    Some(task.control()),
                )
            });
            let job = &mut self.duplicate_jobs[index];
            match committed {
                Ok(report) => {
                    self.selected_instance = Some(report.index);
                    self.scroll_to_instance = Some(report.profile.directory().clone());
                    self.running_text =
                        format!("Duplicated {} as {}.", source.name, report.profile.name);
                    if report.skipped_links.is_empty() {
                        job.window_open = false;
                    } else {
                        job.state = JobState::DoneWithSkippedLinks {
                            new_name: report.profile.name,
                            links: report.skipped_links,
                        };
                        job.window_open = true;
                    }
                }
                Err(DuplicateError::Cancelled) => {
                    job.window_open = false;
                    self.running_text =
                        format!("Stopped duplicating {}. Nothing was added.", source.name);
                }
                Err(error) => {
                    let (reason, details) = failure_text(&source.name, &error);
                    job.state = JobState::Failed { reason, details };
                    job.window_open = true;
                }
            }
        }
        // Finished jobs whose window was closed are done.
        self.duplicate_jobs
            .retain(|job| job.task.is_some() || job.window_open);
    }

    /// Draws the setup modal.
    pub(super) fn duplicate_setup_window(&mut self, context: &egui::Context) {
        let muted_color = self.muted_color();
        let accent = self.accent_color();
        let Some(setup) = self.duplicate_setup.as_mut() else {
            return;
        };
        let name_error = core_instances::validate_instance_name(&setup.name, &self.instances, None)
            .err()
            .map(|error| name_error_text(&error));
        let first = std::mem::take(&mut setup.first_frame);
        let mut cancel = false;
        let mut start = false;
        egui::Modal::new(egui::Id::new("duplicate-setup")).show(context, |ui| {
            ui.set_width(440.0);
            if escape_pressed(ui) {
                cancel = true;
            }
            ui.heading(format!("Duplicate {}", setup.source.name));
            ui.label("Name");
            name_field(
                ui,
                "duplicate-name",
                &mut setup.name,
                name_error.as_deref(),
                first,
            );
            ui.add_space(4.0);
            ui.label("Copies everything: worlds, mods, settings and screenshots.");
            let mut space_ok = true;
            match &setup.size {
                None => muted(ui, "Calculating…", muted_color),
                Some(Ok((scan, None))) => muted(ui, dialogs::size_line(scan), muted_color),
                Some(Ok((_, Some(shortfall)))) => {
                    space_ok = false;
                    ui.label(
                        RichText::new(space_message(shortfall)).color(ui.visuals().warn_fg_color),
                    );
                }
                Some(Err(_)) => muted(ui, "Size unknown.", muted_color),
            }
            if let Some(notice) = &setup.notice {
                ui.add(egui::Label::new(RichText::new(notice).strong()).wrap());
            }
            ui.add_space(6.0);
            let ready = name_error.is_none() && space_ok;
            let clicked = button_row(
                ui,
                |ui| ui.button("Cancel").clicked().then_some(false),
                |ui| {
                    primary_button(ui, "Duplicate", ready, accent)
                        .clicked()
                        .then_some(true)
                },
            );
            match clicked {
                Some(true) => start = true,
                Some(false) => cancel = true,
                None => {}
            }
            if ready && enter_pressed(ui) {
                start = true;
            }
        });
        if cancel {
            self.duplicate_setup = None;
        } else if start {
            let setup = self.duplicate_setup.as_ref().expect("open");
            let source = setup.source.directory().clone();
            let name = setup.name.clone();
            match self.start_duplicate(&source, &name) {
                Ok(_) => self.duplicate_setup = None,
                Err(message) => {
                    if let Some(setup) = self.duplicate_setup.as_mut() {
                        setup.notice = Some(message);
                    }
                }
            }
        }
    }

    /// Draws one window per duplicate job.
    pub(super) fn duplicate_windows(&mut self, context: &egui::Context) {
        let muted_color = self.muted_color();
        let mut retry = None;
        for job in &mut self.duplicate_jobs {
            if !job.window_open {
                continue;
            }
            let title = match &job.state {
                JobState::Copying => format!("Duplicating {}", job.source.name),
                JobState::DoneWithSkippedLinks { .. } => format!("Duplicated {}", job.source.name),
                JobState::Failed { .. } => format!("Couldn't duplicate {}", job.source.name),
            };
            let mut open = true;
            let mut close = false;
            egui::Window::new(title)
                .id(egui::Id::new(("duplicate-job", job.id)))
                .open(&mut open)
                .collapsible(false)
                .resizable(false)
                .default_width(440.0)
                .show(context, |ui| match &job.state {
                    JobState::Copying => {
                        let progress = job.progress().unwrap_or_default();
                        let (label, fraction) = progress_text(&progress);
                        let bar = match fraction {
                            Some(fraction) => egui::ProgressBar::new(fraction),
                            None => egui::ProgressBar::new(0.0).animate(true),
                        };
                        ui.add(bar);
                        ui.label(label);
                        let cancelled = job
                            .task
                            .as_ref()
                            .is_some_and(|task| task.control().is_cancelled());
                        if cancelled {
                            muted(ui, "Stopping…", muted_color);
                        } else if ui.button("Cancel").clicked()
                            && let Some(task) = &job.task
                        {
                            task.control().cancel();
                        }
                    }
                    JobState::DoneWithSkippedLinks { new_name, links } => {
                        ui.add(
                            egui::Label::new(format!(
                                "Duplicated {} as {new_name}, but {} weren't copied:",
                                job.source.name,
                                plural(links.len() as u64, "linked folder", "linked folders")
                            ))
                            .wrap(),
                        );
                        for link in links.iter().take(20) {
                            ui.add(
                                egui::Label::new(
                                    RichText::new(link.display().to_string()).monospace(),
                                )
                                .selectable(true)
                                .wrap(),
                            );
                        }
                        if links.len() > 20 {
                            muted(ui, format!("and {} more", links.len() - 20), muted_color);
                        }
                        ui.add_space(6.0);
                        if button_row(ui, |_| None, |ui| ui.button("Done").clicked().then_some(()))
                            .is_some()
                        {
                            close = true;
                        }
                    }
                    JobState::Failed {
                        reason,
                        details: lines,
                    } => {
                        ui.add(egui::Label::new(reason).wrap());
                        if !lines.is_empty() {
                            details(ui, ("duplicate-details", job.id), lines);
                        }
                        ui.label(format!(
                            "Nothing was added, and {} wasn't changed.",
                            job.source.name
                        ));
                        ui.add_space(6.0);
                        match button_row(
                            ui,
                            |ui| ui.button("Close").clicked().then_some(false),
                            |ui| ui.button("Try again").clicked().then_some(true),
                        ) {
                            Some(true) => retry = Some(job.id),
                            Some(false) => close = true,
                            None => {}
                        }
                    }
                });
            // Closing a copying window keeps the copy going (the chip reopens it).
            if !open || close {
                job.window_open = false;
            }
        }
        if let Some(id) = retry
            && let Some(position) = self.duplicate_jobs.iter().position(|job| job.id == id)
        {
            let job = self.duplicate_jobs.remove(position);
            if let Err(message) = self.start_duplicate(job.source.directory(), &job.new_name) {
                self.duplicate_jobs.insert(
                    position,
                    DuplicateJob {
                        state: JobState::Failed {
                            reason: message,
                            details: Vec::new(),
                        },
                        window_open: true,
                        ..job
                    },
                );
            }
        }
        self.duplicate_jobs
            .retain(|job| job.task.is_some() || job.window_open);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferrite_launcher::core::copy::UncopyableFile;

    #[test]
    fn progress_labels_match_stage_one_and_never_show_full_early() {
        let mut progress = DuplicateProgress {
            step: DuplicateStep::Scanning,
            ..Default::default()
        };
        assert_eq!(progress_text(&progress), ("Checking files…".into(), None));
        progress = DuplicateProgress {
            step: DuplicateStep::Copying,
            files_done: 1204,
            files_total: 5830,
            bytes_done: 1288490189,
            bytes_total: 5153960755,
        };
        let (label, fraction) = progress_text(&progress);
        assert_eq!(label, "Copying 1,204 of 5,830 files · 1.2 of 4.8 GiB");
        assert!((fraction.unwrap() - 0.25).abs() < 0.01);
        assert_eq!(chip_text(&progress), "Copying 25%");
        progress.bytes_done = progress.bytes_total;
        assert_eq!(chip_text(&progress), "Copying 99%");
        progress.step = DuplicateStep::Finalizing;
        assert_eq!(progress_text(&progress), ("Finishing up…".into(), None));
    }

    #[test]
    fn failures_use_plain_words_with_details() {
        let (reason, lines) = failure_text(
            "Survival",
            &DuplicateError::UncopyableFiles(vec![UncopyableFile {
                path: "saves/a/level.dat".into(),
                reason: "permission denied".into(),
            }]),
        );
        assert_eq!(
            reason,
            "Couldn't duplicate Survival. Some files couldn't be read:"
        );
        assert_eq!(lines.len(), 1);
        let (reason, _) = failure_text("Survival", &DuplicateError::NowRunning);
        assert_eq!(
            reason,
            "Survival started while this was open. Close Minecraft, then try again."
        );
        let (reason, lines) = failure_text("Survival", &DuplicateError::WorkerStopped);
        assert!(reason.ends_with("Something went wrong."));
        assert_eq!(lines.len(), 1);
    }

    #[test]
    fn space_message_names_the_drive() {
        let message = space_message(&SpaceShortfall {
            needed: 4_509_715_661,
            available: 1_181_116_006,
            volume: PathBuf::from("/data/instances"),
        });
        assert_eq!(
            message,
            "Not enough space. Needs 4.2 GiB, 1.1 GiB free on /data/instances."
        );
    }
}
