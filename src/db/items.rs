// Copyright 2026 The Ontele Authors
// SPDX-License-Identifier: Apache-2.0

//! `items` queries. Every read joins the caller's watch state and the item's
//! tags in one round trip, so handlers never loop over N+1 lookups.

use crate::model::{Break, Item, Kind, MediaInfo, Metadata, ShowSummary, WatchState};
use chrono::{DateTime, NaiveDate, Utc};
use sqlx::{AssertSqlSafe, FromRow, PgPool, types::Json};
use std::collections::HashMap;

#[derive(FromRow)]
pub struct ItemRow {
    pub id: String,
    pub kind: String,
    pub path: Option<String>,
    pub title: String,
    pub sort_title: Option<String>,
    pub year: Option<i32>,
    pub show: Option<String>,
    pub season: Option<i32>,
    pub episode: Option<i32>,
    pub episode_end: Option<i32>,
    pub air_date: Option<NaiveDate>,
    pub artist: Option<String>,
    pub album_artist: Option<String>,
    pub album: Option<String>,
    pub album_id: Option<String>,
    pub track_no: Option<i32>,
    pub disc_no: Option<i32>,
    pub genre: Option<String>,
    pub subtitle: Option<String>,
    pub description: Option<String>,
    pub channel_id: Option<String>,
    pub channel_name: Option<String>,
    pub start_at: Option<DateTime<Utc>>,
    pub end_at: Option<DateTime<Utc>>,
    pub status: Option<String>,
    pub error: Option<String>,
    pub rule_id: Option<String>,
    pub breaks: Option<Json<Vec<Break>>>,
    pub breaks_state: Option<String>,
    pub info: Json<MediaInfo>,
    pub meta: Json<Metadata>,
    pub auto_tags: Vec<String>,
    pub size_bytes: Option<i64>,
    pub mtime: Option<DateTime<Utc>>,
    pub added: DateTime<Utc>,
    pub updated: DateTime<Utc>,
    pub w_pos: Option<f64>,
    pub w_dur: Option<f64>,
    pub w_done: Option<bool>,
    pub w_updated: Option<DateTime<Utc>>,
    pub tags: Vec<String>,
}

impl From<ItemRow> for Item {
    fn from(r: ItemRow) -> Self {
        let info = r.info.0;
        let watch = match (r.w_pos, r.w_dur, r.w_done, r.w_updated) {
            (Some(pos), Some(dur), Some(done), Some(updated)) => Some(WatchState { pos, dur, done, updated }),
            _ => None,
        };
        let meta = if r.meta.0.is_empty() && r.meta.0.genres.is_empty() { None } else { Some(r.meta.0) };
        Item {
            id: r.id,
            kind: Kind::parse(&r.kind),
            path: r.path,
            title: r.title,
            sort_title: r.sort_title,
            subtitle: r.subtitle,
            year: r.year,
            show: r.show,
            season: r.season,
            episode: r.episode,
            episode_end: r.episode_end,
            air_date: r.air_date,
            artist: r.artist,
            album_artist: r.album_artist,
            album: r.album,
            album_id: r.album_id,
            track_no: r.track_no,
            disc_no: r.disc_no,
            genre: r.genre,
            description: r.description,
            duration: info.duration_sec,
            vcodec: info.vcodec.clone(),
            acodec: info.acodec.clone(),
            width: info.width,
            height: info.height,
            container: if info.container.is_empty() { None } else { Some(info.container.clone()) },
            hdr: info.hdr().map(str::to_string),
            added: r.added,
            watch,
            breaks: r.breaks.map(|b| b.0),
            breaks_state: r.breaks_state,
            status: r.status,
            error: r.error,
            channel: r.channel_name,
            channel_id: r.channel_id,
            rule_id: r.rule_id,
            start: r.start_at,
            end: r.end_at,
            tags: r.tags,
            auto_tags: r.auto_tags,
            meta,
            info: Some(info),
            next_episode: None,
        }
    }
}

/// Base projection. `$1` is always the requesting user id (0 = anonymous →
/// no watch rows match).
pub const BASE: &str = "SELECT i.*, w.pos AS w_pos, w.dur AS w_dur, w.done AS w_done, w.updated AS w_updated,
    COALESCE((SELECT array_agg(t.name ORDER BY t.name) FROM item_tags it JOIN tags t ON t.id = it.tag_id
              WHERE it.item_id = i.id), '{}') AS tags
 FROM items i LEFT JOIN watch w ON w.item_id = i.id AND w.user_id = $1";

fn q(tail: &str) -> AssertSqlSafe<String> {
    AssertSqlSafe(format!("{BASE} {tail}"))
}

