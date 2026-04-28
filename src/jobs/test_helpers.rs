//! Shared helpers for `jobs` inline unit tests. Gated behind `#[cfg(test)]`
//! so it compiles away outside test builds.
//!
//! This is **not** a `tests.rs` sibling file (which the project deliberately
//! avoids — tests live inline next to the code they cover). It's a small
//! fixture module that multiple inline `#[cfg(test)] mod tests` blocks
//! (`reconcile`, `watchdog`, `mod` itself) pull from.

#![cfg(test)]

use std::sync::Arc;

use sqlx::SqlitePool;

use crate::{
    clock::ClockRef,
    jobs::{registry::JobRegistry, worker::Workers},
    scanner::CachePaths,
    video_tool::{MockVideoTool, VideoToolRef},
};

pub(super) fn test_cache(tmp: &std::path::Path) -> CachePaths {
    CachePaths {
        thumb: tmp.join("cache/thumbs"),
        preview: tmp.join("cache/previews"),
    }
}

pub(super) async fn setup() -> (tempfile::TempDir, SqlitePool) {
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

pub(super) fn make_workers(pool: SqlitePool, clock: ClockRef, tmp: &std::path::Path) -> Workers {
    let cfg = crate::config::Config {
        data_dir: tmp.to_path_buf(),
        backup_dir: tmp.join("backups"),
        ..crate::config::Config::default()
    };
    let video_tool: VideoToolRef = Arc::new(MockVideoTool::new());
    Workers {
        pool,
        clock,
        config: Arc::new(cfg.clone()),
        video_tool,
        thumb_dir: cfg.thumb_cache_dir(),
        preview_dir: cfg.preview_cache_dir(),
        registry: JobRegistry::new(),
    }
}
