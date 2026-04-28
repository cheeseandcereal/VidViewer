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
    jobs, player, scanner,
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
    // Run the stuck-job watchdog ad-hoc before spawning the scan. If a probe
    // row was stranded in `running` (the partial unique index prevents a new
    // probe from being enqueued for the same (kind, video_id) until the row
    // is cleared), a manual rescan is the user's natural "please retry" —
    // unsticking those rows now means the rescan's re-enqueues actually land
    // instead of silently no-oping.
    //
    // The threshold here is even tighter than the periodic pass: a manual
    // rescan implies a deliberate wait for things to settle, so we're only
    // sparing the microsecond claim/register race window.
    if let Err(err) = jobs::reset_stuck_running(
        &state.pool,
        &state.clock,
        &state.job_registry,
        chrono::Duration::seconds(5),
    )
    .await
    {
        tracing::warn!(error = %err, "ad-hoc watchdog pass before scan failed");
    }

    // Also sweep out any historical `failed` rows whose failure mode is no
    // longer reproducible by the current code (preview/thumbnail against
    // audio-only rows). A scan is the natural moment to tidy them up — the
    // user has just asked for the state to be refreshed.
    if let Err(err) = jobs::cleanup_obsolete_failed_jobs(&state.pool).await {
        tracing::warn!(error = %err, "failed-job cleanup before scan failed");
    }

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

#[cfg(test)]
mod tests {
    //! Router-level integration tests for every api.rs handler. We build
    //! the real router against a tempdir-backed SQLite pool + mock
    //! `Player` / `VideoTool`, then drive it with
    //! `tower::ServiceExt::oneshot` so both routing and handler logic
    //! are exercised.
    use super::*;
    use crate::http::router;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use serde_json::Value;
    use tower::util::ServiceExt;

    /// Build an AppState wired with mock Player/VideoTool. The tempdir
    /// holding the DB and cache is leaked so the file sticks around for
    /// the duration of the test; each test gets a fresh state and a
    /// fresh tempdir.
    async fn state() -> AppState {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = crate::config::Config {
            data_dir: tmp.path().to_path_buf(),
            backup_dir: tmp.path().join("backups"),
            ..crate::config::Config::default()
        };
        let db_path = tmp.path().join("vidviewer.db");
        let pool = crate::db::init(&cfg, &db_path).await.unwrap();
        std::mem::forget(tmp);
        AppState::for_test(cfg, pool)
    }

