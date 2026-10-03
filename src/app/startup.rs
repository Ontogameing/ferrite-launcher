//! Startup gate for the Stage 1 storage migration, following the UI spec in
//! `ferrite-specs/stage1-migration-ui.md`.
//!
//! Every decision comes from [`ferrite_launcher::core::migration`]; this module only
//! renders the current state and forwards button presses.
//!
//! Screens:
//! - **Old data it couldn't read**: the plan found folders with an `instances.json`
//!   that could not be used. Shown first, so an empty launcher never looks like data
//!   loss. "Continue without it" writes nothing; it is asked again next launch.
//! - **Copying**: progress by step, Pause, and closing the window pauses (with a
//!   one-line confirmation).
//! - **Paused**: after Pause; Resume continues from the staged copy.
//! - **Choose**: two or more different old folders; nothing is preselected.
//! - **Failed**: a plain message per error, details with Copy, "Use old data this
//!   time" and "Try again".
//! - **Ready**: the launcher, with a one-time card describing what happened.

use super::{ACCENT, Ferrite, MUTED};
use eframe::egui::{self, Color32, RichText};
use ferrite_launcher::core::migration::{
    self, Candidate, MigrationError, MigrationProgress, MigrationReport, MigrationStep,
    MigrationTask, Rejection, StartupPlan, format_size,
};
use ferrite_launcher::core::paths::AppPaths;
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const MAX_WIDTH: f32 = 560.0;
const MARGIN: f32 = 32.0;
/// Paths listed under "Details" before "and N more".
const DETAILS_LIMIT: usize = 20;
/// Instance names listed on a choice card before "and N more".
const NAMES_LIMIT: usize = 5;
const UNCHANGED: &str = "Your old data hasn't been changed.";
const USE_OLD_CONSEQUENCE: &str =
    "Ferrite will open with your old folder and offer the move again next time.";

/// The old folder being moved: where it was found and what it resolves to.
#[derive(Clone)]
struct Source {
    found_at: PathBuf,
    path: PathBuf,
}

impl Source {
    fn from_candidate(candidate: &Candidate) -> Self {
        Self {
            found_at: candidate.found_at.clone(),
            path: candidate.path.clone(),
        }
    }

    /// `old → resolved` when the old folder is a link, otherwise just the path.
    fn label(&self) -> String {
        link_label(&self.found_at, &self.path)
    }
}

enum Stage {
    /// Old data with an `instances.json` that couldn't be used; `next` continues.
    Unreadable {
        rejected: Vec<Rejection>,
        next: Box<StartupPlan>,
    },
    Migrating {
        paths: AppPaths,
        source: Source,
        task: MigrationTask,
        /// The window's close button was pressed; asking "Pause and quit?".
        confirm_close: bool,
        /// Close the window once the worker has stopped.
        quit_when_stopped: bool,
    },
    Paused {
        paths: AppPaths,
        source: Source,
    },
    Choosing {
        paths: AppPaths,
        candidates: Vec<Candidate>,
    },
    Failed {
        error: MigrationError,
        paths: AppPaths,
        source: Option<Source>,
    },
    Ready(Box<Ferrite>),
}

/// Pending transition requested by a button; applied after rendering.
enum Action {
    /// Past the "couldn't read" screen, without writing anything.
    ContinueWithoutRejected,
    Choose(usize),
    Pause,
    /// Answer to "Pause and quit?": `true` pauses and quits, `false` keeps going.
    PauseAndQuit(bool),
    Resume,
    TryAgain,
    UseOldData,
    OpenFolder(PathBuf),
    Quit,
}

/// eframe app that runs the storage gate before handing over to [`Ferrite`].
pub(super) struct StartupApp {
    /// Standard storage paths resolved once in [`super::run`].
    paths: AppPaths,
    stage: Stage,
    /// Plan notes shown in the launcher status line.
    notes: Vec<String>,
    /// Cards shown at the top of the launcher once it opens.
    cards: Vec<StartupCard>,
    /// "Continue without it" was chosen; don't ask again this session.
    rejections_acknowledged: bool,
    /// Last "Open folder" failure, shown under the buttons.
    open_error: Option<String>,
}

impl StartupApp {
    pub(super) fn new(paths: AppPaths) -> Self {
        let mut app = Self {
            stage: Stage::Failed {
                error: MigrationError::WorkerStopped,
                paths: paths.clone(),
                source: None,
            },
            paths,
            notes: Vec::new(),
            cards: Vec::new(),
            rejections_acknowledged: false,
            open_error: None,
        };
        app.plan();
        app
    }

    /// Asks the core what to do and moves to the matching stage.
    fn plan(&mut self) {
        let candidates = migration::default_legacy_candidate_dirs();
        self.stage = match migration::plan(&self.paths, &candidates) {
            Ok(plan) => self.enter(plan),
            Err(error) => Stage::Failed {
                error,
                paths: self.paths.clone(),
                source: None,
            },
        };
    }

    fn enter(&mut self, plan: StartupPlan) -> Stage {
        let rejected = match &plan {
            StartupPlan::Ready { rejected, .. }
            | StartupPlan::NeedsMigration { rejected, .. }
            | StartupPlan::NeedsUserChoice { rejected, .. } => rejected,
        };
        if !rejected.is_empty() && !self.rejections_acknowledged {
            return Stage::Unreadable {
                rejected: rejected.clone(),
                next: Box::new(plan),
            };
        }
        match plan {
            StartupPlan::Ready {
                paths,
                notes,
                ignored_legacy,
                ..
            } => {
                self.notes.extend(notes);
                for old in &ignored_legacy {
                    self.cards
                        .push(StartupCard::not_moved(paths.storage_root(), old));
                }
                self.ready(paths)
            }
            StartupPlan::NeedsMigration {
                paths,
                source,
                resuming,
                notes,
                ..
            } => {
                self.notes.extend(notes);
                if resuming {
                    self.notes
                        .push("Resumed an interrupted move of your data.".to_owned());
                }
                self.start_migration(paths, Source::from_candidate(&source))
            }
            StartupPlan::NeedsUserChoice {
                paths,
                candidates,
                notes,
                ..
            } => {
                self.notes.extend(notes);
                Stage::Choosing { paths, candidates }
            }
        }
    }

