// Copyright 2026 The Ontele Authors
// SPDX-License-Identifier: Apache-2.0

//! REST surface + streaming routes + embedded UI. JSON in/out, one error
//! envelope, identity middleware on everything except health/metrics.

pub mod dvr;
pub mod library;
pub mod livetv;
pub mod music;
pub mod settings;
pub mod stream;
pub mod tags;
pub mod trending;

use crate::{auth, state::AppState};
use axum::{
    Router,
    body::Body,
    extract::{MatchedPath, Request, State},
    http::{HeaderValue, StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{delete, get, post, put},
};
use std::{sync::Arc, time::Instant};
use tower_http::{
    compression::{
        CompressionLayer,
        predicate::{NotForContentType, Predicate, SizeAbove},
    },
    set_header::SetResponseHeaderLayer,
};

pub fn router(state: Arc<AppState>) -> Router {
    let api = Router::new()
        // ---- identity / meta ----
        .route("/me", get(settings::me))
        .route("/stats", get(settings::stats))
        .route("/health", get(settings::health))
        .route("/trending", get(trending::trending))
        .route("/activity", get(settings::activity))
        .route("/users", get(settings::users))
        .route("/users/{id}/admin", put(settings::set_admin))
        // ---- library ----
        .route("/home", get(library::home))
        .route("/movies", get(library::movies))
        .route("/genres", get(library::genres))
        .route("/shows", get(library::shows))
        .route("/shows/{show}", get(library::show))
        .route("/items/{id}", get(library::item).delete(library::delete_item))
        .route("/items/{id}/subtitles", get(library::subtitles))
        .route("/items/{id}/subtitles/{idx}", get(library::subtitle_vtt))
        .route("/items/{id}/sprites.vtt", get(library::sprites_vtt))
        .route("/items/{id}/sprites.jpg", get(library::sprites_jpg))
        .route("/items/{id}/refresh-metadata", post(library::refresh_metadata))
        .route("/items/{id}/metadata", put(library::set_metadata))
        .route("/items/{id}/tags", post(tags::add))
        .route("/items/{id}/tags/{tag}", delete(tags::remove))
        .route("/tags", get(tags::list))
        .route("/search", get(library::search))
        .route("/scan", post(library::scan))
        .route("/scan/status", get(library::scan_status))
        .route("/img/{id}", get(library::img))
        .route("/watch/{id}", post(library::watch).delete(library::unwatch))
        // ---- music ----
        .route("/music/artists", get(music::artists))
        .route("/music/artists/{name}", get(music::artist))
        .route("/music/albums", get(music::albums))
        .route("/music/albums/{id}", get(music::album))
        .route("/music/tracks", get(music::tracks))
        // ---- live tv ----
        .route("/livetv/channels", get(livetv::channels))
        .route("/livetv/refresh", post(livetv::refresh))
        .route("/livetv/icon/{num}", get(livetv::icon))
        .route("/guide", get(livetv::guide))
        // ---- dvr ----
        .route("/dvr/recordings", get(dvr::recordings))
        .route("/dvr/record", post(dvr::record))
        .route("/dvr/recordings/{id}", delete(dvr::delete_recording))
        .route("/dvr/recordings/{id}/adscan", post(dvr::adscan))
        .route("/dvr/rules", get(dvr::rules).post(dvr::add_rule))
        .route("/dvr/rules/{id}", delete(dvr::delete_rule))
        // ---- settings ----
        .route("/settings", get(settings::get_settings).put(settings::put_settings))
        .route("/settings/probe", get(settings::probe))
        // ---- playback control ----
        .route("/stream/start", post(stream::start))
        .route("/stream/{sid}", delete(stream::stop))
        .route("/stream/{sid}/keepalive", post(stream::keepalive))
        .layer(SetResponseHeaderLayer::overriding(header::CACHE_CONTROL, HeaderValue::from_static("no-store")))
        .layer(CompressionLayer::new().compress_when(
            SizeAbove::new(512).and(NotForContentType::new("image/")).and(NotForContentType::new("video/")),
        ));

    let media = Router::new()
        .route("/direct/{id}", get(stream::direct))
        .route("/audio/{id}", get(stream::audio))
        .route("/hls/{sid}/{file}", get(stream::hls));

    let authed = Router::new()
        .nest("/api", api)
        .nest("/stream", media)
        .layer(middleware::from_fn_with_state(state.clone(), auth::require_user));

    let ui = crate::web::router().layer(CompressionLayer::new().compress_when(
        SizeAbove::new(1024).and(NotForContentType::new("image/")).and(NotForContentType::new("font/")),
    ));

    Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/readyz", get(readyz))
        .route("/metrics", get(metrics))
        .merge(authed)
        .merge(ui)
        .layer(middleware::from_fn(request_log))
        .layer(axum::extract::DefaultBodyLimit::max(4 * 1024 * 1024))
        .with_state(state)
}

async fn readyz(State(st): State<Arc<AppState>>) -> Response {
    match sqlx::query("SELECT 1").execute(&st.pool).await {
        Ok(_) => (StatusCode::OK, "ready").into_response(),
        Err(e) => (StatusCode::SERVICE_UNAVAILABLE, format!("db: {e}")).into_response(),
    }
}

async fn metrics(State(st): State<Arc<AppState>>) -> Response {
    let (sessions, transcodes) = st.streams.active();
    metrics::gauge!("ontele_streams_active", "mode" => "all").set(sessions as f64);
    metrics::gauge!("ontele_transcodes_active").set(transcodes as f64);
    metrics::gauge!("ontele_recordings_active").set(st.dvr.active_count() as f64);
    Response::builder()
        .header(header::CONTENT_TYPE, "text/plain; version=0.0.4; charset=utf-8")
        .body(Body::from(st.metrics.render()))
        .unwrap()
}

/// One log line per request (`target=ontele.http`) + Prometheus counters,
/// labelled by the matched route template so cardinality stays bounded.
async fn request_log(req: Request, next: Next) -> Response {
    let start = Instant::now();
    let method = req.method().clone();
    let route = req
        .extensions()
        .get::<MatchedPath>()
        .map(|m| m.as_str().to_string())
        .unwrap_or_else(|| if req.uri().path().starts_with("/api") { "/api/*".into() } else { "/ui".into() });
    let path = req.uri().path().to_string();
    let ip = req
        .extensions()
        .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
        .map(|c| c.0.ip().to_string())
        .or_else(|| {
            req.headers()
                .get("x-forwarded-for")
                .and_then(|v| v.to_str().ok())
                .map(|s| s.split(',').next().unwrap_or("").trim().to_string())
        })
        .unwrap_or_default();
    let res = next.run(req).await;
    crate::health::HTTP_REQUESTS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    // Best-effort egress estimate for the Health graph: Content-Length at
    // header time. Compressed/streamed bodies (no length) count zero and an
    // aborted download counts in full — good enough for a trend line.
    if method != axum::http::Method::HEAD
        && let Some(len) = res
            .headers()
            .get(axum::http::header::CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<u64>().ok())
    {
        crate::health::HTTP_BYTES_OUT.fetch_add(len, std::sync::atomic::Ordering::Relaxed);
    }
    let status = res.status().as_u16();
    let user = res.extensions().get::<Arc<crate::model::User>>().map(|u| u.display().to_string());
    let secs = start.elapsed().as_secs_f64();
    metrics::counter!("ontele_http_requests_total", "method" => method.to_string(), "route" => route.clone(), "status" => status.to_string()).increment(1);
    metrics::histogram!("ontele_http_request_duration_seconds", "route" => route.clone()).record(secs);
    if !(path == "/metrics" || path == "/healthz" || path == "/readyz") {
        if status >= 500 {
            tracing::error!(target: "ontele.http", %method, path, route, status, ms = (secs * 1000.0) as u64, ip, user, "request");
        } else {
            tracing::info!(target: "ontele.http", %method, path, route, status, ms = (secs * 1000.0) as u64, ip, user, "request");
        }
    }
    res
}
