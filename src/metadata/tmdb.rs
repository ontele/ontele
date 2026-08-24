// Copyright 2026 The Ontele Authors
// SPDX-License-Identifier: Apache-2.0

//! The Movie Database v3 client (movies, TV, episodes). Images resolve to
//! `https://image.tmdb.org/t/p/<size><path>`.
//!
//! All calls go through [`Tmdb::get`], which spaces requests ≥ 30 ms apart
//! process-wide, honours `Retry-After` on 429 (one retry, ≤ 10 s) and turns
//! 401 into a clear "invalid TMDB API key" error.

use crate::model::{CastMember, Metadata};
use chrono::Utc;
use serde_json::Value;
use std::time::{Duration, Instant};
use unicode_normalization::UnicodeNormalization;

pub const IMG_BASE: &str = "https://image.tmdb.org/t/p/";

/// Minimum spacing between TMDB requests (TMDB allows ~50 req/s; be polite).
const MIN_GAP: Duration = Duration::from_millis(30);
const MAX_RETRY_AFTER: Duration = Duration::from_secs(10);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);

static GATE: tokio::sync::Mutex<Option<Instant>> = tokio::sync::Mutex::const_new(None);

/// Sleep until at least `MIN_GAP` has passed since the previous call.
async fn throttle() {
    let mut last = GATE.lock().await;
    if let Some(prev) = *last {
        let elapsed = prev.elapsed();
        if elapsed < MIN_GAP {
            tokio::time::sleep(MIN_GAP - elapsed).await;
        }
    }
    *last = Some(Instant::now());
}

#[derive(Clone)]
pub struct Tmdb {
    pub http: reqwest::Client,
    pub api_key: String,
    pub language: String,
    /// Override for tests.
    pub base_url: String,
}

impl Tmdb {
    pub fn new(http: reqwest::Client, api_key: String, language: String) -> Self {
        Self { http, api_key, language, base_url: "https://api.themoviedb.org/3".into() }
    }

    /// GET `<base>/<path>` with `api_key` + `language` + `params`. `Ok(None)`
    /// on 404; an error carrying the HTTP status otherwise.
    pub async fn get(&self, path: &str, params: &[(&str, String)]) -> anyhow::Result<Option<Value>> {
        if self.api_key.trim().is_empty() {
            anyhow::bail!("TMDB API key not configured");
        }
        let url = format!("{}/{}", self.base_url.trim_end_matches('/'), path.trim_start_matches('/'));
        let mut query: Vec<(&str, String)> = Vec::with_capacity(params.len() + 2);
        query.push(("api_key", self.api_key.clone()));
        if !self.language.trim().is_empty() {
            query.push(("language", self.language.clone()));
        }
        query.extend(params.iter().map(|(k, v)| (*k, v.clone())));

        let mut retried = false;
        loop {
            throttle().await;
            let resp = self
                .http
                .get(&url)
                .query(&query)
                .timeout(REQUEST_TIMEOUT)
                .send()
                .await
                // `without_url`: reqwest's Display includes the full URL, i.e. the api_key
                .map_err(|e| anyhow::anyhow!("tmdb {path}: {}", e.without_url()))?;
            let status = resp.status();
            if status.is_success() {
                let v: Value = resp.json().await.map_err(|e| anyhow::anyhow!("tmdb {path}: bad json: {e}"))?;
                return Ok(Some(v));
            }
            match status.as_u16() {
                404 => return Ok(None),
                401 => anyhow::bail!("invalid TMDB API key"),
                429 if !retried => {
                    retried = true;
                    let wait = resp
                        .headers()
                        .get(reqwest::header::RETRY_AFTER)
                        .and_then(|v| v.to_str().ok())
                        .and_then(|s| s.trim().parse::<u64>().ok())
                        .map(Duration::from_secs)
                        .unwrap_or(Duration::from_secs(1))
                        .min(MAX_RETRY_AFTER);
                    tracing::debug!(path, wait_ms = wait.as_millis() as u64, "tmdb rate limited, retrying");
                    tokio::time::sleep(wait).await;
                    continue;
                }
                _ => {
                    let body = resp.text().await.unwrap_or_default();
                    let msg = serde_json::from_str::<Value>(&body)
                        .ok()
                        .and_then(|v| v.get("status_message").and_then(Value::as_str).map(str::to_string))
                        .unwrap_or_else(|| body.chars().take(200).collect());
                    anyhow::bail!("tmdb {path}: HTTP {status}: {msg}");
                }
            }
        }
    }

