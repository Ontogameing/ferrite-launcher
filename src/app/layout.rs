//! Configurable widget-grid rendering and its settings editor.
//!
//! Widget names from configuration are represented by the closed [`Widget`] enum and are
//! dispatched through the registry below. No configured text is interpreted as code. Layout
//! snapshots are cloned before rendering because page widgets are free to mutate all of
//! [`Ferrite`] during an immediate-mode frame.

use super::{Ferrite, MUTED, Page};
use crate::ui_settings::{
    GridConfig, LayoutConfig, LayoutPage, MAX_LAYOUT_ROWS, MAX_WIDGET_LABEL_LENGTH,
    MAX_WIDGET_PLACEMENTS, MAX_WIDGET_TEXT_LENGTH, PageLayout, Widget, WidgetAction,
    WidgetPlacement,
};
use eframe::egui::{self, RichText};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WidgetKind {
    TopBar,
    PageBody,
    StatusBar,
    SelectedInstance,
    AccountSummary,
    LauncherStatus,
    Text,
    ActionButton,
}

#[derive(Clone, Copy)]
struct WidgetRegistration {
    kind: WidgetKind,
    label: &'static str,
    default_height: u16,
}

const WIDGET_REGISTRY: &[WidgetRegistration] = &[
    WidgetRegistration {
        kind: WidgetKind::TopBar,
        label: "Top bar",
        default_height: 1,
    },
    WidgetRegistration {
        kind: WidgetKind::PageBody,
        label: "Page body",
        default_height: 10,
    },
    WidgetRegistration {
        kind: WidgetKind::StatusBar,
        label: "Status bar",
        default_height: 1,
    },
    WidgetRegistration {
        kind: WidgetKind::SelectedInstance,
        label: "Selected instance",
        default_height: 3,
    },
    WidgetRegistration {
        kind: WidgetKind::AccountSummary,
        label: "Account summary",
        default_height: 2,
    },
    WidgetRegistration {
        kind: WidgetKind::LauncherStatus,
        label: "Launcher status",
        default_height: 3,
    },
    WidgetRegistration {
        kind: WidgetKind::Text,
        label: "Text",
        default_height: 2,
    },
    WidgetRegistration {
        kind: WidgetKind::ActionButton,
        label: "Action button",
        default_height: 1,
    },
];

const ACTIONS: &[(WidgetAction, &str)] = &[
    (WidgetAction::Launch, "Launch selected instance"),
    (
        WidgetAction::Navigate {
            page: LayoutPage::Play,
        },
        "Go to Play",
    ),
    (
        WidgetAction::Navigate {
            page: LayoutPage::Instances,
        },
        "Go to Instances",
    ),
    (
        WidgetAction::Navigate {
            page: LayoutPage::Mods,
        },
        "Go to Mods",
    ),
    (
        WidgetAction::Navigate {
            page: LayoutPage::Settings,
        },
        "Go to Settings",
    ),
    (WidgetAction::StopGame, "Stop game"),
    (WidgetAction::CreateInstance, "Create instance"),
    (WidgetAction::ImportPack, "Import pack"),
    (WidgetAction::OpenAccount, "Open account"),
    (WidgetAction::Settings, "Open settings"),
];

impl Ferrite {
    /// Whether the opt-in custom shell should replace the fixed shell for this frame.
    pub(super) fn custom_layout_enabled(&self) -> bool {
        self.ui_settings.layout.enabled
    }

