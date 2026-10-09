//! Microsoft account presentation and frontend worker scheduling.

use super::Ferrite;
use crate::auth::SessionUpdate;
use eframe::egui::{self, RichText};

impl Ferrite {
    /// Schedules the core's blocking worker while retaining device/status presentation.
    pub(super) fn start_sign_in(&mut self) {
        match self.auth.session.begin_login(&self.auth.client_id) {
            Ok(Some(worker)) => {
                self.auth.device = None;
                self.auth.status = "Starting Microsoft sign-in...".into();
                std::thread::spawn(move || worker.run());
            }
            Ok(None) => {}
            Err(error) => self.auth.status = error,
        }
    }

    pub(super) fn cancel_sign_in(&mut self) {
        self.auth.session.cancel();
        self.auth.device = None;
        self.auth.status = "Sign-in cancelled.".into();
    }

    pub(super) fn sign_out(&mut self) {
        self.auth.session.sign_out();
        self.auth.device = None;
        self.auth.status = "Signed out.".into();
        self.running_text = self.auth.status.clone();
    }

    /// Drains display-safe updates regardless of the visible page.
    pub(super) fn poll_auth(&mut self) {
        while let Some(update) = self.auth.session.poll() {
            match update {
                SessionUpdate::Progress(message) => self.auth.status = message,
                SessionUpdate::Device {
                    user_code,
                    verification_uri,
                } => {
                    self.auth.device = Some((user_code, verification_uri));
                    self.auth.status =
                        "Open the Microsoft link and enter the code to sign in.".into();
                }
                SessionUpdate::Finished { status } => {
                    self.auth.device = None;
                    self.auth.status = status;
                }
            }
            self.running_text = self.auth.status.clone();
        }
    }

    /// Shows the launch authentication mode and account state on Play and Settings.
    pub(super) fn account_section(&mut self, ui: &mut egui::Ui) {
        ui.heading("Account");
        ui.horizontal(|ui| {
            ui.selectable_value(&mut self.auth.offline_mode, false, "Microsoft account");
            ui.selectable_value(&mut self.auth.offline_mode, true, "Offline mode");
        });

        if self.auth.offline_mode {
            ui.label("Offline mode uses the name 'Player' and does not require Microsoft sign-in.");
            ui.label(
                RichText::new("Online-mode servers and paid-account services will not work.")
                    .color(self.muted_color()),
            );
            return;
        }

        if let Some(account) = self.auth.session.account() {
            ui.label(format!("Signed in as {}", account.name));
            ui.label(if account.is_expired() {
                "Session expired — sign in again before playing."
            } else {
                "Session active. Sign in again when it expires."
            });
        } else {
            ui.label("Sign in with Microsoft to play Minecraft Java.");
        }
        ui.horizontal(|ui| {
            if ui
                .button(if self.auth.session.account().is_some() {
                    "Sign in again / Account"
                } else {
                    "Sign in / Account"
                })
                .clicked()
            {
                self.auth.open = true;
            }
            if self.auth.session.account().is_some() && ui.button("Sign out").clicked() {
                self.sign_out();
            }
        });
    }

    /// Displays public device instructions only; closing the dialog cancels pending login.
    pub(super) fn account_window(&mut self, context: &egui::Context) {
        if !self.auth.open {
            return;
        }
        // Keep egui's `open` borrow local; reconcile it with `self` after the closure.
        let mut open = true;
        egui::Window::new("Microsoft account")
            .open(&mut open)
            .collapsible(false)
            .default_width(440.0)
            .show(context, |ui| {
                self.account_section(ui);
                ui.separator();
                ui.label("Credentials stay in memory for this launcher session only.");
                ui.label("Microsoft public-client application ID");
                ui.add_enabled(!self.auth.session.is_pending(), egui::TextEdit::singleline(&mut self.auth.client_id));
                ui.label("Uses FERRITE_MICROSOFT_CLIENT_ID when set. Supply your own application configured for consumer device-code sign-in and Minecraft API access.");
                if let Some((code, uri)) = &self.auth.device {
                    ui.hyperlink_to("Open Microsoft sign-in in your browser", uri);
                    ui.horizontal(|ui| {
                        ui.monospace(code);
                        if ui.button("Copy code").clicked() {
                            ui.ctx().copy_text(code.clone());
                        }
                    });
                }
                if self.auth.session.is_pending() {
                    ui.spinner();
                    if ui.button("Cancel sign-in").clicked() { self.cancel_sign_in(); }
                } else if ui.button("Start Microsoft sign-in").clicked() {
                    self.start_sign_in();
                }
                ui.label(&self.auth.status);
            });
        self.auth.open = open;
        if !open && self.auth.session.is_pending() {
            self.cancel_sign_in();
        }
    }
}
