// Copyright 2026 The Ontele Authors
// SPDX-License-Identifier: Apache-2.0

use crate::model::Settings;
use sqlx::PgPool;

pub async fn load(pool: &PgPool) -> sqlx::Result<Option<Settings>> {
    let row: Option<(sqlx::types::Json<Settings>,)> =
        sqlx::query_as("SELECT data FROM settings WHERE id = 1").fetch_optional(pool).await?;
    Ok(row.map(|(j,)| {
        let mut s = j.0;
        s.normalize();
        s
    }))
}

pub async fn save(pool: &PgPool, s: &Settings) -> sqlx::Result<()> {
    sqlx::query(
        "INSERT INTO settings (id, data, updated) VALUES (1, $1, now())
         ON CONFLICT (id) DO UPDATE SET data = EXCLUDED.data, updated = now()",
    )
    .bind(sqlx::types::Json(s))
    .execute(pool)
    .await?;
    Ok(())
}
