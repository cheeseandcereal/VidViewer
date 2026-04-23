//! Scanner unit tests. Moved verbatim from the old `src/scanner/mod.rs`
//! `#[cfg(test)] mod tests` block.

use std::path::Path;

use sqlx::SqlitePool;

use crate::{
    clock::{self, ClockRef},
    directories::add as add_dir,
    ids::DirectoryId,
    scanner::{scan_all, CachePaths},
};

async fn setup() -> (tempfile::TempDir, SqlitePool, ClockRef, CachePaths) {
    let tmp = tempfile::tempdir().unwrap();
    let cfg = crate::config::Config {
        data_dir: tmp.path().to_path_buf(),
        backup_dir: tmp.path().join("backups"),
        ..crate::config::Config::default()
    };
    let db_path = cfg.database_path();
    let pool = crate::db::init(&cfg, &db_path).await.unwrap();
    let cache = CachePaths::from_config(&cfg);
    (tmp, pool, clock::system(), cache)
}

fn write_video(dir: &Path, name: &str, bytes: &[u8]) {
    std::fs::write(dir.join(name), bytes).unwrap();
}

#[tokio::test]
async fn inserts_new_videos_and_enqueues_probe() {
    let (tmp, pool, clock, cache) = setup().await;
    let videos_dir = tmp.path().join("videos");
    std::fs::create_dir_all(&videos_dir).unwrap();
    write_video(&videos_dir, "a.mp4", b"x");
    write_video(&videos_dir, "b.mkv", b"xx");
    write_video(&videos_dir, "not-a-video.txt", b"skip");

    add_dir(&pool, &clock, &videos_dir, None).await.unwrap();
    let report = scan_all(&pool, &clock, &cache).await.unwrap();
    assert_eq!(report.new_videos, 2);
    assert_eq!(report.files_seen, 2, "expected only video files counted");
    assert_eq!(report.changed_videos, 0);

    // Probe jobs enqueued for each new video.
    let (pending, _, _, _) = crate::jobs::count_by_status(&pool).await.unwrap();
    assert_eq!(pending, 2);
}

#[tokio::test]
async fn second_scan_is_noop() {
    let (tmp, pool, clock, cache) = setup().await;
    let videos_dir = tmp.path().join("videos");
    std::fs::create_dir_all(&videos_dir).unwrap();
    write_video(&videos_dir, "a.mp4", b"x");

    add_dir(&pool, &clock, &videos_dir, None).await.unwrap();
    let _ = scan_all(&pool, &clock, &cache).await.unwrap();

    // Simulate the probe + thumbnail + preview pipeline having completed
    // successfully, and the expected cache files being on disk. This is the
    // steady-state "nothing to do" condition a second scan should observe.
    let video_id: String = sqlx::query_scalar("SELECT id FROM videos LIMIT 1")
        .fetch_one(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE videos SET thumbnail_ok = 1, preview_ok = 1, duration_secs = 60.0")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE jobs SET status = 'done'")
        .execute(&pool)
        .await
        .unwrap();
    std::fs::create_dir_all(&cache.thumb).unwrap();
    std::fs::create_dir_all(&cache.preview).unwrap();
    std::fs::write(cache.thumb.join(format!("{video_id}.jpg")), b"x").unwrap();
    std::fs::write(cache.preview.join(format!("{video_id}.jpg")), b"x").unwrap();
    std::fs::write(cache.preview.join(format!("{video_id}.vtt")), b"WEBVTT\n").unwrap();

    let report = scan_all(&pool, &clock, &cache).await.unwrap();
    assert_eq!(report.new_videos, 0);
    assert_eq!(report.changed_videos, 0);
    assert_eq!(report.missing_videos, 0);
    assert_eq!(report.recovered_thumbnail_jobs, 0);
    assert_eq!(report.recovered_preview_jobs, 0);
}

#[tokio::test]
async fn detects_change_and_missing() {
    let (tmp, pool, clock, cache) = setup().await;
    let videos_dir = tmp.path().join("videos");
    std::fs::create_dir_all(&videos_dir).unwrap();
    write_video(&videos_dir, "a.mp4", b"x");
    write_video(&videos_dir, "b.mp4", b"y");

    add_dir(&pool, &clock, &videos_dir, None).await.unwrap();
    let _ = scan_all(&pool, &clock, &cache).await.unwrap();

    // Modify a.mp4 and delete b.mp4. Force mtime change.
    std::fs::write(videos_dir.join("a.mp4"), b"xxxx").unwrap();
    let new_mtime = std::time::SystemTime::now();
    filetime::set_file_mtime(
        videos_dir.join("a.mp4"),
        filetime::FileTime::from_system_time(new_mtime + std::time::Duration::from_secs(10)),
    )
    .unwrap();
    std::fs::remove_file(videos_dir.join("b.mp4")).unwrap();

    let report = scan_all(&pool, &clock, &cache).await.unwrap();
    assert_eq!(report.new_videos, 0);
    assert_eq!(report.changed_videos, 1);
    assert_eq!(report.missing_videos, 1);

    // Directory collection should have only the live video.
    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM collection_videos cv \
             JOIN collections c ON c.id = cv.collection_id \
             WHERE c.kind = 'directory'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(count, 1);
}

