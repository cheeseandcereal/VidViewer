//! The shared application state passed to HTTP handlers and background tasks.

use std::sync::Arc;

use sqlx::SqlitePool;
use tokio::sync::RwLock;

use crate::{
    clock::{self, ClockRef},
    config::Config,
    scanner::ScanHandle,
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
}

impl AppState {
    pub fn new(config: Config, pool: SqlitePool) -> Self {
        Self {
            config: Arc::new(config),
            pool,
            clock: clock::system(),
            scans: Arc::new(RwLock::new(ScanRegistry::default())),
        }
    }
}
