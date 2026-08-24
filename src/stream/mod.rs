// Copyright 2026 The Ontele Authors
// SPDX-License-Identifier: Apache-2.0

//! Playback sessions. One ffmpeg per HLS session writing 4 s segments into
//! `<data>/hls/<sid>/`; sessions idle for >90 s are killed and swept.
//! Seeking outside the transcoded range = a new session with `-ss` (the UI
//! offsets its timeline by `start_sec`). Transcodes (not copies) are bounded
//! by `Settings.max_transcodes`.
//!
//! Timestamps inside a session are *relative* (they start at 0 even when
//! `start_sec > 0`). hls.js copes badly with `-output_ts_offset`, so the
//! server never shifts them; the UI adds `offset` (= `start_sec`) to
//! `video.currentTime` to display absolute positions.

pub mod direct;
pub mod subtitles;

use crate::model::{HwAccel, PlaybackPlan, SegmentKind, Settings};
use crate::state::SettingsCache;
use axum::{
    Json,
    extract::Request,
    http::{HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use parking_lot::Mutex;
use serde_json::json;
use std::{
    collections::VecDeque,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio_util::sync::CancellationToken;

pub const SEGMENT_SECS: u32 = 4;
pub const IDLE_SECS: u64 = 90;

/// How long `start` waits for the first playable playlist.
const PLAYLIST_WAIT_FILE: Duration = Duration::from_secs(15);
const PLAYLIST_WAIT_LIVE: Duration = Duration::from_secs(30);
const PLAYLIST_POLL: Duration = Duration::from_millis(150);
/// How long `start` waits for a transcode slot before giving up.
const PERMIT_WAIT: Duration = Duration::from_secs(5);
/// Grace period for ffmpeg to exit after SIGKILL before we stop waiting.
const KILL_WAIT: Duration = Duration::from_secs(2);
/// Lines of ffmpeg stderr kept for error reports.
const STDERR_TAIL: usize = 30;
const GC_INTERVAL: Duration = Duration::from_secs(30);

#[derive(Debug, Clone)]
pub struct StartRequest {
    /// File path or live http URL.
    pub input: String,
    pub start_sec: f64,
    pub live: bool,
    pub plan: PlaybackPlan,
    /// Stream index (ffprobe `index`) of the audio track to use.
    pub audio_index: Option<u32>,
    /// Burn this embedded subtitle stream index (bitmap subs) into the video.
    pub burn_subtitle: Option<u32>,
    /// Burn an external subtitle file.
    pub burn_external: Option<PathBuf>,
    pub duration_sec: f64,
    /// Source video codec (normalized short name) — drives the fMP4 `hvc1` tag.
    pub vcodec: Option<String>,
    /// Source HDR format (hdr10 | hdr10plus | hlg | dv) — drives tone-mapping.
    pub hdr: Option<String>,
}

pub struct Session {
    pub id: String,
    pub dir: PathBuf,
    pub start_sec: f64,
    pub live: bool,
    pub transcode: bool,
    pub segment: SegmentKind,
    pub child: Mutex<Option<tokio::process::Child>>,
    pub last_access: Mutex<Instant>,
    pub created: Instant,
    pub _permit: Option<tokio::sync::OwnedSemaphorePermit>,
}

impl Session {
    pub fn touch(&self) {
        *self.last_access.lock() = Instant::now();
    }
    pub fn playlist_url(&self) -> String {
        format!("/stream/hls/{}/index.m3u8", self.id)
    }
    /// Seconds since the last playlist/segment request or keepalive.
    pub fn idle_secs(&self) -> u64 {
        self.last_access.lock().elapsed().as_secs()
    }
    /// Metric label for `ontele_streams_active`.
    fn mode_label(&self) -> &'static str {
        if self.live {
            "live"
        } else if self.transcode {
            "transcode"
        } else {
            "copy"
        }
    }
}

pub struct Manager {
    pub settings: Arc<SettingsCache>,
    pub cache_dir: PathBuf,
    sessions: dashmap::DashMap<String, Arc<Session>>,
    transcode_sem: Mutex<Arc<tokio::sync::Semaphore>>,
}

/// Shared ring buffer of the most recent ffmpeg stderr lines.
type StderrTail = Arc<Mutex<VecDeque<String>>>;

impl Manager {
    pub fn new(settings: Arc<SettingsCache>, cache_dir: PathBuf) -> Self {
        std::fs::create_dir_all(&cache_dir).ok();
        let n = settings.get().max_transcodes.max(1) as usize;
        Self {
            settings,
            cache_dir,
            sessions: dashmap::DashMap::new(),
            transcode_sem: Mutex::new(Arc::new(tokio::sync::Semaphore::new(n))),
        }
    }

    /// Spawn ffmpeg and wait (≤15 s) for the playlist to appear.
    pub async fn start(&self, req: StartRequest) -> anyhow::Result<Arc<Session>> {
        let set = self.settings.get();
        if req.input.trim().is_empty() {
            anyhow::bail!("empty stream input");
        }
        if !req.live && !is_url(&req.input) {
            let meta =
                tokio::fs::metadata(&req.input).await.map_err(|e| anyhow::anyhow!("media file unavailable: {e}"))?;
            if !meta.is_file() {
                anyhow::bail!("media path is not a file");
            }
        }
        if let Some(ext) = req.burn_external.as_deref()
            && !tokio::fs::metadata(ext).await.map(|m| m.is_file()).unwrap_or(false)
        {
            anyhow::bail!("subtitle file unavailable: {}", ext.display());
        }

        let transcode = req.plan.mode == "transcode";
        let permit = if transcode { Some(self.acquire_permit(set.max_transcodes).await?) } else { None };

        let id = crate::model::rand_id(8);
        let dir = self.cache_dir.join(&id);
        tokio::fs::create_dir_all(&dir).await?;

        let args = build_args(&set, &req, &dir);
        tracing::debug!(session = %id, ffmpeg = %set.ffmpeg_path, args = ?args, "spawning ffmpeg");

        let mut cmd = tokio::process::Command::new(&set.ffmpeg_path);
        cmd.args(&args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                let _ = tokio::fs::remove_dir_all(&dir).await;
                anyhow::bail!("cannot start ffmpeg ({}): {e}", set.ffmpeg_path);
            }
        };
        let tail: StderrTail = Arc::new(Mutex::new(VecDeque::new()));
        if let Some(stderr) = child.stderr.take() {
            tokio::spawn(drain_stderr(id.clone(), stderr, tail.clone()));
        }

        // Wait for a playlist with at least one segment (or ENDLIST).
        let wait = if req.live { PLAYLIST_WAIT_LIVE } else { PLAYLIST_WAIT_FILE };
        let playlist = dir.join("index.m3u8");
        let deadline = Instant::now() + wait;
        let mut failure: Option<String> = None;
        loop {
            if playlist_ready(&playlist).await {
                break;
            }
            match child.try_wait() {
                Ok(Some(status)) => {
                    // ffmpeg may legitimately finish a very short clip before we
                    // noticed the playlist; re-check once before failing.
                    if playlist_ready(&playlist).await {
                        break;
                    }
                    failure = Some(format!("ffmpeg exited early ({status})"));
                    break;
                }
                Ok(None) => {}
                Err(e) => {
                    failure = Some(format!("ffmpeg wait failed: {e}"));
                    break;
                }
            }
            if Instant::now() >= deadline {
                failure = Some(format!("ffmpeg produced no playable segments within {} s", wait.as_secs()));
                break;
            }
            tokio::time::sleep(PLAYLIST_POLL).await;
        }

        if let Some(msg) = failure {
            let _ = child.start_kill();
            let _ = tokio::time::timeout(KILL_WAIT, child.wait()).await;
            // give the stderr drain a moment to flush the last lines
            tokio::time::sleep(Duration::from_millis(50)).await;
            let lines: Vec<String> = tail.lock().iter().cloned().collect();
            let _ = tokio::fs::remove_dir_all(&dir).await;
            tracing::warn!(session = %id, input = %req.input, error = %msg, stderr = ?lines, "stream start failed");
            if lines.is_empty() {
                anyhow::bail!("{msg}");
            }
            anyhow::bail!("{msg}: {}", lines.join(" | "));
        }

        let sess = Arc::new(Session {
            id: id.clone(),
            dir,
            start_sec: req.start_sec,
            live: req.live,
            transcode,
            segment: req.plan.segment,
            child: Mutex::new(Some(child)),
            last_access: Mutex::new(Instant::now()),
            created: Instant::now(),
            _permit: permit,
        });
        self.sessions.insert(id.clone(), sess.clone());
        self.update_gauges();
        tracing::info!(
            session = %id, live = req.live, mode = %req.plan.mode, segment = ?req.plan.segment,
            start = req.start_sec, height = req.plan.height, "stream started"
        );
        Ok(sess)
    }

    /// Take a transcode slot, resizing the semaphore when `max_transcodes`
    /// changed and nothing currently holds a permit.
    async fn acquire_permit(&self, max_transcodes: u32) -> anyhow::Result<tokio::sync::OwnedSemaphorePermit> {
        let want = max_transcodes.max(1) as usize;
        let sem = {
            let mut guard = self.transcode_sem.lock();
            // Every OwnedSemaphorePermit holds an Arc clone, so a strong count
            // of 1 means no permit is outstanding and no start() is mid-flight.
            if Arc::strong_count(&guard) == 1 && guard.available_permits() != want {
                tracing::info!(old = guard.available_permits(), new = want, "transcode limit changed");
                *guard = Arc::new(tokio::sync::Semaphore::new(want));
            }
            guard.clone()
        };
        match tokio::time::timeout(PERMIT_WAIT, sem.acquire_owned()).await {
            Ok(Ok(p)) => Ok(p),
            Ok(Err(_)) => anyhow::bail!("transcode limiter closed"),
            Err(_) => {
                anyhow::bail!("transcode limit reached ({want} concurrent); try again later or lower the quality")
            }
        }
    }

    pub fn get(&self, id: &str) -> Option<Arc<Session>> {
        self.sessions.get(id).map(|s| s.clone())
    }

    /// Refresh idle timer; false if the session is gone.
    pub fn touch(&self, id: &str) -> bool {
        match self.sessions.get(id) {
            Some(s) => {
                s.touch();
                true
            }
            None => false,
        }
    }

    /// Kill + remove a session (and its scratch dir).
    pub fn stop(&self, id: &str) {
        let Some((_, sess)) = self.sessions.remove(id) else {
            return;
        };
        self.update_gauges();
        let child = sess.child.lock().take();
        let dir = sess.dir.clone();
        let sid = sess.id.clone();
        tracing::info!(session = %sid, seconds = sess.created.elapsed().as_secs(), live = sess.live, "stream stopped");
        match tokio::runtime::Handle::try_current() {
            Ok(h) => {
                h.spawn(async move {
                    teardown(&sid, child, &dir).await;
                });
            }
            Err(_) => {
                // No runtime (tests / shutdown path): best-effort synchronous cleanup.
                if let Some(mut c) = child {
                    let _ = c.start_kill();
                }
                let _ = std::fs::remove_dir_all(&dir);
            }
        }
    }

    /// Serve `GET /stream/hls/{sid}/{file}`: m3u8 (no-store) or segment, with
    /// path traversal rejected. 410 when the session is unknown.
    pub async fn serve(&self, sid: &str, file: &str, req: Request) -> Response {
        if !valid_segment_name(file) {
            return (StatusCode::BAD_REQUEST, Json(json!({ "error": "invalid segment name" }))).into_response();
        }
        let Some(sess) = self.get(sid) else {
            return (StatusCode::GONE, Json(json!({ "error": "session expired" }))).into_response();
        };
        sess.touch();
        let Some(mime) = hls_mime(file) else {
            return (StatusCode::NOT_FOUND, Json(json!({ "error": "unknown file type" }))).into_response();
        };
        let path = sess.dir.join(file);
        if !tokio::fs::metadata(&path).await.map(|m| m.is_file()).unwrap_or(false) {
            // Not produced yet — the player retries segment loads.
            return (StatusCode::NOT_FOUND, Json(json!({ "error": "segment not ready" }))).into_response();
        }
        let mut res = direct::serve_file(&path, mime, req).await;
        let cache = if file.ends_with(".m3u8") { "no-store" } else { "private, max-age=3600" };
        res.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static(cache));
        res
    }

    /// (sessions, transcodes) currently alive.
    pub fn active(&self) -> (usize, usize) {
        let t = self.sessions.iter().filter(|s| s.transcode).count();
        (self.sessions.len(), t)
    }

    /// Idle sweep every 30 s; kills everything on cancel.
    pub async fn gc_loop(self: Arc<Self>, cancel: CancellationToken) {
        self.sweep_stale_dirs().await;
        self.update_gauges();
        loop {
            tokio::select! {
                _ = cancel.cancelled() => break,
                _ = tokio::time::sleep(GC_INTERVAL) => {}
            }
            let idle: Vec<(String, u64)> = self
                .sessions
                .iter()
                .map(|s| (s.id.clone(), s.idle_secs()))
                .filter(|(_, idle)| *idle > IDLE_SECS)
                .collect();
            for (id, idle) in idle {
                tracing::info!(session = %id, idle_secs = idle, "reaping idle stream");
                self.stop(&id);
            }
            // Log unexpected ffmpeg exits once (the session itself lives on
            // until idle: finished VOD transcodes exit 0 and stay playable).
            for s in self.sessions.iter() {
                let status = {
                    let mut guard = s.child.lock();
                    match guard.as_mut().map(|c| c.try_wait()) {
                        Some(Ok(Some(status))) => {
                            *guard = None;
                            Some(status)
                        }
                        _ => None,
                    }
                };
                if let Some(status) = status {
                    if status.success() {
                        tracing::debug!(session = %s.id, "ffmpeg finished");
                    } else {
                        tracing::warn!(session = %s.id, status = %status, "ffmpeg exited with error");
                    }
                }
            }
            self.update_gauges();
        }
        // Shutdown: kill every child and drop scratch dirs.
        let all: Vec<Arc<Session>> = self.sessions.iter().map(|s| s.clone()).collect();
        self.sessions.clear();
        for s in all {
            let child = s.child.lock().take();
            teardown(&s.id, child, &s.dir).await;
        }
        self.update_gauges();
    }

    /// Remove leftover session dirs from a previous process (crash / kill -9).
    async fn sweep_stale_dirs(&self) {
        let Ok(mut rd) = tokio::fs::read_dir(&self.cache_dir).await else {
            return;
        };
        while let Ok(Some(ent)) = rd.next_entry().await {
            let name = ent.file_name().to_string_lossy().to_string();
            if self.sessions.contains_key(&name) {
                continue;
            }
            let is_dir = ent.file_type().await.map(|t| t.is_dir()).unwrap_or(false);
            if !is_dir {
                continue;
            }
            let path = ent.path();
            match tokio::fs::remove_dir_all(&path).await {
                Ok(()) => tracing::info!(dir = %path.display(), "removed stale stream dir"),
                Err(e) => {
                    tracing::warn!(dir = %path.display(), error = %e, "cannot remove stale stream dir")
                }
            }
        }
    }

    fn update_gauges(&self) {
        let (mut live, mut copy, mut transcode) = (0usize, 0usize, 0usize);
        for s in self.sessions.iter() {
            match s.mode_label() {
                "live" => live += 1,
                "transcode" => transcode += 1,
                _ => copy += 1,
            }
        }
        metrics::gauge!("ontele_streams_active", "mode" => "live").set(live as f64);
        metrics::gauge!("ontele_streams_active", "mode" => "copy").set(copy as f64);
        metrics::gauge!("ontele_streams_active", "mode" => "transcode").set(transcode as f64);
        let transcodes = self.sessions.iter().filter(|s| s.transcode).count();
        metrics::gauge!("ontele_transcodes_active").set(transcodes as f64);
    }
}

