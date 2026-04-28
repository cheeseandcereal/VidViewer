//! `/api/videos/:id` and `/api/videos/:id/play`.

use axum::{
    extract::{Path as AxPath, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde::Deserialize;

use crate::{
    history,
    http::error::{bad_request, ApiError},
    ids::VideoId,
    player,
    state::AppState,
    videos,
};

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

#[derive(Debug, Deserialize)]
pub struct PlayQuery {
    pub start: Option<f64>,
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

#[cfg(test)]
mod tests {
    use super::super::test_helpers::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::util::ServiceExt;

    #[tokio::test]
    async fn get_video_happy_path_and_not_found() {
        let st = state().await;
        let (dir_id, _) = {
            let app = crate::http::router(st.clone());
            add_temp_directory(&app).await
        };
        let vid = seed_video(&st, dir_id, "sample.mp4").await;

        let app = crate::http::router(st);
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
            let app = crate::http::router(st.clone());
            add_temp_directory(&app).await
        };
        let vid = seed_video(&st, dir_id, "playme.mp4").await;
        let app = crate::http::router(st.clone());

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
}
