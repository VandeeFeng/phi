use std::path::PathBuf;

use axum::{
    Router,
    body::Body,
    extract::State,
    http::{StatusCode, Uri, header},
    response::{IntoResponse, Response},
    routing::get,
};
pub(super) fn router(assets: PathBuf) -> Router {
    Router::new()
        .route("/", get(asset))
        .route("/{file}", get(asset))
        .with_state(assets)
}

async fn asset(State(assets): State<PathBuf>, uri: Uri) -> Response {
    let (file, content_type) = match uri.path() {
        "/" => ("index.html", "text/html; charset=utf-8"),
        "/app.js" => ("app.js", "text/javascript; charset=utf-8"),
        "/demo.js" => ("demo.js", "text/javascript; charset=utf-8"),
        "/styles.css" => ("styles.css", "text/css; charset=utf-8"),
        "/office.svg" => ("office.svg", "image/svg+xml"),
        "/collapse.svg" => ("collapse.svg", "image/svg+xml"),
        "/expand.svg" => ("expand.svg", "image/svg+xml"),
        _ => return StatusCode::NOT_FOUND.into_response(),
    };
    match tokio::fs::read(assets.join(file)).await {
        Ok(bytes) => {
            let body = if file == "index.html" {
                Body::from(
                    String::from_utf8_lossy(&bytes)
                        .replace("data-mode=\"demo\"", "data-mode=\"live\""),
                )
            } else {
                Body::from(bytes)
            };
            ([(header::CONTENT_TYPE, content_type)], body).into_response()
        }
        Err(_) => StatusCode::NOT_FOUND.into_response(),
    }
}
