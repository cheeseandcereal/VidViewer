//! HTTP server setup.
//!
//! Wires the router, static-file serving, page handlers, and graceful shutdown.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};

use anyhow::{Context, Result};
use axum::Router;
use tokio::signal;
use tower_http::{services::ServeDir, trace::TraceLayer};

use crate::state::AppState;

pub mod pages;

pub async fn serve(state: AppState) -> Result<()> {
    let port = state.config.port;
    let app = router(state);

    // Localhost only — this app is not meant for LAN / public exposure.
    let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), port);
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("binding to {addr}"))?;
    tracing::info!(%addr, "vidviewer listening");

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("http server terminated with error")?;
    Ok(())
}

pub(crate) fn router(state: AppState) -> Router {
    let static_dir = ServeDir::new("static");

    Router::new()
        .route("/healthz", axum::routing::get(healthz))
        .route("/", axum::routing::get(pages::home))
        .nest_service("/static", static_dir)
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
}