pub async fn get(pool: &PgPool, user_id: i64, id: &str) -> sqlx::Result<Option<Item>> {
    let row: Option<ItemRow> = sqlx::query_as(q("WHERE i.id = $2")).bind(user_id).bind(id).fetch_optional(pool).await?;
    Ok(row.map(Into::into))
}

pub async fn get_many(pool: &PgPool, user_id: i64, ids: &[String]) -> sqlx::Result<Vec<Item>> {
    let rows: Vec<ItemRow> = sqlx::query_as(q("WHERE i.id = ANY($2)")).bind(user_id).bind(ids).fetch_all(pool).await?;
    Ok(rows.into_iter().map(Into::into).collect())
}

/// Path + playable status for streaming (no watch join needed).
pub async fn path_of(pool: &PgPool, id: &str) -> sqlx::Result<Option<(String, String, Json<MediaInfo>)>> {
    sqlx::query_as("SELECT kind, path, info FROM items WHERE id = $1 AND path IS NOT NULL")
        .bind(id)
        .fetch_optional(pool)
        .await
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sort {
    Title,
    Added,
    Year,
    Rating,
    Duration,
}

impl Sort {
    pub fn parse(s: &str) -> Sort {
        match s {
            "added" => Sort::Added,
            "year" => Sort::Year,
            "rating" => Sort::Rating,
            "duration" => Sort::Duration,
            _ => Sort::Title,
        }
    }
    fn sql(self) -> &'static str {
        match self {
            Sort::Title => "COALESCE(i.sort_title, lower(i.title)) ASC, i.year ASC",
            Sort::Added => "i.added DESC",
            Sort::Year => "i.year DESC NULLS LAST, lower(i.title) ASC",
            Sort::Rating => "(i.meta->>'rating')::float DESC NULLS LAST, lower(i.title) ASC",
            Sort::Duration => "(i.info->>'durationSec')::float DESC NULLS LAST",
        }
    }
}

#[derive(Debug, Default, Clone)]
pub struct ListFilter {
    pub tag: Option<String>,
    pub genre: Option<String>,
    pub q: Option<String>,
    pub unwatched: bool,
}

/// Movies (or any kind) with optional tag / genre / text filters.
pub async fn list_kind(
    pool: &PgPool,
    user_id: i64,
    kind: Kind,
    sort: Sort,
    f: &ListFilter,
    limit: i64,
) -> sqlx::Result<Vec<Item>> {
    let sql = format!(
        "WHERE i.kind = $2
           AND ($3::text IS NULL OR EXISTS (SELECT 1 FROM item_tags it JOIN tags t ON t.id = it.tag_id
                                           WHERE it.item_id = i.id AND t.name = $3)
                                 OR $3 = ANY(i.auto_tags))
           AND ($4::text IS NULL OR i.meta->'genres' ? $4)
           AND ($5::text IS NULL OR i.title ILIKE $5 OR i.show ILIKE $5)
           AND (NOT $6 OR w.done IS DISTINCT FROM TRUE)
         ORDER BY {} LIMIT $7",
        sort.sql()
    );
    let rows: Vec<ItemRow> = sqlx::query_as(q(&sql))
        .bind(user_id)
        .bind(kind.as_str())
        .bind(f.tag.as_deref().map(crate::db::tags::normalize))
        .bind(f.genre.as_deref())
        .bind(f.q.as_deref().filter(|s| !s.trim().is_empty()).map(crate::db::like_contains))
        .bind(f.unwatched)
        .bind(limit)
        .fetch_all(pool)
        .await?;
    Ok(rows.into_iter().map(Into::into).collect())
}

pub struct Home {
    pub cont: Vec<Item>,
    pub movies: Vec<Item>,
    pub episodes: Vec<Item>,
    pub recordings: Vec<Item>,
}

