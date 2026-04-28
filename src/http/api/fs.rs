//! `/api/fs/list` filesystem-picker handler.

use std::path::PathBuf;

use axum::{
    extract::{Query, State},
    response::{IntoResponse, Response},
    Json,
};
use serde::Deserialize;

use crate::{fs_browse, http::error::ApiError, state::AppState, ui_state};

#[derive(Debug, Deserialize)]
pub struct FsListQuery {
    pub path: Option<String>,
}

pub async fn fs_list(
    State(state): State<AppState>,
    Query(q): Query<FsListQuery>,
) -> Result<Response, ApiError> {
    let path = if let Some(p) = q.path {
        PathBuf::from(p)
    } else if let Ok(Some(last)) = ui_state::get_last_browsed_path(&state.pool).await {
        let p = PathBuf::from(&last);
        if p.is_dir() {
            p
        } else {
            home_or_root()
        }
    } else {
        home_or_root()
    };

    let listing = fs_browse::list_dirs(&path)?;
    let _ = ui_state::set_last_browsed_path(&state.pool, &listing.path).await;
    Ok(Json(listing).into_response())
}

fn home_or_root() -> PathBuf {
    dirs::home_dir().unwrap_or_else(|| PathBuf::from("/"))
}

#[cfg(test)]
mod tests {
    use super::super::test_helpers::*;
    use axum::http::StatusCode;
    use tower::util::ServiceExt;

    #[tokio::test]
    async fn fs_list_absolute_path_succeeds() {
        let app = test_app().await;
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("child")).unwrap();
        let path = tmp.path().to_string_lossy().into_owned();

        let resp = app
            .oneshot(get(&format!(
                "/api/fs/list?path={}",
                urlencoding::encode(&path)
            )))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = json_body(resp).await;
        assert_eq!(body["path"], path);
        assert!(body["entries"].is_array());
    }

    #[tokio::test]
    async fn fs_list_relative_path_is_bad_request() {
        let app = test_app().await;
        let resp = app
            .oneshot(get("/api/fs/list?path=relative"))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let body = json_body(resp).await;
        assert_eq!(body["error"], "path_not_absolute");
    }
}
