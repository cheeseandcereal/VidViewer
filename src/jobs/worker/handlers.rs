//! Per-kind job handlers. Each is an `impl Workers` method invoked by
//! `Workers::process` in the parent module.

use anyhow::{anyhow, Context, Result};
use tokio_util::sync::CancellationToken;
use tracing::info;

use super::Workers;
use crate::{
    ids::VideoId,
    jobs::{
        self,
        preview_plan::{self, PlanInput},
        Kind,
    },
};

impl Workers {
    pub(super) async fn run_probe(&self, video_id: &VideoId) -> Result<()> {
        let (abs_path, _duration) = self.load_for_job(video_id).await?;
        info!(path = %abs_path.display(), "probing video");
        let result = self.video_tool.probe(&abs_path).await?;

        let now_s = self.clock.now().to_rfc3339();
        sqlx::query(
            "UPDATE videos SET duration_secs = ?, width = ?, height = ?, codec = ?, \
             is_audio_only = ?, attached_pic_stream_index = ?, updated_at = ? \
             WHERE id = ?",
        )
        .bind(result.duration_secs)
        .bind(result.width)
        .bind(result.height)
        .bind(&result.codec)
        .bind(result.is_audio_only as i64)
        .bind(result.attached_pic_stream_index)
        .bind(&now_s)
        .bind(video_id.as_str())
        .execute(&self.pool)
        .await
        .context("updating video probe result")?;

        // Enqueue the derived jobs now that we know what kind of file this is.
        let mut conn = self.pool.acquire().await?;
        jobs::enqueue_on(&mut conn, Kind::Thumbnail, video_id).await?;
        // Preview jobs only make sense for files with a real video stream
        // AND a known duration. Audio files (even ones with cover art) stay
        // without previews intentionally — see docs/design/06.
        if !result.is_audio_only && result.duration_secs.unwrap_or(0.0) > 0.0 {
            jobs::enqueue_on(&mut conn, Kind::Preview, video_id).await?;
        } else if result.is_audio_only {
            info!("skipping preview enqueue: audio-only file");
        } else {
            info!("skipping preview enqueue: duration unknown or zero");
        }
        info!(
            duration_secs = ?result.duration_secs,
            width = ?result.width,
            height = ?result.height,
            codec = ?result.codec,
            is_audio_only = result.is_audio_only,
            attached_pic = ?result.attached_pic_stream_index,
            "probe produced metadata"
        );
        Ok(())
    }

    pub(super) async fn run_thumbnail(&self, video_id: &VideoId) -> Result<()> {
        let (abs_path, duration, is_audio_only, attached_pic_stream_index) =
            self.load_full_for_job(video_id).await?;

        // Strategy:
        //   - Real video: seek to the video midpoint (or 5s fallback) and
        //     grab one frame. Same behavior this job has always had.
        //   - Audio-only + attached cover art: extract frame 0 of the
        //     cover-art stream. Use thumbnail_width as the target for
        //     consistency with video thumbnails.
        //   - Audio-only + no cover art: skip. The UI falls back to a
        //     static placeholder image when thumbnail_ok = 0 for an audio
        //     row.
        if is_audio_only {
            let Some(stream_idx) = attached_pic_stream_index else {
                info!(
                    video_id = %video_id,
                    "audio-only file with no attached cover art; skipping thumbnail"
                );
                return Ok(());
            };
            let dst = self.thumb_dir.join(format!("{}.jpg", video_id.as_str()));
            info!(
                path = %abs_path.display(),
                stream = stream_idx,
                width = self.config.thumbnail_width,
                dst = %dst.display(),
                "extracting cover-art thumbnail"
            );
            self.video_tool
                .thumbnail(
                    &abs_path,
                    &dst,
                    0.0,
                    self.config.thumbnail_width,
                    Some(stream_idx),
                )
                .await?;
        } else {
            // Use the midpoint of the video for the poster frame. If duration is unknown
            // or zero, fall back to 5 seconds in (a safe default that skips intros/logos
            // on most clips).
            let at = match duration {
                Some(d) if d > 0.0 => d * 0.5,
                _ => 5.0,
            };
            let dst = self.thumb_dir.join(format!("{}.jpg", video_id.as_str()));
            info!(
                path = %abs_path.display(),
                at_secs = at,
                width = self.config.thumbnail_width,
                dst = %dst.display(),
                "generating thumbnail"
            );
            self.video_tool
                .thumbnail(&abs_path, &dst, at, self.config.thumbnail_width, None)
                .await?;
        }

        let now_s = self.clock.now().to_rfc3339();
        sqlx::query("UPDATE videos SET thumbnail_ok = 1, updated_at = ? WHERE id = ?")
            .bind(&now_s)
            .bind(video_id.as_str())
            .execute(&self.pool)
            .await
            .context("marking thumbnail_ok")?;
        Ok(())
    }

    pub(super) async fn run_preview(
        &self,
        video_id: &VideoId,
        cancel: &CancellationToken,
    ) -> Result<()> {
        let (abs_path, duration, is_audio_only, _attached_pic) =
            self.load_full_for_job(video_id).await?;
        // Defense in depth: the scanner and probe-time enqueue both gate on
        // is_audio_only, but a stale pending preview job could still reach
        // us (e.g. pre-audio-support rows cleared by reconcile, or some
        // future edge case). Skipping here means the job transitions to
        // `done` cleanly rather than spraying "tile 0" errors against a
        // file that has no video stream.
        if is_audio_only {
            info!(%video_id, "skipping preview: row is audio-only");
            return Ok(());
        }
        let duration = duration.unwrap_or(0.0);
        if duration <= 0.0 {
            return Err(anyhow!("cannot generate preview without duration"));
        }
        let Some(plan) = preview_plan::plan(&PlanInput {
            duration_secs: duration,
            min_interval_secs: self.config.preview_min_interval,
            target_count: self.config.preview_target_count,
        }) else {
            return Err(anyhow!("plan generation returned None despite duration"));
        };

        let preview_dir = self.preview_dir.clone();
        let sheet_path = preview_dir.join(format!("{}.jpg", video_id.as_str()));
        let vtt_path = preview_dir.join(format!("{}.vtt", video_id.as_str()));

        info!(
            path = %abs_path.display(),
            duration_secs = duration,
            previews = plan.count,
            grid = format!("{}x{}", plan.cols, plan.rows),
            tile_size = format!("{}x{}", plan.tile_width, plan.tile_height),
            sheet = %sheet_path.display(),
            "generating preview tile sheet"
        );

        self.video_tool
            .previews(&abs_path, &sheet_path, &plan, duration, cancel)
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
        Ok(())
    }
}
