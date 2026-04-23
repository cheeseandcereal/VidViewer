//! JSON API handlers.

use std::path::PathBuf;

use axum::{
    extract::{Path as AxPath, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde::Deserialize;

use crate::{
    collections::{self, MutationError},
    directories::{self, AddError},
    fs_browse, history,
    ids::{CollectionId, DirectoryId, VideoId},
    player, scanner,
    state::AppState,
    ui_state, videos,
};

// ---------- Directories ----------

pub async fn list_directories(State(state): State<AppState>) -> Response {
    match directories::list(&state.pool, true).await {
        Ok(list) => Json(list).into_response(),
        Err(err) => internal(err),
    }
}

#[derive(Debug, Deserialize)]
pub struct AddDirectoryReq {
    pub path: String,
    pub label: Option<String>,
}

pub async fn add_directory(
    State(state): State<AppState>,
    Json(req): Json<AddDirectoryReq>,
) -> Response {
    let path = PathBuf::from(&req.path);
    match directories::add(&state.pool, &state.clock, &path, req.label).await {
        Ok(dir) => {
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
            (StatusCode::CREATED, Json(dir)).into_response()
        }
        Err(err) => add_error_response(err),
    }
}

#[derive(Debug, Deserialize)]
pub struct PatchDirectoryReq {
    pub label: String,
}

pub async fn patch_directory(
    State(state): State<AppState>,
    AxPath(id): AxPath<i64>,
    Json(req): Json<PatchDirectoryReq>,
) -> Response {
    if req.label.trim().is_empty() {
        return bad_request("label must be non-empty");
    }
    match directories::set_label(&state.pool, &state.clock, DirectoryId(id), &req.label).await {
        Ok(dir) => Json(dir).into_response(),
        Err(err) => internal(err),
    }
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
) -> Response {
    let mode = q.mode.as_deref().unwrap_or("soft");
    match mode {
        "soft" => match directories::soft_remove(&state.pool, &state.clock, DirectoryId(id)).await
        {
            Ok(()) => (StatusCode::NO_CONTENT, ()).into_response(),
            Err(err) => internal(err),
        },
        "hard" => {
            let cache = scanner::CachePaths::from_config(&state.config);
            match directories::hard_remove(&state.pool, &state.clock, &cache, DirectoryId(id))
                .await
            {
                Ok(report) => (StatusCode::OK, Json(report)).into_response(),
                Err(err) => internal(err),
            }
        }
        _ => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "bad_mode", "message": "mode must be 'soft' or 'hard'"})),
        )
            .into_response(),
    }
}

fn add_error_response(err: AddError) -> Response {
    let status = err.status();
    (status, Json(err)).into_response()
}

// ---------- FS picker ----------

#[derive(Debug, Deserialize)]
pub struct FsListQuery {
    pub path: Option<String>,
}

pub async fn fs_list(State(state): State<AppState>, Query(q): Query<FsListQuery>) -> Response {
    // Resolve starting path: query > ui_state > $HOME > /
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

    match fs_browse::list_dirs(&path) {
        Ok(listing) => {
            // Record this path for next time.
            let _ = ui_state::set_last_browsed_path(&state.pool, &listing.path).await;
            Json(listing).into_response()
        }
        Err(err) => (err.status(), Json(err)).into_response(),
    }
}

fn home_or_root() -> PathBuf {
    dirs::home_dir().unwrap_or_else(|| PathBuf::from("/"))
}

// ---------- Scan ----------

#[derive(Debug, Deserialize)]
pub struct ScanReq {
    pub dir_id: Option<i64>,
}