    /// Renders the current page's configured widgets at explicit grid coordinates.
    pub(super) fn render_layout(&mut self, ui: &mut egui::Ui) {
        let grid = self.ui_settings.layout.grid.clone();
        // Widgets such as PageBody and TopBar mutate `self`; never retain a config borrow here.
        let placements = self.current_page_layout().placements.clone();
        let rows = placements
            .iter()
            .filter(|placement| placement.enabled)
            .map(placement_last_row)
            .max()
            .unwrap_or(1)
            .min(MAX_LAYOUT_ROWS);
        let gap = f32::from(grid.gap);
        let total_height =
            f32::from(rows) * f32::from(grid.row_height) + f32::from(rows.saturating_sub(1)) * gap;
        let (canvas, _) = ui.allocate_exact_size(
            egui::vec2(ui.available_width(), total_height),
            egui::Sense::hover(),
        );
        let mut pending_action = None;

        for (index, placement) in placements.iter().enumerate() {
            if !placement.enabled {
                continue;
            }
            let Some(rect) = placement_rect(canvas, &grid, placement) else {
                continue;
            };
            ui.scope_builder(
                egui::UiBuilder::new()
                    .id_salt(("layout_widget", index))
                    .max_rect(rect),
                |ui| {
                    ui.set_clip_rect(rect.intersect(ui.clip_rect()));
                    ui.set_min_size(rect.size());
                    if let Some(action) = self.render_widget(ui, &placement.widget) {
                        pending_action.get_or_insert(action);
                    }
                },
            );
        }

        if let Some(action) = pending_action {
            self.perform_widget_action(action);
        }
    }

    fn current_page_layout(&self) -> &PageLayout {
        match self.current_page {
            Page::Play => &self.ui_settings.layout.play,
            Page::Instances => &self.ui_settings.layout.instances,
            Page::Mods => &self.ui_settings.layout.mods,
            Page::Settings => &self.ui_settings.layout.settings,
        }
    }

    fn render_widget(&mut self, ui: &mut egui::Ui, widget: &Widget) -> Option<WidgetAction> {
        match widget {
            Widget::TopBar => self.top_bar(ui),
            Widget::PageBody => {
                self.update_banner(ui);
                match self.current_page {
                    Page::Play => self.play_page(ui),
                    Page::Instances => self.instances_page(ui),
                    Page::Mods => self.mods_page(ui),
                    Page::Settings => self.settings_page(ui),
                }
            }
            Widget::StatusBar => self.layout_status_bar(ui),
            Widget::SelectedInstance => self.selected_instance_summary(ui),
            Widget::AccountSummary => self.account_summary(ui),
            Widget::LauncherStatus => self.launcher_status_summary(ui),
            Widget::Text { text } => {
                ui.add(egui::Label::new(text).wrap());
            }
            Widget::ActionButton { label, action } => {
                let enabled =
                    !matches!(action, WidgetAction::Launch) || self.selected_instance().is_some();
                if ui
                    .add_enabled(
                        enabled,
                        egui::Button::new(RichText::new(label).strong())
                            .min_size(ui.available_size()),
                    )
                    .clicked()
                {
                    return Some(action.clone());
                }
            }
        }
        None
    }

