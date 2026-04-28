//! `/api/scan`, `/api/scan/status`, and `/api/directories/jobs` handlers.
//!
//! `start_scan` also runs the stuck-job watchdog and obsolete-failed-jobs
//! cleanup opportunistically before dispatching the scan; see
//! `docs/design/05-jobs-and-workers.md`.

use axum::{
    extract::{Query, State},
    response::{IntoResponse, Response},
    Json,
};
use serde::Deserialize;

use crate::{http::error::ApiError, ids::DirectoryId, jobs, scanner, state::AppState};

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

#[cfg(test)]
mod tests {
    use super::super::test_helpers::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::util::ServiceExt;

    #[tokio::test]
    async fn start_scan_registers_a_scan_handle() {
        let app = test_app().await;
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
        let app = test_app().await;

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
        let app = test_app().await;
        let resp = app.oneshot(get("/api/directories/jobs")).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = json_body(resp).await;
        // It's a map; empty on a fresh DB.
        assert!(body.is_object());
    }
}
