//! Video records and low-level CRUD.

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::Serialize;
use sqlx::{Row, SqlitePool};

use crate::ids::{DirectoryId, VideoId};

#[derive(Debug, Clone, Serialize)]
pub struct Video {
    pub id: VideoId,
    pub directory_id: DirectoryId,
    pub relative_path: String,
    pub filename: String,
    pub size_bytes: i64,
    pub mtime_unix: i64,
    pub duration_secs: Option<f64>,
    pub width: Option<i64>,
    pub height: Option<i64>,
    pub codec: Option<String>,
    pub thumbnail_ok: bool,
    pub preview_ok: bool,
    pub missing: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

pub async fn get(pool: &SqlitePool, id: &VideoId) -> Result<Option<Video>> {
    let row = sqlx::query(SELECT_ALL)
        .bind(id.as_str())
        .fetch_optional(pool)
        .await
        .context("fetch video")?;
    match row {
        Some(r) => Ok(Some(row_to_video(&r)?)),
        None => Ok(None),
    }
}

const SELECT_ALL: &str = "SELECT id, directory_id, relative_path, filename, size_bytes, \
    mtime_unix, duration_secs, width, height, codec, thumbnail_ok, preview_ok, missing, \
    created_at, updated_at FROM videos WHERE id = ?";

pub fn row_to_video(row: &sqlx::sqlite::SqliteRow) -> Result<Video> {
    let id: String = row.get("id");
    let directory_id: i64 = row.get("directory_id");
    let relative_path: String = row.get("relative_path");
    let filename: String = row.get("filename");
    let size_bytes: i64 = row.get("size_bytes");
    let mtime_unix: i64 = row.get("mtime_unix");
    let duration_secs: Option<f64> = row.get("duration_secs");
    let width: Option<i64> = row.get("width");
    let height: Option<i64> = row.get("height");
    let codec: Option<String> = row.get("codec");
    let thumbnail_ok: i64 = row.get("thumbnail_ok");
    let preview_ok: i64 = row.get("preview_ok");
    let missing: i64 = row.get("missing");
    let created_at: String = row.get("created_at");
    let updated_at: String = row.get("updated_at");
    let created_at = chrono::DateTime::parse_from_rfc3339(&created_at)?.with_timezone(&Utc);
    let updated_at = chrono::DateTime::parse_from_rfc3339(&updated_at)?.with_timezone(&Utc);
    Ok(Video {
        id: VideoId(id),
        directory_id: DirectoryId(directory_id),
        relative_path,
        filename,
        size_bytes,
        mtime_unix,
        duration_secs,
        width,
        height,
        codec,
        thumbnail_ok: thumbnail_ok != 0,
        preview_ok: preview_ok != 0,
        missing: missing != 0,
        created_at,
        updated_at,
    })
}
