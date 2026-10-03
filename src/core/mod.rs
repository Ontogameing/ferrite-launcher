//! Ferrite's UI-independent core: storage locations, the versioned instance model,
//! instance storage, and the storage migration.
//!
//! Nothing under `core` may depend on egui/eframe or any other frontend crate; the
//! frontend calls into this module and renders its plain data types.

pub mod activity;
pub mod copy;
pub mod duplicate;
pub mod edit;
pub mod fsutil;
pub mod instances;
pub mod manifest;
pub mod migration;
pub mod paths;
pub mod remove;
pub mod scan;

pub use fsutil::write_atomic;
