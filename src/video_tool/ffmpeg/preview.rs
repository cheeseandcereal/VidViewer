//! Preview tile sheet generation.
//!
//! The shape is two passes: N serial single-frame ffmpeg invocations
//! that write JPEGs into a scratch directory, followed by one tile pass
//! that stitches them into the final sheet. Serialization keeps memory
//! bounded at a single decoder context at a time; tile-pass input is
//! the small JPEG sequence, not the source video.
//!
//! See `docs/design/06-thumbnails-and-previews.md` for the full story.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use tokio_util::sync::CancellationToken;

use super::{run_ffmpeg_silent, ScratchDirGuard};
use crate::video_tool::PreviewPlan;

pub(super) async fn previews(
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
        let args =
            build_single_frame_command(src, &scratch_path, t, plan.tile_width, plan.tile_height);
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
                .with_context(|| format!("copying fallback tile to {}", scratch_path.display()))?;
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

/// Build the ffmpeg args to extract a single frame at timestamp `t`, scaled and
/// padded to tile dimensions, written to `dst` as a JPEG.
pub(crate) fn build_single_frame_command(
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
pub(crate) fn build_tile_from_scratch_command(
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

pub(crate) fn scratch_tile_path(scratch_dir: &Path, index: usize) -> PathBuf {
    scratch_dir.join(format!("{index:03}.jpg"))
}

pub(crate) fn preview_scratch_dir(dst: &Path) -> PathBuf {
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
