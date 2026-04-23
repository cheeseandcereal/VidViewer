//! HTTP server setup.
//!
//! Wires the router, static-file serving, page handlers, and graceful shutdown.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};

use anyhow::{Context, Result};
use axum::Router;
use tokio::signal;
use tower_http::{services::ServeDir, trace::TraceLayer};

use crate::state::AppState;

pub mod api;
pub mod debug;
pub mod error;
pub mod pages;
pub mod static_assets;

pub async fn serve(state: AppState) -> Result<()> {
    let port = state.config.port;

    // Make sure cache dirs exist so ServeDir doesn't 404 due to missing directory.
    for p in [
        state.config.thumb_cache_dir(),
        state.config.preview_cache_dir(),
    ] {
        if let Err(err) = tokio::fs::create_dir_all(&p).await {
            tracing::warn!(path = %p.display(), error = %err, "could not create cache dir");
        }
    }

    // Reconcile leftover jobs from the previous run before workers come online.
    match crate::jobs::reconcile_on_startup(&state.pool).await {
        Ok(report) => {
            if report.dropped_orphan_video
                + report.dropped_removed_dir
                + report.dropped_missing_video
                + report.reset_running
                > 0
            {
                tracing::info!(
                    dropped_orphan_video = report.dropped_orphan_video,
                    dropped_removed_dir = report.dropped_removed_dir,
                    dropped_missing_video = report.dropped_missing_video,
                    reset_running = report.reset_running,
                    "reconciled leftover jobs at startup"
                );
            }
        }
        Err(err) => {
            tracing::error!(error = %err, "startup job reconciliation failed");
        }
    }

    // Spawn background job workers.
    let workers = crate::jobs::worker::Workers {
        pool: state.pool.clone(),
        clock: state.clock.clone(),
        config: state.config.clone(),
        video_tool: state.video_tool.clone(),
        thumb_dir: state.config.thumb_cache_dir(),
        preview_dir: state.config.preview_cache_dir(),
        registry: state.job_registry.clone(),
    };
    let _worker_handles = workers.spawn_all(
        state.config.worker_concurrency,
        state.config.preview_concurrency,
    );

    // Kick off an initial scan in the background if configured.
    if state.config.scan_on_startup {
        let handle = crate::scanner::spawn_all(
            state.pool.clone(),
            state.clock.clone(),
            crate::scanner::CachePaths::from_config(&state.config),
        );
        let mut reg = state.scans.write().await;
        reg.current = Some(handle);
    }

    let app = router(state);

    // Localhost only — this app is not meant for LAN / public exposure.
    let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), port);
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("binding to {addr}"))?;
    let url = format!("http://{addr}/");
    tracing::info!(%addr, %url, "vidviewer listening — open {url}");

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("http server terminated with error")?;
    Ok(())
}

