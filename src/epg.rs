// Copyright 2026 The Ontele Authors
// SPDX-License-Identifier: Apache-2.0

//! XMLTV guide (URL or file, `.gz` ok) parsed with a streaming reader (guide
//! files run to hundreds of MB) and mapped onto the HDHomeRun lineup by
//! guide number or display name. Airings live in memory per channel, sorted
//! by start, for O(log n) window queries.

use crate::{
    model::{Airing, Channel},
    state::SettingsCache,
};
use anyhow::{Context, anyhow};
use chrono::{DateTime, FixedOffset, LocalResult, NaiveDate, NaiveDateTime, TimeZone, Utc};
use parking_lot::RwLock;
use quick_xml::events::{BytesStart, Event};
use std::{
    collections::HashMap,
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
    sync::Arc,
    time::Instant,
};
use tokio::io::AsyncWriteExt;

/// Hard cap on a downloaded guide (XMLTV for a full-country lineup is a
/// few hundred MB uncompressed; 2 GiB leaves plenty of headroom).
const MAX_DOWNLOAD_BYTES: u64 = 2 * 1024 * 1024 * 1024;
/// Time-line sanity: the guide must not sit more than 3 entries past the
/// binary-search point (airings are sorted by start; overlaps are rare
/// data errors).
const OVERLAP_SLACK: usize = 3;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct XmltvChannel {
    pub id: String,
    pub names: Vec<String>,
    pub icon: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct XmltvProgramme {
    pub channel: String,
    pub start: String,
    pub stop: String,
    pub title: String,
    pub subtitle: Option<String>,
    pub desc: Option<String>,
    pub categories: Vec<String>,
    /// raw `<episode-num system="xmltv_ns">` or `onscreen` value
    pub episode_num: Option<(String, String)>,
    pub icon: Option<String>,
    pub new: bool,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct XmltvDoc {
    pub channels: Vec<XmltvChannel>,
    pub programmes: Vec<XmltvProgramme>,
}

#[derive(Default)]
struct Inner {
    by_channel: HashMap<String, Vec<Airing>>,
    icons: HashMap<String, String>,
    updated: Option<DateTime<Utc>>,
}

pub struct Guide {
    pub settings: Arc<SettingsCache>,
    pub http: reqwest::Client,
    inner: RwLock<Inner>,
    /// Guide-API endpoint; overridable in tests.
    pub guide_api: String,
}

/// Result of turning an XMLTV document into the in-memory index.
struct Built {
    by_channel: HashMap<String, Vec<Airing>>,
    icons: HashMap<String, String>,
    count: usize,
    skipped_unmapped: usize,
    skipped_invalid: usize,
}

/// One channel entry from the HDHomeRun guide API (api.hdhomerun.com).
#[derive(Debug, Clone, serde::Deserialize)]
pub struct HdhrGuideChannel {
    #[serde(rename = "GuideNumber", default)]
    pub guide_number: String,
    #[serde(rename = "ImageURL", default)]
    pub image_url: Option<String>,
    #[serde(rename = "Guide", default)]
    pub guide: Vec<HdhrGuideProgram>,
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct HdhrGuideProgram {
    #[serde(rename = "StartTime", default)]
    pub start_time: i64,
    #[serde(rename = "EndTime", default)]
    pub end_time: i64,
    #[serde(rename = "Title", default)]
    pub title: String,
    #[serde(rename = "EpisodeTitle", default)]
    pub episode_title: Option<String>,
    #[serde(rename = "Synopsis", default)]
    pub synopsis: Option<String>,
    #[serde(rename = "EpisodeNumber", default)]
    pub episode_number: Option<String>,
    #[serde(rename = "ImageURL", default)]
    pub image_url: Option<String>,
    #[serde(rename = "OriginalAirdate", default)]
    pub original_airdate: Option<i64>,
    #[serde(rename = "Filter", default)]
    pub filter: Vec<String>,
}

/// Default endpoint of SiliconDust's guide API; tests override `Guide::guide_api`.
pub const HDHR_GUIDE_API: &str = "https://api.hdhomerun.com/api/guide";
/// The API serves ~4h slices; fetch this many hours per refresh.
const HDHR_GUIDE_HOURS: i64 = 24;

/// Convert guide-API slices into the same structure XMLTV parsing produces.
/// Channels not in the tuner lineup are skipped (counted as unmapped);
/// duplicate (channel, start) entries across overlapping slices collapse.
fn build_from_hdhr(slices: &[Vec<HdhrGuideChannel>], lineup: &[Channel]) -> Built {
    let known: std::collections::HashSet<&str> = lineup.iter().map(|c| c.guide_number.as_str()).collect();
    let mut by_channel: HashMap<String, Vec<Airing>> = HashMap::new();
    let mut icons = HashMap::new();
    let mut seen: std::collections::HashSet<(String, i64)> = std::collections::HashSet::new();
    let mut skipped_unmapped = 0usize;
    let mut skipped_invalid = 0usize;
    for slice in slices {
        for ch in slice {
            if ch.guide_number.is_empty() {
                continue;
            }
            if !known.contains(ch.guide_number.as_str()) {
                skipped_unmapped += 1;
                continue;
            }
            if let Some(icon) = ch.image_url.as_deref().filter(|u| !u.is_empty()) {
                icons.entry(ch.guide_number.clone()).or_insert_with(|| icon.to_string());
            }
            for p in &ch.guide {
                if p.start_time <= 0 || p.end_time <= p.start_time || p.title.is_empty() {
                    skipped_invalid += 1;
                    continue;
                }
                if !seen.insert((ch.guide_number.clone(), p.start_time)) {
                    continue;
                }
                let (season, episode) =
                    p.episode_number.as_deref().map(|e| parse_episode_num("onscreen", e)).unwrap_or((None, None));
                let (Some(start), Some(end)) =
                    (DateTime::from_timestamp(p.start_time, 0), DateTime::from_timestamp(p.end_time, 0))
                else {
                    skipped_invalid += 1;
                    continue;
                };
                by_channel.entry(ch.guide_number.clone()).or_default().push(Airing {
                    channel_id: ch.guide_number.clone(),
                    title: p.title.clone(),
                    subtitle: p.episode_title.clone().filter(|t| !t.is_empty()),
                    description: p.synopsis.clone().filter(|t| !t.is_empty()),
                    start,
                    end,
                    categories: p.filter.clone(),
                    season,
                    episode,
                    icon: p.image_url.clone().filter(|u| !u.is_empty()),
                    // first runs carry OriginalAirdate == air date
                    new: p.original_airdate.map(|o| p.start_time - o < 86_400).unwrap_or(false),
                });
            }
        }
    }
    let mut count = 0;
    for airings in by_channel.values_mut() {
        airings.sort_by_key(|a| a.start);
        count += airings.len();
    }
    Built { by_channel, icons, count, skipped_unmapped, skipped_invalid }
}

impl Guide {
    pub fn new(settings: Arc<SettingsCache>, http: reqwest::Client) -> Self {
        Self { settings, http, guide_api: HDHR_GUIDE_API.to_string(), inner: RwLock::new(Inner::default()) }
    }

    /// Build a guide from pre-computed airings (tests, fixtures). Each list
    /// is sorted by start and de-duplicated on (channel, start) like
    /// [`Guide::refresh`] would.
    pub fn with_airings(
        settings: Arc<SettingsCache>,
        http: reqwest::Client,
        airings: HashMap<String, Vec<Airing>>,
    ) -> Self {
        let g = Self::new(settings, http);
        let mut by_channel = airings;
        for list in by_channel.values_mut() {
            normalize_channel(list);
        }
        let count = by_channel.values().map(Vec::len).sum();
        g.install(by_channel, HashMap::new(), count);
        g
    }

    fn install(&self, by_channel: HashMap<String, Vec<Airing>>, icons: HashMap<String, String>, count: usize) {
        let mut g = self.inner.write();
        g.by_channel = by_channel;
        g.icons = icons;
        g.updated = Some(Utc::now());
        drop(g);
        metrics::gauge!("ontele_guide_airings").set(count as f64);
    }

    /// Download/read XMLTV and rebuild the index. Returns airings count.
    /// No-op (Ok(0)) when no guide source is configured.
    pub async fn refresh(&self, channels: &[Channel]) -> anyhow::Result<usize> {
        self.refresh_with_hdhr(channels, None).await
    }

    /// Refresh from the configured XMLTV source; with no source configured
    /// but a tuner DeviceAuth at hand, scrape the guide straight from the
    /// HDHomeRun guide API instead (XMLTV wins once configured).
    pub async fn refresh_with_hdhr(&self, channels: &[Channel], device_auth: Option<&str>) -> anyhow::Result<usize> {
        let source = self.settings.get().xmltv_url.trim().to_string();
        if source.is_empty() {
            if let Some(auth) = device_auth.filter(|a| !a.is_empty()) {
                return self.refresh_from_hdhr(channels, auth).await;
            }
            tracing::debug!("no xmltv source or tuner auth; guide refresh skipped");
            return Ok(0);
        }
        let started = Instant::now();
        let lineup = channels.to_vec();

        let built = if is_http_url(&source) {
            let tmp = TempFile::new("ontele-xmltv");
            download_to(&self.http, &source, &tmp.path).await?;
            let path = tmp.path.clone();
            let gz_hint = url_has_gz_suffix(&source);
            let built = tokio::task::spawn_blocking(move || build_from_file(&path, gz_hint, &lineup))
                .await
                .context("guide parse task")??;
            drop(tmp);
            built
        } else {
            let path = PathBuf::from(&source);
            if !path.is_file() {
                return Err(anyhow!("xmltv path {} is not a readable file", path.display()));
            }
            let gz_hint = url_has_gz_suffix(&source);
            tokio::task::spawn_blocking(move || build_from_file(&path, gz_hint, &lineup))
                .await
                .context("guide parse task")??
        };

        let count = built.count;
        let mapped = built.by_channel.len();
        let icons = built.icons.len();
        self.install(built.by_channel, built.icons, count);
        tracing::info!(
            airings = count,
            channels = mapped,
            icons,
            skipped_unmapped = built.skipped_unmapped,
            skipped_invalid = built.skipped_invalid,
            elapsed_ms = started.elapsed().as_millis() as u64,
            "XMLTV guide refreshed"
        );
        Ok(count)
    }

    async fn refresh_from_hdhr(&self, channels: &[Channel], auth: &str) -> anyhow::Result<usize> {
        let started = Instant::now();
        let now = chrono::Utc::now().timestamp();
        let mut slices = Vec::new();
        let mut errors = 0usize;
        let mut start = now;
        while start < now + HDHR_GUIDE_HOURS * 3600 {
            let url = format!("{}?DeviceAuth={}&Start={}", self.guide_api, urlencoding::encode(auth), start);
            match self.http.get(&url).send().await {
                Ok(resp) if resp.status().is_success() => match resp.json::<Vec<HdhrGuideChannel>>().await {
                    Ok(slice) => {
                        if slice.is_empty() {
                            break;
                        }
                        slices.push(slice);
                    }
                    Err(e) => {
                        errors += 1;
                        tracing::warn!(error = %e, start, "hdhr guide slice parse");
                    }
                },
                Ok(resp) => {
                    errors += 1;
                    tracing::warn!(status = %resp.status(), start, "hdhr guide slice");
                }
                Err(e) => {
                    errors += 1;
                    tracing::warn!(error = %e, start, "hdhr guide slice fetch");
                }
            }
            if errors >= 3 {
                break;
            }
            start += 4 * 3600;
        }
        if slices.is_empty() {
            anyhow::bail!("HDHomeRun guide API returned no data");
        }
        let built = build_from_hdhr(&slices, channels);
        let (count, mapped, icons) = (built.count, built.by_channel.len(), built.icons.len());
        self.install(built.by_channel, built.icons, count);
        tracing::info!(
            airings = count,
            channels = mapped,
            icons,
            skipped_unmapped = built.skipped_unmapped,
            elapsed_ms = started.elapsed().as_millis() as u64,
            "HDHomeRun guide refreshed (no XMLTV source configured)"
        );
        Ok(count)
    }

    /// Airings overlapping [from, to] per channel.
    pub fn window(&self, from: DateTime<Utc>, to: DateTime<Utc>) -> HashMap<String, Vec<Airing>> {
        let g = self.inner.read();
        let mut out = HashMap::new();
        if to <= from {
            return out;
        }
        for (ch, list) in &g.by_channel {
            let first = list.partition_point(|a| a.start <= from).saturating_sub(OVERLAP_SLACK);
            let hits: Vec<Airing> =
                list[first..].iter().take_while(|a| a.start < to).filter(|a| a.end > from).cloned().collect();
            if !hits.is_empty() {
                out.insert(ch.clone(), hits);
            }
        }
        out
    }

    /// Current and next airing on a channel.
    pub fn now_next(&self, guide_number: &str, now: DateTime<Utc>) -> (Option<Airing>, Option<Airing>) {
        let g = self.inner.read();
        let Some(list) = g.by_channel.get(guide_number) else {
            return (None, None);
        };
        // idx = first airing that starts after `now`
        let idx = list.partition_point(|a| a.start <= now);
        let lo = idx.saturating_sub(OVERLAP_SLACK);
        let cur_pos = (lo..idx).rev().find(|&i| list[i].end > now);
        match cur_pos {
            Some(i) => (Some(list[i].clone()), list.get(i + 1).cloned()),
            None => (None, list.get(idx).cloned()),
        }
    }

    /// Future airings whose title equals-fold `title`, optionally on one channel.
    pub fn matches(&self, title: &str, channel: Option<&str>, now: DateTime<Utc>) -> Vec<Airing> {
        let want = fold(title);
        if want.is_empty() {
            return Vec::new();
        }
        let channel = channel.map(str::trim).filter(|c| !c.is_empty());
        let g = self.inner.read();
        let mut out = Vec::new();
        for (ch, list) in &g.by_channel {
            if let Some(c) = channel
                && c != ch
            {
                continue;
            }
            for a in upcoming(list, now) {
                if a.end > now && fold(&a.title) == want {
                    out.push(a.clone());
                }
            }
        }
        out.sort_by(|a, b| a.start.cmp(&b.start).then_with(|| a.channel_id.cmp(&b.channel_id)));
        out
    }

    /// Case-insensitive substring search over upcoming titles (dedup by title).
    pub fn search(&self, q: &str, now: DateTime<Utc>, limit: usize) -> Vec<Airing> {
        let needle = fold(q);
        if needle.is_empty() || limit == 0 {
            return Vec::new();
        }
        let g = self.inner.read();
        let mut best: HashMap<String, Airing> = HashMap::new();
        for list in g.by_channel.values() {
            for a in upcoming(list, now) {
                if a.end <= now {
                    continue;
                }
                let hit = fold(&a.title).contains(&needle)
                    || a.subtitle.as_deref().map(|s| fold(s).contains(&needle)).unwrap_or(false);
                if !hit {
                    continue;
                }
                let key = fold(&a.title);
                match best.get(&key) {
                    Some(prev) if prev.start <= a.start => {}
                    _ => {
                        best.insert(key, a.clone());
                    }
                }
            }
        }
        let mut out: Vec<Airing> = best.into_values().collect();
        out.sort_by(|a, b| a.start.cmp(&b.start).then_with(|| a.title.cmp(&b.title)));
        out.truncate(limit);
        out
    }

    pub fn updated(&self) -> Option<DateTime<Utc>> {
        self.inner.read().updated
    }

    /// GuideNumber → icon URL from the XMLTV `<icon>` elements.
    pub fn channel_icons(&self) -> HashMap<String, String> {
        self.inner.read().icons.clone()
    }
}

/// Slice of `list` starting a few entries before the first airing that
/// starts after `now` (callers still filter on `end > now`).
fn upcoming(list: &[Airing], now: DateTime<Utc>) -> &[Airing] {
    let idx = list.partition_point(|a| a.start <= now).saturating_sub(OVERLAP_SLACK);
    &list[idx..]
}

fn fold(s: &str) -> String {
    s.trim().to_lowercase()
}

/// Sort by start and drop duplicate (channel, start) entries.
fn normalize_channel(list: &mut Vec<Airing>) {
    list.sort_by(|a, b| a.start.cmp(&b.start).then_with(|| a.end.cmp(&b.end)));
    list.dedup_by(|a, b| a.start == b.start);
}

fn is_http_url(s: &str) -> bool {
    let l = s.to_ascii_lowercase();
    l.starts_with("http://") || l.starts_with("https://")
}

fn url_has_gz_suffix(s: &str) -> bool {
    let path = s.split(['?', '#']).next().unwrap_or(s);
    path.to_ascii_lowercase().ends_with(".gz")
}

/// Temp file that is removed on drop (also on error paths).
struct TempFile {
    path: PathBuf,
}

impl TempFile {
    fn new(prefix: &str) -> Self {
        let name = format!("{prefix}-{}-{:016x}.tmp", std::process::id(), rand::random::<u64>());
        Self { path: std::env::temp_dir().join(name) }
    }
}

impl Drop for TempFile {
    fn drop(&mut self) {
        if let Err(e) = std::fs::remove_file(&self.path)
            && e.kind() != std::io::ErrorKind::NotFound
        {
            tracing::debug!(path = %self.path.display(), error = %e, "temp guide file cleanup failed");
        }
    }
}

/// Stream a URL to disk so multi-hundred-MB guides never sit in memory.
async fn download_to(http: &reqwest::Client, url: &str, dest: &Path) -> anyhow::Result<u64> {
    let mut resp = http
        .get(url)
        .timeout(std::time::Duration::from_secs(540))
        .send()
        .await
        .with_context(|| format!("GET {url}"))?;
    let status = resp.status();
    if !status.is_success() {
        return Err(anyhow!("GET {url}: HTTP {status}"));
    }
    if let Some(len) = resp.content_length()
        && len > MAX_DOWNLOAD_BYTES
    {
        return Err(anyhow!("GET {url}: guide too large ({len} bytes)"));
    }
    let mut file = tokio::fs::File::create(dest).await.with_context(|| format!("create {}", dest.display()))?;
    let mut total: u64 = 0;
    while let Some(chunk) = resp.chunk().await.with_context(|| format!("GET {url}: read body"))? {
        total += chunk.len() as u64;
        if total > MAX_DOWNLOAD_BYTES {
            return Err(anyhow!("GET {url}: guide too large (> {MAX_DOWNLOAD_BYTES} bytes)"));
        }
        file.write_all(&chunk).await.with_context(|| format!("write {}", dest.display()))?;
    }
    file.flush().await?;
    tracing::debug!(url, bytes = total, "xmltv downloaded");
    Ok(total)
}

/// Open, transparently gunzip (by suffix hint or magic bytes), parse and index.
fn build_from_file(path: &Path, gz_hint: bool, lineup: &[Channel]) -> anyhow::Result<Built> {
    let file = std::fs::File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut reader = BufReader::with_capacity(256 * 1024, file);
    let magic = reader.fill_buf().context("read guide header")?;
    let is_gz = magic.len() >= 2 && magic[0] == 0x1f && magic[1] == 0x8b;
    if gz_hint != is_gz {
        tracing::debug!(path = %path.display(), gz_suffix = gz_hint, gzip_magic = is_gz, "gzip hint mismatch; trusting magic bytes");
    }
    let doc = if is_gz {
        let gz = flate2::read::GzDecoder::new(reader);
        parse_xmltv(BufReader::with_capacity(256 * 1024, gz))
    } else {
        parse_xmltv(reader)
    }
    .with_context(|| format!("parse xmltv {}", path.display()))?;
    if doc.channels.is_empty() && doc.programmes.is_empty() {
        return Err(anyhow!(
            "parse xmltv {}: no <channel> or <programme> elements (not an XMLTV document?)",
            path.display()
        ));
    }
    Ok(build_index(doc, lineup))
}

/// Turn a parsed document into per-channel sorted airings + icons.
fn build_index(doc: XmltvDoc, lineup: &[Channel]) -> Built {
    let mapping = map_channels(&doc.channels, lineup);
    let mut icons: HashMap<String, String> = HashMap::new();
    for c in &doc.channels {
        if let Some(num) = mapping.get(&c.id)
            && let Some(icon) = c.icon.as_deref().map(str::trim).filter(|s| !s.is_empty())
        {
            icons.entry(num.clone()).or_insert_with(|| icon.to_string());
        }
    }
    let mut by_channel: HashMap<String, Vec<Airing>> = HashMap::new();
    let mut skipped_unmapped = 0usize;
    let mut skipped_invalid = 0usize;
    for p in doc.programmes {
        let Some(num) = mapping.get(&p.channel) else {
            skipped_unmapped += 1;
            continue;
        };
        let (Some(start), Some(end)) = (parse_xmltv_time(&p.start), parse_xmltv_time(&p.stop)) else {
            skipped_invalid += 1;
            continue;
        };
        if end <= start {
            skipped_invalid += 1;
            continue;
        }
        let title = p.title.trim().to_string();
        if title.is_empty() {
            skipped_invalid += 1;
            continue;
        }
        let (season, episode) =
            p.episode_num.as_ref().map(|(sys, val)| parse_episode_num(sys, val)).unwrap_or((None, None));
        by_channel.entry(num.clone()).or_default().push(Airing {
            channel_id: num.clone(),
            title,
            subtitle: p.subtitle.map(|s| s.trim().to_string()).filter(|s| !s.is_empty()),
            description: p.desc.map(|s| s.trim().to_string()).filter(|s| !s.is_empty()),
            start,
            end,
            categories: p.categories,
            season,
            episode,
            icon: p.icon.map(|s| s.trim().to_string()).filter(|s| !s.is_empty()),
            new: p.new,
        });
    }
    for list in by_channel.values_mut() {
        normalize_channel(list);
    }
    let count = by_channel.values().map(Vec::len).sum();
    Built { by_channel, icons, count, skipped_unmapped, skipped_invalid }
}

// ---------------------------------------------------------------------------
// XMLTV parser
// ---------------------------------------------------------------------------

/// Which leaf element's text is being accumulated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Field {
    DisplayName,
    Title,
    SubTitle,
    Desc,
    Category,
    EpisodeNum,
}

/// Minimal state machine over the quick-xml event stream.
struct ParseState {
    doc: XmltvDoc,
    channel: Option<XmltvChannel>,
    programme: Option<XmltvProgramme>,
    /// Nesting depth inside an unrelated element (so nested `<title>` inside
    /// e.g. `<credits>` is never mistaken for the programme title).
    skip_depth: usize,
    field: Option<Field>,
    /// Depth of unknown children *inside* the element being captured.
    field_depth: usize,
    text: String,
    episode_system: String,
}

fn attr(e: &BytesStart<'_>, name: &str) -> Option<String> {
    for a in e.attributes().with_checks(false).flatten() {
        if a.key.local_name().as_ref() == name {
            let v = a.normalized_value(quick_xml::XmlVersion::Implicit1_0).ok()?;
            return Some(v.trim().to_string());
        }
    }
    None
}

impl ParseState {
    fn new() -> Self {
        Self {
            doc: XmltvDoc::default(),
            channel: None,
            programme: None,
            skip_depth: 0,
            field: None,
            field_depth: 0,
            text: String::new(),
            episode_system: String::new(),
        }
    }

    fn start(&mut self, e: &BytesStart<'_>, empty: bool) {
        if self.skip_depth > 0 {
            if !empty {
                self.skip_depth += 1;
            }
            return;
        }
        if self.field.is_some() {
            // Unexpected markup inside a leaf; keep its text, ignore the tag.
            if !empty {
                self.field_depth += 1;
            }
            return;
        }
        let name = e.local_name();
        let name = name.as_ref();
        if let Some(ch) = self.channel.as_mut() {
            match name {
                "display-name" => self.begin(Field::DisplayName, empty),
                "icon" => {
                    if ch.icon.is_none()
                        && let Some(src) = attr(e, "src").filter(|s| !s.is_empty())
                    {
                        ch.icon = Some(src);
                    }
                    if !empty {
                        self.skip_depth = 1;
                    }
                }
                _ => {
                    if !empty {
                        self.skip_depth = 1;
                    }
                }
            }
            return;
        }
        if let Some(p) = self.programme.as_mut() {
            match name {
                "title" => self.begin(Field::Title, empty),
                "sub-title" => self.begin(Field::SubTitle, empty),
                "desc" => self.begin(Field::Desc, empty),
                "category" => self.begin(Field::Category, empty),
                "episode-num" => {
                    self.episode_system = attr(e, "system").unwrap_or_default().to_ascii_lowercase();
                    self.begin(Field::EpisodeNum, empty);
                }
                "icon" => {
                    if p.icon.is_none()
                        && let Some(src) = attr(e, "src").filter(|s| !s.is_empty())
                    {
                        p.icon = Some(src);
                    }
                    if !empty {
                        self.skip_depth = 1;
                    }
                }
                "new" => {
                    p.new = true;
                    if !empty {
                        self.skip_depth = 1;
                    }
                }
                _ => {
                    // previously-shown, credits, date, rating, audio, video, ...
                    if !empty {
                        self.skip_depth = 1;
                    }
                }
            }
            return;
        }
        match name {
            "channel" => {
                if empty {
                    return;
                }
                self.channel = Some(XmltvChannel { id: attr(e, "id").unwrap_or_default(), ..Default::default() });
            }
            "programme" => {
                if empty {
                    return;
                }
                self.programme = Some(XmltvProgramme {
                    channel: attr(e, "channel").unwrap_or_default(),
                    start: attr(e, "start").unwrap_or_default(),
                    stop: attr(e, "stop").unwrap_or_default(),
                    ..Default::default()
                });
            }
            // <tv> root and anything else at top level: descend normally.
            _ => {}
        }
    }

    fn begin(&mut self, f: Field, empty: bool) {
        if empty {
            // `<title/>` — nothing to capture, but an empty episode-num
            // should not leave a stale system around.
            self.episode_system.clear();
            return;
        }
        self.field = Some(f);
        self.field_depth = 0;
        self.text.clear();
    }

    fn text(&mut self, s: &str) {
        if self.skip_depth == 0 && self.field.is_some() {
            self.text.push_str(s);
        }
    }

    fn end(&mut self) {
        if self.skip_depth > 0 {
            self.skip_depth -= 1;
            return;
        }
        if self.field.is_some() {
            if self.field_depth > 0 {
                self.field_depth -= 1;
                return;
            }
            let field = self.field.take().expect("field checked above");
            let value = self.text.trim().to_string();
            self.text.clear();
            self.finish_field(field, value);
            return;
        }
        if let Some(ch) = self.channel.take() {
            self.doc.channels.push(ch);
            return;
        }
        if let Some(p) = self.programme.take() {
            self.doc.programmes.push(p);
        }
        // else: </tv> or stray end tag — nothing to do
    }

    fn finish_field(&mut self, field: Field, value: String) {
        match field {
            Field::DisplayName => {
                if let Some(ch) = self.channel.as_mut()
                    && !value.is_empty()
                {
                    ch.names.push(value);
                }
            }
            Field::Title => {
                if let Some(p) = self.programme.as_mut()
                    && p.title.is_empty()
                {
                    p.title = value;
                }
            }
            Field::SubTitle => {
                if let Some(p) = self.programme.as_mut()
                    && p.subtitle.is_none()
                    && !value.is_empty()
                {
                    p.subtitle = Some(value);
                }
            }
            Field::Desc => {
                if let Some(p) = self.programme.as_mut()
                    && p.desc.is_none()
                    && !value.is_empty()
                {
                    p.desc = Some(value);
                }
            }
            Field::Category => {
                if let Some(p) = self.programme.as_mut()
                    && !value.is_empty()
                    && !p.categories.iter().any(|c| c.eq_ignore_ascii_case(&value))
                {
                    p.categories.push(value);
                }
            }
            Field::EpisodeNum => {
                let system = std::mem::take(&mut self.episode_system);
                if let Some(p) = self.programme.as_mut()
                    && !value.is_empty()
                {
                    let replace = match (&p.episode_num, system.as_str()) {
                        (None, "xmltv_ns" | "onscreen") => true,
                        // xmltv_ns is exact; prefer it over an onscreen label seen earlier
                        (Some((prev, _)), "xmltv_ns") => prev != "xmltv_ns",
                        _ => false,
                    };
                    if replace {
                        p.episode_num = Some((system, value));
                    }
                }
            }
        }
    }
}

/// Streaming XMLTV parse.
pub fn parse_xmltv<R: BufRead>(reader: R) -> anyhow::Result<XmltvDoc> {
    let mut xml = quick_xml::Reader::from_reader(reader);
    {
        let cfg = xml.config_mut();
        cfg.allow_dangling_amp = true;
        cfg.check_end_names = false;
        cfg.trim_text(false);
    }
    let mut buf: Vec<u8> = Vec::with_capacity(64 * 1024);
    let mut st = ParseState::new();
    loop {
        buf.clear();
        let ev = xml
            .read_event_into(&mut buf)
            .map_err(|e| anyhow!("xmltv parse error at byte {}: {e}", xml.error_position()))?;
        match ev {
            Event::Start(e) => st.start(&e, false),
            Event::Empty(e) => st.start(&e, true),
            Event::End(_) => st.end(),
            Event::Text(t) => {
                if st.field.is_some() && st.skip_depth == 0 {
                    let s = t.xml10_content();
                    st.text(&s);
                }
            }
            Event::CData(c) => {
                if st.field.is_some() && st.skip_depth == 0 {
                    let s = c.into_inner();
                    st.text(&s);
                }
            }
            Event::GeneralRef(r) => {
                if st.field.is_some() && st.skip_depth == 0 {
                    match r.resolve_char_ref() {
                        Ok(Some(ch)) => {
                            let mut tmp = [0u8; 4];
                            st.text(ch.encode_utf8(&mut tmp));
                        }
                        Ok(None) => {
                            if let Some(s) = quick_xml::escape::resolve_predefined_entity(&r) {
                                st.text(s);
                            } else {
                                // unknown entity: keep it readable rather than dropping it
                                st.text("&");
                                st.text(&r);
                                st.text(";");
                            }
                        }
                        Err(_) => {}
                    }
                }
            }
            Event::Eof => break,
            Event::Comment(_) | Event::Decl(_) | Event::PI(_) | Event::DocType(_) => {}
        }
    }
    Ok(st.doc)
}

// ---------------------------------------------------------------------------
// Time
// ---------------------------------------------------------------------------

/// `20240115203000 +0000` / `20240115203000` (local) / with fractional seconds.
pub fn parse_xmltv_time(s: &str) -> Option<DateTime<Utc>> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let mut parts = s.split_whitespace();
    let stamp = parts.next()?;
    let tz = parts.next();

    // digits, optionally followed by a fractional part we ignore
    let (digits, _frac) = match stamp.find('.') {
        Some(i) => (&stamp[..i], Some(&stamp[i + 1..])),
        None => (stamp, None),
    };
    if digits.is_empty() || digits.len() > 14 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    // Tolerate truncated stamps: YYYYMMDDHHMM, YYYYMMDDHH, YYYYMMDD.
    if digits.len() < 8 || digits.len() % 2 != 0 {
        return None;
    }
    let mut padded = String::with_capacity(14);
    padded.push_str(digits);
    while padded.len() < 14 {
        padded.push('0');
    }
    let num = |a: usize, b: usize| padded[a..b].parse::<u32>().ok();
    let year = num(0, 4)? as i32;
    let month = num(4, 6)?;
    let day = num(6, 8)?;
    let hour = num(8, 10)?;
    let min = num(10, 12)?;
    let sec = num(12, 14)?;
    let naive: NaiveDateTime = NaiveDate::from_ymd_opt(year, month, day)?.and_hms_opt(hour, min, sec)?;

    match tz {
        Some(tz) => {
            let offset = parse_tz_offset(tz)?;
            match offset.from_local_datetime(&naive) {
                LocalResult::Single(dt) => Some(dt.with_timezone(&Utc)),
                LocalResult::Ambiguous(a, _) => Some(a.with_timezone(&Utc)),
                LocalResult::None => None,
            }
        }
        None => match chrono::Local.from_local_datetime(&naive) {
            LocalResult::Single(dt) => Some(dt.with_timezone(&Utc)),
            LocalResult::Ambiguous(a, _) => Some(a.with_timezone(&Utc)),
            // Inside a DST gap: shift forward an hour like most tools do.
            LocalResult::None => chrono::Local
                .from_local_datetime(&(naive + chrono::Duration::hours(1)))
                .earliest()
                .map(|dt| dt.with_timezone(&Utc)),
        },
    }
}

/// `+0530`, `-0800`, `+05:30`, `Z`, `UTC`, `GMT`.
fn parse_tz_offset(tz: &str) -> Option<FixedOffset> {
    let t = tz.trim();
    if t.eq_ignore_ascii_case("z") || t.eq_ignore_ascii_case("utc") || t.eq_ignore_ascii_case("gmt") {
        return FixedOffset::east_opt(0);
    }
    let (sign, rest) = match t.as_bytes().first()? {
        b'+' => (1i32, &t[1..]),
        b'-' => (-1i32, &t[1..]),
        _ => return None,
    };
    let rest = rest.replace(':', "");
    if !rest.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let (h, m) = match rest.len() {
        4 => (rest[..2].parse::<i32>().ok()?, rest[2..].parse::<i32>().ok()?),
        2 => (rest.parse::<i32>().ok()?, 0),
        _ => return None,
    };
    if h > 14 || m > 59 {
        return None;
    }
    FixedOffset::east_opt(sign * (h * 3600 + m * 60))
}

// ---------------------------------------------------------------------------
// Channel mapping
// ---------------------------------------------------------------------------

fn looks_like_guide_number(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit() || b == b'.') && s.bytes().any(|b| b.is_ascii_digit())
}

