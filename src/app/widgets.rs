//! The widget registry and the render dispatch for every placeable widget.
//!
//! [`WIDGET_REGISTRY`] is the single list the editor's palette and the renderer both
//! read. Adding a widget means adding one enum variant in `config::customization`, one
//! registry row here, and one match arm in [`Ferrite::render_widget`] — nothing else in
//! the launcher needs to know it exists.
//!
//! Widgets draw during nested egui closures that borrow `self`, so a widget never
//! performs a state transition directly. It returns a [`WidgetAction`], which the grid
//! renderer applies after every closure has released its borrow.

use super::{Ferrite, MUTED, Page, view};
use crate::config::{LayoutPage, Widget, WidgetAction, WidgetFrame, WidgetStyle};
use eframe::egui::{self, Color32, RichText};

/// One entry in the editor's "add widget" palette.
#[derive(Clone, Copy)]
pub(super) struct WidgetRegistration {
    pub kind: WidgetKind,
    pub label: &'static str,
    /// Short explanation shown as hover text in the palette.
    pub help: &'static str,
    pub group: WidgetGroup,
    /// Span applied when the widget is first dropped onto the grid.
    pub default_width: u8,
    pub default_height: u16,
}

/// Palette grouping, so a 20-entry list stays scannable.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum WidgetGroup {
    Shell,
    Navigation,
    Play,
    Pages,
    Freeform,
}

impl WidgetGroup {
    pub const ALL: &'static [Self] = &[
        Self::Shell,
        Self::Navigation,
        Self::Play,
        Self::Pages,
        Self::Freeform,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Shell => "Whole bands",
            Self::Navigation => "Navigation",
            Self::Play => "Play page",
            Self::Pages => "Page bodies",
            Self::Freeform => "Free-form",
        }
    }
}

/// A widget variant without its payload, used for palette selection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum WidgetKind {
    TopBar,
    PageBody,
    StatusBar,
    Logo,
    NavBar,
    NavButton,
    AccountButton,
    HeroBanner,
    InstanceSelector,
    LaunchButton,
    SelectedInstance,
    AccountSummary,
    LauncherStatus,
    UpdateBanner,
    InstancesBody,
    ModsBody,
    SettingsBody,
    Text,
    ActionButton,
    Spacer,
    Separator,
}

macro_rules! registry {
    ($($kind:ident, $label:literal, $help:literal, $group:ident, $w:literal, $h:literal;)*) => {
        pub(super) const WIDGET_REGISTRY: &[WidgetRegistration] = &[
            $(WidgetRegistration {
                kind: WidgetKind::$kind,
                label: $label,
                help: $help,
                group: WidgetGroup::$group,
                default_width: $w,
                default_height: $h,
            }),*
        ];
    };
}

registry! {
    TopBar, "Top bar", "The whole original header: logo, navigation, account, settings.", Shell, 12, 1;
    PageBody, "Page body", "Whatever the current page draws, exactly as it always has.", Shell, 12, 10;
    StatusBar, "Status bar", "Task status on the left, launcher version on the right.", Shell, 12, 1;
    Logo, "Logo", "The F mark with the FERRITE LAUNCHER wordmark.", Navigation, 3, 1;
    NavBar, "Navigation buttons", "Play, Instances, and Mods as one row.", Navigation, 5, 1;
    NavButton, "Single nav button", "One button that switches to a page you choose.", Navigation, 2, 1;
    AccountButton, "Account button", "Opens the Microsoft account dialog; shows the signed-in name.", Navigation, 2, 1;
    HeroBanner, "Hero banner", "READY TO PLAY, the instance name, and the instance picker.", Play, 12, 4;
    InstanceSelector, "Instance picker", "Just the instance dropdown, without the banner.", Play, 4, 1;
    LaunchButton, "Launch button", "The large accent-filled launch action.", Play, 12, 1;
    SelectedInstance, "Instance overview", "Profile, version, loader, and memory for the selection.", Play, 6, 3;
    AccountSummary, "Account summary", "Sign-in state as a card.", Play, 6, 2;
    LauncherStatus, "Launcher status", "Discord, game state, and update status as a card.", Play, 6, 3;
    UpdateBanner, "Update banner", "The new-release notice, when one is available.", Play, 12, 1;
    InstancesBody, "Instances page", "The full instance manager.", Pages, 12, 10;
    ModsBody, "Mods page", "The full mod browser and installed list.", Pages, 12, 10;
    SettingsBody, "Settings page", "The full settings UI, including this editor.", Pages, 12, 10;
    Text, "Text", "Any text you want.", Freeform, 4, 1;
    ActionButton, "Action button", "A button with your own label bound to a launcher action.", Freeform, 3, 1;
    Spacer, "Spacer", "Reserves empty space.", Freeform, 1, 1;
    Separator, "Separator", "A horizontal rule.", Freeform, 12, 1;
}

