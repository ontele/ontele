// Copyright 2026 The Ontele Authors
// SPDX-License-Identifier: Apache-2.0

use crate::{auth::CurrentUser, db, error::AppResult, state::AppState};
use axum::{
    Json,
    extract::{Path, State},
};
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;

pub async fn list(State(st): State<Arc<AppState>>, CurrentUser(_u): CurrentUser) -> AppResult<Json<serde_json::Value>> {
    let tags = db::tags::list(&st.pool).await?;
    Ok(Json(json!(tags.into_iter().map(|(name, count)| json!({ "name": name, "count": count })).collect::<Vec<_>>())))
}

#[derive(Deserialize)]
pub struct TagBody {
    #[serde(default)]
    pub tags: Vec<String>,
}

pub async fn add(
    State(st): State<Arc<AppState>>,
    CurrentUser(u): CurrentUser,
    Path(id): Path<String>,
    Json(b): Json<TagBody>,
) -> AppResult<Json<serde_json::Value>> {
    db::tags::add(&st.pool, &id, &b.tags).await?;
    st.activity.record(Some(u.id), "tag.add", Some(&id), json!({ "tags": b.tags }));
    Ok(Json(json!({ "tags": db::tags::for_item(&st.pool, &id).await? })))
}

pub async fn remove(
    State(st): State<Arc<AppState>>,
    CurrentUser(u): CurrentUser,
    Path((id, tag)): Path<(String, String)>,
) -> AppResult<Json<serde_json::Value>> {
    db::tags::remove(&st.pool, &id, &tag).await?;
    st.activity.record(Some(u.id), "tag.remove", Some(&id), json!({ "tag": tag }));
    Ok(Json(json!({ "tags": db::tags::for_item(&st.pool, &id).await? })))
}
