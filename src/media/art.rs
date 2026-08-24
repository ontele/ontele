// Copyright 2026 The Ontele Authors
// SPDX-License-Identifier: Apache-2.0

//! Artwork cache. Resolution order for an item: sidecar files in the folder
//! (`poster.jpg`, `folder.jpg`, `cover.jpg`, `fanart.jpg`, `<stem>-poster.jpg`),
//! embedded pictures (music tags, MKV attachments), provider URLs in
//! `meta.posterUrl`/`backdropUrl`/`stillUrl`, then an ffmpeg frame grab.
//! Outputs are JPEGs under `<data>/img/<key>-<kind>[-w<width>].jpg`;
//! concurrent requests for the same key collapse into one generation.
//!
//! Keys: an item id, `show:<name>` (show poster/backdrop from show metadata
//! or its first episode) or `album:<album_id>` (embedded cover of a track).
//!
//! Scrub sprites: `sprites(id)` renders a `fps=1/N` tile sheet + WebVTT
//! (`#xywh=`) for the player's thumbnail preview.

use crate::{
    db,
    model::{Item, Kind},
    state::SettingsCache,
};
use sqlx::PgPool;
use std::{
    ffi::OsStr,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

/// Largest provider image we are willing to fetch.
const DOWNLOAD_MAX: u64 = 20 * 1024 * 1024;
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(20);
const FFMPEG_TIMEOUT: Duration = Duration::from_secs(120);
const SPRITES_TIMEOUT: Duration = Duration::from_secs(600);
/// Tile geometry of the sprite sheet.
pub const SPRITE_W: u32 = 160;
pub const SPRITE_H: u32 = 90;
pub const SPRITE_COLS: u32 = 10;
pub const SPRITE_MAX_TILES: u32 = 600;
pub const SPRITE_MIN_DURATION: f64 = 30.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ArtKind {
    Poster,
    Backdrop,
    Thumb,
    Still,
}

impl ArtKind {
    pub fn parse(s: &str) -> ArtKind {
        match s {
            "backdrop" => ArtKind::Backdrop,
            "thumb" => ArtKind::Thumb,
            "still" => ArtKind::Still,
            _ => ArtKind::Poster,
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            ArtKind::Poster => "poster",
            ArtKind::Backdrop => "backdrop",
            ArtKind::Thumb => "thumb",
            ArtKind::Still => "still",
        }
    }
    /// Output width when the caller does not ask for one.
    pub fn default_width(self) -> u32 {
        match self {
            ArtKind::Poster => 480,
            ArtKind::Backdrop => 1280,
            ArtKind::Thumb => 320,
            ArtKind::Still => 640,
        }
    }
    /// Where in the file a frame grab for this kind is taken from.
    pub fn frame_time(self, duration: f64) -> f64 {
        if !(duration.is_finite() && duration > 0.0) {
            return 1.0;
        }
        let t = match self {
            ArtKind::Poster => duration * 0.2,
            ArtKind::Backdrop => duration * 0.4,
            ArtKind::Thumb => duration * 0.1,
            ArtKind::Still => duration * 0.3,
        };
        // never seek into the last second — encoders may have no frame there
        t.min((duration - 1.0).max(0.0))
    }
}

/// File-system-safe form of a cache key. Item/album ids pass through; show
/// names get every non `[A-Za-z0-9._-]` byte replaced and a short hash
/// suffix so distinct names never collide.
pub fn safe_key(key: &str) -> String {
    let mut out = String::with_capacity(key.len() + 9);
    let mut replaced = false;
    for c in key.chars().take(96) {
        if c.is_ascii_alphanumeric() || c == '_' || c == '-' || (c == '.' && !out.is_empty()) {
            out.push(c);
        } else {
            out.push('_');
            replaced = true;
        }
    }
    if key.chars().count() > 96 {
        replaced = true;
    }
    if replaced || out.is_empty() {
        let h = blake3::hash(key.as_bytes()).to_hex();
        out.push('_');
        out.push_str(&h[..10]);
    }
    out
}

/// `<safe key>-<kind>[-w<width>].jpg`
pub fn cache_name(key: &str, kind: ArtKind, width: Option<u32>) -> String {
    match width {
        Some(w) => format!("{}-{}-w{}.jpg", safe_key(key), kind.as_str(), w),
        None => format!("{}-{}.jpg", safe_key(key), kind.as_str()),
    }
}

// ---- sprites (pure) --------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpritePlan {
    /// Seconds between tiles.
    pub interval: u32,
    /// Number of tiles (≤ `SPRITE_MAX_TILES`).
    pub count: u32,
    pub rows: u32,
}

