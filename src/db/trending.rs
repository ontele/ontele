// Copyright 2026 The Ontele Authors
// SPDX-License-Identifier: Apache-2.0

//! Playback log (`play_log`) and the Trending aggregates over it.
//! Writes are fire-and-forget from the hot paths: a failed row never fails
//! playback or a progress save.
//!
//! Known trade-offs, on purpose: the log trusts the same client reports as
//! watch state itself (a hostile client can inflate its own numbers — this
//! is a household stats page, not billing), and rows CASCADE away with their
//! item, so re-imported files (item ids derive from paths) restart history.
//! Display names never expose a full email/OIDC subject to other viewers —
//! blank names fall back to the email local part, then `viewer-<id>`.

use serde::Serialize;
use sqlx::PgPool;

/// Longest credit a single progress save may add, in seconds. Progress lands
/// every ~5s during playback; anything larger is a seek, not watching.
pub const MAX_DELTA: f64 = 120.0;

/// Credit watched seconds to today's (user, item) row.
pub async fn accumulate(pool: &PgPool, user_id: i64, item_id: &str, seconds: f64) -> sqlx::Result<()> {
    let seconds = seconds.clamp(0.0, MAX_DELTA);
    if seconds <= 0.0 {
        return Ok(());
    }
    sqlx::query(
        "INSERT INTO play_log (user_id, item_id, day, seconds) VALUES ($1, $2, CURRENT_DATE, $3)
         ON CONFLICT (user_id, item_id, day) DO UPDATE SET seconds = play_log.seconds + EXCLUDED.seconds",
    )
    .bind(user_id)
    .bind(item_id)
    .bind(seconds)
    .execute(pool)
    .await?;
    Ok(())
}

/// Count a started play for today. Idempotent per (user, item, day): the
/// player restarts its session on every seek / quality / audio change, so
/// "views" means "user-days on which this was played", not session starts.
pub async fn bump_view(pool: &PgPool, user_id: i64, item_id: &str) -> sqlx::Result<()> {
    sqlx::query(
        "INSERT INTO play_log (user_id, item_id, day, views) VALUES ($1, $2, CURRENT_DATE, 1)
         ON CONFLICT (user_id, item_id, day) DO UPDATE SET views = GREATEST(play_log.views, 1)",
    )
    .bind(user_id)
    .bind(item_id)
    .execute(pool)
    .await?;
    Ok(())
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TrendingItem {
    pub item_id: String,
    pub title: String,
    pub kind: String,
    pub show: Option<String>,
    pub year: Option<i32>,
    pub seconds: f64,
    pub views: i64,
    pub users: i64,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TrendingUser {
    pub user_id: i64,
    pub name: String,
    pub seconds: f64,
    pub views: i64,
    pub items: i64,
}

type ItemRow = (String, String, String, Option<String>, Option<i32>, f64, i64, i64);

/// `days = None` → all time.
pub async fn top_items(pool: &PgPool, days: Option<i32>, limit: i64) -> sqlx::Result<Vec<TrendingItem>> {
    let rows: Vec<ItemRow> = sqlx::query_as(
        "SELECT p.item_id, i.title, i.kind, i.show, i.year,
                sum(p.seconds), sum(p.views)::bigint, count(DISTINCT p.user_id)
         FROM play_log p JOIN items i ON i.id = p.item_id
         WHERE $1::int IS NULL OR p.day > CURRENT_DATE - $1::int
         GROUP BY p.item_id, i.title, i.kind, i.show, i.year
         ORDER BY sum(p.seconds) DESC, sum(p.views) DESC
         LIMIT $2",
    )
    .bind(days)
    .bind(limit)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(item_id, title, kind, show, year, seconds, views, users)| TrendingItem {
            item_id,
            title,
            kind,
            show,
            year,
            seconds,
            views,
            users,
        })
        .collect())
}