pub async fn home(pool: &PgPool, user_id: i64, n: i64) -> sqlx::Result<Home> {
    let cont: Vec<ItemRow> =
        sqlx::query_as(q("WHERE w.user_id = $1 AND NOT w.done AND w.pos >= 60 AND i.kind <> 'track'
         ORDER BY w.updated DESC LIMIT $2"))
        .bind(user_id)
        .bind(n)
        .fetch_all(pool)
        .await?;
    let movies: Vec<ItemRow> = sqlx::query_as(q("WHERE i.kind = 'movie' ORDER BY i.added DESC LIMIT $2"))
        .bind(user_id)
        .bind(n)
        .fetch_all(pool)
        .await?;
    // newest episode per show, so a 200-episode import doesn't flood the row
    let episodes: Vec<ItemRow> = sqlx::query_as(q("WHERE i.kind = 'episode' AND i.id IN (
            SELECT DISTINCT ON (lower(show)) id FROM items WHERE kind = 'episode'
            ORDER BY lower(show), added DESC, season DESC, episode DESC)
         ORDER BY i.added DESC LIMIT $2"))
    .bind(user_id)
    .bind(n)
    .fetch_all(pool)
    .await?;
    let recordings: Vec<ItemRow> = sqlx::query_as(q(
        "WHERE i.kind = 'recording' AND i.status = 'done' ORDER BY i.start_at DESC NULLS LAST LIMIT $2",
    ))
    .bind(user_id)
    .bind(n)
    .fetch_all(pool)
    .await?;
    Ok(Home {
        cont: cont.into_iter().map(Into::into).collect(),
        movies: movies.into_iter().map(Into::into).collect(),
        episodes: episodes.into_iter().map(Into::into).collect(),
        recordings: recordings.into_iter().map(Into::into).collect(),
    })
}

/// "Up next": for every show the user has progress in, the first unwatched
/// episode after their latest watched one.
pub async fn up_next(pool: &PgPool, user_id: i64, n: i64) -> sqlx::Result<Vec<Item>> {
    let rows: Vec<ItemRow> = sqlx::query_as(q("WHERE i.kind = 'episode' AND i.id IN (
           SELECT DISTINCT ON (lower(e.show)) e.id
           FROM items e
           JOIN (
             SELECT DISTINCT ON (lower(x.show)) lower(x.show) AS show_key, x.season, x.episode, w2.updated
             FROM watch w2 JOIN items x ON x.id = w2.item_id
             WHERE w2.user_id = $1 AND w2.done AND x.kind = 'episode'
             ORDER BY lower(x.show), w2.updated DESC
           ) last ON lower(e.show) = last.show_key
           LEFT JOIN watch we ON we.item_id = e.id AND we.user_id = $1
           WHERE e.kind = 'episode' AND (e.season, e.episode) > (last.season, last.episode)
             AND (we.done IS DISTINCT FROM TRUE)
           ORDER BY lower(e.show), e.season, e.episode)
         ORDER BY i.added DESC LIMIT $2"))
    .bind(user_id)
    .bind(n)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(Into::into).collect())
}

#[derive(FromRow)]
struct ShowRow {
    show: String,
    episodes: i64,
    seasons: i64,
    poster_id: String,
    year: Option<i32>,
    added: DateTime<Utc>,
    watched: i64,
    meta: Option<Json<Metadata>>,
}

pub async fn shows(pool: &PgPool, user_id: i64) -> sqlx::Result<Vec<ShowSummary>> {
    let rows: Vec<ShowRow> = sqlx::query_as(
        "SELECT (array_agg(i.show ORDER BY i.added DESC))[1] AS show,
                COUNT(*) AS episodes, COUNT(DISTINCT i.season) AS seasons,
                (array_agg(i.id ORDER BY i.season, i.episode))[1] AS poster_id,
                MIN(i.year) AS year, MAX(i.added) AS added,
                COUNT(w.item_id) FILTER (WHERE w.done) AS watched,
                (SELECT s.meta FROM shows s WHERE s.key = MIN(lower(i.show))) AS meta
         FROM items i LEFT JOIN watch w ON w.item_id = i.id AND w.user_id = $1
         WHERE i.kind = 'episode' AND i.show IS NOT NULL
         GROUP BY lower(i.show) ORDER BY lower(i.show)",
    )
    .bind(user_id)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| ShowSummary {
            show: r.show,
            episodes: r.episodes,
            seasons: r.seasons,
            poster_id: r.poster_id,
            meta: r.meta.map(|m| m.0).filter(|m| !m.is_empty()),
            year: r.year,
            added: r.added,
            watched: r.watched,
        })
        .collect())
}

pub async fn show_episodes(pool: &PgPool, user_id: i64, show: &str) -> sqlx::Result<Vec<Item>> {
    let rows: Vec<ItemRow> = sqlx::query_as(q(
        "WHERE i.kind = 'episode' AND lower(i.show) = lower($2) ORDER BY i.season, i.episode, i.title",
    ))
    .bind(user_id)
    .bind(show)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(Into::into).collect())
}

