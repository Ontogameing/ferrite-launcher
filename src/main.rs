//! Native entry point for Ferrite Launcher.
//!
//! Application state and UI orchestration live in [`app`]; this module stays thin so
//! eframe startup errors can propagate through `main` without duplicating setup.

mod app;
mod auth;
mod background;
mod config;
mod discord;
mod icons;
mod instance_mods;
mod loaders;
mod minecraft;
mod modrinth;
mod packs;
mod updates;

// Core storage modules live in the `ferrite_launcher` library so they stay free of
// egui/eframe. Re-importing them here keeps `crate::instances::...` paths working.
use ferrite_launcher::core::instances;

/// Starts the native UI and returns any window/event-loop initialization error.
fn main() -> eframe::Result {
    println!("Starting GUI!");
    app::run()
}
