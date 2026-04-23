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
