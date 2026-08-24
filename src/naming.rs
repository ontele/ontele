// Copyright 2026 The Ontele Authors
// SPDX-License-Identifier: Apache-2.0

//! Filename intelligence. Turns release-style names into clean titles and
//! classifies files as movies, episodes or music tracks using the file name
//! *and* its parent folders. Everything here is pure and unit-tested.
//!
//! Handled episode shapes: `S01E02`, `S01E02E03`, `S01E02-E03`, `1x02`,
//! `Season 1/Episode 02`, `Show - 102` is *not* guessed (too ambiguous),
//! date-based `2024-01-15`, anime `[Group] Show - 07 [1080p]` (absolute →
//! season 1). Movie shapes: `Title (2019)`, `Title.2019.2160p.Remux-GRP`,
//! `Title (2019)/Title.mkv`, `Title (2019)/movie.mkv`.

use chrono::NaiveDate;
use regex::Regex;
use std::path::Path;
use std::sync::LazyLock;

pub const VIDEO_EXTS: &[&str] = &[
    "mkv", "mp4", "m4v", "mov", "avi", "ts", "m2ts", "mts", "webm", "wmv", "flv", "ogv", "ogm", "3gp", "3g2", "mpg",
    "mpeg", "mpe", "m2v", "vob", "divx", "asf", "rm", "rmvb", "mxf", "dv", "f4v", "mp2", "qt", "nut", "y4m", "hevc",
    "h264", "264", "265", "av1", "ivf",
];
pub const AUDIO_EXTS: &[&str] = &[
    "mp3", "flac", "m4a", "m4b", "aac", "ogg", "oga", "opus", "wav", "aiff", "aif", "wma", "ape", "wv", "mpc", "dsf",
    "dff", "tta", "tak", "spx", "ac3", "dts", "mka", "alac", "caf", "amr", "au", "mp2", "m4p", "aa3",
];
pub const SUBTITLE_EXTS: &[&str] = &["srt", "vtt", "ass", "ssa", "sub", "sup", "idx"];

