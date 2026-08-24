// Copyright 2026 The Ontele Authors
// SPDX-License-Identifier: Apache-2.0

//! MusicBrainz (release lookup) + Cover Art Archive. MusicBrainz requires a
//! descriptive User-Agent and ≤1 request/second; the client enforces both.
//! (The shared `reqwest::Client` from `state::http_client` carries the UA.)

use crate::model::Metadata;
use chrono::Utc;
use serde_json::Value;
use std::time::{Duration, Instant};

/// MusicBrainz asks for at most one request per second; leave headroom.
const MIN_GAP: Duration = Duration::from_millis(1100);
const RETRY_WAIT: Duration = Duration::from_secs(2);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);
const CAA_TIMEOUT: Duration = Duration::from_secs(15);

static GATE: tokio::sync::Mutex<Option<Instant>> = tokio::sync::Mutex::const_new(None);

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
pub struct MusicBrainz {
    pub http: reqwest::Client,
    pub base_url: String,
    pub caa_url: String,
}

impl MusicBrainz {
    pub fn new(http: reqwest::Client) -> Self {
        Self { http, base_url: "https://musicbrainz.org/ws/2".into(), caa_url: "https://coverartarchive.org".into() }
    }

    /// Rate-limited GET returning parsed JSON; `Ok(None)` on 404. 503/429
    /// (MusicBrainz' "slow down") waits 2 s and retries once.
    async fn get(&self, path: &str, params: &[(&str, String)]) -> anyhow::Result<Option<Value>> {
        let url = format!("{}/{}", self.base_url.trim_end_matches('/'), path.trim_start_matches('/'));
        let mut retried = false;
        loop {
            throttle().await;
            let resp = self
                .http
                .get(&url)
                .query(params)
                .header(reqwest::header::ACCEPT, "application/json")
                .timeout(REQUEST_TIMEOUT)
                .send()
                .await
                .map_err(|e| anyhow::anyhow!("musicbrainz {path}: {e}"))?;
            let status = resp.status();
            if status.is_success() {
                let v: Value = resp.json().await.map_err(|e| anyhow::anyhow!("musicbrainz {path}: bad json: {e}"))?;
                return Ok(Some(v));
            }
            match status.as_u16() {
                404 => return Ok(None),
                503 | 429 if !retried => {
                    retried = true;
                    tracing::debug!(path, "musicbrainz busy, retrying in 2s");
                    tokio::time::sleep(RETRY_WAIT).await;
                }
                _ => {
                    let body = resp.text().await.unwrap_or_default();
                    let msg = serde_json::from_str::<Value>(&body)
                        .ok()
                        .and_then(|v| v.get("error").and_then(Value::as_str).map(str::to_string))
                        .unwrap_or_else(|| body.chars().take(200).collect());
                    anyhow::bail!("musicbrainz {path}: HTTP {status}: {msg}");
                }
            }
        }
    }

    /// Find a release by artist + album (or by a known release MBID from the
    /// tags). Fills `mbid`, `release_date`, `genres` (tags), `poster_url`
    /// (CAA front-500 when available).
    pub async fn release(&self, artist: &str, album: &str, mbid: Option<&str>) -> anyhow::Result<Option<Metadata>> {
        let mut meta: Option<Metadata> = None;

        if let Some(id) = mbid.map(str::trim).filter(|m| is_mbid(m)) {
            let v = self
                .get(
                    &format!("release/{id}"),
                    &[("inc", "artist-credits+release-groups+genres+tags+labels".into()), ("fmt", "json".into())],
                )
                .await?;
            if let Some(v) = v.as_ref() {
                meta = Some(parse_release(v));
            }
        }

        if meta.is_none() {
            let album = album.trim();
            if album.is_empty() {
                return Ok(None);
            }
            let query = build_query(artist, album);
            let v = self.get("release/", &[("query", query), ("fmt", "json".into()), ("limit", "5".into())]).await?;
            let releases =
                v.as_ref().and_then(|v| v.get("releases")).and_then(Value::as_array).cloned().unwrap_or_default();
            if let Some(r) = pick_release(&releases, album) {
                meta = Some(parse_release(r));
            }
        }

        let Some(mut meta) = meta else {
            return Ok(None);
        };
        if let Some(id) = meta.mbid.clone()
            && let Some(url) = self.cover_url(&id).await
        {
            meta.poster_url = Some(url);
        }
        Ok(Some(meta))
    }

