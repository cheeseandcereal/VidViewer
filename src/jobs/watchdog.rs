//! Rescue job rows stuck in `running` state when their worker task is gone.
//!
//! Every well-behaved exit path from the worker transitions a claimed row to
//! `done`, `failed`, or deletes it. If *any* of those DB writes fail (locked
//! DB, panic mid-transaction, process signal, etc.) the row is stranded in
//! `running`. Because of `idx_jobs_outstanding_unique` (see
//! `migrations/0001_initial.sql`), no new pending row can be inserted for the
//! same `(kind, video_id)` pair until that row is cleared. Rescans would
//! appear to silently do nothing for the affected video.
//!
//! The watchdog clears the slot. It is run both periodically from the worker
//! pool and ad-hoc from the HTTP scan trigger so manual rescans can resolve
//! stuck probes immediately.

use anyhow::{Context, Result};
use sqlx::{Row, SqlitePool};

use crate::{clock::ClockRef, jobs::registry::JobRegistry};

/// Find `running` rows older than `threshold` whose id is not tracked by the
/// live [`JobRegistry`] and reset them to `pending`. Returns the number of
/// rows reset.
///
/// The registry check is the source of truth for "is a task still alive
/// behind this row" — long ffmpeg runs are fine, since they stay registered.
/// The age threshold only guards against the claim/register race window: a
/// worker transitions a row to `running` in `claim()` and then registers the
/// `AbortHandle` a few microseconds later. Without the threshold, a watchdog
/// pass landing inside that window would wrongly reset a fresh healthy claim.
pub async fn reset_stuck_running(
    pool: &SqlitePool,
    clock: &ClockRef,
    registry: &JobRegistry,
    threshold: chrono::Duration,
) -> Result<u64> {
    let cutoff = (clock.now() - threshold).to_rfc3339();
    let rows = sqlx::query(
        "SELECT id FROM jobs \
         WHERE status = 'running' AND updated_at < ?",
    )
    .bind(&cutoff)
    .fetch_all(pool)
    .await
    .context("listing stuck running jobs")?;

    let mut reset_ids: Vec<i64> = Vec::new();
    for row in &rows {
        let id: i64 = row.get("id");
        if registry.contains(id) {
            // A live worker task still owns this job; leave it alone.
            continue;
        }
        reset_ids.push(id);
    }

    if reset_ids.is_empty() {
        return Ok(0);
    }

    let now_s = clock.now().to_rfc3339();
    let placeholders = vec!["?"; reset_ids.len()].join(",");
    let sql = format!(
        "UPDATE jobs SET status = 'pending', updated_at = ? \
         WHERE status = 'running' AND id IN ({placeholders})"
    );
    let mut q = sqlx::query(&sql).bind(&now_s);
    for id in &reset_ids {
        q = q.bind(id);
    }
    let affected = q
        .execute(pool)
        .await
        .context("resetting stuck running jobs")?
        .rows_affected();

    if affected > 0 {
        tracing::warn!(
            reset = affected,
            ids = ?reset_ids,
            "watchdog reset stuck running jobs to pending"
        );
    }
    Ok(affected)
}

/// Delete historical `failed` job rows whose failure mode is no longer
/// reproducible by the current code. Two categories:
///
/// 1. `preview` and `thumbnail` jobs against `is_audio_only = 1` rows —
///    these were logged before the audio-support commits added their
///    gates. The rerun behavior today would be either "skip cleanly"
///    (preview) or "extract cover art / skip" (thumbnail).
/// 2. `preview` and `thumbnail` jobs against rows where the corresponding
///    `*_ok` flag is now `1` — the asset was successfully regenerated on
///    a later attempt, so the old failure is just stale noise.
///
/// Failed jobs against real video rows whose asset is *still* missing are
/// left in place as diagnostic history: those are the ones a user or
/// operator might actually want to investigate.
///
/// Returns the number of rows deleted. Idempotent.
pub async fn cleanup_obsolete_failed_jobs(pool: &SqlitePool) -> Result<u64> {
    let mut deleted = 0u64;

    // Audio-only rows: both preview and thumbnail failures are obsolete.
    deleted += sqlx::query(
        "DELETE FROM jobs \
         WHERE status = 'failed' \
           AND kind IN ('preview', 'thumbnail') \
           AND video_id IN (SELECT id FROM videos WHERE is_audio_only = 1)",
    )
    .execute(pool)
    .await
    .context("deleting obsolete failed jobs for audio-only rows")?
    .rows_affected();

    // Real-video rows where the thumbnail has since succeeded: the old
    // `failed thumbnail` row is just noise on the activity feed.
    deleted += sqlx::query(
        "DELETE FROM jobs \
         WHERE status = 'failed' \
           AND kind = 'thumbnail' \
           AND video_id IN (SELECT id FROM videos WHERE thumbnail_ok = 1)",
    )
    .execute(pool)
    .await
    .context("deleting failed thumbnail jobs on videos with thumbnail_ok=1")?
    .rows_affected();

    // Same for preview.
    deleted += sqlx::query(
        "DELETE FROM jobs \
         WHERE status = 'failed' \
           AND kind = 'preview' \
           AND video_id IN (SELECT id FROM videos WHERE preview_ok = 1)",
    )
    .execute(pool)
    .await
    .context("deleting failed preview jobs on videos with preview_ok=1")?
    .rows_affected();

    if deleted > 0 {
        tracing::info!(
            deleted,
            "cleaned up failed jobs whose failure mode is no longer reproducible"
        );
    }
    Ok(deleted)
}

