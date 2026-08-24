// Copyright 2026 The Ontele Authors
// SPDX-License-Identifier: Apache-2.0

use crate::model::ActivityEvent;
use chrono::{DateTime, Utc};
use sqlx::{FromRow, PgPool};

pub async fn insert(
    pool: &PgPool,
    user_id: Option<i64>,
    kind: &str,
    item_id: Option<&str>,
    detail: &serde_json::Value,
) -> sqlx::Result<()> {
    sqlx::query("INSERT INTO activity (user_id, kind, item_id, detail) VALUES ($1, $2, $3, $4)")
        .bind(user_id)
        .bind(kind)
        .bind(item_id)
        .bind(detail)
        .execute(pool)
        .await?;
    Ok(())
}

#[derive(FromRow)]
struct Row {
    id: i64,
    ts: DateTime<Utc>,
    user: Option<String>,
    kind: String,
    item_id: Option<String>,
    item_title: Option<String>,
    detail: serde_json::Value,
}

pub async fn recent(pool: &PgPool, limit: i64) -> sqlx::Result<Vec<ActivityEvent>> {
    let rows: Vec<Row> = sqlx::query_as(
        "SELECT a.id, a.ts, COALESCE(u.name, u.email, u.subject) AS \"user\", a.kind, a.item_id,
                i.title AS item_title, a.detail
         FROM activity a LEFT JOIN users u ON u.id = a.user_id LEFT JOIN items i ON i.id = a.item_id
         ORDER BY a.ts DESC LIMIT $1",
    )
    .bind(limit.clamp(1, 500))
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| ActivityEvent {
            id: r.id,
            ts: r.ts,
            user: r.user,
            kind: r.kind,
            item_id: r.item_id,
            item_title: r.item_title,
            detail: r.detail,
        })
        .collect())
}

pub async fn prune(pool: &PgPool, days: u32) -> sqlx::Result<u64> {
    let res = sqlx::query("DELETE FROM activity WHERE ts < now() - ($1 || ' days')::interval")
        .bind(days.to_string())
        .execute(pool)
        .await?;
    Ok(res.rows_affected())
}
