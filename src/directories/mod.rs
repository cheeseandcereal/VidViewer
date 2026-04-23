//! Directory records and CRUD operations.
//!
//! Directories represent top-level roots configured by the user. Each has a `directory`
//! collection auto-created alongside it (materialized membership, maintained by the scanner).
//!
//! See `docs/design/04-scanner.md` and `docs/design/07-collections.md`.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Utc};
use serde::Serialize;
use sqlx::{Row, SqlitePool};
use thiserror::Error;

use crate::{
    clock::ClockRef,
    ids::{CollectionId, DirectoryId, VideoId},
};

/// A directory record as stored in the `directories` table.
#[derive(Debug, Clone, Serialize)]
pub struct Directory {
    pub id: DirectoryId,
    pub path: String,
    pub label: String,
    pub added_at: DateTime<Utc>,
    pub removed: bool,
    /// Included for UI convenience; filled in by list()/get().
    pub video_count: i64,
    /// Collection id for the directory collection backing this directory.
    pub collection_id: CollectionId,
}

#[derive(Debug, Clone, Error, Serialize)]
#[serde(tag = "error", rename_all = "snake_case")]
pub enum AddError {
    #[error("path must be absolute")]
    PathNotAbsolute,
    #[error("path does not exist")]
    PathNotFound,
    #[error("path is not a directory")]
    PathNotADirectory,
    #[error("path is not readable")]
    PathNotReadable,
    #[error("path is already added")]
    PathAlreadyAdded,
    #[error("internal error: {message}")]
    Internal { message: String },
}

impl AddError {
    pub fn status(&self) -> axum::http::StatusCode {
        use axum::http::StatusCode;
        match self {
            AddError::Internal { .. } => StatusCode::INTERNAL_SERVER_ERROR,
            AddError::PathAlreadyAdded => StatusCode::CONFLICT,
            _ => StatusCode::BAD_REQUEST,
        }
    }
}

pub fn validate_path(path: &Path) -> Result<PathBuf, AddError> {
    if !path.is_absolute() {
        return Err(AddError::PathNotAbsolute);
    }
    let meta = match std::fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Err(AddError::PathNotFound);
        }
        Err(err) if err.kind() == std::io::ErrorKind::PermissionDenied => {
            return Err(AddError::PathNotReadable);
        }
        Err(err) => {
            return Err(AddError::Internal {
                message: err.to_string(),
            });
        }
    };
    // Follow through if this is a symlink.
    let resolved = std::fs::metadata(path).unwrap_or(meta);
    if !resolved.is_dir() {
        return Err(AddError::PathNotADirectory);
    }
    // Attempt to actually read the directory.
    match std::fs::read_dir(path) {
        Ok(_) => Ok(path.to_path_buf()),
        Err(err) if err.kind() == std::io::ErrorKind::PermissionDenied => {
            Err(AddError::PathNotReadable)
        }
        Err(err) => Err(AddError::Internal {
            message: err.to_string(),
        }),
    }
}

/// Add a directory. If a row with the same path already exists:
///   - if `removed = 1`, un-remove (and un-hide its directory collection), keeping the
///     existing collection's `name`.
///   - if `removed = 0`, return `PathAlreadyAdded`.
pub async fn add(
    pool: &SqlitePool,
    clock: &ClockRef,
    path: &Path,
    label: Option<String>,
) -> Result<Directory, AddError> {
    let path = validate_path(path)?;
    let path_str = crate::util::path::path_to_db_string(&path);
    let now_s = clock.now().to_rfc3339();

    let mut tx = pool.begin().await.map_err(internal)?;

    // Does a row already exist for this path?
    let existing = sqlx::query("SELECT id, removed FROM directories WHERE path = ?")
        .bind(&path_str)
        .fetch_optional(&mut *tx)
        .await
        .map_err(internal)?;

    let dir_id: i64 = if let Some(row) = existing {
        let id: i64 = row.get(0);
        let removed: i64 = row.get(1);
        if removed == 0 {
            return Err(AddError::PathAlreadyAdded);
        }
        // Un-remove, optionally update label.
        if let Some(lbl) = &label {
            sqlx::query("UPDATE directories SET removed = 0, label = ? WHERE id = ?")
                .bind(lbl)
                .bind(id)
                .execute(&mut *tx)
                .await
                .map_err(internal)?;
        } else {
            sqlx::query("UPDATE directories SET removed = 0 WHERE id = ?")
                .bind(id)
                .execute(&mut *tx)
                .await
                .map_err(internal)?;
        }
        // Un-hide the existing directory collection, keep its name.
        sqlx::query(
            "UPDATE collections SET hidden = 0, updated_at = ? \
             WHERE kind = 'directory' AND directory_id = ?",
        )
        .bind(&now_s)
        .bind(id)
        .execute(&mut *tx)
        .await
        .map_err(internal)?;
        id
    } else {
        let effective_label = label.unwrap_or_else(|| path_str.clone());
        let id: i64 =
            sqlx::query("INSERT INTO directories (path, label, added_at, removed) VALUES (?, ?, ?, 0) RETURNING id")
                .bind(&path_str)
                .bind(&effective_label)
                .bind(&now_s)
                .fetch_one(&mut *tx)
                .await
                .map_err(internal)?
                .get(0);
        // Create directory collection with name = label by default.
        sqlx::query(
            "INSERT INTO collections (name, kind, directory_id, hidden, created_at, updated_at) \
             VALUES (?, 'directory', ?, 0, ?, ?)",
        )
        .bind(&effective_label)
        .bind(id)
        .bind(&now_s)
        .bind(&now_s)
        .execute(&mut *tx)
        .await
        .map_err(internal)?;
        id
    };

    tx.commit().await.map_err(internal)?;
    get(pool, DirectoryId(dir_id))
        .await
        .map_err(|e| AddError::Internal {
            message: e.to_string(),
        })?
        .ok_or_else(|| AddError::Internal {
            message: "directory not found after insert".into(),
        })
}

