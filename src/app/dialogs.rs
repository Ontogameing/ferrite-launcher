//! Small pieces shared by the Stage 2 instance dialogs (Delete, Edit, Duplicate,
//! Import, Export): the danger button, muted text, the background instance scan, and
//! the size/worlds wording.

use super::startup::{group_thousands, plural};
use crate::instances::{InstanceProfile, NameError};
use eframe::egui::{self, Color32, RichText};
use ferrite_launcher::core::migration::format_size;
use ferrite_launcher::core::paths::AppPaths;
use ferrite_launcher::core::scan::{self, InstanceScan};
use std::path::Path;
use std::sync::mpsc::{self, Receiver, TryRecvError};

/// Fixed fill of destructive buttons (spec §1).
pub(super) const DANGER: Color32 = Color32::from_rgb(200, 50, 50);
/// The "Running" chip dot (spec §2.3).
pub(super) const RUNNING_GREEN: Color32 = Color32::from_rgb(80, 180, 100);

/// The OS word for the trash: "Recycle Bin" on Windows, "Trash" elsewhere.
pub(super) fn trash_word() -> &'static str {
    if cfg!(windows) {
        "Recycle Bin"
    } else {
        "Trash"
    }
}

/// A destructive button: red fill, white text. A click made with Enter is ignored, so
/// Enter can never trigger a deletion (spec §1).
pub(super) fn danger_button(ui: &mut egui::Ui, label: &str, enabled: bool) -> bool {
    let response = ui.add_enabled(
        enabled,
        egui::Button::new(RichText::new(label).color(Color32::WHITE)).fill(DANGER),
    );
    response.clicked() && !ui.input(|input| input.key_pressed(egui::Key::Enter))
}

/// A primary (accent-filled) button.
pub(super) fn primary_button(
    ui: &mut egui::Ui,
    label: &str,
    enabled: bool,
    accent: Color32,
) -> egui::Response {
    ui.add_enabled(
        enabled,
        egui::Button::new(RichText::new(label).color(Color32::WHITE)).fill(accent),
    )
}

/// Muted, wrapped text.
pub(super) fn muted(ui: &mut egui::Ui, text: impl Into<String>, color: Color32) {
    ui.add(egui::Label::new(RichText::new(text.into()).color(color)).wrap());
}

/// "Label  value" fact row with a muted label.
pub(super) fn fact(ui: &mut egui::Ui, label: &str, value: impl Into<String>, color: Color32) {
    ui.horizontal_wrapped(|ui| {
        ui.label(RichText::new(label).color(color));
        ui.add(egui::Label::new(value.into()).wrap());
    });
}

/// A monospace, selectable, wrapped path with an optional "Open folder" link.
/// Returns whether the link was clicked.
pub(super) fn path_fact(
    ui: &mut egui::Ui,
    label: &str,
    path: &Path,
    open_link: bool,
    color: Color32,
) -> bool {
    let mut clicked = false;
    ui.horizontal_wrapped(|ui| {
        ui.label(RichText::new(label).color(color));
        ui.add(
            egui::Label::new(RichText::new(path.display().to_string()).monospace())
                .selectable(true)
                .wrap(),
        );
        if open_link {
            clicked = ui.link("Open folder").clicked();
        }
    });
    clicked
}

/// Hover text of the Quilt badge (spec §8).
const QUILT_HOVER: &str =
    "Quilt support in Ferrite is experimental. Some versions or mods may not install or launch.";

/// Writes `loader` (a stored label like "Quilt") followed by the "Experimental" badge
/// when it is Quilt (spec §8). The one helper every loader display uses.
pub(super) fn loader_text(ui: &mut egui::Ui, loader: &str, text: RichText, color: Color32) {
    ui.label(text);
    quilt_badge(ui, loader, color);
}