/// "7.1 KABC" → Some("KABC"); "KABC" → None.
fn strip_leading_number(s: &str) -> Option<&str> {
    let mut it = s.splitn(2, char::is_whitespace);
    let head = it.next()?;
    let rest = it.next()?.trim();
    if looks_like_guide_number(head) && !rest.is_empty() { Some(rest) } else { None }
}

/// Lowercase call sign with a trailing `DT`/`HD` (and `-DT`, ` HD`…) removed.
fn strip_callsign_suffix(lower: &str) -> Option<String> {
    let t = lower.trim();
    for suf in ["dt", "hd"] {
        if let Some(base) = t.strip_suffix(suf) {
            let base = base.trim_end_matches([' ', '-', '_']).trim();
            if base.len() >= 2 {
                return Some(base.to_string());
            }
        }
    }
    None
}

/// xmltv channel id → HDHR GuideNumber, matching display-names against the
/// lineup's number ("7.1") and name ("KABCDT"), case-insensitively.
pub fn map_channels(channels: &[XmltvChannel], lineup: &[Channel]) -> HashMap<String, String> {
    let mut by_number: HashMap<&str, &str> = HashMap::new();
    let mut by_name: HashMap<String, &str> = HashMap::new();
    let mut by_stripped: HashMap<String, &str> = HashMap::new();
    for c in lineup {
        let num = c.guide_number.trim();
        if num.is_empty() {
            continue;
        }
        by_number.entry(num).or_insert(num);
        let name = fold(&c.guide_name);
        if !name.is_empty() {
            by_name.entry(name.clone()).or_insert(num);
            if let Some(base) = strip_callsign_suffix(&name) {
                by_stripped.entry(base).or_insert(num);
            }
        }
    }

    let mut out = HashMap::with_capacity(channels.len());
    for ch in channels {
        if ch.id.is_empty() {
            continue;
        }
        let names: Vec<&str> = ch.names.iter().map(|s| s.trim()).filter(|s| !s.is_empty()).collect();
        let id = [ch.id.trim()];

        // Display-names are authoritative; the id (which some grabbers set to a
        // bare numeric station id that can collide with a cable GuideNumber)
        // is only consulted when no display-name matched anything.
        let found = match_candidates(&names, &by_number, &by_name, &by_stripped)
            .or_else(|| match_candidates(&id, &by_number, &by_name, &by_stripped));

        if let Some(num) = found {
            out.insert(ch.id.clone(), num.to_string());
        }
    }
    out
}

