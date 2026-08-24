// Copyright 2026 The Ontele Authors
// SPDX-License-Identifier: Apache-2.0

//! Shared application state: one `Arc<AppState>` is cloned into every
//! handler and background task.

use crate::{
    auth::UserCache, config::Config, dvr::Engine, epg::Guide, hdhr::Client as Hdhr, media::Scanner, media::art::Art,
    metadata::Enricher, model::Settings, stream::Manager as Streams, telemetry::Activity,
};
use metrics_exporter_prometheus::PrometheusHandle;
use parking_lot::RwLock;
use sqlx::PgPool;
use std::{path::PathBuf, sync::Arc, time::Instant};

/// In-memory copy of the settings row. Reads are lock-free-ish (one RwLock
/// read + Arc clone); writes persist first, then swap.
pub struct SettingsCache {
    pool: PgPool,
    cur: RwLock<Arc<Settings>>,
    /// Serializes `set` so two concurrent PUTs can't leave the cache holding
    /// a different row than the database (save A, save B, swap B, swap A).
    write: tokio::sync::Mutex<()>,
}

impl SettingsCache {
    pub fn new(pool: PgPool, initial: Settings) -> Self {
        Self { pool, cur: RwLock::new(Arc::new(initial)), write: tokio::sync::Mutex::new(()) }
    }
    pub fn get(&self) -> Arc<Settings> {
        self.cur.read().clone()
    }
    pub async fn set(&self, mut s: Settings) -> sqlx::Result<Arc<Settings>> {
        s.normalize();
        let _g = self.write.lock().await;
        crate::db::settings::save(&self.pool, &s).await?;
        let arc = Arc::new(s);
        *self.cur.write() = arc.clone();
        Ok(arc)
    }
}

pub struct AppState {
    pub cfg: Arc<Config>,
    pub pool: PgPool,
    pub settings: Arc<SettingsCache>,
    pub users: UserCache,
    pub scanner: Arc<Scanner>,
    pub art: Arc<Art>,
    pub streams: Arc<Streams>,
    pub hdhr: Arc<Hdhr>,
    pub guide: Arc<Guide>,
    pub dvr: Arc<Engine>,
    pub metadata: Arc<Enricher>,
    pub activity: Activity,
    pub metrics: PrometheusHandle,
    pub http: reqwest::Client,
    pub started: Instant,
    pub health: Arc<crate::health::Health>,
}

impl AppState {
    pub fn data_dir(&self) -> &PathBuf {
        &self.cfg.data_dir
    }
}

/// Shared HTTP client: rustls, sane timeouts, identifiable UA (MusicBrainz
/// requires one).
pub fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .user_agent(concat!("ontele/", env!("CARGO_PKG_VERSION"), " (https://github.com/ontele/ontele)"))
        .connect_timeout(std::time::Duration::from_secs(10))
        .timeout(std::time::Duration::from_secs(60))
        .build()
        .expect("reqwest client")
}
