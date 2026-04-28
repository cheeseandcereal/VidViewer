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

#[cfg(test)]
mod tests {
    //! Tests for startup reconciliation. Extracted from the historical
    //! oversized test block in `jobs/mod.rs`.

    use super::*;
    use crate::{
        clock,
        directories::{add as add_dir, soft_remove},
        jobs::{
            counts::count_by_status,
            test_helpers::{setup, test_cache},
        },
    };

    #[tokio::test]
    async fn reconcile_drops_jobs_for_removed_directory_and_resets_running() {
        let (tmp, pool) = setup().await;
        let clock = clock::system();

        // Two directories, each with a video.
        let a = tmp.path().join("a");
        let b = tmp.path().join("b");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        crate::test_support::write_video_fixture(&a, "x.mp4", b"x");
        crate::test_support::write_video_fixture(&b, "y.mp4", b"y");

        let dir_a = add_dir(&pool, &clock, &a, None).await.unwrap();
        let _dir_b = add_dir(&pool, &clock, &b, None).await.unwrap();
        let cache = test_cache(tmp.path());
        crate::scanner::scan_all(&pool, &clock, &cache)
            .await
            .unwrap();

        // Two probe jobs enqueued; simulate one of them as 'running' to look like a crash.
        let a_video_id: String =
            sqlx::query_scalar("SELECT id FROM videos WHERE directory_id = ? LIMIT 1")
                .bind(dir_a.id.raw())
                .fetch_one(&pool)
                .await
                .unwrap();
        sqlx::query("UPDATE jobs SET status = 'running' WHERE video_id = ?")
            .bind(&a_video_id)
            .execute(&pool)
            .await
            .unwrap();

        // Soft-remove directory A. Its pending jobs would be cleared by soft_remove,
        // but the 'running' one was skipped (worker running), so it's still there.
        soft_remove(&pool, &clock, dir_a.id).await.unwrap();
        let (_, running_before, _, _) = count_by_status(&pool).await.unwrap();
        assert_eq!(
            running_before, 1,
            "running job for soft-removed directory should still exist pre-reconcile"
        );

        let report = reconcile_on_startup(&pool).await.unwrap();
        // The 'running' job for the removed-directory video should be dropped.
        assert!(report.dropped_removed_dir >= 1);

        // Directory B's pending job should have been reset to pending (it was
        // already pending, so reset_running may be zero here — just confirm it
        // survived reconciliation).
        let (pending_after, running_after, _, _) = count_by_status(&pool).await.unwrap();
        assert_eq!(
            running_after, 0,
            "no jobs should remain in 'running' after reconcile"
        );
        assert!(
            pending_after >= 1,
            "directory B's job should still be pending"
        );
    }

    #[tokio::test]
    async fn reconcile_resets_running_for_active_video() {
        let (tmp, pool) = setup().await;
        let clock = clock::system();
        let a = tmp.path().join("a");
        std::fs::create_dir_all(&a).unwrap();
        crate::test_support::write_video_fixture(&a, "x.mp4", b"x");
        add_dir(&pool, &clock, &a, None).await.unwrap();
        let cache = test_cache(tmp.path());
        crate::scanner::scan_all(&pool, &clock, &cache)
            .await
            .unwrap();

        sqlx::query("UPDATE jobs SET status = 'running'")
            .execute(&pool)
            .await
            .unwrap();
        let report = reconcile_on_startup(&pool).await.unwrap();
        assert_eq!(report.reset_running, 1);
        let (pending, running, _, _) = count_by_status(&pool).await.unwrap();
        assert_eq!(pending, 1);
        assert_eq!(running, 0);
    }

