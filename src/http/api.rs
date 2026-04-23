//! JSON API handlers.
//!
//! Handlers return `Result<Response, ApiError>`. The [`ApiError`](crate::http::error::ApiError)
//! wraps the various module-level typed errors and implements `IntoResponse`, so the
//! error path uses `?` rather than a match cascade at each site.

use std::path::PathBuf;

use axum::{
    extract::{Path as AxPath, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde::Deserialize;

use crate::{
    collections, directories, fs_browse, history,
    http::error::{bad_request, ApiError},
    ids::{CollectionId, DirectoryId, VideoId},
    player, scanner,
    state::AppState,
    ui_state, videos,
};

// ---------- Directories ----------

pub async fn list_directories(State(state): State<AppState>) -> Result<Response, ApiError> {
    let list = directories::list(&state.pool, true).await?;
    Ok(Json(list).into_response())
}

#[derive(Debug, Deserialize)]
pub struct AddDirectoryReq {
    pub path: String,
    pub label: Option<String>,
}

pub async fn add_directory(
    State(state): State<AppState>,
    Json(req): Json<AddDirectoryReq>,
) -> Result<Response, ApiError> {
    let path = PathBuf::from(&req.path);
    let dir = directories::add(&state.pool, &state.clock, &path, req.label).await?;

    // Immediately kick off a scan for the newly-added directory.
    let handle = scanner::spawn_one(
        state.pool.clone(),
        state.clock.clone(),
        scanner::CachePaths::from_config(&state.config),
        dir.id,
    );
    {
        let mut reg = state.scans.write().await;
        reg.current = Some(handle);
    }
    Ok((StatusCode::CREATED, Json(dir)).into_response())
}

#[derive(Debug, Deserialize)]
pub struct PatchDirectoryReq {
    pub label: String,
}

pub async fn patch_directory(
    State(state): State<AppState>,
    AxPath(id): AxPath<i64>,
    Json(req): Json<PatchDirectoryReq>,
) -> Result<Response, ApiError> {
    if req.label.trim().is_empty() {
        return Err(bad_request("bad_request", "label must be non-empty"));
    }
    let dir =
        directories::set_label(&state.pool, &state.clock, DirectoryId(id), &req.label).await?;
    Ok(Json(dir).into_response())
}

#[derive(Debug, Deserialize)]
pub struct DeleteDirectoryQuery {
    #[serde(default)]
    pub mode: Option<String>,
}

pub async fn delete_directory(
    State(state): State<AppState>,
    AxPath(id): AxPath<i64>,
    Query(q): Query<DeleteDirectoryQuery>,
) -> Result<Response, ApiError> {
    let mode = q.mode.as_deref().unwrap_or("soft");
    if mode != "soft" && mode != "hard" {
        return Err(bad_request("bad_mode", "mode must be 'soft' or 'hard'"));
    }

    // Abort any in-flight jobs for videos in this directory before we mutate
    // state. This terminates the underlying ffmpeg/ffprobe processes via
    // `kill_on_drop(true)` on the Command and prevents a running worker from
    // completing its work on a row we're about to soft-delete or cascade-delete.
    if let Err(err) = cancel_running_jobs_for_directory(&state, DirectoryId(id)).await {
        tracing::warn!(
            directory_id = id,
            error = %err,
            "failed to cancel in-flight jobs for directory (continuing with remove)"
        );
    }

    match mode {
        "soft" => {
            directories::soft_remove(&state.pool, &state.clock, DirectoryId(id)).await?;
            Ok((StatusCode::NO_CONTENT, ()).into_response())
        }
        "hard" => {
            let cache = scanner::CachePaths::from_config(&state.config);
            let report =
                directories::hard_remove(&state.pool, &state.clock, &cache, DirectoryId(id))
                    .await?;
            Ok((StatusCode::OK, Json(report)).into_response())
        }
        _ => unreachable!("mode validated above"),
    }
}

/// Purge the jobs table of any pending or running work for videos in this
/// directory, and abort any in-flight worker tasks processing them.
///
/// Order matters: we DELETE pending rows first so no worker can claim a new
/// one after this function returns. Then we cancel running jobs via the
/// registry (which aborts the task and — via `kill_on_drop(true)` on every
/// ffmpeg Command — terminates the OS process immediately).
///
/// This is called before the directory's own state is mutated. The combination
/// is race-free: once this returns, the jobs table contains no `pending` or
/// `running` rows for videos in this directory, and no worker task is actively
/// processing one either.
async fn cancel_running_jobs_for_directory(
    state: &AppState,
    dir_id: DirectoryId,
) -> anyhow::Result<()> {
    use anyhow::Context;
    use sqlx::Row;

    // 1. Delete any pending rows first. This closes the door on any idle
    //    worker that would otherwise claim one while we're cancelling runners.
    //    `status = 'pending'` is the only status that a worker can transition
    //    TO `running`, so after this DELETE no new ffmpeg can be spawned via
    //    the normal claim path for videos in this directory.
    let cancelled_pending = sqlx::query(
        "DELETE FROM jobs \
         WHERE status = 'pending' \
         AND video_id IN (SELECT id FROM videos WHERE directory_id = ?)",
    )
    .bind(dir_id.raw())
    .execute(&state.pool)
    .await
    .context("deleting pending jobs for directory")?
    .rows_affected();

    // 2. Collect video ids so we can signal the registry to abort any worker
    //    tasks currently running jobs for those videos.
    let rows = sqlx::query("SELECT id FROM videos WHERE directory_id = ?")
        .bind(dir_id.raw())
        .fetch_all(&state.pool)
        .await
        .context("listing videos for job cancellation")?;
    let video_ids: Vec<VideoId> = rows
        .into_iter()
        .map(|r| VideoId(r.get::<String, _>(0)))
        .collect();

    let aborted = if video_ids.is_empty() {
        Vec::new()
    } else {
        state.job_registry.cancel_for_videos(&video_ids)
    };

    // 3. Remove the aborted rows from the `jobs` table. The worker loop also
    //    deletes them when it observes the JoinError::cancelled, but we delete
    //    them here defensively so the caller sees a clean `jobs` state.
    if !aborted.is_empty() {
        let placeholders = vec!["?"; aborted.len()].join(",");
        let sql = format!("DELETE FROM jobs WHERE id IN ({placeholders})");
        let mut q = sqlx::query(&sql);
        for id in &aborted {
            q = q.bind(id);
        }
        q.execute(&state.pool)
            .await
            .context("deleting aborted job rows")?;
    }

    if cancelled_pending + aborted.len() as u64 > 0 {
        tracing::info!(
            directory_id = %dir_id,
            cancelled_pending,
            aborted_running = aborted.len(),
            "cancelled jobs for directory"
        );
    }
    Ok(())
}

// ---------- FS picker ----------

#[derive(Debug, Deserialize)]
pub struct FsListQuery {
    pub path: Option<String>,
}

pub async fn fs_list(
    State(state): State<AppState>,
    Query(q): Query<FsListQuery>,
) -> Result<Response, ApiError> {
    let path = if let Some(p) = q.path {
        PathBuf::from(p)
    } else if let Ok(Some(last)) = ui_state::get_last_browsed_path(&state.pool).await {
        let p = PathBuf::from(&last);
        if p.is_dir() {
            p
        } else {
            home_or_root()
        }
    } else {
        home_or_root()
    };

    let listing = fs_browse::list_dirs(&path)?;
    let _ = ui_state::set_last_browsed_path(&state.pool, &listing.path).await;
    Ok(Json(listing).into_response())
}

fn home_or_root() -> PathBuf {
    dirs::home_dir().unwrap_or_else(|| PathBuf::from("/"))
}

// ---------- Scan ----------

#[derive(Debug, Deserialize)]
pub struct ScanReq {
    pub dir_id: Option<i64>,
}

pub async fn start_scan(
    State(state): State<AppState>,
    Query(q): Query<ScanReq>,
) -> Result<Response, ApiError> {
    let only = q.dir_id.map(DirectoryId);
    let cache = scanner::CachePaths::from_config(&state.config);
    let handle = match only {
        Some(id) => scanner::spawn_one(state.pool.clone(), state.clock.clone(), cache, id),
        None => scanner::spawn_all(state.pool.clone(), state.clock.clone(), cache),
    };
    {
        let mut reg = state.scans.write().await;
        reg.current = Some(handle);
    }
    Ok(Json(serde_json::json!({"status": "started"})).into_response())
}

pub async fn scan_status(State(state): State<AppState>) -> Response {
    let (phase, files_seen, new_videos, changed_videos, missing_videos, error) = {
        let reg = state.scans.read().await;
        match &reg.current {
            Some(handle) => {
                let p = &handle.progress;
                let phase = match p.phase.load(std::sync::atomic::Ordering::SeqCst) {
                    0 => "walking",
                    1 => "done",
                    2 => "failed",
                    _ => "unknown",
                };
                let err = p.error.lock().unwrap().clone();
                (
                    phase,
                    p.files_seen.load(std::sync::atomic::Ordering::Relaxed),
                    p.new_videos.load(std::sync::atomic::Ordering::Relaxed),
                    p.changed_videos.load(std::sync::atomic::Ordering::Relaxed),
                    p.missing_videos.load(std::sync::atomic::Ordering::Relaxed),
                    err,
                )
            }
            None => ("idle", 0, 0, 0, 0, None),
        }
    };

    Json(serde_json::json!({
        "phase": phase,
        "files_seen": files_seen,
        "new_videos": new_videos,
        "changed_videos": changed_videos,
        "missing_videos": missing_videos,
        "error": error,
    }))
    .into_response()
}

/// Per-directory job status. Used by the Settings page.
pub async fn directory_job_status(State(state): State<AppState>) -> Result<Response, ApiError> {
    let map = crate::jobs::counts_by_directory(&state.pool).await?;
    let keyed: std::collections::HashMap<String, _> =
        map.into_iter().map(|(k, v)| (k.to_string(), v)).collect();
    Ok(Json(keyed).into_response())
}

// ---------- Collections ----------

#[derive(Debug, Deserialize)]
pub struct KindQuery {
    pub kind: Option<String>,
}

pub async fn list_collections(
    State(state): State<AppState>,
    Query(q): Query<KindQuery>,
) -> Result<Response, ApiError> {
    let kind = match q.kind.as_deref() {
        Some("directory") => Some(collections::Kind::Directory),
        Some("custom") => Some(collections::Kind::Custom),
        _ => None,
    };
    let v = collections::list(&state.pool, kind).await?;
    Ok(Json(v).into_response())
}

#[derive(Debug, Deserialize)]
pub struct CreateCollectionReq {
    pub name: String,
    #[serde(default)]
    pub directory_ids: Vec<i64>,
}

pub async fn create_collection(
    State(state): State<AppState>,
    Json(req): Json<CreateCollectionReq>,
) -> Result<Response, ApiError> {
    let dir_ids: Vec<DirectoryId> = req.directory_ids.into_iter().map(DirectoryId).collect();
    let c = collections::create_custom(&state.pool, &state.clock, &req.name, &dir_ids).await?;
    Ok((StatusCode::CREATED, Json(c)).into_response())
}

#[derive(Debug, Deserialize)]
pub struct RenameCollectionReq {
    pub name: String,
}

pub async fn rename_collection(
    State(state): State<AppState>,
    AxPath(id): AxPath<i64>,
    Json(req): Json<RenameCollectionReq>,
) -> Result<Response, ApiError> {
    let c = collections::rename(&state.pool, &state.clock, CollectionId(id), &req.name).await?;
    Ok(Json(c).into_response())
}

pub async fn delete_collection(
    State(state): State<AppState>,
    AxPath(id): AxPath<i64>,
) -> Result<Response, ApiError> {
    collections::delete_custom(&state.pool, CollectionId(id)).await?;
    Ok((StatusCode::NO_CONTENT, ()).into_response())
}

pub async fn list_collection_videos(
    State(state): State<AppState>,
    AxPath(id): AxPath<i64>,
) -> Result<Response, ApiError> {
    let v = collections::videos_in(&state.pool, CollectionId(id)).await?;
    Ok(Json(v).into_response())
}

pub async fn list_collection_directories(
    State(state): State<AppState>,
    AxPath(id): AxPath<i64>,
) -> Result<Response, ApiError> {
    let v = collections::directories_in(&state.pool, CollectionId(id)).await?;
    Ok(Json(v).into_response())
}

#[derive(Debug, Deserialize)]
pub struct CollectionDirectoryReq {
    pub directory_id: i64,
}

pub async fn add_directory_to_collection(
    State(state): State<AppState>,
    AxPath(id): AxPath<i64>,
    Json(req): Json<CollectionDirectoryReq>,
) -> Result<Response, ApiError> {
    collections::add_directory(
        &state.pool,
        &state.clock,
        CollectionId(id),
        DirectoryId(req.directory_id),
    )
    .await?;
    Ok((StatusCode::CREATED, ()).into_response())
}

pub async fn remove_directory_from_collection(
    State(state): State<AppState>,
    AxPath((cid, did)): AxPath<(i64, i64)>,
) -> Result<Response, ApiError> {
    collections::remove_directory(
        &state.pool,
        &state.clock,
        CollectionId(cid),
        DirectoryId(did),
    )
    .await?;
    Ok((StatusCode::NO_CONTENT, ()).into_response())
}

pub async fn random_from_collection(
    State(state): State<AppState>,
    AxPath(id): AxPath<i64>,
) -> Result<Response, ApiError> {
    match collections::random_video(&state.pool, CollectionId(id)).await? {
        Some(v) => Ok(Json(serde_json::json!({ "video_id": v })).into_response()),
        None => Err(ApiError::NotFound("empty")),
    }
}

// ---------- Videos + player ----------

#[derive(Debug, Deserialize)]
pub struct PlayQuery {
    pub start: Option<f64>,
}

pub async fn get_video(
    State(state): State<AppState>,
    AxPath(id): AxPath<String>,
) -> Result<Response, ApiError> {
    let vid = VideoId(id);
    match videos::get_detail(&state.pool, &vid).await? {
        Some(d) => Ok(Json(d).into_response()),
        None => Err(ApiError::NotFound("not_found")),
    }
}

pub async fn play_video(
    State(state): State<AppState>,
    AxPath(id): AxPath<String>,
    Query(q): Query<PlayQuery>,
) -> Result<Response, ApiError> {
    let vid = VideoId(id);
    let video = videos::get_detail(&state.pool, &vid)
        .await?
        .ok_or(ApiError::NotFound("not_found"))?;
    if video.video.missing {
        return Err(bad_request("video_missing", "video file is not on disk"));
    }
    let abs_path = std::path::PathBuf::from(&video.directory_path).join(&video.video.relative_path);
    let start = if let Some(s) = q.start {
        s.max(0.0)
    } else {
        history::start_position(&state.pool, &vid)
            .await
            .unwrap_or(0.0)
    };

    let session = state.player.launch(&abs_path, start).await.map_err(|err| {
        tracing::error!(error = %err, "launch failed");
        ApiError::Internal(err.context("player launch"))
    })?;

    if let Some(child) = session.child {
        player::session::spawn(
            state.pool.clone(),
            state.clock.clone(),
            vid.clone(),
            session.socket_path.clone(),
            child,
        );
    }

    Ok((
        StatusCode::ACCEPTED,
        Json(serde_json::json!({"status": "launched", "start": start})),
    )
        .into_response())
}

// ---------- History ----------

pub async fn list_history(State(state): State<AppState>) -> Result<Response, ApiError> {
    let v = history::list(&state.pool).await?;
    Ok(Json(v).into_response())
}

pub async fn delete_history(
    State(state): State<AppState>,
    AxPath(id): AxPath<String>,
) -> Result<Response, ApiError> {
    history::clear(&state.pool, &VideoId(id)).await?;
    Ok((StatusCode::NO_CONTENT, ()).into_response())
}
