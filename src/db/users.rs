// Copyright 2026 The Ontele Authors
// SPDX-License-Identifier: Apache-2.0

use crate::model::User;
use chrono::{DateTime, Utc};
use sqlx::{FromRow, PgPool};

#[derive(FromRow)]
struct Row {
    id: i64,
    subject: String,
    email: Option<String>,
    name: Option<String>,
    groups: Vec<String>,
    is_admin: bool,
    created: DateTime<Utc>,
    last_seen: DateTime<Utc>,
}

impl From<Row> for User {
    fn from(r: Row) -> Self {
        User {
            id: r.id,
            subject: r.subject,
            email: r.email,
            name: r.name,
            groups: r.groups,
            is_admin: r.is_admin,
            created: r.created,
            last_seen: r.last_seen,
        }
    }
}

const COLS: &str = "id, subject, email, name, groups, is_admin, created, last_seen";

/// Insert-or-touch a user. `admin` is the caller's verdict from config; it is
/// OR-ed with the stored flag so a manual promotion sticks. When `bootstrap`
/// is set (no admin users/groups configured) and no admin exists yet, the
/// first user becomes one.
pub async fn upsert(
    pool: &PgPool,
    subject: &str,
    email: Option<&str>,
    name: Option<&str>,
    groups: &[String],
    admin: bool,
    bootstrap: bool,
) -> sqlx::Result<User> {
    // Serialize the "is there an admin yet?" check against concurrent first
    // requests; without the lock two strangers racing on a fresh install
    // would both pass `NOT EXISTS (... is_admin)` and both become admin.
    let mut tx = pool.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock(hashtext('ontele.users.upsert'))").execute(&mut *tx).await?;
    let row: Row = sqlx::query_as(
        "INSERT INTO users (subject, email, name, groups, is_admin)
         VALUES ($1, $2, $3, $4, $5 OR ($6 AND NOT EXISTS (SELECT 1 FROM users WHERE is_admin)))
         ON CONFLICT (subject) DO UPDATE SET
            email = COALESCE(EXCLUDED.email, users.email),
            name = COALESCE(EXCLUDED.name, users.name),
            groups = EXCLUDED.groups,
            is_admin = users.is_admin OR EXCLUDED.is_admin,
            last_seen = now()
         RETURNING id, subject, email, name, groups, is_admin, created, last_seen",
    )
    .bind(subject)
    .bind(email)
    .bind(name)
    .bind(groups)
    .bind(admin)
    .bind(bootstrap)
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(row.into())
}

pub async fn get(pool: &PgPool, id: i64) -> sqlx::Result<Option<User>> {
    let row: Option<Row> = sqlx::query_as(sqlx::AssertSqlSafe(format!("SELECT {COLS} FROM users WHERE id = $1")))
        .bind(id)
        .fetch_optional(pool)
        .await?;
    Ok(row.map(Into::into))
}

pub async fn list(pool: &PgPool) -> sqlx::Result<Vec<User>> {
    let rows: Vec<Row> = sqlx::query_as(sqlx::AssertSqlSafe(format!("SELECT {COLS} FROM users ORDER BY created")))
        .fetch_all(pool)
        .await?;
    Ok(rows.into_iter().map(Into::into).collect())
}

pub async fn set_admin(pool: &PgPool, id: i64, admin: bool) -> sqlx::Result<()> {
    sqlx::query("UPDATE users SET is_admin = $2 WHERE id = $1").bind(id).bind(admin).execute(pool).await?;
    Ok(())
}