    /// Search + details for a movie; `None` when nothing matches.
    pub async fn movie(&self, title: &str, year: Option<i32>) -> anyhow::Result<Option<Metadata>> {
        let title = title.trim();
        if title.is_empty() {
            return Ok(None);
        }
        let mut params = vec![("query", title.to_string()), ("include_adult", "false".to_string())];
        if let Some(y) = year.filter(|y| *y > 1800) {
            params.push(("primary_release_year", y.to_string()));
        }
        let mut res = self.get("search/movie", &params).await?;
        let mut id = res.as_ref().and_then(|v| pick_best(results_of(v), title, year, "title", "release_date"));
        if id.is_none() && params.len() > 2 {
            // Year may be wrong in the filename; retry without the filter.
            res = self.get("search/movie", &params[..2]).await?;
            id = res.as_ref().and_then(|v| pick_best(results_of(v), title, year, "title", "release_date"));
        }
        match id {
            Some(id) => self.movie_by_id(id).await,
            None => Ok(None),
        }
    }

    pub async fn movie_by_id(&self, id: i64) -> anyhow::Result<Option<Metadata>> {
        let v = self.get(&format!("movie/{id}"), &[("append_to_response", "credits,release_dates".into())]).await?;
        Ok(v.as_ref().map(parse_movie))
    }

    /// Search + details for a TV show.
    pub async fn show(&self, name: &str, year: Option<i32>) -> anyhow::Result<Option<Metadata>> {
        let name = name.trim();
        if name.is_empty() {
            return Ok(None);
        }
        let mut params = vec![("query", name.to_string()), ("include_adult", "false".to_string())];
        if let Some(y) = year.filter(|y| *y > 1800) {
            params.push(("first_air_date_year", y.to_string()));
        }
        let mut res = self.get("search/tv", &params).await?;
        let mut id = res.as_ref().and_then(|v| pick_best(results_of(v), name, year, "name", "first_air_date"));
        if id.is_none() && params.len() > 2 {
            res = self.get("search/tv", &params[..2]).await?;
            id = res.as_ref().and_then(|v| pick_best(results_of(v), name, year, "name", "first_air_date"));
        }
        match id {
            Some(id) => self.show_by_id(id).await,
            None => Ok(None),
        }
    }

    pub async fn show_by_id(&self, id: i64) -> anyhow::Result<Option<Metadata>> {
        let v = self.get(&format!("tv/{id}"), &[("append_to_response", "credits,content_ratings".into())]).await?;
        Ok(v.as_ref().map(parse_show))
    }

    /// Episode details (title, overview, still, air date) for a show id.
    pub async fn episode(&self, show_id: i64, season: i32, episode: i32) -> anyhow::Result<Option<Metadata>> {
        if season < 0 || episode < 0 {
            return Ok(None);
        }
        let v = self.get(&format!("tv/{show_id}/season/{season}/episode/{episode}"), &[]).await?;
        Ok(v.as_ref().map(|v| parse_episode(v, show_id)))
    }
}

fn results_of(v: &Value) -> &[Value] {
    v.get("results").and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[])
}

fn s(v: &Value, key: &str) -> Option<String> {
    v.get(key).and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()).map(str::to_string)
}

fn img(path: Option<String>, size: &str) -> Option<String> {
    path.map(|p| format!("{IMG_BASE}{size}{p}"))
}

fn genres_of(v: &Value) -> Vec<String> {
    v.get("genres")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(|g| s(g, "name")).collect())
        .unwrap_or_default()
}

fn cast_of(v: &Value) -> Vec<CastMember> {
    v.get("credits")
        .and_then(|c| c.get("cast"))
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|c| {
                    let name = s(c, "name")?;
                    Some(CastMember { name, character: s(c, "character"), profile: img(s(c, "profile_path"), "w185") })
                })
                .take(12)
                .collect()
        })
        .unwrap_or_default()
}

