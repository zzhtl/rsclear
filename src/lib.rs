//! Filesystem operations are separated from the desktop UI for independent testing.
pub mod cache;
pub mod cargo;
pub mod index;
pub mod model;
pub mod operations;
pub mod platform;
pub mod scan;
pub mod settings;
pub mod shortcut;
pub mod system;
#[cfg(feature = "gui")]
pub mod ui;
