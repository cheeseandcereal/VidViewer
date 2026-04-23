//! Scanner: walks configured directories, diffs against the DB, and enqueues work.
//!
//! See `docs/design/04-scanner.md` for the algorithm. The scanner is designed to be
//! cheap on no-op runs so it can run at startup safely.
//!
//! This module is split into focused sibling files:
//!   * [`walk`] orchestrates the per-directory stat walk + diff.
//!   * [`mutations`] holds the small-transaction DB helpers (insert/update/mark missing).
//!   * [`verify`] owns the post-walk cache verification pass.
//!   * [`dry_run`] produces a read-only report of what a scan would do.

use std::{
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};

use anyhow::{Context, Result};
use serde::Serialize;
use sqlx::SqlitePool;
use tokio::task::JoinHandle;

use crate::{
    clock::ClockRef,
    directories,
    ids::{DirectoryId, VideoId},
};

pub mod dry_run;
mod mutations;
#[cfg(test)]
mod tests;
mod verify;
mod walk;

pub use dry_run::{dry_run_report, DryRunReport};

/// Allowed video file extensions (lowercase, no dot).
pub const VIDEO_EXTENSIONS: &[&str] = &[
    "mp4", "mkv", "webm", "mov", "avi", "m4v", "flv", "wmv", "mpg", "mpeg", "ts", "m2ts",
];

/// Filesystem paths to the derived-asset cache. Passed to the scanner so it can verify
/// that flagged-as-done outputs actually exist on disk; if a file is missing (cache was
/// cleared, manual delete, etc.) the scanner clears the flag and re-enqueues the job.
#[derive(Debug, Clone)]
pub struct CachePaths {
    pub thumb: PathBuf,
    pub preview: PathBuf,
}

impl CachePaths {
    pub fn from_config(cfg: &crate::config::Config) -> Self {
        Self {
            thumb: cfg.thumb_cache_dir(),
            preview: cfg.preview_cache_dir(),
        }
    }

    pub fn thumb_path(&self, video_id: &VideoId) -> PathBuf {
        self.thumb.join(format!("{}.jpg", video_id.as_str()))
    }

    pub fn preview_sheet_path(&self, video_id: &VideoId) -> PathBuf {
        self.preview.join(format!("{}.jpg", video_id.as_str()))
    }

    pub fn preview_vtt_path(&self, video_id: &VideoId) -> PathBuf {
        self.preview.join(format!("{}.vtt", video_id.as_str()))
    }
}

/// A single scan pass plan (either real or dry-run).
#[derive(Debug, Default, Clone, Serialize)]
pub struct ScanReport {
    pub directories_scanned: u32,
    pub files_seen: u64,
    pub new_videos: u64,
    pub changed_videos: u64,
    pub missing_videos: u64,
    /// Videos whose thumbnail file was missing on disk; the flag was cleared and a
    /// thumbnail job was re-enqueued.
    pub recovered_thumbnail_jobs: u64,
    /// Videos whose preview tile sheet or VTT file was missing on disk; the flag was
    /// cleared and a preview job was re-enqueued.
    pub recovered_preview_jobs: u64,
    pub errors: Vec<String>,
}

#[derive(Debug, Default)]
pub struct ScanProgress {
    pub phase: std::sync::atomic::AtomicU8,
    pub files_seen: AtomicU64,
    pub new_videos: AtomicU64,
    pub changed_videos: AtomicU64,
    pub missing_videos: AtomicU64,
    pub recovered_thumbnail_jobs: AtomicU64,
    pub recovered_preview_jobs: AtomicU64,
    pub error: std::sync::Mutex<Option<String>>,
}

#[repr(u8)]
#[derive(Clone, Copy)]
pub enum Phase {
    Walking = 0,
    Done = 1,
    Failed = 2,
}

pub struct ScanHandle {
    pub progress: std::sync::Arc<ScanProgress>,
    pub handle: JoinHandle<Result<ScanReport>>,
}

/// Kick off a full scan of all non-removed directories in the background.
pub fn spawn_all(pool: SqlitePool, clock: ClockRef, cache: CachePaths) -> ScanHandle {
    spawn_inner(pool, clock, cache, None)
}

pub fn spawn_one(
    pool: SqlitePool,
    clock: ClockRef,
    cache: CachePaths,
    dir_id: DirectoryId,
) -> ScanHandle {
    spawn_inner(pool, clock, cache, Some(dir_id))
}

fn spawn_inner(
    pool: SqlitePool,
    clock: ClockRef,
    cache: CachePaths,
    only: Option<DirectoryId>,
) -> ScanHandle {
    let progress = std::sync::Arc::new(ScanProgress::default());
    let p2 = progress.clone();
    let handle: JoinHandle<Result<ScanReport>> = tokio::spawn(async move {
        let res = scan(&pool, &clock, &cache, only, &p2).await;
        match &res {
            Ok(report) => {
                p2.phase.store(Phase::Done as u8, Ordering::SeqCst);
                tracing::info!(
                    dirs = report.directories_scanned,
                    files = report.files_seen,
                    new = report.new_videos,
                    changed = report.changed_videos,
                    missing = report.missing_videos,
                    recovered_thumb = report.recovered_thumbnail_jobs,
                    recovered_preview = report.recovered_preview_jobs,
                    "scan complete"
                );
            }
            Err(err) => {
                p2.phase.store(Phase::Failed as u8, Ordering::SeqCst);
                let msg = format!("{err:#}");
                tracing::error!(error = %msg, "scan failed");
                let mut e = p2.error.lock().unwrap();
                *e = Some(msg);
            }
        }
        res
    });
    ScanHandle { progress, handle }
}

/// Perform a full scan synchronously, updating `progress` as it goes.
pub async fn scan(
    pool: &SqlitePool,
    clock: &ClockRef,
    cache: &CachePaths,
    only: Option<DirectoryId>,
    progress: &ScanProgress,
) -> Result<ScanReport> {
    let mut report = ScanReport::default();
    let mut dirs = directories::list(pool, false)
        .await
        .context("listing directories")?;
    if let Some(target) = only {
        dirs.retain(|d| d.id == target);
    }
    report.directories_scanned = dirs.len() as u32;

    for dir in dirs {
        if let Err(err) = walk::scan_one(pool, clock, cache, &dir, progress, &mut report).await {
            let msg = format!("scanning {}: {err:#}", dir.path);
            tracing::error!(directory = %dir.path, error = %format!("{err:#}"), "scan errored");
            report.errors.push(msg);
        }
    }
    Ok(report)
}

/// Convenience: just run a scan to completion, returning the report. Primarily for tests.
pub async fn scan_all(
    pool: &SqlitePool,
    clock: &ClockRef,
    cache: &CachePaths,
) -> Result<ScanReport> {
    let progress = ScanProgress::default();
    scan(pool, clock, cache, None, &progress).await
}