/// Three-stage match of a candidate list against the lineup indexes.
fn match_candidates<'a>(
    candidates: &[&str],
    by_number: &HashMap<&str, &'a str>,
    by_name: &HashMap<String, &'a str>,
    by_stripped: &HashMap<String, &'a str>,
) -> Option<&'a str> {
    // 1. exact guide number on any candidate (or its leading token)
    let found = candidates.iter().find_map(|n| {
        by_number.get(n).copied().or_else(|| {
            let head = n.split_whitespace().next()?;
            if looks_like_guide_number(head) { by_number.get(head).copied() } else { None }
        })
    });
    if found.is_some() {
        return found;
    }

    // 2. case-insensitive name match, incl. "7.1 KABC" → "KABC"
    let found = candidates.iter().find_map(|n| {
        let l = fold(n);
        by_name.get(&l).copied().or_else(|| strip_leading_number(n).and_then(|r| by_name.get(&fold(r)).copied()))
    });
    if found.is_some() {
        return found;
    }

    // 3. call sign with DT/HD stripped on either side
    candidates.iter().find_map(|n| {
        let l = fold(strip_leading_number(n).unwrap_or(n));
        let stripped = strip_callsign_suffix(&l);
        by_stripped
            .get(&l)
            .copied()
            .or_else(|| stripped.as_ref().and_then(|s| by_name.get(s).copied()))
            .or_else(|| stripped.as_ref().and_then(|s| by_stripped.get(s).copied()))
    })
}

