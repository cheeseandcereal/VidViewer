//! Mutating commands for directories: add, rename, soft-remove, hard-remove.

use std::path::Path;

use anyhow::{bail, Context, Result};
use serde::Serialize;
use sqlx::{Row, SqlitePool};

use crate::{
    clock::ClockRef,
    directories::{get, internal, AddError, Directory},
    ids::{DirectoryId, VideoId},
};

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
    let path = super::validate_path(path)?;
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
        let id: i64 = sqlx::query(
            "INSERT INTO directories (path, label, added_at, removed) VALUES (?, ?, ?, 0) RETURNING id",
        )
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
///   * mark all videos in this directory `missing = 1`
///   * cancel any pending background jobs for videos in this directory
///
/// Watch history is preserved. Any `collection_directories` rows linking this
/// directory to custom collections are preserved too — the `missing = 1` flag
/// on its videos is enough to keep them out of listings, and the link will
/// start contributing again the moment the directory is re-added. Jobs
/// already in the `running` state are allowed to finish naturally — the
/// wasted work is bounded and cancelling mid-ffmpeg would require process
/// tracking that isn't worth the complexity for this case.
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
    // Hide the directory's collection. Membership is computed on read from
    // videos.directory_id, so there's nothing to clear.
    sqlx::query(
        "UPDATE collections SET hidden = 1, updated_at = ? \
         WHERE kind = 'directory' AND directory_id = ?",
    )
    .bind(&now_s)
    .bind(id.raw())
    .execute(&mut *tx)
    .await
    .context("hiding directory collection")?;

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
///     `watch_history`, the directory's own `collections` row, and any
///     `collection_directories` rows referencing this directory.
///
/// Custom collections themselves remain, but lose any reference they had to this
/// directory.
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
