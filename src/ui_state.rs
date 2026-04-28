//! Helpers for reading and writing the `ui_state` row.

use anyhow::{Context, Result};
use sqlx::{Row, SqlitePool};

/// Get the last path the user browsed in the directory picker, if any.
pub async fn get_last_browsed_path(pool: &SqlitePool) -> Result<Option<String>> {
    let row = sqlx::query("SELECT last_browsed_path FROM ui_state WHERE id = 1")
        .fetch_one(pool)
        .await
        .context("fetching ui_state")?;
    Ok(row.try_get::<Option<String>, _>(0).ok().flatten())
}

pub async fn set_last_browsed_path(pool: &SqlitePool, path: &str) -> Result<()> {
    sqlx::query("UPDATE ui_state SET last_browsed_path = ? WHERE id = 1")
        .bind(path)
        .execute(pool)
        .await
        .context("updating ui_state.last_browsed_path")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn setup() -> (tempfile::TempDir, SqlitePool) {
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

    #[tokio::test]
    async fn fresh_db_has_no_last_browsed_path() {
        let (_tmp, pool) = setup().await;
        let p = get_last_browsed_path(&pool).await.unwrap();
        assert!(p.is_none());
    }

    #[tokio::test]
    async fn set_and_get_round_trip() {
        let (_tmp, pool) = setup().await;
        set_last_browsed_path(&pool, "/home/user/Videos")
            .await
            .unwrap();
        let p = get_last_browsed_path(&pool).await.unwrap();
        assert_eq!(p.as_deref(), Some("/home/user/Videos"));

        // Overwrite with a new value.
        set_last_browsed_path(&pool, "/srv/media").await.unwrap();
        let p = get_last_browsed_path(&pool).await.unwrap();
        assert_eq!(p.as_deref(), Some("/srv/media"));
    }
}
