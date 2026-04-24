//! Startup reconciliation for the jobs table.
//!
//! Runs once at server startup before workers come online. Deletes jobs whose
//! video no longer exists, whose directory has been soft-removed, or whose video
//! is missing; resets crash-orphaned `running` rows back to `pending`; and
//! heals pre-audio-support probe rows that were never classified as audio-only.

use anyhow::{Context, Result};
use chrono::Utc;
use sqlx::{Row, SqlitePool};

use crate::{ids::VideoId, jobs};

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
    /// Rows healed by the stale-probe sweep (pre-audio-support probe results
    /// with duration but no other metadata). Their derived-asset flags and
    /// metadata columns are cleared, outstanding thumbnail/preview jobs are
    /// dropped, and a fresh `probe` job is enqueued so the current classifier
    /// can re-read the file.
    pub reprobed_stale: u64,
}

/// Reconcile the jobs table against current reality. Intended to run once at startup,
/// before workers are spawned.
///
/// Rules:
/// - Any job whose `video_id` no longer exists is deleted.
/// - Any job whose video's directory is soft-removed (`directories.removed = 1`) is deleted.
/// - Any job whose video is flagged `missing = 1` is deleted — the file isn't on disk anymore,
///   so generating thumbnails/previews would fail.
/// - Rows with a pre-audio-support probe fingerprint (see
///   [`reprobe_stale_rows`]) are healed: their metadata is cleared, their
///   outstanding thumbnail/preview jobs are dropped, and a fresh probe is
///   enqueued.
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

    // Heal pre-audio-support probe rows.
    report.reprobed_stale = reprobe_stale_rows(&mut tx).await?;

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

/// Find rows that were probed before the audio-support migration and heal
/// them: strip stale metadata, drop outstanding thumbnail/preview jobs, and
/// enqueue a fresh probe.
///
/// The fingerprint `width IS NULL AND height IS NULL AND codec IS NULL AND
/// duration_secs IS NOT NULL AND is_audio_only = 0` is specific to the old
/// probe: it populated `duration_secs` but left the other metadata columns
/// empty for audio-only files, and pre-dates the `is_audio_only` flag. Real
/// video rows have width/height/codec populated; real audio-only rows
/// probed by the current code have `is_audio_only = 1`.
///
/// Without this sweep, such rows get re-enqueued as preview jobs on every
/// scan (`verify.rs` trusts the `is_audio_only` flag), and the preview
/// worker fails at tile 0 against files with no video stream.
async fn reprobe_stale_rows(tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>) -> Result<u64> {
    let stale: Vec<String> = sqlx::query(
        "SELECT id FROM videos \
         WHERE width IS NULL \
           AND height IS NULL \
           AND codec IS NULL \
           AND duration_secs IS NOT NULL \
           AND is_audio_only = 0",
    )
    .fetch_all(&mut **tx)
    .await
    .context("listing stale probe rows")?
    .into_iter()
    .map(|r| r.get::<String, _>("id"))
    .collect();

    if stale.is_empty() {
        return Ok(0);
    }

    let now_s = Utc::now().to_rfc3339();
    for id in &stale {
        // Strip stale classification and reset cache flags. A fresh probe
        // job will repopulate all of this.
        sqlx::query(
            "UPDATE videos SET \
                 duration_secs = NULL, \
                 width = NULL, \
                 height = NULL, \
                 codec = NULL, \
                 thumbnail_ok = 0, \
                 preview_ok = 0, \
                 updated_at = ? \
             WHERE id = ?",
        )
        .bind(&now_s)
        .bind(id)
        .execute(&mut **tx)
        .await
        .context("clearing stale probe row")?;

        // Drop outstanding thumbnail/preview jobs so they don't race the
        // new probe. Probe jobs we leave alone; if one is already pending
        // for this row, the enqueue below is a no-op.
        sqlx::query(
            "DELETE FROM jobs \
             WHERE video_id = ? \
               AND kind IN ('thumbnail', 'preview') \
               AND status IN ('pending', 'running')",
        )
        .bind(id)
        .execute(&mut **tx)
        .await
        .context("dropping stale thumbnail/preview jobs")?;

        // Enqueue a fresh probe. Idempotent: if another probe is already
        // pending or running for this video_id, the existing row is
        // reused.
        let vid = VideoId(id.clone());
        jobs::enqueue_on(tx, jobs::Kind::Probe, &vid)
            .await
            .context("enqueuing reprobe for stale row")?;

        tracing::info!(
            video_id = %id,
            "reconcile: healed stale pre-audio-support probe row"
        );
    }
    tracing::warn!(
        count = stale.len(),
        "reconcile: healed stale pre-audio-support probe rows"
    );
    Ok(stale.len() as u64)
}
