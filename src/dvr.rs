// Copyright 2026 The Ontele Authors
// SPDX-License-Identifier: Apache-2.0

//! Recording engine. A 15 s tick drives: series-rule matching against the
//! guide (materializing scheduled recordings), starting captures that are
//! due (start − pre-pad), failing missed airings, keep-N pruning and
//! reaping fully-watched recordings. Capture = HTTP GET of the tuner's
//! MPEG-TS streamed to disk until end + post-pad, then remux → .mkv, probe,
//! commercial pipeline per `Settings.commercial_mode`, metadata enrichment.

use crate::{
    commercials::{self, Detector},
    db::{self, items::NewRecording},
    epg::Guide,
    hdhr::Client as Hdhr,
    metadata::Enricher,
    model::{CommercialMode, Item, Kind, MediaInfo, Settings, breaks_state, rand_id, rec_status},
    naming::safe_filename,
    state::SettingsCache,
    telemetry::Activity,
};
use anyhow::{Context, anyhow, bail};
use chrono::{DateTime, Local, Utc};
use futures::StreamExt;
use serde_json::json;
use sqlx::PgPool;
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use tokio::io::{AsyncWriteExt, BufWriter};
use tokio_util::sync::CancellationToken;

pub const TICK_SECS: u64 = 15;

/// Per-request timeout for the capture GET. The shared client's 60 s default
/// would kill a capture mid-programme; a full day comfortably outlasts any
/// airing plus padding.
const CAPTURE_REQUEST_TIMEOUT: Duration = Duration::from_secs(12 * 3600);
/// A tuner that sends nothing for this long has dropped the stream.
const CAPTURE_STALL: Duration = Duration::from_secs(30);
/// Remux (stream copy) of a finished capture.
const REMUX_TIMEOUT: Duration = Duration::from_secs(2 * 3600);

pub struct Engine {
    pub pool: PgPool,
    pub settings: Arc<SettingsCache>,
    pub guide: Arc<Guide>,
    pub hdhr: Arc<Hdhr>,
    pub http: reqwest::Client,
    pub activity: Activity,
    pub metadata: Arc<Enricher>,
    active: dashmap::DashMap<String, CancellationToken>,
    /// Bounds concurrent comskip/ffmpeg detection passes (CPU heavy).
    pub detect_limit: Arc<tokio::sync::Semaphore>,
    /// Serializes scheduler passes: the API fires an extra `tick()` when a
    /// rule is added, and two overlapping passes would read `recording_keys`
    /// / `recordings_due` before either has inserted, scheduling the same
    /// airing twice or starting two captures of one recording.
    tick_lock: tokio::sync::Mutex<()>,
}

/// Why a capture stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    /// `stop_at` was reached: the airing (plus post-pad) is over.
    Deadline,
    /// The cancellation token fired.
    Cancelled,
    /// The tuner closed the stream before the deadline.
    StreamEnded,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaptureResult {
    pub bytes: u64,
    pub reason: StopReason,
}

impl Engine {
    pub fn new(
        pool: PgPool,
        settings: Arc<SettingsCache>,
        guide: Arc<Guide>,
        hdhr: Arc<Hdhr>,
        http: reqwest::Client,
        activity: Activity,
        metadata: Arc<Enricher>,
    ) -> Self {
        Self {
            pool,
            settings,
            guide,
            hdhr,
            http,
            activity,
            metadata,
            active: dashmap::DashMap::new(),
            detect_limit: Arc::new(tokio::sync::Semaphore::new(2)),
            tick_lock: tokio::sync::Mutex::new(()),
        }
    }

    pub fn active_count(&self) -> usize {
        self.active.len()
    }

    fn publish_gauge(&self) {
        metrics::gauge!("ontele_recordings_active").set(self.active.len() as f64);
    }

    /// Tick forever until cancelled (also resets recordings orphaned by a crash).
    pub async fn run_loop(self: Arc<Self>, cancel: CancellationToken) {
        match db::items::reset_stale_recording(&self.pool).await {
            Ok(n) if n > 0 => {
                tracing::warn!(count = n, "recordings interrupted by restart marked failed")
            }
            Ok(_) => {}
            Err(e) => tracing::warn!(error = %e, "reset stale recordings"),
        }
        self.publish_gauge();
        loop {
            self.tick().await;
            tokio::select! {
                _ = cancel.cancelled() => break,
                _ = tokio::time::sleep(Duration::from_secs(TICK_SECS)) => {}
            }
        }
        let ids: Vec<String> = self.active.iter().map(|e| e.key().clone()).collect();
        for id in ids {
            if let Some(tok) = self.active.get(&id) {
                tok.cancel();
            }
        }
        tracing::info!("dvr engine stopped");
    }

