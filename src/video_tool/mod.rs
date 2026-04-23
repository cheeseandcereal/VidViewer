//! `VideoTool` trait and implementations.
//!
//! The trait abstracts all ffmpeg / ffprobe work so tests can substitute a mock without
//! requiring the real binaries. Do NOT shell out directly from job handlers — always go
//! through the trait.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, bail, Context, Result};
use async_trait::async_trait;
use serde::Deserialize;

use crate::config::Config;

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

/// Real ffmpeg / ffprobe implementation.
#[derive(Debug, Clone)]
pub struct FfmpegTool;

impl FfmpegTool {
    pub fn new() -> Self {
        Self
    }
}

impl Default for FfmpegTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl VideoTool for FfmpegTool {
    async fn probe(&self, path: &Path) -> Result<ProbeResult> {
        let out = tokio::process::Command::new("ffprobe")
            .arg("-v")
            .arg("error")
            .arg("-print_format")
            .arg("json")
            .arg("-show_format")
            .arg("-show_streams")
            .arg(path)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .await
            .with_context(|| format!("spawning ffprobe for {}", path.display()))?;
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            bail!(
                "ffprobe failed for {}: {} ({})",
                path.display(),
                out.status,
                stderr.trim()
            );
        }
        parse_ffprobe_json(&out.stdout)
    }

    async fn thumbnail(&self, src: &Path, dst: &Path, at_secs: f64, width: u32) -> Result<()> {
        if let Some(parent) = dst.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        let at = format!("{at_secs:.3}");
        let vf = format!("scale={width}:-2");
        let status = tokio::process::Command::new("ffmpeg")
            .arg("-y")
            .arg("-ss")
            .arg(&at)
            .arg("-i")
            .arg(src)
            .arg("-frames:v")
            .arg("1")
            .arg("-vf")
            .arg(&vf)
            .arg(dst)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await
            .with_context(|| format!("spawning ffmpeg for thumbnail of {}", src.display()))?;
        if !status.success() {
            bail!("ffmpeg failed to produce thumbnail for {}", src.display());
        }
        Ok(())
    }

    async fn previews(
        &self,
        src: &Path,
        dst: &Path,
        plan: &PreviewPlan,
        duration_secs: f64,
    ) -> Result<()> {
        if plan.count == 0 {
            bail!("preview plan has zero count");
        }
        let _ = duration_secs; // retained in trait signature for compatibility

        if let Some(parent) = dst.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .with_context(|| format!("creating {}", parent.display()))?;
        }

        // Scratch directory, unique per destination (dst stem is the video id).
        // Wiped on entry (in case of stale data from a prior aborted run) and on exit
        // via `ScratchDirGuard` on drop.
        let scratch_dir = preview_scratch_dir(dst);
        let _guard = ScratchDirGuard::new(scratch_dir.clone());
        if scratch_dir.exists() {
            let _ = tokio::fs::remove_dir_all(&scratch_dir).await;
        }
        tokio::fs::create_dir_all(&scratch_dir)
            .await
            .with_context(|| format!("creating scratch {}", scratch_dir.display()))?;

        // 1. Per-timestamp extraction, serial to keep memory bounded.
        //    Each ffmpeg opens `src` exactly once, input-seeks to near its target
        //    keyframe, decodes one frame, writes a tiny JPEG, exits. Memory per
        //    process is bounded by one decoder context.
        let mut partial_tiles: u32 = 0;
        let mut last_successful_scratch: Option<PathBuf> = None;
        for (i, &t) in plan.timestamps.iter().enumerate() {
            let scratch_path = scratch_tile_path(&scratch_dir, i);
            let args = build_single_frame_command(
                src,
                &scratch_path,
                t,
                plan.tile_width,
                plan.tile_height,
            );
            let ok = run_ffmpeg_silent(&args)
                .await
                .with_context(|| format!("spawning ffmpeg for preview frame {i}"))?;
            if ok {
                last_successful_scratch = Some(scratch_path);
                continue;
            }

            // Failure on this tile. Fall back to the most recent successful tile
            // so the final tile sheet still has something at this position.
            tracing::warn!(
                video = %src.display(),
                tile_index = i,
                at_secs = t,
                "preview frame extraction failed; substituting neighbor tile"
            );
            partial_tiles += 1;
            if let Some(src_tile) = last_successful_scratch.as_ref() {
                tokio::fs::copy(src_tile, &scratch_path)
                    .await
                    .with_context(|| {
                        format!("copying fallback tile to {}", scratch_path.display())
                    })?;
            } else {
                // No previous tile yet; try the next timestamp immediately, backfill if it
                // succeeds, else bail.
                if i + 1 >= plan.timestamps.len() {
                    bail!("preview extraction failed at tile 0 with no successor");
                }
                let next_t = plan.timestamps[i + 1];
                let args = build_single_frame_command(
                    src,
                    &scratch_path,
                    next_t,
                    plan.tile_width,
                    plan.tile_height,
                );
                let ok = run_ffmpeg_silent(&args)
                    .await
                    .with_context(|| "spawning ffmpeg for backfill preview frame")?;
                if !ok {
                    bail!("preview extraction failed at tile 0 and backfill also failed");
                }
                last_successful_scratch = Some(scratch_path);
            }
        }

        if partial_tiles > 0 {
            tracing::info!(
                video = %src.display(),
                partial_tiles,
                total = plan.count,
                "preview completed with fallback tiles"
            );
        }

        // 2. Tile pass. A single ffmpeg input reads `%0Nd.jpg` from the scratch dir
        //    and emits one tile sheet. N small JPEG inputs → tiny memory footprint.
        let tile_args =
            build_tile_from_scratch_command(&scratch_dir, dst, plan.cols, plan.rows, plan.count);
        let ok = run_ffmpeg_silent(&tile_args)
            .await
            .with_context(|| format!("spawning ffmpeg to tile preview sheet {}", dst.display()))?;
        if !ok {
            bail!("ffmpeg tile pass failed for {}", dst.display());
        }

        Ok(())
    }
}