    /// Draws the same task-priority status and version footer as the fixed shell.
    pub(super) fn layout_status_bar(&self, ui: &mut egui::Ui) {
        let status = if self.instance_creation_task.is_some() {
            self.instance_creation_status
                .as_deref()
                .unwrap_or(&self.running_text)
        } else if self.pack_task.is_some() {
            self.pack_status.as_deref().unwrap_or(&self.running_text)
        } else {
            &self.running_text
        };
        ui.separator();
        ui.horizontal(|ui| {
            ui.label(
                RichText::new("STATUS")
                    .small()
                    .strong()
                    .color(self.accent_color()),
            );
            ui.add(egui::Label::new(RichText::new(status).small().color(MUTED)).truncate());
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label(
                    RichText::new(format!("Ferrite {}", env!("CARGO_PKG_VERSION")))
                        .small()
                        .color(MUTED),
                );
            });
        });
    }

    fn selected_instance_summary(&self, ui: &mut egui::Ui) {
        summary_frame(self, ui, "SELECTED INSTANCE", |ui| {
            if let Some(instance) = self.selected_instance() {
                summary_row(ui, "Profile", &instance.name);
                summary_row(ui, "Minecraft", &instance.version);
                summary_row(ui, "Loader", &instance.loader);
                summary_row(
                    ui,
                    "Memory",
                    &format!("{} MB", self.config.minecraft.default_memory_mb),
                );
            } else {
                ui.label(RichText::new("No instance selected.").color(MUTED));
            }
        });
    }

    fn account_summary(&self, ui: &mut egui::Ui) {
        summary_frame(self, ui, "ACCOUNT", |ui| {
            let status = if self.auth.offline_mode {
                "Offline mode".to_owned()
            } else if let Some(account) = &self.auth.account {
                if account.is_expired() {
                    format!("{} · session expired", account.name)
                } else {
                    format!("Signed in as {}", account.name)
                }
            } else {
                "Microsoft account required".to_owned()
            };
            ui.label(status);
        });
    }

    fn launcher_status_summary(&self, ui: &mut egui::Ui) {
        summary_frame(self, ui, "LAUNCHER STATUS", |ui| {
            summary_row(
                ui,
                "Discord",
                if self.discord.is_some() {
                    "Rich Presence active"
                } else {
                    "Not connected"
                },
            );
            summary_row(
                ui,
                "Game",
                if crate::minecraft::is_running() {
                    "Running"
                } else {
                    "Ready"
                },
            );
            if let Some(status) = &self.update_status {
                summary_row(ui, "Updates", status);
            }
        });
    }

    fn perform_widget_action(&mut self, action: WidgetAction) {
        match action {
            WidgetAction::Launch => self.launch_selected(),
            WidgetAction::Navigate { page } => self.current_page = page_from_layout(page),
            WidgetAction::StopGame => self.stop_game(),
            WidgetAction::CreateInstance => self.create_instance_open = true,
            WidgetAction::ImportPack => {
                if !self.pack_busy() {
                    self.pack_path.clear();
                    self.pack_name.clear();
                    self.pack_status = None;
                    self.import_pack_open = true;
                }
            }
            WidgetAction::OpenAccount => self.auth.open = true,
            WidgetAction::Settings => self.current_page = Page::Settings,
        }
    }

    /// Central action used by the configurable Stop Game button.
    ///
    /// The fixed Play view should call this method when layout integration is performed so the
    /// backend operation and user-facing result remain defined in one place.
    pub(super) fn stop_game(&mut self) {
        self.running_text = match crate::minecraft::kill() {
            Ok(()) => String::from("Game not running."),
            Err(error) => format!("Failed to stop game: {error}"),
        };
    }

    /// Edits a detached layout snapshot and commits/persists it after egui releases borrows.
    pub(super) fn layout_settings_ui(&mut self, ui: &mut egui::Ui) {
        let mut layout = self.ui_settings.layout.clone();
        let mut changed = false;
        changed |= ui
            .checkbox(&mut layout.enabled, "Enable custom widget layout")
            .changed();
        ui.label(
            RichText::new(
                "Warning: keep a Page body widget enabled on every page that needs full page management controls.",
            )
            .color(self.accent_color()),
        );
        ui.label(
            RichText::new("Without Page body, instance, mod, account, and settings management may only be reachable through action buttons.")
                .small()
                .color(MUTED),
        );
        ui.add_space(8.0);

        let old_columns = layout.grid.columns;
        ui.horizontal_wrapped(|ui| {
            changed |= ui
                .add(egui::Slider::new(&mut layout.grid.columns, 1..=12).text("Columns"))
                .changed();
            changed |= ui
                .add(egui::Slider::new(&mut layout.grid.row_height, 24..=512).text("Row height"))
                .changed();
            changed |= ui
                .add(egui::Slider::new(&mut layout.grid.gap, 0..=128).text("Gap"))
                .changed();
        });
        if layout.grid.columns != old_columns {
            clamp_all_placements(&mut layout);
        }

        let page_id = ui.make_persistent_id("layout_editor_page");
        let mut page = ui
            .data_mut(|data| data.get_temp::<LayoutPage>(page_id))
            .unwrap_or_else(|| layout_page_from_page(self.current_page));
        egui::ComboBox::from_label("Page to edit")
            .selected_text(layout_page_label(page))
            .show_ui(ui, |ui| {
                for candidate in [
                    LayoutPage::Play,
                    LayoutPage::Instances,
                    LayoutPage::Mods,
                    LayoutPage::Settings,
                ] {
                    ui.selectable_value(&mut page, candidate, layout_page_label(candidate));
                }
            });
        ui.data_mut(|data| data.insert_temp(page_id, page));

        let add_id = ui.make_persistent_id("layout_editor_add_widget");
        let mut add_kind = ui
            .data_mut(|data| data.get_temp::<WidgetKind>(add_id))
            .unwrap_or(WidgetKind::PageBody);
        ui.horizontal(|ui| {
            egui::ComboBox::from_id_salt("layout_add_widget_kind")
                .selected_text(widget_kind_label(add_kind))
                .show_ui(ui, |ui| {
                    for registration in WIDGET_REGISTRY {
                        ui.selectable_value(&mut add_kind, registration.kind, registration.label);
                    }
                });
            let can_add = page_layout(&layout, page).placements.len() < MAX_WIDGET_PLACEMENTS;
            if ui
                .add_enabled(can_add, egui::Button::new("Add widget"))
                .clicked()
            {
                let placement = new_placement(add_kind, &layout.grid, page_layout(&layout, page));
                page_layout_mut(&mut layout, page)
                    .placements
                    .push(placement);
                changed = true;
            }
        });
        ui.data_mut(|data| data.insert_temp(add_id, add_kind));

        let mut remove = None;
        let mut move_up = None;
        let mut move_down = None;
        let columns = layout.grid.columns;
        let placements = &mut page_layout_mut(&mut layout, page).placements;
        for (index, placement) in placements.iter_mut().enumerate() {
            ui.push_id(index, |ui| {
                egui::Frame::group(ui.style()).show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.strong(format!(
                            "{} · {}",
                            index + 1,
                            widget_label(&placement.widget)
                        ));
                        changed |= ui.checkbox(&mut placement.enabled, "Enabled").changed();
                        if ui.small_button("↑").on_hover_text("Move earlier").clicked() {
                            move_up = Some(index);
                        }
                        if ui.small_button("↓").on_hover_text("Move later").clicked() {
                            move_down = Some(index);
                        }
                        if ui.small_button("Remove").clicked() {
                            remove = Some(index);
                        }
                    });
                    ui.horizontal_wrapped(|ui| {
                        let max_column = columns.max(1);
                        changed |= ui
                            .add(
                                egui::DragValue::new(&mut placement.column)
                                    .range(1..=max_column)
                                    .prefix("Column "),
                            )
                            .changed();
                        let max_width = max_column
                            .saturating_sub(placement.column)
                            .saturating_add(1)
                            .max(1);
                        placement.width = placement.width.min(max_width).max(1);
                        changed |= ui
                            .add(
                                egui::DragValue::new(&mut placement.width)
                                    .range(1..=max_width)
                                    .prefix("Width "),
                            )
                            .changed();
                        changed |= ui
                            .add(
                                egui::DragValue::new(&mut placement.row)
                                    .range(1..=MAX_LAYOUT_ROWS)
                                    .prefix("Row "),
                            )
                            .changed();
                        let max_height = MAX_LAYOUT_ROWS
                            .saturating_sub(placement.row)
                            .saturating_add(1);
                        placement.height = placement.height.min(max_height).max(1);
                        changed |= ui
                            .add(
                                egui::DragValue::new(&mut placement.height)
                                    .range(1..=max_height)
                                    .prefix("Height "),
                            )
                            .changed();
                    });
                    changed |= widget_fields_ui(ui, &mut placement.widget);
                });
            });
        }
        if let Some(index) = remove {
            placements.remove(index);
            changed = true;
        } else if let Some(index) = move_up.filter(|index| *index > 0) {
            placements.swap(index, index - 1);
            changed = true;
        } else if let Some(index) = move_down.filter(|index| *index + 1 < placements.len()) {
            placements.swap(index, index + 1);
            changed = true;
        }

        if !placements
            .iter()
            .any(|placement| placement.enabled && matches!(placement.widget, Widget::PageBody))
        {
            ui.label(
                RichText::new("This page has no enabled Page body widget; full page management is unavailable in its custom layout.")
                    .strong()
                    .color(self.accent_color()),
            );
        }
        let collisions = overlapping_pairs(placements);
        if !collisions.is_empty() {
            ui.label(
                RichText::new(format!(
                    "Overlapping widgets are layered in list order (later paints on top): {}",
                    collisions
                        .iter()
                        .map(|(left, right)| format!("{} ↔ {}", left + 1, right + 1))
                        .collect::<Vec<_>>()
                        .join(", ")
                ))
                .color(self.accent_color()),
            );
        }

        ui.horizontal(|ui| {
            if ui.button("Reset current page").clicked() {
                let columns = layout.grid.columns;
                *page_layout_mut(&mut layout, page) = default_page_layout(columns);
                changed = true;
            }
            if ui.button("Reset all layout defaults").clicked() {
                layout = LayoutConfig::default();
                changed = true;
            }
        });

        if changed {
            self.ui_settings.layout = layout;
            self.save_config_change("Saved custom layout settings.");
        }
    }
}