    fn start_migration(&self, paths: AppPaths, source: Source) -> Stage {
        match MigrationTask::spawn(paths.clone(), source.path.clone()) {
            Ok(task) => Stage::Migrating {
                paths,
                source,
                task,
                confirm_close: false,
                quit_when_stopped: false,
            },
            Err(error) => Stage::Failed {
                error: MigrationError::Io {
                    context: "start copying".to_owned(),
                    error,
                },
                paths,
                source: Some(source),
            },
        }
    }

    fn ready(&mut self, paths: AppPaths) -> Stage {
        let mut app = Ferrite::new(paths);
        if !self.notes.is_empty() {
            app.running_text = format!("{} {}", self.notes.join(" "), app.running_text);
        }
        app.startup_cards = std::mem::take(&mut self.cards);
        Stage::Ready(Box::new(app))
    }

    /// Turns the window's close button into "Pause and quit?" while copying.
    fn intercept_close(&mut self, ctx: &egui::Context) {
        if let Stage::Migrating { confirm_close, .. } = &mut self.stage
            && ctx.input(|input| input.viewport().close_requested())
        {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            *confirm_close = true;
        }
    }

    /// Polls a running migration and transitions when it finishes.
    fn poll(&mut self, ctx: &egui::Context) {
        let Stage::Migrating {
            paths,
            source,
            task,
            quit_when_stopped,
            ..
        } = &self.stage
        else {
            return;
        };
        let Some(result) = task.try_finish() else {
            return;
        };
        let (paths, source, quit) = (paths.clone(), source.clone(), *quit_when_stopped);
        self.stage = match result {
            Ok(report) => {
                self.cards.push(StartupCard::moved(&report, &source));
                self.ready(paths)
            }
            Err(MigrationError::Cancelled) => Stage::Paused { paths, source },
            Err(error) => Stage::Failed {
                error,
                paths,
                source: Some(source),
            },
        };
        if quit {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
    }

    fn apply(&mut self, action: Action, ctx: &egui::Context) {
        self.open_error = None;
        match action {
            Action::ContinueWithoutRejected => {
                if let Stage::Unreadable { next, .. } = &self.stage {
                    let plan = (**next).clone();
                    self.rejections_acknowledged = true;
                    self.stage = self.enter(plan);
                }
            }
            Action::Choose(index) => {
                if let Stage::Choosing { paths, candidates } = &self.stage
                    && let Some(candidate) = candidates.get(index)
                {
                    let (paths, source) = (paths.clone(), Source::from_candidate(candidate));
                    self.stage = self.start_migration(paths, source);
                }
            }
            Action::Pause => {
                if let Stage::Migrating { task, .. } = &self.stage {
                    task.control().cancel();
                }
            }
            Action::PauseAndQuit(quit) => {
                if let Stage::Migrating {
                    task,
                    confirm_close,
                    quit_when_stopped,
                    ..
                } = &mut self.stage
                {
                    *confirm_close = false;
                    if quit {
                        *quit_when_stopped = true;
                        task.control().cancel();
                    }
                }
            }
            Action::Resume => {
                if let Stage::Paused { paths, source } = &self.stage {
                    let (paths, source) = (paths.clone(), source.clone());
                    self.stage = self.start_migration(paths, source);
                }
            }
            // The core re-reads its state: an interrupted copy resumes, anything
            // else is planned from scratch.
            Action::TryAgain => self.plan(),
            Action::UseOldData => {
                let source = match &self.stage {
                    Stage::Paused { source, .. }
                    | Stage::Failed {
                        source: Some(source),
                        ..
                    } => source.path.clone(),
                    _ => return,
                };
                self.stage = match self.paths.with_legacy_storage_root(source.clone()) {
                    Ok(paths) => {
                        self.notes.push(
                            "Using your old data folder this time; Ferrite will offer the move again next time."
                                .to_owned(),
                        );
                        self.ready(paths)
                    }
                    Err(error) => Stage::Failed {
                        error: MigrationError::LegacyRootUnavailable {
                            path: source,
                            reason: error.to_string(),
                        },
                        paths: self.paths.clone(),
                        source: None,
                    },
                };
            }
            Action::OpenFolder(path) => {
                if let Err(error) = crate::config::open_folder(&path) {
                    self.open_error = Some(format!("Couldn't open {}: {error}", path.display()));
                }
            }
            Action::Quit => ctx.send_viewport_cmd(egui::ViewportCommand::Close),
        }
    }
}

impl eframe::App for StartupApp {
    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        if !matches!(self.stage, Stage::Ready(_)) {
            self.intercept_close(&ctx);
            self.poll(&ctx);
        }
        if let Stage::Ready(app) = &mut self.stage {
            app.ui(ui, frame);
            return;
        }
        let mut action = None;
        let open_error = self.open_error.as_deref();
        centered_column(ui, |ui| {
            match &self.stage {
                Stage::Unreadable { rejected, .. } => unreadable_ui(ui, rejected, &mut action),
                Stage::Migrating {
                    paths,
                    source,
                    task,
                    confirm_close,
                    ..
                } => {
                    migrating_ui(ui, paths, source, task, *confirm_close, &mut action);
                    ctx.request_repaint_after(Duration::from_millis(100));
                }
                Stage::Paused { source, .. } => paused_ui(ui, source, &mut action),
                Stage::Choosing { candidates, .. } => choosing_ui(ui, candidates, &mut action),
                Stage::Failed {
                    error,
                    paths,
                    source,
                } => failed_ui(ui, error, paths, source.as_ref(), &mut action),
                Stage::Ready(_) => {}
            }
            if let Some(message) = open_error {
                ui.add_space(8.0);
                ui.label(RichText::new(message).small().color(MUTED));
            }
        });
        if let Some(action) = action {
            self.apply(action, &ctx);
            ctx.request_repaint();
        }
    }
}

