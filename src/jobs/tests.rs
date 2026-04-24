//! Jobs module unit tests.

use std::sync::Arc;

use sqlx::SqlitePool;

use crate::{
    clock,
    directories::{add as add_dir, soft_remove},
    jobs::{
        counts::count_by_status, enqueue_on, reconcile::reconcile_on_startup,
        registry::JobRegistry, reset_stuck_running, worker::Workers, Kind,
    },
    video_tool::{MockVideoTool, VideoToolRef},
};

fn test_cache(tmp: &std::path::Path) -> crate::scanner::CachePaths {
    crate::scanner::CachePaths {
        thumb: tmp.join("cache/thumbs"),
        preview: tmp.join("cache/previews"),
    }
}

async fn setup() -> (tempfile::TempDir, SqlitePool) {
    let tmp = tempfile::tempdir().unwrap();
    let cfg = crate::config::Config {
        data_dir: tmp.path().to_path_buf(),
        backup_dir: tmp.path().join("backups"),
        ..crate::config::Config::default()
    };
    let db_path = cfg.database_path();
    let pool = crate::db::init(&cfg, &db_path).await.unwrap();
    (tmp, pool)
}

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
async fn enqueue_is_idempotent_for_outstanding_jobs() {
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
    let vid = crate::ids::VideoId(video_id);

    // A probe was already enqueued by the scanner. A redundant enqueue returns
    // the same id and does not duplicate.
    let initial_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM jobs WHERE kind = 'probe'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(initial_count, 1);

    let mut conn = pool.acquire().await.unwrap();
    enqueue_on(&mut conn, Kind::Probe, &vid).await.unwrap();
    enqueue_on(&mut conn, Kind::Probe, &vid).await.unwrap();
    drop(conn);

    let after: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM jobs WHERE kind = 'probe'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(after, 1, "duplicate enqueues must not insert new rows");

    // Once the job is done, re-enqueueing should create a fresh pending row
    // (so you can retry after a failure, or re-run after regeneration).
    sqlx::query("UPDATE jobs SET status = 'done'")
        .execute(&pool)
        .await
        .unwrap();
    let mut conn = pool.acquire().await.unwrap();
    enqueue_on(&mut conn, Kind::Probe, &vid).await.unwrap();
    drop(conn);
    let after_redo: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM jobs WHERE kind = 'probe'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(after_redo, 2, "new enqueue after done must succeed");
}

fn make_workers(pool: SqlitePool, clock: crate::clock::ClockRef, tmp: &std::path::Path) -> Workers {
    let cfg = crate::config::Config {
        data_dir: tmp.to_path_buf(),
        backup_dir: tmp.join("backups"),
        ..crate::config::Config::default()
    };
    let video_tool: VideoToolRef = Arc::new(MockVideoTool::new());
    Workers {
        pool,
        clock,
        config: Arc::new(cfg.clone()),
        video_tool,
        thumb_dir: cfg.thumb_cache_dir(),
        preview_dir: cfg.preview_cache_dir(),
        registry: JobRegistry::new(),
    }
}

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

    // Force the scanner-enqueued probe into a fake-stuck state: status=running
    // with an old updated_at, not tracked in the registry.
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

    // …but register the id with the registry so the watchdog considers the
    // task alive. The handle/token values don't matter for the lookup.
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

/// Exercises the free `reset_stuck_running` used by the ad-hoc scan path with
/// a short threshold, to confirm the threshold is honored end-to-end.
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
    let stale_preview_id: i64 = sqlx::query_scalar("SELECT id FROM jobs LIMIT 1")
        .fetch_one(&pool)
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
    let _ = stale_preview_id;
    let preview_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM jobs WHERE kind = 'preview'")
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

#[tokio::test]
async fn cleanup_obsolete_failed_jobs_removes_audio_only_failures_only() {
    use crate::jobs::cleanup_obsolete_failed_jobs;

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
    use crate::jobs::cleanup_obsolete_failed_jobs;

    let (tmp, pool) = setup().await;
    let clock = clock::system();
    let a = tmp.path().join("a");
    std::fs::create_dir_all(&a).unwrap();
    // Three separate audio-only rows so pending/running/done previews don't
    // collide on the (kind, video_id) outstanding-unique index.
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

    // One pending, one running, one done preview job. None are failed, so
    // the cleanup must leave them all alone.
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
    use crate::jobs::cleanup_obsolete_failed_jobs;

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