    /// One scheduler pass: match rules, start due captures, fail missed, prune.
    pub async fn tick(self: &Arc<Self>) {
        let _pass = self.tick_lock.lock().await;
        let set = self.settings.get();
        let now = Utc::now();
        let pre = set.pre_pad_min as i64 * 60;
        let post = set.post_pad_min as i64 * 60;

        if let Err(e) = self.match_rules(now).await {
            tracing::warn!(error = %e, "dvr rule matching");
        }

        match db::items::fail_missed(&self.pool, now, post).await {
            Ok(n) if n > 0 => tracing::warn!(count = n, "missed recordings"),
            Ok(_) => {}
            Err(e) => tracing::warn!(error = %e, "dvr fail_missed"),
        }

        match db::items::recordings_due(&self.pool, now, pre, post).await {
            Ok(due) => {
                for item in due {
                    let token = CancellationToken::new();
                    match self.active.entry(item.id.clone()) {
                        dashmap::mapref::entry::Entry::Occupied(_) => continue,
                        dashmap::mapref::entry::Entry::Vacant(v) => {
                            v.insert(token.clone());
                        }
                    }
                    self.publish_gauge();
                    let me = self.clone();
                    tokio::spawn(async move { me.record(item, token).await });
                }
            }
            Err(e) => tracing::warn!(error = %e, "dvr recordings_due"),
        }

        if let Err(e) = self.prune(&set).await {
            tracing::warn!(error = %e, "dvr prune");
        }
        self.publish_gauge();
    }

    /// Materialize upcoming airings that match a series rule.
    async fn match_rules(&self, now: DateTime<Utc>) -> anyhow::Result<()> {
        let rules = db::rules::list(&self.pool).await.context("list rules")?;
        if rules.is_empty() {
            return Ok(());
        }
        let mut keys = db::items::recording_keys(&self.pool).await.context("recording keys")?;
        let mut scheduled = 0usize;
        for rule in &rules {
            let channel = rule.channel_id.as_deref().filter(|c| !c.trim().is_empty());
            for airing in self.guide.matches(&rule.title, channel, now) {
                if airing.end <= now || airing.end <= airing.start {
                    continue;
                }
                let key = db::items::recording_key(&rule.id, &airing.channel_id, airing.start);
                // the same airing already scheduled manually counts too
                let manual_key = db::items::recording_key("", &airing.channel_id, airing.start);
                if keys.contains(&key) || keys.contains(&manual_key) {
                    continue;
                }
                let rec = NewRecording {
                    id: rand_id(6),
                    title: airing.title.clone(),
                    subtitle: airing.subtitle.clone().filter(|s| !s.trim().is_empty()),
                    description: airing.description.clone(),
                    channel_id: airing.channel_id.clone(),
                    channel_name: self.hdhr.channel_name(&airing.channel_id),
                    start: airing.start,
                    end: airing.end,
                    rule_id: Some(rule.id.clone()),
                    season: airing.season,
                    episode: airing.episode,
                };
                match db::items::insert_recording(&self.pool, &rec).await {
                    Ok(()) => {
                        keys.insert(key);
                        scheduled += 1;
                        tracing::info!(
                            id = %rec.id, rule = %rule.id, title = %rec.title, channel = %rec.channel_id,
                            start = %rec.start, "recording scheduled by rule"
                        );
                        self.activity.record(
                            rule.user_id,
                            "dvr.scheduled",
                            Some(&rec.id),
                            json!({ "title": rec.title, "subtitle": rec.subtitle, "channel": rec.channel_id,
                                    "start": rec.start, "end": rec.end, "rule": rule.id }),
                        );
                    }
                    Err(e) => {
                        tracing::warn!(rule = %rule.id, title = %airing.title, error = %e, "schedule by rule")
                    }
                }
            }
        }
        if scheduled > 0 {
            tracing::info!(count = scheduled, "rule matching scheduled recordings");
        }
        Ok(())
    }

