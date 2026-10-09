//! Modrinth browsing and local per-instance mod management.
//!
//! All mod operations share one worker slot. Each operation captures its filters or
//! target profile by value, and result application checks that provenance so navigation,
//! selection changes, or deleted instances cannot redirect asynchronous results.

use super::{Ferrite, ModTaskResult, page_heading};
use crate::instances::InstanceProfile;
use crate::modrinth::SearchFilters;
use eframe::egui::{self, RichText};
use std::sync::mpsc::{self, TryRecvError};

impl Ferrite {
    /// Changes only the explicit mod target; active workers and confirmations lock it.
    pub(super) fn set_mod_target(&mut self, target: InstanceProfile) {
        if self.mod_task.is_some() || self.pending_uninstall.is_some() {
            return;
        }
        self.mod_filters.game_version = Some(target.version.clone());
        self.mod_filters.loader =
            (target.loader != "Vanilla").then(|| target.loader.to_ascii_lowercase());
        self.mod_target = Some(target);
        self.installed_mods = None;
    }

    /// Serializes all mod work, including searches triggered by Enter.
    ///
    /// `work` must own its inputs because the detached worker is `'static`; only its
    /// terminal [`ModTaskResult`] crosses back to UI-owned state.
    pub(super) fn start_mod_task(&mut self, work: impl FnOnce() -> ModTaskResult + Send + 'static) {
        let owner = match self.activity.begin_mod(self.pending_uninstall.is_some()) {
            Ok(owner) => owner,
            Err(_) => return,
        };
        self.mod_owner = Some(owner);
        let (sender, receiver) = mpsc::channel();
        match std::thread::Builder::new()
            .name("mods".into())
            .spawn(move || {
                let _ = sender.send(work());
            }) {
            Ok(_) => {
                self.mod_task = Some(receiver);
                self.running_text = "Working on mods…".into();
            }
            Err(error) => {
                if let Some(owner) = self.mod_owner.take() {
                    self.activity.complete(owner);
                }
                self.running_text = format!("Failed to start mod worker: {error}");
            }
        }
    }

    /// Searches with a captured filter set, independent of subsequent form edits.
    pub(super) fn search_modrinth(&mut self, filters: SearchFilters) {
        self.start_mod_task(move || {
            let result = crate::modrinth::search_filtered(&filters);
            ModTaskResult::Search(filters, result)
        });
    }

    /// Downloads project metadata without blocking rendering.
    pub(super) fn open_mod_details(&mut self, id: String, is_mod: bool) {
        self.start_mod_task(move || ModTaskResult::Details(is_mod, crate::modrinth::details(&id)));
    }

    /// Checks the captured target still belongs to the live profile list.
    pub(super) fn target_exists(&self, target: &InstanceProfile) -> bool {
        crate::instance_mods::workflows::target_exists(&self.instances, target)
    }

    /// Downloads a mod and dependencies into an immutable, explicitly chosen target.
    pub(super) fn install_mod(&mut self, project_id: String, title: String, is_mod: bool) {
        if !self.can_install(is_mod) {
            return;
        }
        let Some(target) = self.mod_target.clone() else {
            return;
        };
        self.installed_mods = None;
        let paths = self.paths.clone();
        self.start_mod_task(move || {
            let local =
                crate::instance_mods::workflows::install(&paths, target, &project_id, &title);
            ModTaskResult::Local {
                target: local.target,
                message: local.message,
                result: local.result,
            }
        });
    }

    /// Lists or mutates local jars on a worker, then refreshes even after a failure.
    /// `Some((filename, None))` uninstalls; a boolean enables/disables the exact file.
    pub(super) fn local_mod_task(&mut self, action: Option<(String, Option<bool>)>) {
        if self.mod_task.is_some() || self.pending_uninstall.is_some() {
            return;
        }
        let Some(target) = self.mod_target.clone() else {
            return;
        };
        if !self.target_exists(&target) || crate::minecraft::is_running() {
            return;
        }
        self.installed_mods = None;
        let paths = self.paths.clone();
        self.start_mod_task(move || {
            let local = crate::instance_mods::workflows::manage(&paths, target, action);
            ModTaskResult::Local {
                target: local.target,
                message: local.message,
                result: local.result,
            }
        });
    }

