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
    scanner::{self, CachePaths},
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
    vidviewer::test_support::write_video_fixture(&videos, "sample.mp4", b"xx");

    add_dir(&pool, &clock, &videos, None).await.unwrap();
    let cache = CachePaths::from_config(&cfg);
    let _ = scanner::scan_all(&pool, &clock, &cache).await.unwrap();

    // Pre-seed the mock probe result for this file.
    let mock = MockVideoTool::new();
    mock.set_probe(
        video_path.clone(),
        ProbeResult {
            duration_secs: Some(60.0),
            width: Some(1280),
            height: Some(720),
            codec: Some("h264".into()),
            is_audio_only: false,
            attached_pic_stream_index: None,
        },
    );
    let video_tool: VideoToolRef = Arc::new(mock);

    let workers = Workers {
        pool: pool.clone(),
        clock: clock.clone(),
        config: std::sync::Arc::new(Config {
            thumbnail_width: 320,
            preview_min_interval: 2.0,
            preview_target_count: 100,
            ..cfg.clone()
        }),
        video_tool,
        thumb_dir: cfg.thumb_cache_dir(),
        preview_dir: cfg.preview_cache_dir(),
        registry: vidviewer::jobs::registry::JobRegistry::new(),
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

#[tokio::test]
async fn audio_only_file_skips_preview_and_uses_cover_art() {
    let (tmp, pool, clock, cfg) = setup().await;

    let videos = tmp.path().join("videos");
    std::fs::create_dir_all(&videos).unwrap();
    let song_path = videos.join("song.mp3");
    vidviewer::test_support::write_video_fixture(&videos, "song.mp3", b"tunes");

    add_dir(&pool, &clock, &videos, None).await.unwrap();
    let cache = CachePaths::from_config(&cfg);
    let _ = scanner::scan_all(&pool, &clock, &cache).await.unwrap();

    let mock = MockVideoTool::new();
    // Audio-only with embedded cover art at stream index 1.
    mock.set_probe(
        song_path.clone(),
        ProbeResult {
            duration_secs: Some(201.0),
            width: None,
            height: None,
            codec: Some("mp3".into()),
            is_audio_only: true,
            attached_pic_stream_index: Some(1),
        },
    );
    let video_tool: VideoToolRef = Arc::new(mock.clone());

    let workers = Workers {
        pool: pool.clone(),
        clock: clock.clone(),
        config: std::sync::Arc::new(Config {
            thumbnail_width: 320,
            ..cfg.clone()
        }),
        video_tool,
        thumb_dir: cfg.thumb_cache_dir(),
        preview_dir: cfg.preview_cache_dir(),
        registry: vidviewer::jobs::registry::JobRegistry::new(),
    };
    let _handles = workers.spawn_all(1, 1);

    // Wait for the probe + thumbnail jobs to settle. Total should be exactly
    // 2 (no preview), both done, none failed.
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
        assert_eq!(failed, 0);
        if total >= 2 && done == total {
            break;
        }
        if std::time::Instant::now() > deadline {
            panic!("timed out waiting for jobs; total={total} done={done}");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // Exactly two jobs existed (probe + thumbnail). No preview job.
    let total: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM jobs")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(total, 2, "audio-only should produce probe + thumbnail only");
    let previews: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM jobs WHERE kind = 'preview'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(previews, 0, "audio-only must not enqueue a preview job");

    // Video row: is_audio_only = 1, preview_ok = 0, thumbnail_ok = 1.
    let row = sqlx::query(
        "SELECT is_audio_only, thumbnail_ok, preview_ok, codec, \
                attached_pic_stream_index FROM videos",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(row.get::<i64, _>("is_audio_only"), 1);
    assert_eq!(row.get::<i64, _>("thumbnail_ok"), 1);
    assert_eq!(row.get::<i64, _>("preview_ok"), 0);
    assert_eq!(
        row.get::<Option<String>, _>("codec").as_deref(),
        Some("mp3")
    );
    assert_eq!(
        row.get::<Option<i64>, _>("attached_pic_stream_index"),
        Some(1)
    );

    // The thumbnail call should have been made with Some(stream_index).
    use vidviewer::video_tool::MockCall;
    let calls = mock.calls();
    let cover_art_call = calls.iter().find(|c| {
        matches!(
            c,
            MockCall::Thumbnail {
                stream_index: Some(1),
                ..
            }
        )
    });
    assert!(
        cover_art_call.is_some(),
        "expected thumbnail to be invoked with stream_index=Some(1); got calls: {calls:#?}"
    );
}

#[tokio::test]
async fn run_preview_skips_audio_only_row_without_failing() {
    // Defense-in-depth test: if a preview job somehow makes it to the
    // worker for an is_audio_only=1 row (e.g. pre-existing pending row
    // post-upgrade, a race, a future bug), the worker must skip it
    // cleanly and mark it done — not fail against a file with no video.
    let (tmp, pool, clock, cfg) = setup().await;

    let videos = tmp.path().join("videos");
    std::fs::create_dir_all(&videos).unwrap();
    vidviewer::test_support::write_video_fixture(&videos, "song.mp3", b"bytes");
    add_dir(&pool, &clock, &videos, None).await.unwrap();
    let cache = CachePaths::from_config(&cfg);
    let _ = scanner::scan_all(&pool, &clock, &cache).await.unwrap();

    // Mark the row as audio-only with a known duration, and delete the
    // probe job the scanner enqueued so nothing else runs first.
    sqlx::query(
        "UPDATE videos SET duration_secs = 10.0, is_audio_only = 1, \
             width = NULL, height = NULL, codec = 'mp3'",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("DELETE FROM jobs")
        .execute(&pool)
        .await
        .unwrap();
    let video_id: String = sqlx::query_scalar("SELECT id FROM videos")
        .fetch_one(&pool)
        .await
        .unwrap();

    // Inject a preview job directly, bypassing the probe-time gate.
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

    let mock = MockVideoTool::new();
    let video_tool: VideoToolRef = Arc::new(mock.clone());

    let workers = Workers {
        pool: pool.clone(),
        clock: clock.clone(),
        config: std::sync::Arc::new(cfg.clone()),
        video_tool,
        thumb_dir: cfg.thumb_cache_dir(),
        preview_dir: cfg.preview_cache_dir(),
        registry: vidviewer::jobs::registry::JobRegistry::new(),
    };
    let _handles = workers.spawn_all(1, 1);

    // Wait for the job to transition out of pending. It must go to done,
    // not failed.
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        let row =
            sqlx::query("SELECT status FROM jobs WHERE video_id = ? AND kind = 'preview' LIMIT 1")
                .bind(&video_id)
                .fetch_optional(&pool)
                .await
                .unwrap();
        let status: Option<String> = row.as_ref().map(|r| r.get("status"));
        match status.as_deref() {
            Some("done") => break,
            Some("failed") => panic!("preview job must not fail for audio-only row"),
            _ => {}
        }
        if std::time::Instant::now() > deadline {
            panic!("preview job never completed; status={status:?}");
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    // No preview ffmpeg invocation should have happened on the mock.
    use vidviewer::video_tool::MockCall;
    let preview_calls = mock
        .calls()
        .into_iter()
        .filter(|c| matches!(c, MockCall::Preview { .. }))
        .count();
    assert_eq!(
        preview_calls, 0,
        "no preview work should have been attempted"
    );
}
