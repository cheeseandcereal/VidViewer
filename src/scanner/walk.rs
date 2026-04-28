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
    scanner::{mutations, sniff, verify, CachePaths, ScanProgress, ScanReport},
};

/// Snapshot of a video row that survived the walk. Passed to the
/// post-walk cache-verification pass so it can decide whether to
/// re-enqueue a thumbnail or preview job. Fields mirror the subset of
/// `videos` columns verify needs; we take the snapshot at walk time to
/// avoid a second SELECT per row.
struct SurvivingRow {
    id: VideoId,
    thumbnail_ok: bool,
    preview_ok: bool,
    duration_secs: Option<f64>,
    is_audio_only: bool,
    attached_pic_stream_index: Option<i64>,
}

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
    is_audio_only: bool,
    attached_pic_stream_index: Option<i64>,
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
            thumbnail_ok, preview_ok, duration_secs, is_audio_only, \
            attached_pic_stream_index \
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
        let attached_pic_stream_index: Option<i64> = row.get("attached_pic_stream_index");
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
                is_audio_only: bool_from_i64(&row, "is_audio_only"),
                attached_pic_stream_index,
            },
        );
    }

    // Collect videos that survive the walk (unchanged or updated) so we can verify
    // their cache outputs at the end. Post-walk DB state for these flags is
    // consistent with what we observed in the snapshot, since the only mutation
    // path that clears flags (change detected) is self-contained in
    // `update_changed_video`.
    let mut surviving: Vec<SurvivingRow> = Vec::new();

    // 2. Walk the directory. Non-recursive: only videos sitting directly in
    //    the configured directory are indexed. Subdirectories are ignored
    //    deliberately — if a user wants a nested folder indexed, they add
    //    it as its own top-level directory in Settings.
    for entry in WalkDir::new(&root)
        .follow_links(true)
        .max_depth(1)
        .into_iter()
        .filter_map(|r| r.ok())
    {
        if !entry.file_type().is_file() {
            continue;
        }

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
                // New file — sniff its bytes to decide whether it's media.
                if !sniff_or_warn(entry.path()) {
                    continue;
                }
                progress.files_seen.fetch_add(1, Ordering::Relaxed);
                report.files_seen += 1;

                mutations::insert_new_video(pool, clock, dir, &rel, &filename, size, mtime).await?;
                progress.new_videos.fetch_add(1, Ordering::Relaxed);
                report.new_videos += 1;
                // Newly-inserted videos haven't generated anything yet; the probe job
                // is already queued and will enqueue thumbnail+preview on completion.
                // Skip the cache verification for them below.
            }
            Some(k) if k.size_bytes != size || k.mtime_unix != mtime => {
                // Content changed on disk — re-sniff. If the replaced bytes no
                // longer look like media (someone overwrote the file with
                // text/etc.), put it back in `known` so the post-walk loop
                // marks it missing.
                if !sniff_or_warn(entry.path()) {
                    known.insert(rel.clone(), k);
                    continue;
                }
                progress.files_seen.fetch_add(1, Ordering::Relaxed);
                report.files_seen += 1;

                mutations::update_changed_video(pool, clock, dir, &k.id, size, mtime, k.missing)
                    .await?;
                if !k.missing {
                    progress.changed_videos.fetch_add(1, Ordering::Relaxed);
                    report.changed_videos += 1;
                }
                // Skip cache verification — the probe's follow-up jobs cover regen.
            }
            Some(k) if k.missing => {
                // Un-missing without content change: preserve flags. No need to
                // sniff again; the stat signature matches the row we already
                // accepted as media once.
                progress.files_seen.fetch_add(1, Ordering::Relaxed);
                report.files_seen += 1;
                mutations::un_mark_missing(pool, clock, dir, &k.id).await?;
                surviving.push(SurvivingRow {
                    id: k.id,
                    thumbnail_ok: k.thumbnail_ok,
                    preview_ok: k.preview_ok,
                    duration_secs: k.duration_secs,
                    is_audio_only: k.is_audio_only,
                    attached_pic_stream_index: k.attached_pic_stream_index,
                });
            }
            Some(k) => {
                // Unchanged: verify cache outputs at the end. No sniff — stat
                // matches the row we already decided was media.
                progress.files_seen.fetch_add(1, Ordering::Relaxed);
                report.files_seen += 1;
                surviving.push(SurvivingRow {
                    id: k.id,
                    thumbnail_ok: k.thumbnail_ok,
                    preview_ok: k.preview_ok,
                    duration_secs: k.duration_secs,
                    is_audio_only: k.is_audio_only,
                    attached_pic_stream_index: k.attached_pic_stream_index,
                });
            }
        }
    }

    // 3. Anything still in `known` wasn't found on disk — or was re-sniffed as
    //    non-media after a change. Either way, flag it missing.
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
    for row in surviving {
        verify::verify_cache_for_video(
            pool,
            clock,
            cache,
            &row.id,
            row.thumbnail_ok,
            row.preview_ok,
            row.duration_secs,
            row.is_audio_only,
            row.attached_pic_stream_index,
            progress,
            report,
        )
        .await?;
    }

    Ok(())
}

/// Sniff a file's header; log a warning and reject on I/O error.
pub(super) fn sniff_or_warn(path: &Path) -> bool {
    match sniff::looks_like_media(path) {
        Ok(is_media) => is_media,
        Err(err) => {
            tracing::warn!(path = %path.display(), error = %err, "sniff failed");
            false
        }
    }
}

pub(super) fn mtime_to_unix(meta: &std::fs::Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