/// Build the ffmpeg args to extract a single frame at timestamp `t`, scaled and
/// padded to tile dimensions, written to `dst` as a JPEG.
///
/// Uses input-side seek (`-ss` before `-i`) so ffmpeg jumps to the nearest keyframe
/// instead of decoding from the start. One input, one frame, tiny memory footprint.
fn build_single_frame_command(
    src: &Path,
    dst: &Path,
    t_secs: f64,
    tile_width: u32,
    tile_height: u32,
) -> Vec<std::ffi::OsString> {
    use std::ffi::OsString;
    let vf = format!(
        "scale={w}:{h}:force_original_aspect_ratio=decrease,\
         pad={w}:{h}:(ow-iw)/2:(oh-ih)/2:black",
        w = tile_width,
        h = tile_height,
    );
    let mut args: Vec<OsString> = Vec::with_capacity(12);
    args.push("-y".into());
    args.push("-ss".into());
    args.push(format!("{t_secs:.6}").into());
    args.push("-i".into());
    args.push(src.into());
    args.push("-frames:v".into());
    args.push("1".into());
    args.push("-vf".into());
    args.push(vf.into());
    args.push(dst.into());
    args
}

/// Build the ffmpeg args for the final tile pass: read `NNN.jpg` files from `scratch`
/// and assemble them into a single tile sheet at `dst`.
///
/// Uses the `image2` demuxer with `-start_number 0` and `%03d.jpg` pattern so the tile
/// filter sees the frames in plan order. One ffmpeg input, many small JPEGs → bounded
/// memory regardless of tile count.
fn build_tile_from_scratch_command(
    scratch: &Path,
    dst: &Path,
    cols: u32,
    rows: u32,
    count: u32,
) -> Vec<std::ffi::OsString> {
    use std::ffi::OsString;
    let pattern = scratch.join("%03d.jpg");
    let vf = format!("tile={cols}x{rows}");
    let args: Vec<OsString> = vec![
        "-y".into(),
        "-start_number".into(),
        "0".into(),
        "-framerate".into(),
        "1".into(), // arbitrary; we cap output frames below.
        "-i".into(),
        pattern.into(),
        "-frames:v".into(),
        "1".into(),
        "-vframes".into(),
        count.to_string().into(),
        "-vf".into(),
        vf.into(),
        dst.into(),
    ];
    args
}

