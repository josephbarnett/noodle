//! Embedded UI assets.
//!
//! The React app lives in `crates/noodle-viewer/web/`. After
//! `npm run build`, its `dist/` directory is embedded into the binary
//! via `rust-embed`. In dev builds where `dist/` doesn't exist, we
//! serve a one-page placeholder telling the operator to run
//! `make viewer-build`.

use std::sync::Arc;

use axum::{
    Router,
    extract::Path,
    http::{StatusCode, header},
    response::{Html, IntoResponse, Response},
    routing::get,
};
use rust_embed::Embed;

use crate::hub::HubService;

/// Embedded `dist/` artifacts. Built by `npm run build` in
/// `crates/noodle-viewer/web/`. The folder MUST exist at compile
/// time; we ship a placeholder `dist/.gitkeep` so the path exists.
#[derive(Embed)]
#[folder = "$CARGO_MANIFEST_DIR/web/dist"]
struct Assets;

const PLACEHOLDER_HTML: &str = include_str!("../../assets/placeholder.html");

pub fn router(_hub: Arc<HubService>) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/{*path}", get(asset))
}

async fn index() -> Response {
    match Assets::get("index.html") {
        Some(file) => respond_file("index.html", file.data.as_ref()),
        None => Html(PLACEHOLDER_HTML).into_response(),
    }
}

async fn asset(Path(path): Path<String>) -> Response {
    // SPA-style: unknown paths fall back to index.html so client-side
    // routes work after a refresh.
    if let Some(file) = Assets::get(&path) {
        return respond_file(&path, file.data.as_ref());
    }
    match Assets::get("index.html") {
        Some(file) => respond_file("index.html", file.data.as_ref()),
        None => (StatusCode::NOT_FOUND, Html(PLACEHOLDER_HTML)).into_response(),
    }
}

fn respond_file(path: &str, bytes: &[u8]) -> Response {
    let mime = mime_guess::from_path(path).first_or_octet_stream();
    (
        [(header::CONTENT_TYPE, mime.as_ref().to_owned())],
        bytes.to_owned(),
    )
        .into_response()
}