fn internal<E: std::fmt::Display>(e: E) -> AddError {
    AddError::Internal {
        message: e.to_string(),
    }
}

pub async fn list(pool: &SqlitePool, include_removed: bool) -> Result<Vec<Directory>> {
    let sql = if include_removed {
        LIST_SQL_ALL
    } else {
        LIST_SQL_ACTIVE
    };
    let rows = sqlx::query(sql)
        .fetch_all(pool)
        .await
        .context("listing directories")?;
    let mut out = Vec::with_capacity(rows.len());
    for r in rows {
        out.push(row_to_directory(&r)?);
    }
    Ok(out)
}

pub async fn get(pool: &SqlitePool, id: DirectoryId) -> Result<Option<Directory>> {
    let row = sqlx::query(LIST_SQL_BY_ID)
        .bind(id.raw())
        .fetch_optional(pool)
        .await
        .context("fetching directory")?;
    match row {
        Some(r) => Ok(Some(row_to_directory(&r)?)),
        None => Ok(None),
    }
}

/// Update the label. Also propagates to the directory collection's `name`.
pub async fn set_label(
    pool: &SqlitePool,
    clock: &ClockRef,
    id: DirectoryId,
    label: &str,
) -> Result<Directory> {
    let now_s = clock.now().to_rfc3339();
    let mut tx = pool.begin().await.context("begin tx")?;
    let affected = sqlx::query("UPDATE directories SET label = ? WHERE id = ?")
        .bind(label)
        .bind(id.raw())
        .execute(&mut *tx)
        .await
        .context("updating directory label")?
        .rows_affected();
    if affected == 0 {
        bail!("directory id {id} not found");
    }
    sqlx::query(
        "UPDATE collections SET name = ?, updated_at = ? \
         WHERE kind = 'directory' AND directory_id = ?",
    )
    .bind(label)
    .bind(&now_s)
    .bind(id.raw())
    .execute(&mut *tx)
    .await
    .context("updating directory collection name")?;
    tx.commit().await.context("commit tx")?;
    get(pool, id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("directory {id} not found after update"))
}

