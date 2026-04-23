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
    directories::{self, AddError},
    fs_browse,
    ids::DirectoryId,
    scanner,
    state::AppState,
    ui_state,
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
        Ok(dir) => (StatusCode::CREATED, Json(dir)).into_response(),
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

pub async fn delete_directory(State(state): State<AppState>, AxPath(id): AxPath<i64>) -> Response {
    match directories::soft_remove(&state.pool, &state.clock, DirectoryId(id)).await {
        Ok(()) => (StatusCode::NO_CONTENT, ()).into_response(),
        Err(err) => internal(err),
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
    let handle = match only {
        Some(id) => scanner::spawn_one(state.pool.clone(), state.clock.clone(), id),
        None => scanner::spawn_all(state.pool.clone(), state.clock.clone()),
    };
    {
        let mut reg = state.scans.write().await;
        reg.current = Some(handle);
    }
    Json(serde_json::json!({"status": "started"})).into_response()
}

pub async fn scan_status(State(state): State<AppState>) -> Response {
    let reg = state.scans.read().await;
    let Some(handle) = &reg.current else {
        return Json(serde_json::json!({"phase": "idle"})).into_response();
    };
    let p = &handle.progress;
    let phase = match p.phase.load(std::sync::atomic::Ordering::SeqCst) {
        0 => "walking",
        1 => "done",
        2 => "failed",
        _ => "unknown",
    };
    let error = p.error.lock().unwrap().clone();
    Json(serde_json::json!({
        "phase": phase,
        "files_seen": p.files_seen.load(std::sync::atomic::Ordering::Relaxed),
        "new_videos": p.new_videos.load(std::sync::atomic::Ordering::Relaxed),
        "changed_videos": p.changed_videos.load(std::sync::atomic::Ordering::Relaxed),
        "missing_videos": p.missing_videos.load(std::sync::atomic::Ordering::Relaxed),
        "error": error,
    }))
    .into_response()
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