fn scratch_tile_path(scratch_dir: &Path, index: usize) -> PathBuf {
    scratch_dir.join(format!("{index:03}.jpg"))
}

fn preview_scratch_dir(dst: &Path) -> PathBuf {
    // dst is e.g. <cache>/previews/<video_id>.jpg; scratch lives alongside as
    // <cache>/previews/scratch/<video_id>/.
    let parent = dst
        .parent()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    let stem = dst
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("preview");
    parent.join("scratch").join(stem)
}

/// Best-effort cleanup of a scratch directory on drop (success or failure).
struct ScratchDirGuard {
    path: PathBuf,
}

impl ScratchDirGuard {
    fn new(path: PathBuf) -> Self {
        Self { path }
    }
}

impl Drop for ScratchDirGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

async fn run_ffmpeg_silent(args: &[std::ffi::OsString]) -> Result<bool> {
    let status = tokio::process::Command::new("ffmpeg")
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .context("spawning ffmpeg")?;
    Ok(status.success())
}

pub fn ffmpeg(_cfg: &Config) -> VideoToolRef {
    Arc::new(FfmpegTool::new())
}

// --- ffprobe parsing ---

#[derive(Debug, Deserialize)]
struct FfprobeJson {
    format: Option<Format>,
    streams: Vec<Stream>,
}

#[derive(Debug, Deserialize)]
struct Format {
    duration: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Stream {
    codec_type: Option<String>,
    codec_name: Option<String>,
    width: Option<i64>,
    height: Option<i64>,
    duration: Option<String>,
}

fn parse_ffprobe_json(bytes: &[u8]) -> Result<ProbeResult> {
    let parsed: FfprobeJson =
        serde_json::from_slice(bytes).context("parsing ffprobe json output")?;
    let video_stream = parsed
        .streams
        .iter()
        .find(|s| s.codec_type.as_deref() == Some("video"));

    let duration = parsed
        .format
        .as_ref()
        .and_then(|f| f.duration.as_deref())
        .and_then(|s| s.parse::<f64>().ok())
        .or_else(|| {
            video_stream
                .and_then(|s| s.duration.as_deref())
                .and_then(|s| s.parse::<f64>().ok())
        });

    Ok(ProbeResult {
        duration_secs: duration,
        width: video_stream.and_then(|s| s.width),
        height: video_stream.and_then(|s| s.height),
        codec: video_stream.and_then(|s| s.codec_name.clone()),
    })
}

// --- Mock ---

/// An in-memory mock used in tests. Records invocations and returns preconfigured results.
#[derive(Debug, Default, Clone)]
pub struct MockVideoTool {
    inner: Arc<Mutex<MockState>>,
}

#[derive(Debug, Default)]
struct MockState {
    pub probe_results: std::collections::HashMap<PathBuf, ProbeResult>,
    pub calls: Vec<MockCall>,
}

#[derive(Debug, Clone)]
pub enum MockCall {
    Probe(PathBuf),
    Thumbnail {
        src: PathBuf,
        dst: PathBuf,
        at_secs: f64,
        width: u32,
    },
    Preview {
        src: PathBuf,
        dst: PathBuf,
        plan: PreviewPlan,
        duration_secs: f64,
    },
}

impl MockVideoTool {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_probe(&self, path: PathBuf, res: ProbeResult) {
        self.inner.lock().unwrap().probe_results.insert(path, res);
    }

