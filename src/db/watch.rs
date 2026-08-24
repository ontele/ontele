// Copyright 2026 The Ontele Authors
// SPDX-License-Identifier: Apache-2.0

use crate::model::WatchState;
use sqlx::PgPool;

/// Upsert the watch position; returns the position it replaced (`None` on
/// the first save) so callers can credit the watched delta to the play log.
pub async fn set(
    pool: &PgPool,
    user_id: i64,
    item_id: &str,
    pos: f64,
    dur: f64,
    done: bool,
) -> sqlx::Result<Option<f64>> {
    // `FOR UPDATE` first: under READ COMMITTED it waits for a concurrent
    // saver and re-reads the committed row, so `old` is the position this
    // statement actually replaces (a bare CTE snapshot could be stale and
    // double-credit the play log). Two concurrent *first* saves both take the
    // insert arm; ON CONFLICT serializes them and both report None (delta 0).
    let (old,): (Option<f64>,) = sqlx::query_as(
        "WITH locked AS (SELECT pos FROM watch WHERE user_id = $1 AND item_id = $2 FOR UPDATE),
         up AS (UPDATE watch SET pos = $3, dur = $4, done = $5, updated = now()
                WHERE user_id = $1 AND item_id = $2 AND EXISTS (SELECT 1 FROM locked)),
         ins AS (INSERT INTO watch (user_id, item_id, pos, dur, done, updated)
                 SELECT $1, $2, $3, $4, $5, now() WHERE NOT EXISTS (SELECT 1 FROM locked)
                 ON CONFLICT (user_id, item_id) DO UPDATE SET pos = EXCLUDED.pos, dur = EXCLUDED.dur,
                    done = EXCLUDED.done, updated = now())
         SELECT (SELECT pos FROM locked)",
    )
    .bind(user_id)
    .bind(item_id)
    .bind(pos)
    .bind(dur)
    .bind(done)
    .fetch_one(pool)
    .await
    .map_err(|e| match e {
        // unknown item id → 404 rather than a 500 "database error"
        sqlx::Error::Database(d) if d.is_foreign_key_violation() => sqlx::Error::RowNotFound,
        e => e,
    })?;
    Ok(old)
}

pub async fn get(pool: &PgPool, user_id: i64, item_id: &str) -> sqlx::Result<Option<WatchState>> {
    let row: Option<(f64, f64, bool, chrono::DateTime<chrono::Utc>)> =
        sqlx::query_as("SELECT pos, dur, done, updated FROM watch WHERE user_id = $1 AND item_id = $2")
            .bind(user_id)
            .bind(item_id)
            .fetch_optional(pool)
            .await?;
    Ok(row.map(|(pos, dur, done, updated)| WatchState { pos, dur, done, updated }))
}

pub async fn clear(pool: &PgPool, user_id: i64, item_id: &str) -> sqlx::Result<()> {
    sqlx::query("DELETE FROM watch WHERE user_id = $1 AND item_id = $2")
        .bind(user_id)
        .bind(item_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Item ids every user has finished (used by auto-delete-watched: a recording
/// is reaped only when *all* users who started it are done).
pub async fn fully_watched_recordings(pool: &PgPool) -> sqlx::Result<Vec<(String, Option<String>)>> {
    sqlx::query_as(
        "SELECT i.id, i.path FROM items i
         WHERE i.kind = 'recording' AND i.status = 'done'
           AND EXISTS (SELECT 1 FROM watch w WHERE w.item_id = i.id)
           AND NOT EXISTS (SELECT 1 FROM watch w WHERE w.item_id = i.id AND NOT w.done)",
    )
    .fetch_all(pool)
    .await
}