// =====================================================================
// Screens
// =====================================================================

fn unreadable_ui(ui: &mut egui::Ui, rejected: &[Rejection], action: &mut Option<Action>) {
    heading(ui, "Ferrite found old data it couldn't read");
    let mut buttons = Vec::new();
    if let [only] = rejected {
        sentence(
            ui,
            &[
                Segment::text("Your old data at "),
                Segment::path(&only.path),
                Segment::text(format!(
                    " is still there and hasn't been changed. Ferrite couldn't move it because: {}.",
                    only.reason.trim_end_matches('.')
                )),
            ],
        );
        buttons.push(("Open folder", Action::OpenFolder(only.path.clone())));
    } else {
        ui.label(
            "Your old data in these folders is still there and hasn't been changed. \
             Ferrite couldn't move it:",
        );
        for rejection in rejected {
            ui.add_space(6.0);
            path_line(
                ui,
                None,
                &rejection.path.display().to_string(),
                &rejection.path,
                action,
            );
            muted(ui, &format!("Because: {}", rejection.reason));
        }
    }
    buttons.push(("Continue without it", Action::ContinueWithoutRejected));
    button_row(ui, true, buttons, action);
}

fn migrating_ui(
    ui: &mut egui::Ui,
    paths: &AppPaths,
    source: &Source,
    task: &MigrationTask,
    confirm_close: bool,
    action: &mut Option<Action>,
) {
    let progress = task.control().snapshot();
    heading(ui, "Moving your Ferrite data");
    ui.label(
        "Ferrite now keeps its data in one standard place. Your instances are being \
         copied there. Your old folder isn't changed or deleted.",
    );
    ui.add_space(8.0);
    path_line(ui, Some("From:"), &source.label(), &source.path, action);
    let destination = paths.standard_storage_root();
    path_line(
        ui,
        Some("To:"),
        &destination.display().to_string(),
        &destination,
        action,
    );
    ui.add_space(16.0);
    match progress.step {
        MigrationStep::Starting | MigrationStep::Scanning => {
            indeterminate(ui, "Checking your files…");
        }
        MigrationStep::Copying | MigrationStep::Done => {
            ui.add(
                egui::ProgressBar::new(progress.fraction().unwrap_or(0.0))
                    .desired_width(ui.available_width()),
            );
            ui.label(copying_label(&progress));
        }
        MigrationStep::Verifying | MigrationStep::Finalizing => {
            indeterminate(ui, "Checking the copy…");
        }
    }
    let pausing = task.control().is_cancelled();
    if confirm_close && !pausing {
        ui.add_space(16.0);
        ui.label("Pause and quit? Ferrite will pick up where it left off next time.");
        button_row(
            ui,
            false,
            vec![
                ("Keep going", Action::PauseAndQuit(false)),
                ("Pause and quit", Action::PauseAndQuit(true)),
            ],
            action,
        );
    } else {
        ui.add_space(16.0);
        ui.horizontal(|ui| {
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let label = if pausing { "Pausing…" } else { "Pause" };
                if ui.add_enabled(!pausing, egui::Button::new(label)).clicked() {
                    *action = Some(Action::Pause);
                }
            });
        });
    }
}

fn paused_ui(ui: &mut egui::Ui, source: &Source, action: &mut Option<Action>) {
    heading(ui, "Move paused");
    ui.label(
        "Nothing was lost. The files copied so far are kept, and your old folder is untouched.",
    );
    ui.add_space(8.0);
    path_line(
        ui,
        Some("Old folder:"),
        &source.label(),
        &source.path,
        action,
    );
    button_row(
        ui,
        true,
        vec![
            ("Use old data this time", Action::UseOldData),
            ("Resume", Action::Resume),
        ],
        action,
    );
    muted(ui, USE_OLD_CONSEQUENCE);
}

fn choosing_ui(ui: &mut egui::Ui, candidates: &[Candidate], action: &mut Option<Action>) {
    heading(ui, "Which Ferrite data should we keep?");
    ui.label(
        "Ferrite found more than one folder from an older version, and they're different. \
         Pick one to move. The others stay exactly where they are.",
    );
    ui.add_space(12.0);
    let now = SystemTime::now();
    let most_recent = unique_max(
        &candidates
            .iter()
            .map(|candidate| candidate.last_modified)
            .collect::<Vec<_>>(),
    )
    .filter(|&index| candidates[index].last_modified.is_some());
    let most_instances = unique_max(
        &candidates
            .iter()
            .map(|candidate| candidate.instance_count)
            .collect::<Vec<_>>(),
    )
    .filter(|&index| candidates[index].instance_count > 0);
    for (index, candidate) in candidates.iter().enumerate() {
        egui::Frame::group(ui.style())
            .inner_margin(12.0)
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                let label = link_label(&candidate.found_at, &candidate.path);
                path_line(ui, None, &label, &candidate.path, action);
                let mut summary = format!(
                    "{} · {}",
                    plural(candidate.instance_count as u64, "instance", "instances"),
                    format_size(candidate.total_bytes),
                );
                if let Some(time) = candidate.last_modified {
                    summary.push_str(&format!(" · last changed {}", relative_time(time, now)));
                }
                let response = ui.label(summary);
                if let Some(absolute) = candidate.last_modified.and_then(local_time_label) {
                    response.on_hover_text(format!("Last changed {absolute}"));
                }
                if !candidate.instance_names.is_empty() {
                    ui.label(names_summary(&candidate.instance_names));
                }
                let skipped = candidate.skipped_entries.len() + candidate.skipped_links.len();
                if skipped > 0 {
                    let mut lines = candidate.skipped_entries.clone();
                    lines.extend(
                        candidate
                            .skipped_links
                            .iter()
                            .map(|link| format!("{} (shortcut or link)", link.display())),
                    );
                    muted(
                        ui,
                        &format!(
                            "{} won't be moved",
                            plural(skipped as u64, "entry", "entries")
                        ),
                    );
                    details(ui, ("skipped", index), &lines);
                }
                if !candidate.uncopyable.is_empty() {
                    muted(
                        ui,
                        &format!(
                            "{} can't be copied, so moving this folder would stop before finishing.",
                            plural(candidate.uncopyable.len() as u64, "file", "files")
                        ),
                    );
                    let lines: Vec<String> = candidate
                        .uncopyable
                        .iter()
                        .map(|file| {
                            format!("{} ({})", candidate.path.join(&file.path).display(), file.reason)
                        })
                        .collect();
                    details(ui, ("uncopyable", index), &lines);
                }
                ui.horizontal(|ui| {
                    for (tag, show) in [
                        ("Most recent", most_recent == Some(index)),
                        ("Most instances", most_instances == Some(index)),
                    ] {
                        if show {
                            ui.label(RichText::new(tag).small().color(MUTED));
                        }
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.add(primary_button("Move this one")).clicked() {
                            *action = Some(Action::Choose(index));
                        }
                    });
                });
            });
        ui.add_space(8.0);
    }
    button_row(ui, true, Vec::new(), action);
}

