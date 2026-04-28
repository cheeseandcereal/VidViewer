//! Scanner: walks configured directories, diffs against the DB, and enqueues work.
//!
//! See `docs/design/04-scanner.md` for the algorithm. The scanner is designed to be
//! cheap on no-op runs so it can run at startup safely.
//!
//! This module is split into focused sibling files:
//!   * [`walk`] orchestrates the per-directory stat walk + diff.
//!   * [`mutations`] holds the small-transaction DB helpers (insert/update/mark missing).
//!   * [`verify`] owns the post-walk cache verification pass.
//!   * [`dry_run`] produces a read-only report of what a scan would do.

use std::{
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};

use anyhow::{Context, Result};
use serde::Serialize;
use sqlx::SqlitePool;
use tokio::task::JoinHandle;

use crate::{
    clock::ClockRef,
    directories,
    ids::{DirectoryId, VideoId},
};

pub mod dry_run;
mod mutations;
mod sniff;
mod verify;
mod walk;

pub use dry_run::{dry_run_report, DryRunReport};

/// Filesystem paths to the derived-asset cache. Passed to the scanner so it can verify
/// that flagged-as-done outputs actually exist on disk; if a file is missing (cache was
/// cleared, manual delete, etc.) the scanner clears the flag and re-enqueues the job.
#[derive(Debug, Clone)]
pub struct CachePaths {
    pub thumb: PathBuf,
    pub preview: PathBuf,
}

impl CachePaths {
    pub fn from_config(cfg: &crate::config::Config) -> Self {
        Self {
            thumb: cfg.thumb_cache_dir(),
            preview: cfg.preview_cache_dir(),
        }
    }

    pub fn thumb_path(&self, video_id: &VideoId) -> PathBuf {
        self.thumb.join(format!("{}.jpg", video_id.as_str()))
    }

    pub fn preview_sheet_path(&self, video_id: &VideoId) -> PathBuf {
        self.preview.join(format!("{}.jpg", video_id.as_str()))
    }

    pub fn preview_vtt_path(&self, video_id: &VideoId) -> PathBuf {
        self.preview.join(format!("{}.vtt", video_id.as_str()))
    }
}

/// A single scan pass plan (either real or dry-run).
#[derive(Debug, Default, Clone, Serialize)]
pub struct ScanReport {
    pub directories_scanned: u32,
    pub files_seen: u64,
    pub new_videos: u64,
    pub changed_videos: u64,
    pub missing_videos: u64,
    /// Videos whose thumbnail file was missing on disk; the flag was cleared and a
    /// thumbnail job was re-enqueued.
    pub recovered_thumbnail_jobs: u64,
    /// Videos whose preview tile sheet or VTT file was missing on disk; the flag was
    /// cleared and a preview job was re-enqueued.
    pub recovered_preview_jobs: u64,
    pub errors: Vec<String>,
}

#[derive(Debug, Default)]
pub struct ScanProgress {
    pub phase: std::sync::atomic::AtomicU8,
    pub files_seen: AtomicU64,
    pub new_videos: AtomicU64,
    pub changed_videos: AtomicU64,
    pub missing_videos: AtomicU64,
    pub recovered_thumbnail_jobs: AtomicU64,
    pub recovered_preview_jobs: AtomicU64,
    pub error: std::sync::Mutex<Option<String>>,
}

#[repr(u8)]
#[derive(Clone, Copy)]
pub enum Phase {
    Walking = 0,
    Done = 1,
    Failed = 2,
}

pub struct ScanHandle {
    pub progress: std::sync::Arc<ScanProgress>,
    pub handle: JoinHandle<Result<ScanReport>>,
}

/// Kick off a full scan of all non-removed directories in the background.
pub fn spawn_all(pool: SqlitePool, clock: ClockRef, cache: CachePaths) -> ScanHandle {
    spawn_inner(pool, clock, cache, None)
}

