//! Integration test for the job worker pipeline using `MockVideoTool`.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use sqlx::Row;
use tempfile::TempDir;
use vidviewer::{
    clock::{self, ClockRef},
    config::Config,
    db,
    directories::add as add_dir,
    jobs::worker::Workers,
    scanner,
    video_tool::{MockVideoTool, ProbeResult, VideoToolRef},
};

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

#[tokio::test]
async fn probe_enqueues_thumbnail_and_preview() {
    let (tmp, pool, clock, cfg) = setup().await;

    // Create a dummy video file + directory.
    let videos = tmp.path().join("videos");
    std::fs::create_dir_all(&videos).unwrap();
    let video_path = videos.join("sample.mp4");
    std::fs::write(&video_path, b"xx").unwrap();

    add_dir(&pool, &clock, &videos, None).await.unwrap();
    let _ = scanner::scan_all(&pool, &clock).await.unwrap();

    // Pre-seed the mock probe result for this file.
    let mock = MockVideoTool::new();
    mock.set_probe(
        video_path.clone(),
        ProbeResult {
            duration_secs: Some(60.0),
            width: Some(1280),
            height: Some(720),
            codec: Some("h264".into()),
        },
    );
    let video_tool: VideoToolRef = Arc::new(mock);

    let workers = Workers {
        pool: pool.clone(),
        clock: clock.clone(),
        video_tool,
        thumbnail_width: 320,
        preview_min_interval: 2.0,
        preview_target_count: 100,
        thumb_dir: cfg.thumb_cache_dir(),
        preview_dir: cfg.preview_cache_dir(),
    };
    let _handles = workers.spawn_all(1, 1);

    // Poll until all jobs are 'done'. Timeout after a couple of seconds.
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        let row = sqlx::query(
            "SELECT COUNT(*) AS total, \
                SUM(CASE WHEN status='done' THEN 1 ELSE 0 END) AS done, \
                SUM(CASE WHEN status='failed' THEN 1 ELSE 0 END) AS failed \
             FROM jobs",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        let total: i64 = row.get("total");
        let done: i64 = row.get::<Option<i64>, _>("done").unwrap_or(0);
        let failed: i64 = row.get::<Option<i64>, _>("failed").unwrap_or(0);
        assert_eq!(failed, 0, "no job should fail in this test");
        if total >= 3 && done == total {
            break;
        }
        if std::time::Instant::now() > deadline {
            panic!("timed out waiting for jobs; total={total} done={done}");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // Video row should have the probe result persisted and both flags set.
    let row = sqlx::query(
        "SELECT duration_secs, width, height, codec, thumbnail_ok, preview_ok FROM videos",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    let duration: Option<f64> = row.get(0);
    let width: Option<i64> = row.get(1);
    let thumbnail_ok: i64 = row.get("thumbnail_ok");
    let preview_ok: i64 = row.get("preview_ok");
    assert_eq!(duration, Some(60.0));
    assert_eq!(width, Some(1280));
    assert_eq!(thumbnail_ok, 1);
    assert_eq!(preview_ok, 1);

    // Mock writes must have gone to the tempdir, not the user's real cache.
    let thumb = cfg.thumb_cache_dir();
    assert!(thumb.starts_with(tmp.path()));
    assert!(
        thumb.exists(),
        "thumb cache dir wasn't created under tempdir"
    );
    let preview = cfg.preview_cache_dir();
    assert!(preview.starts_with(tmp.path()));
    assert!(
        preview.exists(),
        "preview cache dir wasn't created under tempdir"
    );
}

// The `tmp` argument is kept alive until the end of the test, which keeps the DB
// file (and all cache outputs) on disk long enough for the assertions above.
#[allow(dead_code)]
fn _keep_tmp_alive(_t: &TempDir, _pb: PathBuf) {}
