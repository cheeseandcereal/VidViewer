//! Collections: directory-backed and custom.
//!
//! See `docs/design/07-collections.md` for the behavioral spec.
//!
//! Membership is computed on read; there is no materialized membership table.
//! Directory collections read their videos via `videos.directory_id =
//! collections.directory_id`. Custom collections read their videos as the union
//! of videos whose `directory_id` appears in the `collection_directories` rows
//! for that collection. Videos with `missing = 1` are excluded everywhere.

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::Serialize;
use sqlx::{Row, SqlitePool};
use thiserror::Error;

use crate::{
    clock::ClockRef,
    db::row::{bool_from_i64, datetime_from_rfc3339},
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

/// One directory included in a custom collection. Used for UI chip rows and
/// the detail page's directory management UI.
#[derive(Debug, Clone, Serialize)]
pub struct CollectionDirectory {
    pub directory_id: DirectoryId,
    pub label: String,
    pub path: String,
    /// `true` if this directory is currently soft-removed. Listed but not
    /// contributing to the collection until re-added.
    pub removed: bool,
    pub added_at: DateTime<Utc>,
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
    #[error("directory not found")]
    DirectoryNotFound,
    #[error("directory is soft-removed")]
    DirectoryRemoved,
    #[error("internal error: {message}")]
    Internal { message: String },
}

impl MutationError {
    pub fn status(&self) -> axum::http::StatusCode {
        use axum::http::StatusCode;
        match self {
            MutationError::NotFound | MutationError::DirectoryNotFound => StatusCode::NOT_FOUND,
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

// ---------------------------------------------------------------------------
// SQL snippets. The video-count and preview-thumbnail subqueries differ by
// collection kind, so they're assembled per row in `row_to_collection` /
// `list_summaries`.
// ---------------------------------------------------------------------------

const SELECT_COLLECTION: &str =
    "SELECT id, name, kind, directory_id, hidden, created_at, updated_at FROM collections";

/// Count of non-missing videos for a collection of the given `kind`. Returns 0
/// for custom collections with no directory members.
async fn count_videos(
    pool: &SqlitePool,
    id: CollectionId,
    kind: Kind,
    directory_id: Option<DirectoryId>,
) -> Result<i64> {
    let row = match kind {
        Kind::Directory => {
            let dir = directory_id.map(|d| d.raw()).unwrap_or(-1);
            sqlx::query("SELECT COUNT(*) FROM videos WHERE directory_id = ? AND missing = 0")
                .bind(dir)
                .fetch_one(pool)
                .await
                .context("count videos (directory)")?
        }
        Kind::Custom => sqlx::query(
            "SELECT COUNT(*) FROM videos v \
             WHERE v.missing = 0 AND v.directory_id IN \
               (SELECT directory_id FROM collection_directories WHERE collection_id = ?)",
        )
        .bind(id.raw())
        .fetch_one(pool)
        .await
        .context("count videos (custom)")?,
    };
    Ok(row.get::<i64, _>(0))
}

pub async fn list(pool: &SqlitePool, kind: Option<Kind>) -> Result<Vec<Collection>> {
    let sql = format!("{SELECT_COLLECTION} WHERE hidden = 0 ORDER BY kind, name COLLATE NOCASE");
    let rows = sqlx::query(&sql).fetch_all(pool).await.context("list")?;
    let mut out = Vec::with_capacity(rows.len());
    for r in rows {
        let c = hydrate(pool, &r).await?;
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
    let sql = format!("{SELECT_COLLECTION} WHERE id = ?");
    let row = sqlx::query(&sql)
        .bind(id.raw())
        .fetch_optional(pool)
        .await
        .context("get collection")?;
    match row {
        Some(r) => Ok(Some(hydrate(pool, &r).await?)),
        None => Ok(None),
    }
}

pub async fn list_summaries(pool: &SqlitePool) -> Result<Vec<CollectionSummary>> {
    let colls = list(pool, None).await?;
    let mut out = Vec::with_capacity(colls.len());
    for c in colls {
        let preview_video_ids = preview_thumbs(pool, &c).await?;
        out.push(CollectionSummary {
            coll: c,
            preview_video_ids,
        });
    }
    Ok(out)
}

async fn preview_thumbs(pool: &SqlitePool, c: &Collection) -> Result<Vec<VideoId>> {
    let rows = match c.kind {
        Kind::Directory => {
            let dir = c.directory_id.map(|d| d.raw()).unwrap_or(-1);
            sqlx::query(
                "SELECT id FROM videos \
                 WHERE directory_id = ? AND missing = 0 AND thumbnail_ok = 1 \
                 ORDER BY updated_at DESC LIMIT 4",
            )
            .bind(dir)
            .fetch_all(pool)
            .await
            .context("preview ids (directory)")?
        }
        Kind::Custom => sqlx::query(
            "SELECT v.id FROM videos v \
             WHERE v.missing = 0 AND v.thumbnail_ok = 1 AND v.directory_id IN \
               (SELECT directory_id FROM collection_directories WHERE collection_id = ?) \
             ORDER BY v.updated_at DESC LIMIT 4",
        )
        .bind(c.id.raw())
        .fetch_all(pool)
        .await
        .context("preview ids (custom)")?,
    };
    Ok(rows
        .into_iter()
        .map(|r| VideoId(r.get::<String, _>(0)))
        .collect())
}

async fn hydrate(pool: &SqlitePool, row: &sqlx::sqlite::SqliteRow) -> Result<Collection> {
    let id: i64 = row.get("id");
    let name: String = row.get("name");
    let kind_s: String = row.get("kind");
    let directory_id: Option<i64> = row.get("directory_id");
    let kind = Kind::from_db(&kind_s).unwrap_or(Kind::Custom);
    let dir_id = directory_id.map(DirectoryId);
    let coll_id = CollectionId(id);
    let video_count = count_videos(pool, coll_id, kind, dir_id).await?;
    Ok(Collection {
        id: coll_id,
        name,
        kind,
        directory_id: dir_id,
        hidden: bool_from_i64(row, "hidden"),
        created_at: datetime_from_rfc3339(row, "created_at")?,
        updated_at: datetime_from_rfc3339(row, "updated_at")?,
        video_count,
    })
}

// ---------------------------------------------------------------------------
// Mutations.
// ---------------------------------------------------------------------------

pub async fn create_custom(
    pool: &SqlitePool,
    clock: &ClockRef,
    name: &str,
    directory_ids: &[DirectoryId],
) -> Result<Collection, MutationError> {
    let name = name.trim();
    if name.is_empty() {
        return Err(MutationError::EmptyName);
    }
    let now_s = clock.now().to_rfc3339();
    let mut tx = pool.begin().await.map_err(internal)?;

    let row = sqlx::query(
        "INSERT INTO collections (name, kind, directory_id, hidden, created_at, updated_at) \
         VALUES (?, 'custom', NULL, 0, ?, ?) RETURNING id",
    )
    .bind(name)
    .bind(&now_s)
    .bind(&now_s)
    .fetch_one(&mut *tx)
    .await
    .map_err(internal)?;
    let coll_id = CollectionId(row.get::<i64, _>(0));

    for did in directory_ids {
        // Validate: directory must exist and not be soft-removed.
        let dir_row: Option<(i64,)> =
            sqlx::query_as("SELECT removed FROM directories WHERE id = ?")
                .bind(did.raw())
                .fetch_optional(&mut *tx)
                .await
                .map_err(internal)?;
        match dir_row {
            None => return Err(MutationError::DirectoryNotFound),
            Some((removed,)) if removed != 0 => return Err(MutationError::DirectoryRemoved),
            Some(_) => {}
        }
        sqlx::query(
            "INSERT OR IGNORE INTO collection_directories \
             (collection_id, directory_id, added_at) VALUES (?, ?, ?)",
        )
        .bind(coll_id.raw())
        .bind(did.raw())
        .bind(&now_s)
        .execute(&mut *tx)
        .await
        .map_err(internal)?;
    }

    tx.commit().await.map_err(internal)?;
    let c = get(pool, coll_id).await.map_err(internal)?;
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

/// Add a directory to a custom collection. Rejects if the collection is of
/// kind `directory`, if the directory does not exist, or if it is currently
/// soft-removed.
pub async fn add_directory(
    pool: &SqlitePool,
    clock: &ClockRef,
    id: CollectionId,
    directory_id: DirectoryId,
) -> Result<(), MutationError> {
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
    if kind != "custom" {
        return Err(MutationError::DirectoryCollectionImmutable);
    }

    let dir_row: Option<(i64,)> = sqlx::query_as("SELECT removed FROM directories WHERE id = ?")
        .bind(directory_id.raw())
        .fetch_optional(&mut *tx)
        .await
        .map_err(internal)?;
    match dir_row {
        None => return Err(MutationError::DirectoryNotFound),
        Some((removed,)) if removed != 0 => return Err(MutationError::DirectoryRemoved),
        Some(_) => {}
    }

    let now_s = clock.now().to_rfc3339();
    sqlx::query(
        "INSERT OR IGNORE INTO collection_directories \
         (collection_id, directory_id, added_at) VALUES (?, ?, ?)",
    )
    .bind(id.raw())
    .bind(directory_id.raw())
    .bind(&now_s)
    .execute(&mut *tx)
    .await
    .map_err(internal)?;

    // Bump updated_at so the collection page can rely on it for cache busts.
    sqlx::query("UPDATE collections SET updated_at = ? WHERE id = ?")
        .bind(&now_s)
        .bind(id.raw())
        .execute(&mut *tx)
        .await
        .map_err(internal)?;

    tx.commit().await.map_err(internal)?;
    Ok(())
}

pub async fn remove_directory(
    pool: &SqlitePool,
    clock: &ClockRef,
    id: CollectionId,
    directory_id: DirectoryId,
) -> Result<(), MutationError> {
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
    if kind != "custom" {
        return Err(MutationError::DirectoryCollectionImmutable);
    }
    let now_s = clock.now().to_rfc3339();
    sqlx::query("DELETE FROM collection_directories WHERE collection_id = ? AND directory_id = ?")
        .bind(id.raw())
        .bind(directory_id.raw())
        .execute(&mut *tx)
        .await
        .map_err(internal)?;
    sqlx::query("UPDATE collections SET updated_at = ? WHERE id = ?")
        .bind(&now_s)
        .bind(id.raw())
        .execute(&mut *tx)
        .await
        .map_err(internal)?;
    tx.commit().await.map_err(internal)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Reads.
// ---------------------------------------------------------------------------

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

/// Videos in a collection, sorted by filename (case-insensitive) with `id` as
/// a stable tiebreaker. Excludes `missing = 1`.
pub async fn videos_in(pool: &SqlitePool, id: CollectionId) -> Result<Vec<VideoCard>> {
    let (kind, directory_id) = match collection_kind(pool, id).await? {
        Some(t) => t,
        None => return Ok(Vec::new()),
    };
    let rows = match kind {
        Kind::Directory => {
            let dir = directory_id.map(|d| d.raw()).unwrap_or(-1);
            sqlx::query(
                "SELECT id, filename, duration_secs, thumbnail_ok, preview_ok, missing, updated_at \
                 FROM videos WHERE directory_id = ? AND missing = 0 \
                 ORDER BY filename COLLATE NOCASE, id",
            )
            .bind(dir)
            .fetch_all(pool)
            .await
            .context("videos_in (directory)")?
        }
        Kind::Custom => sqlx::query(
            "SELECT v.id, v.filename, v.duration_secs, v.thumbnail_ok, v.preview_ok, v.missing, v.updated_at \
             FROM videos v \
             WHERE v.missing = 0 AND v.directory_id IN \
               (SELECT directory_id FROM collection_directories WHERE collection_id = ?) \
             ORDER BY v.filename COLLATE NOCASE, v.id",
        )
        .bind(id.raw())
        .fetch_all(pool)
        .await
        .context("videos_in (custom)")?,
    };
    let mut out = Vec::with_capacity(rows.len());
    for r in rows {
        let dt = datetime_from_rfc3339(&r, "updated_at")?;
        out.push(VideoCard {
            id: VideoId(r.get("id")),
            filename: r.get("filename"),
            duration_secs: r.get("duration_secs"),
            thumbnail_ok: bool_from_i64(&r, "thumbnail_ok"),
            preview_ok: bool_from_i64(&r, "preview_ok"),
            missing: bool_from_i64(&r, "missing"),
            updated_at_epoch: dt.timestamp(),
        });
    }
    Ok(out)
}

/// Pick a uniformly random playable video from a collection.
pub async fn random_video(pool: &SqlitePool, id: CollectionId) -> Result<Option<VideoId>> {
    let (kind, directory_id) = match collection_kind(pool, id).await? {
        Some(t) => t,
        None => return Ok(None),
    };
    let row = match kind {
        Kind::Directory => {
            let dir = directory_id.map(|d| d.raw()).unwrap_or(-1);
            sqlx::query(
                "SELECT id FROM videos WHERE directory_id = ? AND missing = 0 \
                 ORDER BY RANDOM() LIMIT 1",
            )
            .bind(dir)
            .fetch_optional(pool)
            .await
            .context("random_video (directory)")?
        }
        Kind::Custom => sqlx::query(
            "SELECT v.id FROM videos v \
             WHERE v.missing = 0 AND v.directory_id IN \
               (SELECT directory_id FROM collection_directories WHERE collection_id = ?) \
             ORDER BY RANDOM() LIMIT 1",
        )
        .bind(id.raw())
        .fetch_optional(pool)
        .await
        .context("random_video (custom)")?,
    };
    match row {
        Some(r) => {
            let s: String = r.get(0);
            Ok(Some(VideoId(s)))
        }
        None => Ok(None),
    }
}

/// List the directories included in a custom collection. For directory
/// collections this returns an empty vec (they have no such list).
pub async fn directories_in(
    pool: &SqlitePool,
    id: CollectionId,
) -> Result<Vec<CollectionDirectory>> {
    let rows = sqlx::query(
        "SELECT d.id, d.label, d.path, d.removed, cd.added_at \
         FROM collection_directories cd JOIN directories d ON d.id = cd.directory_id \
         WHERE cd.collection_id = ? \
         ORDER BY d.label COLLATE NOCASE",
    )
    .bind(id.raw())
    .fetch_all(pool)
    .await
    .context("directories_in")?;
    let mut out = Vec::with_capacity(rows.len());
    for r in rows {
        out.push(CollectionDirectory {
            directory_id: DirectoryId(r.get("id")),
            label: r.get("label"),
            path: r.get("path"),
            removed: bool_from_i64(&r, "removed"),
            added_at: datetime_from_rfc3339(&r, "added_at")?,
        });
    }
    Ok(out)
}

/// Helper: look up `(kind, directory_id)` for a collection.
async fn collection_kind(
    pool: &SqlitePool,
    id: CollectionId,
) -> Result<Option<(Kind, Option<DirectoryId>)>> {
    let row = sqlx::query("SELECT kind, directory_id FROM collections WHERE id = ?")
        .bind(id.raw())
        .fetch_optional(pool)
        .await
        .context("collection_kind")?;
    Ok(row.map(|r| {
        let kind_s: String = r.get("kind");
        let dir: Option<i64> = r.get("directory_id");
        (
            Kind::from_db(&kind_s).unwrap_or(Kind::Custom),
            dir.map(DirectoryId),
        )
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock;
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

    async fn add_video_row(
        pool: &SqlitePool,
        clock: &ClockRef,
        dir_id: DirectoryId,
        rel: &str,
    ) -> VideoId {
        let now = clock.now().to_rfc3339();
        let id = VideoId(uuid::Uuid::new_v4().to_string());
        sqlx::query(
            "INSERT INTO videos (id, directory_id, relative_path, filename, size_bytes, \
             mtime_unix, thumbnail_ok, preview_ok, missing, created_at, updated_at) \
             VALUES (?, ?, ?, ?, 1, 1, 1, 0, 0, ?, ?)",
        )
        .bind(id.as_str())
        .bind(dir_id.raw())
        .bind(rel)
        .bind(rel)
        .bind(&now)
        .bind(&now)
        .execute(pool)
        .await
        .unwrap();
        id
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
        let c = create_custom(&pool, &clock, "My Collection", &[])
            .await
            .unwrap();
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

    #[tokio::test]
    async fn custom_collection_is_union_of_directories() {
        let (tmp, pool, clock) = setup().await;
        let dir_a = tmp.path().join("a");
        let dir_b = tmp.path().join("b");
        std::fs::create_dir_all(&dir_a).unwrap();
        std::fs::create_dir_all(&dir_b).unwrap();
        let a = add_dir(&pool, &clock, &dir_a, Some("A".into()))
            .await
            .unwrap();
        let b = add_dir(&pool, &clock, &dir_b, Some("B".into()))
            .await
            .unwrap();
        let _va = add_video_row(&pool, &clock, a.id, "va.mp4").await;
        let _vb = add_video_row(&pool, &clock, b.id, "vb.mp4").await;

        let c = create_custom(&pool, &clock, "Both", &[a.id, b.id])
            .await
            .unwrap();
        let cards = videos_in(&pool, c.id).await.unwrap();
        assert_eq!(cards.len(), 2);

        // Removing a directory drops those videos on next read.
        remove_directory(&pool, &clock, c.id, a.id).await.unwrap();
        let cards = videos_in(&pool, c.id).await.unwrap();
        assert_eq!(cards.len(), 1);
        assert_eq!(cards[0].filename, "vb.mp4");

        // Adding it back restores them with no scan needed.
        add_directory(&pool, &clock, c.id, a.id).await.unwrap();
        let cards = videos_in(&pool, c.id).await.unwrap();
        assert_eq!(cards.len(), 2);
    }

    #[tokio::test]
    async fn custom_collection_excludes_missing_videos() {
        let (tmp, pool, clock) = setup().await;
        let dir_a = tmp.path().join("a");
        std::fs::create_dir_all(&dir_a).unwrap();
        let a = add_dir(&pool, &clock, &dir_a, Some("A".into()))
            .await
            .unwrap();
        let va = add_video_row(&pool, &clock, a.id, "va.mp4").await;
        let c = create_custom(&pool, &clock, "Only A", &[a.id])
            .await
            .unwrap();

        assert_eq!(videos_in(&pool, c.id).await.unwrap().len(), 1);
        // Flag as missing; should disappear from the view.
        sqlx::query("UPDATE videos SET missing = 1 WHERE id = ?")
            .bind(va.as_str())
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(videos_in(&pool, c.id).await.unwrap().len(), 0);
        let fetched = get(&pool, c.id).await.unwrap().unwrap();
        assert_eq!(fetched.video_count, 0);
    }

    #[tokio::test]
    async fn soft_remove_then_readd_restores_custom_collection_contents() {
        let (tmp, pool, clock) = setup().await;
        let dir_a = tmp.path().join("a");
        std::fs::create_dir_all(&dir_a).unwrap();
        let a = add_dir(&pool, &clock, &dir_a, Some("A".into()))
            .await
            .unwrap();
        let _va = add_video_row(&pool, &clock, a.id, "va.mp4").await;
        let c = create_custom(&pool, &clock, "Only A", &[a.id])
            .await
            .unwrap();
        assert_eq!(videos_in(&pool, c.id).await.unwrap().len(), 1);

        crate::directories::soft_remove(&pool, &clock, a.id)
            .await
            .unwrap();
        // All videos in the directory are flagged missing → collection empties.
        assert_eq!(videos_in(&pool, c.id).await.unwrap().len(), 0);
        // The collection_directories link is preserved.
        let dirs = directories_in(&pool, c.id).await.unwrap();
        assert_eq!(dirs.len(), 1);
        assert!(dirs[0].removed);

        // Re-add the directory (same path) → un-miss and rescan restores contents.
        crate::directories::add(&pool, &clock, &dir_a, None)
            .await
            .unwrap();
        sqlx::query("UPDATE videos SET missing = 0 WHERE directory_id = ?")
            .bind(a.id.raw())
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(videos_in(&pool, c.id).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn reject_add_removed_directory_to_custom() {
        let (tmp, pool, clock) = setup().await;
        let dir_a = tmp.path().join("a");
        std::fs::create_dir_all(&dir_a).unwrap();
        let a = add_dir(&pool, &clock, &dir_a, Some("A".into()))
            .await
            .unwrap();
        crate::directories::soft_remove(&pool, &clock, a.id)
            .await
            .unwrap();
        let c = create_custom(&pool, &clock, "C", &[]).await.unwrap();
        let err = add_directory(&pool, &clock, c.id, a.id).await.unwrap_err();
        assert!(matches!(err, MutationError::DirectoryRemoved));
    }

    #[tokio::test]
    async fn reject_add_directory_to_directory_collection() {
        let (tmp, pool, clock) = setup().await;
        let dir_a = tmp.path().join("a");
        std::fs::create_dir_all(&dir_a).unwrap();
        let a = add_dir(&pool, &clock, &dir_a, Some("A".into()))
            .await
            .unwrap();
        let err = add_directory(&pool, &clock, a.collection_id, a.id)
            .await
            .unwrap_err();
        assert!(matches!(err, MutationError::DirectoryCollectionImmutable));
    }

    #[tokio::test]
    async fn videos_in_sorts_by_filename_case_insensitive() {
        let (tmp, pool, clock) = setup().await;
        let dir_a = tmp.path().join("a");
        let dir_b = tmp.path().join("b");
        std::fs::create_dir_all(&dir_a).unwrap();
        std::fs::create_dir_all(&dir_b).unwrap();
        let a = add_dir(&pool, &clock, &dir_a, Some("A".into()))
            .await
            .unwrap();
        let b = add_dir(&pool, &clock, &dir_b, Some("B".into()))
            .await
            .unwrap();

        // Insert out of alphabetical order, across both directories, with
        // mixed case to exercise COLLATE NOCASE.
        add_video_row(&pool, &clock, a.id, "charlie.mp4").await;
        add_video_row(&pool, &clock, b.id, "Alpha.mp4").await;
        add_video_row(&pool, &clock, a.id, "bravo.mp4").await;
        add_video_row(&pool, &clock, b.id, "delta.mp4").await;

        // Directory collection A: bravo, charlie.
        let names_a: Vec<String> = videos_in(&pool, a.collection_id)
            .await
            .unwrap()
            .into_iter()
            .map(|c| c.filename)
            .collect();
        assert_eq!(names_a, vec!["bravo.mp4", "charlie.mp4"]);

        // Custom collection over both directories: Alpha, bravo, charlie, delta.
        let c = create_custom(&pool, &clock, "Both", &[a.id, b.id])
            .await
            .unwrap();
        let names: Vec<String> = videos_in(&pool, c.id)
            .await
            .unwrap()
            .into_iter()
            .map(|c| c.filename)
            .collect();
        assert_eq!(
            names,
            vec!["Alpha.mp4", "bravo.mp4", "charlie.mp4", "delta.mp4"]
        );
    }
}
