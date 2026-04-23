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

/// A single scan pass plan (either real or dry-run).
#[derive(Debug, Default, Clone, Serialize)]
pub struct ScanReport {
    pub directories_scanned: u32,
    pub files_seen: u64,
    pub new_videos: u64,
    pub changed_videos: u64,
    pub missing_videos: u64,
    pub errors: Vec<String>,
}

/// Kick off a full scan of all non-removed directories in the background.
/// Returns a `ScanHandle` whose task will populate progress as it goes.
pub fn spawn_all(pool: SqlitePool, clock: ClockRef) -> ScanHandle {
    spawn_inner(pool, clock, None)
}

pub fn spawn_one(pool: SqlitePool, clock: ClockRef, dir_id: DirectoryId) -> ScanHandle {
    spawn_inner(pool, clock, Some(dir_id))
}

fn spawn_inner(pool: SqlitePool, clock: ClockRef, only: Option<DirectoryId>) -> ScanHandle {
    let progress = std::sync::Arc::new(ScanProgress::default());
    let p2 = progress.clone();
    let handle: JoinHandle<Result<ScanReport>> = tokio::spawn(async move {
        let res = scan(&pool, &clock, only, &p2).await;
        match &res {
            Ok(report) => {
                p2.phase.store(Phase::Done as u8, Ordering::SeqCst);
                tracing::info!(
                    dirs = report.directories_scanned,
                    files = report.files_seen,
                    new = report.new_videos,
                    changed = report.changed_videos,
                    missing = report.missing_videos,
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
        if let Err(err) = scan_one(pool, clock, &dir, progress, &mut report).await {
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
}

async fn scan_one(
    pool: &SqlitePool,
    clock: &ClockRef,
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
        "SELECT id, relative_path, size_bytes, mtime_unix, missing \
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
        known.insert(
            rel,
            KnownVideo {
                id: VideoId(id),
                size_bytes: size,
                mtime_unix: mtime,
                missing: missing != 0,
            },
        );
    }

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
            }
            Some(k) if k.size_bytes != size || k.mtime_unix != mtime || k.missing => {
                update_changed_video(pool, clock, dir, &k.id, size, mtime, k.missing).await?;
                if !k.missing && (k.size_bytes != size || k.mtime_unix != mtime) {
                    progress.changed_videos.fetch_add(1, Ordering::Relaxed);
                    report.changed_videos += 1;
                }
            }
            Some(_) => {
                // unchanged; no-op
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
pub async fn scan_all(pool: &SqlitePool, clock: &ClockRef) -> Result<ScanReport> {
    let progress = ScanProgress::default();
    scan(pool, clock, None, &progress).await
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

    async fn setup() -> (tempfile::TempDir, SqlitePool, ClockRef) {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = crate::config::Config {
            data_dir: tmp.path().to_path_buf(),
            backup_dir: tmp.path().join("backups"),
            ..crate::config::Config::default()
        };
        let db_path = tmp.path().join("vidviewer.db");
        let pool = crate::db::init(&cfg, &db_path).await.unwrap();
        (tmp, pool, clock::system())
    }

    fn write_video(dir: &Path, name: &str, bytes: &[u8]) {
        std::fs::write(dir.join(name), bytes).unwrap();
    }

    #[tokio::test]
    async fn inserts_new_videos_and_enqueues_probe() {
        let (tmp, pool, clock) = setup().await;
        let videos_dir = tmp.path().join("videos");
        std::fs::create_dir_all(&videos_dir).unwrap();
        write_video(&videos_dir, "a.mp4", b"x");
        write_video(&videos_dir, "b.mkv", b"xx");
        write_video(&videos_dir, "not-a-video.txt", b"skip");

        add_dir(&pool, &clock, &videos_dir, None).await.unwrap();
        let report = scan_all(&pool, &clock).await.unwrap();
        assert_eq!(report.new_videos, 2);
        assert_eq!(report.files_seen, 2, "expected only video files counted");
        assert_eq!(report.changed_videos, 0);

        // Probe jobs enqueued for each new video.
        let (pending, _, _, _) = crate::jobs::count_by_status(&pool).await.unwrap();
        assert_eq!(pending, 2);
    }

    #[tokio::test]
    async fn second_scan_is_noop() {
        let (tmp, pool, clock) = setup().await;
        let videos_dir = tmp.path().join("videos");
        std::fs::create_dir_all(&videos_dir).unwrap();
        write_video(&videos_dir, "a.mp4", b"x");

        add_dir(&pool, &clock, &videos_dir, None).await.unwrap();
        let _ = scan_all(&pool, &clock).await.unwrap();
        let report = scan_all(&pool, &clock).await.unwrap();
        assert_eq!(report.new_videos, 0);
        assert_eq!(report.changed_videos, 0);
        assert_eq!(report.missing_videos, 0);
    }

    #[tokio::test]
    async fn detects_change_and_missing() {
        let (tmp, pool, clock) = setup().await;
        let videos_dir = tmp.path().join("videos");
        std::fs::create_dir_all(&videos_dir).unwrap();
        write_video(&videos_dir, "a.mp4", b"x");
        write_video(&videos_dir, "b.mp4", b"y");

        add_dir(&pool, &clock, &videos_dir, None).await.unwrap();
        let _ = scan_all(&pool, &clock).await.unwrap();

        // Modify a.mp4 and delete b.mp4. Force mtime change.
        std::fs::write(videos_dir.join("a.mp4"), b"xxxx").unwrap();
        let new_mtime = std::time::SystemTime::now();
        filetime::set_file_mtime(
            videos_dir.join("a.mp4"),
            filetime::FileTime::from_system_time(new_mtime + std::time::Duration::from_secs(10)),
        )
        .unwrap();
        std::fs::remove_file(videos_dir.join("b.mp4")).unwrap();

        let report = scan_all(&pool, &clock).await.unwrap();
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
}
