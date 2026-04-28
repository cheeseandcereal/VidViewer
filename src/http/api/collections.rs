//! `/api/collections` and sub-resources.

use axum::{
    extract::{Path as AxPath, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde::Deserialize;

use crate::{
    collections,
    http::error::ApiError,
    ids::{CollectionId, DirectoryId},
    state::AppState,
};

#[derive(Debug, Deserialize)]
pub struct KindQuery {
    pub kind: Option<String>,
}

pub async fn list_collections(
    State(state): State<AppState>,
    Query(q): Query<KindQuery>,
) -> Result<Response, ApiError> {
    let kind = match q.kind.as_deref() {
        Some("directory") => Some(collections::Kind::Directory),
        Some("custom") => Some(collections::Kind::Custom),
        _ => None,
    };
    let v = collections::list(&state.pool, kind).await?;
    Ok(Json(v).into_response())
}

#[derive(Debug, Deserialize)]
pub struct CreateCollectionReq {
    pub name: String,
    #[serde(default)]
    pub directory_ids: Vec<i64>,
}

pub async fn create_collection(
    State(state): State<AppState>,
    Json(req): Json<CreateCollectionReq>,
) -> Result<Response, ApiError> {
    let dir_ids: Vec<DirectoryId> = req.directory_ids.into_iter().map(DirectoryId).collect();
    let c = collections::create_custom(&state.pool, &state.clock, &req.name, &dir_ids).await?;
    Ok((StatusCode::CREATED, Json(c)).into_response())
}

#[derive(Debug, Deserialize)]
pub struct RenameCollectionReq {
    pub name: String,
}

pub async fn rename_collection(
    State(state): State<AppState>,
    AxPath(id): AxPath<i64>,
    Json(req): Json<RenameCollectionReq>,
) -> Result<Response, ApiError> {
    let c = collections::rename(&state.pool, &state.clock, CollectionId(id), &req.name).await?;
    Ok(Json(c).into_response())
}

pub async fn delete_collection(
    State(state): State<AppState>,
    AxPath(id): AxPath<i64>,
) -> Result<Response, ApiError> {
    collections::delete_custom(&state.pool, CollectionId(id)).await?;
    Ok((StatusCode::NO_CONTENT, ()).into_response())
}

pub async fn list_collection_videos(
    State(state): State<AppState>,
    AxPath(id): AxPath<i64>,
) -> Result<Response, ApiError> {
    let v = collections::videos_in(&state.pool, CollectionId(id)).await?;
    Ok(Json(v).into_response())
}

pub async fn list_collection_directories(
    State(state): State<AppState>,
    AxPath(id): AxPath<i64>,
) -> Result<Response, ApiError> {
    let v = collections::directories_in(&state.pool, CollectionId(id)).await?;
    Ok(Json(v).into_response())
}

#[derive(Debug, Deserialize)]
pub struct CollectionDirectoryReq {
    pub directory_id: i64,
}

pub async fn add_directory_to_collection(
    State(state): State<AppState>,
    AxPath(id): AxPath<i64>,
    Json(req): Json<CollectionDirectoryReq>,
) -> Result<Response, ApiError> {
    collections::add_directory(
        &state.pool,
        &state.clock,
        CollectionId(id),
        DirectoryId(req.directory_id),
    )
    .await?;
    Ok((StatusCode::CREATED, ()).into_response())
}

pub async fn remove_directory_from_collection(
    State(state): State<AppState>,
    AxPath((cid, did)): AxPath<(i64, i64)>,
) -> Result<Response, ApiError> {
    collections::remove_directory(
        &state.pool,
        &state.clock,
        CollectionId(cid),
        DirectoryId(did),
    )
    .await?;
    Ok((StatusCode::NO_CONTENT, ()).into_response())
}

pub async fn random_from_collection(
    State(state): State<AppState>,
    AxPath(id): AxPath<i64>,
) -> Result<Response, ApiError> {
    match collections::random_video(&state.pool, CollectionId(id)).await? {
        Some(v) => Ok(Json(serde_json::json!({ "video_id": v })).into_response()),
        None => Err(ApiError::NotFound("empty")),
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_helpers::*;
    use axum::http::StatusCode;
    use tower::util::ServiceExt;

    #[tokio::test]
    async fn create_rename_delete_custom_collection() {
        let app = test_app().await;

        let resp = app
            .clone()
            .oneshot(post_json(
                "/api/collections",
                serde_json::json!({ "name": "Mine" }),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
        let body = json_body(resp).await;
        let cid = body["id"].as_i64().unwrap();
        assert_eq!(body["name"], "Mine");
        assert_eq!(body["kind"], "custom");

        // Rename.
        let resp = app
            .clone()
            .oneshot(patch_json(
                &format!("/api/collections/{cid}"),
                serde_json::json!({ "name": "Renamed" }),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = json_body(resp).await;
        assert_eq!(body["name"], "Renamed");

        // Delete.
        let resp = app
            .oneshot(delete(&format!("/api/collections/{cid}")))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    }

    #[tokio::test]
    async fn create_collection_rejects_empty_name() {
        let app = test_app().await;
        let resp = app
            .oneshot(post_json(
                "/api/collections",
                serde_json::json!({ "name": "   " }),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let body = json_body(resp).await;
        assert_eq!(body["error"], "empty_name");
    }

    #[tokio::test]
    async fn create_collection_with_seed_directories() {
        let app = test_app().await;
        let (dir_id, _) = add_temp_directory(&app).await;

        let resp = app
            .clone()
            .oneshot(post_json(
                "/api/collections",
                serde_json::json!({
                    "name": "Seeded",
                    "directory_ids": [dir_id],
                }),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
        let body = json_body(resp).await;
        let cid = body["id"].as_i64().unwrap();

        let resp = app
            .oneshot(get(&format!("/api/collections/{cid}/directories")))
            .await
            .unwrap();
        let body = json_body(resp).await;
        assert_eq!(body.as_array().unwrap().len(), 1);
        assert_eq!(body[0]["directory_id"], dir_id);
    }

    #[tokio::test]
    async fn delete_directory_collection_is_rejected() {
        let app = test_app().await;
        let (_dir_id, _) = add_temp_directory(&app).await;
        // The directory collection id: fetch from /api/collections.
        let resp = app.clone().oneshot(get("/api/collections")).await.unwrap();
        let body = json_body(resp).await;
        let coll_id = body[0]["id"].as_i64().unwrap();

        let resp = app
            .oneshot(delete(&format!("/api/collections/{coll_id}")))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let body = json_body(resp).await;
        assert_eq!(body["error"], "directory_collection_immutable");
    }

    #[tokio::test]
    async fn list_collections_filter_by_kind() {
        let app = test_app().await;
        let (_dir_id, _) = add_temp_directory(&app).await;
        // Create a custom one.
        let _ = app
            .clone()
            .oneshot(post_json(
                "/api/collections",
                serde_json::json!({ "name": "A" }),
            ))
            .await
            .unwrap();

        let resp = app
            .clone()
            .oneshot(get("/api/collections?kind=custom"))
            .await
            .unwrap();
        let body = json_body(resp).await;
        let arr = body.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["kind"], "custom");

        let resp = app
            .oneshot(get("/api/collections?kind=directory"))
            .await
            .unwrap();
        let body = json_body(resp).await;
        let arr = body.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["kind"], "directory");
    }

    #[tokio::test]
    async fn add_and_remove_directory_membership_in_custom_collection() {
        let app = test_app().await;
        let (dir_id, _) = add_temp_directory(&app).await;

        // Create empty custom.
        let resp = app
            .clone()
            .oneshot(post_json(
                "/api/collections",
                serde_json::json!({ "name": "Empty" }),
            ))
            .await
            .unwrap();
        let cid = json_body(resp).await["id"].as_i64().unwrap();

        // Add directory.
        let resp = app
            .clone()
            .oneshot(post_json(
                &format!("/api/collections/{cid}/directories"),
                serde_json::json!({ "directory_id": dir_id }),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);

        // Remove directory.
        let resp = app
            .oneshot(delete(&format!(
                "/api/collections/{cid}/directories/{dir_id}"
            )))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    }

    #[tokio::test]
    async fn random_from_empty_collection_is_404() {
        let app = test_app().await;
        let resp = app
            .clone()
            .oneshot(post_json(
                "/api/collections",
                serde_json::json!({ "name": "E" }),
            ))
            .await
            .unwrap();
        let cid = json_body(resp).await["id"].as_i64().unwrap();

        let resp = app
            .oneshot(get(&format!("/api/collections/{cid}/random")))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }
}
