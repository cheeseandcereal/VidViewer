//! Read queries over the collections tables. Both directory-kind and
//! custom-kind collections compute their membership on read (see the
//! module doc for the invariant).

use anyhow::{Context, Result};
use sqlx::{Row, SqlitePool};

use super::types::{Collection, CollectionDirectory, CollectionSummary, Kind, VideoCard};
use crate::{
    db::row::{bool_from_i64, datetime_from_rfc3339},
    ids::{CollectionId, DirectoryId, VideoId},
};

// ---------------------------------------------------------------------------
// SQL snippets. The video-count and preview-thumbnail subqueries differ by
// collection kind, so they're assembled per row in `hydrate` / `preview_thumbs`.
// ---------------------------------------------------------------------------

pub(super) const SELECT_COLLECTION: &str =
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

/// Videos in a collection, returned in `id` order — a stable but otherwise
/// arbitrary sequence. Display order is the caller's responsibility.
/// Excludes `missing = 1`.
pub async fn videos_in(pool: &SqlitePool, id: CollectionId) -> Result<Vec<VideoCard>> {
    let (kind, directory_id) = match collection_kind(pool, id).await? {
        Some(t) => t,
        None => return Ok(Vec::new()),
    };
    let rows = match kind {
        Kind::Directory => {
            let dir = directory_id.map(|d| d.raw()).unwrap_or(-1);
            sqlx::query(
                "SELECT id, filename, duration_secs, thumbnail_ok, preview_ok, missing, \
                        is_audio_only, updated_at \
                 FROM videos WHERE directory_id = ? AND missing = 0 \
                 ORDER BY id",
            )
            .bind(dir)
            .fetch_all(pool)
            .await
            .context("videos_in (directory)")?
        }
        Kind::Custom => sqlx::query(
            "SELECT v.id, v.filename, v.duration_secs, v.thumbnail_ok, v.preview_ok, v.missing, \
                    v.is_audio_only, v.updated_at \
             FROM videos v \
             WHERE v.missing = 0 AND v.directory_id IN \
               (SELECT directory_id FROM collection_directories WHERE collection_id = ?) \
             ORDER BY v.id",
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
            is_audio_only: bool_from_i64(&r, "is_audio_only"),
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
pub(super) async fn collection_kind(
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
    //! Tests for the read surface of collections. Exercises
    //! `list_summaries`, `videos_in`, `directories_in`, `get`'s
    //! `video_count`, and the `Kind`-aware visibility rules.

    use super::super::{
        create_custom, // from mutations.rs — used as setup here
        mutations::{add_directory, remove_directory},
        test_helpers::{add_video_row, setup},
    };
    use super::*;
    use crate::directories::add as add_dir;

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
}