fn failed_ui(
    ui: &mut egui::Ui,
    error: &MigrationError,
    paths: &AppPaths,
    source: Option<&Source>,
    action: &mut Option<Action>,
) {
    let view = describe_failure(
        error,
        source.map(|source| source.path.as_path()),
        &paths.migration_state_file(),
    );
    heading(ui, view.heading);
    ui.label(&view.message);
    if let Some(path) = &view.path {
        ui.add_space(4.0);
        path_line(
            ui,
            None,
            &path.display().to_string(),
            path.parent().unwrap_or(path),
            action,
        );
    }
    if let Some(hint) = view.hint {
        ui.add_space(4.0);
        ui.label(hint);
    }
    if !view.details.is_empty() {
        ui.add_space(4.0);
        details(ui, "failure", &view.details);
    }
    ui.add_space(4.0);
    ui.label(UNCHANGED);

    let mut buttons = Vec::new();
    if source.is_some() {
        buttons.push(("Use old data this time", Action::UseOldData));
    }
    if view.open_data_folder {
        buttons.push((
            "Open data folder",
            Action::OpenFolder(paths.data_dir().to_owned()),
        ));
    }
    if view.can_retry {
        buttons.push(("Try again", Action::TryAgain));
    }
    button_row(ui, true, buttons, action);
    if source.is_some() {
        muted(ui, USE_OLD_CONSEQUENCE);
    }
}

// =====================================================================
// Layout helpers
// =====================================================================

/// Centered column, at most [`MAX_WIDTH`] wide with a [`MARGIN`] around it.
fn centered_column(ui: &mut egui::Ui, add_contents: impl FnOnce(&mut egui::Ui)) {
    let available = ui.available_width();
    let width = (available - 2.0 * MARGIN).clamp(120.0, MAX_WIDTH);
    let side = ((available - width) / 2.0).max(MARGIN);
    egui::ScrollArea::vertical()
        .auto_shrink(false)
        .show(ui, |ui| {
            ui.add_space(MARGIN);
            ui.horizontal_top(|ui| {
                ui.add_space(side);
                ui.vertical(|ui| {
                    ui.set_width(width);
                    add_contents(ui);
                });
            });
            ui.add_space(MARGIN);
        });
}

fn heading(ui: &mut egui::Ui, text: &str) {
    ui.label(RichText::new(text).heading().strong());
    ui.add_space(8.0);
}

fn muted(ui: &mut egui::Ui, text: &str) {
    ui.label(RichText::new(text).small().color(MUTED));
}

fn primary_button(label: &str) -> egui::Button<'static> {
    egui::Button::new(RichText::new(label).strong().color(Color32::WHITE)).fill(ACCENT)
}

/// Spinner with a label, for steps whose length isn't known.
fn indeterminate(ui: &mut egui::Ui, label: &str) {
    ui.horizontal(|ui| {
        ui.spinner();
        ui.label(label);
    });
}

/// [Quit] on the left (if `quit`), `buttons` on the right; the last one is the
/// primary action (accent fill, right-most, triggered by Enter when nothing has focus).
fn button_row(
    ui: &mut egui::Ui,
    quit: bool,
    buttons: Vec<(&str, Action)>,
    action: &mut Option<Action>,
) {
    ui.add_space(16.0);
    ui.horizontal(|ui| {
        if quit && ui.button("Quit").clicked() {
            *action = Some(Action::Quit);
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let count = buttons.len();
            // Right-to-left: add the primary (last) button first so it ends up right-most.
            for (index, (label, button_action)) in buttons.into_iter().enumerate().rev() {
                let primary = index + 1 == count;
                let button = if primary {
                    primary_button(label)
                } else {
                    egui::Button::new(label)
                };
                let enter = primary
                    && ui.input(|input| input.key_pressed(egui::Key::Enter))
                    && ui.memory(|memory| memory.focused().is_none());
                if ui.add(button).clicked() || enter {
                    *action = Some(button_action);
                }
            }
        });
    });
}

/// A small, muted, selectable, wrapped monospace path with an "Open folder" link
/// that opens `open`.
fn path_line(
    ui: &mut egui::Ui,
    label: Option<&str>,
    shown: &str,
    open: &Path,
    action: &mut Option<Action>,
) {
    ui.horizontal_wrapped(|ui| {
        if let Some(label) = label {
            ui.label(RichText::new(label).small().color(MUTED));
        }
        ui.add(
            egui::Label::new(RichText::new(shown).monospace().small().color(MUTED))
                .selectable(true)
                .wrap(),
        );
        if ui.link(RichText::new("Open folder").small()).clicked() {
            *action = Some(Action::OpenFolder(open.to_owned()));
        }
    });
}

/// Piece of a sentence that may contain paths.
pub(super) enum Segment {
    Text(String),
    Path(String),
}

impl Segment {
    pub(super) fn text(text: impl Into<String>) -> Self {
        Self::Text(text.into())
    }

