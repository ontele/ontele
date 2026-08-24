// Copyright 2026 The Ontele Authors
// SPDX-License-Identifier: Apache-2.0

//! Domain types shared by the database layer, the services and the JSON API.
//! Every struct serializes as camelCase; `Option` fields are omitted when
//! `None` so the wire format stays compact for the UI.

use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use std::fmt;

// ---- settings ---------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum CommercialMode {
    Off,
    #[default]
    Skip,
    Delete,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum HwAccel {
    #[default]
    None,
    Vaapi,
    Qsv,
    Nvenc,
    Videotoolbox,
}

impl HwAccel {
    /// ffmpeg encoder name for this accelerator, `None` = software libx264.
    pub fn encoder(self) -> Option<&'static str> {
        match self {
            HwAccel::None => None,
            HwAccel::Vaapi => Some("h264_vaapi"),
            HwAccel::Qsv => Some("h264_qsv"),
            HwAccel::Nvenc => Some("h264_nvenc"),
            HwAccel::Videotoolbox => Some("h264_videotoolbox"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct MetadataProviders {
    pub nfo: bool,
    pub tmdb: bool,
    pub musicbrainz: bool,
}

impl Default for MetadataProviders {
    fn default() -> Self {
        Self { nfo: true, tmdb: true, musicbrainz: true }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Settings {
    pub media_dirs: Vec<String>,
    pub music_dirs: Vec<String>,
    pub recordings_dir: String,
    pub xmltv_url: String,
    pub hdhr_ip: String,
    /// Command run after a recording (and its commercial pass) finalizes.
    /// Invoked as `sh -c <cmd> ontele-post <file>` with ONTELE_FILE/TITLE/ID
    /// in the environment — see tools/handbrake-postprocess.sh.
    pub dvr_post_cmd: String,
    pub commercial_mode: CommercialMode,
    /// In `skip` mode also write ad-break chapters into the recording container.
    pub commercial_chapters: bool,
    pub comskip_path: String,
    pub ffmpeg_path: String,
    pub ffprobe_path: String,
    pub pre_pad_min: u32,
    pub post_pad_min: u32,
    pub auto_delete_watched: bool,
    pub tmdb_api_key: String,
    pub metadata_providers: MetadataProviders,
    pub metadata_language: String,
    pub hwaccel: HwAccel,
    pub transcode_preset: String,
    pub max_transcodes: u32,
    pub scan_interval_min: u32,
    pub watch_filesystem: bool,
    pub guide_refresh_hours: u32,
    pub thumbnails: bool,
    pub activity_retention_days: u32,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            media_dirs: vec![],
            music_dirs: vec![],
            recordings_dir: String::new(),
            xmltv_url: String::new(),
            dvr_post_cmd: String::new(),
            hdhr_ip: String::new(),
            commercial_mode: CommercialMode::Skip,
            commercial_chapters: true,
            comskip_path: "comskip".into(),
            ffmpeg_path: "ffmpeg".into(),
            ffprobe_path: "ffprobe".into(),
            pre_pad_min: 1,
            post_pad_min: 2,
            auto_delete_watched: false,
            tmdb_api_key: String::new(),
            metadata_providers: MetadataProviders::default(),
            metadata_language: "en-US".into(),
            hwaccel: HwAccel::None,
            transcode_preset: "veryfast".into(),
            max_transcodes: 3,
            scan_interval_min: 15,
            watch_filesystem: true,
            guide_refresh_hours: 4,
            thumbnails: true,
            activity_retention_days: 90,
        }
    }
}

impl Settings {
    /// Fill empty strings / zero numbers with defaults (mirrors the Go
    /// `Defaults()` so a partial `PUT /api/settings` never bricks the server).
    pub fn normalize(&mut self) {
        let d = Settings::default();
        if self.comskip_path.trim().is_empty() {
            self.comskip_path = d.comskip_path;
        }
        if self.ffmpeg_path.trim().is_empty() {
            self.ffmpeg_path = d.ffmpeg_path;
        }
        if self.ffprobe_path.trim().is_empty() {
            self.ffprobe_path = d.ffprobe_path;
        }
        if self.transcode_preset.trim().is_empty() {
            self.transcode_preset = d.transcode_preset;
        }
        if self.metadata_language.trim().is_empty() {
            self.metadata_language = d.metadata_language;
        }
        if self.max_transcodes == 0 {
            self.max_transcodes = d.max_transcodes;
        }
        if self.scan_interval_min == 0 {
            self.scan_interval_min = d.scan_interval_min;
        }
        if self.guide_refresh_hours == 0 {
            self.guide_refresh_hours = d.guide_refresh_hours;
        }
        if self.activity_retention_days == 0 {
            self.activity_retention_days = d.activity_retention_days;
        }
        self.media_dirs.retain(|p| !p.trim().is_empty());
        self.music_dirs.retain(|p| !p.trim().is_empty());
        for p in self.media_dirs.iter_mut().chain(self.music_dirs.iter_mut()) {
            *p = p.trim().to_string();
        }
        self.recordings_dir = self.recordings_dir.trim().to_string();
        self.xmltv_url = self.xmltv_url.trim().to_string();
        self.dvr_post_cmd = self.dvr_post_cmd.trim().to_string();
        self.hdhr_ip = self.hdhr_ip.trim().to_string();
        self.tmdb_api_key = self.tmdb_api_key.trim().to_string();
    }
}

// ---- media info ---------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct VideoStream {
    pub index: u32,
    pub codec: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    pub width: u32,
    pub height: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fps: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bit_depth: Option<u32>,
    /// hdr10 | hdr10plus | hlg | dv
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hdr: Option<String>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub interlaced: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct AudioStream {
    pub index: u32,
    pub codec: String,
    pub channels: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lang: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub default: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct SubtitleStream {
    pub index: u32,
    pub codec: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lang: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub forced: bool,
    /// Text-based (convertible to WebVTT) vs bitmap (pgs/dvdsub: burn-in only).
    pub text: bool,
    /// Path of an external sidecar (.srt/.vtt/.ass) when not embedded.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub external: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct Chapter {
    pub start: f64,
    pub end: f64,
    pub title: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct MediaInfo {
    pub duration_sec: f64,
    pub container: String,
    pub size_bytes: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bitrate: Option<u64>,
    // flattened convenience fields (first video / first audio stream)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vcodec: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub acodec: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub video: Option<VideoStream>,
    pub audio: Vec<AudioStream>,
    pub subtitles: Vec<SubtitleStream>,
    pub chapters: Vec<Chapter>,
}

impl MediaInfo {
    pub fn hdr(&self) -> Option<&str> {
        self.video.as_ref().and_then(|v| v.hdr.as_deref())
    }
}

// ---- metadata ------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct CastMember {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub character: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct Metadata {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tmdb_id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub imdb_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tvdb_id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mbid: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub original_title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub overview: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tagline: Option<String>,
    pub genres: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rating: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub votes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub runtime_min: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub release_date: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content_rating: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub studio: Option<String>,
    pub cast: Vec<CastMember>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub poster_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub backdrop_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub still_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub logo_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated: Option<DateTime<Utc>>,
}

impl Metadata {
    pub fn is_empty(&self) -> bool {
        self.provider.is_none() && self.overview.is_none() && self.poster_url.is_none()
    }
    /// Overlay `other` on top of `self` — `other` wins wherever it has a value.
    pub fn merge_from(&mut self, other: Metadata) {
        macro_rules! take {
            ($($f:ident),*) => { $( if other.$f.is_some() { self.$f = other.$f; } )* };
        }
        take!(
            provider,
            provider_id,
            tmdb_id,
            imdb_id,
            tvdb_id,
            mbid,
            original_title,
            overview,
            tagline,
            rating,
            votes,
            runtime_min,
            release_date,
            content_rating,
            studio,
            poster_url,
            backdrop_url,
            still_url,
            logo_url,
            updated
        );
        if !other.genres.is_empty() {
            self.genres = other.genres;
        }
        if !other.cast.is_empty() {
            self.cast = other.cast;
        }
    }
}

// ---- library items -------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[derive(Default)]
pub enum Kind {
    #[default]
    Movie,
    Episode,
    Track,
    Recording,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Movie => "movie",
            Kind::Episode => "episode",
            Kind::Track => "track",
            Kind::Recording => "recording",
        }
    }
    pub fn parse(s: &str) -> Option<Kind> {
        match s {
            "movie" => Some(Kind::Movie),
            "episode" => Some(Kind::Episode),
            "track" => Some(Kind::Track),
            "recording" => Some(Kind::Recording),
            _ => None,
        }
    }
}

impl fmt::Display for Kind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct Break {
    pub start: f64,
    pub end: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct WatchState {
    pub pos: f64,
    pub dur: f64,
    pub done: bool,
    pub updated: DateTime<Utc>,
}

/// Recording lifecycle.
pub mod rec_status {
    pub const SCHEDULED: &str = "scheduled";
    pub const RECORDING: &str = "recording";
    pub const DONE: &str = "done";
    pub const FAILED: &str = "failed";
    pub const CANCELED: &str = "canceled";
}

/// Commercial detection lifecycle: pending → ready (skip) | cut (delete) | failed.
pub mod breaks_state {
    pub const PENDING: &str = "pending";
    pub const READY: &str = "ready";
    pub const CUT: &str = "cut";
    pub const FAILED: &str = "failed";
}

/// The unified item shape the UI renders everywhere (cards, detail, player).
/// Maps 1:1 onto the `items` row plus per-user watch state and tags.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct Item {
    pub id: String,
    pub kind: Option<Kind>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sort_title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subtitle: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub year: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub show: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub season: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub episode: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub episode_end: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub air_date: Option<NaiveDate>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub artist: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub album_artist: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub album: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub album_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub track_no: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disc_no: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub genre: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub duration: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vcodec: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub acodec: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub container: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hdr: Option<String>,
    pub added: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub watch: Option<WatchState>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub breaks: Option<Vec<Break>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub breaks_state: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub channel: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub channel_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rule_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end: Option<DateTime<Utc>>,
    pub tags: Vec<String>,
    pub auto_tags: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub meta: Option<Metadata>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub info: Option<MediaInfo>,
    /// Detail view only: the episode that follows this one in the same show.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_episode: Option<Box<Item>>,
}

impl Item {
    /// Card-sized copy: drops the heavy `info`/`meta.cast` payloads.
    pub fn card(mut self) -> Item {
        self.info = None;
        self.path = None;
        self.next_episode = None;
        if let Some(m) = self.meta.as_mut() {
            // `overview` stays: the home hero and episode rows render it.
            m.cast.clear();
        }
        self
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Rule {
    pub id: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel_id: Option<String>,
    #[serde(default)]
    pub keep: i32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_id: Option<i64>,
    #[serde(default = "Utc::now")]
    pub created: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct Channel {
    pub guide_number: String,
    pub guide_name: String,
    pub url: String,
    pub hd: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct User {
    pub id: i64,
    pub subject: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub groups: Vec<String>,
    pub is_admin: bool,
    pub created: DateTime<Utc>,
    pub last_seen: DateTime<Utc>,
}

impl User {
    pub fn display(&self) -> &str {
        self.name.as_deref().or(self.email.as_deref()).unwrap_or(&self.subject)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ActivityEvent {
    pub id: i64,
    pub ts: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub item_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub item_title: Option<String>,
    pub detail: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ShowSummary {
    pub show: String,
    pub episodes: i64,
    pub seasons: i64,
    pub poster_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub meta: Option<Metadata>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub year: Option<i32>,
    pub added: DateTime<Utc>,
    /// Episodes fully watched by the requesting user.
    pub watched: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct AlbumSummary {
    pub id: String,
    pub artist: String,
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub year: Option<i32>,
    pub tracks: i64,
    pub duration: f64,
    /// Item id whose embedded/sidecar art represents the album.
    pub art_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub meta: Option<Metadata>,
    pub added: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ArtistSummary {
    pub name: String,
    pub albums: i64,
    pub tracks: i64,
    pub art_id: String,
}

// ---- EPG ---------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct Airing {
    /// HDHomeRun GuideNumber.
    pub channel_id: String,
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subtitle: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
    pub categories: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub season: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub episode: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub new: bool,
}

// ---- playback -----------------------------------------------------------------

/// What the browser told us it can decode natively.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ClientCaps {
    pub video: Vec<String>,
    pub audio: Vec<String>,
    pub containers: Vec<String>,
    /// "mse" (hls.js) or "native" (Safari)
    pub hls: String,
    pub max_height: u32,
    pub surround: bool,
}

impl Default for ClientCaps {
    /// Conservative baseline every modern browser satisfies.
    fn default() -> Self {
        Self {
            video: vec!["h264".into()],
            audio: vec!["aac".into(), "mp3".into()],
            containers: vec!["mp4".into()],
            hls: "mse".into(),
            max_height: 2160,
            surround: false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SegmentKind {
    Ts,
    Fmp4,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlaybackPlan {
    /// direct | copy | transcode
    pub mode: String,
    pub video_copy: bool,
    pub audio_copy: bool,
    /// Target height when transcoding (0 = keep).
    pub height: u32,
    pub segment: SegmentKind,
    pub reasons: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct ScanStatus {
    pub scanning: bool,
    pub found: u64,
    pub probed: u64,
    pub added: u64,
    pub updated: u64,
    pub removed: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub started_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
}

/// Stable, path-derived identifier (16 hex chars of blake3).
pub fn item_id(path: &str) -> String {
    blake3::hash(path.as_bytes()).to_hex()[..16].to_string()
}

/// Album key: case-insensitive album artist + album title.
pub fn album_id(album_artist: &str, album: &str) -> String {
    let key = format!("{}|{}", album_artist.trim().to_lowercase(), album.trim().to_lowercase());
    blake3::hash(key.as_bytes()).to_hex()[..16].to_string()
}

/// Random hex id of `n` bytes.
pub fn rand_id(n: usize) -> String {
    let mut buf = vec![0u8; n];
    rand::fill(&mut buf[..]);
    hex::encode(buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_roundtrip_and_defaults() {
        let s: Settings = serde_json::from_str(r#"{"mediaDirs":["/tank/movies"],"commercialMode":"delete"}"#).unwrap();
        assert_eq!(s.media_dirs, vec!["/tank/movies"]);
        assert_eq!(s.commercial_mode, CommercialMode::Delete);
        assert_eq!(s.ffmpeg_path, "ffmpeg");
        assert_eq!(s.pre_pad_min, 1);
        let json = serde_json::to_value(&s).unwrap();
        assert_eq!(json["commercialMode"], "delete");
        assert_eq!(json["hwaccel"], "none");
    }

    #[test]
    fn normalize_restores_blank_paths() {
        let mut s = Settings { ffmpeg_path: "  ".into(), max_transcodes: 0, ..Default::default() };
        s.media_dirs = vec![" /a ".into(), "".into()];
        s.normalize();
        assert_eq!(s.ffmpeg_path, "ffmpeg");
        assert_eq!(s.max_transcodes, 3);
        assert_eq!(s.media_dirs, vec!["/a"]);
    }

    #[test]
    fn ids_are_stable_and_short() {
        assert_eq!(item_id("/x/y.mkv"), item_id("/x/y.mkv"));
        assert_ne!(item_id("/x/y.mkv"), item_id("/x/z.mkv"));
        assert_eq!(item_id("/x/y.mkv").len(), 16);
        assert_eq!(album_id("Daft Punk", "Discovery"), album_id("daft punk ", " discovery"));
        assert_eq!(rand_id(8).len(), 16);
    }

    #[test]
    fn item_omits_empty_options() {
        let it = Item { id: "a".into(), title: "T".into(), ..Default::default() };
        let json = serde_json::to_string(&it).unwrap();
        assert!(!json.contains("\"year\""));
        assert!(json.contains("\"tags\":[]"));
    }

    #[test]
    fn metadata_merge_prefers_other() {
        let mut a = Metadata { overview: Some("a".into()), genres: vec!["Drama".into()], ..Default::default() };
        a.merge_from(Metadata { overview: Some("b".into()), ..Default::default() });
        assert_eq!(a.overview.as_deref(), Some("b"));
        assert_eq!(a.genres, vec!["Drama"]);
    }
}