pub fn spawn_one(
    pool: SqlitePool,
    clock: ClockRef,
    cache: CachePaths,
    dir_id: DirectoryId,
) -> ScanHandle {
    spawn_inner(pool, clock, cache, Some(dir_id))
}

fn spawn_inner(
    pool: SqlitePool,
    clock: ClockRef,
    cache: CachePaths,
    only: Option<DirectoryId>,
) -> ScanHandle {
    let progress = std::sync::Arc::new(ScanProgress::default());
    let p2 = progress.clone();
    let handle: JoinHandle<Result<ScanReport>> = tokio::spawn(async move {
        let res = scan(&pool, &clock, &cache, only, &p2).await;
        match &res {
            Ok(report) => {
                p2.phase.store(Phase::Done as u8, Ordering::SeqCst);
                tracing::info!(
                    dirs = report.directories_scanned,
                    files = report.files_seen,
                    new = report.new_videos,
                    changed = report.changed_videos,
                    missing = report.missing_videos,
                    recovered_thumb = report.recovered_thumbnail_jobs,
                    recovered_preview = report.recovered_preview_jobs,
                    "scan complete"
                );
            }
            Err(err) => {
                p2.phase.store(Phase::Failed as u8, Ordering::SeqCst);
                let msg = format!("{err:#}");
                tracing::error!(error = %msg, "scan failed");
                let mut e = p2.error.lock().unwrap();
                *e = Some(msg);
            }
        }
        res
    });
    ScanHandle { progress, handle }
}

/// Perform a full scan synchronously, updating `progress` as it goes.
pub async fn scan(
    pool: &SqlitePool,
    clock: &ClockRef,
    cache: &CachePaths,
    only: Option<DirectoryId>,
    progress: &ScanProgress,
) -> Result<ScanReport> {
    let mut report = ScanReport::default();
    let mut dirs = directories::list(pool, false)
        .await
        .context("listing directories")?;
    if let Some(target) = only {
        dirs.retain(|d| d.id == target);
    }
    report.directories_scanned = dirs.len() as u32;

    for dir in dirs {
        if let Err(err) = walk::scan_one(pool, clock, cache, &dir, progress, &mut report).await {
            let msg = format!("scanning {}: {err:#}", dir.path);
            tracing::error!(directory = %dir.path, error = %format!("{err:#}"), "scan errored");
            report.errors.push(msg);
        }
    }
    Ok(report)
}

/// Convenience: just run a scan to completion, returning the report. Primarily for tests.
pub async fn scan_all(
    pool: &SqlitePool,
    clock: &ClockRef,
    cache: &CachePaths,
) -> Result<ScanReport> {
    let progress = ScanProgress::default();
    scan(pool, clock, cache, None, &progress).await
}