pub fn ext_of(path: &Path) -> String {
    path.extension().and_then(|e| e.to_str()).map(|e| e.to_ascii_lowercase()).unwrap_or_default()
}
pub fn is_video(path: &Path) -> bool {
    let e = ext_of(path);
    // mp2 is ambiguous; treat as audio
    e != "mp2" && VIDEO_EXTS.contains(&e.as_str())
}
pub fn is_audio(path: &Path) -> bool {
    AUDIO_EXTS.contains(&ext_of(path).as_str())
}
pub fn is_subtitle(path: &Path) -> bool {
    SUBTITLE_EXTS.contains(&ext_of(path).as_str())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideoKind {
    Movie,
    Episode,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ParsedVideo {
    pub is_episode: bool,
    pub title: String,
    pub year: Option<i32>,
    pub show: Option<String>,
    pub season: Option<i32>,
    pub episode: Option<i32>,
    pub episode_end: Option<i32>,
    pub air_date: Option<NaiveDate>,
    pub auto_tags: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ParsedTrack {
    pub artist: Option<String>,
    pub album: Option<String>,
    pub title: String,
    pub track_no: Option<i32>,
    pub disc_no: Option<i32>,
}

static RE_EPISODE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^(?P<show>.*?)[ ._\-\[\(]*\bS(?P<s>\d{1,3})[ ._]?E(?P<e>\d{1,4})(?:(?:[ ._\-]?E|-)(?P<e2>\d{1,4}))?\b[\)\]]?(?:[ ._\-]+(?P<title>.+))?$").unwrap()
});
static RE_X: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^(?P<show>.*?)[ ._\-]*\b(?P<s>\d{1,2})x(?P<e>\d{1,3})(?:-(?:\d{1,2}x)?(?P<e2>\d{1,3}))?\b(?:[ ._\-]+(?P<title>.+))?$").unwrap()
});
static RE_DATE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^(?P<show>.*?)[ ._\-]*\b(?P<y>(?:19|20)\d{2})[ ._\-](?P<m>\d{2})[ ._\-](?P<d>\d{2})\b(?:[ ._\-]+(?P<title>.+))?$").unwrap()
});
static RE_ANIME: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\[(?P<group>[^\]]+)\]\s*(?P<show>.+?)\s+-\s+(?P<e>\d{1,4})(?:v\d)?(?:[\s\(\[].*)?$").unwrap()
});
static RE_SEASON_DIR: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^(?:season|series|s)[ ._]?(?P<s>\d{1,3})$|^specials$").unwrap());
static RE_EP_ONLY: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^(?:e|ep|episode)[ ._]?(?P<e>\d{1,4})\b(?:[ ._\-]+(?P<title>.+))?$|^(?P<e3>\d{1,3})(?:[ ._\-]+(?P<title2>.+))?$").unwrap()
});
static RE_YEAR: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?:^|[ ._\-\(\[])(?P<y>(?:19|20)\d{2})(?:[\)\]]|[ ._\-]|$)").unwrap());
static RE_JUNK: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)(?:^|[ ._\-\[\(])(?:2160p|1080p|1080i|720p|576p|480p|4k|uhd|x264|x265|h\.?264|h\.?265|hevc|avc|av1|vp9|xvid|divx|web[- .]?dl|web[- .]?rip|webrip|web|bluray|blu-ray|bdrip|brrip|bdremux|remux|hdtv|pdtv|dvdrip|dvd|hdrip|cam|telesync|hdcam|proper|repack|rerip|internal|limited|unrated|extended|remastered|theatrical|directors[ .]?cut|imax|multi|dual[ .]?audio|dubbed|subbed|amzn|nf|dsnp|hmax|atvp|pcok|hulu|ddp?[ .]?[257][ .]?[01]|dd[ .]?[257][ .]?[01]|dts(?:-?hd)?(?:[ .]?ma)?|truehd|atmos|aac(?:[ .]?2[ .]?0)?|ac3|eac3|flac|opus|hdr10(?:\+|plus)?|hdr|dv|dolby[ .]?vision|sdr|10bit|8bit|hi10p|hybrid|complete|season[ .]?\d+|s\d{1,2}(?:[ .\-]?s\d{1,2})?)(?:[ ._\-\]\)].*)?$").unwrap()
});
static RE_BRACKETS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\[[^\]]*\]|\{[^}]*\}").unwrap());
static RE_WS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[ ._]+").unwrap());
static RE_TRACK: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(?:(?P<d>\d{1,2})[ .\-_])?(?P<n>\d{1,3})[ .\-_]+(?P<rest>.+)$").unwrap());

/// Dots/underscores → spaces, strip release junk, trim separators.
pub fn clean_title(s: &str) -> String {
    let s = RE_BRACKETS.replace_all(s, " ");
    let s = RE_JUNK.replace(&s, "");
    let s = RE_WS.replace_all(&s, " ");
    s.trim_matches(|c: char| c == ' ' || c == '-' || c == '(' || c == ')' || c == '[' || c == ']').to_string()
}

/// Lowercase sort key without leading articles.
pub fn sort_title(title: &str) -> String {
    let t = title.trim().to_lowercase();
    for art in ["the ", "a ", "an "] {
        if let Some(rest) = t.strip_prefix(art) {
            return rest.trim().to_string();
        }
    }
    t
}

