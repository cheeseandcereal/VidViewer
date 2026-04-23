//! Directory unit tests.

use std::path::Path;

use sqlx::SqlitePool;

use crate::{
    clock::{self, ClockRef},
    directories::{add, hard_remove, list, set_label, soft_remove, AddError},
    ids::DirectoryId,
};

async fn setup() -> (tempfile::TempDir, SqlitePool, ClockRef) {
    let tmp = tempfile::tempdir().unwrap();
    let cfg = crate::config::Config {
        data_dir: tmp.path().to_path_buf(),
        backup_dir: tmp.path().join("backups"),
        ..crate::config::Config::default()
    };
    let db_path = tmp.path().join("vidviewer.db");
    let pool = crate::db::init(&cfg, &db_path).await.unwrap();
    let clock: ClockRef = clock::system();
    (tmp, pool, clock)
}

#[tokio::test]
async fn add_list_and_remove_round_trip() {
    let (tmp, pool, clock) = setup().await;
    let videos = tmp.path().join("videos");
    std::fs::create_dir_all(&videos).unwrap();

    let dir = add(&pool, &clock, &videos, Some("My Vids".into()))
        .await
        .unwrap();
    assert_eq!(dir.label, "My Vids");
    assert!(!dir.removed);
    assert_eq!(dir.video_count, 0);

    let listed = list(&pool, false).await.unwrap();
    assert_eq!(listed.len(), 1);

    // Duplicate add should fail.
    let err = add(&pool, &clock, &videos, None).await.unwrap_err();
    assert!(matches!(err, AddError::PathAlreadyAdded));

    soft_remove(&pool, &clock, dir.id).await.unwrap();
    let listed = list(&pool, false).await.unwrap();
    assert_eq!(listed.len(), 0);
    let all = list(&pool, true).await.unwrap();
    assert_eq!(all.len(), 1);
    assert!(all[0].removed);

    // Re-add un-hides, preserves name.
    let re = add(&pool, &clock, &videos, None).await.unwrap();
    assert_eq!(re.id, dir.id);
    assert!(!re.removed);
}

#[tokio::test]
async fn rejects_non_absolute() {
    let (_tmp, pool, clock) = setup().await;
    let err = add(&pool, &clock, Path::new("relative/path"), None)
        .await
        .unwrap_err();
    assert!(matches!(err, AddError::PathNotAbsolute));
}

#[tokio::test]
async fn rejects_missing() {
    let (tmp, pool, clock) = setup().await;
    let missing = tmp.path().join("does-not-exist");
    let err = add(&pool, &clock, &missing, None).await.unwrap_err();
    assert!(matches!(err, AddError::PathNotFound), "{err:?}");
}

#[tokio::test]
async fn set_label_updates_collection_name() {
    let (tmp, pool, clock) = setup().await;
    let videos = tmp.path().join("videos");
    std::fs::create_dir_all(&videos).unwrap();

    let dir = add(&pool, &clock, &videos, Some("Original".into()))
        .await
        .unwrap();
    let _ = set_label(&pool, &clock, dir.id, "Renamed").await.unwrap();

    let name: String = sqlx::query_scalar("SELECT name FROM collections WHERE id = ?")
        .bind(dir.collection_id.raw())
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(name, "Renamed");
}

