//! Shared top-level navigation and the Play dashboard.
//!
//! These immediate-mode views derive their display from [`Ferrite`] each frame. Actions
//! that require mutable application calls are recorded during nested egui closures and
//! applied afterward to satisfy borrowing rules and keep transitions deterministic.

use super::{Ferrite, MUTED, Page};
use eframe::egui::{self, Color32, RichText};

impl Ferrite {
    /// Draws Ferrite's full-width header and centered primary navigation.
    pub(super) fn top_bar(&mut self, ui: &mut egui::Ui) {
        let account_label = if self.auth.offline_mode {
            "Offline".to_owned()
        } else if let Some(account) = &self.auth.account {
            account.name.clone()
        } else {
            "Account".to_owned()
        };
        egui::Frame::new()
            .fill(self.sidebar_color())
            .corner_radius(self.ui_settings.appearance.corner_radius)
            .inner_margin(egui::Margin::symmetric(18, 12))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new("F")
                            .size(24.0)
                            .strong()
                            .color(self.accent_color()),
                    );
                    ui.label(RichText::new("FERRITE").size(21.0).strong());
                    ui.label(RichText::new("LAUNCHER").size(11.0).color(MUTED));
                    ui.add_space(32.0);
                    top_nav_button(ui, &mut self.current_page, Page::Play, "Play");
                    top_nav_button(ui, &mut self.current_page, Page::Instances, "Instances");
                    top_nav_button(ui, &mut self.current_page, Page::Mods, "Mods");
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button("Settings").clicked() {
                            self.current_page = Page::Settings;
                        }
                        if ui.button(account_label).clicked() {
                            self.auth.open = true;
                        }
                    });
                });
            });
    }

    /// Draws the launch dashboard with a hero, primary action, and status cards.
    pub(super) fn play_page(&mut self, ui: &mut egui::Ui) {
        // Own the snapshot so controls can freely mutate `self` later in this frame.
        let selected = self.selected_instance().cloned();
        // This instance's own state (running, updating, copying, folder missing).
        let launch_block = selected.as_ref().and_then(|instance| {
            ferrite_launcher::core::activity::disabled_reason(
                ferrite_launcher::core::activity::InstanceAction::Play,
                &instance.name,
                &self.instance_status(instance),
            )
        });
        let can_launch = selected.is_some()
            && launch_block.is_none()
            && self.mod_task.is_none()
            && self.pending_uninstall.is_none()
            && !self.pack_busy();
        let mut open_instances = false;
        let mut open_mods = false;
        let mut open_account = false;

        egui::Frame::new()
            .fill(if self.ui_settings.appearance.theme == "light" {
                Color32::from_rgb(222, 228, 239)
            } else {
                Color32::from_rgb(13, 18, 25)
            })
            .stroke(egui::Stroke::new(
                1.0,
                self.accent_color().gamma_multiply(0.35),
            ))
            .corner_radius(self.ui_settings.appearance.corner_radius)
            .inner_margin(0.0)
            .show(ui, |ui| {
                let hero_height = ui.available_height().clamp(210.0, 300.0) * 0.82;
                ui.allocate_ui_with_layout(
                    egui::vec2(ui.available_width(), hero_height),
                    egui::Layout::top_down(egui::Align::Center),
                    |ui| {
                        let rect = ui.max_rect();
                        let painter = ui.painter();
                        painter.circle_filled(
                            rect.left_top() + egui::vec2(rect.width() * 0.18, rect.height() * 0.25),
                            rect.height() * 0.55,
                            self.accent_color().gamma_multiply(0.06),
                        );
                        painter.circle_filled(
                            rect.right_bottom()
                                - egui::vec2(rect.width() * 0.18, rect.height() * 0.15),
                            rect.height() * 0.7,
                            Color32::from_rgb(40, 80, 110).gamma_multiply(0.12),
                        );
                        ui.add_space(36.0);
                        ui.label(
                            RichText::new("READY TO PLAY")
                                .size(11.0)
                                .strong()
                                .color(self.accent_color()),
                        );
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
                        if let Some(instance) = &selected {
                            ui.horizontal(|ui| {
                                super::dialogs::loader_text(
                                    ui,
                                    &instance.loader,
                                    RichText::new(format!(
                                        "Minecraft {}  ·  {}",
                                        instance.version, instance.loader
                                    ))
                                    .size(15.0)
                                    .color(MUTED),
                                    MUTED,
                                );
                            });
                        } else {
                            ui.label(
                                RichText::new("Create or import an instance to get started.")
                                    .color(MUTED),
                            );
                        }
                        ui.add_space(18.0);
                        ui.allocate_ui_with_layout(
                            egui::vec2(ui.available_width(), 30.0),
                            egui::Layout::top_down(egui::Align::Center),
                            |ui| {
                                egui::ComboBox::from_id_salt("hero_instance")
                                    .width(310.0)
                                    .selected_text(self.selected_instance_label())
                                    .show_ui(ui, |ui| {
                                        for (index, instance) in self.instances.iter().enumerate() {
                                            ui.selectable_value(
                                                &mut self.selected_instance,
                                                Some(index),
                                                format!(
                                                    "{} · {} · {}",
                                                    instance.name,
                                                    instance.version,
                                                    instance.loader
                                                ),
                                            );
                                        }
                                    });
                            },
                        );
                    },
                );
            });

        ui.add_space(14.0);
        let launch = ui.add_enabled(
            can_launch,
            egui::Button::new(
                RichText::new(match selected.as_ref() {
                    Some(instance) => format!("LAUNCH {}", instance.loader.to_uppercase()),
                    None => "SELECT AN INSTANCE".to_owned(),
                })
                .size(16.0)
                .strong(),
            )
            .fill(self.accent_color())
            .min_size(egui::vec2(ui.available_width(), 56.0)),
        );
        let launch_clicked = launch.clicked();
        if let Some(reason) = &launch_block {
            launch.on_disabled_hover_text(reason);
        }
        if launch_clicked {
            self.launch_selected();
        }

        ui.add_space(14.0);
        let panel_width = ((ui.available_width() - 14.0) / 2.0).max(260.0);
        let panel_height = 205.0;
        ui.horizontal_top(|ui| {
            ui.allocate_ui_with_layout(
                egui::vec2(panel_width, panel_height),
                egui::Layout::top_down(egui::Align::LEFT),
                |ui| {
                    egui::Frame::new()
                        .fill(self.card_color())
                        .corner_radius(self.ui_settings.appearance.corner_radius)
                        .inner_margin(18.0)
                        .show(ui, |ui| {
                            ui.set_min_size(egui::vec2(
                                (panel_width - 36.0).max(0.0),
                                panel_height - 36.0,
                            ));
                            ui.label(RichText::new("INSTANCE OVERVIEW").strong());
                            ui.separator();
                            if let Some(instance) = &selected {
                                dashboard_row(ui, "Profile", &instance.name);
                                dashboard_row(ui, "Minecraft", &instance.version);
                                dashboard_loader_row(ui, &instance.loader);
                                dashboard_row(
                                    ui,
                                    "Memory",
                                    &format!("{} MB", self.config.minecraft.default_memory_mb),
                                );
                            } else {
                                ui.label(RichText::new("No instance selected.").color(MUTED));
                            }
                            ui.with_layout(egui::Layout::bottom_up(egui::Align::LEFT), |ui| {
                                ui.horizontal_wrapped(|ui| {
                                    open_instances |= ui.button("Manage Instances").clicked();
                                    open_mods |= ui
                                        .add_enabled(
                                            selected.is_some(),
                                            egui::Button::new("Manage Mods"),
                                        )
                                        .clicked();
                                });
                            });
                        });
                },
            );
            ui.add_space(14.0);
            ui.allocate_ui_with_layout(
                egui::vec2(panel_width, panel_height),
                egui::Layout::top_down(egui::Align::LEFT),
                |ui| {
                    egui::Frame::new()
                        .fill(self.card_color())
                        .corner_radius(self.ui_settings.appearance.corner_radius)
                        .inner_margin(18.0)
                        .show(ui, |ui| {
                            ui.set_min_size(egui::vec2(
                                (panel_width - 36.0).max(0.0),
                                panel_height - 36.0,
                            ));
                            ui.label(RichText::new("LAUNCHER STATUS").strong());
                            ui.separator();
                            let account = if self.auth.offline_mode {
                                "Offline mode".to_owned()
                            } else {
                                self.auth
                                    .account
                                    .as_ref()
                                    .map(|account| format!("Signed in as {}", account.name))
                                    .unwrap_or_else(|| "Microsoft account required".to_owned())
                            };
                            dashboard_row(ui, "Account", &account);
                            dashboard_row(
                                ui,
                                "Discord",
                                if self.discord.is_some() {
                                    "Rich Presence active"
                                } else {
                                    "Not connected"
                                },
                            );
                            dashboard_row(
                                ui,
                                "Game",
                                if crate::minecraft::is_running() {
                                    "Running"
                                } else {
                                    "Ready"
                                },
                            );
                            ui.with_layout(egui::Layout::bottom_up(egui::Align::LEFT), |ui| {
                                ui.horizontal_wrapped(|ui| {
                                    open_account |= ui.button("Account").clicked();
                                    if ui.button("Stop Game").clicked() {
                                        self.stop_game();
                                    }
                                });
                            });
                        });
                },
            );
        });

        // Apply navigation after all nested UI closures have released their borrows.
        if open_instances {
            self.current_page = Page::Instances;
        } else if open_mods {
            if let Some(instance) = selected {
                self.set_mod_target(instance);
                self.show_installed = true;
                self.current_page = Page::Mods;
                self.local_mod_task(None);
            }
        }
        if open_account {
            self.auth.open = true;
        }
    }
}