pub async fn next_episode(
    pool: &PgPool,
    user_id: i64,
    show: &str,
    season: i32,
    episode: i32,
) -> sqlx::Result<Option<Item>> {
    let row: Option<ItemRow> =
        sqlx::query_as(q("WHERE i.kind = 'episode' AND lower(i.show) = lower($2) AND (i.season, i.episode) > ($3, $4)
         ORDER BY i.season, i.episode LIMIT 1"))
        .bind(user_id)
        .bind(show)
        .bind(season)
        .bind(episode)
        .fetch_optional(pool)
        .await?;
    Ok(row.map(Into::into))
}

pub async fn get_show_meta(pool: &PgPool, show: &str) -> sqlx::Result<Option<Metadata>> {
    let row: Option<(Json<Metadata>,)> =
        sqlx::query_as("SELECT meta FROM shows WHERE key = lower($1)").bind(show).fetch_optional(pool).await?;
    Ok(row.map(|(m,)| m.0))
}

pub async fn set_show_meta(pool: &PgPool, show: &str, meta: &Metadata) -> sqlx::Result<()> {
    sqlx::query(
        "INSERT INTO shows (key, name, meta, updated) VALUES (lower($1), $1, $2, now())
         ON CONFLICT (key) DO UPDATE SET meta = EXCLUDED.meta, name = EXCLUDED.name, updated = now()",
    )
    .bind(show)
    .bind(Json(meta))
    .execute(pool)
    .await?;
    Ok(())
}

/// Shows that have episodes but no show-level metadata yet (and were not
/// attempted in the last day — a no-match is stamped with `updated`).
pub async fn shows_needing_meta(pool: &PgPool, limit: i64) -> sqlx::Result<Vec<(String, Option<i32>)>> {
    sqlx::query_as(
        "SELECT (array_agg(i.show))[1], MIN(i.year) FROM items i
         WHERE i.kind = 'episode' AND i.show IS NOT NULL
           AND NOT EXISTS (SELECT 1 FROM shows s WHERE s.key = lower(i.show)
                           AND (s.meta ? 'provider' OR s.updated > now() - interval '1 day'))
         GROUP BY lower(i.show) LIMIT $1",
    )
    .bind(limit)
    .fetch_all(pool)
    .await
}

pub struct SearchHits {
    pub movies: Vec<Item>,
    pub episodes: Vec<Item>,
    pub tracks: Vec<Item>,
    pub recordings: Vec<Item>,
}

pub async fn search(pool: &PgPool, user_id: i64, query: &str, per_kind: i64) -> sqlx::Result<SearchHits> {
    let pat = crate::db::like_contains(query);
    // Rank inside each kind so a query that matches hundreds of tracks can't
    // push the one matching movie past the limit.
    let rows: Vec<ItemRow> = sqlx::query_as(q("WHERE i.id IN (
           SELECT id FROM (
             SELECT m.id, row_number() OVER (PARTITION BY m.kind ORDER BY
                      (CASE WHEN lower(m.title) = lower($3) OR lower(m.show) = lower($3) THEN 0 ELSE 1 END),
                      similarity(m.title, $3) DESC, lower(m.title), m.id) AS rn
             FROM items m
             WHERE (m.title ILIKE $2 OR m.show ILIKE $2 OR m.album ILIKE $2 OR m.artist ILIKE $2
                    OR m.album_artist ILIKE $2 OR m.subtitle ILIKE $2)
               AND (m.kind <> 'recording' OR m.status = 'done')
           ) r WHERE r.rn <= $4)
         ORDER BY (CASE WHEN lower(i.title) = lower($3) OR lower(i.show) = lower($3) THEN 0 ELSE 1 END),
                  similarity(i.title, $3) DESC, lower(i.title), i.id"))
    .bind(user_id)
    .bind(&pat)
    .bind(query.trim())
    .bind(per_kind)
    .fetch_all(pool)
    .await?;
    let mut hits = SearchHits { movies: vec![], episodes: vec![], tracks: vec![], recordings: vec![] };
    for r in rows {
        let it: Item = r.into();
        let bucket = match it.kind {
            Some(Kind::Movie) => &mut hits.movies,
            Some(Kind::Episode) => &mut hits.episodes,
            Some(Kind::Track) => &mut hits.tracks,
            Some(Kind::Recording) => &mut hits.recordings,
            None => continue,
        };
        bucket.push(it);
    }
    Ok(hits)
}

pub async fn counts_by_kind(pool: &PgPool) -> sqlx::Result<HashMap<String, i64>> {
    let rows: Vec<(String, i64)> =
        sqlx::query_as("SELECT kind, COUNT(*) FROM items GROUP BY kind").fetch_all(pool).await?;
    Ok(rows.into_iter().collect())
}

pub async fn genres(pool: &PgPool, kind: Kind) -> sqlx::Result<Vec<(String, i64)>> {
    sqlx::query_as(
        "SELECT g, COUNT(*) FROM items i, jsonb_array_elements_text(i.meta->'genres') g
         WHERE i.kind = $1 GROUP BY g ORDER BY COUNT(*) DESC, g",
    )
    .bind(kind.as_str())
    .fetch_all(pool)
    .await
}

// ---- scanner -----------------------------------------------------------------

/// Everything the scanner needs to decide whether a file changed.
#[derive(Debug, Clone)]
pub struct ScanEntry {
    pub id: String,
    pub kind: String,
    pub size_bytes: Option<i64>,
    pub mtime: Option<DateTime<Utc>>,
}

type ScanRow = (String, String, String, Option<i64>, Option<DateTime<Utc>>);

pub async fn scan_index(pool: &PgPool) -> sqlx::Result<HashMap<String, ScanEntry>> {
    let rows: Vec<ScanRow> = sqlx::query_as(
        "SELECT path, id, kind, size_bytes, mtime FROM items WHERE path IS NOT NULL AND kind <> 'recording'",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(path, id, kind, size_bytes, mtime)| (path, ScanEntry { id, kind, size_bytes, mtime }))
        .collect())
}

/// A classified, probed file ready to be written.
#[derive(Debug, Clone, Default)]
pub struct NewItem {
    pub id: String,
    pub kind: Kind,
    pub path: String,
    pub title: String,
    pub sort_title: Option<String>,
    pub year: Option<i32>,
    pub show: Option<String>,
    pub season: Option<i32>,
    pub episode: Option<i32>,
    pub episode_end: Option<i32>,
    pub air_date: Option<NaiveDate>,
    pub artist: Option<String>,
    pub album_artist: Option<String>,
    pub album: Option<String>,
    pub album_id: Option<String>,
    pub track_no: Option<i32>,
    pub disc_no: Option<i32>,
    pub genre: Option<String>,
    pub description: Option<String>,
    pub info: MediaInfo,
    pub meta: Option<Metadata>,
    pub auto_tags: Vec<String>,
    pub size_bytes: i64,
    pub mtime: Option<DateTime<Utc>>,
}

/// Insert or refresh a scanned file. Returns true when the row was new.
/// `meta` (NFO / embedded tags at scan time) is overlaid on whatever the row
/// already holds, so a re-scan of a changed file never wipes provider
/// metadata (e.g. a track's `{mbid}` no longer clobbers its MusicBrainz
/// enrichment).
pub async fn upsert_scanned(pool: &PgPool, n: &NewItem) -> sqlx::Result<bool> {
    let (inserted,): (bool,) = sqlx::query_as(
        "INSERT INTO items (id, kind, path, title, sort_title, year, show, season, episode, episode_end, air_date,
                            artist, album_artist, album, album_id, track_no, disc_no, genre, description,
                            info, meta, auto_tags, size_bytes, mtime)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17, $18, $19,
                 $20, COALESCE($21, '{}'::jsonb), $22, $23, $24)
         ON CONFLICT (id) DO UPDATE SET
            kind = EXCLUDED.kind, path = EXCLUDED.path, title = EXCLUDED.title, sort_title = EXCLUDED.sort_title,
            year = EXCLUDED.year, show = EXCLUDED.show, season = EXCLUDED.season, episode = EXCLUDED.episode,
            episode_end = EXCLUDED.episode_end, air_date = EXCLUDED.air_date, artist = EXCLUDED.artist,
            album_artist = EXCLUDED.album_artist, album = EXCLUDED.album, album_id = EXCLUDED.album_id,
            track_no = EXCLUDED.track_no, disc_no = EXCLUDED.disc_no, genre = EXCLUDED.genre,
            description = COALESCE(EXCLUDED.description, items.description),
            info = EXCLUDED.info, meta = COALESCE(items.meta || $21, items.meta), auto_tags = EXCLUDED.auto_tags,
            size_bytes = EXCLUDED.size_bytes, mtime = EXCLUDED.mtime, updated = now()
         RETURNING (xmax = 0) AS inserted",
    )
    .bind(&n.id)
    .bind(n.kind.as_str())
    .bind(&n.path)
    .bind(&n.title)
    .bind(&n.sort_title)
    .bind(n.year)
    .bind(&n.show)
    .bind(n.season)
    .bind(n.episode)
    .bind(n.episode_end)
    .bind(n.air_date)
    .bind(&n.artist)
    .bind(&n.album_artist)
    .bind(&n.album)
    .bind(&n.album_id)
    .bind(n.track_no)
    .bind(n.disc_no)
    .bind(&n.genre)
    .bind(&n.description)
    .bind(Json(&n.info))
    .bind(scan_meta_patch(n.meta.as_ref()))
    .bind(&n.auto_tags)
    .bind(n.size_bytes)
    .bind(n.mtime)
    .fetch_one(pool)
    .await?;
    Ok(inserted)
}