    /// Keep-N per rule and (optionally) recordings everyone has finished.
    async fn prune(&self, set: &Settings) -> anyhow::Result<()> {
        let rules = db::rules::list(&self.pool).await.context("list rules")?;
        let keep_by_rule: std::collections::HashMap<&str, i32> =
            rules.iter().map(|r| (r.id.as_str(), r.keep)).collect();
        let by_rule = db::items::done_by_rule(&self.pool).await.context("done by rule")?;
        let mut victims: Vec<(String, Option<String>, &'static str)> = Vec::new();
        for (rule_id, recs) in &by_rule {
            let Some(&keep) = keep_by_rule.get(rule_id.as_str()) else {
                continue;
            };
            for (id, path) in victims_for_keep(recs, keep) {
                // `done` is set before the commercial pass finishes; never
                // pull a file out from under a running cut/chapter remux
                if self.active.contains_key(&id) {
                    continue;
                }
                victims.push((id, path, "keep"));
            }
        }
        if set.auto_delete_watched {
            for (id, path) in db::watch::fully_watched_recordings(&self.pool).await.context("fully watched")? {
                if self.active.contains_key(&id) || victims.iter().any(|(v, _, _)| *v == id) {
                    continue;
                }
                victims.push((id, path, "watched"));
            }
        }
        for (id, path, reason) in victims {
            let db_path = match db::items::delete(&self.pool, &id).await {
                Ok(p) => p,
                Err(e) => {
                    tracing::warn!(id, error = %e, "prune delete row");
                    continue;
                }
            };
            let path = db_path.or(path);
            if let Some(p) = path.as_deref() {
                match tokio::fs::remove_file(p).await {
                    Ok(()) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => tracing::warn!(id, path = p, error = %e, "prune delete file"),
                }
            }
            tracing::info!(id, reason, path = path.as_deref().unwrap_or(""), "recording pruned");
            self.activity.record(None, "dvr.pruned", Some(&id), json!({ "reason": reason, "path": path }));
        }
        Ok(())
    }

    /// Stop an in-flight capture or unschedule a pending one.
    pub async fn cancel(&self, id: &str) -> bool {
        let was_active = match self.active.get(id) {
            Some(tok) => {
                tok.cancel();
                true
            }
            None => false,
        };
        let unscheduled = match db::items::cancel_scheduled(&self.pool, id).await {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!(id, error = %e, "cancel scheduled");
                false
            }
        };
        if was_active || unscheduled {
            tracing::info!(id, active = was_active, "recording canceled");
        }
        was_active || unscheduled
    }

    /// Insert a manual/one-off recording.
    pub async fn schedule(&self, rec: NewRecording) -> anyhow::Result<()> {
        if rec.end <= rec.start {
            bail!("recording end must be after start");
        }
        if rec.channel_id.trim().is_empty() {
            bail!("recording needs a channel");
        }
        if rec.title.trim().is_empty() {
            bail!("recording needs a title");
        }
        db::items::insert_recording(&self.pool, &rec).await.context("insert recording")?;
        tracing::info!(id = %rec.id, title = %rec.title, channel = %rec.channel_id, start = %rec.start, "recording scheduled");
        Ok(())
    }

    /// Re-run commercial detection for a finished recording; `and_cut`
    /// hard-cuts the result.
    pub async fn rescan_commercials(&self, id: &str, and_cut: bool) -> anyhow::Result<()> {
        let item = db::items::get(&self.pool, 0, id).await?.ok_or_else(|| anyhow!("unknown recording"))?;
        if item.kind != Some(Kind::Recording) {
            bail!("not a recording");
        }
        if item.status.as_deref() != Some(rec_status::DONE) {
            bail!("recording is not finished");
        }
        let path = item.path.clone().filter(|p| !p.is_empty()).ok_or_else(|| anyhow!("recording has no file"))?;
        let path = PathBuf::from(path);
        if !tokio::fs::try_exists(&path).await.unwrap_or(false) {
            bail!("recording file missing: {}", path.display());
        }
        let set = self.settings.get();
        db::items::set_breaks(&self.pool, id, None, breaks_state::PENDING).await?;
        let detected = {
            let _permit = self.detect_limit.acquire().await.map_err(|_| anyhow!("detector unavailable"))?;
            commercials::detect(&set, &path).await
        };
        let (breaks, detector) = match detected {
            Ok(v) => v,
            Err(e) => {
                db::items::set_breaks(&self.pool, id, None, breaks_state::FAILED).await?;
                return Err(e.context("commercial detection"));
            }
        };
        let total = item.info.as_ref().map(|i| i.duration_sec).unwrap_or(item.duration);
        if and_cut && !breaks.is_empty() {
            let (new_path, kept) = commercials::cut(&set, &path, total, &breaks).await.context("cut")?;
            let info = self.probe_or_fallback(&set, &new_path, Some(kept), item.info.as_ref()).await;
            db::items::set_cut(&self.pool, id, &new_path.to_string_lossy(), &info).await?;
            tracing::info!(id, detector = %detector, breaks = breaks.len(), kept, "recording cut");
        } else {
            db::items::set_breaks(&self.pool, id, Some(&breaks), breaks_state::READY).await?;
            if set.commercial_chapters && !breaks.is_empty() {
                let _ = self.apply_chapters(&set, id, &path, total, &breaks, item.info.as_ref()).await;
            }
            tracing::info!(id, detector = %detector, breaks = breaks.len(), "commercials rescanned");
        }
        Ok(())
    }

    /// Write chapters into the container; updates path/info if the file
    /// moved. Failures are logged, never fatal — the breaks are stored already.
    async fn apply_chapters(
        &self,
        set: &Settings,
        id: &str,
        path: &Path,
        total: f64,
        breaks: &[crate::model::Break],
        prev: Option<&MediaInfo>,
    ) -> Option<std::path::PathBuf> {
        match commercials::write_chapters(set, path, total, breaks).await {
            Ok(new_path) => {
                let info = self.probe_or_fallback(set, &new_path, None, prev).await;
                let res = if new_path != path {
                    db::items::set_path_info(&self.pool, id, &new_path.to_string_lossy(), &info).await
                } else {
                    db::items::set_info(&self.pool, id, &info).await
                };
                if let Err(e) = res {
                    tracing::warn!(id, error = %e, "store chapter remux result");
                }
                Some(new_path)
            }
            Err(e) => {
                tracing::warn!(id, error = %e, "write chapters");
                None
            }
        }
    }