/// Draws one compact destination in the top navigation bar.
pub(super) fn top_nav_button(ui: &mut egui::Ui, current: &mut Page, page: Page, label: &str) {
    let selected = *current == page;
    if ui
        .add_sized(
            [112.0, 38.0],
            egui::Button::selectable(selected, RichText::new(label).strong()),
        )
        .clicked()
    {
        *current = page;
    }
}

/// Draws a two-column label/value row, truncating values to protect card layout.
pub(super) fn dashboard_row(ui: &mut egui::Ui, label: &str, value: &str) {
    ui.columns(2, |columns| {
        columns[0].label(RichText::new(label).color(MUTED));
        columns[1].with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.add(egui::Label::new(RichText::new(value).strong()).truncate());
        });
    });
}

/// The "Loader" overview row, with the Quilt badge when relevant (spec §8).
fn dashboard_loader_row(ui: &mut egui::Ui, loader: &str) {
    ui.columns(2, |columns| {
        columns[0].label(RichText::new("Loader").color(MUTED));
        columns[1].with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            super::dialogs::quilt_badge(ui, loader, MUTED);
            ui.add(egui::Label::new(RichText::new(loader).strong()).truncate());
        });
    });
}

/// Draws a consistent page title and supporting subtitle.
pub(super) fn page_heading(ui: &mut egui::Ui, title: &str, subtitle: &str) {
    ui.heading(RichText::new(title).size(32.0).strong());
    ui.label(RichText::new(subtitle).color(MUTED));
}