impl WidgetKind {
    pub fn registration(self) -> &'static WidgetRegistration {
        WIDGET_REGISTRY
            .iter()
            .find(|registration| registration.kind == self)
            .unwrap_or(&WIDGET_REGISTRY[0])
    }

    pub fn label(self) -> &'static str {
        self.registration().label
    }

    /// Builds a widget of this kind with sensible starting content.
    pub fn build(self) -> Widget {
        match self {
            Self::TopBar => Widget::TopBar,
            Self::PageBody => Widget::PageBody,
            Self::StatusBar => Widget::StatusBar,
            Self::Logo => Widget::Logo,
            Self::NavBar => Widget::NavBar,
            Self::NavButton => Widget::NavButton {
                page: LayoutPage::Play,
            },
            Self::AccountButton => Widget::AccountButton,
            Self::HeroBanner => Widget::HeroBanner,
            Self::InstanceSelector => Widget::InstanceSelector,
            Self::LaunchButton => Widget::LaunchButton,
            Self::SelectedInstance => Widget::SelectedInstance,
            Self::AccountSummary => Widget::AccountSummary,
            Self::LauncherStatus => Widget::LauncherStatus,
            Self::UpdateBanner => Widget::UpdateBanner,
            Self::InstancesBody => Widget::InstancesBody,
            Self::ModsBody => Widget::ModsBody,
            Self::SettingsBody => Widget::SettingsBody,
            Self::Text => Widget::Text {
                text: "Custom text".to_owned(),
            },
            Self::ActionButton => Widget::ActionButton {
                label: "Launch".to_owned(),
                action: WidgetAction::Launch,
            },
            Self::Spacer => Widget::Spacer,
            Self::Separator => Widget::Separator,
        }
    }
}

/// Maps a configured widget back to its palette entry.
pub(super) fn widget_kind(widget: &Widget) -> WidgetKind {
    match widget {
        Widget::TopBar => WidgetKind::TopBar,
        Widget::PageBody => WidgetKind::PageBody,
        Widget::StatusBar => WidgetKind::StatusBar,
        Widget::Logo => WidgetKind::Logo,
        Widget::NavBar => WidgetKind::NavBar,
        Widget::NavButton { .. } => WidgetKind::NavButton,
        Widget::AccountButton => WidgetKind::AccountButton,
        Widget::HeroBanner => WidgetKind::HeroBanner,
        Widget::InstanceSelector => WidgetKind::InstanceSelector,
        Widget::LaunchButton => WidgetKind::LaunchButton,
        Widget::SelectedInstance => WidgetKind::SelectedInstance,
        Widget::AccountSummary => WidgetKind::AccountSummary,
        Widget::LauncherStatus => WidgetKind::LauncherStatus,
        Widget::UpdateBanner => WidgetKind::UpdateBanner,
        Widget::InstancesBody => WidgetKind::InstancesBody,
        Widget::ModsBody => WidgetKind::ModsBody,
        Widget::SettingsBody => WidgetKind::SettingsBody,
        Widget::Text { .. } => WidgetKind::Text,
        Widget::ActionButton { .. } => WidgetKind::ActionButton,
        Widget::Spacer => WidgetKind::Spacer,
        Widget::Separator => WidgetKind::Separator,
    }
}

/// A human label for one configured widget, including its payload where useful.
pub(super) fn widget_label(widget: &Widget) -> String {
    match widget {
        Widget::NavButton { page } => format!("Go to {}", page.label()),
        Widget::ActionButton { label, .. } => format!("Button · {label}"),
        Widget::Text { text } => {
            let preview: String = text.chars().take(24).collect();
            format!("Text · {preview}")
        }
        other => widget_kind(other).label().to_owned(),
    }
}

/// Every action a button widget can be bound to, with its menu label.
pub(super) const ACTIONS: &[(WidgetAction, &str)] = &[
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
    (WidgetAction::ExportPack, "Export pack"),
    (WidgetAction::OpenAccount, "Open account"),
    (WidgetAction::OpenConfigFolder, "Open config folder"),
    (WidgetAction::Settings, "Open settings"),
];

