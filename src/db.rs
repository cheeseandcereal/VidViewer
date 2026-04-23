//! Database setup.
//!
//! Responsibilities:
//! 1. Ensure the data directory exists.
//! 2. Take a `VACUUM INTO` backup before any pending migration runs.
//! 3. Run migrations from `migrations/`.
//! 4. Apply runtime pragmas on every connection.
//! 5. Perform a startup integrity check.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use chrono::Utc;
use sqlx::{
    sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous},
    ConnectOptions, Executor, Row, SqlitePool,
};

use crate::config::Config;

/// Compile-in the migration files at `migrations/` for runtime application.
pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");

/// Initialize the database: ensure directories exist, back up if needed, migrate, verify.
pub async fn init(cfg: &Config, db_path: &Path) -> Result<SqlitePool> {
    ensure_parent_dir(db_path)?;

    // Open (creating if necessary) with WAL journaling for writer/reader concurrency.
    let opts = SqliteConnectOptions::new()
        .filename(db_path)
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal)
        .synchronous(SqliteSynchronous::Normal)
        .foreign_keys(true)
        .disable_statement_logging();

    let pool = SqlitePoolOptions::new()
        .max_connections(8)
        .connect_with(opts)
        .await
        .with_context(|| format!("opening database at {}", db_path.display()))?;

    // Ensure UTF-8 encoding explicitly (default for SQLite, but belt-and-braces).
    pool.execute("PRAGMA encoding = 'UTF-8';")
        .await
        .context("setting PRAGMA encoding")?;

    maybe_backup_before_migration(cfg, db_path, &pool).await?;

    MIGRATOR
        .run(&pool)
        .await
        .context("running database migrations")?;

    integrity_check(&pool).await?;
    Ok(pool)
}

fn ensure_parent_dir(db_path: &Path) -> Result<()> {
    if let Some(parent) = db_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating data dir {}", parent.display()))?;
    }
    Ok(())
}

async fn maybe_backup_before_migration(
    cfg: &Config,
    _db_path: &Path,
    pool: &SqlitePool,
) -> Result<()> {
    if !cfg.backup_before_migration {
        tracing::debug!("pre-migration backups disabled in config");
        return Ok(());
    }

    // Skip backup if the database is effectively empty (no user tables yet). This handles
    // the "fresh install" case: there is nothing worth backing up.
    if is_fresh_database(pool).await? {
        tracing::debug!("fresh database — skipping pre-migration backup");
        return Ok(());
    }

    let current = current_schema_version(pool).await?;
    let applied_ids = applied_migration_ids(pool).await?;
    let has_pending = MIGRATOR.iter().any(|m| !applied_ids.contains(&m.version));

    if !has_pending {
        tracing::debug!(
            current_version = current,
            "no pending migrations — skipping backup"
        );
        return Ok(());
    }

    // If the DB has no migrations table yet, it's either a fresh file or pre-existing but
    // pre-migration-era. Either way, treat as schema version 0.
    let backup_dir = crate::config::expand_tilde(&cfg.backup_dir);
    std::fs::create_dir_all(&backup_dir)
        .with_context(|| format!("creating backup dir {}", backup_dir.display()))?;

    let ts = Utc::now().format("%Y%m%dT%H%M%SZ");
    let backup_path = next_available_backup_path(&backup_dir, &ts.to_string(), current);

    // SQLite's `VACUUM INTO` writes a single consistent .db file. Bind is tricky for this
    // statement; we safely embed the path because we constructed it ourselves above.
    let escaped = backup_path.display().to_string().replace('\'', "''");
    let sql = format!("VACUUM INTO '{escaped}'");
    tracing::info!(target: "vidviewer::db::backup", path = %backup_path.display(), "creating pre-migration backup");
    pool.execute(sql.as_str())
        .await
        .with_context(|| format!("creating backup via VACUUM INTO {}", backup_path.display()))?;

    let size = std::fs::metadata(&backup_path)
        .map(|m| m.len())
        .unwrap_or(0);
    tracing::info!(path = %backup_path.display(), size_bytes = size, "pre-migration backup complete");
    Ok(())
}

fn next_available_backup_path(dir: &Path, ts: &str, version: u32) -> PathBuf {
    let base = format!("vidviewer-{ts}-pre-migration-v{version}.db");
    let first = dir.join(&base);
    if !first.exists() {
        return first;
    }
    for n in 2..u32::MAX {
        let candidate = dir.join(format!("vidviewer-{ts}-pre-migration-v{version}-{n}.db"));
        if !candidate.exists() {
            return candidate;
        }
    }
    first
}