fn studio_of(v: &Value) -> Option<String> {
    v.get("production_companies").and_then(Value::as_array).and_then(|a| a.iter().find_map(|c| s(c, "name")))
}

fn rating_of(v: &Value) -> Option<f64> {
    v.get("vote_average").and_then(Value::as_f64).filter(|r| *r > 0.0).map(|r| (r * 10.0).round() / 10.0)
}

fn votes_of(v: &Value) -> Option<u64> {
    v.get("vote_count").and_then(Value::as_u64).filter(|n| *n > 0)
}

fn common(v: &Value, title_key: &str, date_key: &str) -> Metadata {
    let title = s(v, title_key);
    let original = s(v, &format!("original_{title_key}"));
    Metadata {
        provider: Some("tmdb".into()),
        provider_id: v.get("id").and_then(Value::as_i64).map(|i| i.to_string()),
        tmdb_id: v.get("id").and_then(Value::as_i64),
        imdb_id: s(v, "imdb_id").or_else(|| v.get("external_ids").and_then(|e| s(e, "imdb_id"))),
        tvdb_id: v.get("external_ids").and_then(|e| e.get("tvdb_id")).and_then(Value::as_i64),
        original_title: match (&title, original) {
            (Some(t), Some(o)) if o != *t => Some(o),
            _ => None,
        },
        overview: s(v, "overview"),
        tagline: s(v, "tagline"),
        genres: genres_of(v),
        rating: rating_of(v),
        votes: votes_of(v),
        release_date: s(v, date_key),
        studio: studio_of(v),
        cast: cast_of(v),
        poster_url: img(s(v, "poster_path"), "w500"),
        backdrop_url: img(s(v, "backdrop_path"), "w1280"),
        updated: Some(Utc::now()),
        ..Default::default()
    }
}

/// Map `movie/{id}?append_to_response=credits,release_dates`.
pub fn parse_movie(v: &Value) -> Metadata {
    let mut m = common(v, "title", "release_date");
    m.runtime_min = v.get("runtime").and_then(Value::as_u64).filter(|r| *r > 0).map(|r| r as u32);
    m.content_rating =
        v.get("release_dates").and_then(|r| r.get("results")).and_then(Value::as_array).and_then(|countries| {
            let us = countries.iter().find(|c| c.get("iso_3166_1").and_then(Value::as_str) == Some("US"))?;
            us.get("release_dates").and_then(Value::as_array)?.iter().find_map(|d| s(d, "certification"))
        });
    m
}

/// Map `tv/{id}?append_to_response=credits,content_ratings`.
pub fn parse_show(v: &Value) -> Metadata {
    let mut m = common(v, "name", "first_air_date");
    m.runtime_min = v
        .get("episode_run_time")
        .and_then(Value::as_array)
        .and_then(|a| a.first())
        .and_then(Value::as_u64)
        .filter(|r| *r > 0)
        .map(|r| r as u32);
    m.content_rating =
        v.get("content_ratings").and_then(|r| r.get("results")).and_then(Value::as_array).and_then(|countries| {
            countries
                .iter()
                .find(|c| c.get("iso_3166_1").and_then(Value::as_str) == Some("US"))
                .and_then(|c| s(c, "rating"))
        });
    if m.studio.is_none() {
        m.studio = v.get("networks").and_then(Value::as_array).and_then(|a| a.iter().find_map(|n| s(n, "name")));
    }
    m
}

/// Map `tv/{show}/season/{s}/episode/{e}`. The episode name lands in
/// `original_title` so callers can use it as the item title; `tmdb_id` is
/// the *show* id (what later lookups need), `provider_id` the episode id.
pub fn parse_episode(v: &Value, show_id: i64) -> Metadata {
    Metadata {
        provider: Some("tmdb".into()),
        provider_id: v.get("id").and_then(Value::as_i64).map(|i| i.to_string()),
        tmdb_id: Some(show_id),
        original_title: s(v, "name"),
        overview: s(v, "overview"),
        rating: rating_of(v),
        votes: votes_of(v),
        runtime_min: v.get("runtime").and_then(Value::as_u64).filter(|r| *r > 0).map(|r| r as u32),
        release_date: s(v, "air_date"),
        still_url: img(s(v, "still_path"), "w300"),
        cast: v
            .get("guest_stars")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|c| {
                        let name = s(c, "name")?;
                        Some(CastMember {
                            name,
                            character: s(c, "character"),
                            profile: img(s(c, "profile_path"), "w185"),
                        })
                    })
                    .take(12)
                    .collect()
            })
            .unwrap_or_default(),
        updated: Some(Utc::now()),
        ..Default::default()
    }
}

