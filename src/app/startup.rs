//! Minimal blocking startup gate for the Stage 1 storage migration.
//!
//! This is deliberately thin: every decision comes from
//! [`ferrite_launcher::core::migration`], and this module only renders the current
//! state and forwards button presses. The real screens (per the UI/UX spec) can
//! replace the rendering functions without touching the core API.
//!
//! Stages:
//! - **Migrating**: progress bar with file/byte counts and a Cancel button. Cancel
//!   keeps the staged copy so the next attempt resumes.
//! - **Choosing**: two or more different old data folders were found. Each is listed
//!   with its path, instance count, size, file count, and last-modified time, with a
//!   "Use this one" button per location plus "Quit". Nothing is preselected.
//! - **Failed**: the plan or migration failed or was cancelled. Offers Retry, Quit,
//!   and (when a source is known) using the old location for this session only.
//! - **Ready**: the normal launcher UI.

use super::Ferrite;
use eframe::egui::{self, RichText};
use ferrite_launcher::core::migration::{
    self, Candidate, MigrationError, MigrationStep, MigrationTask, StartupPlan,
};
use ferrite_launcher::core::paths::AppPaths;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

enum Stage {
    Migrating {
        paths: AppPaths,
        source: PathBuf,
        task: MigrationTask,
    },
    Choosing {
        paths: AppPaths,
        candidates: Vec<Candidate>,
    },
    Failed {
        message: String,
        /// Old data folder that can be used in place for this session, if known.
        legacy_source: Option<PathBuf>,
    },
    Ready(Box<Ferrite>),
}

/// Pending transition requested by a button; applied after rendering.
enum Action {
    Choose(PathBuf),
    Cancel,
    Retry,
    UseLegacyForSession(PathBuf),
    Quit,
}

/// eframe app that runs the storage gate before handing over to [`Ferrite`].
pub(super) struct StartupApp {
    /// Standard storage paths resolved once in [`super::run`].
    paths: AppPaths,
    stage: Stage,
    /// Plan notes and migration results shown in the launcher status line.
    notes: Vec<String>,
}

impl StartupApp {
    pub(super) fn new(paths: AppPaths) -> Self {
        let mut app = Self {
            stage: Stage::Failed {
                message: String::new(),
                legacy_source: None,
            },
            paths,
            notes: Vec::new(),
        };
        app.plan();
        app
    }

    /// Asks the core what to do and moves to the matching stage.
    fn plan(&mut self) {
        let candidates = migration::default_legacy_candidate_dirs();
        self.stage = match migration::plan(&self.paths, &candidates) {
            Ok(StartupPlan::Ready { paths, notes, .. }) => {
                self.notes.extend(notes);
                self.ready(paths)
            }
            Ok(StartupPlan::NeedsMigration {
                paths,
                source,
                resuming,
                notes,
                ..
            }) => {
                self.notes.extend(notes);
                if resuming {
                    self.notes
                        .push("Resumed an interrupted storage migration.".to_owned());
                }
                self.start_migration(paths, source.path)
            }
            Ok(StartupPlan::NeedsUserChoice {
                paths,
                candidates,
                notes,
                ..
            }) => {
                self.notes.extend(notes);
                Stage::Choosing { paths, candidates }
            }
            Err(error) => Stage::Failed {
                message: format!("Ferrite could not check its storage: {error}"),
                legacy_source: None,
            },
        };
    }

    fn start_migration(&self, paths: AppPaths, source: PathBuf) -> Stage {
        match MigrationTask::spawn(paths.clone(), source.clone()) {
            Ok(task) => Stage::Migrating {
                paths,
                source,
                task,
            },
            Err(error) => Stage::Failed {
                message: format!("Could not start the storage migration: {error}"),
                legacy_source: Some(source),
            },
        }
    }

    fn ready(&self, paths: AppPaths) -> Stage {
        let mut app = Ferrite::new(paths);
        if !self.notes.is_empty() {
            app.running_text = format!("{} {}", self.notes.join(" "), app.running_text);
        }
        Stage::Ready(Box::new(app))
    }

