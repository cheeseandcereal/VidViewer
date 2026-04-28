//! Shared test fixtures for api/* inline test modules. Gated behind
//! `#[cfg(test)]`.

#![cfg(test)]

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::Value;
use tower::util::ServiceExt;

use crate::{http::router, state::AppState};

/// Build an `AppState` wired with mock Player / VideoTool. The tempdir
/// holding the DB + cache is leaked so the file sticks around for the
/// duration of the test; each test gets a fresh state and a fresh tempdir.
pub(super) async fn state() -> AppState {
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

pub(super) async fn json_body(resp: axum::response::Response) -> Value {
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).expect("valid JSON")
}

pub(super) fn get(uri: &str) -> Request<Body> {
    Request::builder().uri(uri).body(Body::empty()).unwrap()
}

pub(super) fn post_json(uri: &str, body: Value) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

pub(super) fn patch_json(uri: &str, body: Value) -> Request<Body> {
    Request::builder()
        .method("PATCH")
        .uri(uri)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

pub(super) fn delete(uri: &str) -> Request<Body> {
    Request::builder()
        .method("DELETE")
        .uri(uri)
        .body(Body::empty())
        .unwrap()
}

/// Create a temp directory on disk and `POST /api/directories` it,
/// returning its id. Leaks the tempdir so the path survives the test.
pub(super) async fn add_temp_directory(app: &axum::Router) -> (i64, std::path::PathBuf) {
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

/// Seed a video row directly via SQL. Returns the generated VideoId as a String.
pub(super) async fn seed_video(state: &AppState, dir_id: i64, filename: &str) -> String {
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

/// Convenience: build a router over the default test AppState.
pub(super) async fn test_app() -> axum::Router {
    router(state().await)
}
