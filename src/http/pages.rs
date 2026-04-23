//! HTML page handlers.

use askama::Template;
use axum::response::{Html, IntoResponse, Response};

use crate::{directories, state::AppState};

#[derive(Template)]
#[template(path = "home.html")]
struct HomeTemplate {}

pub async fn home(_state: axum::extract::State<AppState>) -> Response {
    render(HomeTemplate {})
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

pub async fn settings(axum::extract::State(state): axum::extract::State<AppState>) -> Response {
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