    /// Polls one terminal result without blocking; local data applies only to its target.
    ///
    /// Matching by game directory prevents a result from following a later mod-target
    /// selection, while `target_exists` rejects results for profiles removed meanwhile.
    pub(super) fn poll_mod_task(&mut self) {
        let Some(receiver) = &self.mod_task else {
            return;
        };
        let result = match receiver.try_recv() {
            Ok(result) => result,
            Err(TryRecvError::Empty) => return,
            Err(TryRecvError::Disconnected) => {
                self.mod_task = None;
                if let Some(owner) = self.mod_owner.take() {
                    self.activity.complete(owner);
                }
                self.running_text =
                    "The mod worker stopped unexpectedly; refresh before retrying.".into();
                return;
            }
        };
        self.mod_task = None;
        match result {
            ModTaskResult::Search(filters, result) => match result {
                Ok(response) => {
                    self.running_text = format!("{} Modrinth projects found.", response.total_hits);
                    self.result_filters = filters;
                    self.mod_results = Some(response);
                    self.mod_details = None;
                }
                Err(error) => self.running_text = format!("Search failed: {error}"),
            },
            ModTaskResult::Details(is_mod, result) => match result {
                Ok(details) => {
                    self.mod_details = Some((is_mod, details));
                    self.running_text = "Project details loaded.".into();
                }
                Err(error) => self.running_text = format!("Details failed: {error}"),
            },
            ModTaskResult::Local {
                target,
                message,
                result,
            } => {
                self.running_text = message;
                if crate::instance_mods::workflows::accepts_result(
                    &self.instances,
                    self.mod_target.as_ref(),
                    &target,
                ) {
                    match result {
                        Ok(mods) => self.installed_mods = Some(mods),
                        Err(error) => {
                            self.installed_mods = None;
                            self.running_text
                                .push_str(&format!(" List failed: {error}"));
                        }
                    }
                }
            }
        }
        if let Some(owner) = self.mod_owner.take() {
            self.activity.complete(owner);
        }
    }

    /// Draws the Modrinth browser and local management with an explicit target.
    pub(super) fn mods_page(&mut self, ui: &mut egui::Ui) {
        page_heading(
            ui,
            "Mods",
            "Manage installed jars or browse Modrinth projects.",
        );
        let idle = self.mod_task.is_none() && self.pending_uninstall.is_none();
        // Defer target mutation until the combo releases its borrow of `instances`.
        let mut target = None;
        ui.add_enabled_ui(idle, |ui| {
            egui::ComboBox::from_id_salt("mod_target")
                .selected_text(
                    self.mod_target
                        .as_ref()
                        .map(|p| format!("Target: {} · {} · {}", p.name, p.version, p.loader))
                        .unwrap_or_else(|| "Choose target instance".into()),
                )
                .show_ui(ui, |ui| {
                    for profile in &self.instances {
                        if ui
                            .selectable_label(
                                false,
                                format!(
                                    "{} · {} · {}",
                                    profile.name, profile.version, profile.loader
                                ),
                            )
                            .clicked()
                        {
                            target = Some(profile.clone());
                        }
                    }
                });
        });
        if let Some(target) = target {
            self.set_mod_target(target);
            if self.show_installed {
                self.local_mod_task(None);
            }
        }
        ui.horizontal(|ui| {
            ui.selectable_value(&mut self.show_installed, true, "Installed mods");
            ui.selectable_value(&mut self.show_installed, false, "Browse Modrinth");
        });
        if self.mod_task.is_some() {
            ui.spinner();
        }
        if crate::minecraft::is_running() {
            ui.label("Stop Minecraft before refreshing or changing installed mods.");
        }
        egui::ScrollArea::vertical()
            .id_salt("mods_content")
            .show(ui, |ui| {
                if self.show_installed {
                    self.installed_mods_ui(ui);
                } else if self.mod_details.is_some() {
                    self.mod_details_ui(ui);
                } else {
                    self.mod_browser_ui(ui);
                }
            });
    }