// ---------------------------------------------------------------------------
// Episode numbers
// ---------------------------------------------------------------------------

/// Season/episode from `xmltv_ns` ("1.4.0/1" → S2E5) or `onscreen` ("S02E05").
pub fn parse_episode_num(system: &str, value: &str) -> (Option<i32>, Option<i32>) {
    let value = value.trim();
    match system.trim().to_ascii_lowercase().as_str() {
        "xmltv_ns" => {
            let mut parts = value.split('.');
            let season = parts.next().and_then(xmltv_ns_part);
            let episode = parts.next().and_then(xmltv_ns_part);
            (season, episode)
        }
        "onscreen" => {
            let b = value.as_bytes();
            let mut i = 0;
            if i < b.len() && (b[i] == b'S' || b[i] == b's') {
                i += 1;
                let s0 = i;
                while i < b.len() && b[i].is_ascii_digit() {
                    i += 1;
                }
                let season: Option<i32> = value[s0..i].parse().ok();
                while i < b.len() && (b[i] == b' ' || b[i] == b'.' || b[i] == b'-' || b[i] == b'_') {
                    i += 1;
                }
                if season.is_some() && i < b.len() && (b[i] == b'E' || b[i] == b'e') {
                    i += 1;
                    let e0 = i;
                    while i < b.len() && b[i].is_ascii_digit() {
                        i += 1;
                    }
                    if let Ok(episode) = value[e0..i].parse::<i32>() {
                        return (season, Some(episode));
                    }
                }
            }
            (None, None)
        }
        _ => (None, None),
    }
}