/// Quality/feature tags derived from the release name (shown as chips and
/// usable as filters): 4K, HDR, Dolby Vision, Remux, Extended, IMAX…
pub fn auto_tags(name: &str) -> Vec<String> {
    let n = name.to_lowercase();
    let has = |pats: &[&str]| pats.iter().any(|p| n.contains(p));
    let mut tags = vec![];
    if has(&["2160p", "4k", "uhd"]) {
        tags.push("4K");
    } else if has(&["1080p", "1080i"]) {
        tags.push("1080p");
    } else if has(&["720p"]) {
        tags.push("720p");
    }
    if has(&["dolby.vision", "dolby vision", ".dv.", " dv ", "-dv"]) || n.ends_with(".dv") {
        tags.push("Dolby Vision");
    }
    if has(&["hdr10+", "hdr10plus"]) {
        tags.push("HDR10+");
    } else if has(&["hdr"]) {
        tags.push("HDR");
    }
    if has(&["remux"]) {
        tags.push("Remux");
    } else if has(&["bluray", "blu-ray", "bdrip", "brrip"]) {
        tags.push("Blu-ray");
    } else if has(&["web-dl", "webdl", "webrip", "web.dl", "web.rip", ".web.", " web "]) {
        tags.push("Web");
    } else if has(&["hdtv"]) {
        tags.push("HDTV");
    }
    if has(&["atmos"]) {
        tags.push("Atmos");
    }
    if has(&["truehd"]) {
        tags.push("TrueHD");
    }
    if has(&["dts-hd", "dts.hd", "dtshd"]) {
        tags.push("DTS-HD");
    }
    if has(&["extended"]) {
        tags.push("Extended");
    }
    if has(&["director", "directors.cut", "director's cut"]) {
        tags.push("Director's Cut");
    }
    if has(&["imax"]) {
        tags.push("IMAX");
    }
    if has(&["remastered"]) {
        tags.push("Remastered");
    }
    if has(&["x265", "hevc", "h265", "h.265"]) {
        tags.push("HEVC");
    }
    if has(&["av1"]) {
        tags.push("AV1");
    }
    tags.into_iter().map(String::from).collect()
}

fn extract_year(s: &str) -> (String, Option<i32>) {
    // the *last* plausible year wins when there are several ("2001 (2019)")
    let mut best: Option<(usize, usize, i32)> = None;
    for c in RE_YEAR.captures_iter(s) {
        let m = c.name("y").unwrap();
        let y: i32 = m.as_str().parse().unwrap_or(0);
        if (1900..=2100).contains(&y) {
            best = Some((m.start(), m.end(), y));
        }
    }
    let sep = |c: char| c == ' ' || c == '.' || c == '_' || c == '-' || c == '(' || c == '[' || c == ')' || c == ']';
    match best {
        Some((start, _, y)) if start > 0 => (s[..start].trim_end_matches(sep).to_string(), Some(y)),
        Some((_, end, y)) => (s[end..].trim_start_matches(sep).to_string(), Some(y)),
        None => (s.to_string(), None),
    }
}

fn parent_names(path: &Path) -> Vec<String> {
    path.ancestors().skip(1).filter_map(|p| p.file_name().and_then(|n| n.to_str()).map(str::to_string)).collect()
}

fn num(s: Option<regex::Match<'_>>) -> Option<i32> {
    s.and_then(|m| m.as_str().parse().ok())
}

/// A folder named like a dedicated movie or TV library ("/media/movies",
/// "/media/tv"). Scans farthest-first — the library root sits near the top,
/// and a show legitimately named "Film" must not out-vote it. Returns the
/// kind and the ancestor's index in `parents` (which is nearest-first).
fn kind_hint(parents: &[String]) -> Option<(VideoKind, usize)> {
    for (i, p) in parents.iter().enumerate().rev() {
        let n = p.to_lowercase().replace(['-', '_', '.'], " ");
        match n.split_whitespace().collect::<Vec<_>>().join(" ").as_str() {
            "movies" | "movie" | "films" | "film" => return Some((VideoKind::Movie, i)),
            "tv" | "tv shows" | "tv show" | "tvshows" | "shows" | "series" | "television" => {
                return Some((VideoKind::Episode, i));
            }
            _ => {}
        }
    }
    None
}