    /// Shows metadata without decoding untrusted archive icons on the UI thread.
    pub(super) fn installed_mods_ui(&mut self, ui: &mut egui::Ui) {
        let ready = self.mod_task.is_none()
            && self.pending_uninstall.is_none()
            && self.mod_target.is_some()
            && !crate::minecraft::is_running();
        if ui
            .add_enabled(ready, egui::Button::new("Refresh installed mods"))
            .clicked()
        {
            self.local_mod_task(None);
        }
        // Record an action while rendering borrowed rows, then mutate state afterward.
        let mut action = None;
        if let Some(mods) = &self.installed_mods {
            if mods.is_empty() {
                ui.label("No installed mods.");
            }
            for installed in mods {
                ui.push_id(&installed.filename, |ui| {
                    ui.separator();
                    ui.strong(&installed.name);
                    ui.label(format!(
                        "Version: {} · {}",
                        installed.version.as_deref().unwrap_or("Unknown"),
                        if installed.enabled {
                            "Enabled"
                        } else {
                            "Disabled"
                        }
                    ));
                    ui.label(&installed.filename);
                    // Include content in the key so replacing a JAR then refreshing
                    // cannot reuse an old icon from the same filename.
                    let mut hash = std::collections::hash_map::DefaultHasher::new();
                    std::hash::Hash::hash(&installed.icon, &mut hash);
                    let key = format!("{:x}", std::hash::Hasher::finish(&hash));
                    self.icons
                        .show_bytes(ui, &key, installed.icon.as_deref(), 48.0);
                    ui.horizontal(|ui| {
                        if ui
                            .add_enabled(
                                ready,
                                egui::Button::new(if installed.enabled {
                                    "Disable"
                                } else {
                                    "Enable"
                                }),
                            )
                            .clicked()
                        {
                            action = Some((installed.filename.clone(), Some(!installed.enabled)));
                        }
                        if ui
                            .add_enabled(ready, egui::Button::new("Uninstall…"))
                            .clicked()
                        {
                            self.pending_uninstall = self
                                .mod_target
                                .clone()
                                .map(|target| (target, installed.filename.clone()));
                        }
                    });
                });
            }
        } else {
            ui.label("Refresh to load this instance's mods.");
        }
        if let Some(action) = action {
            self.local_mod_task(Some(action));
        }
    }

    /// Requires explicit confirmation of both instance and exact filename.
    pub(super) fn uninstall_window(&mut self, context: &egui::Context) {
        let Some((target, filename)) = self.pending_uninstall.clone() else {
            return;
        };
        let mut open = true;
        let mut confirm = false;
        let mut cancel = false;
        egui::Window::new("Uninstall mod?")
            .open(&mut open)
            .collapsible(false)
            .show(context, |ui| {
                ui.label(format!(
                    "Permanently delete {filename} from '{}' ?",
                    target.name
                ));
                ui.label("This cannot be undone. Dependencies are not removed.");
                confirm = ui
                    .add_enabled(
                        self.mod_task.is_none() && !crate::minecraft::is_running(),
                        egui::Button::new("Uninstall permanently"),
                    )
                    .clicked();
                cancel = ui.button("Cancel").clicked();
            });
        if confirm {
            self.pending_uninstall = None;
            if self.target_exists(&target) {
                self.mod_target = Some(target);
                self.local_mod_task(Some((filename, None)));
            }
        } else if cancel || !open {
            self.pending_uninstall = None;
        }
    }

