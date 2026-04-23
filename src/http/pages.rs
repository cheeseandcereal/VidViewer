//! HTML page handlers.

use askama::Template;
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{Html, IntoResponse, Response},
};

use crate::{
    collections::{self, Collection, CollectionSummary, VideoCard},
    directories, ids,
    state::AppState,
};

mod filters {
    /// Format an Option<f64> duration in seconds as `HH:MM:SS` or `MM:SS`.
    pub fn duration(d: &Option<f64>) -> askama::Result<String> {
        let Some(secs) = d else {
            return Ok(String::new());
        };
        let total = secs.max(0.0).round() as u64;
        let h = total / 3600;
        let m = (total % 3600) / 60;
        let s = total % 60;
        if h > 0 {
            Ok(format!("{h}:{m:02}:{s:02}"))
        } else {
            Ok(format!("{m}:{s:02}"))
        }
    }
}

#[derive(Template)]
#[template(path = "home.html")]
struct HomeTemplate {
    directory_collections: Vec<CollectionSummary>,
    custom_collections: Vec<CollectionSummary>,
}

pub async fn home(State(state): State<AppState>) -> Response {
    let summaries = match collections::list_summaries(&state.pool).await {
        Ok(v) => v,
        Err(err) => {
            tracing::error!(error = %err, "listing collections");
            Vec::new()
        }
    };
    let (directory_collections, custom_collections): (Vec<_>, Vec<_>) = summaries
        .into_iter()
        .partition(|s| s.coll.kind == collections::Kind::Directory);
    render(HomeTemplate {
        directory_collections,
        custom_collections,
    })
}

#[derive(Template)]
#[template(path = "collection.html")]
struct CollectionTemplate {
    collection: Collection,
    videos: Vec<VideoCard>,
}

pub async fn collection_page(State(state): State<AppState>, Path(id): Path<i64>) -> Response {
    let cid = ids::CollectionId(id);
    let Some(collection) = (match collections::get(&state.pool, cid).await {
        Ok(v) => v,
        Err(err) => {
            tracing::error!(error = %err, "get collection");
            return (StatusCode::INTERNAL_SERVER_ERROR, "internal error").into_response();
        }
    }) else {
        return (StatusCode::NOT_FOUND, "collection not found").into_response();
    };
    let videos = collections::videos_in(&state.pool, cid)
        .await
        .unwrap_or_default();
    render(CollectionTemplate { collection, videos })
}

#[derive(Template)]
#[template(path = "settings.html")]
struct SettingsTemplate {
    directories: Vec<directories::Directory>,
    port: u16,
    player: String,
    db_path: String,
    thumb_dir: String,
    preview_dir: String,
}

pub async fn settings(State(state): State<AppState>) -> Response {
    let dirs = match directories::list(&state.pool, false).await {
        Ok(v) => v,
        Err(err) => {
            tracing::error!(error = %err, "listing directories");
            Vec::new()
        }
    };
    render(SettingsTemplate {
        directories: dirs,
        port: state.config.port,
        player: state.config.player.clone(),
        db_path: crate::config::database_path().display().to_string(),
        thumb_dir: crate::config::thumb_cache_dir().display().to_string(),
        preview_dir: crate::config::preview_cache_dir().display().to_string(),
    })
}

fn render<T: Template>(t: T) -> Response {
    match t.render() {
        Ok(body) => Html(body).into_response(),
        Err(err) => {
            tracing::error!(error = %err, "template render failed");
            (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                "template render failed",
            )
                .into_response()
        }
    }
}