/// Just the badge (after a "Quilt" already written), or nothing for other loaders.
pub(super) fn quilt_badge(ui: &mut egui::Ui, loader: &str, color: Color32) {
    if loader != "Quilt" {
        return;
    }
    egui::Frame::new()
        .stroke(egui::Stroke::new(1.0, color))
        .corner_radius(4.0)
        .inner_margin(egui::Margin::symmetric(4, 1))
        .show(ui, |ui| {
            ui.label(RichText::new("Experimental").small().color(color));
        })
        .response
        .on_hover_text(QUILT_HOVER);
}

/// Opens `folder` in the file manager; returns a status message on failure.
pub(super) fn open_folder(folder: &Path) -> Option<String> {
    crate::config::open_folder(folder)
        .err()
        .map(|error| format!("Couldn't open {}: {error}", folder.display()))
}

/// A dialog's button row: `left` (Cancel/Close) on the left, `right` laid out from the
/// right edge (primary action first). Returns whichever side produced an action.
pub(super) fn button_row<T>(
    ui: &mut egui::Ui,
    left: impl FnOnce(&mut egui::Ui) -> Option<T>,
    right: impl FnOnce(&mut egui::Ui) -> Option<T>,
) -> Option<T> {
    ui.horizontal(|ui| {
        let left = left(ui);
        let right = ui
            .with_layout(egui::Layout::right_to_left(egui::Align::Center), right)
            .inner;
        left.or(right)
    })
    .inner
}

/// Esc was pressed this frame (and consumed).
pub(super) fn escape_pressed(ui: &egui::Ui) -> bool {
    ui.input_mut(|input| input.consume_key(egui::Modifiers::NONE, egui::Key::Escape))
}

/// Enter was pressed this frame (used for non-destructive primary actions only).
pub(super) fn enter_pressed(ui: &egui::Ui) -> bool {
    ui.input(|input| input.key_pressed(egui::Key::Enter))
}

/// The live message under a name field (spec §4.1).
pub(super) fn name_error_text(error: &NameError) -> String {
    match error {
        NameError::Empty => "Enter a name.".into(),
        NameError::Taken(existing) => format!("Another instance is already called “{existing}”."),
        NameError::ControlCharacter => "Names can't contain tabs or line breaks.".into(),
    }
}

/// A single-line name field. With `select_all`, it takes focus and selects its text
/// (first frame of a dialog). Shows `error` underneath in the warning color.
pub(super) fn name_field(
    ui: &mut egui::Ui,
    id: &str,
    name: &mut String,
    error: Option<&str>,
    select_all: bool,
) -> egui::Response {
    let mut output = egui::TextEdit::singleline(name)
        .id_salt(id)
        .desired_width(f32::INFINITY)
        .show(ui);
    if select_all {
        output.response.response.request_focus();
        let end = egui::text::CCursor::new(name.chars().count());
        output
            .state
            .cursor
            .set_char_range(Some(egui::text::CCursorRange::two(
                egui::text::CCursor::new(0),
                end,
            )));
        output.state.store(ui.ctx(), output.response.response.id);
    }
    if let Some(error) = error {
        ui.label(RichText::new(error).color(ui.visuals().warn_fg_color));
    }
    output.response.response
}

/// A background [`scan::scan_instance`] for one dialog.
pub(super) struct ScanJob {
    receiver: Option<Receiver<Result<InstanceScan, String>>>,
    result: Option<Result<InstanceScan, String>>,
}

impl ScanJob {
    /// Starts scanning `profile`'s folder on a worker thread.
    pub(super) fn start(paths: &AppPaths, profile: &InstanceProfile) -> Self {
        let paths = paths.clone();
        let profile = profile.clone();
        let (sender, receiver) = mpsc::channel();
        let spawned = std::thread::Builder::new()
            .name("instance-scan".into())
            .spawn(move || {
                let result = scan::scan_instance(&paths, &profile).map_err(|e| e.to_string());
                let _ = sender.send(result);
            });
        match spawned {
            Ok(_) => Self {
                receiver: Some(receiver),
                result: None,
            },
            Err(error) => Self::finished(Err(format!("couldn't start the scan: {error}"))),
        }
    }

