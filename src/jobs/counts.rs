//! Diagnostic job-count queries. Used by `/debug`, scan-status, and the Settings UI.

use std::collections::HashMap;

use anyhow::{Context, Result};
use sqlx::{Row, SqlitePool};

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
pub async fn counts_by_directory(pool: &SqlitePool) -> Result<HashMap<i64, DirectoryJobCounts>> {
    let mut out: HashMap<i64, DirectoryJobCounts> = HashMap::new();

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