    /// Exposes all search facets; pagination always reuses the submitted filter set.
    pub(super) fn mod_browser_ui(&mut self, ui: &mut egui::Ui) {
        let idle = self.mod_task.is_none() && self.pending_uninstall.is_none();
        let mut search = false;
        ui.add_enabled_ui(idle, |ui| {
            let response = ui.text_edit_singleline(&mut self.mod_filters.query);
            search = response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            ui.collapsing("Search filters", |ui| {
                optional_text(
                    ui,
                    "Minecraft version (exact)",
                    &mut self.mod_filters.game_version,
                );
                optional_choice(
                    ui,
                    "Loader",
                    &mut self.mod_filters.loader,
                    &["fabric", "forge", "neoforge", "quilt"],
                );
                optional_choice(
                    ui,
                    "Client side",
                    &mut self.mod_filters.client_side,
                    &["required", "optional", "unsupported", "unknown"],
                );
                optional_choice(
                    ui,
                    "Server side",
                    &mut self.mod_filters.server_side,
                    &["required", "optional", "unsupported", "unknown"],
                );
                ui.label("Categories (comma-separated IDs; all must match)");
                ui.text_edit_singleline(&mut self.mod_categories);
                optional_choice(
                    ui,
                    "Project type",
                    &mut self.mod_filters.project_type,
                    &[
                        "mod",
                        "modpack",
                        "resourcepack",
                        "shader",
                        "plugin",
                        "datapack",
                    ],
                );
                egui::ComboBox::from_label("Sort")
                    .selected_text(&self.mod_filters.sort)
                    .show_ui(ui, |ui| {
                        for sort in ["relevance", "downloads", "follows", "newest", "updated"] {
                            ui.selectable_value(&mut self.mod_filters.sort, sort.into(), sort);
                        }
                    });
            });
            ui.horizontal(|ui| {
                search |= ui.button("Search").clicked();
                if ui.button("Popular (downloads)").clicked() {
                    self.mod_filters.query.clear();
                    self.mod_filters.sort = "downloads".into();
                    search = true;
                }
            });
        });
        if idle && search {
            self.mod_filters.categories = self
                .mod_categories
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
                .collect();
            self.mod_filters.offset = 0;
            self.search_modrinth(self.mod_filters.clone());
        }
        let idle = self.mod_task.is_none() && self.pending_uninstall.is_none();
        let can_install =
            self.can_install(self.result_filters.project_type.as_deref() == Some("mod"));
        // Result rows are borrowed for drawing; execute one chosen action after the loop.
        let mut page = None;
        let mut details = None;
        let mut install = None;
        if let Some(response) = &self.mod_results {
            ui.label(format!(
                "{} total · showing {}–{} · {} · {}",
                response.total_hits,
                if response.hits.is_empty() {
                    0
                } else {
                    response.offset + 1
                },
                response.offset + response.hits.len() as u64,
                self.result_filters
                    .project_type
                    .as_deref()
                    .unwrap_or("all project types"),
                self.result_filters.sort
            ));
            ui.horizontal(|ui| {
                if ui
                    .add_enabled(idle && response.offset > 0, egui::Button::new("Previous"))
                    .clicked()
                {
                    page = Some(response.offset.saturating_sub(response.limit));
                }
                if ui
                    .add_enabled(
                        idle && response.limit > 0
                            && response.offset.saturating_add(response.limit) < response.total_hits,
                        egui::Button::new("Next"),
                    )
                    .clicked()
                {
                    page = Some(response.offset.saturating_add(response.limit));
                }
            });
            for hit in &response.hits {
                ui.separator();
                if ui
                    .add_enabled(idle, egui::Button::new(RichText::new(&hit.title).strong()))
                    .clicked()
                {
                    details = Some(hit.project_id.clone());
                }
                self.icons.show_url(ui, hit.icon_url.as_deref(), 48.0);
                ui.label(format!(
                    "By {} · {} downloads",
                    hit.author.as_deref().unwrap_or("Unknown"),
                    format_downloads(hit.downloads)
                ));
                ui.label(&hit.description);
                ui.label(format!(
                    "Categories: {}",
                    hit.categories.as_deref().unwrap_or_default().join(", ")
                ));
                ui.collapsing(
                    format!("Supported Minecraft versions · {}", hit.title),
                    |ui| {
                        ui.label(hit.versions.as_deref().unwrap_or_default().join(", "));
                    },
                );
                if let Some(target) = &self.mod_target {
                    let supported = hit
                        .versions
                        .as_ref()
                        .map(|versions| versions.contains(&target.version));
                    ui.label(format!(
                        "Target Minecraft {}: {} (loader checked at install)",
                        target.version,
                        match supported {
                            Some(true) => "supported",
                            Some(false) => "not listed",
                            None => "unknown",
                        }
                    ));
                }
                if ui
                    .add_enabled(can_install, egui::Button::new("Install into target"))
                    .clicked()
                {
                    install = Some((hit.project_id.clone(), hit.title.clone()));
                }
            }
        }
        if let Some(offset) = page {
            let mut filters = self.result_filters.clone();
            filters.offset = offset;
            self.search_modrinth(filters);
        } else if let Some(id) = details {
            self.open_mod_details(
                id,
                self.result_filters.project_type.as_deref() == Some("mod"),
            );
        } else if let Some((id, title)) = install {
            self.install_mod(
                id,
                title,
                self.result_filters.project_type.as_deref() == Some("mod"),
            );
        }
        ui.label("Installation is available only for mod-only results and a mod-loader target.");
    }

    /// Installation eligibility is based on result provenance, not editable filters.
    pub(super) fn can_install(&self, is_mod: bool) -> bool {
        self.mod_task.is_none()
            && self.pending_uninstall.is_none()
            && self.mod_target.as_ref().is_some_and(|target| {
                crate::instance_mods::workflows::can_install(
                    &self.instances,
                    target,
                    is_mod,
                    crate::minecraft::is_running(),
                )
            })
    }

