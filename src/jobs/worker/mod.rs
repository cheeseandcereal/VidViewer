//! Job worker loop. Runs in two lanes: general (probe + thumbnail) and preview.
//!
//! See `docs/design/05-jobs-and-workers.md`.
//!
//! Split across two files:
//! - `worker/mod.rs` (this file): the `Workers` struct, `spawn_all`, the
//!   long-poll `run_lane` loop, `claim`, `process` dispatch,
//!   `job_is_still_relevant` pre-flight, the row loaders, `mark`, the
//!   `Lane` / `Job` types, and the periodic watchdog driver.
//! - [`handlers`]: the three per-kind methods `run_probe`,
//!   `run_thumbnail`, `run_preview`. They live on `Workers` and are
//!   invoked by `process` here.

use std::{path::PathBuf, sync::Arc, time::Duration};

use anyhow::{anyhow, Context, Result};
use sqlx::{Row, SqlitePool};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, Instrument};

use crate::{
    clock::ClockRef,
    config::Config,
    ids::VideoId,
    jobs::{self, Kind, Status},
    video_tool::VideoToolRef,
};

mod handlers;

#[derive(Clone)]
pub struct Workers {
    pub pool: SqlitePool,
    pub clock: ClockRef,
    pub config: Arc<Config>,
    pub video_tool: VideoToolRef,
    pub thumb_dir: PathBuf,
    pub preview_dir: PathBuf,
    pub registry: crate::jobs::registry::JobRegistry,
}

impl Workers {
    pub fn spawn_all(
        self,
        general_concurrency: u32,
        preview_concurrency: u32,
    ) -> Vec<JoinHandle<()>> {
        info!(
            general_concurrency,
            preview_concurrency,
            thumbnail_width = self.config.thumbnail_width,
            preview_min_interval = self.config.preview_min_interval,
            preview_target_count = self.config.preview_target_count,
            "starting job workers"
        );
        let mut handles = Vec::new();
        for _ in 0..general_concurrency {
            let w = self.clone();
            handles.push(tokio::spawn(async move { w.run_lane(Lane::General).await }));
        }
        for _ in 0..preview_concurrency {
            let w = self.clone();
            handles.push(tokio::spawn(async move { w.run_lane(Lane::Preview).await }));
        }
        // Watchdog: finds `running` rows that no live worker task is tracking
        // and resets them to `pending` so they can be re-claimed. Without this
        // a job row could be stuck in `running` forever if the owning task
        // disappears without cleaning up (e.g. a DB write failure on `mark`,
        // an external process signal, or any other edge case we haven't
        // anticipated). Runs periodically; conservative thresholds mean it
        // never touches jobs that are actually making progress.
        {
            let w = self.clone();
            handles.push(tokio::spawn(async move { w.run_stuck_watchdog().await }));
        }
        handles
    }

