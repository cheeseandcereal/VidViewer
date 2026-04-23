//! Walk phase: stat-and-diff the directory tree against the DB.
//!
//! `scan_one` is the core loop. It loads known videos, walks the tree, and
//! dispatches to `mutations::*` helpers depending on whether a file is new,
//! changed, un-missing, or unchanged. Post-walk it invokes cache verification.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::atomic::Ordering,
};

use anyhow::{Context, Result};
use sqlx::{Row, SqlitePool};
use walkdir::WalkDir;

use crate::{
    clock::ClockRef,
    db::row::bool_from_i64,
    directories,
    ids::VideoId,
    scanner::{mutations, verify, CachePaths, ScanProgress, ScanReport, VIDEO_EXTENSIONS},
};

/// Existing video row projection used for diffing.
#[derive(Debug, Clone)]
struct KnownVideo {
    id: VideoId,
    size_bytes: i64,
    mtime_unix: i64,
    missing: bool,
    thumbnail_ok: bool,
    preview_ok: bool,
    duration_secs: Option<f64>,
}

pub(super) async fn scan_one(
    pool: &SqlitePool,
    clock: &ClockRef,
    cache: &CachePaths,
    dir: &directories::Directory,
    progress: &ScanProgress,
    report: &mut ScanReport,
) -> Result<()> {
    tracing::info!(dir = %dir.path, label = %dir.label, "scanning");
    let root = PathBuf::from(&dir.path);
    if !root.exists() {
        tracing::warn!(path = %dir.path, "directory does not exist on disk; skipping");
        return Ok(());
    }

    // 1. Load known videos into memory.
    let rows = sqlx::query(
        "SELECT id, relative_path, size_bytes, mtime_unix, missing, \
            thumbnail_ok, preview_ok, duration_secs \
         FROM videos WHERE directory_id = ?",
    )
    .bind(dir.id.raw())
    .fetch_all(pool)
    .await
    .context("loading known videos")?;

    let mut known: HashMap<String, KnownVideo> = HashMap::with_capacity(rows.len());
    for row in rows {
        let id: String = row.get("id");
        let rel: String = row.get("relative_path");
        let size: i64 = row.get("size_bytes");
        let mtime: i64 = row.get("mtime_unix");
        let duration_secs: Option<f64> = row.get("duration_secs");
        known.insert(
            rel,
            KnownVideo {
                id: VideoId(id),
                size_bytes: size,
                mtime_unix: mtime,
                missing: bool_from_i64(&row, "missing"),
                thumbnail_ok: bool_from_i64(&row, "thumbnail_ok"),
                preview_ok: bool_from_i64(&row, "preview_ok"),
                duration_secs,
            },
        );
    }

    // Collect videos that survive the walk (unchanged or updated) so we can verify
    // their cache outputs at the end. We keep a (VideoId, thumbnail_ok, preview_ok,
    // duration_secs) snapshot — post-walk DB state for those flags is consistent with
    // what we saw, since the only mutation path that clears flags (change detected)
    // is self-contained in `update_changed_video`.
    let mut surviving: Vec<(VideoId, bool, bool, Option<f64>)> = Vec::new();

    // 2. Walk the directory.
    for entry in WalkDir::new(&root)
        .follow_links(true)
        .into_iter()
        .filter_map(|r| r.ok())
    {
        if !entry.file_type().is_file() {
            continue;
        }
        if !is_video_extension(entry.path()) {
            continue;
        }
        progress.files_seen.fetch_add(1, Ordering::Relaxed);
        report.files_seen += 1;

        let rel = match entry.path().strip_prefix(&root) {
            Ok(p) => crate::util::path::path_to_db_string(p),
            Err(_) => continue,
        };

        let meta = match entry.metadata() {
            Ok(m) => m,
            Err(err) => {
                tracing::warn!(path = %entry.path().display(), error = %err, "stat failed");
                continue;
            }
        };
        let size = meta.len() as i64;
        let mtime = mtime_to_unix(&meta);
        let filename = entry.file_name().to_string_lossy().into_owned();

        let entry_known = known.remove(&rel);

        match entry_known {
            None => {
                mutations::insert_new_video(pool, clock, dir, &rel, &filename, size, mtime).await?;
                progress.new_videos.fetch_add(1, Ordering::Relaxed);
                report.new_videos += 1;
                // Newly-inserted videos haven't generated anything yet; the probe job
                // is already queued and will enqueue thumbnail+preview on completion.
                // Skip the cache verification for them below.
            }
            Some(k) if k.size_bytes != size || k.mtime_unix != mtime => {
                // Content changed on disk: clear flags, re-enqueue probe.
                mutations::update_changed_video(pool, clock, dir, &k.id, size, mtime, k.missing)
                    .await?;
                if !k.missing {
                    progress.changed_videos.fetch_add(1, Ordering::Relaxed);
                    report.changed_videos += 1;
                }
                // Skip cache verification — the probe's follow-up jobs cover regen.
            }
            Some(k) if k.missing => {
                // Un-missing without content change: preserve flags, re-insert the
                // directory collection membership. The post-walk cache verification
                // pass will detect any missing cache files and re-enqueue only the
                // jobs that are actually needed.
                mutations::un_mark_missing(pool, clock, dir, &k.id).await?;
                surviving.push((k.id, k.thumbnail_ok, k.preview_ok, k.duration_secs));
            }
            Some(k) => {
                // Unchanged: verify cache outputs at the end.
                surviving.push((k.id, k.thumbnail_ok, k.preview_ok, k.duration_secs));
            }
        }
    }

    // 3. Anything still in `known` wasn't found on disk.
    for (rel, k) in known.into_iter() {
        if k.missing {
            continue;
        }
        mutations::mark_missing(pool, clock, dir, &k.id).await?;
        progress.missing_videos.fetch_add(1, Ordering::Relaxed);
        report.missing_videos += 1;
        tracing::info!(
            video_id = %k.id,
            relative_path = %rel,
            "marked missing"
        );
    }

    // 4. Verify cache files for unchanged videos. Re-enqueue jobs whose outputs
    //    have been deleted (manual cache wipe, disk swap, etc.). We deliberately
    //    check this only for videos that remain on disk in this scan pass.
    for (video_id, thumbnail_ok, preview_ok, duration_secs) in surviving {
        verify::verify_cache_for_video(
            pool,
            clock,
            cache,
            &video_id,
            thumbnail_ok,
            preview_ok,
            duration_secs,
            progress,
            report,
        )
        .await?;
    }

    Ok(())
}

pub(super) fn is_video_extension(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|ext| {
            VIDEO_EXTENSIONS
                .iter()
                .any(|&v| v.eq_ignore_ascii_case(ext))
        })
        .unwrap_or(false)
}

pub(super) fn mtime_to_unix(meta: &std::fs::Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