/// One `xmltv_ns` component: zero-based index, optional `/total`, may be blank.
fn xmltv_ns_part(p: &str) -> Option<i32> {
    let p = p.split('/').next()?.trim();
    if p.is_empty() {
        return None;
    }
    let n: i32 = p.parse().ok()?;
    if n < 0 { None } else { n.checked_add(1) }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Settings;
    use chrono::Duration;
    use std::io::Write;

    fn settings() -> Arc<SettingsCache> {
        let pool = sqlx::PgPool::connect_lazy("postgres://ontele:ontele@127.0.0.1:1/ontele").expect("lazy pool");
        Arc::new(SettingsCache::new(pool, Settings::default()))
    }

    fn ch(num: &str, name: &str) -> Channel {
        Channel {
            guide_number: num.into(),
            guide_name: name.into(),
            url: format!("http://t/auto/v{num}"),
            hd: true,
            icon: None,
        }
    }

    fn xc(id: &str, names: &[&str]) -> XmltvChannel {
        XmltvChannel { id: id.into(), names: names.iter().map(|s| s.to_string()).collect(), icon: None }
    }

    fn airing(ch: &str, title: &str, start: DateTime<Utc>, mins: i64) -> Airing {
        Airing {
            channel_id: ch.into(),
            title: title.into(),
            subtitle: None,
            description: None,
            start,
            end: start + Duration::minutes(mins),
            categories: vec![],
            season: None,
            episode: None,
            icon: None,
            new: false,
        }
    }

    fn t0() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2024, 1, 15, 20, 0, 0).unwrap()
    }

    const DOC: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE tv SYSTEM "xmltv.dtd">
<tv source-info-name="test" generator-info-name="ontele">
  <channel id="I7.1.KABC">
    <display-name>7.1 KABC</display-name>
    <display-name>KABCDT</display-name>
    <display-name>7.1</display-name>
    <icon src="http://img/kabc.png" width="100"/>
  </channel>
  <channel id="I2.1.KCBS">
    <display-name lang="en">KCBS</display-name>
    <icon src="http://img/kcbs.png"></icon>
  </channel>
  <programme start="20240115200000 +0000" stop="20240115203000 +0000" channel="I7.1.KABC">
    <title lang="en">Tom &amp; Jerry &#8212; Classics</title>
    <sub-title>The &lt;Cat&gt; Concerto</sub-title>
    <desc>  A desc with
      newline.  </desc>
    <category lang="en">Animated</category>
    <category>Children</category>
    <category>animated</category>
    <episode-num system="onscreen">S02E05</episode-num>
    <episode-num system="xmltv_ns">1.4.0/1</episode-num>
    <episode-num system="dd_progid">EP001.0005</episode-num>
    <credits><actor>Someone</actor><title>Ignored nested title</title></credits>
    <icon src="http://img/tj.jpg"/>
    <new/>
  </programme>
  <programme start="20240115203000 +0000" stop="20240115210000 +0000" channel="I7.1.KABC">
    <title><![CDATA[News & Weather]]></title>
    <previously-shown start="20240101000000 +0000"/>
  </programme>
  <programme start="20240115200000 +0000" stop="20240115220000 +0000" channel="I2.1.KCBS">
    <title>Movie Night</title>
    <episode-num system="onscreen">s3e12</episode-num>
  </programme>
