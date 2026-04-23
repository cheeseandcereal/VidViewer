//! Library crate for VidViewer.
//!
//! The binary in `src/main.rs` is a thin CLI wrapper around this crate. All meaningful
//! functionality is exposed here so integration tests can exercise it directly.

pub mod cli;
pub mod config;
pub mod http;
pub mod logging;
