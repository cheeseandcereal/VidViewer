//! Directory records and CRUD operations.
//!
//! Directories represent top-level roots configured by the user. Each has a `directory`
//! collection auto-created alongside it (materialized membership, maintained by the scanner).
//!
//! The mutating commands (add, set_label, soft_remove, hard_remove) live in
//! [`commands`]; this file owns the shared types, validation, and read queries.
//!
//! See `docs/design/04-scanner.md` and `docs/design/07-collections.md`.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::Serialize;
use sqlx::{Row, SqlitePool};
use thiserror::Error;

use crate::{
    db::row::{bool_from_i64, datetime_from_rfc3339},
    ids::{CollectionId, DirectoryId},
};

mod commands;
#[cfg(test)]
mod tests {
    //! Directory unit tests.
    use std::path::Path;

    use sqlx::SqlitePool;

    use super::{add, hard_remove, list, set_label, soft_remove, AddError};
    use crate::{
        clock::{self, ClockRef},
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
        crate::test_support::write_video_fixture(&videos, "a.mp4", b"x");
        crate::test_support::write_video_fixture(&videos, "b.mp4", b"y");

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
        crate::test_support::write_video_fixture(&videos, "a.mp4", b"x");

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

        // Include this directory in a custom collection.
        let _custom = crate::collections::create_custom(&pool, &clock, "Favorites", &[dir.id])
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
        let count_custom_coll_dir_refs: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM collection_directories cd \
                 JOIN collections c ON c.id = cd.collection_id \
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
            count_custom_coll_dir_refs, 0,
            "custom collection_directories rows cascade-deleted"
        );

        // Custom collection itself survives.
        let count_custom_colls: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM collections WHERE kind = 'custom'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(count_custom_colls, 1);

        // suppress unused warning — we only care above that the video id existed.
        let _ = video_id;

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

pub use commands::{add, hard_remove, set_label, soft_remove, HardRemoveReport};

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

pub(crate) fn internal<E: std::fmt::Display>(e: E) -> AddError {
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

fn row_to_directory(row: &sqlx::sqlite::SqliteRow) -> Result<Directory> {
    let id: i64 = row.get("id");
    let path: String = row.get("path");
    let label: String = row.get("label");
    let video_count: i64 = row.get("video_count");
    let collection_id: Option<i64> = row.try_get("collection_id").ok();
    let added_at = datetime_from_rfc3339(row, "added_at")?;
    Ok(Directory {
        id: DirectoryId(id),
        path,
        label,
        added_at,
        removed: bool_from_i64(row, "removed"),
        video_count,
        collection_id: CollectionId(collection_id.unwrap_or(0)),
    })
}