    /// Probe a file; on failure synthesize the minimum we know (size,
    /// container, duration hint) so the row is still playable.
    async fn probe_or_fallback(
        &self,
        set: &Settings,
        path: &Path,
        duration: Option<f64>,
        prev: Option<&MediaInfo>,
    ) -> MediaInfo {
        match crate::media::probe::probe(&set.ffprobe_path, path).await {
            Ok(mut info) => {
                if info.duration_sec <= 0.0
                    && let Some(d) = duration
                {
                    info.duration_sec = d;
                }
                info
            }
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "probe failed; using fallback info");
                let mut info = prev.cloned().unwrap_or_default();
                info.size_bytes = tokio::fs::metadata(path).await.map(|m| m.len() as i64).unwrap_or(0);
                info.container = path.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
                if let Some(d) = duration {
                    info.duration_sec = d;
                }
                info.chapters.clear();
                info
            }
        }
    }

    /// Capture one recording end to end. Runs as its own task.
    async fn record(self: Arc<Self>, item: Item, token: CancellationToken) {
        let id = item.id.clone();
        let res = self.record_inner(&item, &token).await;
        self.active.remove(&id);
        self.publish_gauge();
        if let Err(e) = res {
            tracing::error!(id, title = %item.title, error = %e, "recording failed");
            if let Err(e2) = db::items::set_status(&self.pool, &id, rec_status::FAILED, Some(&e.to_string())).await {
                tracing::warn!(id, error = %e2, "store recording failure");
            }
            self.activity.record(None, "dvr.failed", Some(&id), json!({ "title": item.title, "error": e.to_string() }));
        }
    }

    async fn record_inner(&self, item: &Item, token: &CancellationToken) -> anyhow::Result<()> {
        let set = self.settings.get();
        let (Some(start), Some(end)) = (item.start, item.end) else { bail!("recording has no airing window") };
        let channel =
            item.channel_id.clone().filter(|c| !c.is_empty()).ok_or_else(|| anyhow!("recording has no channel"))?;
        let url = self.hdhr.stream_url(&channel).ok_or_else(|| anyhow!("no tuner stream for channel {channel}"))?;
        if set.recordings_dir.trim().is_empty() {
            bail!("recordings directory not configured");
        }

        let dir = Path::new(&set.recordings_dir).join(safe_filename(&item.title));
        tokio::fs::create_dir_all(&dir).await.with_context(|| format!("create {}", dir.display()))?;
        let base = recording_basename(&item.title, start, item.subtitle.as_deref());
        let ts_path = unique_path(&dir, &base, "ts").await;
        db::items::set_recording_started(&self.pool, &item.id, &ts_path.to_string_lossy())
            .await
            .context("mark recording started")?;

        let stop_at = end + chrono::Duration::seconds(set.post_pad_min as i64 * 60);
        tracing::info!(id = %item.id, title = %item.title, channel, path = %ts_path.display(), stop_at = %stop_at, "capture started");
        self.activity.record(
            None,
            "dvr.started",
            Some(&item.id),
            json!({ "title": item.title, "channel": channel, "path": ts_path }),
        );

        let cap = capture(&self.http, &url, &ts_path, stop_at, token).await;
        let cap = match cap {
            Ok(c) => c,
            Err(e) => {
                let have = tokio::fs::metadata(&ts_path).await.map(|m| m.len()).unwrap_or(0);
                // a tuner hiccup after most of the show is on disk is not worth
                // throwing the recording away
                if have > 0 && Utc::now() >= end {
                    tracing::warn!(id = %item.id, error = %e, bytes = have, "capture ended with error after airing end; keeping");
                    CaptureResult { bytes: have, reason: StopReason::StreamEnded }
                } else {
                    return Err(e);
                }
            }
        };
        tracing::info!(id = %item.id, bytes = cap.bytes, reason = ?cap.reason, "capture stopped");

        match cap.reason {
            StopReason::Cancelled if Utc::now() < end => {
                // keep the partial file for the user to inspect
                db::items::set_status(&self.pool, &item.id, rec_status::FAILED, Some("capture canceled")).await?;
                self.activity.record(
                    None,
                    "dvr.canceled",
                    Some(&item.id),
                    json!({ "title": item.title, "bytes": cap.bytes }),
                );
                return Ok(());
            }
            StopReason::StreamEnded if cap.bytes == 0 => bail!("tuner stream ended with no data"),
            StopReason::StreamEnded if Utc::now() < end => {
                tracing::warn!(id = %item.id, "tuner stream ended before the airing finished; post-processing partial capture");
            }
            _ => {}
        }
        if cap.bytes == 0 {
            bail!("capture produced no data");
        }
        self.post_process(&set, item, &ts_path).await
    }

    async fn post_process(&self, set: &Settings, item: &Item, ts_path: &Path) -> anyhow::Result<()> {
        // remux to mkv (stream copy); on failure keep the .ts playable
        let mkv = ts_path.with_extension("mkv");
        let args: Vec<std::ffi::OsString> = vec![
            "-i".into(),
            ts_path.as_os_str().to_os_string(),
            "-map".into(),
            "0:v:0".into(),
            "-map".into(),
            "0:a?".into(),
            "-c".into(),
            "copy".into(),
            "-y".into(),
            mkv.as_os_str().to_os_string(),
        ];
        let final_path = match commercials::run_ffmpeg(&set.ffmpeg_path, &args, REMUX_TIMEOUT).await {
            Ok(()) => {
                if let Err(e) = tokio::fs::remove_file(ts_path).await {
                    tracing::warn!(path = %ts_path.display(), error = %e, "remove .ts after remux");
                }
                mkv
            }
            Err(e) => {
                tracing::warn!(id = %item.id, error = %e, "remux failed; keeping MPEG-TS");
                let _ = tokio::fs::remove_file(&mkv).await;
                ts_path.to_path_buf()
            }
        };

        let info = self.probe_or_fallback(set, &final_path, None, None).await;
        let mode = set.commercial_mode;
        let state = if mode == CommercialMode::Off { None } else { Some(breaks_state::PENDING) };
        db::items::set_recording_done(&self.pool, &item.id, &final_path.to_string_lossy(), &info, state)
            .await
            .context("mark recording done")?;
        tracing::info!(id = %item.id, path = %final_path.display(), duration = info.duration_sec, "recording done");

        let mut out_path = final_path.clone();
        let mut detector: Option<Detector> = None;
        let mut n_breaks = 0usize;
        if mode != CommercialMode::Off {
            let detected = {
                let _permit = self.detect_limit.acquire().await.map_err(|_| anyhow!("detector unavailable"))?;
                commercials::detect(set, &final_path).await
            };
            match detected {
                Ok((breaks, det)) => {
                    detector = Some(det);
                    n_breaks = breaks.len();
                    match mode {
                        CommercialMode::Delete if !breaks.is_empty() => {
                            match commercials::cut(set, &final_path, info.duration_sec, &breaks).await {
                                Ok((new_path, kept)) => {
                                    out_path = new_path.clone();
                                    let info2 = self.probe_or_fallback(set, &new_path, Some(kept), Some(&info)).await;
                                    log_db(
                                        db::items::set_cut(&self.pool, &item.id, &new_path.to_string_lossy(), &info2)
                                            .await,
                                        &item.id,
                                        "set_cut",
                                    );
                                    tracing::info!(id = %item.id, breaks = breaks.len(), kept, "commercials cut");
                                }
                                Err(e) => {
                                    tracing::warn!(id = %item.id, error = %e, "cut failed; storing breaks for skipping instead");
                                    log_db(
                                        db::items::set_breaks(&self.pool, &item.id, Some(&breaks), breaks_state::READY)
                                            .await,
                                        &item.id,
                                        "set_breaks",
                                    );
                                }
                            }
                        }
                        _ => {
                            log_db(
                                db::items::set_breaks(&self.pool, &item.id, Some(&breaks), breaks_state::READY).await,
                                &item.id,
                                "set_breaks",
                            );
                            if mode == CommercialMode::Skip
                                && set.commercial_chapters
                                && !breaks.is_empty()
                                && let Some(p) = self
                                    .apply_chapters(set, &item.id, &final_path, info.duration_sec, &breaks, Some(&info))
                                    .await
                            {
                                out_path = p; // chapter remux may change the container
                            }
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!(id = %item.id, error = %e, "commercial detection failed");
                    log_db(
                        db::items::set_breaks(&self.pool, &item.id, None, breaks_state::FAILED).await,
                        &item.id,
                        "set_breaks",
                    );
                }
            }
        }

        self.metadata.kick();
        self.activity.record(
            None,
            "dvr.finished",
            Some(&item.id),
            json!({
                "title": item.title,
                "path": final_path,
                "duration": info.duration_sec,
                "detector": detector.map(|d| d.as_str()),
                "breaks": n_breaks,
                "mode": mode,
            }),
        );
        if !set.dvr_post_cmd.is_empty() {
            spawn_post_cmd(&set.dvr_post_cmd, &out_path, &item.title, &item.id, self.activity.clone());
        }
        Ok(())
    }
}

/// Fire-and-forget DVR post-processing hook: `sh -c <cmd> ontele-post <file>`
/// with ONTELE_FILE / ONTELE_TITLE / ONTELE_ID exported. Encodes can be slow —
/// generous timeout, outcome recorded to the activity feed either way.
fn spawn_post_cmd(cmd: &str, path: &std::path::Path, title: &str, id: &str, activity: crate::telemetry::Activity) {
    const POST_CMD_TIMEOUT: Duration = Duration::from_secs(6 * 3600);
    let (cmd, path, title, id) = (cmd.to_string(), path.to_path_buf(), title.to_string(), id.to_string());
    tokio::spawn(async move {
        let started = std::time::Instant::now();
        let outcome = async {
            let mut child = tokio::process::Command::new("sh")
                .arg("-c")
                .arg(&cmd)
                .arg("ontele-post")
                .arg(&path)
                .env("ONTELE_FILE", &path)
                .env("ONTELE_TITLE", &title)
                .env("ONTELE_ID", &id)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::piped())
                .kill_on_drop(true) // a timed-out encode must not linger
                .spawn()
                .map_err(|e| format!("spawn: {e}"))?;
            // Drain stderr keeping only a bounded tail — hours of encoder
            // progress must not accumulate in server memory.
            let mut tail: Vec<u8> = Vec::new();
            if let Some(mut err) = child.stderr.take() {
                use tokio::io::AsyncReadExt;
                let mut buf = [0u8; 4096];
                while let Ok(n) = err.read(&mut buf).await {
                    if n == 0 {
                        break;
                    }
                    tail.extend_from_slice(&buf[..n]);
                    if tail.len() > 4096 {
                        tail.drain(..tail.len() - 4096);
                    }
                }
            }
            let status = child.wait().await.map_err(|e| format!("wait: {e}"))?;
            if status.success() {
                Ok(())
            } else {
                let tail = String::from_utf8_lossy(&tail);
                let tail: String = tail.chars().rev().take(400).collect::<String>().chars().rev().collect();
                Err(format!("exit {}: {}", status.code().unwrap_or(-1), tail.trim()))
            }
        };
        let outcome = match tokio::time::timeout(POST_CMD_TIMEOUT, outcome).await {
            Ok(r) => r,
            Err(_) => Err("timed out".to_string()),
        };
        let secs = started.elapsed().as_secs();
        match outcome {
            Ok(()) => {
                tracing::info!(id, path = %path.display(), secs, "dvr post command done");
                activity.record(None, "dvr.postcmd", Some(&id), json!({ "path": path, "secs": secs, "ok": true }));
            }
            Err(e) => {
                tracing::warn!(id, path = %path.display(), secs, error = %e, "dvr post command failed");
                activity.record(
                    None,
                    "dvr.postcmd",
                    Some(&id),
                    json!({ "path": path, "secs": secs, "ok": false, "error": e }),
                );
            }
        }
    });
}

