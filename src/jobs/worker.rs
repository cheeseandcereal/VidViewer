//! Job worker loop. Runs in two lanes: general (probe + thumbnail) and preview.
//!
//! See `docs/design/05-jobs-and-workers.md`.

use std::{path::PathBuf, time::Duration};

use anyhow::{anyhow, Context, Result};
use sqlx::{Row, SqlitePool};
use tokio::task::JoinHandle;
use tracing::{error, info};

use crate::{
    clock::ClockRef,
    ids::VideoId,
    jobs::{
        self,
        preview_plan::{self, PlanInput},
        Kind, Status,
    },
    video_tool::VideoToolRef,
};

#[derive(Clone)]
pub struct Workers {
    pub pool: SqlitePool,
    pub clock: ClockRef,
    pub video_tool: VideoToolRef,
    pub thumbnail_width: u32,
    pub preview_min_interval: f64,
    pub preview_target_count: u32,
}

impl Workers {
    pub fn spawn_all(
        self,
        general_concurrency: u32,
        preview_concurrency: u32,
    ) -> Vec<JoinHandle<()>> {
        let mut handles = Vec::new();
        for _ in 0..general_concurrency {
            let w = self.clone();
            handles.push(tokio::spawn(async move { w.run_lane(Lane::General).await }));
        }
        for _ in 0..preview_concurrency {
            let w = self.clone();
            handles.push(tokio::spawn(async move { w.run_lane(Lane::Preview).await }));
        }
        handles
    }

