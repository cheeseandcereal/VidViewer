//! Session manager for an active mpv player.
//!
//! Spawns a tokio task that connects to mpv's JSON IPC Unix socket, observes `time-pos`,
//! and persists progress to `watch_history`. See `docs/design/08-mpv-integration.md`.

use std::{path::Path, time::Duration};

use anyhow::{Context, Result};
use serde_json::Value;
use sqlx::SqlitePool;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::UnixStream,
    time::Instant,
};

use crate::{clock::ClockRef, history, ids::VideoId};

/// Spawn a task that waits for the mpv socket, then observes `time-pos` updates and
/// persists them. The task also runs the child process to completion so that when mpv
/// exits, we finalize the history row.
pub fn spawn(
    pool: SqlitePool,
    clock: ClockRef,
    video_id: VideoId,
    socket_path: std::path::PathBuf,
    mut child: tokio::process::Child,
) {
    tokio::spawn(async move {
        if let Err(err) = history::start_session(&pool, &clock, &video_id).await {
            tracing::warn!(error = %err, video_id = %video_id, "start_session failed");
        }

        // Connect to the socket (retrying for up to 5s).
        let stream = connect_with_retry(&socket_path, Duration::from_secs(5)).await;
        if let Ok(stream) = stream {
            if let Err(err) = observe_time_pos(&pool, &clock, &video_id, stream).await {
                tracing::warn!(error = %err, video_id = %video_id, "ipc session ended with error");
            }
        } else {
            tracing::warn!(video_id = %video_id, "could not connect to mpv ipc socket");
        }

        // Wait for the process to finish, then finalize.
        if let Err(err) = child.wait().await {
            tracing::warn!(error = %err, video_id = %video_id, "waiting on mpv child");
        }
        if let Err(err) = history::end_session(&pool, &clock, &video_id).await {
            tracing::warn!(error = %err, video_id = %video_id, "end_session failed");
        }
        // Best-effort socket cleanup.
        let _ = std::fs::remove_file(&socket_path);
    });
}

async fn connect_with_retry(path: &Path, total: Duration) -> Result<UnixStream> {
    let deadline = Instant::now() + total;
    let mut last_err: Option<std::io::Error> = None;
    while Instant::now() < deadline {
        match UnixStream::connect(path).await {
            Ok(s) => return Ok(s),
            Err(err) => {
                last_err = Some(err);
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        }
    }
    Err(anyhow::anyhow!(
        "failed to connect to {} within {:?}: {:?}",
        path.display(),
        total,
        last_err
    ))
}

async fn observe_time_pos(
    pool: &SqlitePool,
    clock: &ClockRef,
    video_id: &VideoId,
    stream: UnixStream,
) -> Result<()> {
    let (r, mut w) = stream.into_split();
    let mut lines = BufReader::new(r).lines();

    // Subscribe.
    w.write_all(b"{\"command\":[\"observe_property\",1,\"time-pos\"]}\n")
        .await
        .context("observe_property write")?;
    // Useful to learn about end-of-file cleanly.
    w.write_all(b"{\"command\":[\"observe_property\",2,\"eof-reached\"]}\n")
        .await
        .context("observe eof write")?;

    let mut last_persist = Instant::now() - Duration::from_secs(10);
    while let Some(line) = lines.next_line().await.context("reading ipc line")? {
        let Ok(v) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if v.get("event").and_then(|x| x.as_str()) == Some("property-change")
            && v.get("name").and_then(|x| x.as_str()) == Some("time-pos")
        {
            if let Some(n) = v.get("data").and_then(|x| x.as_f64()) {
                if last_persist.elapsed() >= Duration::from_secs(5) {
                    if let Err(err) = history::update_position(pool, clock, video_id, n).await {
                        tracing::warn!(error = %err, "update_position failed");
                    }
                    last_persist = Instant::now();
                }
            }
        }
    }
    Ok(())
}
