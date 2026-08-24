// Copyright 2026 The Ontele Authors
// SPDX-License-Identifier: Apache-2.0

//! Kodi-style NFO sidecars (`<movie>`, `<tvshow>`, `<episodedetails>`).
//!
//! Parsing is a tolerant streaming pass over quick-xml events: anything before
//! the root element (scraper URLs, BOMs, stray text) is skipped, unknown
//! elements are ignored, and a malformed tail still yields whatever was read.

use crate::model::{CastMember, Metadata};
use quick_xml::Reader;
use quick_xml::events::{BytesStart, Event};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Default, PartialEq)]
pub struct NfoInfo {
    pub title: Option<String>,
    pub year: Option<i32>,
    pub season: Option<i32>,
    pub episode: Option<i32>,
    pub meta: Metadata,
}

/// The three Kodi roots we understand.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NfoRoot {
    Movie,
    TvShow,
    Episode,
}

impl NfoRoot {
    fn tag(self) -> &'static str {
        match self {
            NfoRoot::Movie => "movie",
            NfoRoot::TvShow => "tvshow",
            NfoRoot::Episode => "episodedetails",
        }
    }
}

/// Locate the NFO for a media file: `<stem>.nfo`, `movie.nfo`, or (for
/// episodes) `tvshow.nfo` two levels up.
pub fn find_nfo(media_path: &Path) -> Option<PathBuf> {
    let dir = media_path.parent()?;
    if let Some(stem) = media_path.file_stem() {
        let p = dir.join(format!("{}.nfo", stem.to_string_lossy()));
        if p.is_file() {
            return Some(p);
        }
    }
    let p = dir.join("movie.nfo");
    if p.is_file() {
        return Some(p);
    }
    None
}

/// `tvshow.nfo` in the file's directory, its parent or its grandparent
/// (`Show/Season 01/ep.mkv` → `Show/tvshow.nfo`).
pub fn find_tvshow_nfo(media_path: &Path) -> Option<PathBuf> {
    let mut dir = media_path.parent();
    for _ in 0..3 {
        let d = dir?;
        let p = d.join("tvshow.nfo");
        if p.is_file() {
            return Some(p);
        }
        dir = d.parent();
    }
    None
}

/// Read + parse an NFO file. `None` when the file is unreadable or not one
/// of the known roots.
pub fn read(path: &Path) -> Option<NfoInfo> {
    let bytes = std::fs::read(path).ok()?;
    let text = String::from_utf8_lossy(&bytes);
    parse(&text)
}

/// Parse any of the three NFO roots. Returns `None` if the XML is not one of
/// them (some NFOs are just a URL line).
pub fn parse(xml: &str) -> Option<NfoInfo> {
    let (root, start) = locate_root(xml)?;
    let mut p = Parser::new(root);
    p.run(&xml[start..]);
    Some(p.finish())
}

/// Which root the document carries, and the byte offset of its `<`.
pub fn detect_root(xml: &str) -> Option<NfoRoot> {
    locate_root(xml).map(|(r, _)| r)
}

fn locate_root(xml: &str) -> Option<(NfoRoot, usize)> {
    let mut best: Option<(NfoRoot, usize)> = None;
    for root in [NfoRoot::Movie, NfoRoot::TvShow, NfoRoot::Episode] {
        let tag = root.tag();
        let mut from = 0usize;
        while let Some(rel) = xml[from..].find('<') {
            let at = from + rel;
            let rest = &xml[at + 1..];
            if let Some(after) = rest.strip_prefix(tag)
                && matches!(
                    after.chars().next(),
                    Some('>') | Some(' ') | Some('\t') | Some('\n') | Some('\r') | Some('/')
                )
            {
                if best.map(|(_, b)| at < b).unwrap_or(true) {
                    best = Some((root, at));
                }
                break;
            }
            from = at + 1;
        }
    }
    best
}