/// Soft-remove the directory:
///   * mark `directories.removed = 1`
///   * mark the directory collection `hidden = 1`
///   * delete `collection_videos` rows for that collection (so on re-add, scanner repopulates)
///   * mark all videos in this directory `missing = 1`
///   * cancel any pending background jobs for videos in this directory
///
/// Watch history and custom collection memberships are preserved. Jobs already in the
/// `running` state are allowed to finish naturally — the wasted work is bounded and
/// cancelling mid-ffmpeg would require process tracking that isn't worth the complexity
/// for this case.
pub async fn soft_remove(pool: &SqlitePool, clock: &ClockRef, id: DirectoryId) -> Result<()> {
    let now_s = clock.now().to_rfc3339();
    let mut tx = pool.begin().await.context("begin tx")?;
    let affected = sqlx::query("UPDATE directories SET removed = 1 WHERE id = ?")
        .bind(id.raw())
        .execute(&mut *tx)
        .await
        .context("flagging directory removed")?
        .rows_affected();
    if affected == 0 {
        bail!("directory id {id} not found");
    }
    // Find the directory's collection id.
    let coll_id: Option<i64> =
        sqlx::query("SELECT id FROM collections WHERE kind = 'directory' AND directory_id = ?")
            .bind(id.raw())
            .fetch_optional(&mut *tx)
            .await
            .context("fetching directory collection id")?
            .map(|r| r.get(0));

    if let Some(cid) = coll_id {
        sqlx::query("DELETE FROM collection_videos WHERE collection_id = ?")
            .bind(cid)
            .execute(&mut *tx)
            .await
            .context("clearing directory collection memberships")?;
        sqlx::query("UPDATE collections SET hidden = 1, updated_at = ? WHERE id = ?")
            .bind(&now_s)
            .bind(cid)
            .execute(&mut *tx)
            .await
            .context("hiding directory collection")?;
    }

    sqlx::query("UPDATE videos SET missing = 1, updated_at = ? WHERE directory_id = ?")
        .bind(&now_s)
        .bind(id.raw())
        .execute(&mut *tx)
        .await
        .context("marking videos missing")?;

    // Cancel pending jobs for videos in this directory. Rows with status = 'running'
    // are left alone; the worker will complete them on a now-missing video, which is
    // harmless — flags on a missing row are not shown in the UI.
    let cancelled = sqlx::query(
        "DELETE FROM jobs \
         WHERE status = 'pending' \
         AND video_id IN (SELECT id FROM videos WHERE directory_id = ?)",
    )
    .bind(id.raw())
    .execute(&mut *tx)
    .await
    .context("cancelling pending jobs for removed directory")?
    .rows_affected();

    tx.commit().await.context("commit tx")?;
    if cancelled > 0 {
        tracing::info!(
            directory_id = %id,
            cancelled_jobs = cancelled,
            "cancelled pending jobs for removed directory"
        );
    }
    Ok(())
}

/// Summary of a hard-remove operation. Useful for UI feedback and debugging.
#[derive(Debug, Clone, Serialize, Default)]
pub struct HardRemoveReport {
    pub deleted_videos: i64,
    pub deleted_cache_files: u64,
    pub deleted_jobs: u64,
}

/// Permanently delete a directory and all state related to it. Unlike [`soft_remove`],
/// this is irreversible:
///
///   * all thumbnail and preview cache files for videos in this directory are removed
///     from disk (best-effort; failures to delete individual files are logged but do
///     not abort the operation);
///   * `jobs` rows referencing videos in this directory are deleted;
///   * the `directories` row is deleted, which cascades via FK to `videos`,
///     `collection_videos` (both directory and custom collection memberships),
///     `watch_history`, and the directory's own `collections` row.
///
/// Custom collections themselves remain, but lose their membership rows for these
/// videos.
pub async fn hard_remove(
    pool: &SqlitePool,
    _clock: &ClockRef,
    cache: &crate::scanner::CachePaths,
    id: DirectoryId,
) -> Result<HardRemoveReport> {
    // Verify the directory exists before doing any destructive work.
    let existed: Option<i64> = sqlx::query_scalar("SELECT id FROM directories WHERE id = ?")
        .bind(id.raw())
        .fetch_optional(pool)
        .await
        .context("looking up directory for hard-remove")?;
    if existed.is_none() {
        bail!("directory id {id} not found");
    }

    // 1. Collect video ids for cache + job cleanup.
    let rows = sqlx::query("SELECT id FROM videos WHERE directory_id = ?")
        .bind(id.raw())
        .fetch_all(pool)
        .await
        .context("listing videos for hard-remove")?;
    let video_ids: Vec<String> = rows.iter().map(|r| r.get::<String, _>(0)).collect();

    // 2. Remove on-disk cache files for each video (best-effort).
    let mut deleted_cache_files: u64 = 0;
    for vid in &video_ids {
        let vid_typed = VideoId(vid.clone());
        for path in [
            cache.thumb_path(&vid_typed),
            cache.preview_sheet_path(&vid_typed),
            cache.preview_vtt_path(&vid_typed),
        ] {
            match std::fs::remove_file(&path) {
                Ok(()) => deleted_cache_files += 1,
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                Err(err) => {
                    tracing::warn!(
                        path = %path.display(),
                        error = %err,
                        "failed to remove cache file during hard-remove"
                    );
                }
            }
        }
    }

    // 3. DB cleanup in a single transaction.
    let mut tx = pool.begin().await.context("begin hard-remove tx")?;

    let deleted_jobs = if video_ids.is_empty() {
        0
    } else {
        // Build an IN (?, ?, …) clause dynamically. SQLite has a practical limit on
        // parameters (~32k) which is far above anything we'd encounter.
        let placeholders = vec!["?"; video_ids.len()].join(",");
        let sql = format!("DELETE FROM jobs WHERE video_id IN ({placeholders})");
        let mut q = sqlx::query(&sql);
        for v in &video_ids {
            q = q.bind(v);
        }
        q.execute(&mut *tx)
            .await
            .context("deleting jobs for hard-removed directory")?
            .rows_affected()
    };

    let deleted_videos: i64 = video_ids.len() as i64;
    sqlx::query("DELETE FROM directories WHERE id = ?")
        .bind(id.raw())
        .execute(&mut *tx)
        .await
        .context("deleting directory row")?;

    tx.commit().await.context("commit hard-remove tx")?;

    tracing::info!(
        directory_id = %id,
        deleted_videos,
        deleted_cache_files,
        deleted_jobs,
        "hard-removed directory"
    );

    Ok(HardRemoveReport {
        deleted_videos,
        deleted_cache_files,
        deleted_jobs,
    })
}

