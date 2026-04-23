//! Startup reconciliation for the jobs table.
//!
//! Runs once at server startup before workers come online. Deletes jobs whose
//! video no longer exists, whose directory has been soft-removed, or whose video
//! is missing; and resets crash-orphaned `running` rows back to `pending`.

use anyhow::{Context, Result};
use chrono::Utc;
use sqlx::SqlitePool;

/// Report produced by [`reconcile_on_startup`].
#[derive(Debug, Default, Clone)]
pub struct ReconcileReport {
    /// Jobs deleted because their video has been deleted.
    pub dropped_orphan_video: u64,
    /// Jobs deleted because the video's directory has been soft-removed.
    pub dropped_removed_dir: u64,
    /// Jobs deleted because the video has been marked missing.
    pub dropped_missing_video: u64,
    /// `running` jobs reset back to `pending` (orphaned by a prior crash).
    pub reset_running: u64,
}

/// Reconcile the jobs table against current reality. Intended to run once at startup,
/// before workers are spawned.
///
/// Rules:
/// - Any job whose `video_id` no longer exists is deleted.
/// - Any job whose video's directory is soft-removed (`directories.removed = 1`) is deleted.
/// - Any job whose video is flagged `missing = 1` is deleted — the file isn't on disk anymore,
///   so generating thumbnails/previews would fail.
/// - Any job in `running` state is orphaned (the process that claimed it is gone); reset it
///   back to `pending` so a worker picks it up cleanly.
///
/// Only targets `pending` and `running` — `done` and `failed` rows are preserved as history.
pub async fn reconcile_on_startup(pool: &SqlitePool) -> Result<ReconcileReport> {
    let mut report = ReconcileReport::default();
    let mut tx = pool.begin().await.context("begin reconcile tx")?;

    // Drop jobs whose video no longer exists.
    let res = sqlx::query(
        "DELETE FROM jobs \
         WHERE status IN ('pending', 'running') \
         AND video_id NOT IN (SELECT id FROM videos)",
    )
    .execute(&mut *tx)
    .await
    .context("deleting jobs with orphaned video_id")?;
    report.dropped_orphan_video = res.rows_affected();

    // Drop jobs whose video's directory has been soft-removed.
    let res = sqlx::query(
        "DELETE FROM jobs \
         WHERE status IN ('pending', 'running') \
         AND video_id IN ( \
            SELECT v.id FROM videos v \
            JOIN directories d ON d.id = v.directory_id \
            WHERE d.removed = 1 \
         )",
    )
    .execute(&mut *tx)
    .await
    .context("deleting jobs whose directory was soft-removed")?;
    report.dropped_removed_dir = res.rows_affected();

    // Drop jobs for missing videos — file isn't on disk, so ffmpeg would fail.
    let res = sqlx::query(
        "DELETE FROM jobs \
         WHERE status IN ('pending', 'running') \
         AND video_id IN (SELECT id FROM videos WHERE missing = 1)",
    )
    .execute(&mut *tx)
    .await
    .context("deleting jobs for missing videos")?;
    report.dropped_missing_video = res.rows_affected();

    // Reset remaining 'running' jobs — they were left behind by a prior crash.
    let now_s = Utc::now().to_rfc3339();
    let res = sqlx::query(
        "UPDATE jobs SET status = 'pending', updated_at = ? \
         WHERE status = 'running'",
    )
    .bind(&now_s)
    .execute(&mut *tx)
    .await
    .context("resetting orphaned running jobs")?;
    report.reset_running = res.rows_affected();

    tx.commit().await.context("commit reconcile tx")?;
    Ok(report)
}
