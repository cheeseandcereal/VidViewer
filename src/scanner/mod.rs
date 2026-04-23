//! Scanner: walks configured directories, diffs against the DB, and enqueues work.
//!
//! See `docs/design/04-scanner.md` for the algorithm. The scanner is designed to be
//! cheap on no-op runs so it can run at startup safely.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use anyhow::{Context, Result};
use chrono::Utc;
use serde::Serialize;
use sqlx::{Row, SqlitePool};
use tokio::task::JoinHandle;
use walkdir::WalkDir;

use crate::{
    clock::ClockRef,
    directories,
    ids::{CollectionId, DirectoryId, VideoId},
    jobs,
};

/// Allowed video file extensions (lowercase, no dot).
pub const VIDEO_EXTENSIONS: &[&str] = &[
    "mp4", "mkv", "webm", "mov", "avi", "m4v", "flv", "wmv", "mpg", "mpeg", "ts", "m2ts",
];

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

/// Kick off a full scan of all non-removed directories in the background.
/// Returns a `ScanHandle` whose task will populate progress as it goes.
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
        if let Err(err) = scan_one(pool, clock, cache, &dir, progress, &mut report).await {
            let msg = format!("scanning {}: {err:#}", dir.path);
            tracing::error!(directory = %dir.path, error = %format!("{err:#}"), "scan errored");
            report.errors.push(msg);
        }
    }
    Ok(report)
}

/// Existing video row projection used for diffing.
#[derive(Debug, Clone)]
struct KnownVideo {
    id: VideoId,
    size_bytes: i64,
    mtime_unix: i64,
    missing: bool,
    thumbnail_ok: bool,
    preview_ok: bool,
    duration_secs: Option<f64>,
}