    pub fn calls(&self) -> Vec<MockCall> {
        self.inner.lock().unwrap().calls.clone()
    }
}

#[async_trait]
impl VideoTool for MockVideoTool {
    async fn probe(&self, path: &Path) -> Result<ProbeResult> {
        let mut st = self.inner.lock().unwrap();
        st.calls.push(MockCall::Probe(path.to_path_buf()));
        st.probe_results
            .get(path)
            .cloned()
            .ok_or_else(|| anyhow!("no mock probe result for {}", path.display()))
    }

    async fn thumbnail(&self, src: &Path, dst: &Path, at_secs: f64, width: u32) -> Result<()> {
        let mut st = self.inner.lock().unwrap();
        st.calls.push(MockCall::Thumbnail {
            src: src.to_path_buf(),
            dst: dst.to_path_buf(),
            at_secs,
            width,
        });
        // Pretend to write the file so callers that stat it succeed.
        if let Some(parent) = dst.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::write(dst, b"fake");
        Ok(())
    }

    async fn previews(
        &self,
        src: &Path,
        dst: &Path,
        plan: &PreviewPlan,
        duration_secs: f64,
    ) -> Result<()> {
        let mut st = self.inner.lock().unwrap();
        st.calls.push(MockCall::Preview {
            src: src.to_path_buf(),
            dst: dst.to_path_buf(),
            plan: plan.clone(),
            duration_secs,
        });
        if let Some(parent) = dst.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::write(dst, b"fake");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ffprobe_duration_and_stream() {
        let sample = br#"{
            "format": {"duration": "123.456"},
            "streams": [
                {"codec_type":"audio","codec_name":"aac"},
                {"codec_type":"video","codec_name":"h264","width":1920,"height":1080}
            ]
        }"#;
        let r = parse_ffprobe_json(sample).unwrap();
        assert!((r.duration_secs.unwrap() - 123.456).abs() < 1e-6);
        assert_eq!(r.width, Some(1920));
        assert_eq!(r.height, Some(1080));
        assert_eq!(r.codec.as_deref(), Some("h264"));
    }

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
        // `-ss` must appear before `-i`.
        let ss_pos = args
            .iter()
            .position(|a| a.to_string_lossy() == "-ss")
            .expect("missing -ss");
        let i_pos = args
            .iter()
            .position(|a| a.to_string_lossy() == "-i")
            .expect("missing -i");
        assert!(ss_pos < i_pos, "expected -ss before -i");

        // Seek timestamp formatted to 6 decimals.
        let ss_val = args[ss_pos + 1].to_string_lossy().into_owned();
        assert_eq!(ss_val, "12.750000");

        // Exactly one input, exactly one output frame.
        assert_eq!(
            args.iter().filter(|a| a.to_string_lossy() == "-i").count(),
            1
        );
        let frames_pos = args
            .iter()
            .position(|a| a.to_string_lossy() == "-frames:v")
            .expect("missing -frames:v");
        assert_eq!(args[frames_pos + 1].to_string_lossy(), "1");

        // Scale+pad for tile dimensions.
        let vf_pos = args
            .iter()
            .position(|a| a.to_string_lossy() == "-vf")
            .expect("missing -vf");
        let vf = args[vf_pos + 1].to_string_lossy().into_owned();
        assert!(vf.contains("scale=160:90"), "vf: {vf}");
        assert!(vf.contains("pad=160:90"), "vf: {vf}");
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
        // Single input (`image2` pattern).
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

        // Tile filter with correct grid.
        let vf_pos = args
            .iter()
            .position(|a| a.to_string_lossy() == "-vf")
            .unwrap();
        assert_eq!(args[vf_pos + 1].to_string_lossy(), "tile=5x3");
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
