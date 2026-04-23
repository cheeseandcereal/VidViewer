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
        let _ = duration_secs; // retained in trait signature for compatibility; no longer needed here
        if let Some(parent) = dst.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .with_context(|| format!("creating {}", parent.display()))?;
        }

        let args = build_preview_command(src, dst, plan);

        let status = tokio::process::Command::new("ffmpeg")
            .args(&args)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await
            .with_context(|| format!("spawning ffmpeg for preview of {}", src.display()))?;
        if !status.success() {
            bail!("ffmpeg failed to produce preview for {}", src.display());
        }
        Ok(())
    }
}

/// Build the ffmpeg argument list for generating a preview tile sheet via
/// per-timestamp input-seeked decodes and `xstack` tiling.
///
/// The produced command:
///   * opens `src` once per preview timestamp with its own `-ss <t>`, so each decode
///     only processes the bitstream near the target keyframe (fast, bounded by GOP size),
///   * selects exactly one frame per input stream with `trim=end_frame=1` + normalized PTS,
///   * scales and pads each tile to the configured dimensions,
///   * tiles all inputs into a single output image using `xstack` with an explicit
///     `layout=` (which works on every ffmpeg 4+; `grid=` is 6.0+).
///
/// Returns the vector of `OsString` args to pass to `ffmpeg`.
fn build_preview_command(src: &Path, dst: &Path, plan: &PreviewPlan) -> Vec<std::ffi::OsString> {
    use std::ffi::OsString;

    let mut args: Vec<OsString> = Vec::with_capacity(6 + 4 * plan.count as usize);
    args.push("-y".into());

    // Per-timestamp inputs, each with its own `-ss` before `-i`.
    for &t in &plan.timestamps {
        args.push("-ss".into());
        args.push(format!("{t:.6}").into());
        args.push("-i".into());
        args.push(src.into());
    }

    // Build filter_complex: scale+pad each input to a single tile, then xstack.
    let mut filter = String::new();
    for i in 0..plan.count {
        if !filter.is_empty() {
            filter.push(';');
        }
        let _ = std::fmt::Write::write_fmt(
            &mut filter,
            format_args!(
                "[{i}:v]trim=end_frame=1,setpts=PTS-STARTPTS,\
                 scale={w}:{h}:force_original_aspect_ratio=decrease,\
                 pad={w}:{h}:(ow-iw)/2:(oh-ih)/2:black[v{i}]",
                i = i,
                w = plan.tile_width,
                h = plan.tile_height,
            ),
        );
    }

    // Input list for xstack.
    filter.push(';');
    for i in 0..plan.count {
        let _ = std::fmt::Write::write_fmt(&mut filter, format_args!("[v{i}]"));
    }
    // Explicit layout string: col*tile_width _ row*tile_height, separated by '|'.
    let mut layout = String::new();
    for i in 0..plan.count {
        if !layout.is_empty() {
            layout.push('|');
        }
        let col = i % plan.cols;
        let row = i / plan.cols;
        let x = col * plan.tile_width;
        let y = row * plan.tile_height;
        let _ = std::fmt::Write::write_fmt(&mut layout, format_args!("{x}_{y}"));
    }
    let _ = std::fmt::Write::write_fmt(
        &mut filter,
        format_args!(
            "xstack=inputs={n}:layout={layout}:fill=black[out]",
            n = plan.count,
            layout = layout,
        ),
    );

    args.push("-filter_complex".into());
    args.push(filter.into());
    args.push("-map".into());
    args.push("[out]".into());
    args.push("-frames:v".into());
    args.push("1".into());
    args.push(dst.into());
    args
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
    fn preview_command_has_one_input_per_timestamp_and_xstack() {
        let plan = PreviewPlan {
            count: 4,
            timestamps: vec![1.0, 3.0, 5.0, 7.0],
            cols: 2,
            rows: 2,
            tile_width: 160,
            tile_height: 90,
        };
        let args = build_preview_command(
            Path::new("/tmp/src.mp4"),
            Path::new("/tmp/sheet.jpg"),
            &plan,
        );

        // Should have one `-ss <t> -i <src>` group per timestamp.
        let ss_count = args.iter().filter(|a| a.to_string_lossy() == "-ss").count();
        let input_count = args.iter().filter(|a| a.to_string_lossy() == "-i").count();
        assert_eq!(ss_count, 4);
        assert_eq!(input_count, 4);

        // filter_complex must reference xstack and an explicit layout.
        let filter_idx = args
            .iter()
            .position(|a| a.to_string_lossy() == "-filter_complex")
            .expect("missing -filter_complex");
        let filter = args[filter_idx + 1].to_string_lossy().into_owned();
        assert!(filter.contains("xstack=inputs=4"), "filter: {filter}");
        assert!(
            filter.contains("layout=0_0|160_0|0_90|160_90"),
            "expected 2x2 layout, got: {filter}"
        );
        assert!(filter.contains("trim=end_frame=1"));
        assert!(filter.contains("pad=160:90"));

        // Output args.
        assert!(args.iter().any(|a| a.to_string_lossy() == "-map"));
        assert!(args.iter().any(|a| a.to_string_lossy() == "[out]"));
        assert!(args.iter().any(|a| a.to_string_lossy() == "-frames:v"));
    }
}