/// Lowercase, NFKD with combining marks stripped, articles dropped,
/// punctuation → space, whitespace collapsed. "The Matrix" == "matrix".
pub fn normalize_title(t: &str) -> String {
    let folded: String = t
        .nfkd()
        .filter(|c| !unicode_normalization::char::is_combining_mark(*c))
        .collect::<String>()
        .to_lowercase()
        .replace('&', " and ");
    let mut words: Vec<String> = Vec::new();
    let mut cur = String::new();
    for c in folded.chars() {
        if c.is_alphanumeric() {
            cur.push(c);
        } else if !cur.is_empty() {
            words.push(std::mem::take(&mut cur));
        }
    }
    if !cur.is_empty() {
        words.push(cur);
    }
    const ARTICLES: &[&str] =
        &["the", "a", "an", "le", "la", "les", "der", "die", "das", "el", "los", "las", "il", "lo"];
    if words.len() > 1 && ARTICLES.contains(&words[0].as_str()) {
        words.remove(0);
    }
    words.join(" ")
}

fn year_of(v: &Value, date_key: &str) -> Option<i32> {
    v.get(date_key).and_then(Value::as_str).and_then(|d| d.get(0..4)).and_then(|y| y.parse().ok())
}

/// Parse a TMDB search response and pick the best hit for `title`/`year`
/// (exact title match > year match > popularity). Pure, tested.
pub fn pick_best(results: &[Value], title: &str, year: Option<i32>, title_key: &str, date_key: &str) -> Option<i64> {
    let want = normalize_title(title);
    if want.is_empty() {
        return None;
    }
    let mut best: Option<(f64, bool, i64)> = None;
    for r in results {
        let Some(id) = r.get("id").and_then(Value::as_i64) else {
            continue;
        };
        let names = [
            r.get(title_key).and_then(Value::as_str).map(normalize_title),
            r.get(format!("original_{title_key}").as_str()).and_then(Value::as_str).map(normalize_title),
        ];
        let exact = names.iter().flatten().any(|n| *n == want);
        let partial = !exact
            && names
                .iter()
                .flatten()
                .any(|n| !n.is_empty() && (n.starts_with(&want) || want.starts_with(n.as_str()) || n.contains(&want)));
        let mut score = 0.0;
        if exact {
            score += 100.0;
        } else if partial {
            score += 30.0;
        }
        if let (Some(y), Some(ry)) = (year, year_of(r, date_key))
            && (y - ry).abs() <= 1
        {
            score += 50.0;
        }
        let popularity = r.get("popularity").and_then(Value::as_f64).unwrap_or(0.0).max(0.0);
        // popularity only ever breaks ties: it can never add a full point
        score += popularity / (popularity + 1.0) * 0.99;
        let qualifies = score >= 50.0 || (year.is_none() && exact);
        if !qualifies {
            continue;
        }
        match best {
            Some((bs, _, _)) if bs >= score => {}
            _ => best = Some((score, exact, id)),
        }
    }
    best.map(|(_, _, id)| id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn normalizes_titles() {
        assert_eq!(normalize_title("The Matrix"), "matrix");
        assert_eq!(normalize_title("Amélie"), "amelie");
        assert_eq!(normalize_title("WALL·E"), "wall e");
        assert_eq!(normalize_title("Spider-Man: No Way Home"), "spider man no way home");
        assert_eq!(normalize_title("Fast & Furious"), "fast and furious");
        assert_eq!(normalize_title("A"), "a");
        assert_eq!(normalize_title("  "), "");
    }

    fn results() -> Vec<Value> {
        vec![
            json!({ "id": 1, "title": "Alien: Covenant", "release_date": "2017-05-09", "popularity": 90.0 }),
            json!({ "id": 2, "title": "Alien", "release_date": "1979-05-25", "popularity": 60.0 }),
            json!({ "id": 3, "title": "Aliens", "release_date": "1986-07-18", "popularity": 70.0 }),
            json!({ "id": 4, "title": "Alien", "original_title": "Alien (Fan Cut)", "release_date": "2005-01-01", "popularity": 5.0 }),
        ]
    }

    #[test]
    fn picks_exact_title_and_year() {
        assert_eq!(pick_best(&results(), "Alien", Some(1979), "title", "release_date"), Some(2));
        // year off by one still counts
        assert_eq!(pick_best(&results(), "Alien", Some(1980), "title", "release_date"), Some(2));
    }

    #[test]
    fn no_year_uses_popularity_among_exact_matches() {
        assert_eq!(pick_best(&results(), "alien", None, "title", "release_date"), Some(2));
        assert_eq!(pick_best(&results(), "The Aliens", None, "title", "release_date"), Some(3));
    }

    #[test]
    fn year_match_alone_qualifies_when_title_differs() {
        // Filename says "Alien Covenant" (no colon): exact after normalisation anyway
        assert_eq!(pick_best(&results(), "Alien Covenant", Some(2017), "title", "release_date"), Some(1));
        // Totally different title but year matches one entry → ≥ 50
        assert_eq!(pick_best(&results(), "Xenomorph", Some(1986), "title", "release_date"), Some(3));
        // No year, no match → None
        assert_eq!(pick_best(&results(), "Xenomorph", None, "title", "release_date"), None);
        assert_eq!(pick_best(&[], "Alien", None, "title", "release_date"), None);
    }

    #[test]
    fn tv_keys() {
        let r = vec![
            json!({ "id": 10, "name": "The Office", "first_air_date": "2005-03-24", "popularity": 300.0 }),
            json!({ "id": 11, "name": "The Office", "first_air_date": "2001-07-09", "popularity": 50.0 }),
        ];
        assert_eq!(pick_best(&r, "Office", Some(2001), "name", "first_air_date"), Some(11));
        assert_eq!(pick_best(&r, "The Office", None, "name", "first_air_date"), Some(10));
    }

    fn movie_json() -> Value {
        json!({
            "id": 78, "imdb_id": "tt0083658", "title": "Blade Runner", "original_title": "Blade Runner",
            "overview": "In the smog-choked dystopian Los Angeles of 2019...", "tagline": "Man has made his match...",
            "genres": [{"id": 878, "name": "Science Fiction"}, {"id": 18, "name": "Drama"}],
            "vote_average": 7.933, "vote_count": 13000, "runtime": 117, "release_date": "1982-06-25",
            "poster_path": "/p.jpg", "backdrop_path": "/b.jpg",
            "production_companies": [{"name": "The Ladd Company"}, {"name": "Warner Bros."}],
            "release_dates": { "results": [
                {"iso_3166_1": "DE", "release_dates": [{"certification": "16"}]},
                {"iso_3166_1": "US", "release_dates": [{"certification": ""}, {"certification": "R"}]}
            ]},
            "credits": { "cast": [
                {"name": "Harrison Ford", "character": "Rick Deckard", "profile_path": "/ford.jpg"},
                {"name": "Rutger Hauer", "character": "Roy Batty", "profile_path": null}
            ]}
        })
    }

    #[test]
    fn maps_movie_details() {
        let m = parse_movie(&movie_json());
        assert_eq!(m.provider.as_deref(), Some("tmdb"));
        assert_eq!(m.provider_id.as_deref(), Some("78"));
        assert_eq!(m.tmdb_id, Some(78));
        assert_eq!(m.imdb_id.as_deref(), Some("tt0083658"));
        assert_eq!(m.original_title, None, "identical original title is dropped");
        assert_eq!(m.genres, vec!["Science Fiction", "Drama"]);
        assert_eq!(m.rating, Some(7.9));
        assert_eq!(m.votes, Some(13000));
        assert_eq!(m.runtime_min, Some(117));
        assert_eq!(m.release_date.as_deref(), Some("1982-06-25"));
        assert_eq!(m.content_rating.as_deref(), Some("R"));
        assert_eq!(m.studio.as_deref(), Some("The Ladd Company"));
        assert_eq!(m.poster_url.as_deref(), Some("https://image.tmdb.org/t/p/w500/p.jpg"));
        assert_eq!(m.backdrop_url.as_deref(), Some("https://image.tmdb.org/t/p/w1280/b.jpg"));
        assert_eq!(m.cast.len(), 2);
        assert_eq!(m.cast[0].profile.as_deref(), Some("https://image.tmdb.org/t/p/w185/ford.jpg"));
        assert_eq!(m.cast[1].profile, None);
        assert!(m.updated.is_some());
    }

    #[test]
    fn maps_show_and_episode() {
        let show = json!({
            "id": 63639, "name": "The Expanse", "original_name": "The Expanse", "first_air_date": "2015-12-14",
            "episode_run_time": [43], "genres": [{"name": "Drama"}], "networks": [{"name": "Syfy"}],
            "content_ratings": {"results": [{"iso_3166_1": "US", "rating": "TV-14"}]},
            "poster_path": "/sp.jpg", "vote_average": 8.4, "vote_count": 1500
        });
        let m = parse_show(&show);
        assert_eq!(m.runtime_min, Some(43));
        assert_eq!(m.content_rating.as_deref(), Some("TV-14"));
        assert_eq!(m.studio.as_deref(), Some("Syfy"));
        assert_eq!(m.release_date.as_deref(), Some("2015-12-14"));
        assert_eq!(m.poster_url.as_deref(), Some("https://image.tmdb.org/t/p/w500/sp.jpg"));

        let ep = json!({ "id": 1, "name": "Dulcinea", "overview": "Miller...", "still_path": "/s.jpg",
                         "air_date": "2015-12-14", "vote_average": 7.6, "vote_count": 40, "runtime": 44 });
        let e = parse_episode(&ep, 63639);
        assert_eq!(e.original_title.as_deref(), Some("Dulcinea"));
        assert_eq!(e.tmdb_id, Some(63639));
        assert_eq!(e.provider_id.as_deref(), Some("1"));
        assert_eq!(e.still_url.as_deref(), Some("https://image.tmdb.org/t/p/w300/s.jpg"));
        assert_eq!(e.release_date.as_deref(), Some("2015-12-14"));
        assert_eq!(e.runtime_min, Some(44));
    }

    // ---- HTTP shape via an in-process axum server ---------------------------

    use axum::{
        Router,
        extract::{Path, Query},
        routing::get,
    };
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    type Seen = Arc<Mutex<Vec<(String, HashMap<String, String>)>>>;

    async fn spawn_server(seen: Seen, fail_first_with_429: bool) -> String {
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let s1 = seen.clone();
        let s2 = seen.clone();
        let s3 = seen.clone();
        let app = Router::new()
            .route(
                "/3/search/movie",
                get(move |Query(q): Query<HashMap<String, String>>| {
                    let s1 = s1.clone();
                    let calls = calls.clone();
                    async move {
                        s1.lock().unwrap().push(("search/movie".into(), q.clone()));
                        let n = calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        if fail_first_with_429 && n == 0 {
                            return (
                                axum::http::StatusCode::TOO_MANY_REQUESTS,
                                [("retry-after", "0")],
                                axum::Json(json!({"status_message": "slow down"})),
                            );
                        }
                        if q.get("api_key").map(String::as_str) != Some("k3y") {
                            return (
                                axum::http::StatusCode::UNAUTHORIZED,
                                [("retry-after", "0")],
                                axum::Json(json!({"status_message": "Invalid API key"})),
                            );
                        }
                        (
                            axum::http::StatusCode::OK,
                            [("retry-after", "0")],
                            axum::Json(json!({ "results": [
                                { "id": 78, "title": "Blade Runner", "release_date": "1982-06-25", "popularity": 50.0 },
                                { "id": 335984, "title": "Blade Runner 2049", "release_date": "2017-10-04", "popularity": 80.0 }
                            ]})),
                        )
                    }
                }),
            )
            .route(
                "/3/movie/{id}",
                get(move |Path(id): Path<i64>, Query(q): Query<HashMap<String, String>>| {
                    let s2 = s2.clone();
                    async move {
                        s2.lock().unwrap().push((format!("movie/{id}"), q));
                        if id == 404 {
                            return (axum::http::StatusCode::NOT_FOUND, axum::Json(json!({"status_message": "not found"})));
                        }
                        (axum::http::StatusCode::OK, axum::Json(movie_json()))
                    }
                }),
            )
            .route(
                "/3/tv/{id}/season/{s}/episode/{e}",
                get(move |Path((id, s, e)): Path<(i64, i32, i32)>, Query(q): Query<HashMap<String, String>>| {
                    let s3 = s3.clone();
                    async move {
                        s3.lock().unwrap().push((format!("tv/{id}/season/{s}/episode/{e}"), q));
                        axum::Json(json!({ "id": 9, "name": "Pilot", "still_path": "/x.jpg" }))
                    }
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://{addr}/3")
    }

    #[tokio::test]
    async fn movie_search_hits_expected_urls_and_params() {
        let seen: Seen = Default::default();
        let base = spawn_server(seen.clone(), false).await;
        let t = Tmdb { http: reqwest::Client::new(), api_key: "k3y".into(), language: "en-US".into(), base_url: base };
        let m = t.movie("Blade Runner", Some(1982)).await.unwrap().expect("match");
        assert_eq!(m.tmdb_id, Some(78));
        assert_eq!(m.content_rating.as_deref(), Some("R"));
        let calls = seen.lock().unwrap().clone();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].0, "search/movie");
        assert_eq!(calls[0].1.get("query").map(String::as_str), Some("Blade Runner"));
        assert_eq!(calls[0].1.get("primary_release_year").map(String::as_str), Some("1982"));
        assert_eq!(calls[0].1.get("language").map(String::as_str), Some("en-US"));
        assert_eq!(calls[0].1.get("api_key").map(String::as_str), Some("k3y"));
        assert_eq!(calls[1].0, "movie/78");
        assert_eq!(calls[1].1.get("append_to_response").map(String::as_str), Some("credits,release_dates"));

        let e = t.episode(5, 1, 2).await.unwrap().unwrap();
        assert_eq!(e.original_title.as_deref(), Some("Pilot"));
        assert_eq!(seen.lock().unwrap().last().unwrap().0, "tv/5/season/1/episode/2");

        // unknown id → 404 → None
        assert!(t.movie_by_id(404).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn retries_once_on_429_and_reports_bad_key() {
        let seen: Seen = Default::default();
        let base = spawn_server(seen.clone(), true).await;
        let t = Tmdb {
            http: reqwest::Client::new(),
            api_key: "k3y".into(),
            language: "en-US".into(),
            base_url: base.clone(),
        };
        let m = t.movie("Blade Runner", None).await.unwrap();
        assert_eq!(m.unwrap().tmdb_id, Some(78));
        let searches = seen.lock().unwrap().iter().filter(|c| c.0 == "search/movie").count();
        assert_eq!(searches, 2, "one 429 + one retry");

        let bad =
            Tmdb { http: reqwest::Client::new(), api_key: "nope".into(), language: String::new(), base_url: base };
        let err = bad.movie("Blade Runner", None).await.unwrap_err();
        assert_eq!(err.to_string(), "invalid TMDB API key");

        let none = Tmdb {
            http: reqwest::Client::new(),
            api_key: String::new(),
            language: String::new(),
            base_url: "http://127.0.0.1:1".into(),
        };
        assert!(none.movie("x", None).await.is_err());

        // transport errors must not echo the URL (it carries the api key)
        let dead = Tmdb {
            http: reqwest::Client::new(),
            api_key: "s3cret".into(),
            language: String::new(),
            base_url: "http://127.0.0.1:1".into(),
        };
        let err = dead.movie("x", None).await.unwrap_err().to_string();
        assert!(!err.contains("s3cret"), "{err}");
    }
}
