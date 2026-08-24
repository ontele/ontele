// Copyright 2026 The Ontele Authors
// SPDX-License-Identifier: Apache-2.0

//! End-to-end API tests over a real Postgres (`DATABASE_URL` required;
//! `#[sqlx::test]` creates a throwaway database per test and applies
//! `migrations/`). Background loops are disabled; ffmpeg is not needed.

mod common;

use axum::http::StatusCode;
use common::{app_with_pool, seed_item};
use serde_json::json;
use sqlx::PgPool;

#[sqlx::test(migrations = "./migrations")]
async fn identity_is_required_in_proxy_mode(pool: PgPool) {
    let t = app_with_pool(pool, "proxy").await;
    assert_eq!(t.get("/api/me", None).await.status, StatusCode::UNAUTHORIZED);
    let r = t.get("/api/me", Some("alice@example.com")).await;
    assert_eq!(r.status, StatusCode::OK);
    let j = r.json();
    assert_eq!(j["user"]["email"], "alice@example.com");
    // an admin list is configured (see common::app_with_pool), so the first
    // visitor must NOT be bootstrapped into admin
    assert_eq!(j["user"]["isAdmin"], false);
    let j2 = t.get("/api/me", Some("bob@example.com")).await.json();
    assert_eq!(j2["user"]["isAdmin"], false);
    // configured admin list wins regardless of order
    let j3 = t.get("/api/me", Some("admin@example.com")).await.json();
    assert_eq!(j3["user"]["isAdmin"], true);
    // cross-site state-changing requests are refused before identity
    let mut b = axum::http::Request::builder().method(axum::http::Method::POST).uri("/api/scan");
    b = b
        .header("x-forwarded-email", "admin@example.com")
        .header("x-forwarded-user", "admin@example.com")
        .header("sec-fetch-site", "cross-site");
    let res = tower::ServiceExt::oneshot(t.app.clone(), b.body(axum::body::Body::empty()).unwrap()).await.unwrap();
    assert_eq!(res.status(), StatusCode::FORBIDDEN);
    // health endpoints stay public
    assert_eq!(t.get("/healthz", None).await.status, StatusCode::OK);
    assert_eq!(t.get("/metrics", None).await.status, StatusCode::OK);
}

#[sqlx::test(migrations = "./migrations")]
async fn none_mode_uses_local_admin(pool: PgPool) {
    let t = app_with_pool(pool, "none").await;
    let j = t.get("/api/me", None).await.json();
    assert_eq!(j["user"]["subject"], "local");
    assert_eq!(j["user"]["isAdmin"], true);
    assert_eq!(j["authMode"], "none");
}

#[sqlx::test(migrations = "./migrations")]
async fn settings_roundtrip_and_admin_guard(pool: PgPool) {
    let t = app_with_pool(pool, "proxy").await;
    let admin = Some("admin@example.com");
    let bob = Some("bob@example.com");
    let _ = t.get("/api/me", admin).await; // admin exists first
    let mut s = t.get("/api/settings", admin).await.json();
    s["mediaDirs"] = json!(["/tank/movies"]);
    s["commercialMode"] = json!("delete");
    s["tmdbApiKey"] = json!("secret-key");
    assert_eq!(t.put("/api/settings", bob, s.clone()).await.status, StatusCode::FORBIDDEN);
    let saved = t.put("/api/settings", admin, s).await;
    assert_eq!(saved.status, StatusCode::OK, "{}", saved.text());
    assert_eq!(saved.json()["commercialMode"], "delete");
    // non-admins never see the key
    assert_eq!(t.get("/api/settings", bob).await.json()["tmdbApiKey"], "••••••");
    // masked value round-trips without clobbering the secret
    let mut s2 = t.get("/api/settings", bob).await.json();
    s2["prePadMin"] = json!(5);
    let again = t.put("/api/settings", admin, s2).await.json();
    assert_eq!(again["tmdbApiKey"], "secret-key");
    assert_eq!(again["prePadMin"], 5);
    assert_eq!(t.state.settings.get().pre_pad_min, 5);
}

