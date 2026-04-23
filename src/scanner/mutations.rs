//! DB mutation helpers used by the scanner's walk phase.
//!
//! These operate on individual videos and are designed to be called from
//! `walk::scan_one`. Each helper is a small, focused transaction; grouping them
//! here keeps the walk orchestration readable.

use anyhow::{Context, Result};
use sqlx::SqlitePool;

use crate::{
    clock::ClockRef,
    directories,
    ids::{CollectionId, VideoId},
    jobs,
};

pub(super) async fn insert_new_video(
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

pub(super) async fn update_changed_video(
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

pub(super) async fn mark_missing(
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
    Ok(())
}

/// Un-mark a video as missing without touching `thumbnail_ok` / `preview_ok`.
///
/// Used on re-add of a soft-removed directory when the file's size and mtime match
/// the stored row: we only need to flip `missing = 0`, touch `updated_at`, and
/// re-insert the directory-collection membership. The post-walk cache verification
/// pass then detects any missing cache files and re-enqueues only what's needed.
pub(super) async fn un_mark_missing(
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

pub(super) async fn add_to_directory_collection(
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