pub(super) fn action_label(action: &WidgetAction) -> &'static str {
    ACTIONS
        .iter()
        .find(|(candidate, _)| candidate == action)
        .map(|(_, label)| *label)
        .unwrap_or("Action")
}

impl Ferrite {
    /// Draws one placement's frame and then its widget, returning any requested action.
    pub(super) fn render_widget(
        &mut self,
        ui: &mut egui::Ui,
        widget: &Widget,
        style: &WidgetStyle,
    ) -> Option<WidgetAction> {
        let theme = self.theme();
        let frame = match style.frame {
            WidgetFrame::None => egui::Frame::new(),
            WidgetFrame::Card => egui::Frame::new()
                .fill(theme.card)
                .corner_radius(theme.corner_radius),
            WidgetFrame::Sidebar => egui::Frame::new()
                .fill(theme.sidebar)
                .corner_radius(theme.corner_radius),
            WidgetFrame::Outline => egui::Frame::new()
                .stroke(egui::Stroke::new(1.0, theme.accent.gamma_multiply(0.45)))
                .corner_radius(theme.corner_radius),
        }
        .inner_margin(egui::Margin::same(i8::try_from(style.padding).unwrap_or(0)));

        let mut action = None;
        frame.show(ui, |ui| {
            ui.set_min_size(ui.available_size());
            action = self.render_widget_content(ui, widget);
        });
        action
    }

    fn render_widget_content(
        &mut self,
        ui: &mut egui::Ui,
        widget: &Widget,
    ) -> Option<WidgetAction> {
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
            Widget::Logo => self.logo_widget(ui),
            Widget::NavBar => self.nav_bar_widget(ui),
            Widget::NavButton { page } => return self.nav_button_widget(ui, *page),
            Widget::AccountButton => return self.account_button_widget(ui),
            Widget::HeroBanner => self.hero_widget(ui),
            Widget::InstanceSelector => self.instance_selector_widget(ui),
            Widget::LaunchButton => return self.launch_button_widget(ui),
            Widget::SelectedInstance => self.selected_instance_summary(ui),
            Widget::AccountSummary => self.account_summary(ui),
            Widget::LauncherStatus => self.launcher_status_summary(ui),
            Widget::UpdateBanner => self.update_banner(ui),
            Widget::InstancesBody => self.instances_page(ui),
            Widget::ModsBody => self.mods_page(ui),
            Widget::SettingsBody => self.settings_page(ui),
            Widget::Text { text } => {
                ui.add(egui::Label::new(text).wrap());
            }
            Widget::ActionButton { label, action } => {
                let enabled = self.action_is_available(action);
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
            Widget::Spacer => {
                ui.allocate_space(ui.available_size());
            }
            Widget::Separator => {
                ui.separator();
            }
        }
        None
    }

    /// Whether an action can run right now, used to grey out buttons.
    fn action_is_available(&self, action: &WidgetAction) -> bool {
        match action {
            WidgetAction::Launch => {
                self.selected_instance().is_some()
                    && self.mod_task.is_none()
                    && self.pending_uninstall.is_none()
                    && !self.pack_busy()
            }
            WidgetAction::ImportPack | WidgetAction::ExportPack => !self.pack_busy(),
            WidgetAction::CreateInstance => self.instance_creation_task.is_none(),
            _ => true,
        }
    }

