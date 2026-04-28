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
    ///
    /// When `stream_index` is `Some(i)`, extract frame 0 of the i-th
    /// stream (used to pull embedded cover art out of audio files via
    /// `-map 0:i`). When `None`, seek to `at_secs` in the file as normal.
    async fn thumbnail(
        &self,
        src: &Path,
        dst: &Path,
        at_secs: f64,
        width: u32,
        stream_index: Option<i64>,
    ) -> Result<()>;

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

#[cfg(test)]
mod tests {
    //! Unit tests for the video_tool module: mock behavior and ffmpeg command builders.

    use std::path::{Path, PathBuf};

    use crate::video_tool::{
        ffmpeg::{
            build_single_frame_command, build_tile_from_scratch_command, preview_scratch_dir,
            scratch_tile_path,
        },
        MockVideoTool, ProbeResult, VideoTool,
    };

    #[tokio::test]
    async fn mock_records_calls() {
        let m = MockVideoTool::new();
        m.set_probe(
            PathBuf::from("/x.mp4"),
            ProbeResult {
                duration_secs: Some(10.0),
                width: Some(640),
                height: Some(360),
                codec: Some("h264".into()),
                is_audio_only: false,
                attached_pic_stream_index: None,
            },
        );
        let r = m.probe(Path::new("/x.mp4")).await.unwrap();
        assert_eq!(r.duration_secs, Some(10.0));
        assert_eq!(m.calls().len(), 1);
    }

    #[test]
    fn single_frame_command_uses_input_side_seek() {
        let args = build_single_frame_command(
            Path::new("/tmp/src.mp4"),
            Path::new("/tmp/scratch/003.jpg"),
            12.75,
            160,
            90,
        );
        let ss_pos = args
            .iter()
            .position(|a| a.to_string_lossy() == "-ss")
            .expect("missing -ss");
        let i_pos = args
            .iter()
            .position(|a| a.to_string_lossy() == "-i")
            .expect("missing -i");
        assert!(ss_pos < i_pos, "expected -ss before -i");

        let ss_val = args[ss_pos + 1].to_string_lossy().into_owned();
        assert_eq!(ss_val, "12.750000");

        assert_eq!(
            args.iter().filter(|a| a.to_string_lossy() == "-i").count(),
            1
        );
        let frames_pos = args
            .iter()
            .position(|a| a.to_string_lossy() == "-frames:v")
            .expect("missing -frames:v");
        assert_eq!(args[frames_pos + 1].to_string_lossy(), "1");

        let vf_pos = args
            .iter()
            .position(|a| a.to_string_lossy() == "-vf")
            .expect("missing -vf");
        let vf = args[vf_pos + 1].to_string_lossy().into_owned();
        assert!(vf.contains("scale=160:90"), "vf: {vf}");
        assert!(vf.contains("pad=160:90"), "vf: {vf}");

        // `-update 1` must be present so newer ffmpeg (Lavf 62+) accepts a
        // single-image output file name instead of demanding an image-sequence
        // pattern. It must sit AFTER -i (it's an output option, not a global
        // one; newer ffmpeg rejects it with exit status 8 if placed globally).
        let update_pos = args
            .iter()
            .position(|a| a.to_string_lossy() == "-update")
            .expect("missing -update");
        assert_eq!(args[update_pos + 1].to_string_lossy(), "1");
        assert!(
        update_pos > i_pos,
        "-update must come after -i (it is an output option), got update_pos={update_pos} i_pos={i_pos}"
    );
    }

    #[test]
    fn tile_from_scratch_command_reads_numbered_pattern() {
        let args = build_tile_from_scratch_command(
            Path::new("/tmp/scratch/abc"),
            Path::new("/tmp/sheet.jpg"),
            5,
            3,
            15,
        );
        assert_eq!(
            args.iter().filter(|a| a.to_string_lossy() == "-i").count(),
            1
        );
        let i_pos = args
            .iter()
            .position(|a| a.to_string_lossy() == "-i")
            .unwrap();
        let pattern = args[i_pos + 1].to_string_lossy().into_owned();
        assert!(pattern.ends_with("%03d.jpg"), "pattern: {pattern}");

        let vf_pos = args
            .iter()
            .position(|a| a.to_string_lossy() == "-vf")
            .unwrap();
        assert_eq!(args[vf_pos + 1].to_string_lossy(), "tile=5x3");

        let update_pos = args
            .iter()
            .position(|a| a.to_string_lossy() == "-update")
            .expect("missing -update");
        assert_eq!(args[update_pos + 1].to_string_lossy(), "1");
        assert!(
            update_pos > i_pos,
            "-update must come after -i (it is an output option)"
        );
    }

    #[test]
    fn scratch_tile_path_is_zero_padded() {
        let p = scratch_tile_path(Path::new("/x"), 7);
        assert_eq!(p, PathBuf::from("/x/007.jpg"));
    }

    #[test]
    fn preview_scratch_dir_sits_next_to_dst() {
        let d = preview_scratch_dir(Path::new("/cache/previews/abc-123.jpg"));
        assert_eq!(d, PathBuf::from("/cache/previews/scratch/abc-123"));
    }
}
