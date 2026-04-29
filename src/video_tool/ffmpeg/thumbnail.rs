//! Single-frame thumbnail extraction. Has two modes:
//!
//! - Cover-art: `stream_index` is set; we extract frame 0 of that stream
//!   (used for embedded audio cover art). One-shot, no retry.
//! - Real video: seek to the video midpoint with fallbacks at 25%, 10%,
//!   and 1s if midpoint-decoding fails. Success is measured by
//!   "output file exists and is non-empty" rather than ffmpeg exit
//!   status, because ffmpeg can write a valid JPEG and still exit
//!   nonzero when the decode-error rate on frames we discarded is high.

use std::path::Path;
use std::process::Stdio;

use anyhow::{bail, Context, Result};

pub(super) async fn thumbnail(
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