/// Classify a video file from its name and folders.
pub fn parse_video(path: &Path) -> ParsedVideo {
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or_default().to_string();
    let all_parents = parent_names(path);
    // A movies/tv library ancestor is authoritative: it decides the kind and
    // scopes folder-derived metadata to the directories below it.
    let (hint, parents) = match kind_hint(&all_parents) {
        Some((k, i)) => (Some(k), all_parents[..i].to_vec()),
        None => (None, all_parents),
    };
    let tags = auto_tags(&format!("{stem} {}", parents.first().cloned().unwrap_or_default()));

    // 1. Explicit SxxEyy / 1x02 / date / anime in the file name — never
    //    inside a movies library.
    let mut ep = if hint == Some(VideoKind::Movie) { None } else { parse_episode_name(&stem) };

    // 2. Folder structure: Show/Season 01/<E02 - Title>.mkv or Show/Season 1/Show - 02.mkv
    if hint != Some(VideoKind::Movie)
        && ep.is_none()
        && let Some(season_dir) = parents.first()
        && let Some(c) = RE_SEASON_DIR.captures(season_dir)
    {
        let season = num(c.name("s")).unwrap_or(0);
        let show = parents.get(1).map(|s| clean_show(s)).unwrap_or_default();
        if let Some(c2) = RE_EP_ONLY.captures(&stem) {
            let e = num(c2.name("e")).or_else(|| num(c2.name("e3")));
            let title =
                c2.name("title").or_else(|| c2.name("title2")).map(|m| clean_title(m.as_str())).unwrap_or_default();
            if let Some(e) = e {
                ep = Some(ParsedVideo {
                    is_episode: true,
                    title,
                    show: Some(show),
                    season: Some(season),
                    episode: Some(e),
                    ..Default::default()
                });
            }
        }
    }

    // A tv-library file with no recognizable episode pattern still becomes an
    // episode: title from the file name, show filled from the folders below
    // the tv root by the block underneath (or "Unknown Show").
    if ep.is_none() && hint == Some(VideoKind::Episode) {
        let (name, year) = extract_year(&stem);
        let title = clean_title(&name);
        ep = Some(ParsedVideo {
            is_episode: true,
            title: if title.is_empty() { stem.clone() } else { title },
            year,
            ..Default::default()
        });
    }

    if let Some(mut e) = ep {
        // fill the show from the folders if the file name didn't carry it
        let show_from_file = e.show.clone().unwrap_or_default();
        if show_from_file.is_empty() || show_from_file.len() < 2 {
            let mut candidates = parents.iter();
            let first = candidates.next().cloned().unwrap_or_default();
            let from_dir = if RE_SEASON_DIR.is_match(&first) { candidates.next().cloned() } else { Some(first) };
            if let Some(d) = from_dir.filter(|d| !d.is_empty()) {
                let (name, year) = extract_year(&d);
                e.show = Some(clean_show(&name));
                if e.year.is_none() {
                    e.year = year;
                }
            }
        }
        e.auto_tags = tags;
        if e.show.as_deref().map(|s| s.is_empty()).unwrap_or(true) {
            e.show = Some("Unknown Show".into());
        }
        return e;
    }

    // 3. Movie: "Title (2019)" from the file, else from a "Title (2019)" folder
    //    when the file name is generic (movie.mkv, title.mkv, same as folder).
    let (name, year) = extract_year(&stem);
    let mut title = clean_title(&name);
    let mut year = year;
    let generic = title.is_empty()
        || ["movie", "film", "video", "title", "feature", "main"].contains(&title.to_lowercase().as_str())
        || title.chars().all(|c| c.is_ascii_digit());
    if let Some(dir) = parents.first() {
        let (dname, dyear) = extract_year(dir);
        let dtitle = clean_title(&dname);
        if !dtitle.is_empty()
            && (generic || (dyear.is_some() && year.is_none() && !RE_SEASON_DIR.is_match(dir)))
            && (generic || dtitle.to_lowercase() == title.to_lowercase() || year.is_none())
        {
            title = dtitle;
            year = year.or(dyear);
        }
    }
    if title.is_empty() {
        title = stem.clone();
    }
    ParsedVideo { is_episode: false, title, year, auto_tags: tags, ..Default::default() }
}

fn clean_show(s: &str) -> String {
    let (name, _) = extract_year(s);
    let c = clean_title(&name);
    if c.is_empty() { clean_title(s) } else { c }
}

