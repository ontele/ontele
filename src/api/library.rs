// Copyright 2026 The Ontele Authors
// SPDX-License-Identifier: Apache-2.0

use crate::{
    auth::{AdminUser, CurrentUser},
    db,
    error::{AppError, AppResult},
    media::art::ArtKind,
    model::{Item, Kind},
    state::AppState,
};
use axum::{
    Json,
    extract::{Path, Query, Request, State},
    http::{StatusCode, header},
    response::{IntoResponse, Response},
};
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;

fn cards(v: Vec<Item>) -> Vec<Item> {
    v.into_iter().map(Item::card).collect()
}

pub async fn home(State(st): State<Arc<AppState>>, CurrentUser(u): CurrentUser) -> AppResult<Json<serde_json::Value>> {
    let h = db::items::home(&st.pool, u.id, 20).await?;
    let up_next = db::items::up_next(&st.pool, u.id, 20).await?;
    let albums = db::music::recent_albums(&st.pool, 20).await?;
    Ok(Json(json!({
        "continue": cards(h.cont),
        "upNext": cards(up_next),
        "recordings": cards(h.recordings),
        "movies": cards(h.movies),
        "episodes": cards(h.episodes),
        "albums": albums,
    })))
}

#[derive(Deserialize, Default)]
pub struct ListQuery {
    #[serde(default)]
    pub sort: String,
    pub tag: Option<String>,
    pub genre: Option<String>,
    pub q: Option<String>,
    #[serde(default)]
    pub unwatched: bool,
    pub limit: Option<i64>,
}

pub async fn movies(
    State(st): State<Arc<AppState>>,
    CurrentUser(u): CurrentUser,
    Query(q): Query<ListQuery>,
) -> AppResult<Json<Vec<Item>>> {
    let f = db::items::ListFilter { tag: q.tag, genre: q.genre, q: q.q, unwatched: q.unwatched };
    let sort = db::items::Sort::parse(&q.sort);
    let items =
        db::items::list_kind(&st.pool, u.id, Kind::Movie, sort, &f, q.limit.unwrap_or(5000).clamp(1, 20000)).await?;
    Ok(Json(cards(items)))
}

pub async fn genres(
    State(st): State<Arc<AppState>>,
    CurrentUser(_u): CurrentUser,
) -> AppResult<Json<serde_json::Value>> {
    let movies = db::items::genres(&st.pool, Kind::Movie).await?;
    let episodes = db::items::genres(&st.pool, Kind::Episode).await?;
    let to = |v: Vec<(String, i64)>| v.into_iter().map(|(g, n)| json!({"name": g, "count": n})).collect::<Vec<_>>();
    Ok(Json(json!({ "movies": to(movies), "shows": to(episodes) })))
}

pub async fn shows(
    State(st): State<Arc<AppState>>,
    CurrentUser(u): CurrentUser,
) -> AppResult<Json<Vec<crate::model::ShowSummary>>> {
    Ok(Json(db::items::shows(&st.pool, u.id).await?))
}

pub async fn show(
    State(st): State<Arc<AppState>>,
    CurrentUser(u): CurrentUser,
    Path(show): Path<String>,
) -> AppResult<Json<serde_json::Value>> {
    let eps = db::items::show_episodes(&st.pool, u.id, &show).await?;
    if eps.is_empty() {
        return Err(AppError::not_found("unknown show"));
    }
    let name = eps[0].show.clone().unwrap_or(show.clone());
    let meta = db::items::get_show_meta(&st.pool, &name).await?;
    let mut seasons: Vec<serde_json::Value> = vec![];
    let mut cur: Option<(i32, Vec<Item>)> = None;
    for e in eps {
        let s = e.season.unwrap_or(0);
        match cur.as_mut() {
            Some((cs, list)) if *cs == s => list.push(e.card()),
            _ => {
                if let Some((cs, list)) = cur.take() {
                    seasons.push(json!({ "season": cs, "episodes": list }));
                }
                cur = Some((s, vec![e.card()]));
            }
        }
    }
    if let Some((cs, list)) = cur.take() {
        seasons.push(json!({ "season": cs, "episodes": list }));
    }
    Ok(Json(json!({ "show": name, "meta": meta, "seasons": seasons })))
}

