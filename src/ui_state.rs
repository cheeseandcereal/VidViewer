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
