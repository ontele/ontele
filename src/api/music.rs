// Copyright 2026 The Ontele Authors
// SPDX-License-Identifier: Apache-2.0

use crate::{
    auth::CurrentUser,
    db,
    error::{AppError, AppResult},
    model::{AlbumSummary, ArtistSummary, Item},
    state::AppState,
};
use axum::{
    Json,
    extract::{Path, Query, State},
};
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;

#[derive(Deserialize, Default)]
pub struct MusicQuery {
    pub artist: Option<String>,
    pub album: Option<String>,
    pub q: Option<String>,
    #[serde(default)]
    pub sort: String,
    pub limit: Option<i64>,
}

pub async fn artists(
    State(st): State<Arc<AppState>>,
    CurrentUser(_u): CurrentUser,
    Query(q): Query<MusicQuery>,
) -> AppResult<Json<Vec<ArtistSummary>>> {
    Ok(Json(db::music::artists(&st.pool, q.q.as_deref()).await?))
}

pub async fn artist(
    State(st): State<Arc<AppState>>,
    CurrentUser(_u): CurrentUser,
    Path(name): Path<String>,
) -> AppResult<Json<serde_json::Value>> {
    let albums = db::music::albums(&st.pool, Some(&name), None, "year", 500).await?;
    if albums.is_empty() {
        return Err(AppError::not_found("unknown artist"));
    }
    let art_id = albums[0].art_id.clone();
    Ok(Json(json!({ "name": name, "artId": art_id, "albums": albums })))
}

pub async fn albums(
    State(st): State<Arc<AppState>>,
    CurrentUser(_u): CurrentUser,
    Query(q): Query<MusicQuery>,
) -> AppResult<Json<Vec<AlbumSummary>>> {
    Ok(Json(
        db::music::albums(
            &st.pool,
            q.artist.as_deref(),
            q.q.as_deref(),
            &q.sort,
            q.limit.unwrap_or(5000).clamp(1, 20000),
        )
        .await?,
    ))
}

pub async fn album(
    State(st): State<Arc<AppState>>,
    CurrentUser(u): CurrentUser,
    Path(id): Path<String>,
) -> AppResult<Json<serde_json::Value>> {
    let album = db::music::album(&st.pool, &id).await?.ok_or_else(|| AppError::not_found("unknown album"))?;
    let tracks: Vec<Item> = db::music::album_tracks(&st.pool, u.id, &id).await?.into_iter().map(Item::card).collect();
    Ok(Json(json!({ "album": album, "tracks": tracks })))
}

pub async fn tracks(
    State(st): State<Arc<AppState>>,
    CurrentUser(u): CurrentUser,
    Query(q): Query<MusicQuery>,
) -> AppResult<Json<Vec<Item>>> {
    let items = match q.album.as_deref() {
        Some(album_id) => db::music::album_tracks(&st.pool, u.id, album_id).await?,
        None => db::music::tracks(&st.pool, u.id, q.q.as_deref(), q.limit.unwrap_or(500).clamp(1, 5000)).await?,
    };
    Ok(Json(items.into_iter().map(Item::card).collect()))
}