#[sqlx::test(migrations = "./migrations")]
async fn library_listing_watch_and_search(pool: PgPool) {
    seed_item(&pool, "m1", "movie", "/m/Heat (1995).mkv", "Heat", json!({"year": 1995, "container": "mkv"})).await;
    seed_item(&pool, "m2", "movie", "/m/Alien (1979).mkv", "Alien", json!({"year": 1979})).await;
    seed_item(
        &pool,
        "e1",
        "episode",
        "/t/S01E01.mkv",
        "Pilot",
        json!({"show": "Severance", "season": 1, "episode": 1}),
    )
    .await;
    seed_item(
        &pool,
        "e2",
        "episode",
        "/t/S01E02.mkv",
        "Half Loop",
        json!({"show": "Severance", "season": 1, "episode": 2}),
    )
    .await;
    seed_item(
        &pool,
        "e3",
        "episode",
        "/t/S02E01.mkv",
        "Hello Ms Cobel",
        json!({"show": "Severance", "season": 2, "episode": 1}),
    )
    .await;
    let t = app_with_pool(pool, "proxy").await;
    let u = Some("alice@example.com");

    let movies = t.get("/api/movies", u).await.json();
    assert_eq!(movies.as_array().unwrap().len(), 2);
    assert_eq!(movies[0]["title"], "Alien"); // title sort
    let by_year = t.get("/api/movies?sort=year", u).await.json();
    assert_eq!(by_year[0]["title"], "Heat");

    let shows = t.get("/api/shows", u).await.json();
    assert_eq!(shows[0]["show"], "Severance");
    assert_eq!(shows[0]["episodes"], 3);
    assert_eq!(shows[0]["seasons"], 2);

    let show = t.get("/api/shows/severance", u).await.json();
    assert_eq!(show["seasons"].as_array().unwrap().len(), 2);
    assert_eq!(show["seasons"][0]["episodes"][1]["title"], "Half Loop");

    // detail carries next episode across seasons
    let e2 = t.get("/api/items/e2", u).await.json();
    assert_eq!(e2["nextEpisode"]["id"], "e3");
    assert!(e2.get("path").is_none(), "paths must not leak");

    // watch progress → continue watching (per user)
    assert_eq!(t.post("/api/watch/m1", u, json!({"pos": 600, "dur": 6000})).await.status, StatusCode::OK);
    let home = t.get("/api/home", u).await.json();
    assert_eq!(home["continue"][0]["id"], "m1");
    assert_eq!(home["continue"][0]["watch"]["pos"], 600.0);
    let other = t.get("/api/home", Some("bob@example.com")).await.json();
    assert!(other["continue"].as_array().unwrap().is_empty());

    // finishing e1 puts e2 in "up next"
    t.post("/api/watch/e1", u, json!({"pos": 2900, "dur": 3000})).await;
    let home = t.get("/api/home", u).await.json();
    assert_eq!(home["upNext"][0]["id"], "e2");

    let s = t.get("/api/search?q=hea", u).await.json();
    assert_eq!(s["movies"][0]["id"], "m1");
    let s = t.get("/api/search?q=sever", u).await.json();
    assert_eq!(s["shows"][0]["show"], "Severance");
    assert!(!s["episodes"].as_array().unwrap().is_empty());
}

#[sqlx::test(migrations = "./migrations")]
async fn tags_add_list_remove(pool: PgPool) {
    seed_item(&pool, "m1", "movie", "/m/Heat (1995).mkv", "Heat", json!({})).await;
    let t = app_with_pool(pool, "proxy").await;
    let u = Some("alice@example.com");
    let r = t.post("/api/items/m1/tags", u, json!({"tags": ["Date Night", " crime "]})).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text());
    assert_eq!(r.json()["tags"], json!(["crime", "date night"]));
    let all = t.get("/api/tags", u).await.json();
    assert_eq!(all.as_array().unwrap().len(), 2);
    let filtered = t.get("/api/movies?tag=crime", u).await.json();
    assert_eq!(filtered[0]["id"], "m1");
    assert!(t.get("/api/movies?tag=nope", u).await.json().as_array().unwrap().is_empty());
    t.delete("/api/items/m1/tags/crime", u).await;
    assert_eq!(t.get("/api/items/m1", u).await.json()["tags"], json!(["date night"]));
    assert_eq!(t.get("/api/tags", u).await.json().as_array().unwrap().len(), 1);
}

#[sqlx::test(migrations = "./migrations")]
async fn music_aggregates(pool: PgPool) {
    let alb = ontele::model::album_id("Daft Punk", "Discovery");
    seed_item(
        &pool,
        "t1",
        "track",
        "/mu/01.flac",
        "One More Time",
        json!({"artist": "Daft Punk", "album": "Discovery", "albumId": alb, "trackNo": 1, "duration": 320.0}),
    )
    .await;
    seed_item(
        &pool,
        "t2",
        "track",
        "/mu/02.flac",
        "Aerodynamic",
        json!({"artist": "Daft Punk", "album": "Discovery", "albumId": alb, "trackNo": 2, "duration": 212.0}),
    )
    .await;
    let t = app_with_pool(pool, "proxy").await;
    let u = Some("alice@example.com");
    let artists = t.get("/api/music/artists", u).await.json();
    assert_eq!(artists[0]["name"], "Daft Punk");
    assert_eq!(artists[0]["tracks"], 2);
    let albums = t.get("/api/music/albums", u).await.json();
    assert_eq!(albums[0]["title"], "Discovery");
    assert_eq!(albums[0]["tracks"], 2);
    assert_eq!(albums[0]["duration"], 532.0);
    let album = t.get(&format!("/api/music/albums/{alb}"), u).await.json();
    assert_eq!(album["tracks"][0]["title"], "One More Time");
    assert_eq!(album["tracks"][1]["trackNo"], 2);
    let home = t.get("/api/home", u).await.json();
    assert_eq!(home["albums"][0]["id"], alb);
}