async fn scan_one(
    pool: &SqlitePool,
    clock: &ClockRef,
    cache: &CachePaths,
    dir: &directories::Directory,
    progress: &ScanProgress,
    report: &mut ScanReport,
) -> Result<()> {
    tracing::info!(dir = %dir.path, label = %dir.label, "scanning");
    let root = PathBuf::from(&dir.path);
    if !root.exists() {
        tracing::warn!(path = %dir.path, "directory does not exist on disk; skipping");
        return Ok(());
    }

    // 1. Load known videos into memory.
    let rows = sqlx::query(
        "SELECT id, relative_path, size_bytes, mtime_unix, missing, \
            thumbnail_ok, preview_ok, duration_secs \
         FROM videos WHERE directory_id = ?",
    )
    .bind(dir.id.raw())
    .fetch_all(pool)
    .await
    .context("loading known videos")?;

    let mut known: HashMap<String, KnownVideo> = HashMap::with_capacity(rows.len());
    for row in rows {
        let id: String = row.get("id");
        let rel: String = row.get("relative_path");
        let size: i64 = row.get("size_bytes");
        let mtime: i64 = row.get("mtime_unix");
        let missing: i64 = row.get("missing");
        let thumbnail_ok: i64 = row.get("thumbnail_ok");
        let preview_ok: i64 = row.get("preview_ok");
        let duration_secs: Option<f64> = row.get("duration_secs");
        known.insert(
            rel,
            KnownVideo {
                id: VideoId(id),
                size_bytes: size,
                mtime_unix: mtime,
                missing: missing != 0,
                thumbnail_ok: thumbnail_ok != 0,
                preview_ok: preview_ok != 0,
                duration_secs,
            },
        );
    }

    // Collect videos that survive the walk (unchanged or updated) so we can verify
    // their cache outputs at the end. We keep a (VideoId, thumbnail_ok, preview_ok,
    // duration_secs) snapshot — post-walk DB state for those flags is consistent with
    // what we saw, since the only mutation path that clears flags (change detected)
    // is self-contained in `update_changed_video`.
    let mut surviving: Vec<(VideoId, bool, bool, Option<f64>)> = Vec::new();

    // 2. Walk the directory.
    for entry in WalkDir::new(&root)
        .follow_links(true)
        .into_iter()
        .filter_map(|r| r.ok())
    {
        if !entry.file_type().is_file() {
            continue;
        }
        if !is_video_extension(entry.path()) {
            continue;
        }
        progress.files_seen.fetch_add(1, Ordering::Relaxed);
        report.files_seen += 1;

        let rel = match entry.path().strip_prefix(&root) {
            Ok(p) => crate::util::path::path_to_db_string(p),
            Err(_) => continue,
        };

        let meta = match entry.metadata() {
            Ok(m) => m,
            Err(err) => {
                tracing::warn!(path = %entry.path().display(), error = %err, "stat failed");
                continue;
            }
        };
        let size = meta.len() as i64;
        let mtime = mtime_to_unix(&meta);
        let filename = entry.file_name().to_string_lossy().into_owned();

        let entry_known = known.remove(&rel);

        match entry_known {
            None => {
                insert_new_video(pool, clock, dir, &rel, &filename, size, mtime).await?;
                progress.new_videos.fetch_add(1, Ordering::Relaxed);
                report.new_videos += 1;
                // Newly-inserted videos haven't generated anything yet; the probe job
                // is already queued and will enqueue thumbnail+preview on completion.
                // Skip the cache verification for them below.
            }
            Some(k) if k.size_bytes != size || k.mtime_unix != mtime => {
                // Content changed on disk: clear flags, re-enqueue probe.
                update_changed_video(pool, clock, dir, &k.id, size, mtime, k.missing).await?;
                if !k.missing {
                    progress.changed_videos.fetch_add(1, Ordering::Relaxed);
                    report.changed_videos += 1;
                }
                // Skip cache verification — the probe's follow-up jobs cover regen.
            }
            Some(k) if k.missing => {
                // Un-missing without content change: preserve flags, re-insert the
                // directory collection membership. The post-walk cache verification
                // pass will detect any missing cache files and re-enqueue only the
                // jobs that are actually needed.
                un_mark_missing(pool, clock, dir, &k.id).await?;
                surviving.push((k.id, k.thumbnail_ok, k.preview_ok, k.duration_secs));
            }
            Some(k) => {
                // Unchanged: verify cache outputs at the end.
                surviving.push((k.id, k.thumbnail_ok, k.preview_ok, k.duration_secs));
            }
        }
    }

    // 3. Anything still in `known` wasn't found on disk.
    for (rel, k) in known.into_iter() {
        if k.missing {
            continue;
        }
        mark_missing(pool, clock, dir, &k.id).await?;
        progress.missing_videos.fetch_add(1, Ordering::Relaxed);
        report.missing_videos += 1;
        tracing::info!(
            video_id = %k.id,
            relative_path = %rel,
            "marked missing"
        );
    }

    // 4. Verify cache files for unchanged videos. Re-enqueue jobs whose outputs
    //    have been deleted (manual cache wipe, disk swap, etc.). We deliberately
    //    check this only for videos that remain on disk in this scan pass.
    for (video_id, thumbnail_ok, preview_ok, duration_secs) in surviving {
        verify_cache_for_video(
            pool,
            clock,
            cache,
            &video_id,
            thumbnail_ok,
            preview_ok,
            duration_secs,
            progress,
            report,
        )
        .await?;
    }

    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn verify_cache_for_video(
    pool: &SqlitePool,
    clock: &ClockRef,
    cache: &CachePaths,
    video_id: &VideoId,
    thumbnail_ok: bool,
    preview_ok: bool,
    duration_secs: Option<f64>,
    progress: &ScanProgress,
    report: &mut ScanReport,
) -> Result<()> {
    // Thumbnail: expected if thumbnail_ok is set.
    if thumbnail_ok {
        let p = cache.thumb_path(video_id);
        if !p.exists() {
            let now_s = clock.now().to_rfc3339();
            let mut tx = pool.begin().await.context("begin tx")?;
            sqlx::query("UPDATE videos SET thumbnail_ok = 0, updated_at = ? WHERE id = ?")
                .bind(&now_s)
                .bind(video_id.as_str())
                .execute(&mut *tx)
                .await
                .context("clearing thumbnail_ok")?;
            jobs::enqueue_on(&mut tx, jobs::Kind::Thumbnail, video_id).await?;
            tx.commit().await.context("commit tx")?;
            progress
                .recovered_thumbnail_jobs
                .fetch_add(1, Ordering::Relaxed);
            report.recovered_thumbnail_jobs += 1;
            tracing::info!(
                video_id = %video_id,
                missing_cache = %p.display(),
                "thumbnail cache missing; cleared flag and re-enqueued"
            );
        }
    }

    // Preview: expected if preview_ok is set AND duration is usable.
    if preview_ok && duration_secs.unwrap_or(0.0) > 0.0 {
        let sheet = cache.preview_sheet_path(video_id);
        let vtt = cache.preview_vtt_path(video_id);
        if !sheet.exists() || !vtt.exists() {
            let now_s = clock.now().to_rfc3339();
            let mut tx = pool.begin().await.context("begin tx")?;
            sqlx::query("UPDATE videos SET preview_ok = 0, updated_at = ? WHERE id = ?")
                .bind(&now_s)
                .bind(video_id.as_str())
                .execute(&mut *tx)
                .await
                .context("clearing preview_ok")?;
            jobs::enqueue_on(&mut tx, jobs::Kind::Preview, video_id).await?;
            tx.commit().await.context("commit tx")?;
            progress
                .recovered_preview_jobs
                .fetch_add(1, Ordering::Relaxed);
            report.recovered_preview_jobs += 1;
            tracing::info!(
                video_id = %video_id,
                missing_sheet = %sheet.display(),
                missing_vtt = %vtt.display(),
                "preview cache missing; cleared flag and re-enqueued"
            );
        }
    }

    Ok(())
}

fn is_video_extension(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|ext| {
            VIDEO_EXTENSIONS
                .iter()
                .any(|&v| v.eq_ignore_ascii_case(ext))
        })
        .unwrap_or(false)
}

