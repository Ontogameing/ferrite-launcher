//! UI-independent Ferrite launcher operations.
//!
//! Call blocking network/filesystem operations on your own workers. Progress
//! callbacks run synchronously on the calling thread. The caller owns loaded
//! configuration, profiles and worker scheduling. [`auth::Session`] owns accepted
//! credentials and authentication attempts; [`core::activity::WorkflowCoordinator`]
//! owns admission and owner-specific completion for one serialized launcher session.
//! Prepared operations own result acceptance, rollback and persistence. The existing
//! process-wide Minecraft child remains library-owned. See [`core`] for storage rules.
//!
//! The native egui binary is enabled by default. Library consumers can disable
//! default features to avoid compiling GUI dependencies. Credentials are
//! session-only Rust values and must not be serialized or logged.

pub mod auth;
pub mod config;
pub mod core;
pub mod discord;
pub mod instance_mods;
pub mod loaders;
pub mod minecraft;
pub mod modrinth;
pub mod packs;
pub mod updates;

use core::instances;