/// `days = None` → all time.
pub async fn top_users(pool: &PgPool, days: Option<i32>, limit: i64) -> sqlx::Result<Vec<TrendingUser>> {
    let rows: Vec<(i64, String, f64, i64, i64)> = sqlx::query_as(
        "SELECT p.user_id, coalesce(nullif(u.name, ''), nullif(split_part(coalesce(u.email, ''), '@', 1), ''), 'viewer-' || p.user_id), sum(p.seconds), sum(p.views)::bigint,
                count(DISTINCT p.item_id)
         FROM play_log p JOIN users u ON u.id = p.user_id
         WHERE $1::int IS NULL OR p.day > CURRENT_DATE - $1::int
         GROUP BY p.user_id, u.name, u.email, u.subject
         ORDER BY sum(p.seconds) DESC
         LIMIT $2",
    )
    .bind(days)
    .bind(limit)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(user_id, name, seconds, views, items)| TrendingUser { user_id, name, seconds, views, items })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn seed(pool: &PgPool) -> (i64, i64) {
        let (a,): (i64,) =
            sqlx::query_as("INSERT INTO users (subject, email, name) VALUES ('a', 'a@x', 'Alice') RETURNING id")
                .fetch_one(pool)
                .await
                .unwrap();
        let (b,): (i64,) =
            sqlx::query_as("INSERT INTO users (subject, email, name) VALUES ('b', 'b@x', '') RETURNING id")
                .fetch_one(pool)
                .await
                .unwrap();
        for (id, title, kind) in [("m1", "Heat", "movie"), ("m2", "Ronin", "movie"), ("e1", "Pilot", "episode")] {
            sqlx::query("INSERT INTO items (id, kind, title, path) VALUES ($1, $2, $3, $4)")
                .bind(id)
                .bind(kind)
                .bind(title)
                .bind(format!("/m/{id}.mkv"))
                .execute(pool)
                .await
                .unwrap();
        }
        (a, b)
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn accumulate_clamps_and_sums(pool: PgPool) {
        let (a, _) = seed(&pool).await;
        accumulate(&pool, a, "m1", 5.0).await.unwrap();
        accumulate(&pool, a, "m1", 6.0).await.unwrap();
        accumulate(&pool, a, "m1", 9999.0).await.unwrap(); // a seek: clamped to MAX_DELTA
        accumulate(&pool, a, "m1", -50.0).await.unwrap(); // rewind: ignored
        bump_view(&pool, a, "m1").await.unwrap();
        bump_view(&pool, a, "m1").await.unwrap(); // seek-restart: still one view today
        let (secs, views): (f64, i32) =
            sqlx::query_as("SELECT seconds, views FROM play_log WHERE user_id = $1 AND item_id = 'm1'")
                .bind(a)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(secs, 11.0 + MAX_DELTA);
        assert_eq!(views, 1);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn windows_rank_and_fall_off(pool: PgPool) {
        let (a, b) = seed(&pool).await;
        // today: Alice watches m1 a lot, Bob watches e1 a little
        accumulate(&pool, a, "m1", 100.0).await.unwrap();
        bump_view(&pool, a, "m1").await.unwrap();
        accumulate(&pool, b, "e1", 40.0).await.unwrap();
        // long ago: m2 was huge
        sqlx::query(
            "INSERT INTO play_log (user_id, item_id, day, seconds, views)
             VALUES ($1, 'm2', CURRENT_DATE - 400, 5000, 12)",
        )
        .bind(b)
        .execute(&pool)
        .await
        .unwrap();

        let day = top_items(&pool, Some(1), 10).await.unwrap();
        assert_eq!(day.iter().map(|t| t.item_id.as_str()).collect::<Vec<_>>(), ["m1", "e1"]);
        assert_eq!(day[0].users, 1);

        let year = top_items(&pool, Some(365), 10).await.unwrap();
        assert!(!year.iter().any(|t| t.item_id == "m2"), "400-day-old rows are outside the year window");

        let all = top_items(&pool, None, 10).await.unwrap();
        assert_eq!(all[0].item_id, "m2", "all time includes ancient plays");
        assert_eq!(all[0].views, 12);

        let users = top_users(&pool, None, 10).await.unwrap();
        assert_eq!(users[0].name, "b", "5040s all-time beats Alice; blank name falls back to the email local part");
        assert_eq!(users[1].name, "Alice");
        let today = top_users(&pool, Some(1), 10).await.unwrap();
        assert_eq!(today[0].name, "Alice", "the day window drops Bob's ancient marathon");
    }
}
