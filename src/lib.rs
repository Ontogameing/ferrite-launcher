//! Ferrite Launcher core library.
//!
//! The binary (`src/main.rs`) owns the egui frontend and the remaining launcher
//! subsystems; this library holds the parts that must stay UI-independent. See
//! [`core`] for the boundary rules.

pub mod core;