/// Oldest-first victims beyond `keep` for one rule; `keep <= 0` keeps all.
pub fn victims_for_keep(recs: &[(String, Option<String>, DateTime<Utc>)], keep: i32) -> Vec<(String, Option<String>)> {
    if keep <= 0 || recs.len() <= keep as usize {
        return vec![];
    }
    let mut sorted: Vec<&(String, Option<String>, DateTime<Utc>)> = recs.iter().collect();
    sorted.sort_by(|a, b| a.2.cmp(&b.2).then_with(|| a.0.cmp(&b.0)));
    let excess = sorted.len() - keep as usize;
    sorted.into_iter().take(excess).map(|(id, path, _)| (id.clone(), path.clone())).collect()
}

/// `<dir>/<base>.<ext>`, adding ` (2)`, ` (3)`… when the name is taken.
async fn unique_path(dir: &Path, base: &str, ext: &str) -> PathBuf {
    let first = dir.join(format!("{base}.{ext}"));
    if !tokio::fs::try_exists(&first).await.unwrap_or(false)
        && !tokio::fs::try_exists(dir.join(format!("{base}.mkv"))).await.unwrap_or(false)
    {
        return first;
    }
    for n in 2..1000 {
        let p = dir.join(format!("{base} ({n}).{ext}"));
        let sibling = dir.join(format!("{base} ({n}).mkv"));
        if !tokio::fs::try_exists(&p).await.unwrap_or(false) && !tokio::fs::try_exists(&sibling).await.unwrap_or(false)
        {
            return p;
        }
    }
    dir.join(format!("{base} ({}).{ext}", rand_id(3)))
}