    pub(super) fn path(path: &Path) -> Self {
        Self::Path(path.display().to_string())
    }
}

/// Renders text with inline monospace, selectable paths, wrapping as one paragraph.
pub(super) fn sentence(ui: &mut egui::Ui, segments: &[Segment]) {
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing.x = 0.0;
        for segment in segments {
            match segment {
                Segment::Text(text) => {
                    ui.label(text);
                }
                Segment::Path(path) => {
                    ui.add(
                        egui::Label::new(RichText::new(path).monospace())
                            .selectable(true)
                            .wrap(),
                    );
                }
            }
        }
    });
}

/// "Details" disclosure listing the first [`DETAILS_LIMIT`] lines, with a Copy
/// button that copies all of them.
pub(super) fn details(
    ui: &mut egui::Ui,
    id: impl std::hash::Hash + std::fmt::Debug,
    lines: &[String],
) {
    egui::CollapsingHeader::new(RichText::new("Details").small())
        .id_salt(id)
        .show(ui, |ui| {
            for line in lines.iter().take(DETAILS_LIMIT) {
                ui.add(
                    egui::Label::new(RichText::new(line).monospace().small())
                        .selectable(true)
                        .wrap(),
                );
            }
            if lines.len() > DETAILS_LIMIT {
                muted(ui, &format!("and {} more", lines.len() - DETAILS_LIMIT));
            }
            if ui.small_button("Copy").clicked() {
                ui.ctx().copy_text(lines.join("\n"));
            }
        });
}

// =====================================================================
// Failure text
// =====================================================================

/// What the "couldn't move your data" screen shows for one error.
struct FailureView {
    heading: &'static str,
    message: String,
    /// Extra advice, e.g. for online-only OneDrive files.
    hint: Option<&'static str>,
    /// Lines under "Details" (all of them are copied by the Copy button).
    details: Vec<String>,
    /// A file the user (or a bug report) needs, shown as a path.
    path: Option<PathBuf>,
    open_data_folder: bool,
    can_retry: bool,
}

/// Plain-language description of `error`. `source` resolves the relative paths of
/// uncopyable files; `state_file` is shown when the migration record is unreadable.
fn describe_failure(
    error: &MigrationError,
    source: Option<&Path>,
    state_file: &Path,
) -> FailureView {
    let mut view = FailureView {
        heading: "Ferrite couldn't move your data",
        message: String::new(),
        hint: None,
        details: Vec::new(),
        path: None,
        open_data_folder: false,
        can_retry: true,
    };
    match error {
        MigrationError::NotEnoughSpace {
            needed,
            available,
            volume,
        } => {
            view.message = format!(
                "There isn't enough free space on {}. Ferrite needs about {} and there's {} \
                 free. Free up some space and try again.",
                drive_label(volume),
                format_size(*needed),
                format_size(*available)
            );
        }
        MigrationError::VerificationFailed(detail) => {
            view.message =
                "The copy didn't match the original, so Ferrite threw the copy away.".to_owned();
            view.details.push(detail.clone());
        }
        MigrationError::UncopyableFiles(files) => {
            view.message = format!(
                "Ferrite couldn't copy {}, so it stopped before finishing. Nothing was moved.",
                plural(files.len() as u64, "file", "files")
            );
            let absolute: Vec<PathBuf> = files
                .iter()
                .map(|file| source.map_or_else(|| file.path.clone(), |root| root.join(&file.path)))
                .collect();
            if absolute.iter().any(|path| is_onedrive_path(path)) {
                view.hint = Some(
                    "These may be online-only OneDrive files. Make them available offline \
                     and try again.",
                );
            }
            view.details = files
                .iter()
                .zip(&absolute)
                .map(|(file, path)| format!("{} ({})", path.display(), file.reason))
                .collect();
        }
        MigrationError::SourceChanged(detail) => {
            view.message = "Your old folder changed while Ferrite was copying it. Close \
                            Minecraft and any other launcher using it, then try again."
                .to_owned();
            view.details.push(detail.clone());
        }
        MigrationError::Io { context, error } => {
            view.message = format!("Ferrite couldn't {context}.");
            view.details.push(error.to_string());
        }
        MigrationError::UnsupportedState { .. } => {
            view.heading = "Ferrite couldn't open its data";
            view.message =
                "This data was set up by a newer version of Ferrite. Update Ferrite to open it."
                    .to_owned();
            view.details.push(error.to_string());
            view.open_data_folder = true;
            view.can_retry = false;
        }
        MigrationError::StateUnreadable(detail) => {
            view.heading = "Ferrite couldn't open its data";
            view.message = "Ferrite's record of moving your data can't be read, so it stopped \
                            instead of guessing. The record is this file:"
                .to_owned();
            view.path = Some(state_file.to_owned());
            view.details.push(detail.clone());
            view.open_data_folder = true;
        }
        MigrationError::NotFerriteData { path, reason } => {
            view.message = format!(
                "{} doesn't look like Ferrite data: {}.",
                path.display(),
                reason.trim_end_matches('.')
            );
        }
        MigrationError::InvalidSource(detail) => {
            view.message = format!(
                "Ferrite can't move that folder: {}.",
                detail.trim_end_matches('.')
            );
        }
        MigrationError::DestinationExists(path) => {
            view.message = format!(
                "{} already has data, so Ferrite won't copy over it.",
                path.display()
            );
        }
        MigrationError::LegacyRootUnavailable { path, reason } => {
            view.message = format!(
                "Ferrite couldn't open your old folder {}: {}.",
                path.display(),
                reason.trim_end_matches('.')
            );
        }
        other => {
            view.message = "Ferrite couldn't finish moving your data.".to_owned();
            view.details.push(other.to_string());
        }
    }
    view
}

// =====================================================================
// Launcher cards (first frame after the gate)
// =====================================================================

