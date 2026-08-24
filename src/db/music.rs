// Copyright 2026 The Ontele Authors
// SPDX-License-Identifier: Apache-2.0

//! Music aggregates (artists/albums) are derived from `items` rows of kind
//! `track`; the `albums` table only stores provider metadata per album.

use crate::db::items::{BASE, ItemRow};
use crate::db::like_contains;
use crate::model::{AlbumSummary, ArtistSummary, Item, Metadata};
use chrono::{DateTime, Utc};
use sqlx::{AssertSqlSafe, FromRow, PgPool, types::Json};

#[derive(FromRow)]
struct AlbumRow {
    id: String,
    artist: String,
    title: String,
    year: Option<i32>,
    tracks: i64,
    duration: f64,
    art_id: String,
    added: DateTime<Utc>,
    meta: Option<Json<Metadata>>,
}

impl From<AlbumRow> for AlbumSummary {
    fn from(r: AlbumRow) -> Self {
        AlbumSummary {
            id: r.id,
            artist: r.artist,
            title: r.title,
            year: r.year,
            tracks: r.tracks,
            duration: r.duration,
            art_id: r.art_id,
            meta: r.meta.map(|m| m.0).filter(|m| !m.is_empty()),
            added: r.added,
        }
    }
}

const ALBUM_SELECT: &str = "SELECT i.album_id AS id,
        COALESCE(MIN(i.album_artist), MIN(i.artist), 'Unknown Artist') AS artist,
        COALESCE(MIN(i.album), 'Unknown Album') AS title,
        MIN(i.year) AS year, COUNT(*) AS tracks,
        COALESCE(SUM((i.info->>'durationSec')::float), 0) AS duration,
        (array_agg(i.id ORDER BY i.disc_no NULLS LAST, i.track_no NULLS LAST))[1] AS art_id,
        MAX(i.added) AS added,
        (SELECT a.meta FROM albums a WHERE a.id = i.album_id) AS meta
    FROM items i WHERE i.kind = 'track' AND i.album_id IS NOT NULL";

pub async fn albums(
    pool: &PgPool,
    artist: Option<&str>,
    q: Option<&str>,
    sort: &str,
    limit: i64,
) -> sqlx::Result<Vec<AlbumSummary>> {
    let order = match sort {
        "added" => "added DESC",
        "year" => "year DESC NULLS LAST, lower(title)",
        "artist" => "lower(artist), year NULLS LAST, lower(title)",
        _ => "lower(title)",
    };
    let sql = format!(
        "SELECT * FROM ({ALBUM_SELECT}
           AND ($1::text IS NULL OR lower(COALESCE(i.album_artist, i.artist)) = lower($1))
           AND ($2::text IS NULL OR i.album ILIKE $2 OR i.album_artist ILIKE $2)
         GROUP BY i.album_id) x ORDER BY {order} LIMIT $3"
    );
    let rows: Vec<AlbumRow> = sqlx::query_as(AssertSqlSafe(sql))
        .bind(artist)
        .bind(q.filter(|s| !s.trim().is_empty()).map(like_contains))
        .bind(limit)
        .fetch_all(pool)
        .await?;
    Ok(rows.into_iter().map(Into::into).collect())
}

pub async fn album(pool: &PgPool, id: &str) -> sqlx::Result<Option<AlbumSummary>> {
    let sql = format!("{ALBUM_SELECT} AND i.album_id = $1 GROUP BY i.album_id");
    let row: Option<AlbumRow> = sqlx::query_as(AssertSqlSafe(sql)).bind(id).fetch_optional(pool).await?;
    Ok(row.map(Into::into))
}