    /// Polls a running migration and transitions when it finishes.
    fn poll(&mut self) {
        let Stage::Migrating {
            paths,
            source,
            task,
        } = &self.stage
        else {
            return;
        };
        let Some(result) = task.try_finish() else {
            return;
        };
        let (paths, source) = (paths.clone(), source.clone());
        self.stage = match result {
            Ok(report) => {
                if !report.already_complete {
                    self.notes.push(format!(
                        "Moved Ferrite data from {} to {} ({} files). The old folder was left in place.",
                        report.source.display(),
                        report.destination.display(),
                        report.files_copied
                    ));
                }
                if !report.skipped_links.is_empty() {
                    self.notes.push(format!(
                        "Skipped {} symbolic link(s) during migration.",
                        report.skipped_links.len()
                    ));
                }
                if !report.skipped_entries.is_empty() {
                    self.notes.push(format!(
                        "{} invalid instance entr(ies) were kept but not loaded.",
                        report.skipped_entries.len()
                    ));
                }
                self.ready(paths)
            }
            Err(MigrationError::Cancelled) => Stage::Failed {
                message: "Migration paused. Progress was kept; Retry resumes it. Your old data was not changed.".to_owned(),
                legacy_source: Some(source),
            },
            Err(error) => Stage::Failed {
                message: format!("Migration failed: {error}. Your old data was not changed."),
                legacy_source: Some(source),
            },
        };
    }

    fn apply(&mut self, action: Action, ctx: &egui::Context) {
        match action {
            Action::Choose(source) => {
                let paths = match &self.stage {
                    Stage::Choosing { paths, .. } => paths.clone(),
                    _ => self.paths.clone(),
                };
                self.stage = self.start_migration(paths, source);
            }
            Action::Cancel => {
                if let Stage::Migrating { task, .. } = &self.stage {
                    task.control().cancel();
                }
            }
            Action::Retry => self.plan(),
            Action::UseLegacyForSession(source) => {
                self.stage = match self.paths.with_legacy_storage_root(source.clone()) {
                    Ok(paths) => {
                        self.notes.push(format!(
                            "Using old data folder {} for this session only; migration will be offered again next launch.",
                            source.display()
                        ));
                        self.ready(paths)
                    }
                    Err(error) => Stage::Failed {
                        message: format!("Cannot use {}: {error}", source.display()),
                        legacy_source: None,
                    },
                };
            }
            Action::Quit => ctx.send_viewport_cmd(egui::ViewportCommand::Close),
        }
    }
}

impl eframe::App for StartupApp {
    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        if let Stage::Ready(app) = &mut self.stage {
            app.ui(ui, frame);
            return;
        }
        self.poll();
        let mut action = None;
        egui::Frame::new()
            .inner_margin(egui::Margin::same(32))
            .show(ui, |ui| match &self.stage {
                Stage::Migrating { task, .. } => {
                    action = migrating_ui(ui, task);
                    ui.ctx().request_repaint_after(Duration::from_millis(100));
                }
                Stage::Choosing { candidates, .. } => action = choosing_ui(ui, candidates),
                Stage::Failed {
                    message,
                    legacy_source,
                } => action = failed_ui(ui, message, legacy_source.as_deref()),
                Stage::Ready(_) => {}
            });
        if let Some(action) = action {
            let ctx = ui.ctx().clone();
            self.apply(action, &ctx);
            ctx.request_repaint();
        }
    }
}

fn migrating_ui(ui: &mut egui::Ui, task: &MigrationTask) -> Option<Action> {
    let progress = task.control().snapshot();
    ui.heading("Moving Ferrite data to its new location");
    ui.label(
        "Your instances and game files are being copied. The old folder is not changed or deleted.",
    );
    ui.add_space(12.0);
    ui.add(
        egui::ProgressBar::new(progress.fraction().unwrap_or(0.0))
            .show_percentage()
            .desired_width(ui.available_width().min(560.0)),
    );
    ui.label(format!(
        "{}: {} of {} files, {} of {}",
        step_label(progress.step),
        progress.files_done,
        progress.files_total,
        format_bytes(progress.bytes_done),
        format_bytes(progress.bytes_total),
    ));
    ui.add_space(12.0);
    let cancelling = task.control().is_cancelled();
    ui.add_enabled(!cancelling, egui::Button::new("Cancel"))
        .clicked()
        .then_some(Action::Cancel)
}

