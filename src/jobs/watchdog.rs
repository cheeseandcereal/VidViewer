//! Rescue job rows stuck in `running` state when their worker task is gone.
//!
//! Every well-behaved exit path from the worker transitions a claimed row to
//! `done`, `failed`, or deletes it. If *any* of those DB writes fail (locked
//! DB, panic mid-transaction, process signal, etc.) the row is stranded in
//! `running`. Because of `idx_jobs_outstanding_unique` (see
//! `migrations/0001_initial.sql`), no new pending row can be inserted for the
//! same `(kind, video_id)` pair until that row is cleared. Rescans would
//! appear to silently do nothing for the affected video.
//!
//! The watchdog clears the slot. It is run both periodically from the worker
//! pool and ad-hoc from the HTTP scan trigger so manual rescans can resolve
//! stuck probes immediately.

use anyhow::{Context, Result};
use sqlx::{Row, SqlitePool};

use crate::{clock::ClockRef, jobs::registry::JobRegistry};

/// Find `running` rows older than `threshold` whose id is not tracked by the
/// live [`JobRegistry`] and reset them to `pending`. Returns the number of
/// rows reset.
///
/// The registry check is the source of truth for "is a task still alive
/// behind this row" — long ffmpeg runs are fine, since they stay registered.
/// The age threshold only guards against the claim/register race window: a
/// worker transitions a row to `running` in `claim()` and then registers the
/// `AbortHandle` a few microseconds later. Without the threshold, a watchdog
/// pass landing inside that window would wrongly reset a fresh healthy claim.
pub async fn reset_stuck_running(
    pool: &SqlitePool,
    clock: &ClockRef,
    registry: &JobRegistry,
    threshold: chrono::Duration,
) -> Result<u64> {
    let cutoff = (clock.now() - threshold).to_rfc3339();
    let rows = sqlx::query(
        "SELECT id FROM jobs \
         WHERE status = 'running' AND updated_at < ?",
    )
    .bind(&cutoff)
    .fetch_all(pool)
    .await
    .context("listing stuck running jobs")?;

    let mut reset_ids: Vec<i64> = Vec::new();
    for row in &rows {
        let id: i64 = row.get("id");
        if registry.contains(id) {
            // A live worker task still owns this job; leave it alone.
            continue;
        }
        reset_ids.push(id);
    }

    if reset_ids.is_empty() {
        return Ok(0);
    }

    let now_s = clock.now().to_rfc3339();
    let placeholders = vec!["?"; reset_ids.len()].join(",");
    let sql = format!(
        "UPDATE jobs SET status = 'pending', updated_at = ? \
         WHERE status = 'running' AND id IN ({placeholders})"
    );
    let mut q = sqlx::query(&sql).bind(&now_s);
    for id in &reset_ids {
        q = q.bind(id);
    }
    let affected = q
        .execute(pool)
        .await
        .context("resetting stuck running jobs")?
        .rows_affected();

    if affected > 0 {
        tracing::warn!(
            reset = affected,
            ids = ?reset_ids,
            "watchdog reset stuck running jobs to pending"
        );
    }
    Ok(affected)
}

/// Delete historical `failed` job rows whose failure mode is no longer
/// reproducible by the current code. Two categories:
///
/// 1. `preview` and `thumbnail` jobs against `is_audio_only = 1` rows —
///    these were logged before the audio-support commits added their
///    gates. The rerun behavior today would be either "skip cleanly"
///    (preview) or "extract cover art / skip" (thumbnail).
/// 2. `preview` and `thumbnail` jobs against rows where the corresponding
///    `*_ok` flag is now `1` — the asset was successfully regenerated on
///    a later attempt, so the old failure is just stale noise.
///
/// Failed jobs against real video rows whose asset is *still* missing are
/// left in place as diagnostic history: those are the ones a user or
/// operator might actually want to investigate.
///
/// Returns the number of rows deleted. Idempotent.
pub async fn cleanup_obsolete_failed_jobs(pool: &SqlitePool) -> Result<u64> {
    let mut deleted = 0u64;

    // Audio-only rows: both preview and thumbnail failures are obsolete.
    deleted += sqlx::query(
        "DELETE FROM jobs \
         WHERE status = 'failed' \
           AND kind IN ('preview', 'thumbnail') \
           AND video_id IN (SELECT id FROM videos WHERE is_audio_only = 1)",
    )
    .execute(pool)
    .await
    .context("deleting obsolete failed jobs for audio-only rows")?
    .rows_affected();

    // Real-video rows where the thumbnail has since succeeded: the old
    // `failed thumbnail` row is just noise on the activity feed.
    deleted += sqlx::query(
        "DELETE FROM jobs \
         WHERE status = 'failed' \
           AND kind = 'thumbnail' \
           AND video_id IN (SELECT id FROM videos WHERE thumbnail_ok = 1)",
    )
    .execute(pool)
    .await
    .context("deleting failed thumbnail jobs on videos with thumbnail_ok=1")?
    .rows_affected();

    // Same for preview.
    deleted += sqlx::query(
        "DELETE FROM jobs \
         WHERE status = 'failed' \
           AND kind = 'preview' \
           AND video_id IN (SELECT id FROM videos WHERE preview_ok = 1)",
    )
    .execute(pool)
    .await
    .context("deleting failed preview jobs on videos with preview_ok=1")?
    .rows_affected();

    if deleted > 0 {
        tracing::info!(
            deleted,
            "cleaned up failed jobs whose failure mode is no longer reproducible"
        );
    }
    Ok(deleted)
}
