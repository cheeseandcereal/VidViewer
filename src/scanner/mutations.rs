//! DB mutation helpers used by the scanner's walk phase.
//!
//! These operate on individual videos and are designed to be called from
//! `walk::scan_one`. Each helper is a small, focused transaction; grouping them
//! here keeps the walk orchestration readable.
//!
//! Collection membership is **not** materialized: directory collections show
//! their videos via `videos.directory_id = collections.directory_id`, and
//! custom collections show the union of videos in their linked directories.
//! The scanner therefore only needs to manage the `videos` table.

use anyhow::{Context, Result};
use sqlx::SqlitePool;

use crate::{clock::ClockRef, directories, ids::VideoId, jobs};

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

    jobs::enqueue_on(&mut tx, jobs::Kind::Probe, &video_id).await?;

    tx.commit().await.context("commit tx")?;

    tracing::debug!(video_id = %video_id, path = %rel, "new video indexed");
    Ok(())
}

pub(super) async fn update_changed_video(
    pool: &SqlitePool,
    clock: &ClockRef,
    _dir: &directories::Directory,
    video_id: &VideoId,
    size: i64,
    mtime: i64,
    _was_missing: bool,
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

    jobs::enqueue_on(&mut tx, jobs::Kind::Probe, video_id).await?;

    tx.commit().await.context("commit tx")?;
    Ok(())
}

pub(super) async fn mark_missing(
    pool: &SqlitePool,
    clock: &ClockRef,
    _dir: &directories::Directory,
    video_id: &VideoId,
) -> Result<()> {
    let now_s = clock.now().to_rfc3339();
    sqlx::query("UPDATE videos SET missing = 1, updated_at = ? WHERE id = ?")
        .bind(&now_s)
        .bind(video_id.as_str())
        .execute(pool)
        .await
        .context("flagging missing")?;
    Ok(())
}

/// Un-mark a video as missing without touching `thumbnail_ok` / `preview_ok`.
///
/// Used on re-add of a soft-removed directory when the file's size and mtime match
/// the stored row: we only need to flip `missing = 0` and touch `updated_at`. The
/// post-walk cache verification pass then detects any missing cache files and
/// re-enqueues only what's needed.
pub(super) async fn un_mark_missing(
    pool: &SqlitePool,
    clock: &ClockRef,
    _dir: &directories::Directory,
    video_id: &VideoId,
) -> Result<()> {
    let now_s = clock.now().to_rfc3339();
    sqlx::query("UPDATE videos SET missing = 0, updated_at = ? WHERE id = ?")
        .bind(&now_s)
        .bind(video_id.as_str())
        .execute(pool)
        .await
        .context("clearing missing flag")?;
    Ok(())
}
