//! Watch history persistence.

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::Serialize;
use sqlx::{Row, SqlitePool};

use crate::{clock::ClockRef, ids::VideoId};

#[derive(Debug, Clone, Serialize)]
pub struct HistoryEntry {
    pub video_id: VideoId,
    pub filename: String,
    pub duration_secs: Option<f64>,
    pub thumbnail_ok: bool,
    pub last_watched_at: DateTime<Utc>,
    pub position_secs: f64,
    pub completed: bool,
    pub watch_count: i64,
    pub updated_at_epoch: i64,
}

/// Ensure a history row exists and increment the watch count once at session start.
pub async fn start_session(pool: &SqlitePool, clock: &ClockRef, video_id: &VideoId) -> Result<()> {
    let now_s = clock.now().to_rfc3339();
    sqlx::query(
        "INSERT INTO watch_history (video_id, last_watched_at, position_secs, completed, watch_count) \
         VALUES (?, ?, 0, 0, 1) \
         ON CONFLICT(video_id) DO UPDATE SET \
            last_watched_at = excluded.last_watched_at, \
            watch_count = watch_history.watch_count + 1",
    )
    .bind(video_id.as_str())
    .bind(&now_s)
    .execute(pool)
    .await
    .context("starting watch history session")?;
    Ok(())
}

/// Update the current playback position.
pub async fn update_position(
    pool: &SqlitePool,
    clock: &ClockRef,
    video_id: &VideoId,
    position_secs: f64,
) -> Result<()> {
    let now_s = clock.now().to_rfc3339();
    sqlx::query(
        "UPDATE watch_history SET position_secs = ?, last_watched_at = ? WHERE video_id = ?",
    )
    .bind(position_secs.max(0.0))
    .bind(&now_s)
    .bind(video_id.as_str())
    .execute(pool)
    .await
    .context("updating position")?;
    Ok(())
}

/// Finalize session at end-of-file / socket close. If >=90% watched, mark completed and reset.
pub async fn end_session(pool: &SqlitePool, clock: &ClockRef, video_id: &VideoId) -> Result<()> {
    let row = sqlx::query(
        "SELECT wh.position_secs AS p, v.duration_secs AS d \
         FROM watch_history wh JOIN videos v ON v.id = wh.video_id \
         WHERE wh.video_id = ?",
    )
    .bind(video_id.as_str())
    .fetch_optional(pool)
    .await
    .context("loading session for end")?;
    let Some(row) = row else {
        return Ok(());
    };
    let position: f64 = row.get("p");
    let duration: Option<f64> = row.get("d");
    let completed = matches!(duration, Some(d) if d > 0.0 && position / d >= 0.9);

    let now_s = clock.now().to_rfc3339();
    if completed {
        sqlx::query(
            "UPDATE watch_history SET completed = 1, position_secs = 0, last_watched_at = ? \
             WHERE video_id = ?",
        )
        .bind(&now_s)
        .bind(video_id.as_str())
        .execute(pool)
        .await
        .context("completing history")?;
    } else {
        sqlx::query("UPDATE watch_history SET last_watched_at = ? WHERE video_id = ?")
            .bind(&now_s)
            .bind(video_id.as_str())
            .execute(pool)
            .await
            .context("touching history")?;
    }
    Ok(())
}

/// Fetch the start position to use for launching: the history position if not completed.
pub async fn start_position(pool: &SqlitePool, video_id: &VideoId) -> Result<f64> {
    let row = sqlx::query("SELECT position_secs, completed FROM watch_history WHERE video_id = ?")
        .bind(video_id.as_str())
        .fetch_optional(pool)
        .await
        .context("loading start position")?;
    let Some(row) = row else {
        return Ok(0.0);
    };
    let pos: f64 = row.get(0);
    let completed: i64 = row.get(1);
    if completed != 0 {
        Ok(0.0)
    } else {
        Ok(pos.max(0.0))
    }
}

pub async fn list(pool: &SqlitePool) -> Result<Vec<HistoryEntry>> {
    let rows = sqlx::query(
        "SELECT wh.video_id, wh.last_watched_at, wh.position_secs, wh.completed, wh.watch_count, \
                v.filename, v.duration_secs, v.thumbnail_ok, v.updated_at \
         FROM watch_history wh JOIN videos v ON v.id = wh.video_id \
         ORDER BY wh.last_watched_at DESC LIMIT 500",
    )
    .fetch_all(pool)
    .await
    .context("listing history")?;
    let mut out = Vec::with_capacity(rows.len());
    for r in rows {
        let lw: String = r.get("last_watched_at");
        let ua: String = r.get("updated_at");
        out.push(HistoryEntry {
            video_id: VideoId(r.get("video_id")),
            filename: r.get("filename"),
            duration_secs: r.get("duration_secs"),
            thumbnail_ok: r.get::<i64, _>("thumbnail_ok") != 0,
            last_watched_at: chrono::DateTime::parse_from_rfc3339(&lw)?.with_timezone(&Utc),
            position_secs: r.get("position_secs"),
            completed: r.get::<i64, _>("completed") != 0,
            watch_count: r.get("watch_count"),
            updated_at_epoch: chrono::DateTime::parse_from_rfc3339(&ua)?.timestamp(),
        });
    }
    Ok(out)
}

pub async fn clear(pool: &SqlitePool, video_id: &VideoId) -> Result<()> {
    sqlx::query("DELETE FROM watch_history WHERE video_id = ?")
        .bind(video_id.as_str())
        .execute(pool)
        .await
        .context("deleting history row")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock;
    use crate::directories::add as add_dir;
    use crate::scanner;

    async fn setup() -> (tempfile::TempDir, SqlitePool, ClockRef) {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = crate::config::Config {
            data_dir: tmp.path().to_path_buf(),
            backup_dir: tmp.path().join("backups"),
            ..crate::config::Config::default()
        };
        let db_path = tmp.path().join("vidviewer.db");
        let pool = crate::db::init(&cfg, &db_path).await.unwrap();
        (tmp, pool, clock::system())
    }

    async fn video_id_in(pool: &SqlitePool) -> VideoId {
        let id: String = sqlx::query_scalar("SELECT id FROM videos LIMIT 1")
            .fetch_one(pool)
            .await
            .unwrap();
        VideoId(id)
    }

    #[tokio::test]
    async fn session_start_update_and_end() {
        let (tmp, pool, clock) = setup().await;
        let videos_dir = tmp.path().join("videos");
        std::fs::create_dir_all(&videos_dir).unwrap();
        std::fs::write(videos_dir.join("a.mp4"), b"x").unwrap();

        add_dir(&pool, &clock, &videos_dir, None).await.unwrap();
        let _ = scanner::scan_all(&pool, &clock).await.unwrap();
        // Set duration directly so end_session can evaluate completion.
        sqlx::query("UPDATE videos SET duration_secs = 100.0")
            .execute(&pool)
            .await
            .unwrap();
        let vid = video_id_in(&pool).await;

        start_session(&pool, &clock, &vid).await.unwrap();
        assert_eq!(start_position(&pool, &vid).await.unwrap(), 0.0);

        update_position(&pool, &clock, &vid, 42.0).await.unwrap();
        assert!((start_position(&pool, &vid).await.unwrap() - 42.0).abs() < 1e-6);

        // >= 90% triggers completion + reset.
        update_position(&pool, &clock, &vid, 95.0).await.unwrap();
        end_session(&pool, &clock, &vid).await.unwrap();
        assert_eq!(start_position(&pool, &vid).await.unwrap(), 0.0);
    }
}
