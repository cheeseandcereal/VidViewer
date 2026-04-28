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

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::SqlitePool;

    async fn setup() -> (tempfile::TempDir, SqlitePool) {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = crate::config::Config {
            data_dir: tmp.path().to_path_buf(),
            backup_dir: tmp.path().join("backups"),
            ..crate::config::Config::default()
        };
        let db_path = cfg.database_path();
        let pool = crate::db::init(&cfg, &db_path).await.unwrap();
        (tmp, pool)
    }

    async fn insert_job(pool: &SqlitePool, kind: &str, status: &str, video_id: &str) {
        let now = chrono::Utc::now().to_rfc3339();
        sqlx::query(
            "INSERT INTO jobs (kind, video_id, status, created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?)",
        )
        .bind(kind)
        .bind(video_id)
        .bind(status)
        .bind(&now)
        .bind(&now)
        .execute(pool)
        .await
        .unwrap();
    }

    /// Insert a video row directly so jobs can reference it in joins.
    async fn insert_video(
        pool: &SqlitePool,
        vid: &str,
        dir_id: i64,
        thumbnail_ok: i64,
        preview_ok: i64,
        duration: f64,
    ) {
        let now = chrono::Utc::now().to_rfc3339();
        sqlx::query(
            "INSERT INTO videos (id, directory_id, relative_path, filename, size_bytes, \
             mtime_unix, duration_secs, thumbnail_ok, preview_ok, missing, is_audio_only, \
             created_at, updated_at) \
             VALUES (?, ?, ?, ?, 1, 1, ?, ?, ?, 0, 0, ?, ?)",
        )
        .bind(vid)
        .bind(dir_id)
        .bind(vid)
        .bind(vid)
        .bind(duration)
        .bind(thumbnail_ok)
        .bind(preview_ok)
        .bind(&now)
        .bind(&now)
        .execute(pool)
        .await
        .unwrap();
    }

    async fn insert_directory(pool: &SqlitePool, path: &str) -> i64 {
        let now = chrono::Utc::now().to_rfc3339();
        let row = sqlx::query(
            "INSERT INTO directories (path, label, added_at, removed) \
             VALUES (?, ?, ?, 0) RETURNING id",
        )
        .bind(path)
        .bind(path)
        .bind(&now)
        .fetch_one(pool)
        .await
        .unwrap();
        use sqlx::Row;
        row.get(0)
    }

    #[test]
    fn kind_counts_arithmetic() {
        let c = KindCounts {
            pending: 3,
            running: 1,
            done: 10,
            failed: 2,
        };
        assert_eq!(c.total_incomplete(), 4);
        assert_eq!(c.total(), 16);
    }

    #[test]
    fn any_incomplete_honors_each_kind() {
        let mut c = JobCounts::default();
        assert!(!c.any_incomplete());
        c.probe.pending = 1;
        assert!(c.any_incomplete());
        c.probe.pending = 0;
        c.thumbnail.running = 1;
        assert!(c.any_incomplete());
        c.thumbnail.running = 0;
        c.preview.pending = 1;
        assert!(c.any_incomplete());
    }

    #[test]
    fn directory_job_counts_busy_reflects_any_incomplete() {
        let mut d = DirectoryJobCounts::default();
        assert!(!d.busy());
        d.probe_incomplete = 1;
        assert!(d.busy());
        d.probe_incomplete = 0;
        d.thumbnail_incomplete = 1;
        assert!(d.busy());
        d.thumbnail_incomplete = 0;
        d.preview_incomplete = 1;
        assert!(d.busy());
    }

    #[tokio::test]
    async fn count_by_status_buckets_by_each_status() {
        let (_tmp, pool) = setup().await;
        let dir_id = insert_directory(&pool, "/tmp/x").await;
        insert_video(&pool, "v1", dir_id, 0, 0, 60.0).await;
        insert_video(&pool, "v2", dir_id, 0, 0, 60.0).await;
        insert_video(&pool, "v3", dir_id, 0, 0, 60.0).await;
        insert_video(&pool, "v4", dir_id, 0, 0, 60.0).await;
        insert_job(&pool, "probe", "pending", "v1").await;
        insert_job(&pool, "probe", "running", "v2").await;
        insert_job(&pool, "thumbnail", "done", "v3").await;
        insert_job(&pool, "preview", "failed", "v4").await;

        let (p, r, d, f) = count_by_status(&pool).await.unwrap();
        assert_eq!((p, r, d, f), (1, 1, 1, 1));
    }

    #[tokio::test]
    async fn counts_breaks_down_by_kind_and_status() {
        let (_tmp, pool) = setup().await;
        let dir_id = insert_directory(&pool, "/tmp/y").await;
        for (i, (kind, status)) in [
            ("probe", "pending"),
            ("probe", "done"),
            ("thumbnail", "pending"),
            ("thumbnail", "running"),
            ("thumbnail", "failed"),
            ("preview", "done"),
            ("preview", "done"),
        ]
        .iter()
        .enumerate()
        {
            let vid = format!("v{i}");
            insert_video(&pool, &vid, dir_id, 0, 0, 60.0).await;
            insert_job(&pool, kind, status, &vid).await;
        }

        let c = counts(&pool).await.unwrap();
        assert_eq!(c.probe.pending, 1);
        assert_eq!(c.probe.done, 1);
        assert_eq!(c.thumbnail.pending, 1);
        assert_eq!(c.thumbnail.running, 1);
        assert_eq!(c.thumbnail.failed, 1);
        assert_eq!(c.preview.done, 2);
        assert_eq!(c.probe.total(), 2);
        assert!(c.any_incomplete());
    }

    #[tokio::test]
    async fn counts_by_directory_aggregates_correctly() {
        let (_tmp, pool) = setup().await;
        let a = insert_directory(&pool, "/tmp/a").await;
        let b = insert_directory(&pool, "/tmp/b").await;

        // Directory A: 2 videos, one with a pending probe and one with a failed thumbnail.
        insert_video(&pool, "a1", a, 0, 0, 30.0).await;
        insert_video(&pool, "a2", a, 1, 1, 30.0).await;
        insert_job(&pool, "probe", "pending", "a1").await;
        insert_job(&pool, "thumbnail", "failed", "a1").await;

        // Directory B: 1 video with a running preview.
        insert_video(&pool, "b1", b, 0, 0, 30.0).await;
        insert_job(&pool, "preview", "running", "b1").await;

        let map = counts_by_directory(&pool).await.unwrap();
        let dc_a = map.get(&a).expect("dir A present");
        assert_eq!(dc_a.probe_incomplete, 1);
        assert_eq!(dc_a.thumbnail_incomplete, 0);
        assert_eq!(dc_a.failed, 1);
        assert_eq!(dc_a.video_total, 2);
        // a1 lacks thumbnail + preview; a2 has both.
        assert_eq!(dc_a.thumbnail_pending_videos, 1);
        assert_eq!(dc_a.preview_pending_videos, 1);
        assert!(dc_a.busy());

        let dc_b = map.get(&b).expect("dir B present");
        assert_eq!(dc_b.preview_incomplete, 1);
        assert_eq!(dc_b.video_total, 1);
    }
}