pub async fn item(
    State(st): State<Arc<AppState>>,
    CurrentUser(u): CurrentUser,
    Path(id): Path<String>,
) -> AppResult<Json<Item>> {
    let mut it = db::items::get(&st.pool, u.id, &id).await?.ok_or_else(|| AppError::not_found("unknown item"))?;
    if it.kind == Some(Kind::Episode) {
        if let (Some(show), Some(s), Some(e)) = (it.show.as_deref(), it.season, it.episode) {
            it.next_episode = db::items::next_episode(&st.pool, u.id, show, s, e).await?.map(|n| Box::new(n.card()));
        }
        // inherit show-level backdrop / genres / rating for the detail page
        let show_meta = match it.show.as_deref() {
            Some(show) => db::items::get_show_meta(&st.pool, show).await?,
            None => None,
        };
        if let Some(sm) = show_meta {
            let mut m = it.meta.take().unwrap_or_default();
            if m.backdrop_url.is_none() {
                m.backdrop_url = sm.backdrop_url;
            }
            if m.genres.is_empty() {
                m.genres = sm.genres;
            }
            if m.content_rating.is_none() {
                m.content_rating = sm.content_rating;
            }
            if m.overview.is_none() {
                m.overview = it.description.clone();
            }
            it.meta = Some(m);
        }
    }
    // never leak filesystem paths to the browser (sidecar subtitles carry
    // their absolute path in `info`; keep just the file name)
    it.path = None;
    if let Some(info) = it.info.as_mut() {
        for s in &mut info.subtitles {
            if let Some(ext) = s.external.take() {
                s.external = Some(
                    std::path::Path::new(&ext).file_name().map(|f| f.to_string_lossy().to_string()).unwrap_or_default(),
                );
            }
        }
    }
    Ok(Json(it))
}

pub async fn delete_item(
    State(st): State<Arc<AppState>>,
    AdminUser(u): AdminUser,
    Path(id): Path<String>,
) -> AppResult<Json<serde_json::Value>> {
    let path = db::items::delete(&st.pool, &id).await?;
    st.art.invalidate(&id);
    // the path goes to the log only: activity detail is shown to every user
    tracing::info!(item = %id, path = path.as_deref().unwrap_or(""), "item deleted");
    st.activity.record(Some(u.id), "item.delete", Some(&id), json!({ "existed": path.is_some() }));
    Ok(Json(json!({ "ok": true })))
}

#[derive(Deserialize)]
pub struct SearchQuery {
    #[serde(default)]
    pub q: String,
}

pub async fn search(
    State(st): State<Arc<AppState>>,
    CurrentUser(u): CurrentUser,
    Query(q): Query<SearchQuery>,
) -> AppResult<Json<serde_json::Value>> {
    let query = q.q.trim();
    if query.is_empty() {
        return Ok(Json(
            json!({ "movies": [], "episodes": [], "shows": [], "albums": [], "tracks": [], "artists": [], "channels": [], "recordings": [], "airings": [] }),
        ));
    }
    let hits = db::items::search(&st.pool, u.id, query, 12).await?;
    let albums = db::music::albums(&st.pool, None, Some(query), "title", 12).await?;
    let artists = db::music::artists(&st.pool, Some(query)).await?;
    let ql = query.to_lowercase();
    let shows: Vec<_> = db::items::shows(&st.pool, u.id)
        .await?
        .into_iter()
        .filter(|s| s.show.to_lowercase().contains(&ql))
        .take(12)
        .collect();
    let channels: Vec<_> = st
        .hdhr
        .channels()
        .into_iter()
        .filter(|c| c.guide_name.to_lowercase().contains(&ql) || c.guide_number.starts_with(query))
        .take(12)
        .collect();
    let airings = st.guide.search(query, chrono::Utc::now(), 12);
    Ok(Json(json!({
        "movies": cards(hits.movies), "episodes": cards(hits.episodes), "shows": shows,
        "albums": albums, "tracks": cards(hits.tracks), "artists": artists.into_iter().take(12).collect::<Vec<_>>(),
        "channels": channels, "recordings": cards(hits.recordings), "airings": airings,
    })))
}

pub async fn scan(
    State(st): State<Arc<AppState>>,
    AdminUser(u): AdminUser,
) -> AppResult<(StatusCode, Json<serde_json::Value>)> {
    let sc = st.scanner.clone();
    st.activity.record(Some(u.id), "scan.start", None, json!({}));
    tokio::spawn(async move {
        if let Err(e) = sc.scan().await {
            tracing::warn!(error = %e, "scan failed");
        }
    });
    Ok((StatusCode::ACCEPTED, Json(json!({ "status": "scanning" }))))
}