/// Episode patterns on a bare file stem (no folders). `None` = not an episode.
pub fn parse_episode_name(stem: &str) -> Option<ParsedVideo> {
    if let Some(c) = RE_EPISODE.captures(stem) {
        let (show_raw, year) = extract_year(c.name("show").map(|m| m.as_str()).unwrap_or(""));
        return Some(ParsedVideo {
            is_episode: true,
            show: Some(clean_show(&show_raw)),
            year,
            season: num(c.name("s")),
            episode: num(c.name("e")),
            episode_end: num(c.name("e2")),
            title: c.name("title").map(|m| clean_title(m.as_str())).unwrap_or_default(),
            ..Default::default()
        });
    }
    if let Some(c) = RE_X.captures(stem) {
        let (show_raw, year) = extract_year(c.name("show").map(|m| m.as_str()).unwrap_or(""));
        return Some(ParsedVideo {
            is_episode: true,
            show: Some(clean_show(&show_raw)),
            year,
            season: num(c.name("s")),
            episode: num(c.name("e")),
            episode_end: num(c.name("e2")),
            title: c.name("title").map(|m| clean_title(m.as_str())).unwrap_or_default(),
            ..Default::default()
        });
    }
    if let Some(c) = RE_DATE.captures(stem) {
        let y = num(c.name("y"))?;
        let m = num(c.name("m"))? as u32;
        let d = num(c.name("d"))? as u32;
        let date = NaiveDate::from_ymd_opt(y, m, d)?;
        return Some(ParsedVideo {
            is_episode: true,
            show: Some(clean_show(c.name("show").map(|m| m.as_str()).unwrap_or(""))),
            season: Some(y),
            episode: Some((m * 100 + d) as i32),
            air_date: Some(date),
            title: c.name("title").map(|m| clean_title(m.as_str())).unwrap_or_default(),
            ..Default::default()
        });
    }
    if let Some(c) = RE_ANIME.captures(stem) {
        return Some(ParsedVideo {
            is_episode: true,
            show: Some(clean_show(c.name("show").map(|m| m.as_str()).unwrap_or(""))),
            season: Some(1),
            episode: num(c.name("e")),
            ..Default::default()
        });
    }
    None
}

/// Fallback for untagged music: `Artist/Album/01 - Title.flac`,
/// `Artist/Album/1-02 Title.mp3`, `Artist - Title.mp3`.
pub fn parse_track_path(path: &Path) -> ParsedTrack {
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or_default().to_string();
    let parents = parent_names(path);
    let mut t = ParsedTrack::default();
    let rest = if let Some(c) = RE_TRACK.captures(&stem) {
        t.disc_no = num(c.name("d"));
        t.track_no = num(c.name("n"));
        c.name("rest").map(|m| m.as_str().to_string()).unwrap_or_default()
    } else {
        stem.clone()
    };
    // "Artist - Title"
    if let Some((a, b)) = rest.split_once(" - ") {
        if t.track_no.is_none() || parents.len() < 2 {
            t.artist = Some(a.trim().to_string());
            t.title = b.trim().to_string();
        } else {
            t.title = rest.trim().to_string();
        }
    } else {
        t.title = rest.trim().to_string();
    }
    if let Some(album) = parents.first() {
        let (name, _) = extract_year(album);
        let a = RE_WS.replace_all(name.trim(), " ").trim().to_string();
        t.album = Some(if a.is_empty() { album.clone() } else { a });
    }
    if t.artist.is_none()
        && let Some(artist) = parents.get(1)
    {
        t.artist = Some(artist.clone());
    }
    if t.title.is_empty() {
        t.title = stem;
    }
    t
}