/// A dismissible card shown at the top of the launcher after startup.
pub(super) struct StartupCard {
    segments: Vec<Segment>,
    /// Folder button label and target.
    open: (&'static str, PathBuf),
    /// Muted lines with a details list each.
    notes: Vec<(String, Vec<String>)>,
}

impl StartupCard {
    fn moved(report: &MigrationReport, source: &Source) -> Self {
        let lead = match report.instance_count {
            0 => "✔ Moved your Ferrite data to ".to_owned(),
            count => format!(
                "✔ Moved {} to ",
                plural(count as u64, "instance", "instances")
            ),
        };
        let mut notes = Vec::new();
        let links = report.skipped_links.len();
        if links > 0 {
            notes.push((
                if links == 1 {
                    "1 shortcut/link wasn't copied".to_owned()
                } else {
                    format!(
                        "{} shortcuts/links weren't copied",
                        group_thousands(links as u64)
                    )
                },
                report
                    .skipped_links
                    .iter()
                    .map(|link| link.display().to_string())
                    .collect(),
            ));
        }
        let entries = report.skipped_entries.len();
        if entries > 0 {
            notes.push((
                if entries == 1 {
                    "1 invalid instance entry was kept but not loaded".to_owned()
                } else {
                    format!(
                        "{} invalid instance entries were kept but not loaded",
                        group_thousands(entries as u64)
                    )
                },
                report.skipped_entries.clone(),
            ));
        }
        Self {
            segments: vec![
                Segment::Text(lead),
                Segment::path(&report.destination),
                Segment::text(". Your old folder at "),
                Segment::Path(source.label()),
                Segment::text(
                    " is still there. You can delete it yourself once you're happy everything works.",
                ),
            ],
            open: ("Open new folder", report.destination.clone()),
            notes,
        }
    }

    fn not_moved(destination: &Path, old: &Path) -> Self {
        Self {
            segments: vec![
                Segment::path(destination),
                Segment::text(" already has data, so old data at "),
                Segment::path(old),
                Segment::text(" wasn't moved."),
            ],
            open: ("Open old folder", old.to_owned()),
            notes: Vec::new(),
        }
    }
}

impl Ferrite {
    /// Draws the startup cards (migration result, old data not moved) until dismissed.
    pub(super) fn startup_cards_ui(&mut self, ui: &mut egui::Ui) {
        if self.startup_cards.is_empty() {
            return;
        }
        let (fill, accent, muted_color) =
            (self.card_color(), self.accent_color(), self.muted_color());
        let radius = self.ui_settings.appearance.corner_radius;
        let mut dismiss = None;
        let mut open = None;
        for (index, card) in self.startup_cards.iter().enumerate() {
            egui::Frame::new()
                .fill(fill)
                .stroke(egui::Stroke::new(1.0, accent))
                .corner_radius(radius)
                .inner_margin(12.0)
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    sentence(ui, &card.segments);
                    for (note_index, (line, lines)) in card.notes.iter().enumerate() {
                        ui.label(RichText::new(line).small().color(muted_color));
                        details(ui, ("startup-card", index, note_index), lines);
                    }
                    ui.add_space(4.0);
                    ui.horizontal(|ui| {
                        if ui.button(card.open.0).clicked() {
                            open = Some(card.open.1.clone());
                        }
                        if ui.button("Dismiss").clicked() {
                            dismiss = Some(index);
                        }
                    });
                });
            ui.add_space(8.0);
        }
        if let Some(path) = open
            && let Err(error) = crate::config::open_folder(&path)
        {
            self.running_text = format!("Couldn't open {}: {error}", path.display());
        }
        if let Some(index) = dismiss {
            self.startup_cards.remove(index);
        }
    }
}

// =====================================================================
// Pure formatting helpers
// =====================================================================

/// `old → resolved` when they differ (the old folder is a link), else the path.
fn link_label(found_at: &Path, resolved: &Path) -> String {
    if found_at == resolved {
        resolved.display().to_string()
    } else {
        format!("{} → {}", found_at.display(), resolved.display())
    }
}

/// `1234567` → `"1,234,567"`.
pub(super) fn group_thousands(value: u64) -> String {
    let digits = value.to_string();
    let mut grouped = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            grouped.push(',');
        }
        grouped.push(digit);
    }
    grouped
}

/// `"1 file"`, `"1,204 files"`.
pub(super) fn plural(count: u64, one: &str, many: &str) -> String {
    format!(
        "{} {}",
        group_thousands(count),
        if count == 1 { one } else { many }
    )
}

/// `"1.2 of 4.8 GiB"`: both numbers in the unit of `total`.
pub(super) fn size_pair(done: u64, total: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut unit = 0;
    let mut scale = 1.0_f64;
    while total as f64 / scale >= 1024.0 && unit < UNITS.len() - 1 {
        scale *= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{} of {} B", group_thousands(done), group_thousands(total))
    } else {
        format!(
            "{:.1} of {:.1} {}",
            done as f64 / scale,
            total as f64 / scale,
            UNITS[unit]
        )
    }
}

/// `"Copying 1,204 of 5,830 files · 1.2 of 4.8 GiB"`.
fn copying_label(progress: &MigrationProgress) -> String {
    format!(
        "Copying {} of {} files · {}",
        group_thousands(progress.files_done),
        group_thousands(progress.files_total),
        size_pair(progress.bytes_done, progress.bytes_total)
    )
}

/// First [`NAMES_LIMIT`] names, then "and N more".
fn names_summary(names: &[String]) -> String {
    let shown = names
        .iter()
        .take(NAMES_LIMIT)
        .map(String::as_str)
        .collect::<Vec<_>>()
        .join(", ");
    if names.len() > NAMES_LIMIT {
        format!("{shown} and {} more", names.len() - NAMES_LIMIT)
    } else {
        shown
    }
}

/// Index of the largest value if exactly one element has it.
fn unique_max<T: Ord>(values: &[T]) -> Option<usize> {
    let max = values.iter().max()?;
    let mut matches = values.iter().enumerate().filter(|(_, value)| *value == max);
    let (index, _) = matches.next()?;
    matches.next().is_none().then_some(index)
}