#[tokio::test]
async fn soft_remove_cancels_pending_jobs_but_keeps_running() {
    let (tmp, pool, clock) = setup().await;
    let videos = tmp.path().join("videos");
    std::fs::create_dir_all(&videos).unwrap();
    std::fs::write(videos.join("a.mp4"), b"x").unwrap();
    std::fs::write(videos.join("b.mp4"), b"y").unwrap();

    add(&pool, &clock, &videos, None).await.unwrap();
    let cache = crate::scanner::CachePaths {
        thumb: tmp.path().join("thumbs"),
        preview: tmp.path().join("previews"),
    };
    let _ = crate::scanner::scan_all(&pool, &clock, &cache)
        .await
        .unwrap();

    // Two probe jobs were enqueued as 'pending'. Mark one as 'running' to simulate
    // a worker that has claimed it mid-flight.
    let job_id: i64 = sqlx::query_scalar("SELECT id FROM jobs ORDER BY id LIMIT 1")
        .fetch_one(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE jobs SET status = 'running' WHERE id = ?")
        .bind(job_id)
        .execute(&pool)
        .await
        .unwrap();

    let (before_pending, before_running, _, _) = crate::jobs::count_by_status(&pool).await.unwrap();
    assert_eq!(before_pending, 1);
    assert_eq!(before_running, 1);

    let dir_id: i64 = sqlx::query_scalar("SELECT id FROM directories LIMIT 1")
        .fetch_one(&pool)
        .await
        .unwrap();
    soft_remove(&pool, &clock, DirectoryId(dir_id))
        .await
        .unwrap();

    // Pending job for this directory must be gone; running job remains untouched.
    let (after_pending, after_running, _, _) = crate::jobs::count_by_status(&pool).await.unwrap();
    assert_eq!(after_pending, 0, "pending jobs should be cancelled");
    assert_eq!(
        after_running, 1,
        "running jobs are allowed to finish naturally"
    );
}

#[tokio::test]
async fn hard_remove_deletes_all_state() {
    let (tmp, pool, clock) = setup().await;
    let videos = tmp.path().join("videos");
    std::fs::create_dir_all(&videos).unwrap();
    std::fs::write(videos.join("a.mp4"), b"x").unwrap();

    let dir = add(&pool, &clock, &videos, Some("Mine".into()))
        .await
        .unwrap();

    let cache = crate::scanner::CachePaths {
        thumb: tmp.path().join("cache/thumbs"),
        preview: tmp.path().join("cache/previews"),
    };
    let _ = crate::scanner::scan_all(&pool, &clock, &cache)
        .await
        .unwrap();

    let video_id: String = sqlx::query_scalar("SELECT id FROM videos LIMIT 1")
        .fetch_one(&pool)
        .await
        .unwrap();

    // Add this video to a custom collection.
    let custom = crate::collections::create_custom(&pool, &clock, "Favorites")
        .await
        .unwrap();
    crate::collections::add_video(
        &pool,
        &clock,
        custom.id,
        &crate::ids::VideoId(video_id.clone()),
    )
    .await
    .unwrap();

    // Write fake cache files on disk + a watch_history row.
    std::fs::create_dir_all(&cache.thumb).unwrap();
    std::fs::create_dir_all(&cache.preview).unwrap();
    let thumb = cache.thumb.join(format!("{video_id}.jpg"));
    let sheet = cache.preview.join(format!("{video_id}.jpg"));
    let vtt = cache.preview.join(format!("{video_id}.vtt"));
    std::fs::write(&thumb, b"x").unwrap();
    std::fs::write(&sheet, b"x").unwrap();
    std::fs::write(&vtt, b"WEBVTT\n").unwrap();

    sqlx::query(
        "INSERT INTO watch_history (video_id, last_watched_at, position_secs, completed, \
                watch_count) VALUES (?, ?, 10.0, 0, 1)",
    )
    .bind(&video_id)
    .bind(clock.now().to_rfc3339())
    .execute(&pool)
    .await
    .unwrap();

    let report = hard_remove(&pool, &clock, &cache, dir.id).await.unwrap();
    assert_eq!(report.deleted_videos, 1);
    assert_eq!(report.deleted_cache_files, 3);

    // DB rows are gone (cascade).
    let count_dirs: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM directories")
        .fetch_one(&pool)
        .await
        .unwrap();
    let count_videos: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM videos")
        .fetch_one(&pool)
        .await
        .unwrap();
    let count_history: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM watch_history")
        .fetch_one(&pool)
        .await
        .unwrap();
    let count_dir_colls: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM collections WHERE kind = 'directory'")
            .fetch_one(&pool)
            .await
            .unwrap();
    let count_custom_coll_memberships: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM collection_videos cv \
             JOIN collections c ON c.id = cv.collection_id \
             WHERE c.kind = 'custom'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(count_dirs, 0);
    assert_eq!(count_videos, 0);
    assert_eq!(count_history, 0);
    assert_eq!(count_dir_colls, 0);
    assert_eq!(
        count_custom_coll_memberships, 0,
        "custom membership rows cascade-deleted"
    );

    // Custom collection itself survives.
    let count_custom_colls: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM collections WHERE kind = 'custom'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(count_custom_colls, 1);

    // Cache files are gone.
    assert!(!thumb.exists());
    assert!(!sheet.exists());
    assert!(!vtt.exists());
}

#[tokio::test]
async fn hard_remove_errors_on_missing_id() {
    let (tmp, pool, clock) = setup().await;
    let cache = crate::scanner::CachePaths {
        thumb: tmp.path().join("cache/thumbs"),
        preview: tmp.path().join("cache/previews"),
    };
    let err = hard_remove(&pool, &clock, &cache, DirectoryId(9999))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("not found"));
}
