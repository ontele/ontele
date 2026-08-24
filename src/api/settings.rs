// Copyright 2026 The Ontele Authors
// SPDX-License-Identifier: Apache-2.0

use crate::{
    auth::{AdminUser, CurrentUser},
    db,
    error::AppResult,
    model::{ActivityEvent, Settings, User},
    state::AppState,
};
use axum::{
    Json,
    extract::{Path, Query, State},
};
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;

pub async fn me(State(st): State<Arc<AppState>>, CurrentUser(u): CurrentUser) -> Json<serde_json::Value> {
    Json(json!({
        "user": *u,
        "authMode": match st.cfg.auth { crate::config::AuthMode::Proxy => "proxy", crate::config::AuthMode::None => "none" },
        "version": env!("CARGO_PKG_VERSION"),
    }))
}

pub async fn get_settings(State(st): State<Arc<AppState>>, CurrentUser(u): CurrentUser) -> Json<serde_json::Value> {
    let s = st.settings.get();
    let mut v = serde_json::to_value(&*s).unwrap_or_default();
    if !u.is_admin {
        // non-admins may read settings for UI hints but never secrets
        if let Some(o) = v.as_object_mut() {
            o.insert("tmdbApiKey".into(), json!(if s.tmdb_api_key.is_empty() { "" } else { "••••••" }));
        }
    }
    Json(v)
}

pub async fn put_settings(
    State(st): State<Arc<AppState>>,
    AdminUser(u): AdminUser,
    Json(mut s): Json<Settings>,
) -> AppResult<Json<Settings>> {
    let old = st.settings.get();
    if s.tmdb_api_key == "••••••" {
        s.tmdb_api_key = old.tmdb_api_key.clone();
    }
    let saved = st.settings.set(s).await?;
    std::fs::create_dir_all(&saved.recordings_dir).ok();
    let libs_changed = saved.media_dirs != old.media_dirs || saved.music_dirs != old.music_dirs;
    st.activity.record(Some(u.id), "settings.update", None, json!({ "librariesChanged": libs_changed }));
    if libs_changed {
        let sc = st.scanner.clone();
        tokio::spawn(async move {
            let _ = sc.scan().await;
        });
    }
    Ok(Json((*saved).clone()))
}

/// Tooling check for the Settings page: versions, comskip presence, hwaccels.
pub async fn probe(State(st): State<Arc<AppState>>, CurrentUser(_u): CurrentUser) -> Json<serde_json::Value> {
    let s = st.settings.get();
    async fn version(bin: &str) -> Option<String> {
        let out = tokio::process::Command::new(bin).arg("-version").output().await.ok()?;
        let text = String::from_utf8_lossy(&out.stdout);
        text.lines().next().map(|l| l.trim().to_string())
    }
    async fn list(bin: &str, flag: &str) -> Vec<String> {
        let Ok(out) = tokio::process::Command::new(bin).arg("-hide_banner").arg(flag).output().await else {
            return vec![];
        };
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .skip(1)
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty())
            .collect()
    }
    let ffmpeg = version(&s.ffmpeg_path).await;
    let ffprobe = version(&s.ffprobe_path).await;
    let comskip = tokio::process::Command::new(&s.comskip_path).arg("--version").output().await.is_ok()
        || which(&s.comskip_path).is_some();
    let hwaccels = list(&s.ffmpeg_path, "-hwaccels").await;
    let encoders: Vec<String> = list(&s.ffmpeg_path, "-encoders")
        .await
        .into_iter()
        .filter(|l| l.contains("h264") || l.contains("hevc") || l.contains("av1"))
        .filter_map(|l| l.split_whitespace().nth(1).map(str::to_string))
        .collect();
    Json(json!({
        "ffmpeg": ffmpeg, "ffprobe": ffprobe, "comskip": comskip,
        "hwaccels": hwaccels, "encoders": encoders,
        "dataDir": st.cfg.data_dir, "uptimeSec": st.started.elapsed().as_secs(),
    }))
}

fn which(bin: &str) -> Option<std::path::PathBuf> {
    if bin.contains('/') {
        return std::fs::metadata(bin).ok().map(|_| bin.into());
    }
    std::env::var_os("PATH").and_then(|paths| std::env::split_paths(&paths).map(|d| d.join(bin)).find(|p| p.is_file()))
}

#[derive(Deserialize)]
pub struct LimitQuery {
    pub limit: Option<i64>,
}

pub async fn activity(
    State(st): State<Arc<AppState>>,
    CurrentUser(_u): CurrentUser,
    Query(q): Query<LimitQuery>,
) -> AppResult<Json<Vec<ActivityEvent>>> {
    Ok(Json(db::activity::recent(&st.pool, q.limit.unwrap_or(100)).await?))
}

pub async fn stats(
    State(st): State<Arc<AppState>>,
    CurrentUser(_u): CurrentUser,
) -> AppResult<Json<serde_json::Value>> {
    let counts = db::items::counts_by_kind(&st.pool).await?;
    let (sessions, transcodes) = st.streams.active();
    Ok(Json(json!({
        "items": counts,
        "streams": sessions, "transcodes": transcodes,
        "recordingsActive": st.dvr.active_count(),
        "channels": st.hdhr.channels().len(),
        "guideUpdated": st.guide.updated(),
        "scan": st.scanner.status(),
        "uptimeSec": st.started.elapsed().as_secs(),
        "version": env!("CARGO_PKG_VERSION"),
    })))
}

/// Ring-buffer samples + disk usage for Settings → Health (admins only).
pub async fn health(State(st): State<Arc<AppState>>, AdminUser(_u): AdminUser) -> AppResult<Json<serde_json::Value>> {
    Ok(Json(json!({
        "samples": st.health.samples(),
        "disks": st.health.disks(),
        "sampleEverySec": crate::health::SAMPLE_EVERY.as_secs(),
        "uptimeSec": st.started.elapsed().as_secs(),
    })))
}

pub async fn users(State(st): State<Arc<AppState>>, AdminUser(_u): AdminUser) -> AppResult<Json<Vec<User>>> {
    Ok(Json(db::users::list(&st.pool).await?))
}

#[derive(Deserialize)]
pub struct AdminBody {
    pub admin: bool,
}

pub async fn set_admin(
    State(st): State<Arc<AppState>>,
    AdminUser(u): AdminUser,
    Path(id): Path<i64>,
    Json(b): Json<AdminBody>,
) -> AppResult<Json<serde_json::Value>> {
    if id == u.id && !b.admin {
        return Err(crate::error::AppError::bad("cannot demote yourself"));
    }
    db::users::set_admin(&st.pool, id, b.admin).await?;
    st.users.clear();
    st.activity.record(Some(u.id), "user.admin", None, json!({ "user": id, "admin": b.admin }));
    Ok(Json(json!({ "ok": true })))
}
