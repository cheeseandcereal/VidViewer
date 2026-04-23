//! HTML page handlers.

use askama::Template;
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::{Html, IntoResponse, Response},
};
use serde::Deserialize;

use crate::{
    collections::{self, Collection, CollectionSummary, VideoCard},
    directories,
    history::{self, HistoryEntry},
    ids,
    state::AppState,
    videos::{self, DetailCollection, Video, WatchHistoryRow},
};

mod filters {
    /// Format an Option<f64> duration in seconds as `HH:MM:SS` or `MM:SS`.
    pub fn duration(d: &Option<f64>) -> askama::Result<String> {
        let Some(secs) = d else {
            return Ok(String::new());
        };
        Ok(format_hms(*secs))
    }

    /// Format a plain f64 number of seconds.
    pub fn duration_secs(d: &f64) -> askama::Result<String> {
        Ok(format_hms(*d))
    }

    /// Progress percentage given (position, duration).
    pub fn progress_pct(position: &f64, duration: &Option<f64>) -> askama::Result<String> {
        let Some(d) = duration else {
            return Ok("0".to_string());
        };
        if *d <= 0.0 {
            return Ok("0".to_string());
        }
        let pct = (position / d * 100.0).clamp(0.0, 100.0);
        Ok(format!("{pct:.1}"))
    }

    fn format_hms(secs: f64) -> String {
        let total = secs.max(0.0).round() as u64;
        let h = total / 3600;
        let m = (total % 3600) / 60;
        let s = total % 60;
        if h > 0 {
            format!("{h}:{m:02}:{s:02}")
        } else {
            format!("{m}:{s:02}")
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

#[derive(Template)]
#[template(path = "history.html")]
struct HistoryTemplate {
    entries: Vec<HistoryEntry>,
}

pub async fn history_page(State(state): State<AppState>) -> Response {
    let entries = history::list(&state.pool).await.unwrap_or_default();
    render(HistoryTemplate { entries })
}

#[derive(Debug, Deserialize)]
pub struct DetailQuery {
    pub cid: Option<i64>,
}

#[derive(Template)]
#[template(path = "video_detail.html")]
struct VideoDetailTemplate {
    video: Video,
    title: String,
    directory_label: String,
    size_label: String,
    history: Option<WatchHistoryRow>,
    has_resume: bool,
    resume_pretty: String,
    last_watched_pretty: String,
    collections: Vec<DetailCollection>,
    updated_at_epoch: i64,
    from_cid: Option<String>,
}

pub async fn video_detail_page(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(q): Query<DetailQuery>,
) -> Response {
    let vid = ids::VideoId(id);
    let Some(detail) = (match videos::get_detail(&state.pool, &vid).await {
        Ok(v) => v,
        Err(err) => {
            tracing::error!(error = %err, "video detail");
            return (StatusCode::INTERNAL_SERVER_ERROR, "internal error").into_response();
        }
    }) else {
        return (StatusCode::NOT_FOUND, "video not found").into_response();
    };

    let title = match detail.video.filename.rsplit_once('.') {
        Some((stem, _)) if !stem.is_empty() => stem.to_string(),
        _ => detail.video.filename.clone(),
    };
    let size_label = humanize_bytes(detail.video.size_bytes);
    let has_resume = detail
        .history
        .as_ref()
        .map(|h| !h.completed && h.position_secs > 0.5)
        .unwrap_or(false);
    let resume_pretty = detail
        .history
        .as_ref()
        .map(|h| format_hms(h.position_secs))
        .unwrap_or_default();
    let last_watched_pretty = detail
        .history
        .as_ref()
        .map(|h| {
            h.last_watched_at
                .format("%Y-%m-%d %H:%M:%S UTC")
                .to_string()
        })
        .unwrap_or_default();
    let updated_at_epoch = detail.video.updated_at.timestamp();

    render(VideoDetailTemplate {
        video: detail.video,
        title,
        directory_label: detail.directory_label,
        size_label,
        history: detail.history,
        has_resume,
        resume_pretty,
        last_watched_pretty,
        collections: detail.collections,
        updated_at_epoch,
        from_cid: q.cid.map(|n| n.to_string()),
    })
}

fn humanize_bytes(bytes: i64) -> String {
    let b = bytes as f64;
    let kib = 1024.0_f64;
    let mib = kib * 1024.0;
    let gib = mib * 1024.0;
    if b >= gib {
        format!("{:.2} GiB", b / gib)
    } else if b >= mib {
        format!("{:.2} MiB", b / mib)
    } else if b >= kib {
        format!("{:.1} KiB", b / kib)
    } else {
        format!("{bytes} bytes")
    }
}

fn format_hms(secs: f64) -> String {
    let total = secs.max(0.0).round() as u64;
    let h = total / 3600;
    let m = (total % 3600) / 60;
    let s = total % 60;
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
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