    /// `<caa>/release/<mbid>/front-500` when the archive has a front image
    /// (2xx or a 3xx redirect to the actual file). Nothing is downloaded.
    pub async fn cover_url(&self, mbid: &str) -> Option<String> {
        let url = format!("{}/release/{}/front-500", self.caa_url.trim_end_matches('/'), mbid);
        let ok = match self.http.head(&url).timeout(CAA_TIMEOUT).send().await {
            Ok(r) => r.status().is_success() || r.status().is_redirection(),
            Err(e) => {
                tracing::debug!(mbid, error = %e, "cover art archive HEAD failed");
                false
            }
        };
        if ok { Some(url) } else { None }
    }
}

fn is_mbid(s: &str) -> bool {
    s.len() == 36 && s.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-') && s.matches('-').count() == 4
}

/// Escape a value for a quoted Lucene term.
fn lucene_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        if c == '"' || c == '\\' {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('"');
    out
}

/// `release:"album" AND artist:"artist"` (artist omitted when blank/unknown).
pub fn build_query(artist: &str, album: &str) -> String {
    let artist = artist.trim();
    let mut q = format!("release:{}", lucene_quote(album.trim()));
    if !artist.is_empty()
        && !artist.eq_ignore_ascii_case("unknown artist")
        && !artist.eq_ignore_ascii_case("various artists")
    {
        q.push_str(" AND artist:");
        q.push_str(&lucene_quote(artist));
    }
    q
}

fn s(v: &Value, key: &str) -> Option<String> {
    v.get(key).and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()).map(str::to_string)
}

fn score_of(v: &Value) -> i64 {
    match v.get("score") {
        Some(Value::Number(n)) => n.as_i64().or_else(|| n.as_f64().map(|f| f as i64)).unwrap_or(0),
        Some(Value::String(s)) => s.parse().unwrap_or(0),
        _ => 0,
    }
}

/// First result with score ≥ 80 whose title matches case-insensitively, else
/// the top result when its score is ≥ 95.
pub fn pick_release<'a>(releases: &'a [Value], album: &str) -> Option<&'a Value> {
    let want = album.trim().to_lowercase();
    if let Some(r) =
        releases.iter().find(|r| score_of(r) >= 80 && s(r, "title").map(|t| t.to_lowercase() == want).unwrap_or(false))
    {
        return Some(r);
    }
    releases.first().filter(|r| score_of(r) >= 95)
}

/// Map a release document (search hit or `release/{mbid}` lookup) to metadata.
pub fn parse_release(v: &Value) -> Metadata {
    let mbid = s(v, "id");
    let mut genres: Vec<(String, i64)> = Vec::new();
    for key in ["genres", "tags"] {
        if let Some(arr) = v.get(key).and_then(Value::as_array) {
            for g in arr {
                if let Some(name) = s(g, "name") {
                    let count = g.get("count").and_then(Value::as_i64).unwrap_or(0);
                    if let Some(e) = genres.iter_mut().find(|(n, _)| n.eq_ignore_ascii_case(&name)) {
                        e.1 = e.1.max(count);
                    } else {
                        genres.push((name, count));
                    }
                }
            }
        }
    }
    // Release-group level tags are often richer than the release's own.
    if let Some(rg) = v.get("release-group") {
        for key in ["genres", "tags"] {
            if let Some(arr) = rg.get(key).and_then(Value::as_array) {
                for g in arr {
                    if let Some(name) = s(g, "name") {
                        let count = g.get("count").and_then(Value::as_i64).unwrap_or(0);
                        if !genres.iter().any(|(n, _)| n.eq_ignore_ascii_case(&name)) {
                            genres.push((name, count));
                        }
                    }
                }
            }
        }
    }
    genres.sort_by_key(|g| std::cmp::Reverse(g.1));
    let genres: Vec<String> = genres.into_iter().take(5).map(|(n, _)| title_case(&n)).collect();

    let studio = v
        .get("label-info")
        .and_then(Value::as_array)
        .and_then(|a| a.first())
        .and_then(|li| li.get("label"))
        .and_then(|l| s(l, "name"));

    let release_date = s(v, "date").or_else(|| v.get("release-group").and_then(|rg| s(rg, "first-release-date")));

    let artist_credit = v.get("artist-credit").and_then(Value::as_array).map(|a| {
        a.iter()
            .map(|c| {
                let name = s(c, "name").or_else(|| c.get("artist").and_then(|ar| s(ar, "name"))).unwrap_or_default();
                let join = s(c, "joinphrase").unwrap_or_default();
                format!("{name}{join}")
            })
            .collect::<String>()
    });

    Metadata {
        provider: Some("musicbrainz".into()),
        provider_id: mbid.clone(),
        mbid,
        original_title: s(v, "title"),
        overview: v.get("disambiguation").and_then(Value::as_str).filter(|d| !d.is_empty()).map(
            |d| match artist_credit.as_deref().filter(|a| !a.is_empty()) {
                Some(a) => format!("{a} — {d}"),
                None => d.to_string(),
            },
        ),
        genres,
        release_date,
        studio,
        updated: Some(Utc::now()),
        ..Default::default()
    }
}