/// Kill a session's ffmpeg (bounded wait) and delete its directory.
async fn teardown(sid: &str, child: Option<tokio::process::Child>, dir: &Path) {
    if let Some(mut c) = child {
        let _ = c.start_kill();
        match tokio::time::timeout(KILL_WAIT, c.wait()).await {
            Ok(Ok(status)) => tracing::debug!(session = %sid, status = %status, "ffmpeg reaped"),
            Ok(Err(e)) => tracing::debug!(session = %sid, error = %e, "ffmpeg wait"),
            Err(_) => tracing::warn!(session = %sid, "ffmpeg did not exit within 2 s after kill"),
        }
    }
    if let Err(e) = tokio::fs::remove_dir_all(dir).await
        && e.kind() != std::io::ErrorKind::NotFound
    {
        tracing::warn!(session = %sid, dir = %dir.display(), error = %e, "cannot remove stream dir");
    }
}

/// Read ffmpeg's stderr line by line into a bounded tail; log on exit.
async fn drain_stderr(sid: String, stderr: tokio::process::ChildStderr, tail: StderrTail) {
    let mut lines = BufReader::new(stderr).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let line = line.trim().to_string();
        if line.is_empty() {
            continue;
        }
        tracing::debug!(session = %sid, "ffmpeg: {line}");
        let mut t = tail.lock();
        if t.len() >= STDERR_TAIL {
            t.pop_front();
        }
        t.push_back(line);
    }
    let lines: Vec<String> = tail.lock().iter().cloned().collect();
    if !lines.is_empty() {
        tracing::warn!(session = %sid, stderr = ?lines, "ffmpeg stderr at exit");
    }
}

