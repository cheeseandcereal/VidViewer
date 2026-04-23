//! Collections: directory-backed and custom.
//!
//! See `docs/design/07-collections.md` for the behavioral spec.

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::Serialize;
use sqlx::{Row, SqlitePool};
use thiserror::Error;

use crate::{
    clock::ClockRef,
    ids::{CollectionId, DirectoryId, VideoId},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Directory,
    Custom,
}

impl Kind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Kind::Directory => "directory",
            Kind::Custom => "custom",
        }
    }
    pub fn from_db(s: &str) -> Option<Kind> {
        match s {
            "directory" => Some(Kind::Directory),
            "custom" => Some(Kind::Custom),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Collection {
    pub id: CollectionId,
    pub name: String,
    pub kind: Kind,
    pub directory_id: Option<DirectoryId>,
    pub hidden: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub video_count: i64,
}

/// Summary for grid/listing views.
#[derive(Debug, Clone, Serialize)]
pub struct CollectionSummary {
    #[serde(flatten)]
    pub coll: Collection,
    /// Up to 4 thumbnail video ids for a mosaic preview.
    pub preview_video_ids: Vec<VideoId>,
}

#[derive(Debug, Clone, Error, Serialize)]
#[serde(tag = "error", rename_all = "snake_case")]
pub enum MutationError {
    #[error("collection not found")]
    NotFound,
    #[error("directory collections cannot be modified that way")]
    DirectoryCollectionImmutable,
    #[error("name must be non-empty")]
    EmptyName,
    #[error("internal error: {message}")]
    Internal { message: String },
}

impl MutationError {
    pub fn status(&self) -> axum::http::StatusCode {
        use axum::http::StatusCode;
        match self {
            MutationError::NotFound => StatusCode::NOT_FOUND,
            MutationError::Internal { .. } => StatusCode::INTERNAL_SERVER_ERROR,
            _ => StatusCode::BAD_REQUEST,
        }
    }
}

fn internal<E: std::fmt::Display>(e: E) -> MutationError {
    MutationError::Internal {
        message: e.to_string(),
    }
}

pub async fn list(pool: &SqlitePool, kind: Option<Kind>) -> Result<Vec<Collection>> {
    let sql = "SELECT id, name, kind, directory_id, hidden, created_at, updated_at, \
        (SELECT COUNT(*) FROM collection_videos cv JOIN videos v ON v.id = cv.video_id \
         WHERE cv.collection_id = collections.id AND v.missing = 0) AS video_count \
        FROM collections WHERE hidden = 0 \
        ORDER BY kind, name COLLATE NOCASE";
    let rows = sqlx::query(sql).fetch_all(pool).await.context("list")?;
    let mut out = Vec::with_capacity(rows.len());
    for r in rows {
        let c = row_to_collection(&r)?;
        if let Some(k) = kind {
            if c.kind != k {
                continue;
            }
        }
        out.push(c);
    }
    Ok(out)
}

pub async fn get(pool: &SqlitePool, id: CollectionId) -> Result<Option<Collection>> {
    let sql = "SELECT id, name, kind, directory_id, hidden, created_at, updated_at, \
        (SELECT COUNT(*) FROM collection_videos cv JOIN videos v ON v.id = cv.video_id \
         WHERE cv.collection_id = collections.id AND v.missing = 0) AS video_count \
        FROM collections WHERE id = ?";
    let row = sqlx::query(sql)
        .bind(id.raw())
        .fetch_optional(pool)
        .await
        .context("get collection")?;
    match row {
        Some(r) => Ok(Some(row_to_collection(&r)?)),
        None => Ok(None),
    }
}

pub async fn list_summaries(pool: &SqlitePool) -> Result<Vec<CollectionSummary>> {
    let colls = list(pool, None).await?;
    let mut out = Vec::with_capacity(colls.len());
    for c in colls {
        let preview = sqlx::query(
            "SELECT v.id FROM collection_videos cv \
             JOIN videos v ON v.id = cv.video_id \
             WHERE cv.collection_id = ? AND v.missing = 0 AND v.thumbnail_ok = 1 \
             ORDER BY cv.added_at DESC LIMIT 4",
        )
        .bind(c.id.raw())
        .fetch_all(pool)
        .await
        .context("preview ids")?;
        let preview_video_ids: Vec<VideoId> = preview
            .iter()
            .map(|r| VideoId(r.get::<String, _>(0)))
            .collect();
        out.push(CollectionSummary {
            coll: c,
            preview_video_ids,
        });
    }
    Ok(out)
}

fn row_to_collection(row: &sqlx::sqlite::SqliteRow) -> Result<Collection> {
    let id: i64 = row.get("id");
    let name: String = row.get("name");
    let kind: String = row.get("kind");
    let directory_id: Option<i64> = row.get("directory_id");
    let hidden: i64 = row.get("hidden");
    let created_at: String = row.get("created_at");
    let updated_at: String = row.get("updated_at");
    let video_count: i64 = row.get("video_count");
    Ok(Collection {
        id: CollectionId(id),
        name,
        kind: Kind::from_db(&kind).unwrap_or(Kind::Custom),
        directory_id: directory_id.map(DirectoryId),
        hidden: hidden != 0,
        created_at: chrono::DateTime::parse_from_rfc3339(&created_at)?.with_timezone(&Utc),
        updated_at: chrono::DateTime::parse_from_rfc3339(&updated_at)?.with_timezone(&Utc),
        video_count,
    })
}

pub async fn create_custom(
    pool: &SqlitePool,
    clock: &ClockRef,
    name: &str,
) -> Result<Collection, MutationError> {
    let name = name.trim();
    if name.is_empty() {
        return Err(MutationError::EmptyName);
    }
    let now_s = clock.now().to_rfc3339();
    let row = sqlx::query(
        "INSERT INTO collections (name, kind, directory_id, hidden, created_at, updated_at) \
         VALUES (?, 'custom', NULL, 0, ?, ?) RETURNING id",
    )
    .bind(name)
    .bind(&now_s)
    .bind(&now_s)
    .fetch_one(pool)
    .await
    .map_err(internal)?;
    let id: i64 = row.get(0);
    let c = get(pool, CollectionId(id)).await.map_err(internal)?;
    c.ok_or(MutationError::NotFound)
}

pub async fn rename(
    pool: &SqlitePool,
    clock: &ClockRef,
    id: CollectionId,
    name: &str,
) -> Result<Collection, MutationError> {
    let name = name.trim();
    if name.is_empty() {
        return Err(MutationError::EmptyName);
    }
    let now_s = clock.now().to_rfc3339();
    let mut tx = pool.begin().await.map_err(internal)?;

    let kind: Option<String> = sqlx::query("SELECT kind FROM collections WHERE id = ?")
        .bind(id.raw())
        .fetch_optional(&mut *tx)
        .await
        .map_err(internal)?
        .map(|r| r.get(0));
    let Some(kind) = kind else {
        return Err(MutationError::NotFound);
    };

    sqlx::query("UPDATE collections SET name = ?, updated_at = ? WHERE id = ?")
        .bind(name)
        .bind(&now_s)
        .bind(id.raw())
        .execute(&mut *tx)
        .await
        .map_err(internal)?;

    // If directory, also update the directories.label to keep in sync.
    if kind == "directory" {
        sqlx::query(
            "UPDATE directories SET label = ? \
             WHERE id = (SELECT directory_id FROM collections WHERE id = ?)",
        )
        .bind(name)
        .bind(id.raw())
        .execute(&mut *tx)
        .await
        .map_err(internal)?;
    }

    tx.commit().await.map_err(internal)?;
    let c = get(pool, id).await.map_err(internal)?;
    c.ok_or(MutationError::NotFound)
}

pub async fn delete_custom(pool: &SqlitePool, id: CollectionId) -> Result<(), MutationError> {
    let kind: Option<String> = sqlx::query("SELECT kind FROM collections WHERE id = ?")
        .bind(id.raw())
        .fetch_optional(pool)
        .await
        .map_err(internal)?
        .map(|r| r.get(0));
    let Some(kind) = kind else {
        return Err(MutationError::NotFound);
    };
    if kind != "custom" {
        return Err(MutationError::DirectoryCollectionImmutable);
    }
    sqlx::query("DELETE FROM collections WHERE id = ?")
        .bind(id.raw())
        .execute(pool)
        .await
        .map_err(internal)?;
    Ok(())
}

pub async fn add_video(
    pool: &SqlitePool,
    clock: &ClockRef,
    id: CollectionId,
    video_id: &VideoId,
) -> Result<(), MutationError> {
    let kind: Option<String> = sqlx::query("SELECT kind FROM collections WHERE id = ?")
        .bind(id.raw())
        .fetch_optional(pool)
        .await
        .map_err(internal)?
        .map(|r| r.get(0));
    let Some(kind) = kind else {
        return Err(MutationError::NotFound);
    };
    if kind != "custom" {
        return Err(MutationError::DirectoryCollectionImmutable);
    }
    let now_s = clock.now().to_rfc3339();
    sqlx::query(
        "INSERT OR IGNORE INTO collection_videos (collection_id, video_id, added_at) \
         VALUES (?, ?, ?)",
    )
    .bind(id.raw())
    .bind(video_id.as_str())
    .bind(&now_s)
    .execute(pool)
    .await
    .map_err(internal)?;
    Ok(())
}

pub async fn remove_video(
    pool: &SqlitePool,
    id: CollectionId,
    video_id: &VideoId,
) -> Result<(), MutationError> {
    let kind: Option<String> = sqlx::query("SELECT kind FROM collections WHERE id = ?")
        .bind(id.raw())
        .fetch_optional(pool)
        .await
        .map_err(internal)?
        .map(|r| r.get(0));
    let Some(kind) = kind else {
        return Err(MutationError::NotFound);
    };
    if kind != "custom" {
        return Err(MutationError::DirectoryCollectionImmutable);
    }
    sqlx::query("DELETE FROM collection_videos WHERE collection_id = ? AND video_id = ?")
        .bind(id.raw())
        .bind(video_id.as_str())
        .execute(pool)
        .await
        .map_err(internal)?;
    Ok(())
}

#[derive(Debug, Clone, Serialize)]
pub struct VideoCard {
    pub id: VideoId,
    pub filename: String,
    pub duration_secs: Option<f64>,
    pub thumbnail_ok: bool,
    pub preview_ok: bool,
    pub missing: bool,
    pub updated_at_epoch: i64,
}

pub async fn videos_in(pool: &SqlitePool, id: CollectionId) -> Result<Vec<VideoCard>> {
    let rows = sqlx::query(
        "SELECT v.id, v.filename, v.duration_secs, v.thumbnail_ok, v.preview_ok, v.missing, v.updated_at \
         FROM collection_videos cv JOIN videos v ON v.id = cv.video_id \
         WHERE cv.collection_id = ? \
         ORDER BY cv.added_at DESC",
    )
    .bind(id.raw())
    .fetch_all(pool)
    .await
    .context("videos_in")?;
    let mut out = Vec::with_capacity(rows.len());
    for r in rows {
        let updated_at: String = r.get("updated_at");
        let dt = chrono::DateTime::parse_from_rfc3339(&updated_at)?.with_timezone(&Utc);
        out.push(VideoCard {
            id: VideoId(r.get("id")),
            filename: r.get("filename"),
            duration_secs: r.get("duration_secs"),
            thumbnail_ok: r.get::<i64, _>("thumbnail_ok") != 0,
            preview_ok: r.get::<i64, _>("preview_ok") != 0,
            missing: r.get::<i64, _>("missing") != 0,
            updated_at_epoch: dt.timestamp(),
        });
    }
    Ok(out)
}

/// Pick a uniformly random playable video from a collection.
pub async fn random_video(pool: &SqlitePool, id: CollectionId) -> Result<Option<VideoId>> {
    let row = sqlx::query(
        "SELECT v.id FROM collection_videos cv JOIN videos v ON v.id = cv.video_id \
         WHERE cv.collection_id = ? AND v.missing = 0 \
         ORDER BY RANDOM() LIMIT 1",
    )
    .bind(id.raw())
    .fetch_optional(pool)
    .await
    .context("random_video")?;
    match row {
        Some(r) => {
            let s: String = r.get(0);
            Ok(Some(VideoId(s)))
        }
        None => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock;
    use crate::directories::add as add_dir;

    async fn setup() -> (tempfile::TempDir, SqlitePool, ClockRef) {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = crate::config::Config {
            backup_dir: tmp.path().join("backups"),
            ..crate::config::Config::default()
        };
        let db_path = tmp.path().join("vidviewer.db");
        let pool = crate::db::init(&cfg, &db_path).await.unwrap();
        (tmp, pool, clock::system())
    }

    #[tokio::test]
    async fn list_summaries_hides_hidden() {
        let (tmp, pool, clock) = setup().await;
        let videos = tmp.path().join("videos");
        std::fs::create_dir_all(&videos).unwrap();
        let dir = add_dir(&pool, &clock, &videos, Some("mine".into()))
            .await
            .unwrap();

        let sums = list_summaries(&pool).await.unwrap();
        assert_eq!(sums.len(), 1);

        // Soft-remove the directory → its collection is hidden.
        crate::directories::soft_remove(&pool, &clock, dir.id)
            .await
            .unwrap();
        let sums = list_summaries(&pool).await.unwrap();
        assert_eq!(sums.len(), 0);
    }

    #[tokio::test]
    async fn create_and_delete_custom() {
        let (_tmp, pool, clock) = setup().await;
        let c = create_custom(&pool, &clock, "My Collection").await.unwrap();
        assert_eq!(c.kind, Kind::Custom);
        delete_custom(&pool, c.id).await.unwrap();
        assert!(get(&pool, c.id).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn directory_collection_cannot_be_deleted() {
        let (tmp, pool, clock) = setup().await;
        let videos = tmp.path().join("videos");
        std::fs::create_dir_all(&videos).unwrap();
        let dir = add_dir(&pool, &clock, &videos, Some("x".into()))
            .await
            .unwrap();
        let err = delete_custom(&pool, dir.collection_id).await.unwrap_err();
        assert!(matches!(err, MutationError::DirectoryCollectionImmutable));
    }
}
