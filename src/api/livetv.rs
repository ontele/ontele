// Copyright 2026 The Ontele Authors
// SPDX-License-Identifier: Apache-2.0

use crate::{
    auth::{AdminUser, CurrentUser},
    error::{AppError, AppResult},
    state::AppState,
};
use axum::{
    Json,
    body::Body,
    extract::{Path, Query, State},
    http::{StatusCode, header},
    response::{IntoResponse, Response},
};
use chrono::{DateTime, Duration, Utc};
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;

pub async fn channels(State(st): State<Arc<AppState>>, CurrentUser(_u): CurrentUser) -> Json<serde_json::Value> {
    let now = Utc::now();
    let chans: Vec<_> = st
        .hdhr
        .channels()
        .into_iter()
        .map(|c| {
            let (cur, next) = st.guide.now_next(&c.guide_number, now);
            json!({
                "guideNumber": c.guide_number, "guideName": c.guide_name, "url": c.url, "hd": c.hd,
                "icon": c.icon.as_ref().map(|_| format!("/api/livetv/icon/{}", c.guide_number)),
                "now": cur, "next": next,
            })
        })
        .collect();
    Json(json!({ "device": st.hdhr.device(), "channels": chans, "guideUpdated": st.guide.updated() }))
}

pub async fn refresh(State(st): State<Arc<AppState>>, AdminUser(u): AdminUser) -> AppResult<Json<serde_json::Value>> {
    tokio::time::timeout(std::time::Duration::from_secs(30), st.hdhr.refresh())
        .await
        .map_err(|_| AppError::Upstream("tuner discovery timed out".into()))?
        .map_err(|e| AppError::Upstream(e.to_string()))?;
    let st2 = st.clone();
    tokio::spawn(async move {
        let chans = st2.hdhr.channels();
        let auth = st2.hdhr.device().map(|d| d.device_auth).filter(|a| !a.is_empty());
        match st2.guide.refresh_with_hdhr(&chans, auth.as_deref()).await {
            Ok(n) => {
                st2.hdhr.set_icons(st2.guide.channel_icons()).await;
                tracing::info!(airings = n, "guide refreshed");
            }
            Err(e) => tracing::warn!(error = %e, "guide refresh"),
        }
    });
    st.activity.record(Some(u.id), "livetv.refresh", None, json!({ "channels": st.hdhr.channels().len() }));
    Ok(Json(json!({ "device": st.hdhr.device(), "channels": st.hdhr.channels() })))
}

/// Proxy + cache channel logos so the browser never talks to the guide provider.
pub async fn icon(State(st): State<Arc<AppState>>, CurrentUser(_u): CurrentUser, Path(num): Path<String>) -> Response {
    let Some(ch) = st.hdhr.channel(&num) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let Some(url) = ch.icon else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let dir = st.cfg.data_dir.join("icons");
    let _ = tokio::fs::create_dir_all(&dir).await;
    let file = dir.join(format!("{}.img", crate::model::item_id(&url)));
    if let Ok(bytes) = tokio::fs::read(&file).await {
        return icon_response(bytes);
    }
    match st.http.get(&url).send().await.and_then(|r| r.error_for_status()) {
        Ok(res) => match res.bytes().await {
            Ok(b) if b.len() < 4 * 1024 * 1024 => {
                let _ = tokio::fs::write(&file, &b).await;
                icon_response(b.to_vec())
            }
            _ => StatusCode::BAD_GATEWAY.into_response(),
        },
        Err(_) => StatusCode::BAD_GATEWAY.into_response(),
    }
}

fn icon_response(bytes: Vec<u8>) -> Response {
    let mime = if bytes.starts_with(b"\x89PNG") {
        "image/png"
    } else if bytes.starts_with(b"<svg") || bytes.starts_with(b"<?xml") {
        "image/svg+xml"
    } else if bytes.starts_with(b"GIF8") {
        "image/gif"
    } else if bytes.starts_with(b"RIFF") {
        "image/webp"
    } else {
        "image/jpeg"
    };
    Response::builder()
        .header(header::CONTENT_TYPE, mime)
        .header(header::CACHE_CONTROL, "public, max-age=604800")
        .body(Body::from(bytes))
        .unwrap()
}

#[derive(Deserialize)]
pub struct GuideQuery {
    pub hours: Option<i64>,
    pub from: Option<DateTime<Utc>>,
}

pub async fn guide(
    State(st): State<Arc<AppState>>,
    CurrentUser(_u): CurrentUser,
    Query(q): Query<GuideQuery>,
) -> Json<serde_json::Value> {
    let hours = q.hours.unwrap_or(6).clamp(1, 48);
    let from = q.from.unwrap_or_else(Utc::now);
    let to = from + Duration::hours(hours);
    let airings = st.guide.window(from - Duration::minutes(30), to);
    let channels: Vec<_> = st
        .hdhr
        .channels()
        .into_iter()
        .map(|c| {
            json!({
                "guideNumber": c.guide_number, "guideName": c.guide_name, "hd": c.hd,
                "icon": c.icon.as_ref().map(|_| format!("/api/livetv/icon/{}", c.guide_number)),
                "airings": airings.get(&c.guide_number).cloned().unwrap_or_default(),
            })
        })
        .collect();
    Json(json!({ "updated": st.guide.updated(), "from": from, "to": to, "channels": channels }))
}