    /// Displays full metadata, team, releases and dependency links with a back action.
    pub(super) fn mod_details_ui(&mut self, ui: &mut egui::Ui) {
        if ui
            .add_enabled(
                self.mod_task.is_none(),
                egui::Button::new("← Back to results"),
            )
            .clicked()
        {
            self.mod_details = None;
            return;
        }
        let Some((is_mod, details)) = &self.mod_details else {
            return;
        };
        let project = &details.project;
        ui.heading(&project.title);
        self.icons.show_url(ui, project.icon_url.as_deref(), 96.0);
        ui.label(format!(
            "By {} · {} downloads",
            details.author().unwrap_or("Unknown"),
            format_downloads(project.downloads)
        ));
        ui.label(&project.description);
        ui.hyperlink_to(
            "Open on Modrinth",
            format!("https://modrinth.com/project/{}", project.slug),
        );
        if let Some(url) = &project.source_url {
            ui.hyperlink_to("Source", url);
        }
        if let Some(url) = &project.issues_url {
            ui.hyperlink_to("Issues", url);
        }
        ui.label(format!("Minecraft: {}", project.game_versions.join(", ")));
        ui.label(format!("Loaders: {}", project.loaders.join(", ")));
        ui.label(format!(
            "Client: {} · Server: {}",
            project.client_side, project.server_side
        ));
        ui.collapsing("Team", |ui| {
            for member in details.team_members.iter().filter(|m| m.accepted) {
                ui.hyperlink_to(
                    format!("{} · {}", member.user.username, member.role),
                    format!("https://modrinth.com/user/{}", member.user.username),
                );
            }
        });
        ui.collapsing("Description (Markdown source)", |ui| {
            ui.label(&project.body);
        });
        let install = ui
            .add_enabled(
                self.can_install(*is_mod),
                egui::Button::new("Install compatible release into target"),
            )
            .clicked();
        for version in &details.versions {
            ui.push_id(&version.id, |ui| {
                ui.collapsing(
                    format!(
                        "{} · {} · {}",
                        version.name, version.version_number, version.version_type
                    ),
                    |ui| {
                        ui.label(format!(
                            "{} · {} downloads",
                            version.date_published,
                            format_downloads(version.downloads)
                        ));
                        ui.label(format!(
                            "Minecraft: {} · Loaders: {}",
                            version.game_versions.join(", "),
                            version.loaders.join(", ")
                        ));
                        ui.hyperlink_to(
                            "Version page",
                            format!(
                                "https://modrinth.com/project/{}/version/{}",
                                version.project_id, version.id
                            ),
                        );
                        for dependency in &version.dependencies {
                            ui.label(format!(
                                "Dependency: {} · {}",
                                dependency.dependency_type,
                                dependency.file_name.as_deref().unwrap_or("")
                            ));
                            if let Some(id) = &dependency.project_id {
                                ui.hyperlink_to(id, format!("https://modrinth.com/project/{id}"));
                                if let Some(version) = &dependency.version_id {
                                    ui.hyperlink_to(
                                        version,
                                        format!(
                                            "https://modrinth.com/project/{id}/version/{version}"
                                        ),
                                    );
                                }
                            } else if let Some(id) = &dependency.version_id {
                                ui.label(format!("Version ID: {id}"));
                            }
                        }
                        if let Some(changelog) = &version.changelog {
                            ui.collapsing("Changelog", |ui| {
                                ui.label(changelog);
                            });
                        }
                    },
                );
            });
        }
        if install {
            // Clone before the mutable call to end the borrow of `self.mod_details`.
            let (id, title, is_mod) = (project.id.clone(), project.title.clone(), *is_mod);
            self.install_mod(id, title, is_mod);
        }
    }
}

/// Optional exact facet text; empty input omits the facet.
fn optional_text(ui: &mut egui::Ui, label: &str, value: &mut Option<String>) {
    ui.label(label);
    let mut text = value.clone().unwrap_or_default();
    if ui.text_edit_singleline(&mut text).changed() {
        *value = (!text.trim().is_empty()).then(|| text.trim().to_owned());
    }
}

/// Selects an API facet or leaves it unrestricted.
fn optional_choice(ui: &mut egui::Ui, label: &str, value: &mut Option<String>, choices: &[&str]) {
    egui::ComboBox::from_label(label)
        .selected_text(value.as_deref().unwrap_or("Any"))
        .show_ui(ui, |ui| {
            ui.selectable_value(value, None, "Any");
            for choice in choices {
                ui.selectable_value(value, Some((*choice).into()), *choice);
            }
        });
}

/// Formats large download counts compactly for result cards.
fn format_downloads(downloads: u64) -> String {
    if downloads >= 1_000_000 {
        format!("{:.1}M", downloads as f64 / 1_000_000.0)
    } else if downloads >= 1_000 {
        format!("{:.1}K", downloads as f64 / 1_000.0)
    } else {
        downloads.to_string()
    }
}