/// interval = clamp(round(duration / 400), 5, 30) s, stretched when the
/// sheet would exceed `SPRITE_MAX_TILES`. `None` for clips under 30 s.
pub fn sprite_plan(duration: f64) -> Option<SpritePlan> {
    if !(duration.is_finite()) || duration < SPRITE_MIN_DURATION {
        return None;
    }
    let mut interval = ((duration / 400.0).round() as u32).clamp(5, 30);
    let tiles = |iv: u32| (duration / iv as f64).floor() as u32 + 1;
    let mut count = tiles(interval);
    if count > SPRITE_MAX_TILES {
        interval = (duration / (SPRITE_MAX_TILES - 1) as f64).ceil() as u32;
        count = tiles(interval).min(SPRITE_MAX_TILES);
    }
    let rows = count.div_ceil(SPRITE_COLS);
    Some(SpritePlan { interval, count, rows })
}

fn vtt_time(secs: f64) -> String {
    let total_ms = (secs.max(0.0) * 1000.0).round() as u64;
    let h = total_ms / 3_600_000;
    let m = (total_ms / 60_000) % 60;
    let s = (total_ms / 1000) % 60;
    let ms = total_ms % 1000;
    format!("{h:02}:{m:02}:{s:02}.{ms:03}")
}

/// WebVTT with one cue per tile: `sprites.jpg#xywh=x,y,160,90`.
pub fn sprite_vtt(duration: f64, plan: SpritePlan) -> String {
    let mut out = String::from("WEBVTT\n\n");
    for i in 0..plan.count {
        let start = (i * plan.interval) as f64;
        if start >= duration {
            break;
        }
        let end = (((i + 1) * plan.interval) as f64).min(duration);
        let x = (i % SPRITE_COLS) * SPRITE_W;
        let y = (i / SPRITE_COLS) * SPRITE_H;
        out.push_str(&format!(
            "{} --> {}\nsprites.jpg#xywh={x},{y},{SPRITE_W},{SPRITE_H}\n\n",
            vtt_time(start),
            vtt_time(end)
        ));
    }
    out
}

// ---- ffmpeg helpers (pure async, testable without a DB) ------------------------------

fn stderr_tail(bytes: &[u8]) -> String {
    let s = String::from_utf8_lossy(bytes);
    let s = s.trim();
    let tail: Vec<&str> = s.lines().rev().take(4).collect();
    tail.into_iter().rev().collect::<Vec<_>>().join(" | ")
}

/// Run ffmpeg with `args`, bounded by `timeout`; the child is killed on
/// timeout or drop. Errors carry the last stderr lines.
pub async fn run_ffmpeg<I, S>(ffmpeg: &str, args: I, timeout: Duration) -> anyhow::Result<()>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let mut cmd = Command::new(ffmpeg);
    cmd.arg("-hide_banner")
        .arg("-loglevel")
        .arg("error")
        .arg("-nostdin")
        .arg("-y")
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    let child = cmd.spawn().map_err(|e| anyhow::anyhow!("cannot start {ffmpeg}: {e}"))?;
    match tokio::time::timeout(timeout, child.wait_with_output()).await {
        Ok(Ok(out)) if out.status.success() => Ok(()),
        Ok(Ok(out)) => anyhow::bail!("ffmpeg failed ({}): {}", out.status, stderr_tail(&out.stderr)),
        Ok(Err(e)) => anyhow::bail!("ffmpeg: {e}"),
        Err(_) => anyhow::bail!("ffmpeg timed out after {}s", timeout.as_secs()),
    }
}

fn scale_filter(width: u32) -> String {
    format!("scale={width}:-2:flags=lanczos,format=yuvj420p")
}

/// Convert/resize any image ffmpeg can read to a JPEG `width` px wide.
pub async fn convert_image(ffmpeg: &str, src: &Path, width: u32, out: &Path) -> anyhow::Result<()> {
    run_ffmpeg(
        ffmpeg,
        [
            OsStr::new("-i"),
            src.as_os_str(),
            OsStr::new("-vf"),
            OsStr::new(&scale_filter(width)),
            OsStr::new("-q:v"),
            OsStr::new("3"),
            OsStr::new("-frames:v"),
            OsStr::new("1"),
            OsStr::new("-f"),
            OsStr::new("image2"),
            out.as_os_str(),
        ],
        FFMPEG_TIMEOUT,
    )
    .await?;
    ensure_output(out)
}

