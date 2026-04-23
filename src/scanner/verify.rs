//! Cache verification pass. For every video that survived the walk, check that
//! the expected derived assets exist on disk and that the DB flags are consistent.
//!
//! The invariant is `(flag = 1) ⇔ (file exists)`. If either side is off, clear
//! the flag and enqueue a fresh job via the idempotent `enqueue_on` helper.

use std::sync::atomic::Ordering;

use anyhow::{Context, Result};
use sqlx::SqlitePool;

use crate::{
    clock::ClockRef,
    ids::VideoId,
    jobs,
    scanner::{CachePaths, ScanProgress, ScanReport},
};

#[allow(clippy::too_many_arguments)]
pub(super) async fn verify_cache_for_video(
    pool: &SqlitePool,
    clock: &ClockRef,
    cache: &CachePaths,
    video_id: &VideoId,
    thumbnail_ok: bool,
    preview_ok: bool,
    duration_secs: Option<f64>,
    progress: &ScanProgress,
    report: &mut ScanReport,
) -> Result<()> {
    // Thumbnail.
    let thumb_path = cache.thumb_path(video_id);
    let thumb_exists = thumb_path.exists();
    if !(thumbnail_ok && thumb_exists) {
        let now_s = clock.now().to_rfc3339();
        let mut tx = pool.begin().await.context("begin tx")?;
        if thumbnail_ok {
            sqlx::query("UPDATE videos SET thumbnail_ok = 0, updated_at = ? WHERE id = ?")
                .bind(&now_s)
                .bind(video_id.as_str())
                .execute(&mut *tx)
                .await
                .context("clearing thumbnail_ok")?;
        }
        jobs::enqueue_on(&mut tx, jobs::Kind::Thumbnail, video_id).await?;
        tx.commit().await.context("commit tx")?;
        progress
            .recovered_thumbnail_jobs
            .fetch_add(1, Ordering::Relaxed);
        report.recovered_thumbnail_jobs += 1;
        tracing::info!(
            video_id = %video_id,
            flag = thumbnail_ok,
            file_exists = thumb_exists,
            "thumbnail cache incomplete; enqueued job"
        );
    }

    // Preview (only when duration is usable).
    if duration_secs.unwrap_or(0.0) > 0.0 {
        let sheet = cache.preview_sheet_path(video_id);
        let vtt = cache.preview_vtt_path(video_id);
        let preview_files_present = sheet.exists() && vtt.exists();
        if !(preview_ok && preview_files_present) {
            let now_s = clock.now().to_rfc3339();
            let mut tx = pool.begin().await.context("begin tx")?;
            if preview_ok {
                sqlx::query("UPDATE videos SET preview_ok = 0, updated_at = ? WHERE id = ?")
                    .bind(&now_s)
                    .bind(video_id.as_str())
                    .execute(&mut *tx)
                    .await
                    .context("clearing preview_ok")?;
            }
            jobs::enqueue_on(&mut tx, jobs::Kind::Preview, video_id).await?;
            tx.commit().await.context("commit tx")?;
            progress
                .recovered_preview_jobs
                .fetch_add(1, Ordering::Relaxed);
            report.recovered_preview_jobs += 1;
            tracing::info!(
                video_id = %video_id,
                flag = preview_ok,
                sheet_exists = sheet.exists(),
                vtt_exists = vtt.exists(),
                "preview cache incomplete; enqueued job"
            );
        }
    }

    Ok(())
}