async fn applied_migration_ids(pool: &SqlitePool) -> Result<Vec<i64>> {
    // sqlx's migrator creates `_sqlx_migrations` once any migration runs. Before the first
    // migration on a fresh file, the table won't exist. Handle both cases.
    let table_exists = {
        let row = sqlx::query(
            "SELECT 1 FROM sqlite_master WHERE type='table' AND name='_sqlx_migrations'",
        )
        .fetch_optional(pool)
        .await
        .context("checking for _sqlx_migrations table")?;
        row.is_some()
    };
    if !table_exists {
        return Ok(Vec::new());
    }

    let rows = sqlx::query("SELECT version FROM _sqlx_migrations ORDER BY version")
        .fetch_all(pool)
        .await
        .context("reading _sqlx_migrations")?;
    let mut out = Vec::with_capacity(rows.len());
    for r in rows {
        out.push(r.get::<i64, _>(0));
    }
    Ok(out)
}

/// Returns true if the database file contains no user tables yet. Used to skip backups on
/// first-run installs where there's nothing to preserve.
async fn is_fresh_database(pool: &SqlitePool) -> Result<bool> {
    let rows = sqlx::query(
        "SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%' AND name NOT LIKE '\\_sqlx\\_%' ESCAPE '\\'",
    )
    .fetch_all(pool)
    .await
    .context("listing user tables")?;
    Ok(rows.is_empty())
}

async fn current_schema_version(pool: &SqlitePool) -> Result<u32> {
    let ids = applied_migration_ids(pool).await?;
    Ok(ids.last().copied().map(|v| v as u32).unwrap_or(0))
}

/// Sanity-check the DB after migrations: all expected tables are present.
async fn integrity_check(pool: &SqlitePool) -> Result<()> {
    let required = [
        "directories",
        "videos",
        "collections",
        "collection_videos",
        "watch_history",
        "jobs",
        "ui_state",
    ];
    for table in required {
        let row = sqlx::query("SELECT 1 FROM sqlite_master WHERE type='table' AND name=?")
            .bind(table)
            .fetch_optional(pool)
            .await
            .with_context(|| format!("checking for table {table}"))?;
        if row.is_none() {
            bail!("startup integrity check failed: missing table '{table}'");
        }
    }
    tracing::debug!("startup integrity check passed");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config(data_dir: &Path) -> Config {
        Config {
            data_dir: data_dir.to_path_buf(),
            backup_dir: data_dir.join("backups"),
            ..Config::default()
        }
    }

    #[tokio::test]
    async fn fresh_db_initializes_and_has_all_tables() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("data").join("vidviewer.db");
        let cfg = test_config(tmp.path());

        let pool = init(&cfg, &db_path).await.unwrap();
        integrity_check(&pool).await.unwrap();

        // Fresh file — should not have produced a backup because there was nothing to back up.
        let backups_dir = tmp.path().join("backups");
        // The directory may exist (we create it up-front) but should be empty.
        if backups_dir.exists() {
            let count = std::fs::read_dir(&backups_dir).unwrap().count();
            assert_eq!(count, 0, "expected no backups for a fresh db");
        }
    }

    #[tokio::test]
    async fn ui_state_row_exists() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("data").join("vidviewer.db");
        let cfg = test_config(tmp.path());
        let pool = init(&cfg, &db_path).await.unwrap();

        let row = sqlx::query("SELECT last_browsed_path FROM ui_state WHERE id=1")
            .fetch_one(&pool)
            .await
            .unwrap();
        let v: Option<String> = row.get(0);
        assert!(v.is_none());
    }

    #[tokio::test]
    async fn second_init_does_not_backup() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("data").join("vidviewer.db");
        let cfg = test_config(tmp.path());
        let backups = cfg.backup_dir.clone();
        {
            let _ = init(&cfg, &db_path).await.unwrap();
        }
        // Re-init on an already migrated DB: no pending migrations → no backup.
        let _ = init(&cfg, &db_path).await.unwrap();
        if backups.exists() {
            let count = std::fs::read_dir(&backups).unwrap().count();
            assert_eq!(count, 0, "unexpected backup after second init");
        }
    }
}
