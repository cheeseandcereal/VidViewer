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
