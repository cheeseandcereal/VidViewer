//! Integration test: directory removal cancels in-flight jobs.
//!
//! Uses a custom `VideoTool` that blocks in `probe` until the outer orchestration
//! cancels the job, so we can observe the abort flow end-to-end without relying on
//! real ffmpeg.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;
use vidviewer::{
    clock::{self, ClockRef},
    config::Config,
    db,
    directories::add as add_dir,
    ids::{DirectoryId, VideoId},
    jobs::{registry::JobRegistry, worker::Workers},
    scanner::{self, CachePaths},
    video_tool::{PreviewPlan, ProbeResult, VideoTool, VideoToolRef},
};

/// A VideoTool that blocks forever on probe — ideal for observing cancellation.
#[derive(Default)]
struct BlockingTool;

#[async_trait]
impl VideoTool for BlockingTool {
    async fn probe(&self, _path: &Path) -> Result<ProbeResult> {
        // Sleep in 100ms ticks so the task reacts to an abort promptly.
        loop {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
    async fn thumbnail(&self, _src: &Path, _dst: &Path, _at_secs: f64, _width: u32) -> Result<()> {
        Ok(())
    }
    async fn previews(
        &self,
        _src: &Path,
        _dst: &Path,
        _plan: &PreviewPlan,
        _duration_secs: f64,
        _cancel: &CancellationToken,
    ) -> Result<()> {
        Ok(())
    }
}

async fn setup() -> (TempDir, sqlx::SqlitePool, ClockRef, Config) {
    let tmp = tempfile::tempdir().unwrap();
    let cfg = Config {
        data_dir: tmp.path().to_path_buf(),
        backup_dir: tmp.path().join("backups"),
        ..Config::default()
    };
    let db_path = cfg.database_path();
    let pool = db::init(&cfg, &db_path).await.unwrap();
    (tmp, pool, clock::system(), cfg)
}

async fn wait_for<F: Fn() -> bool>(desc: &str, check: F) {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while !check() {
        if std::time::Instant::now() > deadline {
            panic!("timed out waiting for {desc}");
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

#[tokio::test]
async fn directory_remove_aborts_running_jobs() {
    let (tmp, pool, clock, cfg) = setup().await;

    // Seed a directory with one video.
    let videos = tmp.path().join("videos");
    std::fs::create_dir_all(&videos).unwrap();
    std::fs::write(videos.join("a.mp4"), b"x").unwrap();

    let dir = add_dir(&pool, &clock, &videos, None).await.unwrap();
    let cache = CachePaths::from_config(&cfg);
    let _ = scanner::scan_all(&pool, &clock, &cache).await.unwrap();

    // One probe job is now pending. Start workers with the blocking tool.
    let tool: VideoToolRef = Arc::new(BlockingTool);
    let registry = JobRegistry::new();
    let workers = Workers {
        pool: pool.clone(),
        clock: clock.clone(),
        config: Arc::new(Config {
            thumbnail_width: 320,
            preview_min_interval: 2.0,
            preview_target_count: 10,
            ..cfg.clone()
        }),
        video_tool: tool,
        thumb_dir: cache.thumb.clone(),
        preview_dir: cache.preview.clone(),
        registry: registry.clone(),
    };
    let _handles = workers.spawn_all(1, 1);

    // Wait for the job to be registered as running.
    wait_for("probe job to register", || registry.len() == 1).await;
    let running_before: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM jobs WHERE status = 'running'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(running_before, 1);

    // Look up the video id and cancel.
    let vid: String = sqlx::query_scalar("SELECT id FROM videos LIMIT 1")
        .fetch_one(&pool)
        .await
        .unwrap();
    let aborted = registry.cancel_for_videos(&[VideoId(vid.clone())]);
    assert_eq!(aborted.len(), 1, "one running job should be aborted");

    // The worker loop observes the cancellation and deletes the row. Wait for it.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM jobs")
            .fetch_one(&pool)
            .await
            .unwrap();
        if n == 0 {
            break;
        }
        if std::time::Instant::now() > deadline {
            panic!("timed out waiting for aborted job row to be deleted");
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    // Registry drained too.
    assert!(registry.is_empty());

    // Helper cleanup hint for the compiler.
    let _ = dir.id;
    let _ = DirectoryId(0);
    let _: PathBuf = cache.thumb.clone();
}

/// A `VideoTool` whose `previews` loops internally, simulating the
/// per-timestamp ffmpeg loop. It increments a counter every "tile" and sleeps
/// briefly so the cancellation token has a chance to fire between iterations.
#[derive(Default)]
struct CountingPreviewTool {
    tiles: Arc<AtomicU32>,
}

#[async_trait]
impl VideoTool for CountingPreviewTool {
    async fn probe(&self, _path: &Path) -> Result<ProbeResult> {
        Ok(ProbeResult {
            duration_secs: Some(10.0),
            width: Some(640),
            height: Some(360),
            codec: Some("h264".into()),
        })
    }
    async fn thumbnail(&self, _src: &Path, _dst: &Path, _at_secs: f64, _width: u32) -> Result<()> {
        Ok(())
    }
    async fn previews(
        &self,
        _src: &Path,
        _dst: &Path,
        _plan: &PreviewPlan,
        _duration_secs: f64,
        cancel: &CancellationToken,
    ) -> Result<()> {
        for _ in 0..100 {
            if cancel.is_cancelled() {
                anyhow::bail!("cancelled");
            }
            self.tiles.fetch_add(1, Ordering::Relaxed);
            // A short yield point so the token has a real chance to flip.
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        Ok(())
    }
}

#[tokio::test]
async fn preview_loop_stops_after_cancellation() {
    let (tmp, pool, clock, cfg) = setup().await;

    // Seed a directory with one video + a probe+thumbnail+preview pipeline.
    let videos = tmp.path().join("videos");
    std::fs::create_dir_all(&videos).unwrap();
    std::fs::write(videos.join("a.mp4"), b"x").unwrap();

    add_dir(&pool, &clock, &videos, None).await.unwrap();
    let cache = CachePaths::from_config(&cfg);
    scanner::scan_all(&pool, &clock, &cache).await.unwrap();

    let counter = Arc::new(AtomicU32::new(0));
    let tool: VideoToolRef = Arc::new(CountingPreviewTool {
        tiles: counter.clone(),
    });
    let registry = JobRegistry::new();
    let workers = Workers {
        pool: pool.clone(),
        clock: clock.clone(),
        config: Arc::new(cfg.clone()),
        video_tool: tool,
        thumb_dir: cache.thumb.clone(),
        preview_dir: cache.preview.clone(),
        registry: registry.clone(),
    };
    let _handles = workers.spawn_all(1, 1);

    // Wait for the preview job to start (probe + thumb complete first; preview is registered).
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let running: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM jobs WHERE status = 'running' AND kind = 'preview'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        if running == 1 {
            break;
        }
        if std::time::Instant::now() > deadline {
            panic!("preview job never started");
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    // Let the preview loop tick a few times, then cancel.
    tokio::time::sleep(Duration::from_millis(80)).await;
    let snapshot = counter.load(Ordering::Relaxed);
    assert!(
        snapshot >= 2,
        "preview loop should have ticked at least twice; got {snapshot}"
    );

    let vid: String = sqlx::query_scalar("SELECT id FROM videos LIMIT 1")
        .fetch_one(&pool)
        .await
        .unwrap();
    let aborted = registry.cancel_for_videos(&[VideoId(vid)]);
    assert!(
        !aborted.is_empty(),
        "cancel should have matched at least one job"
    );

    // After cancellation, the counter should stabilize promptly. Observe that it
    // does not grow unbounded.
    tokio::time::sleep(Duration::from_millis(100)).await;
    let after_cancel = counter.load(Ordering::Relaxed);
    tokio::time::sleep(Duration::from_millis(500)).await;
    let later = counter.load(Ordering::Relaxed);
    assert!(
        later <= after_cancel + 1,
        "preview loop continued ticking after cancel: after_cancel={after_cancel} later={later}"
    );
}