#[tokio::test]
async fn recovers_missing_thumbnail_and_preview_cache() {
    let (tmp, pool, clock, cache) = setup().await;
    let videos_dir = tmp.path().join("videos");
    std::fs::create_dir_all(&videos_dir).unwrap();
    write_video(&videos_dir, "a.mp4", b"x");

    add_dir(&pool, &clock, &videos_dir, None).await.unwrap();
    let _ = scan_all(&pool, &clock, &cache).await.unwrap();

    // Fake a "previously completed" state: mark thumbnail_ok and preview_ok,
    // give the video a duration, and clear the initial probe job so we can
    // isolate the recovery behavior.
    let video_id: String = sqlx::query_scalar("SELECT id FROM videos LIMIT 1")
        .fetch_one(&pool)
        .await
        .unwrap();
    sqlx::query(
        "UPDATE videos SET thumbnail_ok = 1, preview_ok = 1, \
                duration_secs = 60.0 WHERE id = ?",
    )
    .bind(&video_id)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("UPDATE jobs SET status = 'done'")
        .execute(&pool)
        .await
        .unwrap();

    // Pretend the cache was populated, then wiped. We never actually write the
    // files; running the scan with flags set but nothing on disk should detect
    // the discrepancy and re-enqueue.
    std::fs::create_dir_all(&cache.thumb).unwrap();
    std::fs::create_dir_all(&cache.preview).unwrap();

    let report = scan_all(&pool, &clock, &cache).await.unwrap();
    assert_eq!(report.recovered_thumbnail_jobs, 1);
    assert_eq!(report.recovered_preview_jobs, 1);

    // Flags cleared.
    let (thumb_ok, preview_ok): (i64, i64) =
        sqlx::query_as("SELECT thumbnail_ok, preview_ok FROM videos LIMIT 1")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(thumb_ok, 0);
    assert_eq!(preview_ok, 0);

    // Jobs re-enqueued.
    let pending_thumb: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM jobs WHERE kind = 'thumbnail' AND status = 'pending'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    let pending_preview: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM jobs WHERE kind = 'preview' AND status = 'pending'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(pending_thumb, 1);
    assert_eq!(pending_preview, 1);

    // Now create the expected cache files and re-scan: recovery counters stay at 0.
    std::fs::write(cache.thumb.join(format!("{video_id}.jpg")), b"x").unwrap();
    std::fs::write(cache.preview.join(format!("{video_id}.jpg")), b"x").unwrap();
    std::fs::write(cache.preview.join(format!("{video_id}.vtt")), b"WEBVTT\n").unwrap();
    // Mark the flags back to 1 and done out the re-enqueued jobs so the next
    // scan has something to verify.
    sqlx::query("UPDATE videos SET thumbnail_ok = 1, preview_ok = 1")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE jobs SET status = 'done' WHERE status = 'pending'")
        .execute(&pool)
        .await
        .unwrap();

    let report = scan_all(&pool, &clock, &cache).await.unwrap();
    assert_eq!(report.recovered_thumbnail_jobs, 0);
    assert_eq!(report.recovered_preview_jobs, 0);
}

/// Helper: soft-remove the single directory in the test DB and return its id.
async fn soft_remove_only_dir(pool: &SqlitePool, clock: &ClockRef) -> DirectoryId {
    let dir_id: i64 = sqlx::query_scalar("SELECT id FROM directories LIMIT 1")
        .fetch_one(pool)
        .await
        .unwrap();
    let id = DirectoryId(dir_id);
    crate::directories::soft_remove(pool, clock, id)
        .await
        .unwrap();
    id
}

