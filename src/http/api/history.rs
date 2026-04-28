//! `/api/history` and `/api/history/:id`.

use axum::{
    extract::{Path as AxPath, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};

use crate::{history, http::error::ApiError, ids::VideoId, state::AppState};

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
    use super::super::test_helpers::*;
    use axum::http::StatusCode;
    use tower::util::ServiceExt;

    #[tokio::test]
    async fn history_list_and_delete() {
        let st = state().await;
        let (dir_id, _) = {
            let app = crate::http::router(st.clone());
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

        let app = crate::http::router(st);
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
}