    /// Runs a widget action after the frame's nested closures have released `self`.
    pub(super) fn perform_widget_action(&mut self, action: WidgetAction) {
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
            WidgetAction::ExportPack => {
                if !self.pack_busy() {
                    // Same entry point as the card menu, so the prefill flag is set.
                    self.open_export_window();
                }
            }
            WidgetAction::OpenAccount => self.auth.open = true,
            WidgetAction::OpenConfigFolder => {
                self.config_status = Some(match crate::config::open_config_folder() {
                    Ok(()) => "Opened the config folder.".to_owned(),
                    Err(error) => format!("Could not open config folder: {error}"),
                });
            }
            WidgetAction::Settings => self.current_page = Page::Settings,
        }
    }

    /// Central stop action shared by the fixed shell and configurable buttons.
    pub(super) fn stop_game(&mut self) {
        self.running_text = match crate::minecraft::kill() {
            Ok(()) => String::from("Game not running."),
            Err(error) => format!("Failed to stop game: {error}"),
        };
    }

    // -- Granular pieces of the original shell ------------------------------
    //
    // These are the same draw calls the fixed views make, extracted so they can be
    // placed independently. `view.rs` should call them too, so there is exactly one
    // definition of what the logo or the launch button looks like.

    pub(super) fn logo_widget(&mut self, ui: &mut egui::Ui) {
        let accent = self.accent_color();
        ui.horizontal(|ui| {
            ui.label(RichText::new("F").size(24.0).strong().color(accent));
            ui.label(RichText::new("FERRITE").size(21.0).strong());
            ui.label(RichText::new("LAUNCHER").size(11.0).color(MUTED));
        });
    }

    pub(super) fn nav_bar_widget(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            for (page, label) in [
                (Page::Play, "Play"),
                (Page::Instances, "Instances"),
                (Page::Mods, "Mods"),
            ] {
                view::top_nav_button(ui, &mut self.current_page, page, label);
            }
        });
    }

    fn nav_button_widget(&mut self, ui: &mut egui::Ui, page: LayoutPage) -> Option<WidgetAction> {
        let target = page_from_layout(page);
        let selected = self.current_page == target;
        ui.add_sized(
            ui.available_size(),
            egui::Button::selectable(selected, RichText::new(page.label()).strong()),
        )
        .clicked()
        .then_some(WidgetAction::Navigate { page })
    }

    fn account_button_widget(&mut self, ui: &mut egui::Ui) -> Option<WidgetAction> {
        let label = if self.auth.offline_mode {
            "Offline".to_owned()
        } else if let Some(account) = &self.auth.account {
            account.name.clone()
        } else {
            "Account".to_owned()
        };
        ui.add_sized(ui.available_size(), egui::Button::new(label))
            .clicked()
            .then_some(WidgetAction::OpenAccount)
    }

    /// The Play page's hero block, without the surrounding page frame.
    pub(super) fn hero_widget(&mut self, ui: &mut egui::Ui) {
        let selected = self.selected_instance().cloned();
        let accent = self.accent_color();
        let theme = self.theme();
        egui::Frame::new()
            .fill(if theme.light {
                Color32::from_rgb(222, 228, 239)
            } else {
                Color32::from_rgb(13, 18, 25)
            })
            .stroke(egui::Stroke::new(1.0, accent.gamma_multiply(0.35)))
            .corner_radius(theme.corner_radius)
            .show(ui, |ui| {
                ui.set_min_size(ui.available_size());
                ui.vertical_centered(|ui| {
                    ui.add_space(18.0);
                    ui.label(RichText::new("READY TO PLAY").size(11.0).strong().color(accent));
                    ui.add_space(8.0);
                    ui.label(
                        RichText::new(
                            selected
                                .as_ref()
                                .map(|instance| instance.name.as_str())
                                .unwrap_or("No instance selected"),
                        )
                        .size(30.0)
                        .strong(),
                    );
                    match &selected {
                        Some(instance) => ui.label(
                            RichText::new(format!(
                                "Minecraft {}  ·  {}",
                                instance.version, instance.loader
                            ))
                            .size(15.0)
                            .color(MUTED),
                        ),
                        None => ui.label(
                            RichText::new("Create or import an instance to get started.")
                                .color(MUTED),
                        ),
                    };
                    ui.add_space(14.0);
                    self.instance_selector_widget(ui);
                });
            });
    }

    pub(super) fn instance_selector_widget(&mut self, ui: &mut egui::Ui) {
        let label = self.selected_instance_label();
        egui::ComboBox::from_id_salt("widget_instance_selector")
            .width(310.0_f32.min(ui.available_width()))
            .selected_text(label)
            .show_ui(ui, |ui| {
                for (index, instance) in self.instances.iter().enumerate() {
                    ui.selectable_value(
                        &mut self.selected_instance,
                        Some(index),
                        format!(
                            "{} · {} · {}",
                            instance.name, instance.version, instance.loader
                        ),
                    );
                }
            });
    }

    pub(super) fn launch_button_widget(&mut self, ui: &mut egui::Ui) -> Option<WidgetAction> {
        let enabled = self.action_is_available(&WidgetAction::Launch);
        let accent = self.accent_color();
        let label = match self.selected_instance() {
            Some(instance) => format!("LAUNCH {}", instance.loader.to_uppercase()),
            None => "SELECT AN INSTANCE".to_owned(),
        };
        ui.add_enabled(
            enabled,
            egui::Button::new(RichText::new(label).size(16.0).strong())
                .fill(accent)
                .min_size(ui.available_size()),
        )
        .clicked()
        .then_some(WidgetAction::Launch)
    }

    // -- Summary cards ------------------------------------------------------

    pub(super) fn layout_status_bar(&mut self, ui: &mut egui::Ui) {
        let status = if self.instance_creation_task.is_some() {
            self.instance_creation_status
                .clone()
                .unwrap_or_else(|| self.running_text.clone())
        } else if self.pack_task.is_some() {
            self.pack_status
                .clone()
                .unwrap_or_else(|| self.running_text.clone())
        } else {
            self.running_text.clone()
        };
        let accent = self.accent_color();
        ui.separator();
        ui.horizontal(|ui| {
            ui.label(RichText::new("STATUS").small().strong().color(accent));
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

    fn selected_instance_summary(&mut self, ui: &mut egui::Ui) {
        let instance = self.selected_instance().cloned();
        let memory = self.config.minecraft.default_memory_mb;
        self.summary_frame(ui, "SELECTED INSTANCE", |ui| match instance {
            Some(instance) => {
                summary_row(ui, "Profile", &instance.name);
                summary_row(ui, "Minecraft", &instance.version);
                summary_row(ui, "Loader", &instance.loader);
                summary_row(ui, "Memory", &format!("{memory} MB"));
            }
            None => {
                ui.label(RichText::new("No instance selected.").color(MUTED));
            }
        });
    }

    fn account_summary(&mut self, ui: &mut egui::Ui) {
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
        self.summary_frame(ui, "ACCOUNT", |ui| {
            ui.label(status);
        });
    }

    fn launcher_status_summary(&mut self, ui: &mut egui::Ui) {
        let discord = self.discord.is_some();
        let running = crate::minecraft::is_running();
        let update = self.update_status.clone();
        self.summary_frame(ui, "LAUNCHER STATUS", |ui| {
            summary_row(
                ui,
                "Discord",
                if discord {
                    "Rich Presence active"
                } else {
                    "Not connected"
                },
            );
            summary_row(ui, "Game", if running { "Running" } else { "Ready" });
            if let Some(status) = update {
                summary_row(ui, "Updates", &status);
            }
        });
    }

    fn summary_frame(
        &self,
        ui: &mut egui::Ui,
        title: &str,
        add_contents: impl FnOnce(&mut egui::Ui),
    ) {
        let theme = self.theme();
        egui::Frame::new()
            .fill(theme.card)
            .corner_radius(theme.corner_radius)
            .inner_margin(12.0)
            .show(ui, |ui| {
                ui.set_min_size(ui.available_size());
                ui.label(RichText::new(title).strong());
                ui.separator();
                add_contents(ui);
            });
    }
}

fn summary_row(ui: &mut egui::Ui, label: &str, value: &str) {
    ui.horizontal(|ui| {
        ui.label(RichText::new(label).color(MUTED));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.add(egui::Label::new(RichText::new(value).strong()).truncate());
        });
    });
}

pub(super) fn page_from_layout(page: LayoutPage) -> Page {
    match page {
        LayoutPage::Play => Page::Play,
        LayoutPage::Instances => Page::Instances,
        LayoutPage::Mods => Page::Mods,
        LayoutPage::Settings => Page::Settings,
    }
}

pub(super) fn layout_page_from_page(page: Page) -> LayoutPage {
    match page {
        Page::Play => LayoutPage::Play,
        Page::Instances => LayoutPage::Instances,
        Page::Mods => LayoutPage::Mods,
        Page::Settings => LayoutPage::Settings,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_widget_variant_has_a_registry_entry() {
        for registration in WIDGET_REGISTRY {
            let widget = registration.kind.build();
            assert_eq!(widget_kind(&widget), registration.kind);
        }
    }

    #[test]
    fn registry_defaults_fit_the_default_grid() {
        for registration in WIDGET_REGISTRY {
            assert!(registration.default_width >= 1 && registration.default_width <= 12);
            assert!(registration.default_height >= 1);
        }
    }

    #[test]
    fn labels_include_payloads_for_configurable_widgets() {
        let widget = Widget::NavButton {
            page: LayoutPage::Mods,
        };
        assert_eq!(widget_label(&widget), "Go to Mods");
    }
}
