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

/// Per-directory pending+running job counts, plus a rolling completion baseline so the
/// UI can show per-directory progress ("78 / 142 previews") without leaking a global
/// all-time counter. Only incomplete jobs are tracked; there's no cumulative history.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct DirectoryJobCounts {
    pub probe_incomplete: i64,
    pub thumbnail_incomplete: i64,
    pub preview_incomplete: i64,
    pub failed: i64,
    /// Current snapshot of how many videos in this directory still lack a thumbnail.
    pub thumbnail_pending_videos: i64,
    /// Current snapshot of how many videos in this directory still lack a preview sheet
    /// (excluding videos with unknown duration, which can never produce previews).
    pub preview_pending_videos: i64,
    /// Total playable (non-missing) videos in this directory — denominator for progress.
    pub video_total: i64,
}

impl DirectoryJobCounts {
    pub fn total_incomplete(&self) -> i64 {
        self.probe_incomplete + self.thumbnail_incomplete + self.preview_incomplete
    }
    pub fn busy(&self) -> bool {
        self.total_incomplete() > 0
    }
}

/// Return a map of `directory_id -> DirectoryJobCounts`. Directories with no videos and
/// no jobs are omitted.
pub async fn counts_by_directory(
    pool: &SqlitePool,
) -> Result<std::collections::HashMap<i64, DirectoryJobCounts>> {
    let mut out: std::collections::HashMap<i64, DirectoryJobCounts> =
        std::collections::HashMap::new();

    // Incomplete jobs (pending + running) bucketed by directory + kind.
    let rows = sqlx::query(
        "SELECT v.directory_id AS dir_id, j.kind AS kind, COUNT(*) AS n \
         FROM jobs j JOIN videos v ON v.id = j.video_id \
         WHERE j.status IN ('pending', 'running') \
         GROUP BY v.directory_id, j.kind",
    )
    .fetch_all(pool)
    .await
    .context("per-directory incomplete job counts")?;
    for row in rows {
        let dir_id: i64 = row.get("dir_id");
        let kind: String = row.get("kind");
        let n: i64 = row.get("n");
        let entry = out.entry(dir_id).or_default();
        match kind.as_str() {
            "probe" => entry.probe_incomplete = n,
            "thumbnail" => entry.thumbnail_incomplete = n,
            "preview" => entry.preview_incomplete = n,
            _ => {}
        }
    }

    // Failed counts per directory (all kinds, all-time since last reset).
    let rows = sqlx::query(
        "SELECT v.directory_id AS dir_id, COUNT(*) AS n \
         FROM jobs j JOIN videos v ON v.id = j.video_id \
         WHERE j.status = 'failed' \
         GROUP BY v.directory_id",
    )
    .fetch_all(pool)
    .await
    .context("per-directory failed job counts")?;
    for row in rows {
        let dir_id: i64 = row.get("dir_id");
        let n: i64 = row.get("n");
        out.entry(dir_id).or_default().failed = n;
    }

    // Per-directory snapshot of how many live videos still need thumbnails / previews.
    let rows = sqlx::query(
        "SELECT directory_id, \
            SUM(CASE WHEN thumbnail_ok = 0 THEN 1 ELSE 0 END) AS t_need, \
            SUM(CASE WHEN preview_ok = 0 AND duration_secs IS NOT NULL AND duration_secs > 0 THEN 1 ELSE 0 END) AS p_need, \
            COUNT(*) AS total \
         FROM videos \
         WHERE missing = 0 \
         GROUP BY directory_id",
    )
    .fetch_all(pool)
    .await
    .context("per-directory video snapshot")?;
    for row in rows {
        let dir_id: i64 = row.get("directory_id");
        let t_need: Option<i64> = row.get("t_need");
        let p_need: Option<i64> = row.get("p_need");
        let total: i64 = row.get("total");
        let entry = out.entry(dir_id).or_default();
        entry.thumbnail_pending_videos = t_need.unwrap_or(0);
        entry.preview_pending_videos = p_need.unwrap_or(0);
        entry.video_total = total;
    }

    Ok(out)
}

/// Report produced by [`reconcile_on_startup`].
#[derive(Debug, Default, Clone)]
pub struct ReconcileReport {
    /// Jobs deleted because their video has been deleted.
    pub dropped_orphan_video: u64,
    /// Jobs deleted because the video's directory has been soft-removed.
    pub dropped_removed_dir: u64,
    /// Jobs deleted because the video has been marked missing.
    pub dropped_missing_video: u64,
    /// `running` jobs reset back to `pending` (orphaned by a prior crash).
    pub reset_running: u64,
}