    #[tokio::test]
    async fn reconcile_heals_stale_pre_audio_support_rows() {
        // Simulates the upgrade story: a row was probed before the
        // audio-support migration (duration populated, other metadata NULL,
        // is_audio_only defaulting to 0 after the column was added), and a
        // preview job was left pending against it. Reconcile should clear the
        // stale metadata, drop the stale preview job, and enqueue a fresh
        // probe.
        let (tmp, pool) = setup().await;
        let clock = clock::system();
        let a = tmp.path().join("a");
        std::fs::create_dir_all(&a).unwrap();
        crate::test_support::write_video_fixture(&a, "x.mp4", b"x");
        add_dir(&pool, &clock, &a, None).await.unwrap();
        let cache = test_cache(tmp.path());
        crate::scanner::scan_all(&pool, &clock, &cache)
            .await
            .unwrap();

        let video_id: String = sqlx::query_scalar("SELECT id FROM videos LIMIT 1")
            .fetch_one(&pool)
            .await
            .unwrap();

        // Force the "pre-audio-support probe" fingerprint:
        //   duration set, width/height/codec NULL, is_audio_only = 0,
        //   cache flags zero.
        sqlx::query(
            "UPDATE videos SET duration_secs = 263.0, width = NULL, height = NULL, \
             codec = NULL, thumbnail_ok = 0, preview_ok = 0, is_audio_only = 0 \
             WHERE id = ?",
        )
        .bind(&video_id)
        .execute(&pool)
        .await
        .unwrap();

        // Remove the initial probe job the scanner enqueued, then insert a
        // stale preview job (as if the verify pass had re-enqueued one).
        sqlx::query("DELETE FROM jobs")
            .execute(&pool)
            .await
            .unwrap();
        let now_s = clock.now().to_rfc3339();
        sqlx::query(
            "INSERT INTO jobs (kind, video_id, status, created_at, updated_at) \
             VALUES ('preview', ?, 'pending', ?, ?)",
        )
        .bind(&video_id)
        .bind(&now_s)
        .bind(&now_s)
        .execute(&pool)
        .await
        .unwrap();

        let report = reconcile_on_startup(&pool).await.unwrap();
        assert_eq!(report.reprobed_stale, 1);

        // The row's stale metadata has been cleared.
        let row = sqlx::query(
            "SELECT duration_secs, width, height, codec, thumbnail_ok, preview_ok \
             FROM videos WHERE id = ?",
        )
        .bind(&video_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        use sqlx::Row;
        let duration: Option<f64> = row.get(0);
        let width: Option<i64> = row.get(1);
        let height: Option<i64> = row.get(2);
        let codec: Option<String> = row.get(3);
        let thumbnail_ok: i64 = row.get("thumbnail_ok");
        let preview_ok: i64 = row.get("preview_ok");
        assert_eq!(duration, None);
        assert_eq!(width, None);
        assert_eq!(height, None);
        assert_eq!(codec, None);
        assert_eq!(thumbnail_ok, 0);
        assert_eq!(preview_ok, 0);

        // The stale preview job is gone; a fresh probe job has taken its
        // place. (Can't rely on job id alone: SQLite reuses INTEGER PRIMARY
        // KEY values, so the new probe may land with the same id as the
        // deleted preview. Assert by kind instead.)
        let preview_count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM jobs WHERE kind = 'preview'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(preview_count, 0, "stale preview should be deleted");
        let probe_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM jobs WHERE kind = 'probe' AND status = 'pending' AND video_id = ?",
        )
        .bind(&video_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(probe_count, 1, "fresh probe job should be enqueued");
    }

    #[tokio::test]
    async fn reconcile_leaves_non_stale_rows_alone() {
        // A real video row (all metadata populated) must not be touched by
        // the stale sweep. Audio-only rows that were correctly classified
        // must also be left alone.
        let (tmp, pool) = setup().await;
        let clock = clock::system();
        let a = tmp.path().join("a");
        std::fs::create_dir_all(&a).unwrap();
        crate::test_support::write_video_fixture(&a, "real_video.mp4", b"x");
        crate::test_support::write_video_fixture(&a, "audio_only.mp3", b"y");
        add_dir(&pool, &clock, &a, None).await.unwrap();
        let cache = test_cache(tmp.path());
        crate::scanner::scan_all(&pool, &clock, &cache)
            .await
            .unwrap();

        // Mark one as a fully-probed real video, the other as a correctly
        // classified audio-only file.
        sqlx::query(
            "UPDATE videos SET duration_secs = 60.0, width = 1920, height = 1080, \
             codec = 'h264', is_audio_only = 0 \
             WHERE filename = 'real_video.mp4'",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "UPDATE videos SET duration_secs = 200.0, width = NULL, height = NULL, \
             codec = 'mp3', is_audio_only = 1 \
             WHERE filename = 'audio_only.mp3'",
        )
        .execute(&pool)
        .await
        .unwrap();

        let report = reconcile_on_startup(&pool).await.unwrap();
        assert_eq!(
            report.reprobed_stale, 0,
            "no rows match the stale fingerprint"
        );
    }
}