/// "just now", "5 minutes ago", "yesterday", "3 days ago", "2 months ago", ...
fn relative_time(then: SystemTime, now: SystemTime) -> String {
    let secs = now
        .duration_since(then)
        .map_or(0, |elapsed| elapsed.as_secs());
    let days = secs / 86_400;
    match secs {
        0..60 => "just now".to_owned(),
        60..3_600 => format!("{} ago", plural(secs / 60, "minute", "minutes")),
        3_600..86_400 => format!("{} ago", plural(secs / 3_600, "hour", "hours")),
        86_400..172_800 => "yesterday".to_owned(),
        _ if days < 30 => format!("{} ago", plural(days, "day", "days")),
        _ if days < 365 => format!("{} ago", plural(days / 30, "month", "months")),
        _ => format!("{} ago", plural(days / 365, "year", "years")),
    }
}

/// Absolute local time for tooltips (`2026-09-21 14:13`), or `None` if the local
/// offset is unknown. Never shows UTC.
fn local_time_label(time: SystemTime) -> Option<String> {
    let secs = i64::try_from(time.duration_since(UNIX_EPOCH).ok()?.as_secs()).ok()?;
    Some(format_civil(secs.checked_add(local_offset_seconds(secs)?)?))
}

/// Seconds east of UTC at `secs` (Unix time), from the C library's time zone data.
#[cfg(any(
    target_os = "linux",
    target_os = "android",
    target_os = "macos",
    target_os = "freebsd"
))]
// `time_t` and `c_long` are 32-bit on some targets, so the conversions are not
// useless everywhere.
#[allow(clippy::useless_conversion)]
fn local_offset_seconds(secs: i64) -> Option<i64> {
    let time: libc::time_t = secs.try_into().ok()?;
    // SAFETY: an all-zero `tm` is a valid value to be overwritten.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    // SAFETY: `localtime_r` is the reentrant variant; both pointers are valid for the call.
    let result = unsafe { libc::localtime_r(&time, &mut tm) };
    (!result.is_null()).then(|| i64::from(tm.tm_gmtoff))
}

/// Seconds east of UTC, using the current Windows time zone rules.
#[cfg(windows)]
fn local_offset_seconds(secs: i64) -> Option<i64> {
    use windows_sys::Win32::Foundation::FILETIME;
    use windows_sys::Win32::Storage::FileSystem::FileTimeToLocalFileTime;
    /// Seconds from 1601-01-01 (FILETIME epoch) to 1970-01-01.
    const EPOCH_OFFSET: i64 = 11_644_473_600;
    let ticks = u64::try_from(secs.checked_add(EPOCH_OFFSET)?.checked_mul(10_000_000)?).ok()?;
    let utc = FILETIME {
        dwLowDateTime: ticks as u32,
        dwHighDateTime: (ticks >> 32) as u32,
    };
    let mut local = FILETIME::default();
    // SAFETY: both pointers reference live FILETIME values for the duration of the call.
    if unsafe { FileTimeToLocalFileTime(&utc, &mut local) } == 0 {
        return None;
    }
    let local_ticks = (u64::from(local.dwHighDateTime) << 32) | u64::from(local.dwLowDateTime);
    Some((i64::try_from(local_ticks).ok()? - i64::try_from(ticks).ok()?) / 10_000_000)
}

#[cfg(not(any(
    windows,
    target_os = "linux",
    target_os = "android",
    target_os = "macos",
    target_os = "freebsd"
)))]
fn local_offset_seconds(_secs: i64) -> Option<i64> {
    None
}

/// Formats seconds since 1970 (already shifted to local time) as `YYYY-MM-DD HH:MM`.
fn format_civil(local_secs: i64) -> String {
    let (days, rem) = (local_secs.div_euclid(86_400), local_secs.rem_euclid(86_400));
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02}",
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

/// Whether `path` is inside a OneDrive folder (`OneDrive`, `OneDrive - Contoso`, ...),
/// where files may be online-only placeholders.
fn is_onedrive_path(path: &Path) -> bool {
    path.components().any(|component| match component {
        Component::Normal(name) => name
            .to_string_lossy()
            .to_lowercase()
            .starts_with("onedrive"),
        _ => false,
    })
}

