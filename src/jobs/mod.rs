//! Job queue primitives.
//!
//! Kinds and lifecycle are described in `docs/design/05-jobs-and-workers.md`.

use anyhow::{Context, Result};
use chrono::Utc;
use sqlx::{Row, SqliteConnection, SqlitePool};

use crate::ids::VideoId;

pub mod counts;
pub mod preview_plan;
pub mod reconcile;
pub mod registry;
#[cfg(test)]
mod tests;
pub mod watchdog;
pub mod worker;

pub use counts::{
    count_by_status, counts, counts_by_directory, DirectoryJobCounts, JobCounts, KindCounts,
};
pub use reconcile::{reconcile_on_startup, ReconcileReport};
pub use watchdog::reset_stuck_running;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Probe,
    Thumbnail,
    Preview,
}

impl Kind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Kind::Probe => "probe",
            Kind::Thumbnail => "thumbnail",
            Kind::Preview => "preview",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Pending,
    Running,
    Done,
    Failed,
}

impl Status {
    pub fn as_str(&self) -> &'static str {
        match self {
            Status::Pending => "pending",
            Status::Running => "running",
            Status::Done => "done",
            Status::Failed => "failed",
        }
    }
}

/// Enqueue a job on a specific connection. Returns the job id.
///
/// This is idempotent per `(kind, video_id)`: if there's already a `pending` or
/// `running` job for the same kind+video, no new row is inserted and the existing
/// id is returned. This prevents duplicate ffmpeg processes being spawned for the
/// same work (e.g. if the scanner re-enqueues before the worker picks up the previous
/// job, or if probe finishes and re-enqueues thumbnail/preview that already exist).
pub async fn enqueue_on(
    conn: &mut SqliteConnection,
    kind: Kind,
    video_id: &VideoId,
) -> Result<i64> {
    // Is there already an outstanding job for this (kind, video)?
    if let Some(row) = sqlx::query(
        "SELECT id FROM jobs \
         WHERE kind = ? AND video_id = ? AND status IN ('pending', 'running') \
         LIMIT 1",
    )
    .bind(kind.as_str())
    .bind(video_id.as_str())
    .fetch_optional(&mut *conn)
    .await
    .context("checking for existing outstanding job")?
    {
        let id: i64 = row.get(0);
        tracing::debug!(
            job_id = id,
            kind = kind.as_str(),
            video_id = %video_id,
            "enqueue skipped: outstanding job already exists"
        );
        return Ok(id);
    }

    let now_s = Utc::now().to_rfc3339();
    let row = sqlx::query(
        "INSERT INTO jobs (kind, video_id, status, created_at, updated_at) \
         VALUES (?, ?, 'pending', ?, ?) RETURNING id",
    )
    .bind(kind.as_str())
    .bind(video_id.as_str())
    .bind(&now_s)
    .bind(&now_s)
    .fetch_one(&mut *conn)
    .await
    .context("enqueueing job")?;
    let id: i64 = row.get(0);
    tracing::debug!(job_id = id, kind = kind.as_str(), video_id = %video_id, "job enqueued");
    Ok(id)
}

/// Convenience wrapper: enqueue against a pool.
#[allow(dead_code)]
pub async fn enqueue(pool: &SqlitePool, kind: Kind, video_id: &VideoId) -> Result<i64> {
    let mut conn = pool.acquire().await.context("acquiring connection")?;
    enqueue_on(&mut conn, kind, video_id).await
}