fn summary_frame(
    app: &Ferrite,
    ui: &mut egui::Ui,
    title: &str,
    add_contents: impl FnOnce(&mut egui::Ui),
) {
    egui::Frame::new()
        .fill(app.card_color())
        .corner_radius(app.ui_settings.appearance.corner_radius)
        .inner_margin(12.0)
        .show(ui, |ui| {
            ui.set_min_size(ui.available_size());
            ui.label(RichText::new(title).strong());
            ui.separator();
            add_contents(ui);
        });
}

fn summary_row(ui: &mut egui::Ui, label: &str, value: &str) {
    ui.horizontal(|ui| {
        ui.label(RichText::new(label).color(MUTED));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.add(egui::Label::new(RichText::new(value).strong()).truncate());
        });
    });
}

fn placement_rect(
    canvas: egui::Rect,
    grid: &GridConfig,
    placement: &WidgetPlacement,
) -> Option<egui::Rect> {
    if placement.column == 0
        || placement.row == 0
        || placement.width == 0
        || placement.height == 0
        || grid.columns == 0
    {
        return None;
    }
    let gap = f32::from(grid.gap);
    let usable_width = (canvas.width() - gap * f32::from(grid.columns.saturating_sub(1))).max(0.0);
    let column_width = usable_width / f32::from(grid.columns);
    let x = canvas.left() + f32::from(placement.column - 1) * (column_width + gap);
    let y = canvas.top() + f32::from(placement.row - 1) * (f32::from(grid.row_height) + gap);
    let width = f32::from(placement.width) * column_width
        + f32::from(placement.width.saturating_sub(1)) * gap;
    let height = f32::from(placement.height) * f32::from(grid.row_height)
        + f32::from(placement.height.saturating_sub(1)) * gap;
    Some(egui::Rect::from_min_size(
        egui::pos2(x, y),
        egui::vec2(width.max(0.0), height.max(0.0)),
    ))
}