pub(crate) fn router(state: AppState) -> Router {
    let thumbs_dir = ServeDir::new(state.config.thumb_cache_dir());
    let previews_dir = ServeDir::new(state.config.preview_cache_dir());

    Router::new()
        .route("/healthz", axum::routing::get(healthz))
        .route("/", axum::routing::get(pages::home))
        .route(
            "/collections/:id",
            axum::routing::get(pages::collection_page),
        )
        .route("/videos/:id", axum::routing::get(pages::video_detail_page))
        .route("/history", axum::routing::get(pages::history_page))
        .route("/settings", axum::routing::get(pages::settings))
        .route(
            "/api/directories",
            axum::routing::get(api::list_directories).post(api::add_directory),
        )
        .route(
            "/api/directories/:id",
            axum::routing::patch(api::patch_directory).delete(api::delete_directory),
        )
        .route(
            "/api/collections",
            axum::routing::get(api::list_collections).post(api::create_collection),
        )
        .route(
            "/api/collections/:id",
            axum::routing::patch(api::rename_collection).delete(api::delete_collection),
        )
        .route(
            "/api/collections/:id/videos",
            axum::routing::get(api::list_collection_videos),
        )
        .route(
            "/api/collections/:id/directories",
            axum::routing::get(api::list_collection_directories)
                .post(api::add_directory_to_collection),
        )
        .route(
            "/api/collections/:cid/directories/:did",
            axum::routing::delete(api::remove_directory_from_collection),
        )
        .route(
            "/api/collections/:id/random",
            axum::routing::get(api::random_from_collection),
        )
        .route("/api/videos/:id", axum::routing::get(api::get_video))
        .route("/api/videos/:id/play", axum::routing::post(api::play_video))
        .route("/api/history", axum::routing::get(api::list_history))
        .route(
            "/api/history/:id",
            axum::routing::delete(api::delete_history),
        )
        .route("/api/fs/list", axum::routing::get(api::fs_list))
        .route("/api/scan", axum::routing::post(api::start_scan))
        .route("/api/scan/status", axum::routing::get(api::scan_status))
        .route(
            "/api/directories/jobs",
            axum::routing::get(api::directory_job_status),
        )
        .route("/debug", axum::routing::get(debug::debug_dump))
        .route("/static/*path", axum::routing::get(static_assets::serve))
        .route("/favicon.ico", axum::routing::get(static_assets::favicon))
        .nest_service("/thumbs", thumbs_dir)
        .nest_service("/previews", previews_dir)
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

async fn healthz() -> axum::response::Response {
    use axum::response::IntoResponse;
    (
        [(
            axum::http::header::CONTENT_TYPE,
            "text/plain; charset=utf-8",
        )],
        "ok",
    )
        .into_response()
}

async fn shutdown_signal() {
    let ctrl_c = async {
        if let Err(err) = signal::ctrl_c().await {
            tracing::warn!(error = %err, "failed to install SIGINT handler");
        }
    };

    let terminate = async {
        match signal::unix::signal(signal::unix::SignalKind::terminate()) {
            Ok(mut s) => {
                s.recv().await;
            }
            Err(err) => {
                tracing::warn!(error = %err, "failed to install SIGTERM handler");
                std::future::pending::<()>().await;
            }
        }
    };

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }

    tracing::info!("shutdown signal received");
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::util::ServiceExt;

    pub(crate) async fn test_state() -> AppState {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = crate::config::Config {
            data_dir: tmp.path().to_path_buf(),
            backup_dir: tmp.path().join("backups"),
            ..crate::config::Config::default()
        };
        let db_path = tmp.path().join("vidviewer.db");
        let pool = crate::db::init(&cfg, &db_path).await.unwrap();
        // Leak the tempdir so the DB file lives for the duration of the test.
        std::mem::forget(tmp);
        AppState::new(cfg, pool)
    }

    #[tokio::test]
    async fn healthz_returns_ok() {
        let app = router(test_state().await);
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/healthz")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn root_serves_utf8_html() {
        let app = router(test_state().await);
        let resp = app
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let ctype = resp
            .headers()
            .get("content-type")
            .unwrap()
            .to_str()
            .unwrap();
        assert!(ctype.contains("text/html"), "got {ctype}");
        assert!(ctype.contains("charset=utf-8"), "got {ctype}");

        let body = axum::body::to_bytes(resp.into_body(), 1 << 20)
            .await
            .unwrap();
        let s = std::str::from_utf8(&body).unwrap();
        assert!(s.contains("<!doctype html>"));
        assert!(s.contains("Noto Sans CJK") || s.contains("/static/app.css"));
    }

    #[tokio::test]
    async fn static_app_css_served_from_embedded_assets() {
        let app = router(test_state().await);
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/static/app.css")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let ctype = resp
            .headers()
            .get("content-type")
            .unwrap()
            .to_str()
            .unwrap();
        assert!(ctype.starts_with("text/css"), "got {ctype}");
        let body = axum::body::to_bytes(resp.into_body(), 1 << 20)
            .await
            .unwrap();
        assert!(!body.is_empty(), "embedded app.css should have content");
    }

    #[tokio::test]
    async fn static_unknown_path_returns_404() {
        let app = router(test_state().await);
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/static/does-not-exist.xyz")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn favicon_is_served_from_embedded_svg() {
        let app = router(test_state().await);
        // Both the classic /favicon.ico path and the declared /static/favicon.svg
        // path should return the SVG bytes with the right MIME type.
        for uri in ["/favicon.ico", "/static/favicon.svg"] {
            let resp = app
                .clone()
                .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::OK, "uri={uri}");
            let ctype = resp
                .headers()
                .get("content-type")
                .unwrap()
                .to_str()
                .unwrap();
            assert_eq!(ctype, "image/svg+xml", "uri={uri}");
            let body = axum::body::to_bytes(resp.into_body(), 1 << 20)
                .await
                .unwrap();
            assert!(
                body.starts_with(b"<?xml") || body.starts_with(b"<svg"),
                "uri={uri} body did not look like SVG"
            );
        }
    }
}
