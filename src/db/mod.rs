// Copyright 2026 The Ontele Authors
// SPDX-License-Identifier: Apache-2.0

//! PostgreSQL access. Each submodule owns one aggregate; handlers never write
//! SQL. All queries are parameterized; the only dynamic SQL is whitelisted
//! `ORDER BY` fragments wrapped in `AssertSqlSafe`.

pub mod activity;
pub mod channels;
pub mod items;
pub mod music;
pub mod rules;
pub mod settings;
pub mod tags;
pub mod trending;
pub mod users;
pub mod watch;

use sqlx::{PgPool, postgres::PgPoolOptions};
use std::time::Duration;

pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");

/// `%…%` pattern for a user-supplied substring search. `\`, `%` and `_`
/// are escaped so they match literally instead of acting as wildcards
/// (Postgres' default `LIKE` escape character is `\`).
pub fn like_contains(q: &str) -> String {
    let mut out = String::with_capacity(q.len() + 2);
    out.push('%');
    for c in q.trim().chars() {
        if matches!(c, '\\' | '%' | '_') {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('%');
    out
}

pub async fn connect(url: &str, max: u32) -> anyhow::Result<PgPool> {
    use sqlx::ConnectOptions;
    let opts: sqlx::postgres::PgConnectOptions = url.parse()?;
    // Migrations and big GIN index builds legitimately exceed sqlx's 1 s
    // default; the warning would dump the whole statement into the logs.
    let opts = opts
        .log_statements(tracing::log::LevelFilter::Trace)
        .log_slow_statements(tracing::log::LevelFilter::Debug, Duration::from_secs(10));
    let pool = PgPoolOptions::new()
        .max_connections(max.max(2))
        .min_connections(1)
        .acquire_timeout(Duration::from_secs(10))
        .idle_timeout(Duration::from_secs(300))
        .connect_with(opts)
        .await?;
    Ok(pool)
}

/// Connect with retries (Postgres in the same compose/k8s stack may still be
/// booting) and apply migrations.
pub async fn connect_and_migrate(url: &str, max: u32) -> anyhow::Result<PgPool> {
    let mut attempt = 0u32;
    let pool = loop {
        match connect(url, max).await {
            Ok(p) => break p,
            Err(e) if attempt < 30 => {
                attempt += 1;
                tracing::warn!(attempt, error = %e, "postgres not ready, retrying");
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
            Err(e) => return Err(e),
        }
    };
    MIGRATOR.run(&pool).await?;
    Ok(pool)
}

#[cfg(test)]
mod tests {
    #[test]
    fn like_contains_escapes_wildcards() {
        assert_eq!(super::like_contains(" 100% _x_ "), "%100\\% \\_x\\_%");
        assert_eq!(super::like_contains("a\\b"), "%a\\\\b%");
        assert_eq!(super::like_contains("plain"), "%plain%");
    }
}