#[cfg(test)]
mod tests {
    //! Tests for the stuck-job watchdog and the obsolete-failed-jobs
    //! cleanup. Extracted from the oversized test block in `jobs/mod.rs`.

    use super::*;
    use crate::{
        clock,
        directories::add as add_dir,
        jobs::{
            counts::count_by_status,
            registry::JobRegistry,
            test_helpers::{make_workers, setup, test_cache},
        },
    };

    #[tokio::test]
    async fn watchdog_resets_stuck_running_jobs_not_tracked_by_registry() {
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

        // Force the scanner-enqueued probe into a fake-stuck state:
        // status=running with an old updated_at, not tracked in the registry.
        let old = (clock.now() - chrono::Duration::hours(1)).to_rfc3339();
        sqlx::query("UPDATE jobs SET status = 'running', updated_at = ?")
            .bind(&old)
            .execute(&pool)
            .await
            .unwrap();

        let workers = make_workers(pool.clone(), clock.clone(), tmp.path());
        let reset = workers
            .reset_stuck_running(chrono::Duration::minutes(5))
            .await
            .unwrap();
        assert_eq!(reset, 1);

        let (pending, running, _, _) = count_by_status(&pool).await.unwrap();
        assert_eq!(pending, 1, "stuck running must have been reset to pending");
        assert_eq!(running, 0);
    }

    #[tokio::test]
    async fn watchdog_leaves_live_running_jobs_alone() {
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

        // Flip the row to running with an old updated_at…
        let old = (clock.now() - chrono::Duration::hours(1)).to_rfc3339();
        sqlx::query("UPDATE jobs SET status = 'running', updated_at = ?")
            .bind(&old)
            .execute(&pool)
            .await
            .unwrap();
        let job_id: i64 = sqlx::query_scalar("SELECT id FROM jobs LIMIT 1")
            .fetch_one(&pool)
            .await
            .unwrap();

        // …but register the id with the registry so the watchdog considers
        // the task alive. The handle/token values don't matter for the
        // lookup.
        let workers = make_workers(pool.clone(), clock.clone(), tmp.path());
        let dummy_task: tokio::task::JoinHandle<()> = tokio::spawn(async {
            tokio::time::sleep(std::time::Duration::from_secs(60)).await;
        });
        let abort = dummy_task.abort_handle();
        workers.registry.register(
            job_id,
            crate::ids::VideoId("dummy".into()),
            abort,
            tokio_util::sync::CancellationToken::new(),
        );

        let reset = workers
            .reset_stuck_running(chrono::Duration::minutes(5))
            .await
            .unwrap();
        assert_eq!(reset, 0);
        let (_, running, _, _) = count_by_status(&pool).await.unwrap();
        assert_eq!(running, 1, "tracked running job must not be touched");

        dummy_task.abort();
    }

    #[tokio::test]
    async fn watchdog_respects_age_threshold() {
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

        // Running very recently — should be spared.
        let fresh = clock.now().to_rfc3339();
        sqlx::query("UPDATE jobs SET status = 'running', updated_at = ?")
            .bind(&fresh)
            .execute(&pool)
            .await
            .unwrap();

        let workers = make_workers(pool.clone(), clock.clone(), tmp.path());
        let reset = workers
            .reset_stuck_running(chrono::Duration::minutes(5))
            .await
            .unwrap();
        assert_eq!(reset, 0, "fresh running job must not be touched");
    }

