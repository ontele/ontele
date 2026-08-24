// Copyright 2026 The Ontele Authors
// SPDX-License-Identifier: Apache-2.0

use crate::{
    auth::CurrentUser,
    db,
    error::{AppError, AppResult},
    media::playback,
    model::{ClientCaps, Kind, PlaybackPlan, SegmentKind},
    state::AppState,
    stream::StartRequest,
};
use axum::{
    Json,
    extract::{Path, Query, Request, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct StartBody {
    pub id: Option<String>,
    pub channel: Option<String>,
    #[serde(default)]
    pub start: f64,
    #[serde(default)]
    pub quality: String,
    /// ffprobe stream index of the audio track
    pub audio: Option<u32>,
    /// subtitle selection: `"burn:<streamIndex>"` or `"burn-ext:<listIndex>"`
    pub subtitle: Option<String>,
    pub caps: Option<ClientCaps>,
}

/// Decide direct vs HLS and, for HLS, spawn the session.
pub async fn start(
    State(st): State<Arc<AppState>>,
    CurrentUser(u): CurrentUser,
    Json(b): Json<StartBody>,
) -> AppResult<Json<serde_json::Value>> {
    let caps = b.caps.unwrap_or_default();
    let quality = if b.quality.is_empty() { "auto".to_string() } else { b.quality.clone() };
    let set = st.settings.get();

    // ---- live ----
    if let Some(ch) = b.channel.as_deref().filter(|c| !c.is_empty()) {
        let url = st
            .hdhr
            .stream_url(ch)
            .ok_or_else(|| AppError::Upstream("channel unavailable — refresh the tuner lineup".into()))?;
        let plan = PlaybackPlan {
            mode: "transcode".into(),
            video_copy: false,
            audio_copy: false,
            height: match quality.as_str() {
                "1080" | "original" => 1080,
                "480" => 480,
                "360" => 360,
                _ => 720,
            },
            segment: SegmentKind::Ts,
            reasons: vec!["live broadcast (MPEG-2)".into()],
        };
        let req = StartRequest {
            input: url,
            start_sec: 0.0,
            live: true,
            plan,
            audio_index: b.audio,
            burn_subtitle: None,
            burn_external: None,
            duration_sec: 0.0,
            vcodec: Some("mpeg2".into()),
            hdr: None,
        };
        let sess = st.streams.start(req).await.map_err(|e| AppError::Upstream(e.to_string()))?;
        st.activity.record(Some(u.id), "play.live", None, json!({ "channel": ch, "quality": quality }));
        metrics::counter!("ontele_playback_starts_total", "mode" => "live").increment(1);
        return Ok(Json(
            json!({ "sessionId": sess.id, "url": sess.playlist_url(), "offset": 0.0, "live": true, "mode": "transcode", "segment": "ts" }),
        ));
    }

    // ---- library / recording ----
    let id = b.id.as_deref().filter(|s| !s.is_empty()).ok_or_else(|| AppError::bad("need id or channel"))?;
    let (kind, path, info) =
        db::items::path_of(&st.pool, id).await?.ok_or_else(|| AppError::not_found("unknown item"))?;
    if kind == Kind::Recording.as_str() {
        // only finished recordings are playable
        let it = db::items::get(&st.pool, u.id, id).await?;
        if it.and_then(|i| i.status).as_deref() != Some("done") {
            return Err(AppError::bad("recording not finished"));
        }
    }
    let info = info.0;
    let mut plan = playback::decide(&info, &caps, &quality, false);

    // subtitle burn-in forces a transcode
    let mut burn_subtitle = None;
    let mut burn_external = None;
    if let Some(sub) = b.subtitle.as_deref() {
        if let Some(idx) = sub.strip_prefix("burn:").and_then(|s| s.parse::<u32>().ok()) {
            match info.subtitles.iter().find(|s| s.index == idx && s.external.is_none()) {
                Some(s) if s.text => {
                    return Err(AppError::bad(
                        "text subtitles are served as WebVTT tracks; burn-in is for bitmap subtitles",
                    ));
                }
                Some(_) => burn_subtitle = Some(idx),
                None => return Err(AppError::bad("unknown subtitle stream")),
            }
        } else if let Some(li) = sub.strip_prefix("burn-ext:").and_then(|s| s.parse::<usize>().ok()) {
            burn_external = info.subtitles.get(li).and_then(|s| s.external.clone()).map(std::path::PathBuf::from);
        }
        if burn_subtitle.is_some() || burn_external.is_some() {
            plan.mode = "transcode".into();
            plan.video_copy = false;
            plan.segment = SegmentKind::Ts;
            plan.reasons.push("subtitle burn-in".into());
            if plan.height == 0 {
                plan.height = info.height.unwrap_or(1080).min(1080);
            }
        }
    }
    // an explicit audio track other than the first also rules out direct play
    if let Some(a) = b.audio
        && plan.mode == "direct"
        && info.audio.first().map(|s| s.index) != Some(a)
    {
        plan.mode = "copy".into();
        plan.video_copy = true;
        plan.reasons.push("alternate audio track".into());
    }

    let count_view = |st: &AppState| {
        let (pool, uid, iid) = (st.pool.clone(), u.id, id.to_string());
        tokio::spawn(async move { db::trending::bump_view(&pool, uid, &iid).await.ok() });
    };

    if plan.mode == "direct" {
        count_view(&st);
        st.activity.record(Some(u.id), "play.start", Some(id), json!({ "mode": "direct", "start": b.start }));
        metrics::counter!("ontele_playback_starts_total", "mode" => "direct").increment(1);
        return Ok(Json(json!({
            "sessionId": null, "url": format!("/stream/direct/{id}"), "offset": 0.0, "live": false,
            "mode": "direct", "plan": plan,
        })));
    }

    let req = StartRequest {
        input: path,
        start_sec: b.start.max(0.0),
        live: false,
        plan: plan.clone(),
        audio_index: b.audio,
        burn_subtitle,
        burn_external,
        duration_sec: info.duration_sec,
        vcodec: info.vcodec.clone(),
        hdr: info.hdr().map(str::to_string),
    };
    let sess = st.streams.start(req).await.map_err(AppError::Internal)?;
    count_view(&st);
    st.activity.record(
        Some(u.id),
        "play.start",
        Some(id),
        json!({ "mode": plan.mode, "quality": quality, "start": b.start, "hw": set.hwaccel }),
    );
    metrics::counter!("ontele_playback_starts_total", "mode" => plan.mode.clone()).increment(1);
    Ok(Json(json!({
        "sessionId": sess.id, "url": sess.playlist_url(), "offset": sess.start_sec, "live": false,
        "mode": plan.mode, "segment": plan.segment, "plan": plan,
    })))
}

pub async fn keepalive(
    State(st): State<Arc<AppState>>,
    CurrentUser(_u): CurrentUser,
    Path(sid): Path<String>,
) -> Response {
    if st.streams.touch(&sid) {
        Json(json!({ "ok": true })).into_response()
    } else {
        (StatusCode::GONE, Json(json!({ "error": "session expired" }))).into_response()
    }
}

pub async fn stop(
    State(st): State<Arc<AppState>>,
    CurrentUser(u): CurrentUser,
    Path(sid): Path<String>,
) -> Json<serde_json::Value> {
    if let Some(s) = st.streams.get(&sid) {
        st.activity.record(
            Some(u.id),
            "play.stop",
            None,
            json!({ "session": sid, "seconds": s.created.elapsed().as_secs(), "live": s.live }),
        );
    }
    st.streams.stop(&sid);
    Json(json!({ "ok": true }))
}

pub async fn direct(
    State(st): State<Arc<AppState>>,
    CurrentUser(_u): CurrentUser,
    Path(id): Path<String>,
    req: Request,
) -> Response {
    let Ok(Some((_, path, info))) = db::items::path_of(&st.pool, &id).await else {
        return (StatusCode::NOT_FOUND, Json(json!({ "error": "unknown item" }))).into_response();
    };
    let ext = std::path::Path::new(&path).extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
    let mime = playback::direct_mime(&info.0.container, &ext);
    crate::stream::direct::serve_file(std::path::Path::new(&path), mime, req).await
}

#[derive(Deserialize, Default)]
pub struct AudioQuery {
    #[serde(default)]
    pub fmt: String,
    #[serde(default)]
    pub t: f64,
}

pub async fn audio(
    State(st): State<Arc<AppState>>,
    CurrentUser(u): CurrentUser,
    Path(id): Path<String>,
    Query(q): Query<AudioQuery>,
    req: Request,
) -> Response {
    let Ok(Some((_, path, info))) = db::items::path_of(&st.pool, &id).await else {
        return (StatusCode::NOT_FOUND, Json(json!({ "error": "unknown item" }))).into_response();
    };
    if q.t <= 0.0 && !req.headers().contains_key(axum::http::header::RANGE) {
        st.activity.record(Some(u.id), "play.track", Some(&id), json!({}));
    }
    let set = st.settings.get();
    let fmt = if q.fmt.is_empty() { "auto" } else { q.fmt.as_str() };
    crate::stream::direct::audio(&set, std::path::Path::new(&path), &info.0, fmt, q.t.max(0.0), req).await
}

pub async fn hls(
    State(st): State<Arc<AppState>>,
    CurrentUser(_u): CurrentUser,
    Path((sid, file)): Path<(String, String)>,
    req: Request,
) -> Response {
    st.streams.serve(&sid, &file, req).await
}
