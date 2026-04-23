//! Job queue primitives.
//!
//! Kinds and lifecycle are described in `docs/design/05-jobs-and-workers.md`.

use anyhow::{Context, Result};
use chrono::Utc;
use sqlx::{Row, SqliteConnection, SqlitePool};

use crate::ids::VideoId;

pub mod preview_plan;
pub mod worker;

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

/// Enqueue a job on a specific connection. Returns the new job id.
pub async fn enqueue_on(
    conn: &mut SqliteConnection,
    kind: Kind,
    video_id: &VideoId,
) -> Result<i64> {
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

/// Count jobs by status. Useful for `/debug` and scan-status.
#[allow(dead_code)]
pub async fn count_by_status(pool: &SqlitePool) -> Result<(i64, i64, i64, i64)> {
    let (mut pending, mut running, mut done, mut failed) = (0i64, 0i64, 0i64, 0i64);
    let rows = sqlx::query("SELECT status, COUNT(*) FROM jobs GROUP BY status")
        .fetch_all(pool)
        .await
        .context("counting jobs by status")?;
    for row in rows {
        let status: String = row.get(0);
        let count: i64 = row.get(1);
        match status.as_str() {
            "pending" => pending = count,
            "running" => running = count,
            "done" => done = count,
            "failed" => failed = count,
            _ => {}
        }
    }
    Ok((pending, running, done, failed))
}

/// Per-kind job counts.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct KindCounts {
    pub pending: i64,
    pub running: i64,
    pub done: i64,
    pub failed: i64,
}

impl KindCounts {
    pub fn total_incomplete(&self) -> i64 {
        self.pending + self.running
    }
    pub fn total(&self) -> i64 {
        self.pending + self.running + self.done + self.failed
    }
}

#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct JobCounts {
    pub probe: KindCounts,
    pub thumbnail: KindCounts,
    pub preview: KindCounts,
}

impl JobCounts {
    /// True when any kind still has incomplete (pending/running) jobs.
    pub fn any_incomplete(&self) -> bool {
        self.probe.total_incomplete() > 0
            || self.thumbnail.total_incomplete() > 0
            || self.preview.total_incomplete() > 0
    }
}

/// Load per-kind job counts (breakdown by status).
pub async fn counts(pool: &SqlitePool) -> Result<JobCounts> {
    let rows = sqlx::query("SELECT kind, status, COUNT(*) AS n FROM jobs GROUP BY kind, status")
        .fetch_all(pool)
        .await
        .context("counting jobs by kind+status")?;
    let mut out = JobCounts::default();
    for row in rows {
        let kind: String = row.get("kind");
        let status: String = row.get("status");
        let n: i64 = row.get("n");
        let bucket = match kind.as_str() {
            "probe" => &mut out.probe,
            "thumbnail" => &mut out.thumbnail,
            "preview" => &mut out.preview,
            _ => continue,
        };
        match status.as_str() {
            "pending" => bucket.pending = n,
            "running" => bucket.running = n,
            "done" => bucket.done = n,
            "failed" => bucket.failed = n,
            _ => {}
        }
    }
    Ok(out)
}
