//! Real `FfmpegTool` implementation of [`VideoTool`]. Shells out to `ffmpeg` and
//! `ffprobe` on the host.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use crate::config::Config;
use crate::video_tool::{
    ffprobe::parse_ffprobe_json, PreviewPlan, ProbeResult, VideoTool, VideoToolRef,
};

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
        // 60 seconds is *far* more than ffprobe needs for any real video —
        // typical runs are well under a second, even for huge files. A hung
        // ffprobe would otherwise leave the job row in `running` forever.
        const PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

        let fut = tokio::process::Command::new("ffprobe")
            .arg("-v")
            .arg("error")
            .arg("-print_format")
            .arg("json")
            .arg("-show_format")
            .arg("-show_streams")
            .arg(path)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .output();

        let out = match tokio::time::timeout(PROBE_TIMEOUT, fut).await {
            Ok(res) => res.with_context(|| format!("spawning ffprobe for {}", path.display()))?,
            Err(_) => bail!(
                "ffprobe timed out after {}s for {}",
                PROBE_TIMEOUT.as_secs(),
                path.display()
            ),
        };
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

    async fn thumbnail(
        &self,
        src: &Path,
        dst: &Path,
        at_secs: f64,
        width: u32,
        stream_index: Option<i64>,
    ) -> Result<()> {
        if let Some(parent) = dst.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .with_context(|| format!("creating {}", parent.display()))?;
        }

        // Cover-art mode is a one-shot: the attached_pic stream has exactly
        // one frame and there's no timestamp to retry.
        if let Some(idx) = stream_index {
            return run_thumbnail_attempt(src, dst, None, width, Some(idx)).await;
        }

        // Real-video mode. Try the video midpoint first, then fall back to
        // timestamps earlier in the file if the midpoint lands in a
        // corrupted region. Some source files have garbage NAL units
        // scattered through the middle that drive ffmpeg's decode-error
        // rate over the threshold; an earlier keyframe usually decodes
        // cleanly. Each attempt uses input-side seek for speed.
        //
        // We check file presence + non-empty size rather than just exit
        // status because ffmpeg can write a valid JPEG and still exit
        // nonzero when the decode-error rate is high on frames we
        // discarded anyway.
        let fallbacks = [at_secs, at_secs * 0.25, at_secs * 0.10, 1.0];
        let mut last_err: Option<anyhow::Error> = None;
        for (i, &t) in fallbacks.iter().enumerate() {
            let _ = tokio::fs::remove_file(dst).await;
            let attempt = run_thumbnail_attempt(src, dst, Some(t), width, None).await;
            // A good outcome is "the output file exists and is non-empty",
            // regardless of what the ffmpeg exit status claimed.
            let good = match tokio::fs::metadata(dst).await {
                Ok(md) => md.is_file() && md.len() > 0,
                Err(_) => false,
            };
            if good {
                if i > 0 {
                    tracing::info!(
                        src = %src.display(),
                        at_secs = t,
                        attempt = i,
                        "thumbnail succeeded on fallback timestamp"
                    );
                }
                return Ok(());
            }
            last_err = Some(attempt.err().unwrap_or_else(|| {
                anyhow::anyhow!("ffmpeg exited with success but no thumbnail file was written")
            }));
        }
        Err(last_err.unwrap_or_else(|| {
            anyhow::anyhow!("ffmpeg failed to produce thumbnail for {}", src.display())
        }))
        .with_context(|| format!("thumbnail for {}", src.display()))
    }

    async fn previews(
        &self,
        src: &Path,
        dst: &Path,
        plan: &PreviewPlan,
        duration_secs: f64,
        cancel: &CancellationToken,
    ) -> Result<()> {
        if plan.count == 0 {
            bail!("preview plan has zero count");
        }
        let _ = duration_secs; // retained in trait signature for compatibility

        if cancel.is_cancelled() {
            bail!("preview cancelled before start");
        }

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
        //    Check the cancellation token at the top of each iteration so we
        //    don't spawn more ffmpegs after the job has been cancelled.
        let mut partial_tiles: u32 = 0;
        let mut last_successful_scratch: Option<PathBuf> = None;
        for (i, &t) in plan.timestamps.iter().enumerate() {
            if cancel.is_cancelled() {
                bail!("preview cancelled after {} tiles", i);
            }
            let scratch_path = scratch_tile_path(&scratch_dir, i);
            let args = build_single_frame_command(
                src,
                &scratch_path,
                t,
                plan.tile_width,
                plan.tile_height,
            );
            let ok = run_ffmpeg_silent(&args, cancel)
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
                if cancel.is_cancelled() {
                    bail!("preview cancelled before backfill");
                }
                let next_t = plan.timestamps[i + 1];
                let args = build_single_frame_command(
                    src,
                    &scratch_path,
                    next_t,
                    plan.tile_width,
                    plan.tile_height,
                );
                let ok = run_ffmpeg_silent(&args, cancel)
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

        if cancel.is_cancelled() {
            bail!("preview cancelled before tile pass");
        }

        // 2. Tile pass.
        let tile_args =
            build_tile_from_scratch_command(&scratch_dir, dst, plan.cols, plan.rows, plan.count);
        let ok = run_ffmpeg_silent(&tile_args, cancel)
            .await
            .with_context(|| format!("spawning ffmpeg to tile preview sheet {}", dst.display()))?;
        if !ok {
            bail!("ffmpeg tile pass failed for {}", dst.display());
        }

        Ok(())
    }
}

