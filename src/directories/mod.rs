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
    ids::{CollectionId, DirectoryId},
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
///
/// Watch history and custom collection memberships are preserved.
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

    tx.commit().await.context("commit tx")?;
    Ok(())
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
}