/// The drive (`C:\`) holding `path` on Windows; elsewhere the path itself.
fn drive_label(path: &Path) -> String {
    match path.components().next() {
        Some(Component::Prefix(prefix)) => {
            format!("{}\\", prefix.as_os_str().to_string_lossy())
        }
        _ => path.display().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferrite_launcher::core::migration::UncopyableFile;

    const GIB: u64 = 1024 * 1024 * 1024;

    #[test]
    fn sizes_use_binary_units() {
        assert_eq!(format_size(0), "0 B");
        assert_eq!(format_size(1023), "1023 B");
        assert_eq!(format_size(1536), "1.5 KiB");
        assert_eq!(format_size(5 * GIB), "5.0 GiB");
    }

    #[test]
    fn numbers_get_thousands_separators() {
        assert_eq!(group_thousands(0), "0");
        assert_eq!(group_thousands(999), "999");
        assert_eq!(group_thousands(1_204), "1,204");
        assert_eq!(group_thousands(5_830_000), "5,830,000");
        assert_eq!(plural(1, "file", "files"), "1 file");
        assert_eq!(plural(1_204, "file", "files"), "1,204 files");
    }

    #[test]
    fn copying_label_matches_the_spec() {
        let progress = MigrationProgress {
            step: MigrationStep::Copying,
            files_done: 1_204,
            files_total: 5_830,
            bytes_done: (1.2 * GIB as f64) as u64,
            bytes_total: (4.8 * GIB as f64) as u64,
        };
        assert_eq!(
            copying_label(&progress),
            "Copying 1,204 of 5,830 files · 1.2 of 4.8 GiB"
        );
        assert_eq!(size_pair(10, 1_000), "10 of 1,000 B");
        assert_eq!(size_pair(512 * 1024, 2 * 1024 * 1024), "0.5 of 2.0 MiB");
    }

    #[test]
    fn times_are_relative_with_a_local_tooltip() {
        let now = UNIX_EPOCH + Duration::from_secs(1_790_000_000);
        let ago = |secs: u64| relative_time(now - Duration::from_secs(secs), now);
        assert_eq!(ago(5), "just now");
        assert_eq!(
            relative_time(now + Duration::from_secs(60), now),
            "just now"
        );
        assert_eq!(ago(60), "1 minute ago");
        assert_eq!(ago(45 * 60), "45 minutes ago");
        assert_eq!(ago(3 * 3_600), "3 hours ago");
        assert_eq!(ago(30 * 3_600), "yesterday");
        assert_eq!(ago(3 * 86_400), "3 days ago");
        assert_eq!(ago(65 * 86_400), "2 months ago");
        assert_eq!(ago(800 * 86_400), "2 years ago");

        assert_eq!(format_civil(0), "1970-01-01 00:00");
        assert_eq!(format_civil(1_790_000_000), "2026-09-21 14:13");
        assert_eq!(format_civil(951_782_400), "2000-02-29 00:00");
        assert_eq!(format_civil(-60), "1969-12-31 23:59");
        #[cfg(any(windows, target_os = "linux", target_os = "macos"))]
        assert!(local_time_label(now).is_some());
    }

    #[test]
    fn onedrive_folders_are_detected() {
        let path = |parts: &[&str]| parts.iter().collect::<PathBuf>();
        assert!(is_onedrive_path(&path(&[
            "home",
            "OneDrive",
            "minecraft",
            "a.jar"
        ])));
        assert!(is_onedrive_path(&path(&[
            "Users",
            "OneDrive - Contoso",
            "x"
        ])));
        assert!(is_onedrive_path(&path(&["Users", "onedrive", "x"])));
        assert!(!is_onedrive_path(&path(&[
            "Users",
            "MyOneDriveBackup",
            "x"
        ])));
        assert!(!is_onedrive_path(&path(&[
            "Users",
            "Documents",
            "minecraft"
        ])));
    }

    #[test]
    fn names_summary_and_unique_max() {
        let names: Vec<String> = (1..=7).map(|n| format!("Pack {n}")).collect();
        assert_eq!(names_summary(&names[..2]), "Pack 1, Pack 2");
        assert_eq!(
            names_summary(&names),
            "Pack 1, Pack 2, Pack 3, Pack 4, Pack 5 and 2 more"
        );
        assert_eq!(unique_max(&[3, 9, 1]), Some(1));
        assert_eq!(unique_max(&[9, 9, 1]), None);
        assert_eq!(unique_max::<u8>(&[]), None);
    }

    #[test]
    fn link_label_shows_the_resolved_path_for_links() {
        let found = Path::new("old").join("minecraft");
        let resolved = Path::new("other").join("minecraft");
        assert_eq!(link_label(&found, &found), found.display().to_string());
        assert_eq!(
            link_label(&found, &resolved),
            format!("{} → {}", found.display(), resolved.display())
        );
    }

    #[test]
    fn failures_are_described_in_plain_words() {
        let state = Path::new("data").join("migration-state.json");
        let describe =
            |error: &MigrationError, source: Option<&Path>| describe_failure(error, source, &state);

        let space = describe(
            &MigrationError::NotEnoughSpace {
                needed: 5 * GIB,
                available: GIB + GIB / 5,
                volume: PathBuf::from("data"),
            },
            None,
        );
        assert_eq!(
            space.message,
            "There isn't enough free space on data. Ferrite needs about 5.0 GiB and \
             there's 1.2 GiB free. Free up some space and try again."
        );
        assert!(space.can_retry && !space.open_data_folder);

        let source: PathBuf = ["Users", "a", "OneDrive", "minecraft"].iter().collect();
        let files: Vec<UncopyableFile> = (0..25)
            .map(|n| UncopyableFile {
                path: PathBuf::from(format!("mods/{n}.jar")),
                reason: "cannot read".to_owned(),
            })
            .collect();
        let uncopyable = describe(
            &MigrationError::UncopyableFiles(files.clone()),
            Some(&source),
        );
        assert_eq!(
            uncopyable.message,
            "Ferrite couldn't copy 25 files, so it stopped before finishing. Nothing was moved."
        );
        assert!(
            uncopyable
                .hint
                .is_some_and(|hint| hint.contains("OneDrive"))
        );
        assert_eq!(uncopyable.details.len(), 25, "Copy gets every path");
        assert!(uncopyable.details[0].starts_with(&source.display().to_string()));
        let elsewhere: PathBuf = ["Users", "a", "minecraft"].iter().collect();
        let plain = describe(&MigrationError::UncopyableFiles(files), Some(&elsewhere));
        assert!(plain.hint.is_none());

        let changed = describe(
            &MigrationError::SourceChanged("1 file changed".into()),
            None,
        );
        assert!(
            changed
                .message
                .contains("Close Minecraft and any other launcher")
        );
        assert!(changed.can_retry);

        let io = describe(
            &MigrationError::Io {
                context: "read config".into(),
                error: std::io::Error::other("disk on fire"),
            },
            None,
        );
        assert_eq!(io.message, "Ferrite couldn't read config.");
        assert_eq!(io.details, vec!["disk on fire".to_owned()]);

        let newer = describe(&MigrationError::UnsupportedState { found: 9 }, None);
        assert!(newer.message.contains("newer version of Ferrite"));
        assert!(!newer.can_retry && newer.open_data_folder);

        let unreadable = describe(&MigrationError::StateUnreadable("bad json".into()), None);
        assert_eq!(unreadable.path.as_deref(), Some(state.as_path()));
        assert!(unreadable.can_retry && unreadable.open_data_folder);
    }

    #[test]
    fn drive_label_falls_back_to_the_path() {
        let path = Path::new("some").join("data");
        assert_eq!(drive_label(&path), path.display().to_string());
        #[cfg(windows)]
        assert_eq!(drive_label(Path::new(r"C:\Users\a\AppData")), r"C:\");
    }
}
