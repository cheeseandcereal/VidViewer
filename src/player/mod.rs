//! `Player` trait and implementations.
//!
//! The trait abstracts launching an external player (mpv) so tests can substitute a mock.
//! See `docs/design/08-mpv-integration.md`.

use std::path::Path;
use std::process::Stdio;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use async_trait::async_trait;

use crate::config::Config;
use crate::ids::VideoId;

/// A handle to an active player session. Dropping it does not stop the player.
#[derive(Debug)]
pub struct SessionHandle {
    pub video_id: VideoId,
    pub socket_path: std::path::PathBuf,
    /// The child process handle, so it can be awaited / killed. `None` for mocks.
    pub child: Option<tokio::process::Child>,
}

#[async_trait]
pub trait Player: Send + Sync {
    async fn launch(&self, video_abs_path: &Path, start_secs: f64) -> Result<SessionHandle>;
}

pub type PlayerRef = Arc<dyn Player>;

#[derive(Debug, Clone)]
pub struct MpvPlayer {
    player_binary: String,
    player_args: Vec<String>,
}

impl MpvPlayer {
    pub fn new(cfg: &Config) -> Self {
        Self {
            player_binary: cfg.player.clone(),
            player_args: cfg.player_args.clone(),
        }
    }
}

#[async_trait]
impl Player for MpvPlayer {
    async fn launch(&self, video_abs_path: &Path, start_secs: f64) -> Result<SessionHandle> {
        let socket =
            std::env::temp_dir().join(format!("vidviewer-mpv-{}.sock", uuid::Uuid::new_v4()));
        // Best-effort: remove a stale socket.
        let _ = std::fs::remove_file(&socket);

        let mut cmd = tokio::process::Command::new(&self.player_binary);
        cmd.arg(format!("--input-ipc-server={}", socket.display()));
        if start_secs > 0.0 {
            cmd.arg(format!("--start={start_secs:.3}"));
        }
        for a in &self.player_args {
            cmd.arg(a);
        }
        cmd.arg(video_abs_path);
        cmd.stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());

        let child = cmd
            .spawn()
            .with_context(|| format!("spawning {}", self.player_binary))?;

        Ok(SessionHandle {
            video_id: VideoId(String::new()),
            socket_path: socket,
            child: Some(child),
        })
    }
}

pub fn mpv(cfg: &Config) -> PlayerRef {
    Arc::new(MpvPlayer::new(cfg))
}

/// Mock player that records launches but doesn't spawn anything.
#[derive(Debug, Default, Clone)]
pub struct MockPlayer {
    inner: Arc<Mutex<MockState>>,
}

#[derive(Debug, Default)]
struct MockState {
    pub launches: Vec<(std::path::PathBuf, f64)>,
}

impl MockPlayer {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn launches(&self) -> Vec<(std::path::PathBuf, f64)> {
        self.inner.lock().unwrap().launches.clone()
    }
}

#[async_trait]
impl Player for MockPlayer {
    async fn launch(&self, video_abs_path: &Path, start_secs: f64) -> Result<SessionHandle> {
        self.inner
            .lock()
            .unwrap()
            .launches
            .push((video_abs_path.to_path_buf(), start_secs));
        Ok(SessionHandle {
            video_id: VideoId(String::new()),
            socket_path: std::path::PathBuf::from("/dev/null"),
            child: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn mock_records_launch() {
        let m = MockPlayer::new();
        let _ = m.launch(Path::new("/x.mp4"), 0.0).await.unwrap();
        assert_eq!(m.launches().len(), 1);
    }
}
