//! `VideoTool` trait and implementations.
//!
//! The trait abstracts all ffmpeg / ffprobe work so tests can substitute a mock without
//! requiring the real binaries. Do NOT shell out directly from job handlers — always go
//! through the trait.
//!
//! Submodules:
//!   * [`ffmpeg`] — the real implementation (`FfmpegTool`) + command builders.
//!   * [`ffprobe`] — self-contained ffprobe JSON parser.
//!   * [`mock`] — `MockVideoTool` used in tests.

use std::path::Path;
use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;

pub mod ffmpeg;
mod ffprobe;
pub mod mock;
#[cfg(test)]
mod tests;

pub use ffmpeg::{ffmpeg, FfmpegTool};
pub use mock::{MockCall, MockVideoTool};

#[derive(Debug, Clone, PartialEq)]
pub struct ProbeResult {
    pub duration_secs: Option<f64>,
    pub width: Option<i64>,
    pub height: Option<i64>,
    pub codec: Option<String>,
}

/// A plan for a preview tile sheet.
#[derive(Debug, Clone, PartialEq)]
pub struct PreviewPlan {
    /// Number of preview frames to generate.
    pub count: u32,
    /// Seconds from start for each preview (center of its time-slice).
    pub timestamps: Vec<f64>,
    /// Tile grid dimensions.
    pub cols: u32,
    pub rows: u32,
    /// Tile dimensions (pixels).
    pub tile_width: u32,
    pub tile_height: u32,
}

#[async_trait]
pub trait VideoTool: Send + Sync {
    async fn probe(&self, path: &Path) -> Result<ProbeResult>;

    /// Generate a single poster thumbnail at the given path.
    async fn thumbnail(&self, src: &Path, dst: &Path, at_secs: f64, width: u32) -> Result<()>;

    /// Generate a tile-sheet JPEG at `dst` according to the plan.
    async fn previews(
        &self,
        src: &Path,
        dst: &Path,
        plan: &PreviewPlan,
        duration_secs: f64,
    ) -> Result<()>;
}

pub type VideoToolRef = Arc<dyn VideoTool>;
