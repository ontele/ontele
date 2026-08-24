// Copyright 2026 The Ontele Authors
// SPDX-License-Identifier: Apache-2.0

//! Shared harness for integration tests. Each test gets a fresh database
//! from `#[sqlx::test]` (needs `DATABASE_URL`), a temp data dir and an app
//! with background loops disabled.

#![allow(dead_code)]

use axum::{
    Router,
    body::{Body, to_bytes},
    http::{Method, Request, StatusCode, header},
};
use clap::Parser;
use ontele::{config::Config, state::AppState};
use sqlx::PgPool;
use std::sync::Arc;
use tower::ServiceExt;

pub struct TestApp {
    pub app: Router,
    pub state: Arc<AppState>,
    pub dir: tempfile::TempDir,
}

/// Build the app against an already-migrated pool (sqlx::test applies
/// `migrations/` for us when asked to).
pub async fn app_with_pool(pool: PgPool, auth: &str) -> TestApp {
    let dir = tempfile::tempdir().unwrap();
    let db_url = std::env::var("DATABASE_URL").unwrap_or_default();
    let cfg = Config::try_parse_from([
        "ontele",
        "--database-url",
        &db_url,
        "--auth",
        auth,
        "--data-dir",
        dir.path().to_str().unwrap(),
        "--admin-users",
        "admin@example.com",
        "--no-background",
    ])
    .unwrap();
    let state = ontele::build_state_with_pool(cfg, pool).await.unwrap();
    let app = ontele::build_app(state.clone());
    TestApp { app, state, dir }
}

pub struct Resp {
    pub status: StatusCode,
    pub body: Vec<u8>,
    pub headers: axum::http::HeaderMap,
}

impl Resp {
    pub fn json(&self) -> serde_json::Value {
        serde_json::from_slice(&self.body)
            .unwrap_or_else(|e| panic!("bad json ({e}): {}", String::from_utf8_lossy(&self.body)))
    }
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).to_string()
    }
}

impl TestApp {
    /// Send a request as `user` (oauth2-proxy style headers) or anonymous.
    pub async fn req(&self, method: Method, path: &str, user: Option<&str>, body: Option<serde_json::Value>) -> Resp {
        let mut b = Request::builder().method(method).uri(path);
        if let Some(u) = user {
            b = b.header("x-forwarded-email", u).header("x-forwarded-user", u);
        }
        let req = match body {
            Some(v) => b.header(header::CONTENT_TYPE, "application/json").body(Body::from(v.to_string())).unwrap(),
            None => b.body(Body::empty()).unwrap(),
        };
        let res = self.app.clone().oneshot(req).await.unwrap();
        let status = res.status();
        let headers = res.headers().clone();
        let body = to_bytes(res.into_body(), 64 * 1024 * 1024).await.unwrap().to_vec();
        Resp { status, body, headers }
    }
    pub async fn get(&self, path: &str, user: Option<&str>) -> Resp {
        self.req(Method::GET, path, user, None).await
    }
    pub async fn post(&self, path: &str, user: Option<&str>, body: serde_json::Value) -> Resp {
        self.req(Method::POST, path, user, Some(body)).await
    }
    pub async fn put(&self, path: &str, user: Option<&str>, body: serde_json::Value) -> Resp {
        self.req(Method::PUT, path, user, Some(body)).await
    }
    pub async fn delete(&self, path: &str, user: Option<&str>) -> Resp {
        self.req(Method::DELETE, path, user, None).await
    }
}

/// Insert a library row directly (no ffprobe needed).
pub async fn seed_item(pool: &PgPool, id: &str, kind: &str, path: &str, title: &str, extra: serde_json::Value) {
    let info = serde_json::json!({
        "durationSec": extra.get("duration").and_then(|d| d.as_f64()).unwrap_or(3600.0),
        "container": extra.get("container").and_then(|c| c.as_str()).unwrap_or("mp4"),
        "sizeBytes": 1000, "vcodec": "h264", "acodec": "aac", "width": 1920, "height": 1080,
        "audio": [{"index": 1, "codec": "aac", "channels": 2, "default": true}], "subtitles": [], "chapters": []
    });
    sqlx::query(
        "INSERT INTO items (id, kind, path, title, year, show, season, episode, artist, album_artist, album, album_id, track_no, status, info)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15)",
    )
    .bind(id)
    .bind(kind)
    .bind(path)
    .bind(title)
    .bind(extra.get("year").and_then(|v| v.as_i64()).map(|v| v as i32))
    .bind(extra.get("show").and_then(|v| v.as_str()))
    .bind(extra.get("season").and_then(|v| v.as_i64()).map(|v| v as i32))
    .bind(extra.get("episode").and_then(|v| v.as_i64()).map(|v| v as i32))
    .bind(extra.get("artist").and_then(|v| v.as_str()))
    .bind(extra.get("artist").and_then(|v| v.as_str()))
    .bind(extra.get("album").and_then(|v| v.as_str()))
    .bind(extra.get("albumId").and_then(|v| v.as_str()))
    .bind(extra.get("trackNo").and_then(|v| v.as_i64()).map(|v| v as i32))
    .bind(extra.get("status").and_then(|v| v.as_str()))
    .bind(sqlx::types::Json(info))
    .execute(pool)
    .await
    .unwrap();
}
