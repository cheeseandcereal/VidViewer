//! Introspection endpoint. Localhost-only and gated by
//! `config.enable_debug_endpoint = true`. Exposes job queue counts, scan progress,
//! and a brief directory/collection snapshot.

use axum::{extract::State, http::StatusCode, response::Response, Json};
use sqlx::Row;

use crate::{jobs, state::AppState};

pub async fn debug_dump(State(state): State<AppState>) -> Response {
    if !state.config.enable_debug_endpoint {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "debug_endpoint_disabled"})),
        )
            .into_response();
    }

    let (pending, running, done, failed) = jobs::count_by_status(&state.pool)
        .await
        .unwrap_or((0, 0, 0, 0));

    let video_totals = sqlx::query(
        "SELECT \
            COUNT(*) AS total, \
            SUM(CASE WHEN missing = 1 THEN 1 ELSE 0 END) AS missing, \
            SUM(CASE WHEN thumbnail_ok = 1 THEN 1 ELSE 0 END) AS thumb_ok, \
            SUM(CASE WHEN preview_ok = 1 THEN 1 ELSE 0 END) AS preview_ok \
         FROM videos",
    )
    .fetch_one(&state.pool)
    .await
    .ok();

    let (total, missing, thumb_ok, preview_ok) = if let Some(r) = video_totals {
        (
            r.get::<i64, _>("total"),
            r.get::<Option<i64>, _>("missing").unwrap_or(0),
            r.get::<Option<i64>, _>("thumb_ok").unwrap_or(0),
            r.get::<Option<i64>, _>("preview_ok").unwrap_or(0),
        )
    } else {
        (0, 0, 0, 0)
    };

    let scan_phase = {
        let reg = state.scans.read().await;
        reg.current.as_ref().map(|h| {
            let phase = h.progress.phase.load(std::sync::atomic::Ordering::SeqCst);
            match phase {
                0 => "walking",
                1 => "done",
                2 => "failed",
                _ => "unknown",
            }
        })
    };

    use axum::response::IntoResponse;
    Json(serde_json::json!({
        "jobs": {
            "pending": pending,
            "running": running,
            "done": done,
            "failed": failed,
        },
        "videos": {
            "total": total,
            "missing": missing,
            "thumbnail_ok": thumb_ok,
            "preview_ok": preview_ok,
        },
        "scan": scan_phase,
        "config": {
            "port": state.config.port,
            "worker_concurrency": state.config.worker_concurrency,
            "preview_concurrency": state.config.preview_concurrency,
            "scan_on_startup": state.config.scan_on_startup,
        },
    }))
    .into_response()
}
