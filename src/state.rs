//! The shared application state passed to HTTP handlers and background tasks.

use std::sync::Arc;

use sqlx::SqlitePool;
use tokio::sync::RwLock;

use crate::{
    clock::{self, ClockRef},
    config::Config,
    player::{self, PlayerRef},
    scanner::ScanHandle,
    video_tool::{self, VideoToolRef},
};

#[derive(Default)]
pub struct ScanRegistry {
    pub current: Option<ScanHandle>,
}

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub pool: SqlitePool,
    pub clock: ClockRef,
    pub scans: Arc<RwLock<ScanRegistry>>,
    pub player: PlayerRef,
    pub video_tool: VideoToolRef,
}

impl AppState {
    pub fn new(config: Config, pool: SqlitePool) -> Self {
        let player = player::mpv(&config);
        let video_tool = video_tool::ffmpeg(&config);
        Self {
            config: Arc::new(config),
            pool,
            clock: clock::system(),
            scans: Arc::new(RwLock::new(ScanRegistry::default())),
            player,
            video_tool,
        }
    }

    /// For tests: construct a state with mock player/video tool.
    #[cfg(test)]
    pub fn for_test(config: Config, pool: SqlitePool) -> Self {
        Self {
            config: Arc::new(config),
            pool,
            clock: clock::system(),
            scans: Arc::new(RwLock::new(ScanRegistry::default())),
            player: Arc::new(player::MockPlayer::new()),
            video_tool: Arc::new(video_tool::MockVideoTool::new()),
        }
    }
}
