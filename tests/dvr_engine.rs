// Copyright 2026 The Ontele Authors
// SPDX-License-Identifier: Apache-2.0

//! DVR engine against a real Postgres: rule matching/dedupe, missed airings,
//! keep-N pruning, and an end-to-end capture from a fake tuner.

mod common;

use axum::{Router, routing::get};
use chrono::{Duration, Utc};
use ontele::{
    db,
    dvr::Engine,
    epg::Guide,
    hdhr::Client as Hdhr,
    media::art::Art,
    metadata::Enricher,
    model::{Airing, Channel, CommercialMode, Rule, Settings},
    state::{SettingsCache, http_client},
    telemetry::Activity,
};
use sqlx::PgPool;
use std::{collections::HashMap, sync::Arc};

fn airing(ch: &str, title: &str, start_min: i64, len_min: i64) -> Airing {
    use chrono::Timelike;
    // Force sub-microsecond digits: Postgres stores microseconds, so the
    // engine's dedupe must survive the round trip (regression for the
    // re-scheduling bug this used to hide when the clock was micro-aligned).
    let start = (Utc::now() + Duration::minutes(start_min)).with_nanosecond(123_456_789).unwrap();
    Airing {
        channel_id: ch.into(),
        title: title.into(),
        start,
        end: start + Duration::minutes(len_min),
        ..Default::default()
    }
}

async fn engine(
    pool: PgPool,
    airings: HashMap<String, Vec<Airing>>,
    mut set: Settings,
) -> (Arc<Engine>, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    set.recordings_dir = dir.path().to_string_lossy().to_string();
    set.commercial_mode = CommercialMode::Off;
    let settings = Arc::new(SettingsCache::new(pool.clone(), set));
    let http = http_client();
    let activity = Activity::new(pool.clone());
    let guide = Arc::new(Guide::with_airings(settings.clone(), http.clone(), airings));
    let hdhr = Arc::new(Hdhr::new(settings.clone(), pool.clone(), http.clone()));
    hdhr.load_cached().await.unwrap();
    let art = Arc::new(Art::new(pool.clone(), settings.clone(), dir.path().join("img"), http.clone()));
    let meta = Arc::new(Enricher::new(pool.clone(), settings.clone(), http.clone(), art, activity.clone()));
    (Arc::new(Engine::new(pool, settings, guide, hdhr, http, activity, meta)), dir)
}

#[sqlx::test(migrations = "./migrations")]
async fn series_pass_materializes_and_dedupes(pool: PgPool) {
    let mut airings = HashMap::new();
    airings.insert(
        "7.1".to_string(),
        vec![
            airing("7.1", "Jeopardy!", 60, 30),
            airing("7.1", "News", 90, 30),
            airing("7.1", "jeopardy!", 60 * 24, 30),
        ],
    );
    airings.insert("4.1".to_string(), vec![airing("4.1", "Jeopardy!", 120, 30)]);
    let (eng, _dir) = engine(pool.clone(), airings, Settings::default()).await;

    db::rules::insert(
        &pool,
        &Rule {
            id: "r1".into(),
            title: "Jeopardy!".into(),
            channel_id: None,
            keep: 0,
            user_id: None,
            created: Utc::now(),
        },
    )
    .await
    .unwrap();
    eng.tick().await;
    let recs = db::items::recordings(&pool, 0).await.unwrap();
    assert_eq!(recs.len(), 3, "case-insensitive title match on any channel: {recs:?}");
    assert!(recs.iter().all(|r| r.status.as_deref() == Some("scheduled") && r.rule_id.as_deref() == Some("r1")));

    // second pass is idempotent
    eng.tick().await;
    assert_eq!(db::items::recordings(&pool, 0).await.unwrap().len(), 3);

    // channel-restricted rule only matches its channel, and an airing already
    // scheduled by another rule is not duplicated by key (rule|channel|start)
    db::rules::insert(
        &pool,
        &Rule {
            id: "r2".into(),
            title: "News".into(),
            channel_id: Some("4.1".into()),
            keep: 0,
            user_id: None,
            created: Utc::now(),
        },
    )
    .await
    .unwrap();
    eng.tick().await;
    assert_eq!(db::items::recordings(&pool, 0).await.unwrap().len(), 3, "News is only on 7.1");
}

