// Copyright 2026 The Ontele Authors
// SPDX-License-Identifier: Apache-2.0

//! Cached HDHomeRun lineup so Live TV renders instantly after a restart,
//! before the first tuner refresh completes.

use crate::model::Channel;
use sqlx::PgPool;

pub async fn replace(pool: &PgPool, chans: &[Channel]) -> sqlx::Result<()> {
    let mut tx = pool.begin().await?;
    sqlx::query("DELETE FROM channels").execute(&mut *tx).await?;
    for c in chans {
        sqlx::query("INSERT INTO channels (guide_number, guide_name, url, hd, icon) VALUES ($1, $2, $3, $4, $5)")
            .bind(&c.guide_number)
            .bind(&c.guide_name)
            .bind(&c.url)
            .bind(c.hd)
            .bind(&c.icon)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await
}

pub async fn set_icons(pool: &PgPool, icons: &[(String, String)]) -> sqlx::Result<()> {
    for (num, icon) in icons {
        sqlx::query("UPDATE channels SET icon = $2 WHERE guide_number = $1").bind(num).bind(icon).execute(pool).await?;
    }
    Ok(())
}

pub async fn list(pool: &PgPool) -> sqlx::Result<Vec<Channel>> {
    let rows: Vec<(String, String, String, bool, Option<String>)> =
        sqlx::query_as("SELECT guide_number, guide_name, url, hd, icon FROM channels ORDER BY guide_number")
            .fetch_all(pool)
            .await?;
    let mut out: Vec<Channel> = rows
        .into_iter()
        .map(|(guide_number, guide_name, url, hd, icon)| Channel { guide_number, guide_name, url, hd, icon })
        .collect();
    out.sort_by(|a, b| {
        crate::hdhr::channel_sort_key(&a.guide_number)
            .partial_cmp(&crate::hdhr::channel_sort_key(&b.guide_number))
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    Ok(out)
}