    async fn run_lane(self, lane: Lane) {
        loop {
            match self.claim(lane).await {
                Ok(Some(job)) => {
                    if let Err(err) = self.process(&job).await {
                        let msg = format!("{err:#}");
                        error!(job_id = job.id, kind = %job.kind.as_str(), error = %msg, "job failed");
                        let _ = self.mark(job.id, Status::Failed, Some(&msg)).await;
                    } else {
                        let _ = self.mark(job.id, Status::Done, None).await;
                    }
                }
                Ok(None) => {
                    tokio::time::sleep(Duration::from_millis(500)).await;
                }
                Err(err) => {
                    error!(error = %err, "worker claim failed");
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
            }
        }
    }

    async fn claim(&self, lane: Lane) -> Result<Option<Job>> {
        // Atomic claim: read one pending job in this lane, try to transition to running.
        let kinds = lane.kinds();
        let placeholders = kinds.iter().map(|_| "?").collect::<Vec<_>>().join(",");
        let select_sql = format!(
            "SELECT id, kind, video_id FROM jobs \
             WHERE status = 'pending' AND kind IN ({placeholders}) \
             ORDER BY id LIMIT 1"
        );
        let mut query = sqlx::query(&select_sql);
        for k in kinds {
            query = query.bind(k.as_str());
        }
        let Some(row) = query
            .fetch_optional(&self.pool)
            .await
            .context("selecting pending job")?
        else {
            return Ok(None);
        };
        let id: i64 = row.get("id");
        let kind_str: String = row.get("kind");
        let video_id: String = row.get("video_id");

        let affected = sqlx::query(
            "UPDATE jobs SET status = 'running', updated_at = ? WHERE id = ? AND status = 'pending'",
        )
        .bind(self.clock.now().to_rfc3339())
        .bind(id)
        .execute(&self.pool)
        .await
        .context("claiming job")?
        .rows_affected();

        if affected == 0 {
            // Lost the race to another worker.
            return Ok(None);
        }

        let kind = match kind_str.as_str() {
            "probe" => Kind::Probe,
            "thumbnail" => Kind::Thumbnail,
            "preview" => Kind::Preview,
            other => return Err(anyhow!("unknown job kind '{other}'")),
        };

        Ok(Some(Job {
            id,
            kind,
            video_id: VideoId(video_id),
        }))
    }

    async fn process(&self, job: &Job) -> Result<()> {
        match job.kind {
            Kind::Probe => self.run_probe(&job.video_id).await,
            Kind::Thumbnail => self.run_thumbnail(&job.video_id).await,
            Kind::Preview => self.run_preview(&job.video_id).await,
        }
    }

    async fn run_probe(&self, video_id: &VideoId) -> Result<()> {
        let (abs_path, _duration) = self.load_for_job(video_id).await?;
        let result = self.video_tool.probe(&abs_path).await?;

        let now_s = self.clock.now().to_rfc3339();
        sqlx::query(
            "UPDATE videos SET duration_secs = ?, width = ?, height = ?, codec = ?, updated_at = ? \
             WHERE id = ?",
        )
        .bind(result.duration_secs)
        .bind(result.width)
        .bind(result.height)
        .bind(&result.codec)
        .bind(&now_s)
        .bind(video_id.as_str())
        .execute(&self.pool)
        .await
        .context("updating video probe result")?;

        // Enqueue the derived jobs now that we know duration.
        let mut conn = self.pool.acquire().await?;
        jobs::enqueue_on(&mut conn, Kind::Thumbnail, video_id).await?;
        if result.duration_secs.unwrap_or(0.0) > 0.0 {
            jobs::enqueue_on(&mut conn, Kind::Preview, video_id).await?;
        }
        info!(video_id = %video_id, duration = ?result.duration_secs, "probe complete");
        Ok(())
    }

    async fn run_thumbnail(&self, video_id: &VideoId) -> Result<()> {
        let (abs_path, duration) = self.load_for_job(video_id).await?;
        // Use the midpoint of the video for the poster frame. If duration is unknown
        // or zero, fall back to 5 seconds in (a safe default that skips intros/logos
        // on most clips).
        let at = match duration {
            Some(d) if d > 0.0 => d * 0.5,
            _ => 5.0,
        };
        let dst = crate::config::thumb_cache_dir().join(format!("{}.jpg", video_id.as_str()));
        self.video_tool
            .thumbnail(&abs_path, &dst, at, self.thumbnail_width)
            .await?;

        let now_s = self.clock.now().to_rfc3339();
        sqlx::query("UPDATE videos SET thumbnail_ok = 1, updated_at = ? WHERE id = ?")
            .bind(&now_s)
            .bind(video_id.as_str())
            .execute(&self.pool)
            .await
            .context("marking thumbnail_ok")?;
        info!(video_id = %video_id, "thumbnail complete");
        Ok(())
    }

    async fn run_preview(&self, video_id: &VideoId) -> Result<()> {
        let (abs_path, duration) = self.load_for_job(video_id).await?;
        let duration = duration.unwrap_or(0.0);
        if duration <= 0.0 {
            return Err(anyhow!("cannot generate preview without duration"));
        }
        let Some(plan) = preview_plan::plan(&PlanInput {
            duration_secs: duration,
            min_interval_secs: self.preview_min_interval,
            target_count: self.preview_target_count,
        }) else {
            return Err(anyhow!("plan generation returned None despite duration"));
        };

        let preview_dir = crate::config::preview_cache_dir();
        let sheet_path = preview_dir.join(format!("{}.jpg", video_id.as_str()));
        let vtt_path = preview_dir.join(format!("{}.vtt", video_id.as_str()));

        self.video_tool
            .previews(&abs_path, &sheet_path, &plan, duration)
            .await?;

        let updated_at_epoch = self.clock.now().timestamp();
        let sheet_url = format!("/previews/{}.jpg?v={updated_at_epoch}", video_id.as_str());
        let vtt = preview_plan::render_vtt(&plan, &sheet_url, duration);
        tokio::fs::create_dir_all(&preview_dir)
            .await
            .with_context(|| format!("creating {}", preview_dir.display()))?;
        tokio::fs::write(&vtt_path, vtt)
            .await
            .with_context(|| format!("writing {}", vtt_path.display()))?;

        let now_s = self.clock.now().to_rfc3339();
        sqlx::query("UPDATE videos SET preview_ok = 1, updated_at = ? WHERE id = ?")
            .bind(&now_s)
            .bind(video_id.as_str())
            .execute(&self.pool)
            .await
            .context("marking preview_ok")?;
        info!(video_id = %video_id, "preview complete");
        Ok(())
    }

    /// Load the absolute path and current duration for a video.
    async fn load_for_job(&self, video_id: &VideoId) -> Result<(PathBuf, Option<f64>)> {
        let row = sqlx::query(
            "SELECT d.path, v.relative_path, v.duration_secs \
             FROM videos v JOIN directories d ON d.id = v.directory_id \
             WHERE v.id = ?",
        )
        .bind(video_id.as_str())
        .fetch_optional(&self.pool)
        .await
        .context("loading video for job")?
        .ok_or_else(|| anyhow!("video {video_id} not found"))?;

        let dir_path: String = row.get("path");
        let relative_path: String = row.get("relative_path");
        let duration: Option<f64> = row.get("duration_secs");

        let abs = PathBuf::from(dir_path).join(relative_path);
        Ok((abs, duration))
    }

    async fn mark(&self, id: i64, status: Status, error: Option<&str>) -> Result<()> {
        let now_s = self.clock.now().to_rfc3339();
        sqlx::query("UPDATE jobs SET status = ?, error = ?, updated_at = ? WHERE id = ?")
            .bind(status.as_str())
            .bind(error)
            .bind(&now_s)
            .bind(id)
            .execute(&self.pool)
            .await
            .context("updating job status")?;
        Ok(())
    }
}

#[derive(Debug, Clone, Copy)]
enum Lane {
    General,
    Preview,
}

impl Lane {
    fn kinds(&self) -> &'static [Kind] {
        match self {
            Lane::General => &[Kind::Probe, Kind::Thumbnail],
            Lane::Preview => &[Kind::Preview],
        }
    }
}

#[derive(Debug, Clone)]
pub struct Job {
    pub id: i64,
    pub kind: Kind,
    pub video_id: VideoId,
}