const LIST_COLUMNS: &str = "d.id, d.path, d.label, d.added_at, d.removed, \
    (SELECT COUNT(*) FROM videos v WHERE v.directory_id = d.id AND v.missing = 0) AS video_count, \
    (SELECT c.id FROM collections c WHERE c.kind = 'directory' AND c.directory_id = d.id) AS collection_id";

fn list_sql(where_clause: &str) -> String {
    format!(
        "SELECT {LIST_COLUMNS} FROM directories d {where_clause} ORDER BY d.label COLLATE NOCASE"
    )
}

// These are only used via `list_sql()`, but we inline the common queries as &'static str
// computed at runtime (via Box::leak / lazy_static if we wanted that). Since `.bind()`
// makes the queries simple, just hardcode the SQL here.
const LIST_SQL_ACTIVE: &str = "SELECT d.id, d.path, d.label, d.added_at, d.removed, \
    (SELECT COUNT(*) FROM videos v WHERE v.directory_id = d.id AND v.missing = 0) AS video_count, \
    (SELECT c.id FROM collections c WHERE c.kind = 'directory' AND c.directory_id = d.id) AS collection_id \
    FROM directories d WHERE d.removed = 0 ORDER BY d.label COLLATE NOCASE";
const LIST_SQL_ALL: &str = "SELECT d.id, d.path, d.label, d.added_at, d.removed, \
    (SELECT COUNT(*) FROM videos v WHERE v.directory_id = d.id AND v.missing = 0) AS video_count, \
    (SELECT c.id FROM collections c WHERE c.kind = 'directory' AND c.directory_id = d.id) AS collection_id \
    FROM directories d ORDER BY d.label COLLATE NOCASE";
const LIST_SQL_BY_ID: &str = "SELECT d.id, d.path, d.label, d.added_at, d.removed, \
    (SELECT COUNT(*) FROM videos v WHERE v.directory_id = d.id AND v.missing = 0) AS video_count, \
    (SELECT c.id FROM collections c WHERE c.kind = 'directory' AND c.directory_id = d.id) AS collection_id \
    FROM directories d WHERE d.id = ?";

#[allow(dead_code)] // kept for future uses of the helper
fn _ensure_list_sql_consistency() {
    let _ = list_sql("WHERE d.removed = 0");
}

fn row_to_directory(row: &sqlx::sqlite::SqliteRow) -> Result<Directory> {
    let id: i64 = row.get("id");
    let path: String = row.get("path");
    let label: String = row.get("label");
    let added_at: String = row.get("added_at");
    let removed: i64 = row.get("removed");
    let video_count: i64 = row.get("video_count");
    let collection_id: Option<i64> = row.try_get("collection_id").ok();
    let added_at = chrono::DateTime::parse_from_rfc3339(&added_at)
        .with_context(|| format!("parsing added_at for directory {id}"))?
        .with_timezone(&Utc);
    Ok(Directory {
        id: DirectoryId(id),
        path,
        label,
        added_at,
        removed: removed != 0,
        video_count,
        collection_id: CollectionId(collection_id.unwrap_or(0)),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::{self};

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

        let (before_pending, before_running, _, _) =
            crate::jobs::count_by_status(&pool).await.unwrap();
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
        let (after_pending, after_running, _, _) =
            crate::jobs::count_by_status(&pool).await.unwrap();
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
}