pub async fn scan_status(
    State(st): State<Arc<AppState>>,
    CurrentUser(_u): CurrentUser,
) -> Json<crate::model::ScanStatus> {
    Json(st.scanner.status())
}

#[derive(Deserialize)]
pub struct ImgQuery {
    #[serde(rename = "type", default)]
    pub kind: String,
    pub w: Option<u32>,
}

pub async fn img(
    State(st): State<Arc<AppState>>,
    CurrentUser(_u): CurrentUser,
    Path(id): Path<String>,
    Query(q): Query<ImgQuery>,
    req: Request,
) -> Response {
    let kind = ArtKind::parse(&q.kind);
    let width = q.w.filter(|w| (64..=2560).contains(w));
    match st.art.path(&id, kind, width).await {
        Ok(p) => {
            let mut res = crate::stream::direct::serve_file(&p, "image/jpeg", req).await;
            res.headers_mut().insert(header::CACHE_CONTROL, header::HeaderValue::from_static("public, max-age=86400"));
            res
        }
        Err(e) => {
            tracing::debug!(id, error = %e, "artwork unavailable");
            (StatusCode::NOT_FOUND, Json(json!({ "error": "no artwork" }))).into_response()
        }
    }
}

pub async fn sprites_vtt(
    State(st): State<Arc<AppState>>,
    CurrentUser(_u): CurrentUser,
    Path(id): Path<String>,
    req: Request,
) -> Response {
    if !st.settings.get().thumbnails {
        return StatusCode::NOT_FOUND.into_response();
    }
    match st.art.sprites(&id).await {
        Ok((vtt, _)) => crate::stream::direct::serve_file(&vtt, "text/vtt", req).await,
        Err(e) => {
            tracing::debug!(id, error = %e, "sprites unavailable");
            (StatusCode::NOT_FOUND, Json(json!({ "error": "no thumbnails" }))).into_response()
        }
    }
}

pub async fn sprites_jpg(
    State(st): State<Arc<AppState>>,
    CurrentUser(_u): CurrentUser,
    Path(id): Path<String>,
    req: Request,
) -> Response {
    match st.art.sprites(&id).await {
        Ok((_, jpg)) => {
            let mut res = crate::stream::direct::serve_file(&jpg, "image/jpeg", req).await;
            res.headers_mut().insert(header::CACHE_CONTROL, header::HeaderValue::from_static("public, max-age=86400"));
            res
        }
        Err(e) => {
            tracing::debug!(id, error = %e, "sprites unavailable");
            (StatusCode::NOT_FOUND, Json(json!({ "error": "no thumbnails" }))).into_response()
        }
    }
}

pub async fn subtitles(
    State(st): State<Arc<AppState>>,
    CurrentUser(_u): CurrentUser,
    Path(id): Path<String>,
) -> AppResult<Json<serde_json::Value>> {
    let (_, _, info) = db::items::path_of(&st.pool, &id).await?.ok_or_else(|| AppError::not_found("unknown item"))?;
    let list: Vec<_> = crate::stream::subtitles::list(&info.0)
        .into_iter()
        .enumerate()
        .map(|(i, s)| {
            json!({
                "index": i, "streamIndex": s.index, "lang": s.lang, "title": s.title, "codec": s.codec,
                "forced": s.forced, "text": s.text, "external": s.external.is_some(),
                "url": if s.text { Some(format!("/api/items/{id}/subtitles/{i}.vtt")) } else { None },
            })
        })
        .collect();
    Ok(Json(json!(list)))
}

