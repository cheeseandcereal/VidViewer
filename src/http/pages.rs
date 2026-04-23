//! HTML page handlers.

use askama::Template;
use axum::response::{Html, IntoResponse, Response};

use crate::state::AppState;

#[derive(Template)]
#[template(path = "home.html")]
struct HomeTemplate {}

pub async fn home(_state: axum::extract::State<AppState>) -> Response {
    render(HomeTemplate {})
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