#[cfg(test)]
mod tests {
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
        // Thin wrapper around the shared test-support helper: prepends a
        // minimal MP4 `ftyp` header so content-sniffing accepts the file as
        // video, then appends caller-supplied filler so size can vary.
        crate::test_support::write_video_fixture(dir, name, bytes);
    }

    /// Write a file whose contents are the caller-supplied bytes verbatim —
    /// used for fixtures that should NOT sniff as media (e.g. `.txt`, random
    /// blobs) so tests can verify they're skipped.
    fn write_plain(dir: &Path, name: &str, bytes: &[u8]) {
        std::fs::write(dir.join(name), bytes).unwrap();
    }

    #[tokio::test]
    async fn inserts_new_videos_and_enqueues_probe() {
        let (tmp, pool, clock, cache) = setup().await;
        let videos_dir = tmp.path().join("videos");
        std::fs::create_dir_all(&videos_dir).unwrap();
        write_video(&videos_dir, "a.mp4", b"x");
        write_video(&videos_dir, "b.mkv", b"xx");
        write_plain(&videos_dir, "not-a-video.txt", b"skip");

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
    async fn scan_is_not_recursive() {
        // Files in subdirectories of the configured root must be ignored.
        // Only the top-level is indexed; nested folders are a user's problem
        // to add as their own directories.
        let (tmp, pool, clock, cache) = setup().await;
        let videos_dir = tmp.path().join("videos");
        std::fs::create_dir_all(&videos_dir).unwrap();
        write_video(&videos_dir, "top.mp4", b"x");
        let nested = videos_dir.join("nested");
        std::fs::create_dir_all(&nested).unwrap();
        write_video(&nested, "buried.mp4", b"y");
        let deeper = nested.join("deeper");
        std::fs::create_dir_all(&deeper).unwrap();
        write_video(&deeper, "deep.mp4", b"z");

        add_dir(&pool, &clock, &videos_dir, None).await.unwrap();
        let report = scan_all(&pool, &clock, &cache).await.unwrap();
        assert_eq!(
            report.new_videos, 1,
            "only the top-level video should be indexed"
        );
        assert_eq!(report.files_seen, 1);

        let filenames: Vec<String> = sqlx::query_scalar("SELECT filename FROM videos")
            .fetch_all(&pool)
            .await
            .unwrap();
        assert_eq!(filenames, vec!["top.mp4"]);
    }

    #[tokio::test]
    async fn extensionless_mp3_is_indexed_via_content_sniff() {
        // File has no extension and a made-up name, but its first bytes are a
        // valid ID3v2 header. Content sniffing should still classify it as
        // media and index it.
        let (tmp, pool, clock, cache) = setup().await;
        let videos_dir = tmp.path().join("videos");
        std::fs::create_dir_all(&videos_dir).unwrap();
        let path = videos_dir.join("mystery_track");
        let mut buf = Vec::new();
        // ID3v2.4 tag header: "ID3" + version(2) + flags(1) + size(4).
        buf.extend_from_slice(b"ID3\x04\x00\x00\x00\x00\x00\x00");
        buf.resize(256, 0);
        std::fs::write(&path, &buf).unwrap();
        // Plus a text file that should not be indexed.
        std::fs::write(videos_dir.join("readme"), b"this is not media").unwrap();

        add_dir(&pool, &clock, &videos_dir, None).await.unwrap();
        let report = scan_all(&pool, &clock, &cache).await.unwrap();
        assert_eq!(report.new_videos, 1);
        assert_eq!(report.files_seen, 1);

        let filenames: Vec<String> = sqlx::query_scalar("SELECT filename FROM videos")
            .fetch_all(&pool)
            .await
            .unwrap();
        assert_eq!(filenames, vec!["mystery_track"]);
    }

    #[tokio::test]
    async fn rescan_flags_replaced_non_media_file_as_missing() {
        // A previously-indexed media file, after a content change that turns it
        // into non-media (e.g. truncated / overwritten with a text editor),
        // should be flagged missing on the next rescan so watch history and
        // custom collection references stay intact.
        let (tmp, pool, clock, cache) = setup().await;
        let videos_dir = tmp.path().join("videos");
        std::fs::create_dir_all(&videos_dir).unwrap();
        write_video(&videos_dir, "thing.mp4", b"x");
        add_dir(&pool, &clock, &videos_dir, None).await.unwrap();
        let _ = scan_all(&pool, &clock, &cache).await.unwrap();

        // Overwrite with plain text + bump mtime.
        let path = videos_dir.join("thing.mp4");
        std::fs::write(&path, b"not video content anymore").unwrap();
        let t = std::time::SystemTime::now() + std::time::Duration::from_secs(10);
        filetime::set_file_mtime(&path, filetime::FileTime::from_system_time(t)).unwrap();

        let report = scan_all(&pool, &clock, &cache).await.unwrap();
        assert_eq!(report.new_videos, 0);
        assert_eq!(report.missing_videos, 1);
        let missing: i64 = sqlx::query_scalar("SELECT missing FROM videos LIMIT 1")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(missing, 1);
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

        // Modify a.mp4 (still valid MP4 bytes but different size) and delete
        // b.mp4. Force mtime change.
        write_video(&videos_dir, "a.mp4", b"xxxxxxxx");
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

        // Directory collection should list only the live video on read.
        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM videos v \
             JOIN collections c ON c.kind = 'directory' AND c.directory_id = v.directory_id \
             WHERE v.missing = 0",
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
        let pending_thumb: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM jobs WHERE kind='thumbnail' AND status='pending'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        let pending_preview: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM jobs WHERE kind='preview' AND status='pending'",
        )
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

        let pending_thumb: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM jobs WHERE kind='thumbnail' AND status='pending'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        let pending_preview: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM jobs WHERE kind='preview' AND status='pending'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(pending_thumb, 1);
        assert_eq!(pending_preview, 1);
    }

    #[tokio::test]
    async fn audio_only_without_cover_art_does_not_get_thumbnail_jobs_re_enqueued() {
        // Regression test: audio-only files with no embedded cover art
        // cannot produce a thumbnail. The worker skips them cleanly
        // (returns Ok(()) without setting thumbnail_ok=1), so a naive
        // verify pass would enqueue a fresh thumbnail job on every scan,
        // and the pipeline would loop forever. The verify pass must skip
        // this specific case.
        let (tmp, pool, clock, cache) = setup().await;
        let videos_dir = tmp.path().join("videos");
        std::fs::create_dir_all(&videos_dir).unwrap();
        write_video(&videos_dir, "song.mp3", b"x");

        add_dir(&pool, &clock, &videos_dir, None).await.unwrap();
        let _ = scan_all(&pool, &clock, &cache).await.unwrap();

        // Simulate probe outcome: audio-only, no attached cover art,
        // duration populated. Flags all zero because the worker's
        // thumbnail path skipped (no file produced).
        sqlx::query(
            "UPDATE videos SET is_audio_only = 1, attached_pic_stream_index = NULL, \
             duration_secs = 200.0, codec = 'mp3', \
             thumbnail_ok = 0, preview_ok = 0",
        )
        .execute(&pool)
        .await
        .unwrap();

        // Clear the job queue so the scan we're about to run starts clean.
        sqlx::query("DELETE FROM jobs")
            .execute(&pool)
            .await
            .unwrap();

        // Rescan. Verify must not enqueue a thumbnail job for this row.
        let report = scan_all(&pool, &clock, &cache).await.unwrap();
        assert_eq!(
            report.recovered_thumbnail_jobs, 0,
            "audio-only-no-cover-art rows must not trigger thumbnail re-enqueue"
        );
        assert_eq!(report.recovered_preview_jobs, 0);
        let total_jobs: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM jobs")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(total_jobs, 0, "no jobs should have been enqueued");
    }

    #[tokio::test]
    async fn audio_only_with_cover_art_still_recovers_thumbnail_after_cache_wipe() {
        // Audio-only rows *with* cover art must still be eligible for
        // thumbnail regeneration if the cache file is missing.
        let (tmp, pool, clock, cache) = setup().await;
        let videos_dir = tmp.path().join("videos");
        std::fs::create_dir_all(&videos_dir).unwrap();
        write_video(&videos_dir, "album.flac", b"x");

        add_dir(&pool, &clock, &videos_dir, None).await.unwrap();
        let _ = scan_all(&pool, &clock, &cache).await.unwrap();

        // Audio-only with cover art at stream 1, previously-successful
        // thumbnail (thumbnail_ok=1) but no file on disk (cache wiped).
        sqlx::query(
            "UPDATE videos SET is_audio_only = 1, attached_pic_stream_index = 1, \
             duration_secs = 240.0, codec = 'flac', \
             thumbnail_ok = 1, preview_ok = 0",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query("DELETE FROM jobs")
            .execute(&pool)
            .await
            .unwrap();

        let report = scan_all(&pool, &clock, &cache).await.unwrap();
        assert_eq!(
            report.recovered_thumbnail_jobs, 1,
            "audio-only with cover art must re-enqueue when cache is missing"
        );
        let pending_thumb: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM jobs WHERE kind = 'thumbnail'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(pending_thumb, 1);
    }
}
