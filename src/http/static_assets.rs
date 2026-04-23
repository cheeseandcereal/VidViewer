//! Static CSS/JS bundled into the binary via `rust-embed`.
//!
//! In debug builds, `rust-embed` reads files from disk on each request so
//! editing `static/*` reloads without a rebuild. In release builds the file
//! bytes are embedded into the binary at compile time — the binary is
//! portable and doesn't need `static/` sitting next to it at runtime.

use axum::{
    body::Body,
    extract::Path,
    http::{header, StatusCode},
    response::Response,
};
use rust_embed::Embed;

#[derive(Embed)]
#[folder = "static/"]
struct StaticAssets;

pub async fn serve(Path(path): Path<String>) -> Response<Body> {
    let mime = mime_guess::from_path(&path).first_or_octet_stream();
    match StaticAssets::get(&path) {
        Some(file) => Response::builder()
            .header(header::CONTENT_TYPE, mime.as_ref())
            .body(Body::from(file.data.into_owned()))
            .unwrap(),
        None => Response::builder()
            .status(StatusCode::NOT_FOUND)
            .body(Body::empty())
            .unwrap(),
    }
}

/// Serve the site favicon at `/favicon.ico`. We don't ship an ICO file —
/// some browsers request this path implicitly regardless of the `<link>`
/// tag, so we respond with the same embedded SVG and the correct MIME type.
pub async fn favicon() -> Response<Body> {
    match StaticAssets::get("favicon.svg") {
        Some(file) => Response::builder()
            .header(header::CONTENT_TYPE, "image/svg+xml")
            .body(Body::from(file.data.into_owned()))
            .unwrap(),
        None => Response::builder()
            .status(StatusCode::NOT_FOUND)
            .body(Body::empty())
            .unwrap(),
    }
}