/// Filesystem-safe name for recordings.
pub fn safe_filename(s: &str) -> String {
    let out: String = s.chars().map(|c| if c.is_alphanumeric() || " .()-_,'&".contains(c) { c } else { '_' }).collect();
    let out = out.trim().trim_end_matches('.').to_string();
    if out.is_empty() { "untitled".into() } else { out }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn pv(p: &str) -> ParsedVideo {
        parse_video(&PathBuf::from(p))
    }

    #[test]
    fn library_kind_hint() {
        // movies/ ancestor forces movie — even on an SxxEyy-looking name
        let m = pv("/media/movies/Heat (1995)/Heat (1995) 1080p.mkv");
        assert!(!m.is_episode);
        assert_eq!(m.title, "Heat");
        assert_eq!(m.year, Some(1995));
        assert!(!pv("/media/movies/Doc.S01E01.The.Beginning.mkv").is_episode);
        // tv/ ancestor forces episode even with no episode pattern
        let e = pv("/media/tv/Firefly/Serenity Special.mkv");
        assert!(e.is_episode);
        assert_eq!(e.show.as_deref(), Some("Firefly"));
        assert_eq!(e.title, "Serenity Special");
        assert_eq!(e.season, None);
        // directly in the tv root: no folder to name the show
        assert_eq!(pv("/media/tv/Pilot.mkv").show.as_deref(), Some("Unknown Show"));
        // normal parsing below the root is untouched
        let e = pv("/media/tv/Static Signal (2019)/Season 02/03 - Cold Boot.mkv");
        assert!(e.is_episode);
        assert_eq!((e.season, e.episode), (Some(2), Some(3)));
        assert_eq!(e.show.as_deref(), Some("Static Signal"));
        // "TV Shows" spelling variants; hint dir itself never becomes the show
        assert!(pv("/srv/TV Shows/Doc/ep.mkv").is_episode);
        // a show named "Film" inside a tv library stays an episode (farthest wins)
        let e = pv("/media/tv/Film/S01E02.mkv");
        assert!(e.is_episode);
        // movie directly in the movies root: folder name must not leak into the title
        assert_eq!(pv("/media/movies/movie.mkv").title, "movie");
        // no hint dir anywhere: heuristics unchanged
        assert!(!pv("/data/videos/Heat (1995).mkv").is_episode);
    }

    #[test]
    fn go_suite_parity() {
        // the cases from the original Go test
        let e = pv("/tv/Static Signal S01E03 Cold Boot.mkv");
        assert!(e.is_episode);
        assert_eq!(e.show.as_deref(), Some("Static Signal"));
        assert_eq!((e.season, e.episode), (Some(1), Some(3)));
        assert_eq!(e.title, "Cold Boot");

        let e = pv("/tv/the.expanse.s02e11.1080p.web-dl.mkv");
        assert_eq!(e.show.as_deref(), Some("the expanse"));
        assert_eq!((e.season, e.episode), (Some(2), Some(11)));
        assert_eq!(e.title, "");
        assert!(e.auto_tags.contains(&"1080p".to_string()));

        let e = pv("/tv/Severance - S01E09 - The We We Are.mkv");
        assert_eq!(e.show.as_deref(), Some("Severance"));
        assert_eq!(e.title, "The We We Are");

        let e = pv("/tv/Show (2019) S03E01.mkv");
        assert_eq!(e.show.as_deref(), Some("Show"));
        assert_eq!(e.year, Some(2019));
        assert_eq!((e.season, e.episode), (Some(3), Some(1)));

        let m = pv("/movies/Blade Circuit (2023).mkv");
        assert!(!m.is_episode);
        assert_eq!(m.title, "Blade Circuit");
        assert_eq!(m.year, Some(2023));

        let m = pv("/movies/Heat.1995.2160p.Remux.mkv");
        assert_eq!(m.title, "Heat");
        assert_eq!(m.year, Some(1995));
        assert!(m.auto_tags.contains(&"4K".to_string()));
        assert!(m.auto_tags.contains(&"Remux".to_string()));

        let m = pv("/movies/Some Movie.mkv");
        assert_eq!(m.title, "Some Movie");
        assert_eq!(m.year, None);
    }

    #[test]
    fn multi_episode_and_1x02() {
        let e = pv("/tv/Show S01E01E02 Pilot.mkv");
        assert_eq!((e.episode, e.episode_end), (Some(1), Some(2)));
        let e = pv("/tv/Show S01E01-E02.mkv");
        assert_eq!((e.episode, e.episode_end), (Some(1), Some(2)));
        let e = pv("/tv/Firefly 1x03 Bushwhacked.mkv");
        assert_eq!(e.show.as_deref(), Some("Firefly"));
        assert_eq!((e.season, e.episode), (Some(1), Some(3)));
        assert_eq!(e.title, "Bushwhacked");
    }

    #[test]
    fn folder_structure_supplies_show() {
        let e = pv("/tv/Breaking Bad (2008)/Season 02/S02E05 - Breakage.mkv");
        assert_eq!(e.show.as_deref(), Some("Breaking Bad"));
        assert_eq!(e.year, Some(2008));
        assert_eq!((e.season, e.episode), (Some(2), Some(5)));
        assert_eq!(e.title, "Breakage");

        let e = pv("/tv/Cowboy Bebop/Season 1/Episode 05.mkv");
        assert_eq!(e.show.as_deref(), Some("Cowboy Bebop"));
        assert_eq!((e.season, e.episode), (Some(1), Some(5)));

        let e = pv("/tv/Cowboy Bebop/Season 1/05 - Ballad of Fallen Angels.mkv");
        assert_eq!(e.show.as_deref(), Some("Cowboy Bebop"));
        assert_eq!(e.episode, Some(5));
        assert_eq!(e.title, "Ballad of Fallen Angels");
    }

    #[test]
    fn date_based_and_anime() {
        let e = pv("/tv/The Daily Show/The Daily Show 2024-01-15 Guest.mkv");
        assert!(e.is_episode);
        assert_eq!(e.show.as_deref(), Some("The Daily Show"));
        assert_eq!(e.air_date, NaiveDate::from_ymd_opt(2024, 1, 15));
        assert_eq!(e.title, "Guest");

        let e = pv("/anime/[SubsPlease] Frieren - 07 (1080p) [ABCD1234].mkv");
        assert!(e.is_episode);
        assert_eq!(e.show.as_deref(), Some("Frieren"));
        assert_eq!((e.season, e.episode), (Some(1), Some(7)));
    }

    #[test]
    fn movie_folders_and_junk() {
        let m = pv("/movies/Dune Part Two (2024)/movie.mkv");
        assert_eq!(m.title, "Dune Part Two");
        assert_eq!(m.year, Some(2024));

        let m = pv("/movies/Dune.Part.Two.2024.2160p.WEB-DL.DDP5.1.Atmos.DV.HDR.H.265-FLUX.mkv");
        assert_eq!(m.title, "Dune Part Two");
        assert_eq!(m.year, Some(2024));
        for t in ["4K", "Dolby Vision", "HDR", "Atmos", "HEVC", "Web"] {
            assert!(m.auto_tags.contains(&t.to_string()), "missing tag {t} in {:?}", m.auto_tags);
        }

        let m = pv("/movies/2001 A Space Odyssey (1968).mkv");
        assert_eq!(m.title, "2001 A Space Odyssey");
        assert_eq!(m.year, Some(1968));

        let m = pv("/movies/Heat (1995)/Heat.1995.1080p.BluRay.x264-GRP.mkv");
        assert_eq!(m.title, "Heat");
        assert_eq!(m.year, Some(1995));
    }

    #[test]
    fn track_paths() {
        let t = parse_track_path(&PathBuf::from("/music/Daft Punk/Discovery (2001)/03 - Digital Love.flac"));
        assert_eq!(t.artist.as_deref(), Some("Daft Punk"));
        assert_eq!(t.album.as_deref(), Some("Discovery"));
        assert_eq!(t.track_no, Some(3));
        assert_eq!(t.title, "Digital Love");

        let t = parse_track_path(&PathBuf::from("/music/Various/Mix/1-02 Intro.mp3"));
        assert_eq!((t.disc_no, t.track_no), (Some(1), Some(2)));
        assert_eq!(t.title, "Intro");

        let t = parse_track_path(&PathBuf::from("/music/Loose/Radiohead - Creep.mp3"));
        assert_eq!(t.artist.as_deref(), Some("Radiohead"));
        assert_eq!(t.title, "Creep");
    }

    #[test]
    fn helpers() {
        assert_eq!(sort_title("The Matrix"), "matrix");
        assert_eq!(sort_title("A Quiet Place"), "quiet place");
        assert_eq!(safe_filename("Late Show: Part 1/2?"), "Late Show_ Part 1_2_");
        assert!(is_video(&PathBuf::from("a.MKV")));
        assert!(is_audio(&PathBuf::from("a.flac")));
        assert!(!is_video(&PathBuf::from("a.txt")));
        assert_eq!(clean_title("Some.Title.2019.1080p.BluRay"), "Some Title 2019");
    }
}
