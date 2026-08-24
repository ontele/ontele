// Copyright 2026 The Ontele Authors
// SPDX-License-Identifier: Apache-2.0

use crate::{
    auth::CurrentUser,
    db::{self, items::NewRecording},
    error::{AppError, AppResult},
    model::{Item, Rule, rand_id},
    state::AppState,
};
use axum::{
    Json,
    extract::{Path, Query, State},
};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;

pub async fn recordings(State(st): State<Arc<AppState>>, CurrentUser(u): CurrentUser) -> AppResult<Json<Vec<Item>>> {
    Ok(Json(db::items::recordings(&st.pool, u.id).await?.into_iter().map(Item::card).collect()))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RecordBody {
    pub channel_id: String,
    #[serde(default)]
    pub title: String,
    pub subtitle: Option<String>,
    pub description: Option<String>,
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
    pub season: Option<i32>,
    pub episode: Option<i32>,
}

pub async fn record(
    State(st): State<Arc<AppState>>,
    CurrentUser(u): CurrentUser,
    Json(b): Json<RecordBody>,
) -> AppResult<Json<Item>> {
    if b.channel_id.trim().is_empty() || b.end <= b.start {
        return Err(AppError::bad("need channelId and start < end"));
    }
    if b.end < Utc::now() {
        return Err(AppError::bad("airing already ended"));
    }
    let rec = NewRecording {
        id: rand_id(6),
        title: if b.title.trim().is_empty() { "Manual recording".into() } else { b.title.trim().to_string() },
        subtitle: b.subtitle.filter(|s| !s.trim().is_empty()),
        description: b.description,
        channel_id: b.channel_id.trim().to_string(),
        channel_name: st.hdhr.channel_name(b.channel_id.trim()),
        start: b.start,
        end: b.end,
        rule_id: None,
        season: b.season,
        episode: b.episode,
    };
    let id = rec.id.clone();
    st.dvr.schedule(rec).await.map_err(|e| AppError::bad(e.to_string()))?;
    st.activity.record(Some(u.id), "dvr.schedule", Some(&id), json!({ "title": b.title, "channel": b.channel_id }));
    let it = db::items::get(&st.pool, u.id, &id).await?.ok_or_else(|| AppError::not_found("recording vanished"))?;
    Ok(Json(it.card()))
}

pub async fn delete_recording(
    State(st): State<Arc<AppState>>,
    CurrentUser(u): CurrentUser,
    Path(id): Path<String>,
) -> AppResult<Json<serde_json::Value>> {
    st.dvr.cancel(&id).await;
    let path = db::items::delete(&st.pool, &id).await?;
    if let Some(p) = path.as_deref() {
        match tokio::fs::remove_file(p).await {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => tracing::warn!(path = p, error = %e, "delete recording file"),
        }
    }
    st.art.invalidate(&id);
    // the path goes to the log only: activity detail is shown to every user
    tracing::info!(recording = %id, path = path.as_deref().unwrap_or(""), "recording deleted");
    st.activity.record(Some(u.id), "dvr.delete", Some(&id), json!({ "existed": path.is_some() }));
    Ok(Json(json!({ "ok": true })))
}

#[derive(Deserialize)]
pub struct AdscanQuery {
    #[serde(default)]
    pub cut: String,
}

pub async fn adscan(
    State(st): State<Arc<AppState>>,
    CurrentUser(u): CurrentUser,
    Path(id): Path<String>,
    Query(q): Query<AdscanQuery>,
) -> AppResult<Json<Item>> {
    let cut = q.cut == "1" || q.cut == "true";
    st.dvr.rescan_commercials(&id, cut).await.map_err(|e| AppError::bad(e.to_string()))?;
    st.activity.record(Some(u.id), "dvr.adscan", Some(&id), json!({ "cut": cut }));
    let it = db::items::get(&st.pool, u.id, &id).await?.ok_or_else(|| AppError::not_found("unknown recording"))?;
    Ok(Json(it.card()))
}

pub async fn rules(State(st): State<Arc<AppState>>, CurrentUser(_u): CurrentUser) -> AppResult<Json<Vec<Rule>>> {
    Ok(Json(db::rules::list(&st.pool).await?))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RuleBody {
    #[serde(default)]
    pub title: String,
    pub channel_id: Option<String>,
    #[serde(default)]
    pub keep: i32,
}

pub async fn add_rule(
    State(st): State<Arc<AppState>>,
    CurrentUser(u): CurrentUser,
    Json(b): Json<RuleBody>,
) -> AppResult<Json<Rule>> {
    let title = b.title.trim().to_string();
    if title.is_empty() {
        return Err(AppError::bad("need title"));
    }
    let rule = Rule {
        id: rand_id(6),
        title,
        channel_id: b.channel_id.filter(|c| !c.trim().is_empty()),
        keep: b.keep.max(0),
        user_id: Some(u.id),
        created: Utc::now(),
    };
    db::rules::insert(&st.pool, &rule).await?;
    st.activity.record(Some(u.id), "dvr.rule.add", None, json!({ "title": rule.title, "channel": rule.channel_id }));
    // materialize matches right away instead of waiting for the next tick
    let dvr = st.dvr.clone();
    tokio::spawn(async move { dvr.tick().await });
    Ok(Json(rule))
}

pub async fn delete_rule(
    State(st): State<Arc<AppState>>,
    CurrentUser(u): CurrentUser,
    Path(id): Path<String>,
) -> AppResult<Json<serde_json::Value>> {
    if let Some(r) = db::rules::get(&st.pool, &id).await?
        && !u.is_admin
        && r.user_id.is_some()
        && r.user_id != Some(u.id)
    {
        return Err(AppError::Forbidden("not your series pass".into()));
    }
    db::rules::delete(&st.pool, &id).await?;
    st.activity.record(Some(u.id), "dvr.rule.delete", None, json!({ "rule": id }));
    Ok(Json(json!({ "ok": true })))
}
