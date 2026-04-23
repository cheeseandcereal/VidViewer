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

use crate::ids::{CollectionId, DirectoryId};

mod commands;
#[cfg(test)]
mod tests;

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
