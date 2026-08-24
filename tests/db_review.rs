// Copyright 2026 The Ontele Authors
// SPDX-License-Identifier: Apache-2.0

//! Regression tests for the `db` layer: LIKE escaping, per-kind search
//! ranking, case-insensitive show grouping, re-scan metadata merging, the
//! first-admin race and the settings cache.

mod common;

use axum::http::StatusCode;
use common::{app_with_pool, seed_item};
use ontele::{
    db,
    model::{Kind, Metadata, Settings},
};
use serde_json::json;
use sqlx::PgPool;

#[sqlx::test(migrations = "./migrations")]
async fn like_wildcards_in_queries_are_literal(pool: PgPool) {
    seed_item(&pool, "m1", "movie", "/m/100 Percent.mkv", "100% Pure", json!({})).await;
    seed_item(&pool, "m2", "movie", "/m/Alien.mkv", "Alien", json!({})).await;
    seed_item(&pool, "t1", "track", "/a/x.flac", "Under_score", json!({"artist": "A", "album": "B", "albumId": "ab"}))
        .await;

    // search(): "%" must not match everything, "_" must not match any char
    let hits = db::items::search(&pool, 0, "%", 12).await.unwrap();
    assert_eq!(hits.movies.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(), vec!["m1"]);
    assert!(hits.tracks.is_empty());
    let hits = db::items::search(&pool, 0, "Al_en", 12).await.unwrap();
    assert!(hits.movies.is_empty());
    let hits = db::items::search(&pool, 0, "der_sc", 12).await.unwrap();
    assert_eq!(hits.tracks.len(), 1);

    // list_kind() text filter
    let f = db::items::ListFilter { q: Some("%".into()), ..Default::default() };
    let rows = db::items::list_kind(&pool, 0, Kind::Movie, db::items::Sort::Title, &f, 100).await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].id, "m1");

    // music filters
    assert!(db::music::tracks(&pool, 0, Some("_"), 100).await.unwrap().len() == 1);
    assert!(db::music::tracks(&pool, 0, Some("%"), 100).await.unwrap().is_empty());
    assert!(db::music::albums(&pool, None, Some("%"), "title", 100).await.unwrap().is_empty());
    assert!(db::music::artists(&pool, Some("%")).await.unwrap().is_empty());
}

#[sqlx::test(migrations = "./migrations")]
async fn search_ranks_per_kind(pool: PgPool) {
    // 60 tracks all mentioning "alien" must not crowd out the one movie
    for i in 0..60 {
        seed_item(
            &pool,
            &format!("t{i}"),
            "track",
            &format!("/a/{i}.flac"),
            &format!("Alien song {i}"),
            json!({"artist": "Alien Band", "album": "Aliens", "albumId": "al"}),
        )
        .await;
    }
    seed_item(&pool, "m1", "movie", "/m/Aliens.mkv", "Aliens", json!({})).await;
    let hits = db::items::search(&pool, 0, "alien", 12).await.unwrap();
    assert_eq!(hits.movies.len(), 1);
    assert_eq!(hits.movies[0].id, "m1");
    assert_eq!(hits.tracks.len(), 12);
}

#[sqlx::test(migrations = "./migrations")]
async fn shows_group_case_insensitively(pool: PgPool) {
    seed_item(&pool, "e1", "episode", "/t/1.mkv", "One", json!({"show": "Lost", "season": 1, "episode": 1})).await;
    seed_item(&pool, "e2", "episode", "/t/2.mkv", "Two", json!({"show": "lost", "season": 1, "episode": 2})).await;
    let shows = db::items::shows(&pool, 0).await.unwrap();
    assert_eq!(shows.len(), 1, "{shows:?}");
    assert_eq!(shows[0].episodes, 2);
    assert_eq!(shows[0].poster_id, "e1");
    // the detail view groups the same way
    let eps = db::items::show_episodes(&pool, 0, "LOST").await.unwrap();
    assert_eq!(eps.len(), 2);
}

#[sqlx::test(migrations = "./migrations")]
async fn rescan_keeps_provider_metadata(pool: PgPool) {
    let mut n = db::items::NewItem {
        id: "t1".into(),
        kind: Kind::Track,
        path: "/a/x.flac".into(),
        title: "X".into(),
        size_bytes: 10,
        ..Default::default()
    };
    assert!(db::items::upsert_scanned(&pool, &n).await.unwrap());
    // enrichment pass fills in provider data
    let enriched = Metadata {
        provider: Some("musicbrainz".into()),
        poster_url: Some("http://art/1.jpg".into()),
        genres: vec!["Jazz".into()],
        ..Default::default()
    };
    db::items::set_meta(&pool, "t1", &enriched).await.unwrap();
    // file changed on disk → re-scan with only the embedded mbid tag
    n.size_bytes = 11;
    n.meta = Some(Metadata { mbid: Some("mb-123".into()), ..Default::default() });
    assert!(!db::items::upsert_scanned(&pool, &n).await.unwrap());
    let m = db::items::get_meta(&pool, "t1").await.unwrap().unwrap();
    assert_eq!(m.provider.as_deref(), Some("musicbrainz"));
    assert_eq!(m.poster_url.as_deref(), Some("http://art/1.jpg"));
    assert_eq!(m.genres, vec!["Jazz"]);
    assert_eq!(m.mbid.as_deref(), Some("mb-123"));
    // a re-scan without scan-time meta leaves the row untouched
    n.meta = None;
    n.size_bytes = 12;
    db::items::upsert_scanned(&pool, &n).await.unwrap();
    assert_eq!(db::items::get_meta(&pool, "t1").await.unwrap().unwrap(), m);
}

#[sqlx::test(migrations = "./migrations")]
async fn watch_on_unknown_item_is_404(pool: PgPool) {
    let t = app_with_pool(pool, "proxy").await;
    let r = t.post("/api/watch/nope", Some("alice@example.com"), json!({"pos": 10, "dur": 100})).await;
    assert_eq!(r.status, StatusCode::NOT_FOUND, "{}", r.text());
}

#[sqlx::test(migrations = "./migrations")]
async fn first_admin_bootstrap_is_race_free(pool: PgPool) {
    let mut tasks = Vec::new();
    for i in 0..16 {
        let pool = pool.clone();
        tasks.push(tokio::spawn(async move {
            db::users::upsert(&pool, &format!("user-{i}"), None, None, &[], false, true).await.unwrap()
        }));
    }
    let mut admins = 0;
    for t in tasks {
        if t.await.unwrap().is_admin {
            admins += 1;
        }
    }
    assert_eq!(admins, 1, "exactly one bootstrap admin");
    let stored = db::users::list(&pool).await.unwrap().iter().filter(|u| u.is_admin).count();
    assert_eq!(stored, 1);
}

#[sqlx::test(migrations = "./migrations")]
async fn settings_cache_matches_database_after_concurrent_writes(pool: PgPool) {
    let t = app_with_pool(pool.clone(), "none").await;
    let mut tasks = Vec::new();
    for i in 0..12u32 {
        let st = t.state.clone();
        tasks.push(tokio::spawn(async move {
            st.settings.set(Settings { max_transcodes: i + 1, ..Default::default() }).await.unwrap();
        }));
    }
    for t in tasks {
        t.await.unwrap();
    }
    let db_row = db::settings::load(&pool).await.unwrap().unwrap();
    assert_eq!(*t.state.settings.get(), db_row);
}