</tv>"#;

    #[test]
    fn time_with_tz() {
        assert_eq!(
            parse_xmltv_time("20240115203000 +0000"),
            Some(Utc.with_ymd_and_hms(2024, 1, 15, 20, 30, 0).unwrap())
        );
        assert_eq!(
            parse_xmltv_time("20240115203000 -0800"),
            Some(Utc.with_ymd_and_hms(2024, 1, 16, 4, 30, 0).unwrap())
        );
        assert_eq!(
            parse_xmltv_time("20240115203000 +0530"),
            Some(Utc.with_ymd_and_hms(2024, 1, 15, 15, 0, 0).unwrap())
        );
        assert_eq!(
            parse_xmltv_time(" 20240115203000  +05:30 "),
            Some(Utc.with_ymd_and_hms(2024, 1, 15, 15, 0, 0).unwrap())
        );
        assert_eq!(parse_xmltv_time("20240115203000 Z"), Some(Utc.with_ymd_and_hms(2024, 1, 15, 20, 30, 0).unwrap()));
        // fractional seconds and short forms
        assert_eq!(
            parse_xmltv_time("20240115203000.500 +0000"),
            Some(Utc.with_ymd_and_hms(2024, 1, 15, 20, 30, 0).unwrap())
        );
        assert_eq!(parse_xmltv_time("202401152030 +0000"), Some(Utc.with_ymd_and_hms(2024, 1, 15, 20, 30, 0).unwrap()));
        assert_eq!(parse_xmltv_time("20240115 +0000"), Some(Utc.with_ymd_and_hms(2024, 1, 15, 0, 0, 0).unwrap()));
    }

    #[test]
    fn time_without_tz_is_local() {
        let naive = NaiveDate::from_ymd_opt(2024, 6, 1).unwrap().and_hms_opt(12, 0, 0).unwrap();
        let expect = chrono::Local.from_local_datetime(&naive).single().map(|d| d.with_timezone(&Utc));
        assert_eq!(parse_xmltv_time("20240601120000"), expect);
    }

    #[test]
    fn time_invalid() {
        assert_eq!(parse_xmltv_time(""), None);
        assert_eq!(parse_xmltv_time("garbage"), None);
        assert_eq!(parse_xmltv_time("20241315203000 +0000"), None); // month 13
        assert_eq!(parse_xmltv_time("20240115253000 +0000"), None); // hour 25
        assert_eq!(parse_xmltv_time("2024011 +0000"), None); // odd length
        assert_eq!(parse_xmltv_time("20240115203000 +9900"), None);
        assert_eq!(parse_xmltv_time("20240115203000 PST"), None);
        // non-ASCII tz token must not panic on byte slicing
        assert_eq!(parse_xmltv_time("20240115203000 +0\u{e9}0"), None);
        assert_eq!(parse_xmltv_time("20240115203000 +\u{e9}\u{e9}"), None);
        assert_eq!(parse_xmltv_time("20240115203000 +0\u{e9}"), None);
    }

    #[test]
    fn parse_inline_doc() {
        let doc = parse_xmltv(std::io::Cursor::new(DOC)).unwrap();
        assert_eq!(doc.channels.len(), 2);
        assert_eq!(doc.channels[0].id, "I7.1.KABC");
        assert_eq!(doc.channels[0].names, vec!["7.1 KABC", "KABCDT", "7.1"]);
        assert_eq!(doc.channels[0].icon.as_deref(), Some("http://img/kabc.png"));
        assert_eq!(doc.channels[1].names, vec!["KCBS"]);
        assert_eq!(doc.channels[1].icon.as_deref(), Some("http://img/kcbs.png"));

        assert_eq!(doc.programmes.len(), 3);
        let p = &doc.programmes[0];
        assert_eq!(p.channel, "I7.1.KABC");
        assert_eq!(p.start, "20240115200000 +0000");
        assert_eq!(p.stop, "20240115203000 +0000");
        assert_eq!(p.title, "Tom & Jerry — Classics");
        assert_eq!(p.subtitle.as_deref(), Some("The <Cat> Concerto"));
        assert_eq!(p.desc.as_deref(), Some("A desc with\n      newline."));
        assert_eq!(p.categories, vec!["Animated", "Children"]);
        assert_eq!(p.episode_num, Some(("xmltv_ns".into(), "1.4.0/1".into())));
        assert_eq!(p.icon.as_deref(), Some("http://img/tj.jpg"));
        assert!(p.new);

        let p = &doc.programmes[1];
        assert_eq!(p.title, "News & Weather");
        assert!(!p.new);
        assert_eq!(p.episode_num, None);

        let p = &doc.programmes[2];
        assert_eq!(p.title, "Movie Night");
        assert_eq!(p.episode_num, Some(("onscreen".into(), "s3e12".into())));
    }

    #[test]
    fn parse_gz_variant() {
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        enc.write_all(DOC.as_bytes()).unwrap();
        let gz = enc.finish().unwrap();
        let plain = parse_xmltv(std::io::Cursor::new(DOC)).unwrap();
        let via_gz =
            parse_xmltv(BufReader::new(flate2::read::GzDecoder::new(std::io::Cursor::new(gz.clone())))).unwrap();
        assert_eq!(plain, via_gz);

        // and through the file path (magic-byte detection, no .gz suffix)
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("guide.xml");
        std::fs::write(&path, &gz).unwrap();
        let lineup = vec![ch("7.1", "KABCDT"), ch("2.1", "KCBS")];
        let built = build_from_file(&path, false, &lineup).unwrap();
        assert_eq!(built.count, 3);
        assert_eq!(built.icons.get("7.1").map(String::as_str), Some("http://img/kabc.png"));
        assert_eq!(built.icons.get("2.1").map(String::as_str), Some("http://img/kcbs.png"));
        // .gz suffix hint but plain content must still parse
        let plain_path = dir.path().join("guide2.xml.gz");
        std::fs::write(&plain_path, DOC.as_bytes()).unwrap();
        assert_eq!(build_from_file(&plain_path, true, &lineup).unwrap().count, 3);
        // an HTML error page (well-formed but not XMLTV) must be an error, not an empty guide
        let html_path = dir.path().join("error.html");
        std::fs::write(&html_path, b"<html><body><h1>503 Service Unavailable</h1></body></html>").unwrap();
        match build_from_file(&html_path, false, &lineup) {
            Ok(b) => panic!("expected error, got {} airings", b.count),
            Err(err) => assert!(format!("{err:#}").contains("no <channel> or <programme>"), "{err:#}"),
        }
        let empty_path = dir.path().join("empty.xml");
        std::fs::write(&empty_path, b"").unwrap();
        assert!(build_from_file(&empty_path, false, &lineup).is_err());
    }

    #[test]
    fn parse_malformed_is_error_not_panic() {
        // truncated inside a tag is a hard error (no panic)
        let r = parse_xmltv(std::io::Cursor::new(
            "<tv><channel id=\"x\"><display-name>abc</display-name></channel><programme start=\"a\"",
        ));
        assert!(r.is_err(), "{r:?}");
        // truncated inside text: whatever is complete is kept
        let doc = parse_xmltv(std::io::Cursor::new("<tv><channel id=\"x\"><display-name>abc</display-name></channel><programme start=\"a\" stop=\"b\" channel=\"c\"><title>unfinished")).unwrap();
        assert_eq!(doc.channels.len(), 1);
        assert!(doc.programmes.is_empty());
        // unterminated attribute value
        assert!(parse_xmltv(std::io::Cursor::new("<tv><programme start=\"a></tv>")).is_err());
        // empty input
        assert_eq!(parse_xmltv(std::io::Cursor::new("")).unwrap(), XmltvDoc::default());
        // unknown entity is kept verbatim, dangling ampersand tolerated
        let doc = parse_xmltv(std::io::Cursor::new("<tv><programme start=\"20240101000000 +0000\" stop=\"20240101010000 +0000\" channel=\"c\"><title>A &nbsp; B & C</title></programme></tv>")).unwrap();
        assert_eq!(doc.programmes[0].title, "A &nbsp; B & C");
    }

    #[test]
    fn map_channels_cases() {
        let lineup = vec![
            ch("7.1", "KABCDT"),
            ch("2.1", "KCBS"),
            ch("4.1", "KNBC-HD"),
            ch("1001", "WEATHER"),
            ch("5.1", "KTLA"),
        ];
        let xml = vec![
            xc("a", &["7.1 KABC"]),                // leading number exact
            xc("b", &["kcbs"]),                    // case-insensitive name
            xc("c", &["KABC"]),                    // lineup name has DT suffix
            xc("d", &["KNBC"]),                    // lineup name has -HD suffix
            xc("e", &["4.1 KNBCDT"]),              // leading number + DT vs -HD
            xc("f", &["Weather Channel", "1001"]), // second display-name is the number
            xc("g", &["KTLAHD"]),                  // xmltv has HD suffix, lineup bare
            xc("h", &["Nothing"]),                 // unmapped
            xc("5.1", &[]),                        // id itself is the number
            xc("", &["KTLA"]),                     // empty id ignored
        ];
        let m = map_channels(&xml, &lineup);
        assert_eq!(m.get("a").map(String::as_str), Some("7.1"));
        assert_eq!(m.get("b").map(String::as_str), Some("2.1"));
        assert_eq!(m.get("c").map(String::as_str), Some("7.1"));
        assert_eq!(m.get("d").map(String::as_str), Some("4.1"));
        assert_eq!(m.get("e").map(String::as_str), Some("4.1"));
        assert_eq!(m.get("f").map(String::as_str), Some("1001"));
        assert_eq!(m.get("g").map(String::as_str), Some("5.1"));
        assert_eq!(m.get("h"), None);
        assert_eq!(m.get("5.1").map(String::as_str), Some("5.1"));
        assert_eq!(m.len(), 8);
        // number match beats a misleading name
        let lineup2 = vec![ch("7.1", "KCBS"), ch("2.1", "KABC")];
        let m = map_channels(&[xc("x", &["KABC", "7.1"])], &lineup2);
        assert_eq!(m.get("x").map(String::as_str), Some("7.1"));
        // a numeric station id must not outrank a display-name match
        let lineup3 = vec![ch("1001", "WEATHER"), ch("2.1", "KCBS")];
        let m = map_channels(&[xc("1001", &["KCBS"]), xc("1001x", &["nomatch"]), xc("2.1", &["nomatch"])], &lineup3);
        assert_eq!(m.get("1001").map(String::as_str), Some("2.1"));
        assert_eq!(m.get("1001x"), None);
        assert_eq!(m.get("2.1").map(String::as_str), Some("2.1"), "id still used as fallback");
    }

    #[test]
    fn episode_numbers() {
        assert_eq!(parse_episode_num("xmltv_ns", "1.4.0/1"), (Some(2), Some(5)));
        assert_eq!(parse_episode_num("xmltv_ns", "1 . 4 . 0"), (Some(2), Some(5)));
        assert_eq!(parse_episode_num("xmltv_ns", ".4."), (None, Some(5)));
        assert_eq!(parse_episode_num("xmltv_ns", "3/10..0"), (Some(4), None));
        assert_eq!(parse_episode_num("xmltv_ns", "x.y"), (None, None));
        assert_eq!(parse_episode_num("xmltv_ns", "-1.2"), (None, Some(3)));
        assert_eq!(parse_episode_num("onscreen", "S02E05"), (Some(2), Some(5)));
        assert_eq!(parse_episode_num("onscreen", "s2e5"), (Some(2), Some(5)));
        assert_eq!(parse_episode_num("onscreen", "S2 E5"), (Some(2), Some(5)));
        assert_eq!(parse_episode_num("onscreen", "205"), (None, None));
        assert_eq!(parse_episode_num("onscreen", "E5"), (None, None));
        assert_eq!(parse_episode_num("onscreen", "S2"), (None, None));
        assert_eq!(parse_episode_num("dd_progid", "EP001.0005"), (None, None));
        assert_eq!(parse_episode_num("ONSCREEN", "S01E01"), (Some(1), Some(1)));
    }

    #[test]
    fn build_index_skips_bad_programmes() {
        let mut doc = parse_xmltv(std::io::Cursor::new(DOC)).unwrap();
        // invalid time, end <= start, unmapped channel, duplicate (channel,start)
        doc.programmes.push(XmltvProgramme {
            channel: "I7.1.KABC".into(),
            start: "bad".into(),
            stop: "bad".into(),
            title: "x".into(),
            ..Default::default()
        });
        doc.programmes.push(XmltvProgramme {
            channel: "I7.1.KABC".into(),
            start: "20240115210000 +0000".into(),
            stop: "20240115210000 +0000".into(),
            title: "x".into(),
            ..Default::default()
        });
        doc.programmes.push(XmltvProgramme {
            channel: "nope".into(),
            start: "20240115210000 +0000".into(),
            stop: "20240115220000 +0000".into(),
            title: "x".into(),
            ..Default::default()
        });
        doc.programmes.push(XmltvProgramme {
            channel: "I7.1.KABC".into(),
            start: "20240115200000 +0000".into(),
            stop: "20240115203000 +0000".into(),
            title: "dup".into(),
            ..Default::default()
        });
        let built = build_index(doc, &[ch("7.1", "KABCDT"), ch("2.1", "KCBS")]);
        assert_eq!(built.count, 3);
        assert_eq!(built.skipped_invalid, 2);
        assert_eq!(built.skipped_unmapped, 1);
        let kabc = &built.by_channel["7.1"];
        assert_eq!(kabc.len(), 2);
        assert_eq!(kabc[0].title, "Tom & Jerry — Classics");
        assert_eq!((kabc[0].season, kabc[0].episode), (Some(2), Some(5)));
        assert!(kabc[0].new);
        assert_eq!(kabc[0].categories, vec!["Animated", "Children"]);
        assert!(kabc[0].start < kabc[1].start);
        let kcbs = &built.by_channel["2.1"];
        assert_eq!((kcbs[0].season, kcbs[0].episode), (Some(3), Some(12)));
    }

    fn synthetic_guide() -> Guide {
        let t = t0();
        let mut m = HashMap::new();
        m.insert(
            "7.1".to_string(),
            vec![
                airing("7.1", "Morning Show", t - Duration::hours(2), 60),
                airing("7.1", "Noon News", t - Duration::hours(1), 60),
                airing("7.1", "Jeopardy!", t, 30),
                airing("7.1", "Wheel of Fortune", t + Duration::minutes(30), 30),
                airing("7.1", "Jeopardy!", t + Duration::hours(1), 30),
                airing("7.1", "Late Movie", t + Duration::hours(2), 120),
            ],
        );
        m.insert(
            "2.1".to_string(),
            vec![airing("2.1", "jeopardy!", t - Duration::minutes(10), 40), {
                let mut a = airing("2.1", "Evening News", t + Duration::minutes(30), 60);
                a.subtitle = Some("Jeopardy winners".into());
                a
            }],
        );
        // unsorted + duplicate start on purpose: with_airings must normalize
        m.insert(
            "9.9".to_string(),
            vec![
                airing("9.9", "B", t + Duration::hours(1), 60),
                airing("9.9", "A", t, 60),
                airing("9.9", "A-dup", t, 60),
            ],
        );
        Guide::with_airings(settings(), reqwest::Client::new(), m)
    }

    #[tokio::test]
    async fn with_airings_normalizes() {
        let g = synthetic_guide();
        let w = g.window(t0() - Duration::hours(1), t0() + Duration::hours(5));
        let nine: Vec<_> = w["9.9"].iter().map(|a| a.title.as_str()).collect();
        assert_eq!(nine, vec!["A", "B"]);
        assert!(g.updated().is_some());
        assert!(g.channel_icons().is_empty());
    }

    #[tokio::test]
    async fn window_overlap() {
        let g = synthetic_guide();
        let t = t0();
        // [t+10m, t+50m] → Jeopardy (t..t+30) and Wheel (t+30..t+60)
        let w = g.window(t + Duration::minutes(10), t + Duration::minutes(50));
        let titles: Vec<_> = w["7.1"].iter().map(|a| a.title.as_str()).collect();
        assert_eq!(titles, vec!["Jeopardy!", "Wheel of Fortune"]);
        // an airing that started before `from` but is still running is included
        let w = g.window(t + Duration::minutes(15), t + Duration::minutes(16));
        assert_eq!(w["7.1"].len(), 1);
        assert_eq!(w["7.1"][0].title, "Jeopardy!");
        // exact boundary: airing ending at `from` is excluded, starting at `to` excluded
        let w = g.window(t + Duration::minutes(30), t + Duration::hours(1));
        let titles: Vec<_> = w["7.1"].iter().map(|a| a.title.as_str()).collect();
        assert_eq!(titles, vec!["Wheel of Fortune"]);
        // far future: nothing
        let w = g.window(t + Duration::days(3), t + Duration::days(4));
        assert!(w.is_empty());
        // inverted range: nothing
        assert!(g.window(t + Duration::hours(1), t).is_empty());
        // past range covers the first airing
        let w = g.window(t - Duration::hours(3), t - Duration::minutes(90));
        assert_eq!(w["7.1"][0].title, "Morning Show");
    }

    #[tokio::test]
    async fn now_next_cases() {
        let g = synthetic_guide();
        let t = t0();
        let (cur, next) = g.now_next("7.1", t + Duration::minutes(5));
        assert_eq!(cur.unwrap().title, "Jeopardy!");
        assert_eq!(next.unwrap().title, "Wheel of Fortune");
        // exactly at a boundary: the airing starting now is current
        let (cur, next) = g.now_next("7.1", t + Duration::minutes(30));
        assert_eq!(cur.unwrap().title, "Wheel of Fortune");
        assert_eq!(next.unwrap().title, "Jeopardy!");
        // in a gap (t+90m..t+120m) → no current, next is Late Movie
        let (cur, next) = g.now_next("7.1", t + Duration::minutes(100));
        assert!(cur.is_none());
        assert_eq!(next.unwrap().title, "Late Movie");
        // before everything
        let (cur, next) = g.now_next("7.1", t - Duration::days(1));
        assert!(cur.is_none());
        assert_eq!(next.unwrap().title, "Morning Show");
        // after everything
        let (cur, next) = g.now_next("7.1", t + Duration::days(1));
        assert!(cur.is_none());
        assert!(next.is_none());
        // last airing running, no next
        let (cur, next) = g.now_next("7.1", t + Duration::hours(3));
        assert_eq!(cur.unwrap().title, "Late Movie");
        assert!(next.is_none());
        // unknown channel
        assert_eq!(g.now_next("nope", t), (None, None));
    }

    #[tokio::test]
    async fn matches_cases() {
        let g = synthetic_guide();
        let t = t0();
        let m = g.matches("  jeopardy! ", None, t - Duration::minutes(5));
        // 2.1 (running), 7.1 @t, 7.1 @t+1h — ordered by start
        let got: Vec<_> = m.iter().map(|a| (a.channel_id.as_str(), a.start)).collect();
        assert_eq!(got, vec![("2.1", t - Duration::minutes(10)), ("7.1", t), ("7.1", t + Duration::hours(1))]);
        // only upcoming/running (end > now)
        let m = g.matches("Jeopardy!", None, t + Duration::minutes(31));
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].start, t + Duration::hours(1));
        // channel filter
        let m = g.matches("JEOPARDY!", Some("2.1"), t - Duration::hours(1));
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].channel_id, "2.1");
        assert!(g.matches("Jeopardy!", Some("nope"), t).is_empty());
        assert!(g.matches("", None, t).is_empty());
        assert!(g.matches("Jeopardy", None, t).is_empty(), "equality, not substring");
    }

    #[tokio::test]
    async fn search_cases() {
        let g = synthetic_guide();
        let t = t0();
        let r = g.search("jeop", t - Duration::hours(1), 10);
        let titles: Vec<_> = r.iter().map(|a| a.title.as_str()).collect();
        // dedup by title (case-insensitive): earliest Jeopardy (2.1) then Evening News via subtitle
        assert_eq!(titles, vec!["jeopardy!", "Evening News"]);
        assert_eq!(r[0].channel_id, "2.1");
        // limit
        assert_eq!(g.search("jeop", t - Duration::hours(1), 1).len(), 1);
        assert!(g.search("jeop", t, 0).is_empty());
        // only upcoming: after all Jeopardy airings ended only the subtitle hit remains... which also ended
        assert!(g.search("jeop", t + Duration::hours(2), 10).is_empty());
        // past airings are excluded even though they match
        let r = g.search("morning", t, 10);
        assert!(r.is_empty());
        assert!(g.search("   ", t, 10).is_empty());
        // substring inside a word
        let r = g.search("MOVIE", t, 10);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].title, "Late Movie");
    }

    #[tokio::test]
    async fn refresh_without_source_is_noop() {
        let g = Guide::new(settings(), reqwest::Client::new());
        assert_eq!(g.refresh(&[ch("7.1", "KABCDT")]).await.unwrap(), 0);
        assert!(g.updated().is_none());
    }

    #[tokio::test]
    async fn refresh_from_local_file_and_http() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("guide.xml");
        std::fs::write(&path, DOC.as_bytes()).unwrap();
        let s = settings();
        let mut cfg = (*s.get()).clone();
        cfg.xmltv_url = path.to_string_lossy().to_string();
        // avoid touching the DB: build a cache seeded with the desired settings
        let pool = sqlx::PgPool::connect_lazy("postgres://ontele:ontele@127.0.0.1:1/ontele").unwrap();
        let s = Arc::new(SettingsCache::new(pool.clone(), cfg.clone()));
        let g = Guide::new(s, reqwest::Client::new());
        let lineup = vec![ch("7.1", "KABCDT"), ch("2.1", "KCBS")];
        assert_eq!(g.refresh(&lineup).await.unwrap(), 3);
        assert_eq!(g.channel_icons().get("7.1").map(String::as_str), Some("http://img/kabc.png"));
        let (cur, next) = g.now_next("7.1", Utc.with_ymd_and_hms(2024, 1, 15, 20, 10, 0).unwrap());
        assert_eq!(cur.unwrap().title, "Tom & Jerry — Classics");
        assert_eq!(next.unwrap().title, "News & Weather");

        // missing file → error
        cfg.xmltv_url = dir.path().join("missing.xml").to_string_lossy().to_string();
        let g2 = Guide::new(Arc::new(SettingsCache::new(pool.clone(), cfg.clone())), reqwest::Client::new());
        assert!(g2.refresh(&lineup).await.is_err());

        // HTTP (gzipped) through a local server; temp download must be cleaned up
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        enc.write_all(DOC.as_bytes()).unwrap();
        let gz = enc.finish().unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = axum::Router::new()
            .route(
                "/guide.xml.gz",
                axum::routing::get(move || {
                    let gz = gz.clone();
                    async move { gz }
                }),
            )
            .route("/missing.xml", axum::routing::get(|| async { axum::http::StatusCode::NOT_FOUND }));
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        cfg.xmltv_url = format!("http://{addr}/guide.xml.gz");
        let g3 = Guide::new(Arc::new(SettingsCache::new(pool.clone(), cfg.clone())), reqwest::Client::new());
        assert_eq!(g3.refresh(&lineup).await.unwrap(), 3);
        let w = g3.window(
            Utc.with_ymd_and_hms(2024, 1, 15, 19, 0, 0).unwrap(),
            Utc.with_ymd_and_hms(2024, 1, 15, 23, 0, 0).unwrap(),
        );
        assert_eq!(w["7.1"].len(), 2);
        assert_eq!(w["2.1"].len(), 1);
        let leftovers: Vec<_> = std::fs::read_dir(std::env::temp_dir())
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().starts_with(&format!("ontele-xmltv-{}-", std::process::id())))
            .collect();
        assert!(leftovers.is_empty(), "temp guide files left behind: {leftovers:?}");
        cfg.xmltv_url = format!("http://{addr}/missing.xml");
        let g4 = Guide::new(Arc::new(SettingsCache::new(pool, cfg)), reqwest::Client::new());
        assert!(g4.refresh(&lineup).await.is_err());
        server.abort();
    }

    #[test]
    fn hdhr_guide_builds_airings() {
        let slice1: Vec<HdhrGuideChannel> = serde_json::from_value(serde_json::json!([
            {"GuideNumber": "2.1", "GuideName": "KCBS", "ImageURL": "http://img/2.1.png", "Guide": [
                {"StartTime": 1700000000, "EndTime": 1700003600, "Title": "Evening News",
                 "Synopsis": "News.", "Filter": ["News"], "OriginalAirdate": 1700000000},
                {"StartTime": 1700003600, "EndTime": 1700007200, "Title": "Static Signal",
                 "EpisodeTitle": "Cold Boot", "EpisodeNumber": "S02E05", "OriginalAirdate": 1500000000}
            ]},
            {"GuideNumber": "99.9", "GuideName": "NotInLineup", "Guide": [
                {"StartTime": 1700000000, "EndTime": 1700003600, "Title": "Ghost"}
            ]}
        ]))
        .unwrap();
        // second slice overlaps the first: the duplicate airing must collapse
        let slice2: Vec<HdhrGuideChannel> = serde_json::from_value(serde_json::json!([
            {"GuideNumber": "2.1", "Guide": [
                {"StartTime": 1700003600, "EndTime": 1700007200, "Title": "Static Signal"},
                {"StartTime": 1700007200, "EndTime": 1700010800, "Title": "Late Show"},
                {"StartTime": 0, "EndTime": 100, "Title": "invalid"}
            ]}
        ]))
        .unwrap();
        let lineup = vec![ch("2.1", "KCBS")];
        let built = build_from_hdhr(&[slice1, slice2], &lineup);
        let a = &built.by_channel["2.1"];
        assert_eq!(a.len(), 3, "dedupe across slices, invalid dropped");
        assert_eq!(a[0].title, "Evening News");
        assert!(a[0].new, "orig airdate == start ⇒ first run");
        assert_eq!(a[1].subtitle.as_deref(), Some("Cold Boot"));
        assert_eq!((a[1].season, a[1].episode), (Some(2), Some(5)));
        assert!(!a[1].new, "old airdate ⇒ repeat");
        assert_eq!(built.icons["2.1"], "http://img/2.1.png");
        assert_eq!(built.skipped_unmapped, 1);
        assert_eq!(built.skipped_invalid, 1);
        assert_eq!(built.count, 3);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn hdhr_guide_fallback_when_no_xmltv(pool: sqlx::PgPool) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = axum::Router::new().route(
            "/api/guide",
            axum::routing::get(|q: axum::extract::RawQuery| async move {
                let q = q.0.unwrap_or_default();
                assert!(q.contains("DeviceAuth=tok"), "auth forwarded: {q}");
                // one slice, then empty ⇒ loop stops
                let first = !FIRST_DONE.swap(true, std::sync::atomic::Ordering::SeqCst);
                if first {
                    axum::Json(serde_json::json!([
                        {"GuideNumber": "2.1", "Guide": [
                            {"StartTime": chrono::Utc::now().timestamp(),
                             "EndTime": chrono::Utc::now().timestamp() + 3600, "Title": "Now Showing"}
                        ]}
                    ]))
                } else {
                    axum::Json(serde_json::json!([]))
                }
            }),
        );
        static FIRST_DONE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let cfg = Settings { xmltv_url: String::new(), ..Default::default() };
        let mut g = Guide::new(Arc::new(SettingsCache::new(pool, cfg)), reqwest::Client::new());
        g.guide_api = format!("http://{addr}/api/guide");
        let lineup = vec![ch("2.1", "KCBS")];
        // no auth ⇒ quiet skip; with auth ⇒ scraped from the tuner API
        assert_eq!(g.refresh_with_hdhr(&lineup, None).await.unwrap(), 0);
        assert_eq!(g.refresh_with_hdhr(&lineup, Some("tok")).await.unwrap(), 1);
        let (now, _) = g.now_next("2.1", Utc::now());
        assert_eq!(now.unwrap().title, "Now Showing");
        server.abort();
    }
}
