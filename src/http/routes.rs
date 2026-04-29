//! Router wiring for the HTTP server. Builds the axum `Router` by
//! pairing each route path with the appropriate handler from [`api`],
//! [`pages`], [`debug`], and [`static_assets`], and mounts
//! `tower_http::ServeDir` instances for the runtime asset caches.
//!
//! Kept in its own file so `http/mod.rs` stays focused on the server
//! lifecycle (startup reconciliation, worker spawn, bind + graceful
//! shutdown).

use axum::Router;
use tower_http::{services::ServeDir, trace::TraceLayer};

use super::{api, debug, pages, static_assets};
use crate::state::AppState;

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
