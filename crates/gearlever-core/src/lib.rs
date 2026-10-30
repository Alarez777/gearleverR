//! Core, UI-independent logic for Gear Lever R.
//!
//! This crate knows how to inspect AppImage files and, in later phases, how to
//! integrate them into the desktop and check for updates. It deliberately has no
//! GTK dependency so the CLI and the test suite can run without a display.

pub mod appimage;
pub mod args;
pub mod cache;
pub mod config;
pub mod desktop;
pub mod error;
pub mod hash;
pub mod install;
pub mod paths;
pub mod scan;
pub mod uninstall;
pub mod update;

pub use error::{Error, Result};
