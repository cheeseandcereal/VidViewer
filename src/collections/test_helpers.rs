//! Shared test fixtures for `collections` inline unit tests.
//! Gated behind `#[cfg(test)]`; not part of the public crate surface.

#![cfg(test)]

use sqlx::SqlitePool;

use crate::{
    clock::{self, ClockRef},
    ids::{DirectoryId, VideoId},
};

pub(super) async fn setup() -> (tempfile::TempDir, SqlitePool, ClockRef) {
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

pub(super) async fn add_video_row(
    pool: &SqlitePool,
    clock: &ClockRef,
    dir_id: DirectoryId,
    rel: &str,
) -> VideoId {
    let now = clock.now().to_rfc3339();
    let id = VideoId(uuid::Uuid::new_v4().to_string());
    sqlx::query(
        "INSERT INTO videos (id, directory_id, relative_path, filename, size_bytes, \
         mtime_unix, thumbnail_ok, preview_ok, missing, created_at, updated_at) \
         VALUES (?, ?, ?, ?, 1, 1, 1, 0, 0, ?, ?)",
    )
    .bind(id.as_str())
    .bind(dir_id.raw())
    .bind(rel)
    .bind(rel)
    .bind(&now)
    .bind(&now)
    .execute(pool)
    .await
    .unwrap();
    id
}