    async fn run_lane(self, lane: Lane) {
        loop {
            match self.claim(lane).await {
                Ok(Some(job)) => {
                    let span = tracing::info_span!(
                        "job",
                        job_id = job.id,
                        kind = job.kind.as_str(),
                        video_id = %job.video_id,
                    );
                    let started = std::time::Instant::now();
                    span.in_scope(|| info!("job started"));

                    // Spawn the actual work as a separate task so we have an
                    // `AbortHandle` to expose via the registry. Abort interrupts
                    // any currently-awaiting `.await` (e.g. Child::wait) and
                    // `kill_on_drop(true)` SIGKILLs the ffmpeg child.
                    //
                    // The `CancellationToken` alongside it is a cooperative
                    // flag polled between ffmpeg invocations inside
                    // `VideoTool::previews`, so the preview loop stops spawning
                    // more ffmpegs as soon as cancellation is signalled.
                    let token = CancellationToken::new();
                    let w = self.clone();
                    let job_clone = job.clone();
                    let token_for_task = token.clone();
                    let task: tokio::task::JoinHandle<Result<()>> = tokio::spawn(
                        async move { w.process(&job_clone, &token_for_task).await }
                            .instrument(span.clone()),
                    );
                    let abort = task.abort_handle();
                    self.registry
                        .register(job.id, job.video_id.clone(), abort, token);

                    let outcome = task.await;
                    self.registry.deregister(job.id);

                    let elapsed_ms = started.elapsed().as_millis() as u64;
                    let _enter = span.enter();
                    match outcome {
                        Ok(Ok(())) => {
                            info!(elapsed_ms, "job done");
                            if let Err(err) = self.mark(job.id, Status::Done, None).await {
                                error!(error = %err, "failed to mark job done; watchdog will reset it");
                            }
                        }
                        Ok(Err(err)) => {
                            let msg = format!("{err:#}");
                            error!(elapsed_ms, error = %msg, "job failed");
                            if let Err(mark_err) =
                                self.mark(job.id, Status::Failed, Some(&msg)).await
                            {
                                error!(error = %mark_err, "failed to mark job failed; watchdog will reset it");
                            }
                        }
                        Err(join_err) if join_err.is_cancelled() => {
                            info!(elapsed_ms, "job cancelled (aborted)");
                            if let Err(err) = sqlx::query("DELETE FROM jobs WHERE id = ?")
                                .bind(job.id)
                                .execute(&self.pool)
                                .await
                            {
                                error!(error = %err, "failed to delete cancelled job; watchdog will reset it");
                            }
                        }
                        Err(join_err) => {
                            let msg = format!("{join_err}");
                            error!(elapsed_ms, error = %msg, "job task panicked");
                            if let Err(mark_err) =
                                self.mark(job.id, Status::Failed, Some(&msg)).await
                            {
                                error!(error = %mark_err, "failed to mark panicked job failed; watchdog will reset it");
                            }
                        }
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

    /// Periodic watchdog that rescues job rows stuck in `running` after their
    /// worker task has disappeared. Runs forever as a background task.
    ///
    /// Why this exists: every well-behaved exit path from `run_lane` transitions
    /// the row to `done` / `failed` / deletes it. If *any* of those DB writes
    /// fails (locked DB, panic mid-transaction, process crashing between
    /// `task.await` and the match arm, etc.) the row is stranded in `running`
    /// and — because of `idx_jobs_outstanding_unique` — no duplicate can be
    /// enqueued for the same `(kind, video_id)` either. The watchdog un-sticks
    /// those rows by resetting them to `pending` so a worker can re-claim.
    ///
    /// The registry check is the source of truth for "is a task still alive
    /// behind this row" — a stale `updated_at` alone isn't enough (long ffmpeg
    /// runs are fine; they just don't touch `updated_at`). A row is only
    /// considered stuck if its id is not in the registry AND its `updated_at`
    /// is older than `STUCK_AFTER` — the age threshold guards against the
    /// tiny claim/register race window where the row exists in `running`
    /// before the registry entry does (synchronous microseconds between
    /// `claim()` returning and `registry.register(...)`).
    async fn run_stuck_watchdog(self) {
        // The race window between claim() returning and registry.register()
        // is a handful of synchronous microseconds — no `.await` between
        // them. 30 seconds is ~6 orders of magnitude of headroom and still
        // reacts quickly to real strandings.
        const STUCK_AFTER: chrono::Duration = chrono::Duration::seconds(30);
        const POLL_INTERVAL: Duration = Duration::from_secs(30);

        // Wait one tick before the first check so startup reconcile has a
        // chance to run and workers have a chance to pick up real work.
        tokio::time::sleep(POLL_INTERVAL).await;

        loop {
            if let Err(err) =
                jobs::reset_stuck_running(&self.pool, &self.clock, &self.registry, STUCK_AFTER)
                    .await
            {
                tracing::warn!(error = %err, "stuck-job watchdog pass failed");
            }
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    }

    /// Find `running` rows older than `threshold` whose id is not tracked by
    /// the live registry and reset them to `pending`. Returns the number of
    /// rows reset (for tests and observability).
    #[cfg(test)]
    pub(crate) async fn reset_stuck_running(&self, threshold: chrono::Duration) -> Result<u64> {
        jobs::reset_stuck_running(&self.pool, &self.clock, &self.registry, threshold).await
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

    async fn process(&self, job: &Job, cancel: &CancellationToken) -> Result<()> {
        // Pre-flight: verify this job is still relevant before spawning any
        // ffmpeg. There's a small window between `claim()` (which atomically
        // transitions the row to 'running') and registering the AbortHandle
        // in the registry. During that window, a directory-remove request can
        // delete pending jobs and cancel the registry — but since we aren't
        // registered yet, our task won't be aborted. This check closes that
        // race: if the job row has been deleted, or the video's directory has
        // been soft-removed, we bail cleanly without spawning anything.
        if !self.job_is_still_relevant(job.id).await? {
            return Err(anyhow!("job {} is no longer relevant (cancelled)", job.id));
        }
        match job.kind {
            Kind::Probe => self.run_probe(&job.video_id).await,
            Kind::Thumbnail => self.run_thumbnail(&job.video_id).await,
            Kind::Preview => self.run_preview(&job.video_id, cancel).await,
        }
    }

    /// Returns true if the job's row still exists and its video's directory
    /// is not soft-removed. A `false` return means the job has been cancelled
    /// out from under us by a concurrent directory-remove.
    async fn job_is_still_relevant(&self, job_id: i64) -> Result<bool> {
        let row = sqlx::query(
            "SELECT d.removed \
             FROM jobs j \
             JOIN videos v ON v.id = j.video_id \
             JOIN directories d ON d.id = v.directory_id \
             WHERE j.id = ?",
        )
        .bind(job_id)
        .fetch_optional(&self.pool)
        .await
        .context("pre-flight job relevance check")?;
        match row {
            Some(r) => {
                let removed: i64 = r.get("removed");
                Ok(removed == 0)
            }
            None => Ok(false),
        }
    }

    /// Load the absolute path and current duration for a video.
    pub(super) async fn load_for_job(&self, video_id: &VideoId) -> Result<(PathBuf, Option<f64>)> {
        let (abs, duration, _, _) = self.load_full_for_job(video_id).await?;
        Ok((abs, duration))
    }

    /// Like `load_for_job`, but also returns `is_audio_only` and the
    /// attached-pic stream index. The thumbnail job needs both to decide
    /// whether to extract cover art; other jobs can use the smaller variant.
    pub(super) async fn load_full_for_job(
        &self,
        video_id: &VideoId,
    ) -> Result<(PathBuf, Option<f64>, bool, Option<i64>)> {
        let row = sqlx::query(
            "SELECT d.path, v.relative_path, v.duration_secs, \
                    v.is_audio_only, v.attached_pic_stream_index \
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
        let is_audio_only: i64 = row.get("is_audio_only");
        let attached_pic_stream_index: Option<i64> = row.get("attached_pic_stream_index");

        let abs = PathBuf::from(dir_path).join(relative_path);
        Ok((abs, duration, is_audio_only != 0, attached_pic_stream_index))
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