fn mtime_to_unix(meta: &std::fs::Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

async fn insert_new_video(
    pool: &SqlitePool,
    clock: &ClockRef,
    dir: &directories::Directory,
    rel: &str,
    filename: &str,
    size: i64,
    mtime: i64,
) -> Result<()> {
    let now_s = clock.now().to_rfc3339();
    let video_id = VideoId::new_random();

    let mut tx = pool.begin().await.context("begin tx")?;

    sqlx::query(
        "INSERT INTO videos (id, directory_id, relative_path, filename, size_bytes, mtime_unix, \
            thumbnail_ok, preview_ok, missing, created_at, updated_at) \
         VALUES (?, ?, ?, ?, ?, ?, 0, 0, 0, ?, ?)",
    )
    .bind(video_id.as_str())
    .bind(dir.id.raw())
    .bind(rel)
    .bind(filename)
    .bind(size)
    .bind(mtime)
    .bind(&now_s)
    .bind(&now_s)
    .execute(&mut *tx)
    .await
    .context("inserting video")?;

    add_to_directory_collection(&mut tx, dir.collection_id, &video_id, &now_s).await?;

    jobs::enqueue_on(&mut tx, jobs::Kind::Probe, &video_id).await?;

    tx.commit().await.context("commit tx")?;

    tracing::debug!(video_id = %video_id, path = %rel, "new video indexed");
    Ok(())
}

async fn update_changed_video(
    pool: &SqlitePool,
    clock: &ClockRef,
    dir: &directories::Directory,
    video_id: &VideoId,
    size: i64,
    mtime: i64,
    was_missing: bool,
) -> Result<()> {
    let now_s = clock.now().to_rfc3339();
    let mut tx = pool.begin().await.context("begin tx")?;

    sqlx::query(
        "UPDATE videos SET size_bytes = ?, mtime_unix = ?, \
            thumbnail_ok = 0, preview_ok = 0, missing = 0, updated_at = ? \
         WHERE id = ?",
    )
    .bind(size)
    .bind(mtime)
    .bind(&now_s)
    .bind(video_id.as_str())
    .execute(&mut *tx)
    .await
    .context("updating changed video")?;

    if was_missing {
        add_to_directory_collection(&mut tx, dir.collection_id, video_id, &now_s).await?;
    }

    jobs::enqueue_on(&mut tx, jobs::Kind::Probe, video_id).await?;

    tx.commit().await.context("commit tx")?;
    Ok(())
}

async fn mark_missing(
    pool: &SqlitePool,
    clock: &ClockRef,
    dir: &directories::Directory,
    video_id: &VideoId,
) -> Result<()> {
    let now_s = clock.now().to_rfc3339();
    let mut tx = pool.begin().await.context("begin tx")?;

    sqlx::query("UPDATE videos SET missing = 1, updated_at = ? WHERE id = ?")
        .bind(&now_s)
        .bind(video_id.as_str())
        .execute(&mut *tx)
        .await
        .context("flagging missing")?;

    sqlx::query("DELETE FROM collection_videos WHERE collection_id = ? AND video_id = ?")
        .bind(dir.collection_id.raw())
        .bind(video_id.as_str())
        .execute(&mut *tx)
        .await
        .context("removing from directory collection")?;

    tx.commit().await.context("commit tx")?;
    let _ = clock; // suppress unused-warning if above path becomes no-op
    Ok(())
}

/// Un-mark a video as missing without touching `thumbnail_ok` / `preview_ok`.
///
/// Used on re-add of a soft-removed directory when the file's size and mtime match
/// the stored row: we only need to flip `missing = 0`, touch `updated_at`, and
/// re-insert the directory-collection membership. The post-walk cache verification
/// pass then detects any missing cache files and re-enqueues only what's needed.
async fn un_mark_missing(
    pool: &SqlitePool,
    clock: &ClockRef,
    dir: &directories::Directory,
    video_id: &VideoId,
) -> Result<()> {
    let now_s = clock.now().to_rfc3339();
    let mut tx = pool.begin().await.context("begin tx")?;

    sqlx::query("UPDATE videos SET missing = 0, updated_at = ? WHERE id = ?")
        .bind(&now_s)
        .bind(video_id.as_str())
        .execute(&mut *tx)
        .await
        .context("clearing missing flag")?;

    add_to_directory_collection(&mut tx, dir.collection_id, video_id, &now_s).await?;

    tx.commit().await.context("commit tx")?;
    Ok(())
}

async fn add_to_directory_collection(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    collection_id: CollectionId,
    video_id: &VideoId,
    now_s: &str,
) -> Result<()> {
    sqlx::query(
        "INSERT OR IGNORE INTO collection_videos (collection_id, video_id, added_at) \
         VALUES (?, ?, ?)",
    )
    .bind(collection_id.raw())
    .bind(video_id.as_str())
    .bind(now_s)
    .execute(&mut **tx)
    .await
    .context("adding to directory collection")?;
    Ok(())
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

/// Produce a textual dry-run report for a directory. Does not write anything.
pub async fn dry_run_report(pool: &SqlitePool, only: Option<DirectoryId>) -> Result<DryRunReport> {
    let mut out = DryRunReport::default();
    let mut dirs = directories::list(pool, false).await?;
    if let Some(target) = only {
        dirs.retain(|d| d.id == target);
    }
    for dir in dirs {
        let root = PathBuf::from(&dir.path);
        if !root.exists() {
            out.missing_directories.push(dir.path.clone());
            continue;
        }
        let rows = sqlx::query(
            "SELECT relative_path, size_bytes, mtime_unix, missing FROM videos WHERE directory_id = ?",
        )
        .bind(dir.id.raw())
        .fetch_all(pool)
        .await?;
        let mut known: HashMap<String, (i64, i64, bool)> = HashMap::with_capacity(rows.len());
        for row in rows {
            let rel: String = row.get("relative_path");
            let size: i64 = row.get("size_bytes");
            let mtime: i64 = row.get("mtime_unix");
            let missing: i64 = row.get("missing");
            known.insert(rel, (size, mtime, missing != 0));
        }

        for entry in WalkDir::new(&root)
            .follow_links(true)
            .into_iter()
            .filter_map(|r| r.ok())
        {
            if !entry.file_type().is_file() || !is_video_extension(entry.path()) {
                continue;
            }
            let rel = match entry.path().strip_prefix(&root) {
                Ok(p) => crate::util::path::path_to_db_string(p),
                Err(_) => continue,
            };
            let meta = match entry.metadata() {
                Ok(m) => m,
                Err(_) => continue,
            };
            let size = meta.len() as i64;
            let mtime = mtime_to_unix(&meta);
            out.seen_files += 1;

            match known.remove(&rel) {
                None => out.would_insert.push(rel),
                Some((s, m, missing)) if s != size || m != mtime || missing => {
                    out.would_update.push(rel);
                }
                Some(_) => {}
            }
        }
        for (rel, (_, _, missing)) in known.into_iter() {
            if !missing {
                out.would_mark_missing.push(rel);
            }
        }
    }
    let _ = Utc::now();
    Ok(out)
}

#[derive(Debug, Default, Clone, Serialize)]
pub struct DryRunReport {
    pub seen_files: u64,
    pub would_insert: Vec<String>,
    pub would_update: Vec<String>,
    pub would_mark_missing: Vec<String>,
    pub missing_directories: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::{self};
    use crate::directories::add as add_dir;

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
}