/// Scan-time metadata as a jsonb patch: empty `genres` / `cast` are dropped
/// so overlaying it (`meta || patch`) keeps what a provider already filled in.
fn scan_meta_patch(meta: Option<&Metadata>) -> Option<serde_json::Value> {
    let mut v = serde_json::to_value(meta?).ok()?;
    if let Some(o) = v.as_object_mut() {
        o.retain(|_, x| !x.as_array().is_some_and(|a| a.is_empty()));
    }
    Some(v)
}

pub async fn delete_ids(pool: &PgPool, ids: &[String]) -> sqlx::Result<u64> {
    if ids.is_empty() {
        return Ok(0);
    }
    let res = sqlx::query("DELETE FROM items WHERE id = ANY($1)").bind(ids).execute(pool).await?;
    Ok(res.rows_affected())
}

/// Delete one item; returns its path so the caller can remove the file.
pub async fn delete(pool: &PgPool, id: &str) -> sqlx::Result<Option<String>> {
    let row: Option<(Option<String>,)> =
        sqlx::query_as("DELETE FROM items WHERE id = $1 RETURNING path").bind(id).fetch_optional(pool).await?;
    Ok(row.and_then(|(p,)| p))
}

pub async fn set_info(pool: &PgPool, id: &str, info: &MediaInfo) -> sqlx::Result<()> {
    sqlx::query("UPDATE items SET info = $2, updated = now() WHERE id = $1")
        .bind(id)
        .bind(Json(info))
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn set_meta(pool: &PgPool, id: &str, meta: &Metadata) -> sqlx::Result<()> {
    sqlx::query("UPDATE items SET meta = $2, updated = now() WHERE id = $1")
        .bind(id)
        .bind(Json(meta))
        .execute(pool)
        .await?;
    Ok(())
}

/// Manual title/year correction (also clears provider metadata so the next
/// enrichment pass re-matches).
pub async fn set_identity(
    pool: &PgPool,
    id: &str,
    title: Option<&str>,
    year: Option<i32>,
    tmdb_id: Option<i64>,
) -> sqlx::Result<()> {
    sqlx::query(
        "UPDATE items SET title = COALESCE($2, title), year = COALESCE($3, year),
                meta = CASE WHEN $4::bigint IS NULL THEN '{}'::jsonb ELSE jsonb_build_object('tmdbId', $4::bigint) END,
                updated = now()
         WHERE id = $1",
    )
    .bind(id)
    .bind(title)
    .bind(year)
    .bind(tmdb_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Items with no provider metadata yet (and not tried in the last day).
pub async fn needing_meta(pool: &PgPool, kinds: &[&str], limit: i64) -> sqlx::Result<Vec<Item>> {
    let kinds: Vec<String> = kinds.iter().map(|k| k.to_string()).collect();
    let rows: Vec<ItemRow> = sqlx::query_as(q("WHERE i.kind = ANY($2) AND NOT (i.meta ? 'provider')
           AND (NOT (i.meta ? 'updated') OR (i.meta->>'updated')::timestamptz < now() - interval '1 day')
           AND (i.kind <> 'recording' OR i.status = 'done')
         ORDER BY i.added DESC LIMIT $3"))
    .bind(0i64)
    .bind(&kinds)
    .bind(limit)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(Into::into).collect())
}

// ---- recordings ---------------------------------------------------------------

pub struct NewRecording {
    pub id: String,
    pub title: String,
    pub subtitle: Option<String>,
    pub description: Option<String>,
    pub channel_id: String,
    pub channel_name: Option<String>,
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
    pub rule_id: Option<String>,
    pub season: Option<i32>,
    pub episode: Option<i32>,
}

pub async fn insert_recording(pool: &PgPool, r: &NewRecording) -> sqlx::Result<()> {
    sqlx::query(
        "INSERT INTO items (id, kind, title, subtitle, description, channel_id, channel_name, start_at, end_at,
                            status, rule_id, season, episode)
         VALUES ($1, 'recording', $2, $3, $4, $5, $6, $7, $8, 'scheduled', $9, $10, $11)",
    )
    .bind(&r.id)
    .bind(&r.title)
    .bind(&r.subtitle)
    .bind(&r.description)
    .bind(&r.channel_id)
    .bind(&r.channel_name)
    .bind(r.start)
    .bind(r.end)
    .bind(&r.rule_id)
    .bind(r.season)
    .bind(r.episode)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn recordings(pool: &PgPool, user_id: i64) -> sqlx::Result<Vec<Item>> {
    let rows: Vec<ItemRow> = sqlx::query_as(q("WHERE i.kind = 'recording' ORDER BY i.start_at DESC NULLS LAST"))
        .bind(user_id)
        .fetch_all(pool)
        .await?;
    Ok(rows.into_iter().map(Into::into).collect())
}

/// Canonical `rule|channel|start` dedupe key for a recording. The timestamp
/// is truncated to whole microseconds first: Postgres stores timestamptz at
/// microsecond precision (and sqlx encodes by truncation), so a guide airing
/// carrying nanoseconds must map to the same key before and after the round
/// trip through the database — otherwise every scheduler tick re-schedules
/// the same airing.
pub fn recording_key(rule_id: &str, channel_id: &str, start: DateTime<Utc>) -> String {
    use chrono::Timelike;
    let micros = start.with_nanosecond(start.nanosecond() / 1_000 * 1_000).unwrap_or(start);
    format!("{}|{}|{}", rule_id, channel_id, micros.to_rfc3339())
}

/// Dedupe keys of all recordings (see [`recording_key`]).
type KeyRow = (Option<String>, Option<String>, Option<DateTime<Utc>>);

pub async fn recording_keys(pool: &PgPool) -> sqlx::Result<std::collections::HashSet<String>> {
    let rows: Vec<KeyRow> = sqlx::query_as("SELECT rule_id, channel_id, start_at FROM items WHERE kind = 'recording'")
        .fetch_all(pool)
        .await?;
    Ok(rows
        .into_iter()
        .map(|(r, c, s)| match s {
            Some(t) => recording_key(r.as_deref().unwrap_or_default(), c.as_deref().unwrap_or_default(), t),
            None => format!("{}|{}|", r.unwrap_or_default(), c.unwrap_or_default()),
        })
        .collect())
}

/// Scheduled recordings whose capture window (start-pre .. end+post) contains `now`.
pub async fn recordings_due(
    pool: &PgPool,
    now: DateTime<Utc>,
    pre_secs: i64,
    post_secs: i64,
) -> sqlx::Result<Vec<Item>> {
    let rows: Vec<ItemRow> = sqlx::query_as(q("WHERE i.kind = 'recording' AND i.status = 'scheduled'
           AND i.start_at - make_interval(secs => $3) <= $2 AND $2 < i.end_at + make_interval(secs => $4)
         ORDER BY i.start_at"))
    .bind(0i64)
    .bind(now)
    .bind(pre_secs as f64)
    .bind(post_secs as f64)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(Into::into).collect())
}

/// Mark scheduled recordings whose window has fully passed as missed.
pub async fn fail_missed(pool: &PgPool, now: DateTime<Utc>, post_secs: i64) -> sqlx::Result<u64> {
    let res = sqlx::query(
        "UPDATE items SET status = 'failed', error = 'missed airing (server offline?)', updated = now()
         WHERE kind = 'recording' AND status = 'scheduled' AND end_at + make_interval(secs => $2) <= $1",
    )
    .bind(now)
    .bind(post_secs as f64)
    .execute(pool)
    .await?;
    Ok(res.rows_affected())
}

pub async fn set_status(pool: &PgPool, id: &str, status: &str, error: Option<&str>) -> sqlx::Result<()> {
    sqlx::query("UPDATE items SET status = $2, error = $3, updated = now() WHERE id = $1")
        .bind(id)
        .bind(status)
        .bind(error)
        .execute(pool)
        .await?;
    Ok(())
}

/// Cancel only if still scheduled (in-flight captures are stopped by the engine).
pub async fn cancel_scheduled(pool: &PgPool, id: &str) -> sqlx::Result<bool> {
    let res =
        sqlx::query("UPDATE items SET status = 'canceled', updated = now() WHERE id = $1 AND status = 'scheduled'")
            .bind(id)
            .execute(pool)
            .await?;
    Ok(res.rows_affected() > 0)
}

pub async fn set_recording_started(pool: &PgPool, id: &str, path: &str) -> sqlx::Result<()> {
    sqlx::query("UPDATE items SET status = 'recording', path = $2, error = NULL, updated = now() WHERE id = $1")
        .bind(id)
        .bind(path)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn set_recording_done(
    pool: &PgPool,
    id: &str,
    path: &str,
    info: &MediaInfo,
    breaks_state: Option<&str>,
) -> sqlx::Result<()> {
    sqlx::query(
        "UPDATE items SET status = 'done', path = $2, info = $3, breaks_state = $4, size_bytes = $5, updated = now()
         WHERE id = $1",
    )
    .bind(id)
    .bind(path)
    .bind(Json(info))
    .bind(breaks_state)
    .bind(info.size_bytes)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn set_breaks(pool: &PgPool, id: &str, breaks: Option<&[Break]>, state: &str) -> sqlx::Result<()> {
    sqlx::query("UPDATE items SET breaks = $2, breaks_state = $3, updated = now() WHERE id = $1")
        .bind(id)
        .bind(breaks.map(Json))
        .bind(state)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn set_cut(pool: &PgPool, id: &str, path: &str, info: &MediaInfo) -> sqlx::Result<()> {
    sqlx::query(
        "UPDATE items SET path = $2, info = $3, breaks = NULL, breaks_state = 'cut', size_bytes = $4, updated = now()
         WHERE id = $1",
    )
    .bind(id)
    .bind(path)
    .bind(Json(info))
    .bind(info.size_bytes)
    .execute(pool)
    .await?;
    Ok(())
}

/// Done recordings grouped by rule, oldest first — for keep-N pruning.
/// (id, path, start) of a finished recording.
pub type DoneRec = (String, Option<String>, DateTime<Utc>);

pub async fn done_by_rule(pool: &PgPool) -> sqlx::Result<HashMap<String, Vec<DoneRec>>> {
    let rows: Vec<(String, String, Option<String>, DateTime<Utc>)> = sqlx::query_as(
        "SELECT rule_id, id, path, COALESCE(start_at, added) FROM items
         WHERE kind = 'recording' AND status = 'done' AND rule_id IS NOT NULL
         ORDER BY rule_id, COALESCE(start_at, added)",
    )
    .fetch_all(pool)
    .await?;
    let mut out: HashMap<String, Vec<DoneRec>> = HashMap::new();
    for (rule, id, path, start) in rows {
        out.entry(rule).or_default().push((id, path, start));
    }
    Ok(out)
}

/// Recordings stuck in `recording` from a previous process (crash/restart).
pub async fn reset_stale_recording(pool: &PgPool) -> sqlx::Result<u64> {
    let res = sqlx::query(
        "UPDATE items SET status = 'failed', error = 'interrupted by restart', updated = now()
         WHERE kind = 'recording' AND status = 'recording'",
    )
    .execute(pool)
    .await?;
    Ok(res.rows_affected())
}

/// Point a recording at a new file (e.g. after a chapter remux changed the
/// container) and refresh its probe info.
pub async fn set_path_info(pool: &PgPool, id: &str, path: &str, info: &MediaInfo) -> sqlx::Result<()> {
    sqlx::query("UPDATE items SET path = $2, info = $3, size_bytes = $4, updated = now() WHERE id = $1")
        .bind(id)
        .bind(path)
        .bind(Json(info))
        .bind(info.size_bytes)
        .execute(pool)
        .await?;
    Ok(())
}

/// Set the display title only (used when an episode scanned without a name
/// gets one from the metadata provider).
pub async fn set_title(pool: &PgPool, id: &str, title: &str) -> sqlx::Result<()> {
    sqlx::query("UPDATE items SET title = $2, updated = now() WHERE id = $1")
        .bind(id)
        .bind(title)
        .execute(pool)
        .await?;
    Ok(())
}

/// Raw `meta` column (unlike `Item.meta`, not collapsed to `None` when it
/// holds only ids such as a manual `{tmdbId}` fix or a track's `mbid`).
pub async fn get_meta(pool: &PgPool, id: &str) -> sqlx::Result<Option<Metadata>> {
    let row: Option<(Json<Metadata>,)> =
        sqlx::query_as("SELECT meta FROM items WHERE id = $1").bind(id).fetch_optional(pool).await?;
    Ok(row.map(|(m,)| m.0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recording_key_survives_postgres_microsecond_truncation() {
        use chrono::TimeZone;
        let nanos = Utc.with_ymd_and_hms(2026, 8, 23, 20, 0, 0).unwrap() + chrono::Duration::nanoseconds(123_456_789);
        let micros = Utc.with_ymd_and_hms(2026, 8, 23, 20, 0, 0).unwrap() + chrono::Duration::microseconds(123_456);
        // sqlx encodes timestamptz by truncating to microseconds; the key of
        // the in-memory airing must equal the key of the DB round trip
        assert_eq!(recording_key("r1", "7.1", nanos), recording_key("r1", "7.1", micros));
        assert_ne!(recording_key("r1", "7.1", nanos), recording_key("r2", "7.1", nanos));
        assert_ne!(recording_key("r1", "7.1", nanos), recording_key("r1", "4.1", nanos));
        assert!(recording_key("", "7.1", micros).starts_with("|7.1|2026-08-23T20:00:00.123456"));
    }
}