fn title_case(s: &str) -> String {
    s.split(' ')
        .map(|w| {
            let mut c = w.chars();
            match c.next() {
                Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn search_json() -> Value {
        json!({ "created": "2026-01-01T00:00:00Z", "count": 2, "offset": 0, "releases": [
            { "id": "47b83a2e-4a5f-4a4b-9d3e-1c1a2b3c4d5e", "score": 100, "title": "Discovery", "date": "2001-03-12",
              "country": "FR", "status": "Official",
              "artist-credit": [{ "name": "Daft Punk", "artist": { "id": "x", "name": "Daft Punk" } }],
              "release-group": { "id": "rg1", "primary-type": "Album", "first-release-date": "2001-02-26" },
              "label-info": [{ "catalog-number": "V2940", "label": { "id": "l1", "name": "Virgin" } }],
              "tags": [{ "count": 7, "name": "electronic" }, { "count": 3, "name": "house" }, { "count": 9, "name": "french house" }]
            },
            { "id": "ffffffff-4a5f-4a4b-9d3e-1c1a2b3c4d5e", "score": 90, "title": "Discovery (Deluxe)", "date": "2002" }
        ]})
    }

    #[test]
    fn query_escapes_quotes() {
        assert_eq!(build_query("Daft Punk", "Discovery"), r#"release:"Discovery" AND artist:"Daft Punk""#);
        assert_eq!(build_query("", r#"Say "Hi""#), r#"release:"Say \"Hi\"""#);
        assert_eq!(build_query("Various Artists", "Now 42"), r#"release:"Now 42""#);
    }

    #[test]
    fn picks_and_parses_release() {
        let v = search_json();
        let rel = v["releases"].as_array().unwrap();
        let picked = pick_release(rel, "discovery").unwrap();
        assert_eq!(picked["id"], "47b83a2e-4a5f-4a4b-9d3e-1c1a2b3c4d5e");
        // Title mismatch but top score ≥ 95 → top result
        assert_eq!(pick_release(rel, "Something Else").unwrap()["id"], "47b83a2e-4a5f-4a4b-9d3e-1c1a2b3c4d5e");
        // Title match on a lower-scored result
        assert_eq!(pick_release(rel, "Discovery (Deluxe)").unwrap()["id"], "ffffffff-4a5f-4a4b-9d3e-1c1a2b3c4d5e");
        let low = vec![json!({ "id": "a", "score": 60, "title": "Other" })];
        assert!(pick_release(&low, "Other").is_none() || score_of(&low[0]) >= 80);
        assert!(pick_release(&low, "Zzz").is_none());

        let m = parse_release(picked);
        assert_eq!(m.provider.as_deref(), Some("musicbrainz"));
        assert_eq!(m.mbid.as_deref(), Some("47b83a2e-4a5f-4a4b-9d3e-1c1a2b3c4d5e"));
        assert_eq!(m.release_date.as_deref(), Some("2001-03-12"));
        assert_eq!(m.studio.as_deref(), Some("Virgin"));
        assert_eq!(m.genres, vec!["French House", "Electronic", "House"]);
        assert_eq!(m.original_title.as_deref(), Some("Discovery"));
        assert!(m.poster_url.is_none());
    }

    #[test]
    fn genres_capped_and_release_group_fallback() {
        let v = json!({ "id": "11111111-2222-3333-4444-555555555555", "title": "X",
            "genres": [{"name":"a","count":1},{"name":"b","count":2},{"name":"c","count":3},{"name":"d","count":4},{"name":"e","count":5},{"name":"f","count":6}],
            "release-group": { "first-release-date": "1999-01-01", "tags": [{"name":"z","count":1}] } });
        let m = parse_release(&v);
        assert_eq!(m.genres.len(), 5);
        assert_eq!(m.genres[0], "F");
        assert_eq!(m.release_date.as_deref(), Some("1999-01-01"));
        assert!(is_mbid("11111111-2222-3333-4444-555555555555"));
        assert!(!is_mbid("nope"));
    }

    // ---- in-process server: URL / param shape + CAA probing -----------------

    use axum::{
        Router,
        extract::{Path, Query},
        http::StatusCode,
        routing::{get, head},
    };
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    type Seen = Arc<Mutex<Vec<(String, HashMap<String, String>)>>>;

    async fn spawn(seen: Seen) -> String {
        let s1 = seen.clone();
        let s2 = seen.clone();
        let app = Router::new()
            .route(
                "/ws/2/release/",
                get(move |Query(q): Query<HashMap<String, String>>| {
                    let s1 = s1.clone();
                    async move {
                        s1.lock().unwrap().push(("search".into(), q));
                        axum::Json(search_json())
                    }
                }),
            )
            .route(
                "/ws/2/release/{mbid}",
                get(move |Path(mbid): Path<String>, Query(q): Query<HashMap<String, String>>| {
                    let s2 = s2.clone();
                    async move {
                        s2.lock().unwrap().push((format!("lookup/{mbid}"), q));
                        if mbid.starts_with("ffffffff") {
                            return (StatusCode::NOT_FOUND, axum::Json(json!({"error": "Not Found"})));
                        }
                        (StatusCode::OK, axum::Json(search_json()["releases"][0].clone()))
                    }
                }),
            )
            .route(
                "/release/{mbid}/front-500",
                head(|Path(mbid): Path<String>| async move {
                    if mbid.starts_with("47b83a2e") { StatusCode::TEMPORARY_REDIRECT } else { StatusCode::NOT_FOUND }
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn search_then_cover_art() {
        let seen: Seen = Default::default();
        let base = spawn(seen.clone()).await;
        let http = reqwest::Client::builder().redirect(reqwest::redirect::Policy::none()).build().unwrap();
        let mb = MusicBrainz { http, base_url: format!("{base}/ws/2"), caa_url: base.clone() };
        let m = mb.release("Daft Punk", "Discovery", None).await.unwrap().expect("match");
        assert_eq!(m.mbid.as_deref(), Some("47b83a2e-4a5f-4a4b-9d3e-1c1a2b3c4d5e"));
        assert_eq!(
            m.poster_url.as_deref(),
            Some(format!("{base}/release/47b83a2e-4a5f-4a4b-9d3e-1c1a2b3c4d5e/front-500").as_str())
        );
        let calls = seen.lock().unwrap().clone();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "search");
        assert_eq!(calls[0].1.get("query").map(String::as_str), Some(r#"release:"Discovery" AND artist:"Daft Punk""#));
        assert_eq!(calls[0].1.get("fmt").map(String::as_str), Some("json"));
        assert_eq!(calls[0].1.get("limit").map(String::as_str), Some("5"));

        // mbid lookup path (404 falls back to search)
        let m2 =
            mb.release("Daft Punk", "Discovery", Some("ffffffff-4a5f-4a4b-9d3e-1c1a2b3c4d5e")).await.unwrap().unwrap();
        assert_eq!(m2.mbid.as_deref(), Some("47b83a2e-4a5f-4a4b-9d3e-1c1a2b3c4d5e"));
        let calls = seen.lock().unwrap().clone();
        assert_eq!(calls[1].0, "lookup/ffffffff-4a5f-4a4b-9d3e-1c1a2b3c4d5e");
        assert!(calls[1].1.get("inc").unwrap().contains("artist-credits+release-groups+genres+tags"));
        assert_eq!(calls[2].0, "search");

        assert!(mb.release("x", "", None).await.unwrap().is_none());
    }
}