    /// Exercises the free `reset_stuck_running` used by the ad-hoc scan
    /// path with a short threshold, to confirm the threshold is honored
    /// end-to-end.
    #[tokio::test]
    async fn ad_hoc_reset_with_short_threshold_unsticks_jobs_quickly() {
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

        // A probe row stuck in `running` 30 seconds ago, not tracked.
        let stale = (clock.now() - chrono::Duration::seconds(30)).to_rfc3339();
        sqlx::query("UPDATE jobs SET status = 'running', updated_at = ?")
            .bind(&stale)
            .execute(&pool)
            .await
            .unwrap();

        // A 5-second threshold catches it (30s old > 5s threshold).
        let registry = JobRegistry::new();
        let reset = reset_stuck_running(&pool, &clock, &registry, chrono::Duration::seconds(5))
            .await
            .unwrap();
        assert_eq!(reset, 1);
        let (pending, running, _, _) = count_by_status(&pool).await.unwrap();
        assert_eq!(pending, 1);
        assert_eq!(running, 0);
    }

    #[tokio::test]
    async fn cleanup_obsolete_failed_jobs_removes_audio_only_failures_only() {
        let (tmp, pool) = setup().await;
        let clock = clock::system();
        let a = tmp.path().join("a");
        std::fs::create_dir_all(&a).unwrap();
        crate::test_support::write_video_fixture(&a, "audio_only.mp3", b"x");
        crate::test_support::write_video_fixture(&a, "real_video.mp4", b"y");
        add_dir(&pool, &clock, &a, None).await.unwrap();
        let cache = test_cache(tmp.path());
        crate::scanner::scan_all(&pool, &clock, &cache)
            .await
            .unwrap();

        // Mark the two rows.
        sqlx::query("UPDATE videos SET is_audio_only = 1 WHERE filename = 'audio_only.mp3'")
            .execute(&pool)
            .await
            .unwrap();

        let audio_id: String =
            sqlx::query_scalar("SELECT id FROM videos WHERE filename = 'audio_only.mp3'")
                .fetch_one(&pool)
                .await
                .unwrap();
        let video_id: String =
            sqlx::query_scalar("SELECT id FROM videos WHERE filename = 'real_video.mp4'")
                .fetch_one(&pool)
                .await
                .unwrap();

        // Clear the jobs table and seed a mix of failed rows.
        sqlx::query("DELETE FROM jobs")
            .execute(&pool)
            .await
            .unwrap();
        let now_s = clock.now().to_rfc3339();
        for (kind, vid) in [
            // Should be deleted: audio-only preview + thumbnail failures.
            ("preview", &audio_id),
            ("thumbnail", &audio_id),
            // Should be kept: real-video thumbnail failure (diagnostic history).
            ("thumbnail", &video_id),
            // Should be kept: audio-only probe failure (not preview/thumbnail).
            ("probe", &audio_id),
        ] {
            sqlx::query(
                "INSERT INTO jobs (kind, video_id, status, error, created_at, updated_at) \
                 VALUES (?, ?, 'failed', 'some error', ?, ?)",
            )
            .bind(kind)
            .bind(vid)
            .bind(&now_s)
            .bind(&now_s)
            .execute(&pool)
            .await
            .unwrap();
        }

        let deleted = cleanup_obsolete_failed_jobs(&pool).await.unwrap();
        assert_eq!(deleted, 2);

        let remaining: Vec<(String, String)> =
            sqlx::query_as("SELECT kind, video_id FROM jobs ORDER BY kind, video_id")
                .fetch_all(&pool)
                .await
                .unwrap();
        // Only the probe-against-audio and thumbnail-against-video rows survive.
        assert_eq!(remaining.len(), 2);
        assert!(remaining
            .iter()
            .any(|(k, v)| k == "probe" && v == &audio_id));
        assert!(remaining
            .iter()
            .any(|(k, v)| k == "thumbnail" && v == &video_id));
    }