#[tokio::test]
async fn re_add_preserves_flags_when_cache_present() {
    let (tmp, pool, clock, cache) = setup().await;
    let videos_dir = tmp.path().join("videos");
    std::fs::create_dir_all(&videos_dir).unwrap();
    write_video(&videos_dir, "a.mp4", b"x");

    add_dir(&pool, &clock, &videos_dir, None).await.unwrap();
    scan_all(&pool, &clock, &cache).await.unwrap();

    // Simulate the probe+thumb+preview pipeline having completed.
    let video_id: String = sqlx::query_scalar("SELECT id FROM videos LIMIT 1")
        .fetch_one(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE videos SET thumbnail_ok = 1, preview_ok = 1, duration_secs = 60.0")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE jobs SET status = 'done'")
        .execute(&pool)
        .await
        .unwrap();
    std::fs::create_dir_all(&cache.thumb).unwrap();
    std::fs::create_dir_all(&cache.preview).unwrap();
    std::fs::write(cache.thumb.join(format!("{video_id}.jpg")), b"x").unwrap();
    std::fs::write(cache.preview.join(format!("{video_id}.jpg")), b"x").unwrap();
    std::fs::write(cache.preview.join(format!("{video_id}.vtt")), b"WEBVTT\n").unwrap();

    // Soft-remove, then re-add the same directory path.
    soft_remove_only_dir(&pool, &clock).await;
    add_dir(&pool, &clock, &videos_dir, None).await.unwrap();

    let report = scan_all(&pool, &clock, &cache).await.unwrap();
    assert_eq!(report.recovered_thumbnail_jobs, 0);
    assert_eq!(report.recovered_preview_jobs, 0);

    // Flags preserved, missing cleared.
    let (thumb_ok, preview_ok, missing): (i64, i64, i64) =
        sqlx::query_as("SELECT thumbnail_ok, preview_ok, missing FROM videos LIMIT 1")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(thumb_ok, 1);
    assert_eq!(preview_ok, 1);
    assert_eq!(missing, 0);

    // No probe/thumbnail/preview jobs were enqueued by the re-add scan.
    let pending: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM jobs WHERE status IN ('pending','running')")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(pending, 0);
}

#[tokio::test]
async fn re_add_regenerates_flags_when_cache_missing() {
    let (tmp, pool, clock, cache) = setup().await;
    let videos_dir = tmp.path().join("videos");
    std::fs::create_dir_all(&videos_dir).unwrap();
    write_video(&videos_dir, "a.mp4", b"x");

    add_dir(&pool, &clock, &videos_dir, None).await.unwrap();
    scan_all(&pool, &clock, &cache).await.unwrap();

    // Same setup as above, but cache files never exist on disk.
    sqlx::query("UPDATE videos SET thumbnail_ok = 1, preview_ok = 1, duration_secs = 60.0")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE jobs SET status = 'done'")
        .execute(&pool)
        .await
        .unwrap();

    soft_remove_only_dir(&pool, &clock).await;
    add_dir(&pool, &clock, &videos_dir, None).await.unwrap();

    let report = scan_all(&pool, &clock, &cache).await.unwrap();
    assert_eq!(report.recovered_thumbnail_jobs, 1);
    assert_eq!(report.recovered_preview_jobs, 1);

    // Flags cleared.
    let (thumb_ok, preview_ok): (i64, i64) =
        sqlx::query_as("SELECT thumbnail_ok, preview_ok FROM videos LIMIT 1")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(thumb_ok, 0);
    assert_eq!(preview_ok, 0);

    // Thumbnail + preview jobs re-enqueued; no probe (duration is still valid).
    let pending_thumb: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM jobs WHERE kind='thumbnail' AND status='pending'")
            .fetch_one(&pool)
            .await
            .unwrap();
    let pending_preview: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM jobs WHERE kind='preview' AND status='pending'")
            .fetch_one(&pool)
            .await
            .unwrap();
    let pending_probe: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM jobs WHERE kind='probe' AND status='pending'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(pending_thumb, 1);
    assert_eq!(pending_preview, 1);
    assert_eq!(pending_probe, 0);
}

#[tokio::test]
async fn rescan_enqueues_missing_jobs_even_when_flags_are_zero() {
    // Covers the case where a past thumbnail/preview job never completed (e.g.
    // failed, was aborted by a directory remove, or the worker crashed). The
    // flag is 0 and the file is absent. A fresh scan should notice the gap
    // and enqueue a job regardless of the flag state.
    let (tmp, pool, clock, cache) = setup().await;
    let videos_dir = tmp.path().join("videos");
    std::fs::create_dir_all(&videos_dir).unwrap();
    write_video(&videos_dir, "a.mp4", b"x");

    add_dir(&pool, &clock, &videos_dir, None).await.unwrap();
    scan_all(&pool, &clock, &cache).await.unwrap();

    // Pretend the probe completed but thumbnail and preview jobs never did.
    // Flags stay at 0; no cache files exist on disk.
    sqlx::query("UPDATE videos SET duration_secs = 60.0, thumbnail_ok = 0, preview_ok = 0")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM jobs")
        .execute(&pool)
        .await
        .unwrap();

    let report = scan_all(&pool, &clock, &cache).await.unwrap();
    assert_eq!(report.recovered_thumbnail_jobs, 1);
    assert_eq!(report.recovered_preview_jobs, 1);

    let pending_thumb: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM jobs WHERE kind='thumbnail' AND status='pending'")
            .fetch_one(&pool)
            .await
            .unwrap();
    let pending_preview: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM jobs WHERE kind='preview' AND status='pending'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(pending_thumb, 1);
    assert_eq!(pending_preview, 1);
}
