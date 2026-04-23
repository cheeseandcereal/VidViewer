//! Jobs module unit tests.

use std::sync::Arc;

use sqlx::SqlitePool;

use crate::{
    clock,
    directories::{add as add_dir, soft_remove},
    jobs::{
        counts::count_by_status, enqueue_on, reconcile::reconcile_on_startup,
        registry::JobRegistry, worker::Workers, Kind,
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
    std::fs::write(a.join("x.mp4"), b"x").unwrap();
    std::fs::write(b.join("y.mp4"), b"y").unwrap();

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
    std::fs::write(a.join("x.mp4"), b"x").unwrap();
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
    std::fs::write(a.join("x.mp4"), b"x").unwrap();
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
    std::fs::write(a.join("x.mp4"), b"x").unwrap();
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
    std::fs::write(a.join("x.mp4"), b"x").unwrap();
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
    std::fs::write(a.join("x.mp4"), b"x").unwrap();
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
