//! Library crate for VidViewer.
//!
//! The binary in `src/main.rs` is a thin CLI wrapper around this crate. All meaningful
//! functionality is exposed here so integration tests can exercise it directly.

pub mod cli;
pub mod clock;
pub mod config;
pub mod db;
pub mod directories;
pub mod fs_browse;
pub mod http;
pub mod ids;
pub mod jobs;
pub mod logging;
pub mod player;
pub mod scanner;
pub mod state;
pub mod ui_state;
pub mod util;
pub mod video_tool;
pub mod videos;