/// Reconcile the jobs table against current reality. Intended to run once at startup,
/// before workers are spawned.
///
/// Rules:
/// - Any job whose `video_id` no longer exists is deleted.
/// - Any job whose video's directory is soft-removed (`directories.removed = 1`) is deleted.
/// - Any job whose video is flagged `missing = 1` is deleted — the file isn't on disk anymore,
///   so generating thumbnails/previews would fail.
/// - Any job in `running` state is orphaned (the process that claimed it is gone); reset it
///   back to `pending` so a worker picks it up cleanly.
///
/// Only targets `pending` and `running` — `done` and `failed` rows are preserved as history.
pub async fn reconcile_on_startup(pool: &SqlitePool) -> Result<ReconcileReport> {
    let mut report = ReconcileReport::default();
    let mut tx = pool.begin().await.context("begin reconcile tx")?;

    // Drop jobs whose video no longer exists.
    let res = sqlx::query(
        "DELETE FROM jobs \
         WHERE status IN ('pending', 'running') \
         AND video_id NOT IN (SELECT id FROM videos)",
    )
    .execute(&mut *tx)
    .await
    .context("deleting jobs with orphaned video_id")?;
    report.dropped_orphan_video = res.rows_affected();

    // Drop jobs whose video's directory has been soft-removed.
    let res = sqlx::query(
        "DELETE FROM jobs \
         WHERE status IN ('pending', 'running') \
         AND video_id IN ( \
            SELECT v.id FROM videos v \
            JOIN directories d ON d.id = v.directory_id \
            WHERE d.removed = 1 \
         )",
    )
    .execute(&mut *tx)
    .await
    .context("deleting jobs whose directory was soft-removed")?;
    report.dropped_removed_dir = res.rows_affected();

    // Drop jobs for missing videos — file isn't on disk, so ffmpeg would fail.
    let res = sqlx::query(
        "DELETE FROM jobs \
         WHERE status IN ('pending', 'running') \
         AND video_id IN (SELECT id FROM videos WHERE missing = 1)",
    )
    .execute(&mut *tx)
    .await
    .context("deleting jobs for missing videos")?;
    report.dropped_missing_video = res.rows_affected();

    // Reset remaining 'running' jobs — they were left behind by a prior crash.
    let now_s = Utc::now().to_rfc3339();
    let res = sqlx::query(
        "UPDATE jobs SET status = 'pending', updated_at = ? \
         WHERE status = 'running'",
    )
    .bind(&now_s)
    .execute(&mut *tx)
    .await
    .context("resetting orphaned running jobs")?;
    report.reset_running = res.rows_affected();

    tx.commit().await.context("commit reconcile tx")?;
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock;
    use crate::directories::{add as add_dir, soft_remove};

    async fn setup() -> (tempfile::TempDir, SqlitePool) {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = crate::config::Config {
            backup_dir: tmp.path().join("backups"),
            ..crate::config::Config::default()
        };
        let db_path = tmp.path().join("vidviewer.db");
        let pool = crate::db::init(&cfg, &db_path).await.unwrap();
        (tmp, pool)
    }

    #[tokio::test]
    async fn reconcile_drops_jobs_for_removed_directory_and_resets_running() {
        let (tmp, pool) = setup().await;
        let clock = clock::system();

        // Two directories, each with a video.
        let a = tmp.path().join("a");
        let b = tmp.path().join("b");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        std::fs::write(a.join("x.mp4"), b"x").unwrap();
        std::fs::write(b.join("y.mp4"), b"y").unwrap();

        let dir_a = add_dir(&pool, &clock, &a, None).await.unwrap();
        let _dir_b = add_dir(&pool, &clock, &b, None).await.unwrap();
        crate::scanner::scan_all(&pool, &clock).await.unwrap();

        // Two probe jobs enqueued; simulate one of them as 'running' to look like a crash.
        let a_video_id: String =
            sqlx::query_scalar("SELECT id FROM videos WHERE directory_id = ? LIMIT 1")
                .bind(dir_a.id.raw())
                .fetch_one(&pool)
                .await
                .unwrap();
        sqlx::query("UPDATE jobs SET status = 'running' WHERE video_id = ?")
            .bind(&a_video_id)
            .execute(&pool)
            .await
            .unwrap();

        // Soft-remove directory A. Its pending jobs would be cleared by soft_remove,
        // but the 'running' one was skipped (worker running), so it's still there.
        soft_remove(&pool, &clock, dir_a.id).await.unwrap();
        let (_, running_before, _, _) = count_by_status(&pool).await.unwrap();
        assert_eq!(
            running_before, 1,
            "running job for soft-removed directory should still exist pre-reconcile"
        );

        let report = reconcile_on_startup(&pool).await.unwrap();
        // The 'running' job for the removed-directory video should be dropped.
        assert!(report.dropped_removed_dir >= 1);

        // Directory B's pending job should have been reset to pending (it was
        // already pending, so reset_running may be zero here — just confirm it
        // survived reconciliation).
        let (pending_after, running_after, _, _) = count_by_status(&pool).await.unwrap();
        assert_eq!(
            running_after, 0,
            "no jobs should remain in 'running' after reconcile"
        );
        assert!(
            pending_after >= 1,
            "directory B's job should still be pending"
        );
    }

    #[tokio::test]
    async fn reconcile_resets_running_for_active_video() {
        let (tmp, pool) = setup().await;
        let clock = clock::system();
        let a = tmp.path().join("a");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::write(a.join("x.mp4"), b"x").unwrap();
        add_dir(&pool, &clock, &a, None).await.unwrap();
        crate::scanner::scan_all(&pool, &clock).await.unwrap();

        sqlx::query("UPDATE jobs SET status = 'running'")
            .execute(&pool)
            .await
            .unwrap();
        let report = reconcile_on_startup(&pool).await.unwrap();
        assert_eq!(report.reset_running, 1);
        let (pending, running, _, _) = count_by_status(&pool).await.unwrap();
        assert_eq!(pending, 1);
        assert_eq!(running, 0);
    }
}
