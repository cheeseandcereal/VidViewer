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