pub fn ffmpeg(_cfg: &Config) -> VideoToolRef {
    Arc::new(FfmpegTool::new())
}

/// Run a single thumbnail-extraction ffmpeg invocation. Returns Ok on clean
/// exit; Err with the ffmpeg status otherwise. Caller is responsible for
/// checking whether a usable output file was actually written — ffmpeg can
/// exit nonzero (due to decode errors on frames we didn't want) while still
/// producing a valid JPEG.
async fn run_thumbnail_attempt(
    src: &Path,
    dst: &Path,
    at_secs: Option<f64>,
    width: u32,
    stream_index: Option<i64>,
) -> Result<()> {
    let vf = format!("scale={width}:-2");
    let mut cmd = tokio::process::Command::new("ffmpeg");
    cmd.arg("-y");
    if let Some(idx) = stream_index {
        // Cover-art mode: pull frame 0 of the specified stream index. This
        // is how we extract embedded album art from audio files — ffprobe
        // reports such streams with disposition.attached_pic = 1, and
        // `-map 0:<N>` + `-frames:v 1` extracts exactly that image.
        cmd.arg("-i").arg(src);
        cmd.arg("-map").arg(format!("0:{idx}"));
    } else if let Some(t) = at_secs {
        // Normal mode: input-side seek to the requested timestamp.
        cmd.arg("-ss").arg(format!("{t:.3}"));
        cmd.arg("-i").arg(src);
    } else {
        cmd.arg("-i").arg(src);
    }
    cmd.arg("-frames:v").arg("1");
    cmd.arg("-vf").arg(&vf);
    // `-update 1` must sit between the input/filter block and the output
    // path — it's an *output* option. Placing it at the top alongside `-y`
    // gets ffmpeg to reject it with "Option not found" (exit status 8) on
    // newer builds. Required by Lavf 62+ to accept single-image output
    // without an image-sequence filename pattern.
    cmd.arg("-update").arg("1");
    cmd.arg(dst);
    let status = cmd
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .status()
        .await
        .with_context(|| format!("spawning ffmpeg for thumbnail of {}", src.display()))?;
    if !status.success() {
        bail!(
            "ffmpeg exited with {status} for thumbnail of {}",
            src.display()
        );
    }
    Ok(())
}

/// Build the ffmpeg args to extract a single frame at timestamp `t`, scaled and
/// padded to tile dimensions, written to `dst` as a JPEG.
pub(super) fn build_single_frame_command(
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
    let mut args: Vec<OsString> = Vec::with_capacity(14);
    args.push("-y".into());
    args.push("-ss".into());
    args.push(format!("{t_secs:.6}").into());
    args.push("-i".into());
    args.push(src.into());
    args.push("-frames:v".into());
    args.push("1".into());
    args.push("-vf".into());
    args.push(vf.into());
    // `-update 1` is an *output* option: it must come after the input and
    // filter block, immediately before the output path. Required by newer
    // ffmpeg (Lavf 62+) to accept single-image output.
    args.push("-update".into());
    args.push("1".into());
    args.push(dst.into());
    args
}

/// Build the ffmpeg args for the final tile pass.
pub(super) fn build_tile_from_scratch_command(
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
        // `-update 1` is an output option: newer ffmpeg rejects it if
        // placed with global/input flags. Keeps single-image output
        // accepted without an image-sequence filename pattern.
        "-update".into(),
        "1".into(),
        dst.into(),
    ];
    args
}

pub(super) fn scratch_tile_path(scratch_dir: &Path, index: usize) -> PathBuf {
    scratch_dir.join(format!("{index:03}.jpg"))
}

pub(super) fn preview_scratch_dir(dst: &Path) -> PathBuf {
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

async fn run_ffmpeg_silent(
    args: &[std::ffi::OsString],
    cancel: &CancellationToken,
) -> Result<bool> {
    // Spawn the child, then race its exit against the cancel token. If the
    // token fires first, we drop the Child — combined with `kill_on_drop(true)`
    // that sends SIGKILL and reaps the process.
    let mut child = tokio::process::Command::new("ffmpeg")
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .context("spawning ffmpeg")?;

    tokio::select! {
        exit = child.wait() => {
            let status = exit.context("awaiting ffmpeg")?;
            Ok(status.success())
        }
        _ = cancel.cancelled() => {
            // Dropping `child` here triggers kill_on_drop.
            drop(child);
            bail!("ffmpeg cancelled");
        }
    }
}