fn choosing_ui(ui: &mut egui::Ui, candidates: &[Candidate]) -> Option<Action> {
    let mut action = None;
    ui.heading("Choose which Ferrite data to use");
    ui.label(
        "Ferrite found more than one data folder from an older version, and they differ. \
         Choose one to copy to the new location. The others are left untouched.",
    );
    ui.add_space(12.0);
    egui::ScrollArea::vertical().show(ui, |ui| {
        for candidate in candidates {
            egui::Frame::group(ui.style()).show(ui, |ui| {
                ui.set_width(ui.available_width().min(640.0));
                ui.label(RichText::new(candidate.path.display().to_string()).strong());
                ui.label(format!(
                    "{} instance(s) · {} in {} files · last modified {}",
                    candidate.instance_count,
                    format_bytes(candidate.total_bytes),
                    candidate.file_count,
                    candidate
                        .last_modified
                        .map_or_else(|| "unknown".to_owned(), format_time),
                ));
                if ui.button("Use this one").clicked() {
                    action = Some(Action::Choose(candidate.path.clone()));
                }
            });
            ui.add_space(8.0);
        }
        if ui.button("Quit").clicked() {
            action = Some(Action::Quit);
        }
    });
    action
}

fn failed_ui(
    ui: &mut egui::Ui,
    message: &str,
    legacy_source: Option<&std::path::Path>,
) -> Option<Action> {
    let mut action = None;
    ui.heading("Ferrite storage needs attention");
    ui.label(message);
    ui.add_space(12.0);
    ui.horizontal(|ui| {
        if ui.button("Retry").clicked() {
            action = Some(Action::Retry);
        }
        if let Some(source) = legacy_source
            && ui
                .button("Use old location for this session")
                .on_hover_text(source.display().to_string())
                .clicked()
        {
            action = Some(Action::UseLegacyForSession(source.to_owned()));
        }
        if ui.button("Quit").clicked() {
            action = Some(Action::Quit);
        }
    });
    action
}

fn step_label(step: MigrationStep) -> &'static str {
    match step {
        MigrationStep::Starting => "Starting",
        MigrationStep::Scanning => "Scanning",
        MigrationStep::Copying => "Copying",
        MigrationStep::Verifying => "Verifying",
        MigrationStep::Finalizing => "Finishing",
        MigrationStep::Done => "Done",
    }
}

/// Formats a byte count with binary units (e.g. `1.5 GiB`).
fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// Formats a timestamp as `YYYY-MM-DD HH:MM UTC` without extra dependencies.
fn format_time(time: SystemTime) -> String {
    let Ok(since_epoch) = time.duration_since(UNIX_EPOCH) else {
        return "before 1970".to_owned();
    };
    let secs = since_epoch.as_secs();
    let (days, rem) = (secs / 86_400, secs % 86_400);
    let (year, month, day) = civil_from_days(days as i64);
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02} UTC",
        rem / 3600,
        (rem % 3600) / 60
    )
}

/// Days since 1970-01-01 to a proleptic Gregorian date (Howard Hinnant's algorithm).
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bytes_are_formatted_with_binary_units() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(1023), "1023 B");
        assert_eq!(format_bytes(1536), "1.5 KiB");
        assert_eq!(format_bytes(5 * 1024 * 1024 * 1024), "5.0 GiB");
    }

    #[test]
    fn timestamps_are_formatted_in_utc() {
        assert_eq!(format_time(UNIX_EPOCH), "1970-01-01 00:00 UTC");
        let time = UNIX_EPOCH + Duration::from_secs(1_790_000_000);
        assert_eq!(format_time(time), "2026-09-21 14:13 UTC");
        let leap = UNIX_EPOCH + Duration::from_secs(951_782_400);
        assert_eq!(format_time(leap), "2000-02-29 00:00 UTC");
    }
}