/// True when the playlist exists and references a segment or is finished.
async fn playlist_ready(path: &Path) -> bool {
    match tokio::fs::read_to_string(path).await {
        Ok(s) => playlist_has_segment(&s),
        Err(_) => false,
    }
}

/// Pure check for [`playlist_ready`].
pub fn playlist_has_segment(m3u8: &str) -> bool {
    m3u8.lines().any(|l| {
        let l = l.trim();
        l.starts_with("#EXTINF") || l == "#EXT-X-ENDLIST"
    })
}

/// Only flat names made of `[A-Za-z0-9_.-]`, never starting with a dot, with
/// a known HLS extension. Rejects `/`, `..`, and anything exotic.
pub fn valid_segment_name(file: &str) -> bool {
    !file.is_empty()
        && file.len() <= 64
        && !file.starts_with('.')
        && !file.contains("..")
        && file.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
}

/// Content type by HLS file extension; `None` for anything we never emit.
pub fn hls_mime(file: &str) -> Option<&'static str> {
    let ext = file.rsplit_once('.').map(|(_, e)| e).unwrap_or("");
    match ext {
        "m3u8" => Some("application/vnd.apple.mpegurl"),
        "ts" => Some("video/mp2t"),
        "m4s" | "mp4" => Some("video/mp4"),
        "vtt" => Some("text/vtt"),
        _ => None,
    }
}

fn is_url(s: &str) -> bool {
    let l = s.to_ascii_lowercase();
    l.starts_with("http://") || l.starts_with("https://") || l.starts_with("rtsp://") || l.starts_with("udp://")
}

fn is_http(s: &str) -> bool {
    let l = s.to_ascii_lowercase();
    l.starts_with("http://") || l.starts_with("https://")
}