/// Stream a tuner URL to `dest` until `stop_at`, cancellation, or the tuner
/// closes the connection. Returns how many bytes landed and why it stopped.
/// Non-200 responses fail immediately (HDHomeRun answers 503 when every
/// tuner is busy).
pub async fn capture(
    http: &reqwest::Client,
    url: &str,
    dest: &Path,
    stop_at: DateTime<Utc>,
    cancel: &CancellationToken,
) -> anyhow::Result<CaptureResult> {
    let resp =
        http.get(url).timeout(CAPTURE_REQUEST_TIMEOUT).send().await.with_context(|| format!("tuner request {url}"))?;
    let status = resp.status();
    if status != reqwest::StatusCode::OK {
        bail!("tuner: {status} (all tuners busy?)");
    }
    let file = tokio::fs::File::create(dest).await.with_context(|| format!("create {}", dest.display()))?;
    let mut out = BufWriter::with_capacity(1 << 20, file);
    let mut stream = resp.bytes_stream();
    let mut bytes = 0u64;

    let remaining = (stop_at - Utc::now()).to_std().unwrap_or(Duration::ZERO);
    let deadline = tokio::time::sleep(remaining);
    tokio::pin!(deadline);

    let reason = loop {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => break StopReason::Cancelled,
            _ = &mut deadline => break StopReason::Deadline,
            next = tokio::time::timeout(CAPTURE_STALL, stream.next()) => match next {
                Ok(Some(Ok(chunk))) => {
                    out.write_all(&chunk).await.with_context(|| format!("write {}", dest.display()))?;
                    bytes += chunk.len() as u64;
                }
                Ok(Some(Err(e))) => {
                    out.flush().await.ok();
                    return Err(anyhow!(e).context(format!("tuner stream after {bytes} bytes")));
                }
                Ok(None) => break StopReason::StreamEnded,
                Err(_) => {
                    out.flush().await.ok();
                    bail!("tuner stalled for {:?} after {bytes} bytes", CAPTURE_STALL);
                }
            },
        }
    };
    out.flush().await.with_context(|| format!("flush {}", dest.display()))?;
    out.into_inner().sync_all().await.ok();
    Ok(CaptureResult { bytes, reason })
}