#[sqlx::test(migrations = "./migrations")]
async fn missed_airings_are_failed(pool: PgPool) {
    let (eng, _dir) = engine(pool.clone(), HashMap::new(), Settings { post_pad_min: 0, ..Default::default() }).await;
    eng.schedule(db::items::NewRecording {
        id: "old".into(),
        title: "Old".into(),
        subtitle: None,
        description: None,
        channel_id: "7.1".into(),
        channel_name: None,
        start: Utc::now() - Duration::hours(3),
        end: Utc::now() - Duration::hours(2),
        rule_id: None,
        season: None,
        episode: None,
    })
    .await
    .unwrap();
    eng.tick().await;
    let rec = db::items::get(&pool, 0, "old").await.unwrap().unwrap();
    assert_eq!(rec.status.as_deref(), Some("failed"));
    assert!(rec.error.unwrap_or_default().contains("missed"));
}

#[sqlx::test(migrations = "./migrations")]
async fn keep_n_prunes_oldest_files(pool: PgPool) {
    let (eng, dir) = engine(pool.clone(), HashMap::new(), Settings::default()).await;
    db::rules::insert(
        &pool,
        &Rule { id: "r1".into(), title: "Show".into(), channel_id: None, keep: 2, user_id: None, created: Utc::now() },
    )
    .await
    .unwrap();
    for i in 0..4 {
        let path = dir.path().join(format!("show{i}.mkv"));
        std::fs::write(&path, b"x").unwrap();
        sqlx::query(
            "INSERT INTO items (id, kind, path, title, channel_id, start_at, end_at, status, rule_id)
             VALUES ($1, 'recording', $2, 'Show', '7.1', $3, $3 + interval '30 min', 'done', 'r1')",
        )
        .bind(format!("rec{i}"))
        .bind(path.to_string_lossy().to_string())
        .bind(Utc::now() - Duration::days(10 - i))
        .execute(&pool)
        .await
        .unwrap();
    }
    eng.tick().await;
    let left: Vec<String> = db::items::recordings(&pool, 0).await.unwrap().into_iter().map(|r| r.id).collect();
    assert_eq!(left.len(), 2);
    assert!(left.contains(&"rec2".to_string()) && left.contains(&"rec3".to_string()), "newest two survive: {left:?}");
    assert!(
        !dir.path().join("show0.mkv").exists() && !dir.path().join("show1.mkv").exists(),
        "pruned files removed from disk"
    );
    assert!(dir.path().join("show3.mkv").exists());
}

#[sqlx::test(migrations = "./migrations")]
async fn capture_from_fake_tuner_completes(pool: PgPool) {
    // fake HDHomeRun: streams 64 KiB of pseudo-TS forever (until the client hangs up)
    let app = Router::new().route(
        "/auto/v7.1",
        get(|| async {
            let stream = futures::stream::unfold(0u32, |n| async move {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                Some((Ok::<_, std::io::Error>(bytes::Bytes::from(vec![0x47u8; 188 * 16])), n + 1))
            });
            axum::response::Response::builder()
                .header("content-type", "video/mp2t")
                .body(axum::body::Body::from_stream(stream))
                .unwrap()
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    db::channels::replace(
        &pool,
        &[Channel {
            guide_number: "7.1".into(),
            guide_name: "KABC".into(),
            url: format!("http://{addr}/auto/v7.1"),
            hd: true,
            icon: None,
        }],
    )
    .await
    .unwrap();
    let (eng, dir) =
        engine(pool.clone(), HashMap::new(), Settings { pre_pad_min: 0, post_pad_min: 0, ..Default::default() }).await;
    assert_eq!(eng.hdhr.channels().len(), 1, "lineup loaded from the channels table");

    eng.schedule(db::items::NewRecording {
        id: "cap".into(),
        title: "Evening News".into(),
        subtitle: Some("Late edition".into()),
        description: None,
        channel_id: "7.1".into(),
        channel_name: Some("KABC".into()),
        start: Utc::now() - Duration::seconds(5),
        end: Utc::now() + Duration::seconds(3),
        rule_id: None,
        season: None,
        episode: None,
    })
    .await
    .unwrap();
    eng.tick().await;
    assert_eq!(eng.active_count(), 1, "capture task started");

    // wait for the deadline + post-processing (remux of fake TS fails gracefully → keeps .ts)
    let mut status = String::new();
    for _ in 0..60 {
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        let rec = db::items::get(&pool, 0, "cap").await.unwrap().unwrap();
        status = rec.status.clone().unwrap_or_default();
        if status == "done" || status == "failed" {
            if status == "done" {
                let path = rec.path.expect("path stored");
                assert!(path.starts_with(dir.path().to_str().unwrap()), "recorded under recordings dir: {path}");
                assert!(path.contains("Evening News"), "{path}");
                let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
                assert!(size > 0, "captured bytes landed on disk");
            } else {
                panic!("capture failed: {:?}", rec.error);
            }
            break;
        }
    }
    assert_eq!(status, "done", "recording finished within the wait budget");
    assert_eq!(eng.active_count(), 0);
}