#[derive(Default)]
struct Actor {
    name: String,
    role: String,
    thumb: String,
}

struct Parser {
    root: NfoRoot,
    info: NfoInfo,
    /// Element name stack relative to (and including) the root.
    stack: Vec<String>,
    text: String,
    actor: Option<Actor>,
    /// `<uniqueid type="...">` / `<thumb aspect="...">` / `<rating name=.. default=..>` attributes
    /// for the element currently being read.
    attr_type: Option<String>,
    attr_aspect: Option<String>,
    attr_default: bool,
    attr_rating_value: Option<f64>,
    /// Inside `<ratings>`: the rating block we are going to use.
    rating_block_default: bool,
    rating_block_votes: Option<u64>,
    rating_block_value: Option<f64>,
    picked_rating: Option<(bool, f64, Option<u64>)>,
    poster_candidate: Option<(bool, String)>,
    seen_first_thumb: bool,
    seen_first_fanart: bool,
    date_year: Option<i32>,
}

impl Parser {
    fn new(root: NfoRoot) -> Self {
        Self {
            root,
            info: NfoInfo::default(),
            stack: Vec::new(),
            text: String::new(),
            actor: None,
            attr_type: None,
            attr_aspect: None,
            attr_default: false,
            attr_rating_value: None,
            rating_block_default: false,
            rating_block_votes: None,
            rating_block_value: None,
            picked_rating: None,
            poster_candidate: None,
            seen_first_thumb: false,
            seen_first_fanart: false,
            date_year: None,
        }
    }

    fn run(&mut self, xml: &str) {
        let mut reader = Reader::from_str(xml);
        {
            let cfg = reader.config_mut();
            cfg.check_end_names = false;
            cfg.allow_unmatched_ends = true;
            cfg.allow_dangling_amp = true;
        }
        let mut root_depth_done = false;
        loop {
            let ev = match reader.read_event() {
                Ok(ev) => ev,
                Err(e) => {
                    tracing::debug!(error = %e, "nfo: stopping at malformed xml");
                    break;
                }
            };
            match ev {
                Event::Start(e) => {
                    self.on_start(&e);
                }
                Event::Empty(e) => {
                    self.on_start(&e);
                    self.on_end();
                }
                Event::End(_) => {
                    if self.stack.len() == 1 {
                        root_depth_done = true;
                    }
                    self.on_end();
                }
                Event::Text(t) => self.text.push_str(&t),
                Event::CData(c) => self.text.push_str(&c),
                Event::GeneralRef(r) => {
                    if let Ok(Some(c)) = r.resolve_char_ref() {
                        self.text.push(c);
                    } else if let Some(s) = quick_xml::escape::resolve_predefined_entity(&r) {
                        self.text.push_str(s);
                    }
                }
                Event::Eof => break,
                _ => {}
            }
            if root_depth_done {
                break;
            }
        }
    }

    fn on_start(&mut self, e: &BytesStart<'_>) {
        let name = e.local_name().as_ref().to_string();
        self.text.clear();
        self.attr_type = None;
        self.attr_aspect = None;
        self.attr_default = false;
        self.attr_rating_value = None;
        for a in e.attributes().flatten() {
            let key = a.key.local_name().as_ref().to_string();
            let val = a.normalized_value(quick_xml::XmlVersion::Implicit1_0).map(|v| v.to_string()).unwrap_or_default();
            match key.as_str() {
                "type" => self.attr_type = Some(val.to_lowercase()),
                "aspect" => self.attr_aspect = Some(val.to_lowercase()),
                "default" => self.attr_default = matches!(val.trim().to_lowercase().as_str(), "true" | "1" | "yes"),
                "value" => self.attr_rating_value = parse_f64(&val),
                _ => {}
            }
        }
        if name == "actor" && self.stack.len() == 1 {
            self.actor = Some(Actor::default());
        }
        if name == "rating" && self.stack.last().map(|s| s == "ratings").unwrap_or(false) {
            self.rating_block_default = self.attr_default;
            self.rating_block_votes = None;
            self.rating_block_value = self.attr_rating_value;
        }
        self.stack.push(name);
    }

