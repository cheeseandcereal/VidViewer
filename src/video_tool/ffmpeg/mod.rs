//! Real `FfmpegTool` implementation of [`VideoTool`]. Shells out to `ffmpeg`
//! and `ffprobe` on the host.
//!
//! Split across per-operation submodules:
//! - [`probe`] — ffprobe JSON invocation with a wall-clock timeout.
//! - [`thumbnail`] — single-frame extraction with fallback timestamps and
//!   cover-art mode.
//! - [`preview`] — tile-sheet generation (N serial single-frame ffmpegs +
//!   one tile pass) plus the command builders reused there.
//!
//! Shared infrastructure (the `VideoTool` impl dispatch, the `FfmpegTool`
//! struct, the `run_ffmpeg_silent` helper, and the `ScratchDirGuard`
//! RAII cleaner) lives in this file.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use crate::config::Config;
use crate::video_tool::{PreviewPlan, ProbeResult, VideoTool, VideoToolRef};

mod preview;
mod probe;
mod thumbnail;

// Re-export the command-builder helpers and path helpers at the module
// surface so existing inline tests in `src/video_tool/mod.rs::tests` that
// reference them as `ffmpeg::build_single_frame_command` etc. keep working.
// `#[allow(unused_imports)]` because rustc can't see uses in the parent
// module's `#[cfg(test)] mod tests` block when type-checking this file
// standalone.
#[allow(unused_imports)]
pub(super) use preview::{
    build_single_frame_command, build_tile_from_scratch_command, preview_scratch_dir,
    scratch_tile_path,
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
        probe::probe(path).await
    }

    async fn thumbnail(
        &self,
        src: &Path,
        dst: &Path,
        at_secs: f64,
        width: u32,
        stream_index: Option<i64>,
    ) -> Result<()> {
        thumbnail::thumbnail(src, dst, at_secs, width, stream_index).await
    }

    async fn previews(
        &self,
        src: &Path,
        dst: &Path,
        plan: &PreviewPlan,
        duration_secs: f64,
        cancel: &CancellationToken,
    ) -> Result<()> {
        preview::previews(src, dst, plan, duration_secs, cancel).await
    }
}

pub fn ffmpeg(_cfg: &Config) -> VideoToolRef {
    Arc::new(FfmpegTool::new())
}

/// Best-effort cleanup of a scratch directory on drop (success or failure).
pub(super) struct ScratchDirGuard {
    path: PathBuf,
}

impl ScratchDirGuard {
    pub(super) fn new(path: PathBuf) -> Self {
        Self { path }
    }
}

impl Drop for ScratchDirGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// Run an ffmpeg command silently, racing child exit against a cancel
/// token. Returns `Ok(true)` if the child exited successfully, `Ok(false)`
/// if it exited with a nonzero status, `Err(...)` on spawn failure or
/// cancellation.
///
/// Dropping the `Child` via `kill_on_drop(true)` SIGKILLs the process
/// when cancellation fires.
pub(super) async fn run_ffmpeg_silent(
    args: &[std::ffi::OsString],
    cancel: &CancellationToken,
) -> Result<bool> {
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
