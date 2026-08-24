// Copyright 2026 The Ontele Authors
// SPDX-License-Identifier: Apache-2.0

use sqlx::PgPool;

/// Normalized tag name: trimmed, lowercase, single spaces.
pub fn normalize(name: &str) -> String {
    name.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase()
}

pub async fn list(pool: &PgPool) -> sqlx::Result<Vec<(String, i64)>> {
    sqlx::query_as(
        "SELECT t.name, COUNT(it.item_id) FROM tags t LEFT JOIN item_tags it ON it.tag_id = t.id
         GROUP BY t.id ORDER BY t.name",
    )
    .fetch_all(pool)
    .await
}

pub async fn add(pool: &PgPool, item_id: &str, names: &[String]) -> sqlx::Result<()> {
    let mut tx = pool.begin().await?;
    for raw in names {
        let name = normalize(raw);
        if name.is_empty() {
            continue;
        }
        let (tag_id,): (i32,) = sqlx::query_as(
            "INSERT INTO tags (name) VALUES ($1) ON CONFLICT (name) DO UPDATE SET name = EXCLUDED.name RETURNING id",
        )
        .bind(&name)
        .fetch_one(&mut *tx)
        .await?;
        sqlx::query("INSERT INTO item_tags (item_id, tag_id) VALUES ($1, $2) ON CONFLICT DO NOTHING")
            .bind(item_id)
            .bind(tag_id)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await
}

pub async fn remove(pool: &PgPool, item_id: &str, name: &str) -> sqlx::Result<()> {
    sqlx::query("DELETE FROM item_tags WHERE item_id = $1 AND tag_id = (SELECT id FROM tags WHERE name = $2)")
        .bind(item_id)
        .bind(normalize(name))
        .execute(pool)
        .await?;
    // drop orphans so the tag list stays tidy
    sqlx::query("DELETE FROM tags WHERE NOT EXISTS (SELECT 1 FROM item_tags WHERE tag_id = tags.id)")
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn for_item(pool: &PgPool, item_id: &str) -> sqlx::Result<Vec<String>> {
    let rows: Vec<(String,)> = sqlx::query_as(
        "SELECT t.name FROM item_tags it JOIN tags t ON t.id = it.tag_id WHERE it.item_id = $1 ORDER BY t.name",
    )
    .bind(item_id)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(|(n,)| n).collect())
}

#[cfg(test)]
mod tests {
    #[test]
    fn normalizes() {
        assert_eq!(super::normalize("  Date   Night "), "date night");
    }
}