pub async fn album_tracks(pool: &PgPool, user_id: i64, album_id: &str) -> sqlx::Result<Vec<Item>> {
    let rows: Vec<ItemRow> = sqlx::query_as(AssertSqlSafe(format!(
        "{BASE} WHERE i.kind = 'track' AND i.album_id = $2
         ORDER BY i.disc_no NULLS FIRST, i.track_no NULLS LAST, lower(i.title)"
    )))
    .bind(user_id)
    .bind(album_id)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(Into::into).collect())
}

pub async fn tracks(pool: &PgPool, user_id: i64, q: Option<&str>, limit: i64) -> sqlx::Result<Vec<Item>> {
    let rows: Vec<ItemRow> = sqlx::query_as(AssertSqlSafe(format!(
        "{BASE} WHERE i.kind = 'track'
           AND ($2::text IS NULL OR i.title ILIKE $2 OR i.artist ILIKE $2 OR i.album ILIKE $2)
         ORDER BY lower(i.artist), lower(i.album), i.disc_no NULLS FIRST, i.track_no NULLS LAST LIMIT $3"
    )))
    .bind(user_id)
    .bind(q.filter(|s| !s.trim().is_empty()).map(like_contains))
    .bind(limit)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(Into::into).collect())
}

pub async fn artists(pool: &PgPool, q: Option<&str>) -> sqlx::Result<Vec<ArtistSummary>> {
    let rows: Vec<(String, i64, i64, String)> = sqlx::query_as(
        "SELECT MIN(COALESCE(i.album_artist, i.artist, 'Unknown Artist')) AS name,
                COUNT(DISTINCT i.album_id) AS albums, COUNT(*) AS tracks,
                (array_agg(i.id ORDER BY i.album, i.disc_no NULLS LAST, i.track_no NULLS LAST))[1] AS art_id
         FROM items i WHERE i.kind = 'track'
           AND ($1::text IS NULL OR COALESCE(i.album_artist, i.artist) ILIKE $1)
         GROUP BY lower(COALESCE(i.album_artist, i.artist, 'Unknown Artist'))
         ORDER BY lower(COALESCE(i.album_artist, i.artist, 'Unknown Artist'))",
    )
    .bind(q.filter(|s| !s.trim().is_empty()).map(like_contains))
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(|(name, albums, tracks, art_id)| ArtistSummary { name, albums, tracks, art_id }).collect())
}

pub async fn set_album_meta(
    pool: &PgPool,
    id: &str,
    artist: &str,
    title: &str,
    year: Option<i32>,
    meta: &Metadata,
) -> sqlx::Result<()> {
    sqlx::query(
        "INSERT INTO albums (id, artist, title, year, meta, updated) VALUES ($1, $2, $3, $4, $5, now())
         ON CONFLICT (id) DO UPDATE SET meta = EXCLUDED.meta, artist = EXCLUDED.artist, title = EXCLUDED.title,
            year = COALESCE(EXCLUDED.year, albums.year), updated = now()",
    )
    .bind(id)
    .bind(artist)
    .bind(title)
    .bind(year)
    .bind(Json(meta))
    .execute(pool)
    .await?;
    Ok(())
}

/// Albums with tracks but no provider metadata (and not attempted in the last day).
pub async fn albums_needing_meta(pool: &PgPool, limit: i64) -> sqlx::Result<Vec<AlbumSummary>> {
    let sql = format!(
        "SELECT * FROM ({ALBUM_SELECT}
           AND NOT EXISTS (SELECT 1 FROM albums a WHERE a.id = i.album_id
                           AND (a.meta ? 'provider' OR a.updated > now() - interval '1 day'))
         GROUP BY i.album_id) x ORDER BY added DESC LIMIT $1"
    );
    let rows: Vec<AlbumRow> = sqlx::query_as(AssertSqlSafe(sql)).bind(limit).fetch_all(pool).await?;
    Ok(rows.into_iter().map(Into::into).collect())
}

pub async fn recent_albums(pool: &PgPool, limit: i64) -> sqlx::Result<Vec<AlbumSummary>> {
    albums(pool, None, None, "added", limit).await
}