fn placement_last_row(placement: &WidgetPlacement) -> u16 {
    placement
        .row
        .saturating_add(placement.height)
        .saturating_sub(1)
}

fn page_from_layout(page: LayoutPage) -> Page {
    match page {
        LayoutPage::Play => Page::Play,
        LayoutPage::Instances => Page::Instances,
        LayoutPage::Mods => Page::Mods,
        LayoutPage::Settings => Page::Settings,
    }
}

fn layout_page_from_page(page: Page) -> LayoutPage {
    match page {
        Page::Play => LayoutPage::Play,
        Page::Instances => LayoutPage::Instances,
        Page::Mods => LayoutPage::Mods,
        Page::Settings => LayoutPage::Settings,
    }
}

fn layout_page_label(page: LayoutPage) -> &'static str {
    match page {
        LayoutPage::Play => "Play",
        LayoutPage::Instances => "Instances",
        LayoutPage::Mods => "Mods",
        LayoutPage::Settings => "Settings",
    }
}

fn page_layout(layout: &LayoutConfig, page: LayoutPage) -> &PageLayout {
    match page {
        LayoutPage::Play => &layout.play,
        LayoutPage::Instances => &layout.instances,
        LayoutPage::Mods => &layout.mods,
        LayoutPage::Settings => &layout.settings,
    }
}

