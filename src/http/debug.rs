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

#[cfg(test)]
mod tests {
    use crate::http::router;
    use crate::state::AppState;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use serde_json::Value;
    use tower::util::ServiceExt;

    async fn state_with_debug(enabled: bool) -> AppState {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = crate::config::Config {
            data_dir: tmp.path().to_path_buf(),
            backup_dir: tmp.path().join("backups"),
            enable_debug_endpoint: enabled,
            ..crate::config::Config::default()
        };
        let db_path = tmp.path().join("vidviewer.db");
        let pool = crate::db::init(&cfg, &db_path).await.unwrap();
        std::mem::forget(tmp);
        AppState::for_test(cfg, pool)
    }

    #[tokio::test]
    async fn debug_endpoint_returns_404_when_disabled() {
        let app = router(state_with_debug(false).await);
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/debug")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        let body = axum::body::to_bytes(resp.into_body(), 1 << 20)
            .await
            .unwrap();
        let json: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["error"], "debug_endpoint_disabled");
    }

    #[tokio::test]
    async fn debug_endpoint_returns_shape_when_enabled() {
        let app = router(state_with_debug(true).await);
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/debug")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), 1 << 20)
            .await
            .unwrap();
        let json: Value = serde_json::from_slice(&body).unwrap();
        // Required top-level keys.
        for key in ["jobs", "videos", "scan", "config"] {
            assert!(json.get(key).is_some(), "missing key {key}: {json}");
        }
        // Jobs sub-object.
        for key in ["pending", "running", "done", "failed"] {
            assert!(
                json["jobs"].get(key).is_some(),
                "missing jobs.{key}: {json}"
            );
        }
        // Videos sub-object.
        for key in ["total", "missing", "thumbnail_ok", "preview_ok"] {
            assert!(
                json["videos"].get(key).is_some(),
                "missing videos.{key}: {json}"
            );
        }
        // Config sub-object carries runtime knobs.
        assert_eq!(json["config"]["port"], 7878);
    }
}