    fn on_end(&mut self) {
        let Some(name) = self.stack.pop() else { return };
        let text = std::mem::take(&mut self.text).trim().to_string();
        let depth = self.stack.len(); // depth of parent; 1 = direct child of root
        let parent = self.stack.last().map(String::as_str).unwrap_or("");
        let grand = if self.stack.len() >= 2 { self.stack[self.stack.len() - 2].as_str() } else { "" };

        // ---- actor block --------------------------------------------------
        if let Some(actor) = self.actor.as_mut() {
            if name == "actor" && depth == 1 {
                let a = self.actor.take().unwrap_or_default();
                if !a.name.is_empty() {
                    self.info.meta.cast.push(CastMember {
                        name: a.name,
                        character: non_empty(a.role),
                        profile: non_empty(a.thumb),
                    });
                }
                return;
            }
            if parent == "actor" {
                match name.as_str() {
                    "name" => actor.name = text,
                    "role" => actor.role = text,
                    "thumb" => actor.thumb = text,
                    _ => {}
                }
            }
            return;
        }

        // ---- <ratings><rating default="true"><value>/<votes> --------------
        if grand == "ratings" && parent == "rating" {
            match name.as_str() {
                "value" => self.rating_block_value = parse_f64(&text),
                "votes" => self.rating_block_votes = parse_u64(&text),
                _ => {}
            }
            return;
        }
        if parent == "ratings" && name == "rating" {
            if let Some(v) = self.rating_block_value {
                let is_default = self.rating_block_default;
                let take = match self.picked_rating {
                    None => true,
                    Some((prev_default, _, _)) => is_default && !prev_default,
                };
                if take {
                    self.picked_rating = Some((is_default, v, self.rating_block_votes));
                }
            }
            return;
        }

        // ---- <fanart><thumb> ---------------------------------------------
        if parent == "fanart" && name == "thumb" {
            if !self.seen_first_fanart && !text.is_empty() {
                self.seen_first_fanart = true;
                self.info.meta.backdrop_url = Some(text);
            }
            return;
        }

        if depth != 1 {
            return;
        }

        // ---- direct children of the root ----------------------------------
        let m = &mut self.info.meta;
        match name.as_str() {
            "title" => {
                if !text.is_empty() {
                    self.info.title = Some(text);
                }
            }
            "originaltitle" => m.original_title = non_empty(text),
            "sorttitle" => {}
            "year" => {
                if let Some(y) = parse_i32(&text) {
                    self.info.year = Some(y);
                }
            }
            "premiered" | "aired" | "releasedate" => {
                if !text.is_empty() {
                    if m.release_date.is_none() || name == "premiered" {
                        m.release_date = Some(text.clone());
                    }
                    if self.date_year.is_none() {
                        self.date_year = text.get(0..4).and_then(|y| y.parse::<i32>().ok());
                    }
                }
            }
            "plot" => {
                if !text.is_empty() {
                    m.overview = Some(text);
                }
            }
            "outline" => {
                if m.overview.is_none() && !text.is_empty() {
                    m.overview = Some(text);
                }
            }
            "tagline" => m.tagline = non_empty(text),
            "runtime" => {
                // "120", "120 min", "1h 55m"
                if let Some(min) = parse_runtime(&text) {
                    m.runtime_min = Some(min);
                }
            }
            "genre" => {
                for g in text.split(['/', ',', ';']) {
                    let g = g.trim();
                    if !g.is_empty() && !m.genres.iter().any(|x| x.eq_ignore_ascii_case(g)) {
                        m.genres.push(g.to_string());
                    }
                }
            }
            "mpaa" => m.content_rating = non_empty(normalize_mpaa(&text)),
            "studio" => {
                if m.studio.is_none() {
                    m.studio = non_empty(text);
                }
            }
            "rating" => {
                // Legacy flat <rating>7.8</rating> (or <rating value="7.8"/>)
                if let Some(v) = self.attr_rating_value.or_else(|| parse_f64(&text))
                    && self.picked_rating.is_none()
                {
                    self.picked_rating = Some((false, v, None));
                }
            }
            "votes" => {
                if let Some(v) = parse_u64(&text) {
                    m.votes = Some(v);
                }
            }
            "uniqueid" => {
                let t = self.attr_type.clone().unwrap_or_default();
                self.set_id(&t, &text);
            }
            "tmdbid" => self.set_id("tmdb", &text),
            "imdbid" => self.set_id("imdb", &text),
            "tvdbid" => self.set_id("tvdb", &text),
            "id" => {
                // Kodi's legacy <id> is IMDB for movies, TVDB for shows; sniff the shape.
                let t = text.trim();
                if t.starts_with("tt") {
                    self.set_id("imdb", t);
                } else if self.root == NfoRoot::TvShow {
                    self.set_id("tvdb", t);
                } else if m.imdb_id.is_none() && m.tmdb_id.is_none() {
                    self.set_id("tmdb", t);
                }
            }
            "thumb" => {
                if !text.is_empty() {
                    let aspect = self.attr_aspect.clone().unwrap_or_default();
                    let is_poster = aspect == "poster";
                    let is_other = !aspect.is_empty() && !is_poster; // banner/landscape/clearlogo...
                    if is_other {
                        if aspect == "clearlogo" || aspect == "logo" {
                            m.logo_url.get_or_insert(text);
                        }
                    } else {
                        let replace = match &self.poster_candidate {
                            None => true,
                            Some((prev_poster, _)) => is_poster && !prev_poster,
                        };
                        if replace {
                            self.poster_candidate = Some((is_poster, text));
                        }
                    }
                    self.seen_first_thumb = true;
                }
            }
            "season" if self.root == NfoRoot::Episode => self.info.season = parse_i32(&text),
            "episode" if self.root == NfoRoot::Episode => self.info.episode = parse_i32(&text),
            _ => {}
        }
    }

