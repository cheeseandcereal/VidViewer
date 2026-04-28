//! `/api/directories` and `/api/directories/:id` handlers.

use std::path::PathBuf;

use axum::{
    extract::{Path as AxPath, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde::Deserialize;

use crate::{
    directories,
    http::error::{bad_request, ApiError},
    ids::{DirectoryId, VideoId},
    scanner,
    state::AppState,
};

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

#[cfg(test)]
mod tests {
    use super::super::test_helpers::*;
    use axum::http::StatusCode;
    use tower::util::ServiceExt;

    #[tokio::test]
    async fn list_and_add_directory() {
        let app = test_app().await;

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
        let app = test_app().await;
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
        let app = test_app().await;
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
        let app = test_app().await;
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
        let app = test_app().await;
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
        let app = test_app().await;

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
}