fn page_layout_mut(layout: &mut LayoutConfig, page: LayoutPage) -> &mut PageLayout {
    match page {
        LayoutPage::Play => &mut layout.play,
        LayoutPage::Instances => &mut layout.instances,
        LayoutPage::Mods => &mut layout.mods,
        LayoutPage::Settings => &mut layout.settings,
    }
}

fn widget_kind(widget: &Widget) -> WidgetKind {
    match widget {
        Widget::TopBar => WidgetKind::TopBar,
        Widget::PageBody => WidgetKind::PageBody,
        Widget::StatusBar => WidgetKind::StatusBar,
        Widget::SelectedInstance => WidgetKind::SelectedInstance,
        Widget::AccountSummary => WidgetKind::AccountSummary,
        Widget::LauncherStatus => WidgetKind::LauncherStatus,
        Widget::Text { .. } => WidgetKind::Text,
        Widget::ActionButton { .. } => WidgetKind::ActionButton,
    }
}

fn widget_kind_label(kind: WidgetKind) -> &'static str {
    WIDGET_REGISTRY
        .iter()
        .find(|registration| registration.kind == kind)
        .map(|registration| registration.label)
        .unwrap_or("Unknown widget")
}

fn widget_label(widget: &Widget) -> &'static str {
    widget_kind_label(widget_kind(widget))
}

fn widget_from_kind(kind: WidgetKind) -> Widget {
    match kind {
        WidgetKind::TopBar => Widget::TopBar,
        WidgetKind::PageBody => Widget::PageBody,
        WidgetKind::StatusBar => Widget::StatusBar,
        WidgetKind::SelectedInstance => Widget::SelectedInstance,
        WidgetKind::AccountSummary => Widget::AccountSummary,
        WidgetKind::LauncherStatus => Widget::LauncherStatus,
        WidgetKind::Text => Widget::Text {
            text: "Custom text".to_owned(),
        },
        WidgetKind::ActionButton => Widget::ActionButton {
            label: "Launch".to_owned(),
            action: WidgetAction::Launch,
        },
    }
}

fn new_placement(kind: WidgetKind, grid: &GridConfig, page: &PageLayout) -> WidgetPlacement {
    let default_height = WIDGET_REGISTRY
        .iter()
        .find(|registration| registration.kind == kind)
        .map(|registration| registration.default_height)
        .unwrap_or(1);
    let latest_start = MAX_LAYOUT_ROWS
        .saturating_sub(default_height)
        .saturating_add(1);
    WidgetPlacement {
        widget: widget_from_kind(kind),
        column: 1,
        row: page
            .placements
            .iter()
            .filter(|placement| placement.enabled)
            .map(placement_last_row)
            .max()
            .unwrap_or(0)
            .saturating_add(1)
            .clamp(1, latest_start),
        width: grid.columns.max(1),
        height: default_height,
        enabled: true,
    }
}

fn widget_fields_ui(ui: &mut egui::Ui, widget: &mut Widget) -> bool {
    let mut changed = false;
    match widget {
        Widget::Text { text } => {
            changed |= ui
                .add(
                    egui::TextEdit::multiline(text)
                        .hint_text("Text shown by this widget")
                        .desired_rows(2),
                )
                .changed();
            trim_chars(text, MAX_WIDGET_TEXT_LENGTH);
        }
        Widget::ActionButton { label, action } => {
            ui.horizontal_wrapped(|ui| {
                ui.label("Label");
                changed |= ui.text_edit_singleline(label).changed();
                trim_chars(label, MAX_WIDGET_LABEL_LENGTH);
                egui::ComboBox::from_id_salt("action")
                    .selected_text(action_label(action))
                    .show_ui(ui, |ui| {
                        for (candidate, candidate_label) in ACTIONS {
                            changed |= ui
                                .selectable_value(action, candidate.clone(), *candidate_label)
                                .changed();
                        }
                    });
            });
        }
        _ => {}
    }
    changed
}

fn action_label(action: &WidgetAction) -> &'static str {
    ACTIONS
        .iter()
        .find(|(candidate, _)| candidate == action)
        .map(|(_, label)| *label)
        .unwrap_or("Action")
}