    async fn json_body(resp: Response) -> Value {
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).expect("valid JSON")
    }

    fn get(uri: &str) -> Request<Body> {
        Request::builder().uri(uri).body(Body::empty()).unwrap()
    }

    fn post_json(uri: &str, body: Value) -> Request<Body> {
        Request::builder()
            .method("POST")
            .uri(uri)
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap()
    }

    fn patch_json(uri: &str, body: Value) -> Request<Body> {
        Request::builder()
            .method("PATCH")
            .uri(uri)
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap()
    }

    fn delete(uri: &str) -> Request<Body> {
        Request::builder()
            .method("DELETE")
            .uri(uri)
            .body(Body::empty())
            .unwrap()
    }

    /// Create a temp directory on disk and `POST /api/directories` it,
    /// returning its id. Leaks the tempdir so the path survives the
    /// test.
    async fn add_temp_directory(app: &axum::Router) -> (i64, std::path::PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().to_path_buf();
        std::mem::forget(tmp);
        let resp = app
            .clone()
            .oneshot(post_json(
                "/api/directories",
                serde_json::json!({ "path": path.to_string_lossy() }),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
        let body = json_body(resp).await;
        let id = body.get("id").and_then(|v| v.as_i64()).unwrap();
        (id, path)
    }

    // ---------- Directories ----------

    #[tokio::test]
    async fn list_and_add_directory() {
        let app = router(state().await);

        // Empty at start.
        let resp = app.clone().oneshot(get("/api/directories")).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = json_body(resp).await;
        assert_eq!(body.as_array().unwrap().len(), 0);

        // Add one.
        let (_id, _path) = add_temp_directory(&app).await;

        let resp = app.clone().oneshot(get("/api/directories")).await.unwrap();
        let body = json_body(resp).await;
        assert_eq!(body.as_array().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn add_directory_rejects_non_absolute_path() {
        let app = router(state().await);
        let resp = app
            .oneshot(post_json(
                "/api/directories",
                serde_json::json!({ "path": "relative/path" }),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let body = json_body(resp).await;
        assert_eq!(body["error"], "path_not_absolute");
    }

    #[tokio::test]
    async fn add_directory_rejects_missing_path() {
        let app = router(state().await);
        let resp = app
            .oneshot(post_json(
                "/api/directories",
                serde_json::json!({ "path": "/tmp/does-not-exist-vidviewer-test" }),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let body = json_body(resp).await;
        assert_eq!(body["error"], "path_not_found");
    }

    #[tokio::test]
    async fn add_directory_duplicate_returns_conflict() {
        let app = router(state().await);
        let (_id, path) = add_temp_directory(&app).await;
        let resp = app
            .oneshot(post_json(
                "/api/directories",
                serde_json::json!({ "path": path.to_string_lossy() }),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CONFLICT);
        let body = json_body(resp).await;
        assert_eq!(body["error"], "path_already_added");
    }

    #[tokio::test]
    async fn patch_directory_renames_and_rejects_empty() {
        let app = router(state().await);
        let (id, _path) = add_temp_directory(&app).await;

        let resp = app
            .clone()
            .oneshot(patch_json(
                &format!("/api/directories/{id}"),
                serde_json::json!({ "label": "New Label" }),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = json_body(resp).await;
        assert_eq!(body["label"], "New Label");

        let resp = app
            .oneshot(patch_json(
                &format!("/api/directories/{id}"),
                serde_json::json!({ "label": "   " }),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn delete_directory_soft_and_hard_and_bad_mode() {
        let app = router(state().await);

        // Soft remove.
        let (id, _) = add_temp_directory(&app).await;
        let resp = app
            .clone()
            .oneshot(delete(&format!("/api/directories/{id}")))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);

        // Hard remove a different one.
        let (id2, _) = add_temp_directory(&app).await;
        let resp = app
            .clone()
            .oneshot(delete(&format!("/api/directories/{id2}?mode=hard")))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = json_body(resp).await;
        assert!(body.get("deleted_videos").is_some());

        // Bad mode.
        let (id3, _) = add_temp_directory(&app).await;
        let resp = app
            .oneshot(delete(&format!("/api/directories/{id3}?mode=banana")))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let body = json_body(resp).await;
        assert_eq!(body["error"], "bad_mode");
    }

    // ---------- Collections ----------

    #[tokio::test]
    async fn create_rename_delete_custom_collection() {
        let app = router(state().await);

        let resp = app
            .clone()
            .oneshot(post_json(
                "/api/collections",
                serde_json::json!({ "name": "Mine" }),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
        let body = json_body(resp).await;
        let cid = body["id"].as_i64().unwrap();
        assert_eq!(body["name"], "Mine");
        assert_eq!(body["kind"], "custom");

        // Rename.
        let resp = app
            .clone()
            .oneshot(patch_json(
                &format!("/api/collections/{cid}"),
                serde_json::json!({ "name": "Renamed" }),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = json_body(resp).await;
        assert_eq!(body["name"], "Renamed");

        // Delete.
        let resp = app
            .oneshot(delete(&format!("/api/collections/{cid}")))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    }

    #[tokio::test]
    async fn create_collection_rejects_empty_name() {
        let app = router(state().await);
        let resp = app
            .oneshot(post_json(
                "/api/collections",
                serde_json::json!({ "name": "   " }),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let body = json_body(resp).await;
        assert_eq!(body["error"], "empty_name");
    }

    #[tokio::test]
    async fn create_collection_with_seed_directories() {
        let app = router(state().await);
        let (dir_id, _) = add_temp_directory(&app).await;

        let resp = app
            .clone()
            .oneshot(post_json(
                "/api/collections",
                serde_json::json!({
                    "name": "Seeded",
                    "directory_ids": [dir_id],
                }),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
        let body = json_body(resp).await;
        let cid = body["id"].as_i64().unwrap();

        let resp = app
            .oneshot(get(&format!("/api/collections/{cid}/directories")))
            .await
            .unwrap();
        let body = json_body(resp).await;
        assert_eq!(body.as_array().unwrap().len(), 1);
        assert_eq!(body[0]["directory_id"], dir_id);
    }

    #[tokio::test]
    async fn delete_directory_collection_is_rejected() {
        let app = router(state().await);
        let (_dir_id, _) = add_temp_directory(&app).await;
        // The directory collection id: fetch from /api/collections.
        let resp = app.clone().oneshot(get("/api/collections")).await.unwrap();
        let body = json_body(resp).await;
        let coll_id = body[0]["id"].as_i64().unwrap();

        let resp = app
            .oneshot(delete(&format!("/api/collections/{coll_id}")))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let body = json_body(resp).await;
        assert_eq!(body["error"], "directory_collection_immutable");
    }

    #[tokio::test]
    async fn list_collections_filter_by_kind() {
        let app = router(state().await);
        let (_dir_id, _) = add_temp_directory(&app).await;
        // Create a custom one.
        let _ = app
            .clone()
            .oneshot(post_json(
                "/api/collections",
                serde_json::json!({ "name": "A" }),
            ))
            .await
            .unwrap();

        let resp = app
            .clone()
            .oneshot(get("/api/collections?kind=custom"))
            .await
            .unwrap();
        let body = json_body(resp).await;
        let arr = body.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["kind"], "custom");

        let resp = app
            .oneshot(get("/api/collections?kind=directory"))
            .await
            .unwrap();
        let body = json_body(resp).await;
        let arr = body.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["kind"], "directory");
    }

    #[tokio::test]
    async fn add_and_remove_directory_membership_in_custom_collection() {
        let app = router(state().await);
        let (dir_id, _) = add_temp_directory(&app).await;

        // Create empty custom.
        let resp = app
            .clone()
            .oneshot(post_json(
                "/api/collections",
                serde_json::json!({ "name": "Empty" }),
            ))
            .await
            .unwrap();
        let cid = json_body(resp).await["id"].as_i64().unwrap();

        // Add directory.
        let resp = app
            .clone()
            .oneshot(post_json(
                &format!("/api/collections/{cid}/directories"),
                serde_json::json!({ "directory_id": dir_id }),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);

        // Remove directory.
        let resp = app
            .oneshot(delete(&format!(
                "/api/collections/{cid}/directories/{dir_id}"
            )))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    }

    #[tokio::test]
    async fn random_from_empty_collection_is_404() {
        let app = router(state().await);
        let resp = app
            .clone()
            .oneshot(post_json(
                "/api/collections",
                serde_json::json!({ "name": "E" }),
            ))
            .await
            .unwrap();
        let cid = json_body(resp).await["id"].as_i64().unwrap();

        let resp = app
            .oneshot(get(&format!("/api/collections/{cid}/random")))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    // ---------- Videos / history ----------

    async fn seed_video(state: &AppState, dir_id: i64, filename: &str) -> String {
        use crate::ids::VideoId;
        let vid = VideoId::new_random();
        let now_s = state.clock.now().to_rfc3339();
        sqlx::query(
            "INSERT INTO videos (id, directory_id, relative_path, filename, size_bytes, \
             mtime_unix, duration_secs, codec, width, height, thumbnail_ok, preview_ok, \
             missing, is_audio_only, attached_pic_stream_index, created_at, updated_at) \
             VALUES (?, ?, ?, ?, 1, 1, 60.0, 'h264', 1280, 720, 0, 0, 0, 0, NULL, ?, ?)",
        )
        .bind(vid.as_str())
        .bind(dir_id)
        .bind(filename)
        .bind(filename)
        .bind(&now_s)
        .bind(&now_s)
        .execute(&state.pool)
        .await
        .unwrap();
        vid.to_string()
    }

    #[tokio::test]
    async fn get_video_happy_path_and_not_found() {
        let st = state().await;
        let (dir_id, _) = {
            let app = router(st.clone());
            add_temp_directory(&app).await
        };
        let vid = seed_video(&st, dir_id, "sample.mp4").await;

        let app = router(st);
        let resp = app
            .clone()
            .oneshot(get(&format!("/api/videos/{vid}")))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = json_body(resp).await;
        assert_eq!(body["video"]["filename"], "sample.mp4");

        let resp = app
            .oneshot(get("/api/videos/does-not-exist"))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn play_video_happy_path_and_missing() {
        let st = state().await;
        let (dir_id, _) = {
            let app = router(st.clone());
            add_temp_directory(&app).await
        };
        let vid = seed_video(&st, dir_id, "playme.mp4").await;
        let app = router(st.clone());

        // Happy path — MockPlayer records the launch and returns a
        // SessionHandle with child=None, so the handler skips the
        // session task and returns 202.
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/api/videos/{vid}/play"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::ACCEPTED);
        let body = json_body(resp).await;
        assert_eq!(body["status"], "launched");

        // Missing file → 400.
        sqlx::query("UPDATE videos SET missing = 1 WHERE id = ?")
            .bind(&vid)
            .execute(&st.pool)
            .await
            .unwrap();
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/api/videos/{vid}/play"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let body = json_body(resp).await;
        assert_eq!(body["error"], "video_missing");
    }

    #[tokio::test]
    async fn history_list_and_delete() {
        let st = state().await;
        let (dir_id, _) = {
            let app = router(st.clone());
            add_temp_directory(&app).await
        };
        let vid = seed_video(&st, dir_id, "watched.mp4").await;
        // Seed a history row.
        let now_s = st.clock.now().to_rfc3339();
        sqlx::query(
            "INSERT INTO watch_history (video_id, last_watched_at, position_secs, completed, \
             watch_count) VALUES (?, ?, 10.0, 0, 1)",
        )
        .bind(&vid)
        .bind(&now_s)
        .execute(&st.pool)
        .await
        .unwrap();

        let app = router(st);
        let resp = app.clone().oneshot(get("/api/history")).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = json_body(resp).await;
        assert_eq!(body.as_array().unwrap().len(), 1);

        let resp = app
            .clone()
            .oneshot(delete(&format!("/api/history/{vid}")))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);

        let resp = app.oneshot(get("/api/history")).await.unwrap();
        let body = json_body(resp).await;
        assert_eq!(body.as_array().unwrap().len(), 0);
    }

    // ---------- Scan / FS / directory-job-status ----------

    #[tokio::test]
    async fn start_scan_registers_a_scan_handle() {
        let app = router(state().await);
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/scan")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = json_body(resp).await;
        assert_eq!(body["status"], "started");
    }

    #[tokio::test]
    async fn scan_status_reports_phase_after_start() {
        let st = state().await;
        let app = router(st.clone());

        // Before any scan — status is idle.
        let resp = app.clone().oneshot(get("/api/scan/status")).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = json_body(resp).await;
        assert!(body.get("phase").is_some());

        // Kick off a scan.
        let _ = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/scan")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let resp = app.oneshot(get("/api/scan/status")).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = json_body(resp).await;
        assert!(body.get("phase").is_some());
    }

    #[tokio::test]
    async fn directory_job_status_shape() {
        let app = router(state().await);
        let resp = app.oneshot(get("/api/directories/jobs")).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = json_body(resp).await;
        // It's a map; empty on a fresh DB.
        assert!(body.is_object());
    }

    #[tokio::test]
    async fn fs_list_absolute_path_succeeds() {
        let app = router(state().await);
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("child")).unwrap();
        let path = tmp.path().to_string_lossy().into_owned();

        let resp = app
            .oneshot(get(&format!(
                "/api/fs/list?path={}",
                urlencoding::encode(&path)
            )))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = json_body(resp).await;
        assert_eq!(body["path"], path);
        assert!(body["entries"].is_array());
    }

    #[tokio::test]
    async fn fs_list_relative_path_is_bad_request() {
        let app = router(state().await);
        let resp = app
            .oneshot(get("/api/fs/list?path=relative"))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let body = json_body(resp).await;
        assert_eq!(body["error"], "path_not_absolute");
    }
}