    /// A job that already has its result (folder missing, tests).
    pub(super) fn finished(result: Result<InstanceScan, String>) -> Self {
        Self {
            receiver: None,
            result: Some(result),
        }
    }

    /// A job still waiting on `receiver` (tests).
    #[cfg(test)]
    pub(super) fn pending(receiver: Receiver<Result<InstanceScan, String>>) -> Self {
        Self {
            receiver: Some(receiver),
            result: None,
        }
    }

    /// Collects the result if it arrived. Returns `true` while still running.
    pub(super) fn poll(&mut self) -> bool {
        let Some(receiver) = &self.receiver else {
            return false;
        };
        match receiver.try_recv() {
            Ok(result) => self.result = Some(result),
            Err(TryRecvError::Empty) => return true,
            Err(TryRecvError::Disconnected) => {
                self.result = Some(Err("the scan stopped unexpectedly".into()));
            }
        }
        self.receiver = None;
        false
    }

    pub(super) fn is_running(&self) -> bool {
        self.receiver.is_some()
    }

    pub(super) fn result(&self) -> Option<&Result<InstanceScan, String>> {
        self.result.as_ref()
    }

    pub(super) fn scan(&self) -> Option<&InstanceScan> {
        self.result.as_ref().and_then(|result| result.as_ref().ok())
    }
}

/// `"4.2 GiB · 12,345 files"`.
pub(super) fn size_line(scan: &InstanceScan) -> String {
    format!(
        "{} · {}",
        format_size(scan.bytes),
        plural(scan.files, "file", "files")
    )
}

/// `"3 worlds: New World, Skyblock, Hardcore 2"`; at most three names, then
/// `" and 5 more"`. `None` when there are no worlds.
pub(super) fn worlds_line(worlds: &[String]) -> Option<String> {
    if worlds.is_empty() {
        return None;
    }
    let shown = worlds
        .iter()
        .take(3)
        .cloned()
        .collect::<Vec<_>>()
        .join(", ");
    let more = worlds.len().saturating_sub(3);
    let count = plural(worlds.len() as u64, "world", "worlds");
    Some(if more > 0 {
        format!("{count}: {shown} and {} more", group_thousands(more as u64))
    } else {
        format!("{count}: {shown}")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(count: usize) -> Vec<String> {
        (1..=count).map(|n| format!("W{n}")).collect()
    }

    #[test]
    fn worlds_line_lists_three_names_then_a_count() {
        assert_eq!(worlds_line(&[]), None);
        assert_eq!(worlds_line(&names(1)).unwrap(), "1 world: W1");
        assert_eq!(worlds_line(&names(3)).unwrap(), "3 worlds: W1, W2, W3");
        assert_eq!(
            worlds_line(&names(8)).unwrap(),
            "8 worlds: W1, W2, W3 and 5 more"
        );
    }

    #[test]
    fn size_line_uses_binary_units_and_grouped_counts() {
        let scan = InstanceScan {
            bytes: 4_509_715_661,
            files: 12_345,
            ..Default::default()
        };
        assert_eq!(size_line(&scan), "4.2 GiB · 12,345 files");
    }

    #[test]
    fn name_errors_read_as_the_spec_says() {
        assert_eq!(name_error_text(&NameError::Empty), "Enter a name.");
        assert_eq!(
            name_error_text(&NameError::Taken("Foo".into())),
            "Another instance is already called “Foo”."
        );
    }

    #[test]
    fn finished_scan_jobs_are_not_running() {
        let mut job = ScanJob::finished(Ok(InstanceScan::default()));
        assert!(!job.poll());
        assert!(!job.is_running());
        assert!(job.scan().is_some());
    }
}
