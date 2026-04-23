//! Library crate for VidViewer.
//!
//! The binary in `src/main.rs` is a thin CLI wrapper around this crate. All meaningful
//! functionality is exposed here so integration tests can exercise it directly.

pub mod cli;
pub mod clock;
pub mod config;
pub mod db;
pub mod http;
pub mod ids;
pub mod logging;
pub mod state;
pub mod util;
