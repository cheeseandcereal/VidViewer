//! Dry-run mode: walk the tree and report what would change without mutating
//! the DB. Used by `vidviewer scan --dry-run` for diagnostics.

use std::{collections::HashMap, path::PathBuf};

use anyhow::Result;
use serde::Serialize;
use sqlx::{Row, SqlitePool};
use walkdir::WalkDir;

use crate::{directories, ids::DirectoryId, scanner::walk};

#[derive(Debug, Default, Clone, Serialize)]
pub struct DryRunReport {
    pub seen_files: u64,
    pub would_insert: Vec<String>,
    pub would_update: Vec<String>,
    pub would_mark_missing: Vec<String>,
    pub missing_directories: Vec<String>,
}

/// Produce a textual dry-run report for a directory. Does not write anything.
pub async fn dry_run_report(pool: &SqlitePool, only: Option<DirectoryId>) -> Result<DryRunReport> {
    let mut out = DryRunReport::default();
    let mut dirs = directories::list(pool, false).await?;
    if let Some(target) = only {
        dirs.retain(|d| d.id == target);
    }
    for dir in dirs {
        let root = PathBuf::from(&dir.path);
        if !root.exists() {
            out.missing_directories.push(dir.path.clone());
            continue;
        }
        let rows = sqlx::query(
            "SELECT relative_path, size_bytes, mtime_unix, missing FROM videos WHERE directory_id = ?",
        )
        .bind(dir.id.raw())
        .fetch_all(pool)
        .await?;
        let mut known: HashMap<String, (i64, i64, bool)> = HashMap::with_capacity(rows.len());
        for row in rows {
            let rel: String = row.get("relative_path");
            let size: i64 = row.get("size_bytes");
            let mtime: i64 = row.get("mtime_unix");
            let missing: i64 = row.get("missing");
            known.insert(rel, (size, mtime, missing != 0));
        }

        for entry in WalkDir::new(&root)
            .follow_links(true)
            .max_depth(1)
            .into_iter()
            .filter_map(|r| r.ok())
        {
            if !entry.file_type().is_file() {
                continue;
            }
            // Dry-run is a diagnostic tool; match the real scanner's media
            // classification so its output is faithful. The sniff cost is
            // fine here — dry-run is a human-triggered command, not a
            // background pass.
            if !walk::sniff_or_warn(entry.path()) {
                continue;
            }
            let rel = match entry.path().strip_prefix(&root) {
                Ok(p) => crate::util::path::path_to_db_string(p),
                Err(_) => continue,
            };
            let meta = match entry.metadata() {
                Ok(m) => m,
                Err(_) => continue,
            };
            let size = meta.len() as i64;
            let mtime = walk::mtime_to_unix(&meta);
            out.seen_files += 1;

            match known.remove(&rel) {
                None => out.would_insert.push(rel),
                Some((s, m, missing)) if s != size || m != mtime || missing => {
                    out.would_update.push(rel);
                }
                Some(_) => {}
            }
        }
        for (rel, (_, _, missing)) in known.into_iter() {
            if !missing {
                out.would_mark_missing.push(rel);
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::directories::add as add_dir;

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
    async fn dry_run_reports_insert_update_and_mark_missing() {
        let (tmp, pool) = setup().await;
        let clock = crate::clock::system();
        let videos = tmp.path().join("videos");
        std::fs::create_dir_all(&videos).unwrap();

        // Seed three real media files that will be picked up by sniff.
        crate::test_support::write_video_fixture(&videos, "a.mp4", b"aa");
        crate::test_support::write_video_fixture(&videos, "b.mp4", b"b");
        crate::test_support::write_video_fixture(&videos, "c.mp4", b"cc");

        let dir = add_dir(&pool, &clock, &videos, None).await.unwrap();

        // Seed DB rows for the three files so we can trigger all three
        // branches:
        //   a.mp4: stored stat matches disk -> no action.
        //   b.mp4: stored stat differs from disk -> would_update.
        //   (d.mp4 exists in DB but not on disk) -> would_mark_missing.
        // c.mp4 has no DB row -> would_insert.
        let a_meta = std::fs::metadata(videos.join("a.mp4")).unwrap();
        let now = clock.now().to_rfc3339();
        sqlx::query(
            "INSERT INTO videos (id, directory_id, relative_path, filename, size_bytes, \
             mtime_unix, thumbnail_ok, preview_ok, missing, is_audio_only, created_at, updated_at) \
             VALUES ('a', ?, 'a.mp4', 'a.mp4', ?, ?, 0, 0, 0, 0, ?, ?)",
        )
        .bind(dir.id.raw())
        .bind(a_meta.len() as i64)
        .bind(
            a_meta
                .modified()
                .unwrap()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs() as i64,
        )
        .bind(&now)
        .bind(&now)
        .execute(&pool)
        .await
        .unwrap();

        // b.mp4 row with stale size so dry-run flags it would_update.
        sqlx::query(
            "INSERT INTO videos (id, directory_id, relative_path, filename, size_bytes, \
             mtime_unix, thumbnail_ok, preview_ok, missing, is_audio_only, created_at, updated_at) \
             VALUES ('b', ?, 'b.mp4', 'b.mp4', 99999, 0, 0, 0, 0, 0, ?, ?)",
        )
        .bind(dir.id.raw())
        .bind(&now)
        .bind(&now)
        .execute(&pool)
        .await
        .unwrap();

        // d.mp4 DB-only (not on disk) -> would_mark_missing.
        sqlx::query(
            "INSERT INTO videos (id, directory_id, relative_path, filename, size_bytes, \
             mtime_unix, thumbnail_ok, preview_ok, missing, is_audio_only, created_at, updated_at) \
             VALUES ('d', ?, 'd.mp4', 'd.mp4', 1, 1, 0, 0, 0, 0, ?, ?)",
        )
        .bind(dir.id.raw())
        .bind(&now)
        .bind(&now)
        .execute(&pool)
        .await
        .unwrap();

        let report = dry_run_report(&pool, None).await.unwrap();
        assert_eq!(report.seen_files, 3);
        assert_eq!(report.would_insert, vec!["c.mp4"]);
        assert_eq!(report.would_update, vec!["b.mp4"]);
        assert_eq!(report.would_mark_missing, vec!["d.mp4"]);
        assert!(report.missing_directories.is_empty());
    }

    #[tokio::test]
    async fn dry_run_flags_missing_directory_paths() {
        let (tmp, pool) = setup().await;
        let clock = crate::clock::system();
        // Add a directory, then delete its path on disk so dry_run sees
        // it as gone.
        let videos = tmp.path().join("gone");
        std::fs::create_dir_all(&videos).unwrap();
        add_dir(&pool, &clock, &videos, None).await.unwrap();
        std::fs::remove_dir_all(&videos).unwrap();

        let report = dry_run_report(&pool, None).await.unwrap();
        assert_eq!(report.missing_directories.len(), 1);
        assert!(report.missing_directories[0].ends_with("gone"));
    }

    #[tokio::test]
    async fn dry_run_filters_by_only_directory_id() {
        let (tmp, pool) = setup().await;
        let clock = crate::clock::system();
        let a = tmp.path().join("a");
        let b = tmp.path().join("b");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        crate::test_support::write_video_fixture(&a, "in_a.mp4", b"x");
        crate::test_support::write_video_fixture(&b, "in_b.mp4", b"x");
        let dir_a = add_dir(&pool, &clock, &a, None).await.unwrap();
        let _dir_b = add_dir(&pool, &clock, &b, None).await.unwrap();

        let report = dry_run_report(&pool, Some(dir_a.id)).await.unwrap();
        // Only directory A's single new file should be proposed.
        assert_eq!(report.seen_files, 1);
        assert_eq!(report.would_insert, vec!["in_a.mp4"]);
    }
}