pub async fn subtitle_vtt(
    State(st): State<Arc<AppState>>,
    CurrentUser(_u): CurrentUser,
    Path((id, idx)): Path<(String, String)>,
    req: Request,
) -> Response {
    let idx: usize = match idx.trim_end_matches(".vtt").parse() {
        Ok(i) => i,
        Err(_) => return StatusCode::BAD_REQUEST.into_response(),
    };
    let Ok(Some((_, path, info))) = db::items::path_of(&st.pool, &id).await else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let subs = crate::stream::subtitles::list(&info.0);
    let Some(s) = subs.get(idx) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if !s.text {
        return (StatusCode::BAD_REQUEST, "bitmap subtitle: use burn-in").into_response();
    }
    let set = st.settings.get();
    let cache = st.cfg.data_dir.join("subs");
    let key = format!("{id}-{idx}");
    let media = std::path::PathBuf::from(&path);
    let external = s.external.as_ref().map(std::path::PathBuf::from);
    let stream_index = if external.is_some() { None } else { Some(s.index) };
    match crate::stream::subtitles::to_vtt(&set.ffmpeg_path, &media, stream_index, external.as_deref(), &cache, &key)
        .await
    {
        Ok(p) => crate::stream::direct::serve_file(&p, "text/vtt; charset=utf-8", req).await,
        Err(e) => {
            tracing::error!(id, idx, error = %e, "subtitle conversion failed");
            (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": "subtitle conversion failed" }))).into_response()
        }
    }
}

#[derive(Deserialize)]
pub struct WatchBody {
    #[serde(default)]
    pub pos: f64,
    #[serde(default)]
    pub dur: f64,
    #[serde(default)]
    pub done: bool,
}

pub async fn watch(
    State(st): State<Arc<AppState>>,
    CurrentUser(u): CurrentUser,
    Path(id): Path<String>,
    Json(b): Json<WatchBody>,
) -> AppResult<Json<serde_json::Value>> {
    let done = b.done || (b.dur > 0.0 && b.pos / b.dur > 0.95);
    let old = db::watch::set(&st.pool, u.id, &id, b.pos, b.dur, done).await?;
    // Credit watched time to the play log (fire-and-forget; capped in db::trending)
    let delta = b.pos - old.unwrap_or(b.pos);
    if delta > 0.0 {
        let (pool, uid, iid) = (st.pool.clone(), u.id, id.clone());
        tokio::spawn(async move { db::trending::accumulate(&pool, uid, &iid, delta).await.ok() });
    }
    if done {
        st.activity.record(Some(u.id), "watch.done", Some(&id), json!({ "dur": b.dur }));
    }
    Ok(Json(json!({ "ok": true, "done": done })))
}

pub async fn unwatch(
    State(st): State<Arc<AppState>>,
    CurrentUser(u): CurrentUser,
    Path(id): Path<String>,
) -> AppResult<Json<serde_json::Value>> {
    db::watch::clear(&st.pool, u.id, &id).await?;
    Ok(Json(json!({ "ok": true })))
}

pub async fn refresh_metadata(
    State(st): State<Arc<AppState>>,
    CurrentUser(u): CurrentUser,
    Path(id): Path<String>,
) -> AppResult<Json<Item>> {
    if let Some(show) = id.strip_prefix("show:").map(str::to_string) {
        st.metadata.enrich_show(&show).await?;
        st.art.invalidate(&id);
        let m = db::items::get_show_meta(&st.pool, &show).await?;
        return Ok(Json(Item { id, title: show, meta: m, ..Default::default() }));
    }
    st.metadata.enrich_item(&id).await?;
    st.art.invalidate(&id);
    st.activity.record(Some(u.id), "metadata.refresh", Some(&id), json!({}));
    let it = db::items::get(&st.pool, u.id, &id).await?.ok_or_else(|| AppError::not_found("unknown item"))?;
    Ok(Json(it.card()))
}

#[derive(Deserialize)]
pub struct MetaBody {
    pub title: Option<String>,
    pub year: Option<i32>,
    pub tmdb_id: Option<i64>,
    #[serde(rename = "tmdbId")]
    pub tmdb_id_camel: Option<i64>,
}

pub async fn set_metadata(
    State(st): State<Arc<AppState>>,
    AdminUser(u): AdminUser,
    Path(id): Path<String>,
    Json(b): Json<MetaBody>,
) -> AppResult<Json<Item>> {
    let tmdb = b.tmdb_id.or(b.tmdb_id_camel);
    db::items::set_identity(&st.pool, &id, b.title.as_deref().map(str::trim).filter(|s| !s.is_empty()), b.year, tmdb)
        .await?;
    let _ = st.metadata.enrich_item(&id).await;
    st.art.invalidate(&id);
    st.activity.record(
        Some(u.id),
        "metadata.fix",
        Some(&id),
        json!({ "title": b.title, "year": b.year, "tmdbId": tmdb }),
    );
    let it = db::items::get(&st.pool, u.id, &id).await?.ok_or_else(|| AppError::not_found("unknown item"))?;
    Ok(Json(it.card()))
}