#[sqlx::test(migrations = "./migrations")]
async fn dvr_rules_and_manual_recording(pool: PgPool) {
    let t = app_with_pool(pool, "proxy").await;
    let u = Some("alice@example.com");
    let bob = Some("bob@example.com");
    let r = t.post("/api/dvr/rules", u, json!({"title": "Jeopardy!", "keep": 5})).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text());
    let rule_id = r.json()["id"].as_str().unwrap().to_string();
    assert_eq!(t.get("/api/dvr/rules", u).await.json()[0]["title"], "Jeopardy!");
    assert_eq!(t.post("/api/dvr/rules", u, json!({"title": "  "})).await.status, StatusCode::BAD_REQUEST);
    // bob can't delete alice's pass
    assert_eq!(t.delete(&format!("/api/dvr/rules/{rule_id}"), bob).await.status, StatusCode::FORBIDDEN);
    assert_eq!(t.delete(&format!("/api/dvr/rules/{rule_id}"), u).await.status, StatusCode::OK);
    assert!(t.get("/api/dvr/rules", u).await.json().as_array().unwrap().is_empty());

    let start = chrono::Utc::now() + chrono::Duration::hours(1);
    let end = start + chrono::Duration::minutes(30);
    let r =
        t.post("/api/dvr/record", u, json!({"channelId": "7.1", "title": "News", "start": start, "end": end})).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text());
    let rec = r.json();
    assert_eq!(rec["status"], "scheduled");
    assert_eq!(rec["kind"], "recording");
    let id = rec["id"].as_str().unwrap().to_string();
    assert_eq!(t.get("/api/dvr/recordings", u).await.json()[0]["id"], id);
    assert_eq!(
        t.post("/api/dvr/record", u, json!({"channelId": "7.1", "start": end, "end": start})).await.status,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(t.delete(&format!("/api/dvr/recordings/{id}"), u).await.status, StatusCode::OK);
    assert!(t.get("/api/dvr/recordings", u).await.json().as_array().unwrap().is_empty());
}

#[sqlx::test(migrations = "./migrations")]
async fn activity_and_stats(pool: PgPool) {
    seed_item(&pool, "m1", "movie", "/m/Heat (1995).mkv", "Heat", json!({})).await;
    let t = app_with_pool(pool, "proxy").await;
    let u = Some("alice@example.com");
    t.post("/api/watch/m1", u, json!({"pos": 5900, "dur": 6000})).await;
    // activity inserts are fire-and-forget; poll rather than racing a fixed sleep
    let mut act = serde_json::Value::Null;
    for _ in 0..50 {
        act = t.get("/api/activity", u).await.json();
        if act[0]["kind"] == "watch.done" {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert_eq!(act[0]["kind"], "watch.done");
    assert_eq!(act[0]["itemTitle"], "Heat");
    assert_eq!(act[0]["user"], "alice@example.com");
    let stats = t.get("/api/stats", u).await.json();
    assert_eq!(stats["items"]["movie"], 1);
    // deleting an item must not leak its filesystem path into the shared feed
    let admin = Some("admin@example.com");
    assert_eq!(t.delete("/api/items/m1", u).await.status, StatusCode::FORBIDDEN);
    assert_eq!(t.delete("/api/items/m1", admin).await.status, StatusCode::OK);
    let mut act = String::new();
    for _ in 0..50 {
        act = t.get("/api/activity", u).await.text();
        if act.contains("item.delete") {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert!(!act.contains("/m/Heat"), "{act}");
    let m = t.get("/metrics", None).await.text();
    assert!(m.contains("ontele_http_requests_total"), "{m}");
}

#[sqlx::test(migrations = "./migrations")]
async fn ui_is_served_with_etag_and_fallback(pool: PgPool) {
    let t = app_with_pool(pool, "proxy").await;
    let r = t.get("/", None).await;
    assert_eq!(r.status, StatusCode::OK);
    assert!(r.text().contains("<html"));
    let etag = r.headers.get("etag").unwrap().clone();
    let r2 = t.req(axum::http::Method::GET, "/some/deep/route", None, None).await;
    assert_eq!(r2.status, StatusCode::OK, "hash-router fallback");
    let cached = t.app.clone();
    let req =
        axum::http::Request::builder().uri("/").header("if-none-match", etag).body(axum::body::Body::empty()).unwrap();
    let res = tower::ServiceExt::oneshot(cached, req).await.unwrap();
    assert_eq!(res.status(), StatusCode::NOT_MODIFIED);
}