/// ffmpeg filtergraph escaping, level 1: the content of a filter option
/// value (`\`, `'` and the option separator `:`).
pub fn escape_filter_value(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        if matches!(c, '\\' | '\'' | ':') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// ffmpeg filtergraph escaping, level 2: the whole graph description
/// (`\`, `'`, `[`, `]`, `,`, `;`).
pub fn escape_filter_graph(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        if matches!(c, '\\' | '\'' | '[' | ']' | ',' | ';') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// A file path ready to be embedded as `subtitles=<path>` inside `-vf` /
/// `-filter_complex` (both escaping levels applied).
pub fn escape_subtitle_path(path: &Path) -> String {
    escape_filter_graph(&escape_filter_value(&path.to_string_lossy()))
}

/// `(maxrate, bufsize)` caps for the software/hardware h264 encoder by
/// output height (0 = keep source → assume 1080p).
pub fn bitrate_caps(height: u32) -> (&'static str, &'static str) {
    let h = if height == 0 { 1080 } else { height };
    match h {
        h if h >= 2160 => ("20M", "40M"),
        h if h >= 1440 => ("12M", "24M"),
        h if h >= 1080 => ("8M", "16M"),
        h if h >= 720 => ("4M", "8M"),
        h if h >= 480 => ("2M", "4M"),
        _ => ("1M", "2M"),
    }
}

const TONEMAP_FILTER: &str = "zscale=t=linear:npl=100,format=gbrpf32le,zscale=p=bt709,tonemap=tonemap=hable:desat=0,zscale=t=bt709:m=bt709:r=tv,format=yuv420p";

/// Build the ffmpeg argument list for a session (pure, tested).
/// `out_dir` receives `index.m3u8` + `s%05d.(ts|m4s)`.
///
/// Layout: global flags → input flags (seek / live probing / hw decode) →
/// `-i` → stream maps → video codec (+ filters) → audio codec → HLS muxer.
/// Timestamps stay relative to the seek point (see module docs).
pub fn build_args(set: &Settings, req: &StartRequest, out_dir: &std::path::Path) -> Vec<String> {
    let plan = &req.plan;
    let video_copy = plan.video_copy;
    let hdr = !video_copy && req.hdr.is_some();
    let burn_bitmap = (!video_copy).then_some(req.burn_subtitle).flatten();
    let burn_text = if video_copy { None } else { req.burn_external.as_deref() };
    let burning = burn_bitmap.is_some() || burn_text.is_some();
    // Hardware filter chains cannot host the subtitle/overlay/tonemap filters;
    // in those cases decode in software and only hand the frames to the hw
    // encoder at the very end (vaapi/qsv need an explicit upload).
    let hw = if video_copy { HwAccel::None } else { set.hwaccel };
    let hw_filters = hw != HwAccel::None && !burning && !hdr;
    let height = plan.height;
    let scale = height > 0;

    let mut a: Vec<String> = Vec::with_capacity(64);
    let mut push = |v: &[&str]| a.extend(v.iter().map(|s| s.to_string()));

    // ---- global ----
    push(&["-hide_banner", "-loglevel", "error", "-nostats", "-nostdin", "-y"]);

    // ---- input ----
    if req.live {
        push(&["-fflags", "+genpts+discardcorrupt", "-analyzeduration", "3M", "-probesize", "8M"]);
        if is_http(&req.input) {
            push(&["-reconnect", "1", "-reconnect_streamed", "1", "-reconnect_delay_max", "5"]);
        }
    } else if req.start_sec > 0.0 {
        // Fast seek (before -i): the demuxer jumps to the nearest keyframe.
        // With -c:v copy the first segment may start up to one GOP early;
        // the UI's offset is still `start_sec`, which is close enough.
        let ss = format!("{:.3}", req.start_sec);
        push(&["-ss", &ss]);
    }
    if hw_filters {
        match hw {
            HwAccel::Vaapi => {
                push(&["-hwaccel", "vaapi", "-hwaccel_output_format", "vaapi", "-vaapi_device", "/dev/dri/renderD128"])
            }
            HwAccel::Qsv => push(&["-hwaccel", "qsv", "-hwaccel_output_format", "qsv"]),
            HwAccel::Nvenc => push(&["-hwaccel", "cuda", "-hwaccel_output_format", "cuda"]),
            HwAccel::Videotoolbox => push(&["-hwaccel", "videotoolbox"]),
            HwAccel::None => {}
        }
    } else if hw == HwAccel::Vaapi {
        // software frames needed for filters, but still a hw encoder
        push(&["-vaapi_device", "/dev/dri/renderD128"]);
    }
    push(&["-i", &req.input]);

    // ---- maps ----
    // Video: via filter_complex when a bitmap subtitle is overlaid.
    let mut vf: Vec<String> = Vec::new();
    if !video_copy {
        if hw_filters && hw == HwAccel::Vaapi {
            // Works whether the decoder produced vaapi surfaces (pass-through)
            // or fell back to software frames (uploaded here).
            vf.push("format=nv12|vaapi,hwupload".into());
        }
        if req.live {
            vf.push(
                match (hw_filters, hw) {
                    (true, HwAccel::Vaapi) => "deinterlace_vaapi",
                    (true, HwAccel::Qsv) => "deinterlace_qsv",
                    (true, HwAccel::Nvenc) => "yadif_cuda",
                    _ => "yadif",
                }
                .to_string(),
            );
        }
        if hdr {
            vf.push(TONEMAP_FILTER.to_string());
        }
        if let Some(p) = burn_text {
            // `filename=` key form: a bare positional value is split on the
            // first '=' by ffmpeg's option scanner before any unescaping, so
            // paths like "[tmdbid=603]" would break the filter otherwise.
            let sub = format!("subtitles=filename={}", escape_subtitle_path(p));
            if !req.live && req.start_sec > 0.0 {
                // `-ss` before `-i` rebases frame timestamps to 0, but libass
                // renders by frame pts, so the burned text would lag by
                // start_sec. Shift pts back for the filter only, then undo it
                // so segments keep relative timestamps (module docs).
                let ss = format!("{:.3}", req.start_sec);
                vf.push(format!("setpts=PTS+{ss}/TB,{sub},setpts=PTS-{ss}/TB"));
            } else {
                vf.push(sub);
            }
        }
        if scale {
            vf.push(match (hw_filters, hw) {
                (true, HwAccel::Vaapi) => format!("scale_vaapi=w=-2:h={height}"),
                (true, HwAccel::Qsv) => format!("scale_qsv=w=-2:h={height}"),
                (true, HwAccel::Nvenc) => format!("scale_cuda=-2:{height}"),
                _ => format!("scale=-2:{height}"),
            });
        }
        // hand software frames to the hw encoder: vaapi needs an explicit
        // upload (the device comes from -vaapi_device); h264_qsv accepts
        // nv12 system-memory frames itself, and `hwupload` would have no
        // device on this path (no -hwaccel/-init_hw_device was given).
        match hw {
            HwAccel::Vaapi if !hw_filters => vf.push("format=nv12,hwupload".into()),
            HwAccel::Qsv if !hw_filters => vf.push("format=nv12".into()),
            _ => {}
        }
    }

    if let Some(idx) = burn_bitmap {
        // [0:v:0] → pre-overlay filters → overlay bitmap subtitle → post filters → [v]
        let graph = if vf.is_empty() {
            format!("[0:v:0][0:{idx}]overlay=eof_action=pass[v]")
        } else {
            // deinterlace / tonemap first, overlay at source resolution, then scale
            let (pre, post): (Vec<&String>, Vec<&String>) =
                vf.iter().partition(|f| !f.starts_with("scale") && !f.starts_with("format="));
            let mut g = String::new();
            let mut src = "[0:v:0]".to_string();
            if !pre.is_empty() {
                g.push_str(&format!("[0:v:0]{}[base];", pre.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(",")));
                src = "[base]".to_string();
            }
            g.push_str(&format!("{src}[0:{idx}]overlay=eof_action=pass"));
            for f in post {
                g.push(',');
                g.push_str(f);
            }
            g.push_str("[v]");
            g
        };
        push(&["-filter_complex", &graph, "-map", "[v]"]);
    } else {
        push(&["-map", "0:v:0"]);
        if !vf.is_empty() {
            let chain = vf.join(",");
            push(&["-vf", &chain]);
        }
    }
    // Audio: explicit ffprobe index, else the first track (optional so
    // silent sources still play).
    match req.audio_index {
        Some(i) => {
            let m = format!("0:{i}");
            push(&["-map", &m]);
        }
        None => push(&["-map", "0:a:0?"]),
    }
    push(&["-sn", "-dn"]);

    // ---- video codec ----
    if video_copy {
        push(&["-c:v", "copy"]);
        // Safari/hls.js need the `hvc1` sample entry for HEVC in fMP4.
        if plan.segment == SegmentKind::Fmp4 && req.vcodec.as_deref() == Some("hevc") {
            push(&["-tag:v", "hvc1"]);
        }
    } else {
        let (maxrate, bufsize) = bitrate_caps(height);
        match hw.encoder() {
            None => {
                let preset =
                    if set.transcode_preset.trim().is_empty() { "veryfast" } else { set.transcode_preset.trim() };
                push(&[
                    "-c:v", "libx264", "-preset", preset, "-crf", "21", "-maxrate", maxrate, "-bufsize", bufsize,
                    "-pix_fmt", "yuv420p",
                ]);
            }
            Some(enc) => {
                push(&["-c:v", enc]);
                match hw {
                    HwAccel::Nvenc => push(&["-preset", "p4", "-tune", "ll", "-rc", "vbr", "-cq", "23"]),
                    HwAccel::Videotoolbox => push(&["-realtime", "1", "-pix_fmt", "yuv420p"]),
                    HwAccel::Qsv => push(&["-global_quality", "23", "-look_ahead", "0"]),
                    HwAccel::Vaapi => push(&["-qp", "23"]),
                    HwAccel::None => {}
                }
                push(&["-maxrate", maxrate, "-bufsize", bufsize]);
            }
        }
        let kf = format!("expr:gte(t,n_forced*{SEGMENT_SECS})");
        push(&["-force_key_frames", &kf, "-sc_threshold", "0"]);
    }

    // ---- audio codec ----
    if plan.audio_copy {
        push(&["-c:a", "copy"]);
    } else {
        push(&["-c:a", "aac", "-ac", "2", "-b:a", "160k"]);
    }
    if req.live {
        push(&["-max_muxing_queue_size", "1024"]);
    }

    // ---- HLS muxer ----
    let hls_time = SEGMENT_SECS.to_string();
    push(&["-f", "hls", "-hls_time", &hls_time]);
    if req.live {
        push(&["-hls_list_size", "12", "-hls_flags", "delete_segments+independent_segments+temp_file"]);
    } else {
        push(&["-hls_playlist_type", "event", "-hls_list_size", "0", "-hls_flags", "independent_segments+temp_file"]);
    }
    let seg_ext = match plan.segment {
        SegmentKind::Fmp4 => {
            push(&["-hls_segment_type", "fmp4", "-hls_fmp4_init_filename", "init.mp4"]);
            "m4s"
        }
        SegmentKind::Ts => "ts",
    };
    let seg = out_dir.join(format!("s%05d.{seg_ext}")).to_string_lossy().to_string();
    let playlist = out_dir.join("index.m3u8").to_string_lossy().to_string();
    push(&["-hls_segment_filename", &seg, &playlist]);
    a
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;

    fn plan(mode: &str, video_copy: bool, audio_copy: bool, height: u32, segment: SegmentKind) -> PlaybackPlan {
        PlaybackPlan { mode: mode.into(), video_copy, audio_copy, height, segment, reasons: vec![] }
    }

    fn req(plan: PlaybackPlan) -> StartRequest {
        StartRequest {
            input: "/media/movie.mkv".into(),
            start_sec: 0.0,
            live: false,
            plan,
            audio_index: None,
            burn_subtitle: None,
            burn_external: None,
            duration_sec: 3600.0,
            vcodec: None,
            hdr: None,
        }
    }

    fn out() -> PathBuf {
        PathBuf::from("/data/hls/abc")
    }

    /// Value following the first occurrence of `flag`.
    fn after<'a>(args: &'a [String], flag: &str) -> Option<&'a str> {
        args.iter().position(|a| a == flag).map(|i| args[i + 1].as_str())
    }
    fn has(args: &[String], s: &str) -> bool {
        args.iter().any(|a| a == s)
    }
    fn pos(args: &[String], s: &str) -> usize {
        args.iter().position(|a| a == s).unwrap_or_else(|| panic!("missing {s} in {args:?}"))
    }
    fn count(args: &[String], s: &str) -> usize {
        args.iter().filter(|a| *a == s).count()
    }

    #[test]
    fn copy_ts_basic() {
        let set = Settings::default();
        let a = build_args(&set, &req(plan("copy", true, true, 0, SegmentKind::Ts)), &out());
        assert_eq!(&a[..6], &["-hide_banner", "-loglevel", "error", "-nostats", "-nostdin", "-y"]);
        assert_eq!(after(&a, "-c:v"), Some("copy"));
        assert_eq!(after(&a, "-c:a"), Some("copy"));
        assert!(!has(&a, "-vf"));
        assert!(!has(&a, "-ss"));
        assert!(!has(&a, "-force_key_frames"));
        assert!(!has(&a, "-hls_segment_type"));
        assert!(!has(&a, "-tag:v"));
        assert_eq!(after(&a, "-hls_playlist_type"), Some("event"));
        assert_eq!(after(&a, "-hls_list_size"), Some("0"));
        assert_eq!(after(&a, "-hls_flags"), Some("independent_segments+temp_file"));
        assert_eq!(after(&a, "-hls_segment_filename"), Some("/data/hls/abc/s%05d.ts"));
        assert_eq!(a.last().map(|s| s.as_str()), Some("/data/hls/abc/index.m3u8"));
        assert_eq!(after(&a, "-hls_time"), Some("4"));
        // audio defaults to the optional first track
        let maps: Vec<&str> =
            a.iter().enumerate().filter(|(_, s)| *s == "-map").map(|(i, _)| a[i + 1].as_str()).collect();
        assert_eq!(maps, vec!["0:v:0", "0:a:0?"]);
    }

    #[test]
    fn copy_fmp4_hevc_tag() {
        let set = Settings::default();
        let p = plan("copy", true, false, 0, SegmentKind::Fmp4);
        let mut r = req(p.clone());
        r.vcodec = Some("hevc".into());
        let a = build_args(&set, &r, &out());
        assert_eq!(after(&a, "-hls_segment_type"), Some("fmp4"));
        assert_eq!(after(&a, "-hls_fmp4_init_filename"), Some("init.mp4"));
        assert_eq!(after(&a, "-hls_segment_filename"), Some("/data/hls/abc/s%05d.m4s"));
        assert_eq!(after(&a, "-tag:v"), Some("hvc1"));
        // audio transcoded to stereo AAC
        let i = pos(&a, "-c:a");
        assert_eq!(&a[i..i + 6], &["-c:a", "aac", "-ac", "2", "-b:a", "160k"]);
        // av1 source → no tag
        r.vcodec = Some("av1".into());
        let a = build_args(&set, &r, &out());
        assert!(!has(&a, "-tag:v"));
    }

    #[test]
    fn transcode_software_720() {
        let set = Settings { transcode_preset: "fast".into(), ..Default::default() };
        let a = build_args(&set, &req(plan("transcode", false, false, 720, SegmentKind::Ts)), &out());
        assert_eq!(after(&a, "-c:v"), Some("libx264"));
        assert_eq!(after(&a, "-preset"), Some("fast"));
        assert_eq!(after(&a, "-crf"), Some("21"));
        assert_eq!(after(&a, "-maxrate"), Some("4M"));
        assert_eq!(after(&a, "-bufsize"), Some("8M"));
        assert_eq!(after(&a, "-pix_fmt"), Some("yuv420p"));
        assert_eq!(after(&a, "-vf"), Some("scale=-2:720"));
        assert_eq!(after(&a, "-force_key_frames"), Some("expr:gte(t,n_forced*4)"));
        assert_eq!(after(&a, "-sc_threshold"), Some("0"));
        assert!(!has(&a, "-hwaccel"));
        assert!(!has(&a, "yadif"));
        // -vf comes after -i and before the codec
        assert!(pos(&a, "-i") < pos(&a, "-vf"));
        assert!(pos(&a, "-vf") < pos(&a, "-c:v"));
    }

    #[test]
    fn transcode_keep_height_and_caps() {
        let set = Settings::default();
        let a = build_args(&set, &req(plan("transcode", false, false, 0, SegmentKind::Ts)), &out());
        assert!(!has(&a, "-vf"), "no scale when height == 0: {a:?}");
        assert_eq!(after(&a, "-maxrate"), Some("8M"));
        for (h, m) in [(2160, "20M"), (1440, "12M"), (1080, "8M"), (720, "4M"), (480, "2M"), (360, "1M"), (240, "1M")] {
            assert_eq!(bitrate_caps(h).0, m, "height {h}");
        }
    }

    #[test]
    fn start_offset_before_input() {
        let set = Settings::default();
        let mut r = req(plan("transcode", false, false, 1080, SegmentKind::Ts));
        r.start_sec = 1234.5678;
        let a = build_args(&set, &r, &out());
        assert_eq!(after(&a, "-ss"), Some("1234.568"));
        assert!(pos(&a, "-ss") < pos(&a, "-i"));
        assert!(!has(&a, "-output_ts_offset"));
        assert!(!has(&a, "-copyts"));
        // zero offset → no -ss at all
        r.start_sec = 0.0;
        let a = build_args(&set, &r, &out());
        assert!(!has(&a, "-ss"));
    }

    #[test]
    fn live_flags() {
        let set = Settings::default();
        let mut r = req(plan("transcode", false, false, 720, SegmentKind::Ts));
        r.live = true;
        r.input = "http://192.168.1.20:5004/auto/v5.1".into();
        r.start_sec = 99.0; // ignored for live
        let a = build_args(&set, &r, &out());
        assert!(!has(&a, "-ss"));
        assert_eq!(after(&a, "-analyzeduration"), Some("3M"));
        assert_eq!(after(&a, "-probesize"), Some("8M"));
        assert_eq!(after(&a, "-reconnect"), Some("1"));
        assert_eq!(after(&a, "-reconnect_streamed"), Some("1"));
        assert_eq!(after(&a, "-reconnect_delay_max"), Some("5"));
        assert!(pos(&a, "-reconnect") < pos(&a, "-i"));
        assert_eq!(after(&a, "-vf"), Some("yadif,scale=-2:720"));
        assert!(!has(&a, "-hls_playlist_type"));
        assert_eq!(after(&a, "-hls_list_size"), Some("12"));
        assert_eq!(after(&a, "-hls_flags"), Some("delete_segments+independent_segments+temp_file"));
        assert!(has(&a, "-max_muxing_queue_size"));
        // non-http live input gets no reconnect flags
        r.input = "udp://239.0.0.1:1234".into();
        let a = build_args(&set, &r, &out());
        assert!(!has(&a, "-reconnect"));
        assert!(has(&a, "-analyzeduration"));
    }

    #[test]
    fn explicit_audio_index() {
        let set = Settings::default();
        let mut r = req(plan("copy", true, true, 0, SegmentKind::Ts));
        r.audio_index = Some(3);
        let a = build_args(&set, &r, &out());
        let maps: Vec<&str> =
            a.iter().enumerate().filter(|(_, s)| *s == "-map").map(|(i, _)| a[i + 1].as_str()).collect();
        assert_eq!(maps, vec!["0:v:0", "0:3"]);
        assert!(!has(&a, "0:a:0?"));
    }

    #[test]
    fn hdr_tonemap_from_source_flag() {
        let set = Settings::default();
        let p = plan("transcode", false, false, 1080, SegmentKind::Ts);
        let mut r = req(p);
        r.hdr = Some("hdr10".into());
        let a = build_args(&set, &r, &out());
        let vf = after(&a, "-vf").unwrap();
        assert!(vf.starts_with("zscale=t=linear:npl=100,"), "{vf}");
        assert!(vf.contains("tonemap=tonemap=hable:desat=0"));
        assert!(vf.ends_with(",scale=-2:1080"), "{vf}");
        // copy mode never tone-maps even for an HDR source
        r.plan.video_copy = true;
        let a = build_args(&set, &r, &out());
        assert!(!has(&a, "-vf"));
        // SDR source → plain scale
        r.plan.video_copy = false;
        r.hdr = None;
        let a = build_args(&set, &r, &out());
        assert_eq!(after(&a, "-vf"), Some("scale=-2:1080"));
    }

    #[test]
    fn hwaccel_vaapi() {
        let set = Settings { hwaccel: HwAccel::Vaapi, ..Default::default() };
        let a = build_args(&set, &req(plan("transcode", false, false, 720, SegmentKind::Ts)), &out());
        assert_eq!(after(&a, "-hwaccel"), Some("vaapi"));
        assert_eq!(after(&a, "-hwaccel_output_format"), Some("vaapi"));
        assert_eq!(after(&a, "-vaapi_device"), Some("/dev/dri/renderD128"));
        assert!(pos(&a, "-hwaccel") < pos(&a, "-i"));
        assert_eq!(after(&a, "-vf"), Some("format=nv12|vaapi,hwupload,scale_vaapi=w=-2:h=720"));
        assert_eq!(after(&a, "-c:v"), Some("h264_vaapi"));
        assert!(!has(&a, "libx264"));
        assert!(!has(&a, "-crf"));
        assert!(!has(&a, "-pix_fmt"));
        assert_eq!(after(&a, "-maxrate"), Some("4M"));
        assert!(has(&a, "-force_key_frames"));
        // keep-height: still needs the upload chain so the encoder gets vaapi frames
        let a = build_args(&set, &req(plan("transcode", false, false, 0, SegmentKind::Ts)), &out());
        assert_eq!(after(&a, "-vf"), Some("format=nv12|vaapi,hwupload"));
        // live: hardware deinterlace
        let mut r = req(plan("transcode", false, false, 720, SegmentKind::Ts));
        r.live = true;
        r.input = "http://x/y".into();
        let a = build_args(&set, &r, &out());
        assert_eq!(after(&a, "-vf"), Some("format=nv12|vaapi,hwupload,deinterlace_vaapi,scale_vaapi=w=-2:h=720"));
        // copy mode: hwaccel is irrelevant
        let a = build_args(&set, &req(plan("copy", true, true, 0, SegmentKind::Ts)), &out());
        assert!(!has(&a, "-hwaccel"));
        assert!(!has(&a, "-vaapi_device"));
        assert_eq!(after(&a, "-c:v"), Some("copy"));
    }

    #[test]
    fn hwaccel_vaapi_falls_back_to_software_filters_when_burning() {
        let set = Settings { hwaccel: HwAccel::Vaapi, ..Default::default() };
        let mut r = req(plan("transcode", false, false, 720, SegmentKind::Ts));
        r.burn_external = Some(PathBuf::from("/media/movie.en.srt"));
        let a = build_args(&set, &r, &out());
        assert!(!has(&a, "-hwaccel"));
        assert_eq!(after(&a, "-vaapi_device"), Some("/dev/dri/renderD128"));
        assert_eq!(after(&a, "-vf"), Some("subtitles=filename=/media/movie.en.srt,scale=-2:720,format=nv12,hwupload"));
        assert_eq!(after(&a, "-c:v"), Some("h264_vaapi"));
        // qsv: software frames go straight to h264_qsv (no device for hwupload here)
        let qsv = Settings { hwaccel: HwAccel::Qsv, ..Default::default() };
        let a = build_args(&qsv, &r, &out());
        assert!(!has(&a, "-hwaccel"));
        assert_eq!(after(&a, "-vf"), Some("subtitles=filename=/media/movie.en.srt,scale=-2:720,format=nv12"));
        assert_eq!(after(&a, "-c:v"), Some("h264_qsv"));
    }

    #[test]
    fn hwaccel_qsv_nvenc_videotoolbox() {
        let qsv = Settings { hwaccel: HwAccel::Qsv, ..Default::default() };
        let a = build_args(&qsv, &req(plan("transcode", false, false, 1080, SegmentKind::Ts)), &out());
        assert_eq!(after(&a, "-hwaccel"), Some("qsv"));
        assert_eq!(after(&a, "-vf"), Some("scale_qsv=w=-2:h=1080"));
        assert_eq!(after(&a, "-c:v"), Some("h264_qsv"));

        let nv = Settings { hwaccel: HwAccel::Nvenc, ..Default::default() };
        let a = build_args(&nv, &req(plan("transcode", false, false, 480, SegmentKind::Ts)), &out());
        assert_eq!(after(&a, "-hwaccel"), Some("cuda"));
        assert_eq!(after(&a, "-hwaccel_output_format"), Some("cuda"));
        assert_eq!(after(&a, "-vf"), Some("scale_cuda=-2:480"));
        assert_eq!(after(&a, "-c:v"), Some("h264_nvenc"));
        assert_eq!(after(&a, "-preset"), Some("p4"));
        assert_eq!(after(&a, "-tune"), Some("ll"));
        assert_eq!(after(&a, "-maxrate"), Some("2M"));

        let vt = Settings { hwaccel: HwAccel::Videotoolbox, ..Default::default() };
        let a = build_args(&vt, &req(plan("transcode", false, false, 720, SegmentKind::Ts)), &out());
        assert_eq!(after(&a, "-hwaccel"), Some("videotoolbox"));
        assert!(!has(&a, "-hwaccel_output_format"));
        assert_eq!(after(&a, "-vf"), Some("scale=-2:720"));
        assert_eq!(after(&a, "-c:v"), Some("h264_videotoolbox"));
        assert_eq!(after(&a, "-realtime"), Some("1"));
    }

    #[test]
    fn burn_external_text_subtitle_escaped() {
        let set = Settings::default();
        let mut r = req(plan("transcode", false, false, 1080, SegmentKind::Ts));
        r.burn_external = Some(PathBuf::from("/m/Bob's Movie: Part 2, [Director's Cut].srt"));
        let a = build_args(&set, &r, &out());
        let vf = after(&a, "-vf").unwrap();
        assert_eq!(
            vf,
            "subtitles=filename=/m/Bob\\\\\\'s Movie\\\\: Part 2\\, \\[Director\\\\\\'s Cut\\].srt,scale=-2:1080"
        );
        assert!(!has(&a, "-filter_complex"));
        // '=' in the path is harmless with the filename= key form
        r.burn_external = Some(PathBuf::from("/m/Movie (1999) [tmdbid=603].srt"));
        let a = build_args(&set, &r, &out());
        assert_eq!(after(&a, "-vf"), Some("subtitles=filename=/m/Movie (1999) \\[tmdbid=603\\].srt,scale=-2:1080"));
    }

    #[test]
    fn burn_external_with_seek_shifts_pts_for_libass() {
        let set = Settings::default();
        let mut r = req(plan("transcode", false, false, 720, SegmentKind::Ts));
        r.burn_external = Some(PathBuf::from("/m/movie.srt"));
        r.start_sec = 600.25;
        let a = build_args(&set, &r, &out());
        assert_eq!(after(&a, "-ss"), Some("600.250"));
        assert_eq!(
            after(&a, "-vf"),
            Some("setpts=PTS+600.250/TB,subtitles=filename=/m/movie.srt,setpts=PTS-600.250/TB,scale=-2:720")
        );
        // no seek → no pts games
        r.start_sec = 0.0;
        let a = build_args(&set, &r, &out());
        assert_eq!(after(&a, "-vf"), Some("subtitles=filename=/m/movie.srt,scale=-2:720"));
    }

    #[test]
    fn burn_bitmap_uses_filter_complex() {
        let set = Settings::default();
        let mut r = req(plan("transcode", false, false, 720, SegmentKind::Ts));
        r.burn_subtitle = Some(4);
        let a = build_args(&set, &r, &out());
        assert!(!has(&a, "-vf"));
        assert_eq!(after(&a, "-filter_complex"), Some("[0:v:0][0:4]overlay=eof_action=pass,scale=-2:720[v]"));
        let maps: Vec<&str> =
            a.iter().enumerate().filter(|(_, s)| *s == "-map").map(|(i, _)| a[i + 1].as_str()).collect();
        assert_eq!(maps, vec!["[v]", "0:a:0?"]);
        assert_eq!(after(&a, "-c:v"), Some("libx264"));
        // live + bitmap: deinterlace before the overlay
        r.live = true;
        r.input = "http://x/y".into();
        let a = build_args(&set, &r, &out());
        assert_eq!(
            after(&a, "-filter_complex"),
            Some("[0:v:0]yadif[base];[base][0:4]overlay=eof_action=pass,scale=-2:720[v]")
        );
        // keep height + bitmap: plain overlay
        r.live = false;
        r.plan.height = 0;
        let a = build_args(&set, &r, &out());
        assert_eq!(after(&a, "-filter_complex"), Some("[0:v:0][0:4]overlay=eof_action=pass[v]"));
        // burn requests are ignored in copy mode
        let mut c = req(plan("copy", true, true, 0, SegmentKind::Ts));
        c.burn_subtitle = Some(4);
        let a = build_args(&set, &c, &out());
        assert!(!has(&a, "-filter_complex"));
        assert_eq!(after(&a, "-c:v"), Some("copy"));
    }

    #[test]
    fn escaping_levels() {
        assert_eq!(escape_filter_value("a:b"), "a\\:b");
        assert_eq!(escape_filter_value("a'b\\c"), "a\\'b\\\\c");
        assert_eq!(escape_filter_graph("a,b;[c]"), "a\\,b\\;\\[c\\]");
        assert_eq!(escape_subtitle_path(Path::new("/plain/path.srt")), "/plain/path.srt");
        assert_eq!(escape_subtitle_path(Path::new("C:/x")), "C\\\\:/x");
    }

    #[test]
    fn no_duplicate_flags() {
        let set = Settings { hwaccel: HwAccel::Vaapi, ..Default::default() };
        let mut r = req(plan("transcode", false, false, 720, SegmentKind::Fmp4));
        r.start_sec = 10.0;
        r.audio_index = Some(2);
        let a = build_args(&set, &r, &out());
        for f in ["-i", "-c:v", "-c:a", "-f", "-vf", "-hls_time", "-hls_segment_filename", "-ss"] {
            assert_eq!(count(&a, f), 1, "{f} appears {} times", count(&a, f));
        }
    }

    #[test]
    fn segment_name_validation() {
        for ok in ["index.m3u8", "s00001.ts", "s00001.m4s", "init.mp4", "a-b_c.ts"] {
            assert!(valid_segment_name(ok), "{ok}");
        }
        for bad in ["", "../x.ts", "a/b.ts", ".hidden", "s00001.ts/..", "x\\y", "s 1.ts", "s%05d.ts", &"a".repeat(65)] {
            assert!(!valid_segment_name(bad), "{bad}");
        }
        assert_eq!(hls_mime("index.m3u8"), Some("application/vnd.apple.mpegurl"));
        assert_eq!(hls_mime("s00001.ts"), Some("video/mp2t"));
        assert_eq!(hls_mime("s00001.m4s"), Some("video/mp4"));
        assert_eq!(hls_mime("init.mp4"), Some("video/mp4"));
        assert_eq!(hls_mime("evil.sh"), None);
        assert_eq!(hls_mime("noext"), None);
    }

    #[test]
    fn playlist_readiness() {
        assert!(!playlist_has_segment(""));
        assert!(!playlist_has_segment("#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:4\n"));
        assert!(playlist_has_segment("#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXTINF:4.000000,\ns00000.ts\n"));
        assert!(playlist_has_segment("#EXTM3U\n#EXT-X-ENDLIST\n"));
    }

    #[tokio::test]
    async fn serve_rejects_traversal_and_unknown_session() {
        let dir = tempfile::tempdir().unwrap();
        let pool = sqlx::PgPool::connect_lazy("postgres://localhost/unused").unwrap();
        let settings = Arc::new(SettingsCache::new(pool, Settings::default()));
        let m = Manager::new(settings, dir.path().join("hls"));
        let r = m.serve("abc", "../etc/passwd", Request::new(Body::empty())).await;
        assert_eq!(r.status(), StatusCode::BAD_REQUEST);
        let r = m.serve("abc", "index.m3u8", Request::new(Body::empty())).await;
        assert_eq!(r.status(), StatusCode::GONE);
        assert!(!m.touch("abc"));
        assert_eq!(m.active(), (0, 0));
        m.stop("nope"); // no-op
    }

    #[tokio::test]
    async fn serve_session_files() {
        let dir = tempfile::tempdir().unwrap();
        let pool = sqlx::PgPool::connect_lazy("postgres://localhost/unused").unwrap();
        let settings = Arc::new(SettingsCache::new(pool, Settings::default()));
        let m = Manager::new(settings, dir.path().join("hls"));
        let sdir = m.cache_dir.join("sess1");
        std::fs::create_dir_all(&sdir).unwrap();
        std::fs::write(sdir.join("index.m3u8"), "#EXTM3U\n#EXTINF:4.0,\ns00000.ts\n").unwrap();
        std::fs::write(sdir.join("s00000.ts"), vec![0x47u8; 188 * 3]).unwrap();
        m.sessions.insert(
            "sess1".into(),
            Arc::new(Session {
                id: "sess1".into(),
                dir: sdir.clone(),
                start_sec: 0.0,
                live: false,
                transcode: false,
                segment: SegmentKind::Ts,
                child: Mutex::new(None),
                last_access: Mutex::new(Instant::now()),
                created: Instant::now(),
                _permit: None,
            }),
        );
        let r = m.serve("sess1", "index.m3u8", Request::new(Body::empty())).await;
        assert_eq!(r.status(), StatusCode::OK);
        assert_eq!(r.headers().get(header::CONTENT_TYPE).unwrap(), "application/vnd.apple.mpegurl");
        assert_eq!(r.headers().get(header::CACHE_CONTROL).unwrap(), "no-store");
        let r = m.serve("sess1", "s00000.ts", Request::new(Body::empty())).await;
        assert_eq!(r.status(), StatusCode::OK);
        assert_eq!(r.headers().get(header::CONTENT_TYPE).unwrap(), "video/mp2t");
        assert_eq!(r.headers().get(header::ACCEPT_RANGES).unwrap(), "bytes");
        let r = m.serve("sess1", "s00009.ts", Request::new(Body::empty())).await;
        assert_eq!(r.status(), StatusCode::NOT_FOUND);
        let r = m.serve("sess1", "run.sh", Request::new(Body::empty())).await;
        assert_eq!(r.status(), StatusCode::NOT_FOUND);
        assert_eq!(m.active(), (1, 0));
        m.stop("sess1");
        assert_eq!(m.active(), (0, 0));
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(!sdir.exists(), "session dir removed");
    }

    /// End-to-end: transcode a synthesized clip (needs ffmpeg on PATH).
    #[tokio::test]
    async fn start_transcodes_fixture() {
        let Ok(ffmpeg) = which_ffmpeg() else { return };
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("clip.mp4");
        let st = std::process::Command::new(&ffmpeg)
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-y",
                "-f",
                "lavfi",
                "-i",
                "testsrc=size=320x180:rate=25",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440",
                "-t",
                "3",
                "-c:v",
                "libx264",
                "-preset",
                "ultrafast",
                "-pix_fmt",
                "yuv420p",
                "-c:a",
                "aac",
            ])
            .arg(&src)
            .status()
            .unwrap();
        if !st.success() {
            return;
        }
        let pool = sqlx::PgPool::connect_lazy("postgres://localhost/unused").unwrap();
        let settings = Arc::new(SettingsCache::new(
            pool,
            Settings {
                ffmpeg_path: ffmpeg.clone(),
                transcode_preset: "ultrafast".into(),
                max_transcodes: 1,
                ..Default::default()
            },
        ));
        let m = Arc::new(Manager::new(settings, dir.path().join("hls")));
        let mut r = req(plan("transcode", false, false, 180, SegmentKind::Ts));
        r.input = src.to_string_lossy().to_string();
        r.start_sec = 1.0;
        r.duration_sec = 3.0;
        let s = m.start(r.clone()).await.expect("start");
        assert!(s.transcode);
        assert_eq!(m.active(), (1, 1));
        let res = m.serve(&s.id, "index.m3u8", Request::new(Body::empty())).await;
        assert_eq!(res.status(), StatusCode::OK);
        // the permit is held: a second transcode must wait/fail quickly → we stop first
        m.stop(&s.id);
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(m.active(), (0, 0));
        // bad input fails with stderr in the message
        r.input = dir.path().join("missing.mkv").to_string_lossy().to_string();
        let err = match m.start(r).await {
            Ok(_) => panic!("missing input must fail"),
            Err(e) => e.to_string(),
        };
        assert!(err.contains("unavailable"), "{err}");
    }

    fn which_ffmpeg() -> Result<String, ()> {
        for c in ["ffmpeg", "/opt/homebrew/bin/ffmpeg", "/usr/local/bin/ffmpeg", "/usr/bin/ffmpeg"] {
            if std::process::Command::new(c)
                .arg("-version")
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .map(|s| s.success())
                .unwrap_or(false)
            {
                return Ok(c.to_string());
            }
        }
        Err(())
    }
}