/// `<Title> - YYYY-MM-DD HH-MM[ - Subtitle]` (filesystem-safe).
///
/// The timestamp is rendered in the server's **local** time zone
/// (`chrono::Local`) so filenames match what the user saw in the guide.
pub fn recording_basename(title: &str, start: DateTime<Utc>, subtitle: Option<&str>) -> String {
    let local = start.with_timezone(&Local);
    let mut s = format!("{} - {}", safe_filename(title), local.format("%Y-%m-%d %H-%M"));
    if let Some(sub) = subtitle.map(str::trim).filter(|s| !s.is_empty()) {
        s.push_str(" - ");
        s.push_str(&safe_filename(sub));
    }
    s
}

/// After a recording is marked done its file is complete and playable; a
/// transient DB failure while storing ad-break results must not flip the
/// recording to `failed`. Log and carry on instead.
fn log_db(res: sqlx::Result<()>, id: &str, what: &str) {
    if let Err(e) = res {
        tracing::warn!(id, what, error = %e, "post-processing DB write failed (recording stays done)");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{Router, body::Body, routing::get};
    use chrono::TimeZone;

    #[test]
    fn basename_formatting() {
        let start = Utc.with_ymd_and_hms(2026, 3, 4, 20, 30, 0).unwrap();
        let stamp = start.with_timezone(&Local).format("%Y-%m-%d %H-%M").to_string();
        assert_eq!(recording_basename("Static Signal", start, None), format!("Static Signal - {stamp}"));
        assert_eq!(
            recording_basename("Static Signal", start, Some("Cold Boot")),
            format!("Static Signal - {stamp} - Cold Boot")
        );
        // subtitle whitespace-only is dropped; unsafe chars are replaced
        assert_eq!(recording_basename("A/B: C?", start, Some("   ")), format!("A_B_ C_ - {stamp}"));
        assert_eq!(recording_basename("Show", start, Some("Part 1/2")), format!("Show - {stamp} - Part 1_2"));
        assert_eq!(recording_basename("", start, None), format!("untitled - {stamp}"));
        assert!(!recording_basename("Trailing dots...", start, None).contains("..."));
    }

    #[test]
    fn keep_n_selects_oldest() {
        let t = |h: u32| Utc.with_ymd_and_hms(2026, 1, 1, h, 0, 0).unwrap();
        let recs = vec![
            ("c".to_string(), Some("/r/c.mkv".to_string()), t(3)),
            ("a".to_string(), Some("/r/a.mkv".to_string()), t(1)),
            ("d".to_string(), None, t(4)),
            ("b".to_string(), Some("/r/b.mkv".to_string()), t(2)),
        ];
        let v = victims_for_keep(&recs, 2);
        assert_eq!(
            v,
            vec![("a".to_string(), Some("/r/a.mkv".to_string())), ("b".to_string(), Some("/r/b.mkv".to_string()))]
        );
        assert!(victims_for_keep(&recs, 0).is_empty());
        assert!(victims_for_keep(&recs, -1).is_empty());
        assert!(victims_for_keep(&recs, 4).is_empty());
        assert!(victims_for_keep(&recs, 10).is_empty());
        assert_eq!(victims_for_keep(&recs, 3).len(), 1);
        assert!(victims_for_keep(&[], 1).is_empty());
    }

    async fn serve(router: Router) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        format!("http://{addr}")
    }

    fn ts_stream_router() -> Router {
        Router::new()
            .route(
                "/auto/v7.1",
                get(|| async {
                    // an endless "tuner": 188-byte TS packets every 20 ms
                    let s = futures::stream::unfold(0u32, |i| async move {
                        tokio::time::sleep(Duration::from_millis(20)).await;
                        let mut pkt = vec![0x47u8; 188];
                        pkt[1..5].copy_from_slice(&i.to_be_bytes());
                        Some((Ok::<_, std::io::Error>(bytes::Bytes::from(pkt)), i + 1))
                    });
                    axum::response::Response::builder()
                        .header("content-type", "video/mp2t")
                        .body(Body::from_stream(s))
                        .unwrap()
                }),
            )
            .route("/busy", get(|| async { (axum::http::StatusCode::SERVICE_UNAVAILABLE, "all tuners in use") }))
            .route("/short", get(|| async { bytes::Bytes::from_static(b"hello ts") }))
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn post_cmd_runs_with_env_and_records_outcome(pool: sqlx::PgPool) {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("marker");
        let rec = dir.path().join("Show S01E02.mkv");
        std::fs::write(&rec, b"x").unwrap();
        let cmd = format!("printf '%s|%s|%s' \"$1\" \"$ONTELE_TITLE\" \"$ONTELE_ID\" > {}", marker.display());
        spawn_post_cmd(&cmd, &rec, "Show", "rec1", crate::telemetry::Activity::new(pool));
        for _ in 0..50 {
            if marker.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let got = std::fs::read_to_string(&marker).expect("post command ran");
        assert_eq!(got, format!("{}|Show|rec1", rec.display()));
    }

    #[tokio::test]
    async fn capture_stops_at_deadline_and_writes_bytes() {
        let base = serve(ts_stream_router()).await;
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("cap.ts");
        let http = reqwest::Client::builder().timeout(Duration::from_secs(1)).build().unwrap();
        let stop_at = Utc::now() + chrono::Duration::milliseconds(600);
        let t0 = std::time::Instant::now();
        let res =
            capture(&http, &format!("{base}/auto/v7.1"), &dest, stop_at, &CancellationToken::new()).await.unwrap();
        let elapsed = t0.elapsed();
        assert_eq!(res.reason, StopReason::Deadline);
        assert!(elapsed < Duration::from_secs(3), "took {elapsed:?}");
        let data = std::fs::read(&dest).unwrap();
        assert_eq!(data.len() as u64, res.bytes);
        assert!(data.len() >= 188 * 5, "got {} bytes", data.len());
        assert_eq!(data.len() % 188, 0);
        // packets arrived in order, starting at 0
        assert_eq!(data[0], 0x47);
        assert_eq!(&data[1..5], &0u32.to_be_bytes());
        assert_eq!(&data[188 + 1..188 + 5], &1u32.to_be_bytes());
    }

    #[tokio::test]
    async fn capture_honours_cancel() {
        let base = serve(ts_stream_router()).await;
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("cap.ts");
        let http = reqwest::Client::new();
        let token = CancellationToken::new();
        let t = token.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(200)).await;
            t.cancel();
        });
        let stop_at = Utc::now() + chrono::Duration::seconds(30);
        let res = capture(&http, &format!("{base}/auto/v7.1"), &dest, stop_at, &token).await.unwrap();
        assert_eq!(res.reason, StopReason::Cancelled);
        assert!(res.bytes > 0);
        assert_eq!(std::fs::metadata(&dest).unwrap().len(), res.bytes);
    }

    #[tokio::test]
    async fn capture_reports_busy_tuner_and_stream_end() {
        let base = serve(ts_stream_router()).await;
        let dir = tempfile::tempdir().unwrap();
        let http = reqwest::Client::new();
        let stop_at = Utc::now() + chrono::Duration::seconds(30);
        let token = CancellationToken::new();

        let err =
            capture(&http, &format!("{base}/busy"), &dir.path().join("busy.ts"), stop_at, &token).await.unwrap_err();
        let msg = err.to_string();
        assert!(msg.starts_with("tuner: 503"), "{msg}");
        assert!(msg.contains("busy"), "{msg}");

        let dest = dir.path().join("short.ts");
        let res = capture(&http, &format!("{base}/short"), &dest, stop_at, &token).await.unwrap();
        assert_eq!(res.reason, StopReason::StreamEnded);
        assert_eq!(std::fs::read(&dest).unwrap(), b"hello ts");
        assert_eq!(res.bytes, 8);

        // unreachable tuner → connection error, not a panic
        let err =
            capture(&http, "http://127.0.0.1:1/auto/v1", &dir.path().join("x.ts"), stop_at, &token).await.unwrap_err();
        assert!(err.to_string().contains("tuner request"));
    }
}
