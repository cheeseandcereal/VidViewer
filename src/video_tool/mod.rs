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
use tokio_util::sync::CancellationToken;

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
    /// True when the file has at least one audio stream and zero video
    /// streams that aren't flagged `disposition.attached_pic` (i.e.
    /// embedded cover art). Drives preview-skip and UI affordances
    /// downstream.
    pub is_audio_only: bool,
    /// Zero-based index of a still-image stream inside the container,
    /// typically embedded cover art. `Some` when the file has an
    /// `attached_pic` stream that the thumbnail job can extract. None
    /// otherwise.
    pub attached_pic_stream_index: Option<i64>,
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
    ///
    /// The `cancel` token is polled between per-timestamp ffmpeg invocations so
    /// the loop stops spawning new ffmpegs promptly when a directory is
    /// removed while this job is in flight. It is separate from, and
    /// complementary to, tokio task abort (which interrupts whatever `.await`
    /// is currently active).
    async fn previews(
        &self,
        src: &Path,
        dst: &Path,
        plan: &PreviewPlan,
        duration_secs: f64,
        cancel: &CancellationToken,
    ) -> Result<()>;
}

pub type VideoToolRef = Arc<dyn VideoTool>;