/// Grab one frame at `at` seconds as a JPEG `width` px wide. Falls back to
/// the first frame when seeking fails (duration over-estimated, etc.).
pub async fn grab_frame(ffmpeg: &str, src: &Path, at: f64, width: u32, out: &Path) -> anyhow::Result<()> {
    let attempt = |ss: f64| {
        let ss = format!("{:.3}", ss.max(0.0));
        let vf = scale_filter(width);
        async move {
            run_ffmpeg(
                ffmpeg,
                [
                    OsStr::new("-ss"),
                    OsStr::new(&ss),
                    OsStr::new("-i"),
                    src.as_os_str(),
                    OsStr::new("-map"),
                    OsStr::new("0:v:0"),
                    OsStr::new("-an"),
                    OsStr::new("-sn"),
                    OsStr::new("-vf"),
                    OsStr::new(&vf),
                    OsStr::new("-frames:v"),
                    OsStr::new("1"),
                    OsStr::new("-q:v"),
                    OsStr::new("3"),
                    OsStr::new("-f"),
                    OsStr::new("image2"),
                    out.as_os_str(),
                ],
                FFMPEG_TIMEOUT,
            )
            .await?;
            ensure_output(out)
        }
    };
    match attempt(at).await {
        Ok(()) => Ok(()),
        Err(first) if at > 0.0 => {
            tracing::debug!(error = %first, at, "frame grab failed, retrying at start");
            attempt(0.0).await.map_err(|_| first)
        }
        Err(e) => Err(e),
    }
}

/// Flat 1:1 placeholder (audio without a cover) so the UI always gets a JPEG.
pub async fn placeholder(ffmpeg: &str, size: u32, out: &Path) -> anyhow::Result<()> {
    let size = size.clamp(16, 2560);
    run_ffmpeg(
        ffmpeg,
        [
            OsStr::new("-f"),
            OsStr::new("lavfi"),
            OsStr::new("-i"),
            OsStr::new(&format!("color=c=0x14161f:s={size}x{size}:d=1")),
            OsStr::new("-frames:v"),
            OsStr::new("1"),
            OsStr::new("-q:v"),
            OsStr::new("3"),
            OsStr::new("-f"),
            OsStr::new("image2"),
            out.as_os_str(),
        ],
        FFMPEG_TIMEOUT,
    )
    .await?;
    ensure_output(out)
}

/// Render the tile sheet for `plan`. Tiles are letterboxed to exactly
/// 160x90 so the VTT geometry holds for any aspect ratio.
pub async fn render_sprites(ffmpeg: &str, src: &Path, plan: SpritePlan, out: &Path) -> anyhow::Result<()> {
    let vf = format!(
        "fps=1/{iv},scale={w}:{h}:force_original_aspect_ratio=decrease:flags=fast_bilinear,pad={w}:{h}:-1:-1:color=black,tile={cols}x{rows},format=yuvj420p",
        iv = plan.interval,
        w = SPRITE_W,
        h = SPRITE_H,
        cols = SPRITE_COLS,
        rows = plan.rows.max(1)
    );
    run_ffmpeg(
        ffmpeg,
        [
            OsStr::new("-skip_frame"),
            OsStr::new("nokey"),
            OsStr::new("-i"),
            src.as_os_str(),
            OsStr::new("-map"),
            OsStr::new("0:v:0"),
            OsStr::new("-an"),
            OsStr::new("-sn"),
            OsStr::new("-vf"),
            OsStr::new(&vf),
            OsStr::new("-frames:v"),
            OsStr::new("1"),
            OsStr::new("-q:v"),
            OsStr::new("4"),
            OsStr::new("-f"),
            OsStr::new("image2"),
            out.as_os_str(),
        ],
        SPRITES_TIMEOUT,
    )
    .await?;
    ensure_output(out)
}

fn ensure_output(p: &Path) -> anyhow::Result<()> {
    match std::fs::metadata(p) {
        Ok(m) if m.len() > 0 => Ok(()),
        _ => anyhow::bail!("ffmpeg produced no output"),
    }
}

fn is_nonempty_file(p: &Path) -> bool {
    std::fs::metadata(p).map(|m| m.is_file() && m.len() > 0).unwrap_or(false)
}

/// A scratch file removed on drop.
struct TempFile(PathBuf);

impl Drop for TempFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Where the bytes for a render come from.
enum Source {
    /// An existing image on disk (sidecar).
    File(PathBuf),
    /// A downloaded / extracted image in a scratch file.
    Temp(TempFile),
    /// Grab a frame from a video.
    Frame { path: PathBuf, at: f64 },
    /// Flat colour (audio without a cover).
    Placeholder,
}

/// Removes the inflight entry + wakes waiters when generation ends (also on
/// panic/cancellation).
struct Inflight<'a> {
    map: &'a dashmap::DashMap<String, Arc<tokio::sync::Notify>>,
    name: String,
    notify: Arc<tokio::sync::Notify>,
}

impl Drop for Inflight<'_> {
    fn drop(&mut self) {
        self.map.remove(&self.name);
        self.notify.notify_waiters();
    }
}

pub struct Art {
    pub pool: PgPool,
    pub settings: Arc<SettingsCache>,
    pub cache_dir: PathBuf,
    pub http: reqwest::Client,
    inflight: dashmap::DashMap<String, Arc<tokio::sync::Notify>>,
    /// Keys that resolved to "no artwork" recently — answered instantly
    /// instead of re-reading tags / re-probing on every card render.
    misses: dashmap::DashMap<String, std::time::Instant>,
}