    fn set_id(&mut self, kind: &str, value: &str) {
        let v = value.trim();
        if v.is_empty() {
            return;
        }
        let m = &mut self.info.meta;
        match kind {
            "tmdb" => {
                if let Ok(n) = v.parse::<i64>() {
                    m.tmdb_id = Some(n);
                }
            }
            "imdb" => {
                if v.starts_with("tt") && m.imdb_id.is_none() {
                    m.imdb_id = Some(v.to_string());
                }
            }
            "tvdb" => {
                if let Ok(n) = v.parse::<i64>() {
                    m.tvdb_id = Some(n);
                }
            }
            _ => {}
        }
    }

    fn finish(mut self) -> NfoInfo {
        if let Some((_, v, votes)) = self.picked_rating {
            self.info.meta.rating = Some(v);
            if self.info.meta.votes.is_none() {
                self.info.meta.votes = votes;
            }
        }
        if let Some((_, url)) = self.poster_candidate.take() {
            self.info.meta.poster_url = Some(url);
        }
        if self.info.year.is_none() {
            self.info.year = self.date_year;
        }
        if self.info.meta.release_date.is_none()
            && let Some(y) = self.info.year
        {
            // keep a year-only date so the UI has something to show
            self.info.meta.release_date = Some(format!("{y:04}"));
        }
        self.info.meta.provider = Some("nfo".into());
        self.info.meta.provider_id = match self.root {
            NfoRoot::Movie | NfoRoot::TvShow | NfoRoot::Episode => self
                .info
                .meta
                .tmdb_id
                .map(|i| i.to_string())
                .or_else(|| self.info.meta.imdb_id.clone())
                .or_else(|| self.info.meta.tvdb_id.map(|i| i.to_string())),
        };
        self.info
    }
}