    #[tokio::test]
    async fn cleanup_obsolete_failed_jobs_leaves_non_failed_rows_alone() {
        let (tmp, pool) = setup().await;
        let clock = clock::system();
        let a = tmp.path().join("a");
        std::fs::create_dir_all(&a).unwrap();
        // Three separate audio-only rows so pending/running/done previews
        // don't collide on the (kind, video_id) outstanding-unique index.
        crate::test_support::write_video_fixture(&a, "one.mp3", b"x");
        crate::test_support::write_video_fixture(&a, "two.mp3", b"y");
        crate::test_support::write_video_fixture(&a, "three.mp3", b"z");
        add_dir(&pool, &clock, &a, None).await.unwrap();
        let cache = test_cache(tmp.path());
        crate::scanner::scan_all(&pool, &clock, &cache)
            .await
            .unwrap();

        sqlx::query("UPDATE videos SET is_audio_only = 1")
            .execute(&pool)
            .await
            .unwrap();
        let vids: Vec<String> = sqlx::query_scalar("SELECT id FROM videos ORDER BY filename")
            .fetch_all(&pool)
            .await
            .unwrap();
        assert_eq!(vids.len(), 3);

        // One pending, one running, one done preview job. None are failed,
        // so the cleanup must leave them all alone.
        sqlx::query("DELETE FROM jobs")
            .execute(&pool)
            .await
            .unwrap();
        let now_s = clock.now().to_rfc3339();
        for (status, vid) in [
            ("pending", &vids[0]),
            ("running", &vids[1]),
            ("done", &vids[2]),
        ] {
            sqlx::query(
                "INSERT INTO jobs (kind, video_id, status, created_at, updated_at) \
                 VALUES ('preview', ?, ?, ?, ?)",
            )
            .bind(vid)
            .bind(status)
            .bind(&now_s)
            .bind(&now_s)
            .execute(&pool)
            .await
            .unwrap();
        }

        let deleted = cleanup_obsolete_failed_jobs(&pool).await.unwrap();
        assert_eq!(deleted, 0);
        let remaining: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM jobs")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(remaining, 3);
    }

    #[tokio::test]
    async fn cleanup_obsolete_failed_jobs_removes_recovered_video_failures() {
        // Real video whose thumbnail_ok is 1 now (a later attempt succeeded).
        // Any historical `failed thumbnail` row for it is just noise and
        // should be cleaned up. Same logic for preview on preview_ok=1 rows.
        let (tmp, pool) = setup().await;
        let clock = clock::system();
        let a = tmp.path().join("a");
        std::fs::create_dir_all(&a).unwrap();
        crate::test_support::write_video_fixture(&a, "recovered_thumb.mp4", b"x");
        crate::test_support::write_video_fixture(&a, "recovered_prev.mp4", b"y");
        crate::test_support::write_video_fixture(&a, "still_broken.mp4", b"z");
        add_dir(&pool, &clock, &a, None).await.unwrap();
        let cache = test_cache(tmp.path());
        crate::scanner::scan_all(&pool, &clock, &cache)
            .await
            .unwrap();

        // Mark the three rows: two recovered (one thumbnail, one preview),
        // one still broken (nothing generated yet).
        sqlx::query("UPDATE videos SET thumbnail_ok = 1 WHERE filename = 'recovered_thumb.mp4'")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("UPDATE videos SET preview_ok = 1 WHERE filename = 'recovered_prev.mp4'")
            .execute(&pool)
            .await
            .unwrap();
        let recovered_thumb_id: String =
            sqlx::query_scalar("SELECT id FROM videos WHERE filename = 'recovered_thumb.mp4'")
                .fetch_one(&pool)
                .await
                .unwrap();
        let recovered_prev_id: String =
            sqlx::query_scalar("SELECT id FROM videos WHERE filename = 'recovered_prev.mp4'")
                .fetch_one(&pool)
                .await
                .unwrap();
        let still_broken_id: String =
            sqlx::query_scalar("SELECT id FROM videos WHERE filename = 'still_broken.mp4'")
                .fetch_one(&pool)
                .await
                .unwrap();

        // Seed failed rows.
        sqlx::query("DELETE FROM jobs")
            .execute(&pool)
            .await
            .unwrap();
        let now_s = clock.now().to_rfc3339();
        for (kind, vid) in [
            // Should be deleted: failure superseded by a successful run.
            ("thumbnail", &recovered_thumb_id),
            ("preview", &recovered_prev_id),
            // Should be kept: still broken, operator may want to investigate.
            ("thumbnail", &still_broken_id),
        ] {
            sqlx::query(
                "INSERT INTO jobs (kind, video_id, status, error, created_at, updated_at) \
                 VALUES (?, ?, 'failed', 'e', ?, ?)",
            )
            .bind(kind)
            .bind(vid)
            .bind(&now_s)
            .bind(&now_s)
            .execute(&pool)
            .await
            .unwrap();
        }

        let deleted = cleanup_obsolete_failed_jobs(&pool).await.unwrap();
        assert_eq!(deleted, 2);

        let remaining: Vec<(String, String)> =
            sqlx::query_as("SELECT kind, video_id FROM jobs ORDER BY id")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].0, "thumbnail");
        assert_eq!(remaining[0].1, still_broken_id);
    }
}