const MISS_TTL: std::time::Duration = std::time::Duration::from_secs(600);

impl Art {
    pub fn new(pool: PgPool, settings: Arc<SettingsCache>, cache_dir: PathBuf, http: reqwest::Client) -> Self {
        std::fs::create_dir_all(&cache_dir).ok();
        Self { pool, settings, cache_dir, http, inflight: dashmap::DashMap::new(), misses: dashmap::DashMap::new() }
    }

    fn ffmpeg(&self) -> String {
        let s = self.settings.get();
        if s.ffmpeg_path.trim().is_empty() { "ffmpeg".into() } else { s.ffmpeg_path.clone() }
    }

    fn scratch(&self, ext: &str) -> PathBuf {
        self.cache_dir.join(format!(".tmp-{}.{ext}", crate::model::rand_id(8)))
    }

    /// Become the single generator for `name`, waiting for any in-progress
    /// generation of the same name to finish first.
    async fn acquire(&self, name: &str) -> Inflight<'_> {
        enum Slot {
            Mine(Arc<tokio::sync::Notify>),
            Wait(Arc<tokio::sync::Notify>),
        }
        loop {
            // The entry guard (a shard lock) is released before any await.
            let slot = match self.inflight.entry(name.to_string()) {
                dashmap::mapref::entry::Entry::Occupied(e) => Slot::Wait(e.get().clone()),
                dashmap::mapref::entry::Entry::Vacant(v) => {
                    let n = Arc::new(tokio::sync::Notify::new());
                    v.insert(n.clone());
                    Slot::Mine(n)
                }
            };
            match slot {
                Slot::Mine(notify) => {
                    return Inflight { map: &self.inflight, name: name.to_string(), notify };
                }
                Slot::Wait(n) => {
                    // Bounded wait: if the generator finished between our lookup and the
                    // registration, the loop re-checks instead of hanging.
                    let _ = tokio::time::timeout(Duration::from_millis(500), n.notified()).await;
                }
            }
        }
    }

    /// Cached JPEG for `key`/`kind`, generated on first request.
    /// `width` (optional) resizes; default widths: poster 480, backdrop 1280,
    /// thumb 320, still 640.
    pub async fn path(&self, key: &str, kind: ArtKind, width: Option<u32>) -> anyhow::Result<PathBuf> {
        let key = key.trim();
        if key.is_empty() || key.len() > 512 {
            anyhow::bail!("invalid art key");
        }
        let width = width.filter(|w| (16..=2560).contains(w));
        let name = cache_name(key, kind, width);
        let file = self.cache_dir.join(&name);
        if is_nonempty_file(&file) {
            return Ok(file);
        }
        let miss_key = format!("{key}|{}", kind.as_str());
        if let Some(at) = self.misses.get(&miss_key)
            && at.elapsed() < MISS_TTL
        {
            anyhow::bail!("no artwork available for {key}");
        }
        let _guard = self.acquire(&name).await;
        if is_nonempty_file(&file) {
            return Ok(file);
        }
        std::fs::create_dir_all(&self.cache_dir).ok();
        if let Err(e) = self.generate(key, kind, width.unwrap_or(kind.default_width()), &file).await {
            self.misses.insert(miss_key, std::time::Instant::now());
            return Err(e);
        }
        Ok(file)
    }

    async fn generate(&self, key: &str, kind: ArtKind, width: u32, out: &Path) -> anyhow::Result<()> {
        let source = self.resolve(key, kind).await?;
        let ffmpeg = self.ffmpeg();
        let tmp = TempFile(self.scratch("jpg"));
        match &source {
            Source::File(p) => convert_image(&ffmpeg, p, width, &tmp.0).await?,
            Source::Temp(t) => convert_image(&ffmpeg, &t.0, width, &tmp.0).await?,
            Source::Frame { path, at } => grab_frame(&ffmpeg, path, *at, width, &tmp.0).await?,
            // No artwork at all: answer 404 so the UI renders its text
            // fallback instead of caching a flat grey square.
            Source::Placeholder => anyhow::bail!("no artwork available for {key}"),
        }
        std::fs::rename(&tmp.0, out).map_err(|e| anyhow::anyhow!("cannot move art into cache: {e}"))?;
        tracing::debug!(key, kind = kind.as_str(), width, "art rendered");
        Ok(())
    }

    async fn resolve(&self, key: &str, kind: ArtKind) -> anyhow::Result<Source> {
        if let Some(show) = key.strip_prefix("show:") {
            return self.resolve_show(show, kind).await;
        }
        if let Some(album) = key.strip_prefix("album:") {
            return self.resolve_album(album, kind).await;
        }
        let item = db::items::get(&self.pool, 0, key).await?.ok_or_else(|| anyhow::anyhow!("unknown item {key}"))?;
        self.resolve_item(&item, kind, true).await
    }

    async fn resolve_item(&self, item: &Item, kind: ArtKind, show_fallback: bool) -> anyhow::Result<Source> {
        let path = item
            .path
            .as_deref()
            .filter(|p| !p.is_empty())
            .ok_or_else(|| anyhow::anyhow!("item {} has no file", item.id))?;
        let path = PathBuf::from(path);
        if item.kind == Some(Kind::Track) {
            return self.resolve_track(item, &path).await;
        }
        let dir = path.parent().map(Path::to_path_buf).unwrap_or_default();
        let stem = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        let meta = item.meta.as_ref();

        let (sidecars, url): (Vec<String>, Option<&str>) = match kind {
            ArtKind::Poster => (
                vec![
                    "poster.jpg".into(),
                    "poster.png".into(),
                    "folder.jpg".into(),
                    "cover.jpg".into(),
                    format!("{stem}-poster.jpg"),
                    format!("{stem}-poster.png"),
                ],
                meta.and_then(|m| m.poster_url.as_deref()),
            ),
            ArtKind::Backdrop => (
                vec![
                    "fanart.jpg".into(),
                    "fanart.png".into(),
                    "backdrop.jpg".into(),
                    format!("{stem}-fanart.jpg"),
                    format!("{stem}-backdrop.jpg"),
                ],
                meta.and_then(|m| m.backdrop_url.as_deref()),
            ),
            ArtKind::Thumb => (
                vec![format!("{stem}-thumb.jpg"), format!("{stem}-thumb.png")],
                meta.and_then(|m| m.still_url.as_deref()),
            ),
            ArtKind::Still => (vec![format!("{stem}-thumb.jpg")], meta.and_then(|m| m.still_url.as_deref())),
        };
        for name in &sidecars {
            let p = dir.join(name);
            if is_nonempty_file(&p) {
                return Ok(Source::File(p));
            }
        }
        if let Some(url) = url {
            match self.download(url).await {
                Ok(t) => return Ok(Source::Temp(t)),
                Err(e) => tracing::warn!(id = %item.id, url, error = %e, "art download failed"),
            }
        }
        if show_fallback
            && item.kind == Some(Kind::Episode)
            && matches!(kind, ArtKind::Poster | ArtKind::Backdrop)
            && let Some(show) = item.show.as_deref().filter(|s| !s.trim().is_empty())
        {
            match Box::pin(self.resolve_show(show, kind)).await {
                Ok(src) => return Ok(src),
                Err(e) => {
                    tracing::debug!(show, error = %e, "no show-level art; falling back to a frame")
                }
            }
        }
        if item.kind == Some(Kind::Recording)
            && item.status.as_deref() != Some(crate::model::rec_status::DONE)
            && !is_nonempty_file(&path)
        {
            anyhow::bail!("recording {} not available yet", item.id);
        }
        Ok(Source::Frame { path, at: kind.frame_time(item.duration) })
    }

    async fn resolve_track(&self, item: &Item, path: &Path) -> anyhow::Result<Source> {
        // 1. embedded picture
        let p = path.to_path_buf();
        let embedded = tokio::task::spawn_blocking(move || crate::metadata::tags::picture(&p)).await.ok().flatten();
        if let Some((mime, bytes)) = embedded
            && !bytes.is_empty()
        {
            let ext = match mime.as_str() {
                "image/png" => "png",
                "image/webp" => "webp",
                "image/gif" => "gif",
                "image/bmp" => "bmp",
                _ => "jpg",
            };
            let tmp = TempFile(self.scratch(ext));
            tokio::fs::write(&tmp.0, &bytes).await?;
            return Ok(Source::Temp(tmp));
        }
        // 2. folder art
        if let Some(dir) = path.parent() {
            for name in ["cover.jpg", "cover.png", "folder.jpg", "folder.png", "front.jpg", "front.png", "album.jpg"] {
                let p = dir.join(name);
                if is_nonempty_file(&p) {
                    return Ok(Source::File(p));
                }
            }
        }
        // 3. provider URLs: the track's own, then the album's (Cover Art Archive)
        let mut urls: Vec<String> = Vec::new();
        if let Some(u) = item.meta.as_ref().and_then(|m| m.poster_url.clone()) {
            urls.push(u);
        }
        if let Some(album_id) = item.album_id.as_deref()
            && let Ok(Some(album)) = db::music::album(&self.pool, album_id).await
            && let Some(u) = album.meta.and_then(|m| m.poster_url)
        {
            urls.push(u);
        }
        for url in urls {
            match self.download(&url).await {
                Ok(t) => return Ok(Source::Temp(t)),
                Err(e) => tracing::warn!(id = %item.id, url, error = %e, "cover download failed"),
            }
        }
        Ok(Source::Placeholder)
    }

    async fn resolve_show(&self, show: &str, kind: ArtKind) -> anyhow::Result<Source> {
        let show = show.trim();
        if show.is_empty() {
            anyhow::bail!("empty show name");
        }
        let kind = match kind {
            ArtKind::Poster | ArtKind::Thumb => ArtKind::Poster,
            ArtKind::Backdrop | ArtKind::Still => ArtKind::Backdrop,
        };
        let episodes = db::items::show_episodes(&self.pool, 0, show).await?;
        let first = episodes.into_iter().next();

        // 1. show-folder sidecars (the episode's folder and its parent, i.e. `Show/Season 01/..` → `Show/`)
        if let Some(p) = first.as_ref().and_then(|e| e.path.as_deref()) {
            let p = Path::new(p);
            let mut dirs: Vec<&Path> = Vec::new();
            if let Some(d) = p.parent() {
                dirs.push(d);
                if let Some(g) = d.parent() {
                    dirs.push(g);
                }
            }
            let names: &[&str] = match kind {
                ArtKind::Poster => &["poster.jpg", "poster.png", "folder.jpg", "cover.jpg"],
                _ => &["fanart.jpg", "fanart.png", "backdrop.jpg"],
            };
            for d in dirs {
                for n in names {
                    let f = d.join(n);
                    if is_nonempty_file(&f) {
                        return Ok(Source::File(f));
                    }
                }
            }
        }
        // 2. show metadata (TMDB / tvshow.nfo)
        let meta = db::items::get_show_meta(&self.pool, show).await?;
        let url = meta.as_ref().and_then(|m| match kind {
            ArtKind::Poster => m.poster_url.as_deref(),
            _ => m.backdrop_url.as_deref(),
        });
        if let Some(url) = url {
            match self.download(url).await {
                Ok(t) => return Ok(Source::Temp(t)),
                Err(e) => tracing::warn!(show, url, error = %e, "show art download failed"),
            }
        }
        // 3. the first episode's own art / a frame of it
        match first {
            Some(ep) => Box::pin(self.resolve_item(&ep, kind, false)).await,
            None => anyhow::bail!("no episodes for show {show}"),
        }
    }

    async fn resolve_album(&self, album_id: &str, _kind: ArtKind) -> anyhow::Result<Source> {
        let tracks = db::music::album_tracks(&self.pool, 0, album_id).await?;
        if let Some(t) = tracks.first() {
            return self.resolve_item(t, ArtKind::Poster, false).await;
        }
        if let Some(url) = db::music::album(&self.pool, album_id).await?.and_then(|a| a.meta).and_then(|m| m.poster_url)
        {
            return Ok(Source::Temp(self.download(&url).await?));
        }
        anyhow::bail!("unknown album {album_id}")
    }

    /// Fetch a provider image into a scratch file (≤ 20 MB, 20 s).
    async fn download(&self, url: &str) -> anyhow::Result<TempFile> {
        if !(url.starts_with("https://") || url.starts_with("http://")) {
            anyhow::bail!("unsupported image url");
        }
        let mut resp = self.http.get(url).timeout(DOWNLOAD_TIMEOUT).send().await?;
        if !resp.status().is_success() {
            anyhow::bail!("HTTP {}", resp.status());
        }
        if resp.content_length().unwrap_or(0) > DOWNLOAD_MAX {
            anyhow::bail!("image larger than {} MB", DOWNLOAD_MAX / 1024 / 1024);
        }
        let ext = match resp.headers().get(reqwest::header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or("") {
            t if t.starts_with("image/png") => "png",
            t if t.starts_with("image/webp") => "webp",
            _ => "jpg",
        };
        let tmp = TempFile(self.scratch(ext));
        let mut file = tokio::fs::File::create(&tmp.0).await?;
        let mut total: u64 = 0;
        let deadline = tokio::time::Instant::now() + DOWNLOAD_TIMEOUT;
        loop {
            let chunk = match tokio::time::timeout_at(deadline, resp.chunk()).await {
                Ok(c) => c?,
                Err(_) => anyhow::bail!("image download timed out"),
            };
            let Some(chunk) = chunk else { break };
            total += chunk.len() as u64;
            if total > DOWNLOAD_MAX {
                anyhow::bail!("image larger than {} MB", DOWNLOAD_MAX / 1024 / 1024);
            }
            file.write_all(&chunk).await?;
        }
        file.flush().await?;
        drop(file);
        if total == 0 {
            anyhow::bail!("empty image response");
        }
        Ok(tmp)
    }

    /// (vtt, jpg) sprite sheet for an item, generated lazily.
    pub async fn sprites(&self, id: &str) -> anyhow::Result<(PathBuf, PathBuf)> {
        let id = id.trim();
        if id.is_empty() || id.len() > 128 {
            anyhow::bail!("invalid item id");
        }
        let base = format!("{}-sprites", safe_key(id));
        let jpg = self.cache_dir.join(format!("{base}.jpg"));
        let vtt = self.cache_dir.join(format!("{base}.vtt"));
        if is_nonempty_file(&jpg) && is_nonempty_file(&vtt) {
            return Ok((vtt, jpg));
        }
        let _guard = self.acquire(&base).await;
        if is_nonempty_file(&jpg) && is_nonempty_file(&vtt) {
            return Ok((vtt, jpg));
        }

        let (kind, path, info) =
            db::items::path_of(&self.pool, id).await?.ok_or_else(|| anyhow::anyhow!("unknown item {id}"))?;
        if kind == "track" {
            anyhow::bail!("no sprites for audio");
        }
        let src = PathBuf::from(&path);
        if !is_nonempty_file(&src) {
            anyhow::bail!("file missing for {id}");
        }
        let duration = info.0.duration_sec;
        let plan = sprite_plan(duration).ok_or_else(|| anyhow::anyhow!("clip too short for sprites"))?;

        std::fs::create_dir_all(&self.cache_dir).ok();
        let tmp = TempFile(self.scratch("jpg"));
        render_sprites(&self.ffmpeg(), &src, plan, &tmp.0).await?;
        std::fs::rename(&tmp.0, &jpg)?;
        let tmp_vtt = TempFile(self.scratch("vtt"));
        std::fs::write(&tmp_vtt.0, sprite_vtt(duration, plan))?;
        std::fs::rename(&tmp_vtt.0, &vtt)?;
        tracing::debug!(id, tiles = plan.count, interval = plan.interval, "sprites rendered");
        Ok((vtt, jpg))
    }

    /// Drop cached renders for a key (after metadata refresh / file change).
    pub fn invalidate(&self, key: &str) {
        let key = key.trim();
        if key.is_empty() {
            return;
        }
        self.misses.retain(|k, _| !k.starts_with(&format!("{key}|")));
        let prefix = format!("{}-", safe_key(key));
        let Ok(rd) = std::fs::read_dir(&self.cache_dir) else {
            return;
        };
        let mut n = 0usize;
        for entry in rd.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with(&prefix) && std::fs::remove_file(entry.path()).is_ok() {
                n += 1;
            }
        }
        if n > 0 {
            tracing::debug!(key, removed = n, "art cache invalidated");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_are_sanitized_and_stable() {
        assert_eq!(safe_key("0123456789abcdef"), "0123456789abcdef");
        let a = safe_key("show:The Office (US)");
        assert!(a.starts_with("show_The_Office__US__"), "{a}");
        assert!(!a.contains(':') && !a.contains(' ') && !a.contains('('));
        assert_eq!(a, safe_key("show:The Office (US)"));
        assert_ne!(safe_key("show:a b"), safe_key("show:a_b"));
        assert_ne!(safe_key("show:a b"), safe_key("show:a-b"));
        // traversal attempts are neutralised
        let t = safe_key("../../etc/passwd");
        assert!(!t.contains('/') && !t.starts_with('.'), "{t}");
        assert!(!safe_key("").is_empty());
        // unicode show names
        let u = safe_key("show:Les Revenants – Émilie");
        assert!(u.is_ascii(), "{u}");
        // long keys are bounded
        assert!(safe_key(&"x".repeat(1000)).len() < 120);
        assert_eq!(cache_name("abc", ArtKind::Poster, None), "abc-poster.jpg");
        assert_eq!(cache_name("abc", ArtKind::Backdrop, Some(640)), "abc-backdrop-w640.jpg");
    }

    #[test]
    fn frame_times() {
        assert_eq!(ArtKind::Poster.frame_time(100.0), 20.0);
        assert_eq!(ArtKind::Backdrop.frame_time(100.0), 40.0);
        assert_eq!(ArtKind::Thumb.frame_time(100.0), 10.0);
        assert_eq!(ArtKind::Still.frame_time(100.0), 30.0);
        assert_eq!(ArtKind::Poster.frame_time(0.0), 1.0);
        assert_eq!(ArtKind::Backdrop.frame_time(1.5), 0.5);
        assert_eq!(ArtKind::Poster.default_width(), 480);
        assert_eq!(ArtKind::parse("backdrop").default_width(), 1280);
    }

    #[test]
    fn sprite_plan_math() {
        assert_eq!(sprite_plan(10.0), None);
        assert_eq!(sprite_plan(f64::NAN), None);
        // short clip: minimum 5 s interval
        let p = sprite_plan(35.0).unwrap();
        assert_eq!(p, SpritePlan { interval: 5, count: 8, rows: 1 });
        // 2 h film: 7200/400 = 18 s
        let p = sprite_plan(7200.0).unwrap();
        assert_eq!(p.interval, 18);
        assert_eq!(p.count, 401);
        assert_eq!(p.rows, 41);
        // very long: interval caps at 30 until the tile cap kicks in
        let p = sprite_plan(15000.0).unwrap();
        assert_eq!(p.interval, 30);
        assert_eq!(p.count, 501);
        let p = sprite_plan(40000.0).unwrap();
        assert!(p.count <= SPRITE_MAX_TILES, "{p:?}");
        assert!(p.interval > 30);
        assert_eq!(p.rows, p.count.div_ceil(10));
    }

    #[test]
    fn sprite_vtt_cues() {
        let plan = SpritePlan { interval: 5, count: 12, rows: 2 };
        let vtt = sprite_vtt(57.0, plan);
        assert!(vtt.starts_with("WEBVTT\n\n"));
        let cues: Vec<&str> = vtt.trim_end().split("\n\n").skip(1).collect();
        assert_eq!(cues.len(), 12);
        assert_eq!(cues[0], "00:00:00.000 --> 00:00:05.000\nsprites.jpg#xywh=0,0,160,90");
        assert_eq!(cues[1], "00:00:05.000 --> 00:00:10.000\nsprites.jpg#xywh=160,0,160,90");
        assert_eq!(cues[10], "00:00:50.000 --> 00:00:55.000\nsprites.jpg#xywh=0,90,160,90");
        // last cue is clamped to the duration
        assert_eq!(cues[11], "00:00:55.000 --> 00:00:57.000\nsprites.jpg#xywh=160,90,160,90");
        assert_eq!(vtt_time(3661.5), "01:01:01.500");
        // cues never start past the end
        let short = sprite_vtt(8.0, SpritePlan { interval: 5, count: 4, rows: 1 });
        assert_eq!(short.matches("-->").count(), 2);
    }

    #[test]
    fn stderr_tail_keeps_last_lines() {
        let s = stderr_tail(b"a\nb\nc\nd\ne\nf\n");
        assert_eq!(s, "c | d | e | f");
    }

    fn ffmpeg_on_path() -> Option<String> {
        let path = std::env::var_os("PATH")?;
        for dir in std::env::split_paths(&path) {
            let p = dir.join("ffmpeg");
            if p.is_file() {
                return Some(p.to_string_lossy().into_owned());
            }
        }
        None
    }

    async fn synth_clip(ffmpeg: &str, dir: &Path, secs: u32) -> PathBuf {
        let out = dir.join("clip.mp4");
        let secs = secs.to_string();
        run_ffmpeg(
            ffmpeg,
            [
                "-f",
                "lavfi",
                "-i",
                "testsrc=size=320x180:rate=25",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440",
                "-t",
                secs.as_str(),
                "-c:v",
                "libx264",
                "-preset",
                "ultrafast",
                "-pix_fmt",
                "yuv420p",
                "-c:a",
                "aac",
                "-shortest",
                out.to_str().unwrap(),
            ],
            Duration::from_secs(120),
        )
        .await
        .expect("synthesize clip");
        out
    }

    fn is_jpeg(p: &Path) -> bool {
        std::fs::read(p).map(|b| b.len() > 2 && b[0] == 0xFF && b[1] == 0xD8).unwrap_or(false)
    }

    #[tokio::test]
    async fn ffmpeg_frame_grab_resize_placeholder_and_sprites() {
        let Some(ffmpeg) = ffmpeg_on_path() else {
            eprintln!("ffmpeg not on PATH; skipping");
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let clip = synth_clip(&ffmpeg, dir.path(), 35).await;

        let poster = dir.path().join("poster.jpg");
        grab_frame(&ffmpeg, &clip, ArtKind::Poster.frame_time(35.0), 480, &poster).await.unwrap();
        assert!(is_jpeg(&poster));

        // seeking past the end falls back to the first frame
        let late = dir.path().join("late.jpg");
        grab_frame(&ffmpeg, &clip, 9999.0, 160, &late).await.unwrap();
        assert!(is_jpeg(&late));

        let small = dir.path().join("small.jpg");
        convert_image(&ffmpeg, &poster, 100, &small).await.unwrap();
        assert!(is_jpeg(&small));
        assert!(std::fs::metadata(&small).unwrap().len() < std::fs::metadata(&poster).unwrap().len());

        let ph = dir.path().join("ph.jpg");
        placeholder(&ffmpeg, 480, &ph).await.unwrap();
        assert!(is_jpeg(&ph));

        let plan = sprite_plan(35.0).unwrap();
        let sheet = dir.path().join("sprites.jpg");
        render_sprites(&ffmpeg, &clip, plan, &sheet).await.unwrap();
        assert!(is_jpeg(&sheet));

        // a bogus input is an error, not a panic
        let bad = dir.path().join("bad.jpg");
        let err = grab_frame(&ffmpeg, &dir.path().join("missing.mp4"), 0.0, 100, &bad).await.unwrap_err();
        assert!(err.to_string().contains("ffmpeg"), "{err}");
        assert!(run_ffmpeg("/definitely/not/ffmpeg", ["-version"], Duration::from_secs(5)).await.is_err());
    }
}