fn non_empty(s: String) -> Option<String> {
    let t = s.trim();
    if t.is_empty() { None } else { Some(t.to_string()) }
}

fn parse_i32(s: &str) -> Option<i32> {
    // leading sign + digits only: "2015-12-14" → 2015, "-1" → -1
    let t = s.trim();
    let (sign, rest) = match t.strip_prefix('-') {
        Some(r) => ("-", r),
        None => ("", t),
    };
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        return None;
    }
    format!("{sign}{digits}").parse().ok()
}

fn parse_u64(s: &str) -> Option<u64> {
    let digits: String = s.trim().chars().filter(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() { None } else { digits.parse().ok() }
}

fn parse_f64(s: &str) -> Option<f64> {
    let t = s.trim().replace(',', ".");
    let v: f64 = t.parse().ok()?;
    if v.is_finite() && (0.0..=10.0).contains(&v) { Some(v) } else { None }
}

/// "120", "120 min", "1h 55m", "1:55".
fn parse_runtime(s: &str) -> Option<u32> {
    let t = s.trim().to_lowercase();
    if t.is_empty() {
        return None;
    }
    if let Some((h, m)) = t.split_once(':') {
        let h: u32 = h.trim().parse().ok()?;
        let m: u32 = m.trim().chars().take_while(|c| c.is_ascii_digit()).collect::<String>().parse().ok()?;
        return Some(h * 60 + m);
    }
    if t.contains('h') {
        let (h, rest) = t.split_once('h')?;
        let h: u32 = h.trim().parse().ok()?;
        let m: u32 = rest.trim().chars().take_while(|c| c.is_ascii_digit()).collect::<String>().parse().unwrap_or(0);
        return Some(h * 60 + m);
    }
    let digits: String = t.chars().take_while(|c| c.is_ascii_digit()).collect();
    let n: u32 = digits.parse().ok()?;
    if n == 0 { None } else { Some(n) }
}

/// "Rated PG-13" → "PG-13"; "TV-14" stays.
fn normalize_mpaa(s: &str) -> String {
    let t = s.trim();
    let t = t.strip_prefix("Rated ").or_else(|| t.strip_prefix("rated ")).unwrap_or(t);
    t.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    const MOVIE: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes" ?>
<movie>
  <title>Blade Runner</title>
  <originaltitle>Blade Runner</originaltitle>
  <sorttitle>Blade Runner</sorttitle>
  <ratings>
    <rating name="imdb" max="10">
      <value>8.1</value>
      <votes>700000</votes>
    </rating>
    <rating name="themoviedb" max="10" default="true">
      <value>7.9</value>
      <votes>12000</votes>
    </rating>
  </ratings>
  <outline>Short outline.</outline>
  <plot>A blade runner must pursue &amp; terminate four replicants.</plot>
  <tagline>Man has made his match... now it's his problem.</tagline>
  <runtime>117</runtime>
  <thumb aspect="banner">https://x/banner.jpg</thumb>
  <thumb aspect="poster" preview="https://x/p.jpg">https://x/poster.jpg</thumb>
  <fanart>
    <thumb preview="https://x/f.jpg">https://x/fanart.jpg</thumb>
    <thumb>https://x/fanart2.jpg</thumb>
  </fanart>
  <mpaa>Rated R</mpaa>
  <uniqueid type="imdb">tt0083658</uniqueid>
  <uniqueid type="tmdb" default="true">78</uniqueid>
  <genre>Science Fiction</genre>
  <genre>Drama</genre>
  <premiered>1982-06-25</premiered>
  <studio>Warner Bros.</studio>
  <actor>
    <name>Harrison Ford</name>
    <role>Rick Deckard</role>
    <order>0</order>
    <thumb>https://x/ford.jpg</thumb>
  </actor>
  <actor>
    <name>Rutger Hauer</name>
    <role>Roy Batty</role>
  </actor>
</movie>"#;

    #[test]
    fn movie_root() {
        let n = parse(MOVIE).expect("parsed");
        assert_eq!(n.title.as_deref(), Some("Blade Runner"));
        assert_eq!(n.year, Some(1982));
        let m = &n.meta;
        assert_eq!(m.provider.as_deref(), Some("nfo"));
        assert_eq!(m.tmdb_id, Some(78));
        assert_eq!(m.imdb_id.as_deref(), Some("tt0083658"));
        assert_eq!(m.rating, Some(7.9));
        assert_eq!(m.votes, Some(12000));
        assert_eq!(m.overview.as_deref(), Some("A blade runner must pursue & terminate four replicants."));
        assert_eq!(m.tagline.as_deref(), Some("Man has made his match... now it's his problem."));
        assert_eq!(m.runtime_min, Some(117));
        assert_eq!(m.genres, vec!["Science Fiction", "Drama"]);
        assert_eq!(m.content_rating.as_deref(), Some("R"));
        assert_eq!(m.studio.as_deref(), Some("Warner Bros."));
        assert_eq!(m.release_date.as_deref(), Some("1982-06-25"));
        assert_eq!(m.poster_url.as_deref(), Some("https://x/poster.jpg"));
        assert_eq!(m.backdrop_url.as_deref(), Some("https://x/fanart.jpg"));
        assert_eq!(m.logo_url, None);
        assert_eq!(m.cast.len(), 2);
        assert_eq!(m.cast[0].name, "Harrison Ford");
        assert_eq!(m.cast[0].character.as_deref(), Some("Rick Deckard"));
        assert_eq!(m.cast[0].profile.as_deref(), Some("https://x/ford.jpg"));
        assert_eq!(m.cast[1].profile, None);
        assert_eq!(n.season, None);
    }

    #[test]
    fn tvshow_root_with_legacy_fields() {
        let xml = r#"<tvshow>
  <title>The Expanse</title>
  <year>2015</year>
  <rating>8.5</rating>
  <votes>1,234</votes>
  <plot><![CDATA[Hundreds of years in the future <b>humans</b> have colonized the solar system.]]></plot>
  <mpaa>TV-14</mpaa>
  <genre>Drama / Sci-Fi</genre>
  <id>280619</id>
  <tmdbid>63639</tmdbid>
  <thumb>https://x/show-poster.jpg</thumb>
  <fanart><thumb>https://x/show-fanart.jpg</thumb></fanart>
  <studio>Syfy</studio>
  <studio>Amazon</studio>
</tvshow>"#;
        let n = parse(xml).unwrap();
        assert_eq!(n.title.as_deref(), Some("The Expanse"));
        assert_eq!(n.year, Some(2015));
        let m = &n.meta;
        assert_eq!(m.rating, Some(8.5));
        assert_eq!(m.votes, Some(1234));
        assert_eq!(m.tvdb_id, Some(280619));
        assert_eq!(m.tmdb_id, Some(63639));
        assert_eq!(m.genres, vec!["Drama", "Sci-Fi"]);
        assert_eq!(m.content_rating.as_deref(), Some("TV-14"));
        assert_eq!(m.studio.as_deref(), Some("Syfy"));
        assert_eq!(m.poster_url.as_deref(), Some("https://x/show-poster.jpg"));
        assert_eq!(m.backdrop_url.as_deref(), Some("https://x/show-fanart.jpg"));
        assert!(m.overview.as_deref().unwrap().starts_with("Hundreds of years"));
        assert_eq!(m.release_date.as_deref(), Some("2015"));
    }

    #[test]
    fn episode_root() {
        let xml = r#"<episodedetails>
  <title>Dulcinea</title>
  <season>1</season>
  <episode>1</episode>
  <aired>2015-12-14</aired>
  <plot>Miller is assigned to find Julie Mao.</plot>
  <runtime>44 min</runtime>
  <thumb>https://x/still.jpg</thumb>
  <uniqueid type="tvdb">5123</uniqueid>
</episodedetails>"#;
        let n = parse(xml).unwrap();
        assert_eq!(n.title.as_deref(), Some("Dulcinea"));
        assert_eq!(n.season, Some(1));
        assert_eq!(n.episode, Some(1));
        assert_eq!(n.year, Some(2015));
        assert_eq!(n.meta.release_date.as_deref(), Some("2015-12-14"));
        assert_eq!(n.meta.runtime_min, Some(44));
        assert_eq!(n.meta.poster_url.as_deref(), Some("https://x/still.jpg"));
        assert_eq!(n.meta.tvdb_id, Some(5123));
        assert_eq!(n.meta.provider.as_deref(), Some("nfo"));
    }

    #[test]
    fn junk_prefix_is_skipped() {
        let xml = "https://www.themoviedb.org/movie/603-the-matrix\n\n<movie><title>The Matrix</title><year>1999</year></movie>\ntrailing junk";
        let n = parse(xml).unwrap();
        assert_eq!(n.title.as_deref(), Some("The Matrix"));
        assert_eq!(n.year, Some(1999));
        assert_eq!(n.meta.release_date.as_deref(), Some("1999"));
    }

    #[test]
    fn url_only_nfo_is_none() {
        assert!(parse("https://www.imdb.com/title/tt0133093/\n").is_none());
        assert!(parse("").is_none());
        assert!(parse("<movies><movie><title>x</title></movie></movies>").is_some());
        assert_eq!(detect_root("<episodedetails/>"), Some(NfoRoot::Episode));
    }

    #[test]
    fn truncated_nfo_keeps_what_was_read() {
        let n = parse("<movie><title>Half</title><year>2001</year><plot>unterminated").unwrap();
        assert_eq!(n.title.as_deref(), Some("Half"));
        assert_eq!(n.year, Some(2001));
    }

    #[test]
    fn year_with_date_tail_and_negative_numbers() {
        assert_eq!(parse_i32("2015-12-14"), Some(2015));
        assert_eq!(parse_i32("-1"), Some(-1));
        assert_eq!(parse_i32(" 7 "), Some(7));
        assert_eq!(parse_i32("abc"), None);
        let n = parse("<movie><title>x</title><year>2015-12-14</year></movie>").unwrap();
        assert_eq!(n.year, Some(2015));
    }

    #[test]
    fn runtime_forms() {
        assert_eq!(parse_runtime("117"), Some(117));
        assert_eq!(parse_runtime("117 min"), Some(117));
        assert_eq!(parse_runtime("1h 55m"), Some(115));
        assert_eq!(parse_runtime("1:55"), Some(115));
        assert_eq!(parse_runtime("0"), None);
        assert_eq!(parse_runtime("abc"), None);
    }

    #[test]
    fn find_sidecars() {
        let dir = tempfile::tempdir().unwrap();
        let show = dir.path().join("Show");
        let season = show.join("Season 01");
        std::fs::create_dir_all(&season).unwrap();
        let ep = season.join("Show - S01E01.mkv");
        std::fs::write(&ep, b"").unwrap();
        assert_eq!(find_nfo(&ep), None);
        assert_eq!(find_tvshow_nfo(&ep), None);
        std::fs::write(season.join("movie.nfo"), b"<movie/>").unwrap();
        assert_eq!(find_nfo(&ep), Some(season.join("movie.nfo")));
        std::fs::write(season.join("Show - S01E01.nfo"), b"<episodedetails/>").unwrap();
        assert_eq!(find_nfo(&ep), Some(season.join("Show - S01E01.nfo")));
        std::fs::write(show.join("tvshow.nfo"), b"<tvshow/>").unwrap();
        assert_eq!(find_tvshow_nfo(&ep), Some(show.join("tvshow.nfo")));
    }
}
