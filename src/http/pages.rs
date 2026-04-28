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
    /// Only populated for custom collections; directory collections leave this empty.
    member_directories: Vec<collections::CollectionDirectory>,
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
    let member_directories = if matches!(collection.kind, collections::Kind::Custom) {
        collections::directories_in(&state.pool, cid)
            .await
            .unwrap_or_default()
    } else {
        Vec::new()
    };
    render(CollectionTemplate {
        collection,
        videos,
        member_directories,
    })
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
        db_path: state.config.database_path().display().to_string(),
        thumb_dir: state.config.thumb_cache_dir().display().to_string(),
        preview_dir: state.config.preview_cache_dir().display().to_string(),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::router;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::util::ServiceExt;

    async fn state() -> AppState {
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

    async fn fetch_html(app: axum::Router, uri: &str) -> (StatusCode, String) {
        let resp = app
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = resp.status();
        let body = axum::body::to_bytes(resp.into_body(), 1 << 20)
            .await
            .unwrap();
        (status, String::from_utf8(body.to_vec()).unwrap())
    }

    // ---------- filter + helper coverage ----------

    #[test]
    fn format_hms_handles_all_magnitudes() {
        assert_eq!(super::format_hms(0.0), "0:00");
        assert_eq!(super::format_hms(1.4), "0:01");
        assert_eq!(super::format_hms(59.5), "1:00");
        assert_eq!(super::format_hms(60.0), "1:00");
        assert_eq!(super::format_hms(3599.0), "59:59");
        assert_eq!(super::format_hms(3600.0), "1:00:00");
        assert_eq!(super::format_hms(3661.0), "1:01:01");
        // Negative clamps to zero.
        assert_eq!(super::format_hms(-5.0), "0:00");
    }

    #[test]
    fn duration_filter_for_some_and_none() {
        assert_eq!(super::filters::duration(&None).unwrap(), "");
        assert_eq!(super::filters::duration(&Some(125.0)).unwrap(), "2:05");
        assert_eq!(super::filters::duration(&Some(3600.0)).unwrap(), "1:00:00");
    }

    #[test]
    fn duration_secs_filter_format() {
        assert_eq!(super::filters::duration_secs(&30.0).unwrap(), "0:30");
        assert_eq!(super::filters::duration_secs(&0.0).unwrap(), "0:00");
    }

    #[test]
    fn progress_pct_handles_none_zero_and_clamps() {
        assert_eq!(super::filters::progress_pct(&10.0, &None).unwrap(), "0");
        assert_eq!(
            super::filters::progress_pct(&10.0, &Some(0.0)).unwrap(),
            "0"
        );
        assert_eq!(
            super::filters::progress_pct(&50.0, &Some(200.0)).unwrap(),
            "25.0"
        );
        // Over 100% clamps.
        assert_eq!(
            super::filters::progress_pct(&300.0, &Some(100.0)).unwrap(),
            "100.0"
        );
        // Negatives clamp to 0.
        assert_eq!(
            super::filters::progress_pct(&-5.0, &Some(100.0)).unwrap(),
            "0.0"
        );
    }

    #[test]
    fn humanize_bytes_chooses_sensible_unit() {
        assert_eq!(super::humanize_bytes(500), "500 bytes");
        assert_eq!(super::humanize_bytes(1536), "1.5 KiB");
        assert!(super::humanize_bytes(3 * 1024 * 1024).ends_with("MiB"));
        assert!(super::humanize_bytes(5 * 1024 * 1024 * 1024).ends_with("GiB"));
    }

    // ---------- page handler coverage ----------

    #[tokio::test]
    async fn home_page_renders_empty_library_placeholder() {
        let (status, body) = fetch_html(router(state().await), "/").await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("<!doctype html>"));
        assert!(body.contains("Your library is empty"));
    }

    #[tokio::test]
    async fn settings_page_renders() {
        let (status, body) = fetch_html(router(state().await), "/settings").await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("Settings"));
        assert!(body.contains("Add Directory"));
    }

    #[tokio::test]
    async fn history_page_renders_empty_state() {
        let (status, body) = fetch_html(router(state().await), "/history").await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("No watch history yet"));
    }

    #[tokio::test]
    async fn collection_page_renders_and_404s_on_unknown() {
        let st = state().await;
        // Seed a directory via the DB directly so we have a directory collection.
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().to_path_buf();
        std::mem::forget(tmp);
        let dir = crate::directories::add(&st.pool, &st.clock, &path, Some("MyLib".into()))
            .await
            .unwrap();

        let (status, body) = fetch_html(
            router(st.clone()),
            &format!("/collections/{}", dir.collection_id),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("MyLib"));

        let (status, _) = fetch_html(router(st), "/collections/99999").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn video_detail_page_renders_with_seeded_row_and_404s_on_missing() {
        let st = state().await;

        // Seed a directory and a video row that get_detail can resolve.
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().to_path_buf();
        std::mem::forget(tmp);
        let dir = crate::directories::add(&st.pool, &st.clock, &path, None)
            .await
            .unwrap();

        let vid = crate::ids::VideoId::new_random();
        let now_s = st.clock.now().to_rfc3339();
        sqlx::query(
            "INSERT INTO videos (id, directory_id, relative_path, filename, size_bytes, \
             mtime_unix, duration_secs, codec, width, height, thumbnail_ok, preview_ok, \
             missing, is_audio_only, attached_pic_stream_index, created_at, updated_at) \
             VALUES (?, ?, ?, ?, 2048, 1, 125.0, 'h264', 1920, 1080, 0, 0, 0, 0, NULL, ?, ?)",
        )
        .bind(vid.as_str())
        .bind(dir.id.raw())
        .bind("clip.mp4")
        .bind("clip.mp4")
        .bind(&now_s)
        .bind(&now_s)
        .execute(&st.pool)
        .await
        .unwrap();

        let (status, body) =
            fetch_html(router(st.clone()), &format!("/videos/{}", vid.as_str())).await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("clip.mp4"));
        assert!(body.contains("1920"));
        // Duration humanizer picked up.
        assert!(body.contains("2:05"));

        let (status, _) = fetch_html(router(st), "/videos/does-not-exist").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }
}