fn trim_chars(value: &mut String, maximum: usize) {
    if value.chars().count() > maximum {
        *value = value.chars().take(maximum).collect();
    }
}

fn clamp_all_placements(layout: &mut LayoutConfig) {
    let columns = layout.grid.columns.max(1);
    for page in [
        &mut layout.play,
        &mut layout.instances,
        &mut layout.mods,
        &mut layout.settings,
    ] {
        for placement in &mut page.placements {
            placement.column = placement.column.clamp(1, columns);
            let available = columns - placement.column + 1;
            placement.width = placement.width.clamp(1, available);
        }
    }
}

fn default_page_layout(columns: u8) -> PageLayout {
    let mut page = PageLayout::default();
    for placement in &mut page.placements {
        placement.column = 1;
        placement.width = columns.max(1);
    }
    page
}

fn overlapping_pairs(placements: &[WidgetPlacement]) -> Vec<(usize, usize)> {
    let mut pairs = Vec::new();
    for (right_index, right) in placements.iter().enumerate() {
        if !right.enabled {
            continue;
        }
        for (left_index, left) in placements[..right_index].iter().enumerate() {
            if left.enabled && placements_overlap(left, right) {
                pairs.push((left_index, right_index));
            }
        }
    }
    pairs
}

fn placements_overlap(left: &WidgetPlacement, right: &WidgetPlacement) -> bool {
    let left_right = u16::from(left.column) + u16::from(left.width);
    let right_right = u16::from(right.column) + u16::from(right.width);
    let left_bottom = u32::from(left.row) + u32::from(left.height);
    let right_bottom = u32::from(right.row) + u32::from(right.height);
    u16::from(left.column) < right_right
        && u16::from(right.column) < left_right
        && u32::from(left.row) < right_bottom
        && u32::from(right.row) < left_bottom
}

#[cfg(test)]
mod tests {
    use super::*;

    fn placement(column: u8, row: u16, width: u8, height: u16) -> WidgetPlacement {
        WidgetPlacement {
            column,
            row,
            width,
            height,
            ..WidgetPlacement::default()
        }
    }

    #[test]
    fn placement_rect_accounts_for_spans_and_gaps() {
        let grid = GridConfig {
            columns: 4,
            row_height: 20,
            gap: 10,
        };
        let rect = placement_rect(
            egui::Rect::from_min_size(egui::pos2(5.0, 7.0), egui::vec2(430.0, 200.0)),
            &grid,
            &placement(2, 2, 2, 3),
        )
        .unwrap();
        assert_eq!(rect.min, egui::pos2(115.0, 37.0));
        assert_eq!(rect.size(), egui::vec2(210.0, 80.0));
    }

    #[test]
    fn overlap_uses_half_open_grid_coordinates() {
        assert!(placements_overlap(
            &placement(1, 1, 2, 2),
            &placement(2, 2, 2, 2)
        ));
        assert!(!placements_overlap(
            &placement(1, 1, 2, 2),
            &placement(3, 1, 2, 2)
        ));
        assert!(!placements_overlap(
            &placement(1, 1, 2, 2),
            &placement(1, 3, 2, 2)
        ));
    }

    #[test]
    fn disabled_widgets_are_excluded_from_collision_report() {
        let first = placement(1, 1, 2, 2);
        let mut second = placement(2, 2, 2, 2);
        second.enabled = false;
        assert!(overlapping_pairs(&[first, second]).is_empty());
    }

    #[test]
    fn new_widgets_start_below_existing_enabled_content() {
        let page = PageLayout {
            placements: vec![placement(1, 2, 2, 3), placement(3, 9, 1, 1)],
        };
        let added = new_placement(WidgetKind::Text, &GridConfig::default(), &page);
        assert_eq!(added.row, 10);
        assert_eq!(added.height, 2);
        assert_eq!(added.width, GridConfig::default().columns);
    }

    #[test]
    fn reset_page_adapts_defaults_to_current_column_count() {
        let page = default_page_layout(6);
        assert!(
            page.placements
                .iter()
                .all(|placement| placement.column == 1)
        );
        assert!(page.placements.iter().all(|placement| placement.width == 6));
    }
}
