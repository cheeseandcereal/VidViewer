//! The shared application state passed to HTTP handlers and background tasks.

use std::sync::Arc;

use sqlx::SqlitePool;

use crate::{
    clock::{self, ClockRef},
    config::Config,
};

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub pool: SqlitePool,
    pub clock: ClockRef,
}

impl AppState {
    pub fn new(config: Config, pool: SqlitePool) -> Self {
        Self {
            config: Arc::new(config),
            pool,
            clock: clock::system(),
        }
    }
}
