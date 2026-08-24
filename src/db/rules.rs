// Copyright 2026 The Ontele Authors
// SPDX-License-Identifier: Apache-2.0

use crate::model::Rule;
use chrono::{DateTime, Utc};
use sqlx::{FromRow, PgPool};

#[derive(FromRow)]
struct Row {
    id: String,
    title: String,
    channel_id: Option<String>,
    keep: i32,
    user_id: Option<i64>,
    created: DateTime<Utc>,
}

impl From<Row> for Rule {
    fn from(r: Row) -> Self {
        Rule {
            id: r.id,
            title: r.title,
            channel_id: r.channel_id,
            keep: r.keep,
            user_id: r.user_id,
            created: r.created,
        }
    }
}

pub async fn list(pool: &PgPool) -> sqlx::Result<Vec<Rule>> {
    let rows: Vec<Row> =
        sqlx::query_as("SELECT id, title, channel_id, keep, user_id, created FROM rules ORDER BY created DESC")
            .fetch_all(pool)
            .await?;
    Ok(rows.into_iter().map(Into::into).collect())
}

pub async fn get(pool: &PgPool, id: &str) -> sqlx::Result<Option<Rule>> {
    let row: Option<Row> =
        sqlx::query_as("SELECT id, title, channel_id, keep, user_id, created FROM rules WHERE id = $1")
            .bind(id)
            .fetch_optional(pool)
            .await?;
    Ok(row.map(Into::into))
}

pub async fn insert(pool: &PgPool, r: &Rule) -> sqlx::Result<()> {
    sqlx::query("INSERT INTO rules (id, title, channel_id, keep, user_id, created) VALUES ($1, $2, $3, $4, $5, $6)")
        .bind(&r.id)
        .bind(&r.title)
        .bind(r.channel_id.as_deref().filter(|c| !c.is_empty()))
        .bind(r.keep)
        .bind(r.user_id)
        .bind(r.created)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn delete(pool: &PgPool, id: &str) -> sqlx::Result<bool> {
    let res = sqlx::query("DELETE FROM rules WHERE id = $1").bind(id).execute(pool).await?;
    Ok(res.rows_affected() > 0)
}
