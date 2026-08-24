// Copyright 2026 The Ontele Authors
// SPDX-License-Identifier: Apache-2.0

//! Embedded SPA. Assets ship inside the binary (rust-embed); responses carry
//! strong ETags so the browser revalidates cheaply, vendor files get a long
//! immutable cache, and unknown paths fall back to `index.html` for the
//! hash router.

use axum::{
    Router,
    body::Body,
    extract::Request,
    http::{HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
    routing::get,
};
use rust_embed::RustEmbed;

#[derive(RustEmbed)]
#[folder = "ui/"]
#[exclude = "*.md"]
struct Assets;

pub fn router<S: Clone + Send + Sync + 'static>() -> Router<S> {
    Router::new().fallback(get(serve))
}

fn etag_for(hash: &[u8]) -> HeaderValue {
    HeaderValue::from_str(&format!("\"{}\"", hex::encode(&hash[..16]))).unwrap()
}

async fn serve(req: Request) -> Response {
    let path = req.uri().path().trim_start_matches('/');
    let path = if path.is_empty() { "index.html" } else { path };
    let (file, name) = match Assets::get(path) {
        Some(f) => (f, path.to_string()),
        None => match Assets::get("index.html") {
            Some(f) => (f, "index.html".to_string()),
            None => return StatusCode::NOT_FOUND.into_response(),
        },
    };
    let etag = etag_for(&file.metadata.sha256_hash());
    if let Some(inm) = req.headers().get(header::IF_NONE_MATCH)
        && inm == etag
    {
        return Response::builder()
            .status(StatusCode::NOT_MODIFIED)
            .header(header::ETAG, etag)
            .body(Body::empty())
            .unwrap();
    }
    let mime = mime_guess::from_path(&name).first_or_octet_stream();
    let cache = if name.starts_with("vendor/") { "public, max-age=31536000, immutable" } else { "no-cache" };
    let mut res = Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, mime.as_ref())
        .header(header::ETAG, etag)
        .header(header::CACHE_CONTROL, cache)
        .header("X-Content-Type-Options", "nosniff");
    if name == "index.html" {
        res = res.header("Referrer-Policy", "same-origin");
    }
    res.body(Body::from(file.data.into_owned())).unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn index_is_embedded() {
        assert!(Assets::get("index.html").is_some());
        assert!(Assets::get("vendor/hls.min.js").is_some());
    }
}