pub async fn start_scan(State(state): State<AppState>, Query(q): Query<ScanReq>) -> Response {
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
    Json(serde_json::json!({"status": "started"})).into_response()
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

/// Per-directory job status. Used by the Settings page to show each directory's
/// current activity inline, without any all-time global counters.
pub async fn directory_job_status(State(state): State<AppState>) -> Response {
    match crate::jobs::counts_by_directory(&state.pool).await {
        Ok(map) => {
            // Re-key by string so JSON serializes cleanly regardless of JSON number limits.
            let keyed: std::collections::HashMap<String, _> =
                map.into_iter().map(|(k, v)| (k.to_string(), v)).collect();
            Json(keyed).into_response()
        }
        Err(err) => internal(err),
    }
}

// ---------- Collections ----------

#[derive(Debug, Deserialize)]
pub struct KindQuery {
    pub kind: Option<String>,
}

pub async fn list_collections(
    State(state): State<AppState>,
    Query(q): Query<KindQuery>,
) -> Response {
    let kind = match q.kind.as_deref() {
        Some("directory") => Some(collections::Kind::Directory),
        Some("custom") => Some(collections::Kind::Custom),
        _ => None,
    };
    match collections::list(&state.pool, kind).await {
        Ok(v) => Json(v).into_response(),
        Err(err) => internal(err),
    }
}

#[derive(Debug, Deserialize)]
pub struct CreateCollectionReq {
    pub name: String,
}

pub async fn create_collection(
    State(state): State<AppState>,
    Json(req): Json<CreateCollectionReq>,
) -> Response {
    match collections::create_custom(&state.pool, &state.clock, &req.name).await {
        Ok(c) => (StatusCode::CREATED, Json(c)).into_response(),
        Err(err) => mutation_error_response(err),
    }
}

#[derive(Debug, Deserialize)]
pub struct RenameCollectionReq {
    pub name: String,
}

pub async fn rename_collection(
    State(state): State<AppState>,
    AxPath(id): AxPath<i64>,
    Json(req): Json<RenameCollectionReq>,
) -> Response {
    match collections::rename(&state.pool, &state.clock, CollectionId(id), &req.name).await {
        Ok(c) => Json(c).into_response(),
        Err(err) => mutation_error_response(err),
    }
}

pub async fn delete_collection(State(state): State<AppState>, AxPath(id): AxPath<i64>) -> Response {
    match collections::delete_custom(&state.pool, CollectionId(id)).await {
        Ok(()) => (StatusCode::NO_CONTENT, ()).into_response(),
        Err(err) => mutation_error_response(err),
    }
}

pub async fn list_collection_videos(
    State(state): State<AppState>,
    AxPath(id): AxPath<i64>,
) -> Response {
    match collections::videos_in(&state.pool, CollectionId(id)).await {
        Ok(v) => Json(v).into_response(),
        Err(err) => internal(err),
    }
}

#[derive(Debug, Deserialize)]
pub struct CollectionVideoReq {
    pub video_id: String,
}

pub async fn add_video_to_collection(
    State(state): State<AppState>,
    AxPath(id): AxPath<i64>,
    Json(req): Json<CollectionVideoReq>,
) -> Response {
    match collections::add_video(
        &state.pool,
        &state.clock,
        CollectionId(id),
        &VideoId(req.video_id),
    )
    .await
    {
        Ok(()) => (StatusCode::CREATED, ()).into_response(),
        Err(err) => mutation_error_response(err),
    }
}

pub async fn remove_video_from_collection(
    State(state): State<AppState>,
    AxPath((cid, vid)): AxPath<(i64, String)>,
) -> Response {
    match collections::remove_video(&state.pool, CollectionId(cid), &VideoId(vid)).await {
        Ok(()) => (StatusCode::NO_CONTENT, ()).into_response(),
        Err(err) => mutation_error_response(err),
    }
}

pub async fn random_from_collection(
    State(state): State<AppState>,
    AxPath(id): AxPath<i64>,
) -> Response {
    match collections::random_video(&state.pool, CollectionId(id)).await {
        Ok(Some(v)) => Json(serde_json::json!({ "video_id": v })).into_response(),
        Ok(None) => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "empty"})),
        )
            .into_response(),
        Err(err) => internal(err),
    }
}

fn mutation_error_response(err: MutationError) -> Response {
    let status = err.status();
    (status, Json(err)).into_response()
}

// ---------- Videos + player ----------

#[derive(Debug, Deserialize)]
pub struct PlayQuery {
    pub start: Option<f64>,
}

pub async fn get_video(State(state): State<AppState>, AxPath(id): AxPath<String>) -> Response {
    let vid = VideoId(id);
    match videos::get_detail(&state.pool, &vid).await {
        Ok(Some(d)) => Json(d).into_response(),
        Ok(None) => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "not_found"})),
        )
            .into_response(),
        Err(err) => internal(err),
    }
}

pub async fn play_video(
    State(state): State<AppState>,
    AxPath(id): AxPath<String>,
    Query(q): Query<PlayQuery>,
) -> Response {
    let vid = VideoId(id);
    let video = match videos::get_detail(&state.pool, &vid).await {
        Ok(Some(v)) => v,
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({"error": "not_found"})),
            )
                .into_response();
        }
        Err(err) => return internal(err),
    };
    if video.video.missing {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "video_missing"})),
        )
            .into_response();
    }
    let abs_path = std::path::PathBuf::from(&video.directory_path).join(&video.video.relative_path);
    let start = if let Some(s) = q.start {
        s.max(0.0)
    } else {
        history::start_position(&state.pool, &vid)
            .await
            .unwrap_or(0.0)
    };

    // Launch via trait.
    let session = match state.player.launch(&abs_path, start).await {
        Ok(s) => s,
        Err(err) => {
            tracing::error!(error = %err, "launch failed");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({
                    "error": "player_launch_failed",
                    "message": err.to_string(),
                })),
            )
                .into_response();
        }
    };

    // Hand off the child to the session manager if we actually spawned one.
    if let Some(child) = session.child {
        player::session::spawn(
            state.pool.clone(),
            state.clock.clone(),
            vid.clone(),
            session.socket_path.clone(),
            child,
        );
    }

    (
        StatusCode::ACCEPTED,
        Json(serde_json::json!({"status": "launched", "start": start})),
    )
        .into_response()
}

// ---------- History ----------

pub async fn list_history(State(state): State<AppState>) -> Response {
    match history::list(&state.pool).await {
        Ok(v) => Json(v).into_response(),
        Err(err) => internal(err),
    }
}

pub async fn delete_history(State(state): State<AppState>, AxPath(id): AxPath<String>) -> Response {
    match history::clear(&state.pool, &VideoId(id)).await {
        Ok(()) => (StatusCode::NO_CONTENT, ()).into_response(),
        Err(err) => internal(err),
    }
}

// ---------- helpers ----------

fn internal<E: std::fmt::Display>(err: E) -> Response {
    tracing::error!(error = %err, "internal api error");
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(serde_json::json!({"error": "internal", "message": err.to_string()})),
    )
        .into_response()
}

fn bad_request(message: &str) -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(serde_json::json!({"error": "bad_request", "message": message})),
    )
        .into_response()
}
