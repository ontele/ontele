// Copyright 2026 The Ontele Authors
// SPDX-License-Identifier: Apache-2.0

//! Ontele — a single-binary media server in Rust: library (movies, TV,
//! music) with metadata, direct play + HLS transcoding for every codec
//! ffmpeg can read, HDHomeRun live TV, DVR with series passes, commercial
//! skip/chapter-tag/delete, identity from an OAuth2 proxy, state in
//! PostgreSQL, logs/metrics for Loki + Prometheus + Grafana.

pub mod api;
pub mod auth;
pub mod commercials;
pub mod config;
pub mod db;
pub mod dvr;
pub mod epg;
pub mod error;
pub mod hdhr;
pub mod health;
pub mod media;
pub mod metadata;
pub mod model;
pub mod naming;
pub mod state;
pub mod stream;
pub mod telemetry;
pub mod web;

use crate::{
    config::Config,
    model::Settings,
    state::{AppState, SettingsCache},
};
use std::{sync::Arc, time::Duration};
use tokio_util::sync::CancellationToken;

/// Connect, migrate, seed settings from flags (first run only) and wire up
/// every service.
pub async fn build_state(cfg: Config) -> anyhow::Result<Arc<AppState>> {
    let pool = db::connect_and_migrate(&cfg.database_url, cfg.db_pool).await?;
    build_state_with_pool(cfg, pool).await
}

/// Same as [`build_state`] but on an existing (already migrated) pool —
/// used by integration tests that get a per-test database from `sqlx::test`.
pub async fn build_state_with_pool(cfg: Config, pool: sqlx::PgPool) -> anyhow::Result<Arc<AppState>> {
    let cfg = Arc::new(cfg);
    std::fs::create_dir_all(&cfg.data_dir)?;

    // Flags seed settings only where unset, so a redeploy with different
    // flags never clobbers what was tuned in the UI.
    let loaded = db::settings::load(&pool).await?;
    let first_run = loaded.is_none();
    let mut settings = loaded.unwrap_or_default();
    seed_settings(&mut settings, &cfg);
    // Empty is a deliberate "hook disabled" state after first run: an admin
    // clearing the field must not have the env/flag silently re-arm it.
    if first_run && settings.dvr_post_cmd.is_empty() {
        settings.dvr_post_cmd = cfg.dvr_post_cmd.clone();
    }
    settings.normalize();
    db::settings::save(&pool, &settings).await?;
    std::fs::create_dir_all(&settings.recordings_dir).ok();

    let settings = Arc::new(SettingsCache::new(pool.clone(), settings));
    let http = state::http_client();
    let activity = telemetry::Activity::new(pool.clone());
    let metrics = telemetry::metrics_handle();
    telemetry::describe_metrics();

    let art = Arc::new(media::art::Art::new(pool.clone(), settings.clone(), cfg.data_dir.join("img"), http.clone()));
    let metadata =
        Arc::new(metadata::Enricher::new(pool.clone(), settings.clone(), http.clone(), art.clone(), activity.clone()));
    let scanner = Arc::new(media::Scanner::new(pool.clone(), settings.clone(), activity.clone()));
    let streams = Arc::new(stream::Manager::new(settings.clone(), cfg.data_dir.join("hls")));
    let hdhr = Arc::new(hdhr::Client::new(settings.clone(), pool.clone(), http.clone()));
    let guide = Arc::new(epg::Guide::new(settings.clone(), http.clone()));
    let dvr = Arc::new(dvr::Engine::new(
        pool.clone(),
        settings.clone(),
        guide.clone(),
        hdhr.clone(),
        http.clone(),
        activity.clone(),
        metadata.clone(),
    ));
    if let Err(e) = hdhr.load_cached().await {
        tracing::warn!(error = %e, "channel cache load failed");
    }

    Ok(Arc::new(AppState {
        cfg,
        pool,
        settings,
        users: auth::UserCache::default(),
        scanner,
        art,
        streams,
        hdhr,
        guide,
        dvr,
        metadata,
        activity,
        metrics,
        http,
        started: std::time::Instant::now(),
        health: Arc::new(crate::health::Health::default()),
    }))
}

fn seed_settings(s: &mut Settings, cfg: &Config) {
    if s.media_dirs.is_empty() {
        s.media_dirs = Config::split_list(&cfg.media_dirs);
    }
    if s.music_dirs.is_empty() {
        s.music_dirs = Config::split_list(&cfg.music_dirs);
    }
    if s.recordings_dir.is_empty() {
        s.recordings_dir = if cfg.recordings_dir.is_empty() {
            cfg.data_dir.join("recordings").to_string_lossy().to_string()
        } else {
            cfg.recordings_dir.clone()
        };
    }
    if s.xmltv_url.is_empty() {
        s.xmltv_url = cfg.xmltv.clone();
    }
    if s.hdhr_ip.is_empty() {
        s.hdhr_ip = cfg.hdhr_ip.clone();
    }
    if s.tmdb_api_key.is_empty() {
        s.tmdb_api_key = cfg.tmdb_api_key.clone();
    }
    match cfg.commercials.as_str() {
        "off" => s.commercial_mode = model::CommercialMode::Off,
        "skip" => s.commercial_mode = model::CommercialMode::Skip,
        "delete" => s.commercial_mode = model::CommercialMode::Delete,
        _ => {}
    }
}

/// The full HTTP application (API + streaming + embedded UI).
pub fn build_app(state: Arc<AppState>) -> axum::Router {
    api::router(state)
}

/// Start the background loops. They stop when `cancel` fires.
pub fn spawn_background(state: Arc<AppState>, cancel: CancellationToken) {
    let set = state.settings.get();

    tokio::spawn(state.streams.clone().gc_loop(cancel.clone()));

    // health sampler: compute/network every tick, disks every 4th
    {
        let st = state.clone();
        let cancel = cancel.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(crate::health::SAMPLE_EVERY);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            let mut n = 0u64;
            loop {
                tokio::select! {
                    _ = cancel.cancelled() => break,
                    _ = tick.tick() => {}
                }
                let (streams, transcodes) = st.streams.active();
                st.health.sample(streams, transcodes, st.dvr.active_count());
                if n.is_multiple_of(4) {
                    let set = st.settings.get();
                    let mut roots = vec![("data".to_string(), st.cfg.data_dir.display().to_string())];
                    if !set.recordings_dir.is_empty() {
                        roots.push(("recordings".into(), set.recordings_dir.clone()));
                    }
                    if let Some(d) = set.media_dirs.first() {
                        roots.push(("media".into(), d.clone()));
                    }
                    if let Some(d) = set.music_dirs.first() {
                        roots.push(("music".into(), d.clone()));
                    }
                    let h = st.health.clone();
                    // df + statfs walk off the async thread
                    tokio::task::spawn_blocking(move || h.sample_disks(&roots)).await.ok();
                }
                n += 1;
            }
        });
    }
    tokio::spawn(state.metadata.clone().run_loop(cancel.clone()));
    tokio::spawn(state.dvr.clone().run_loop(cancel.clone()));
    tokio::spawn(
        state.scanner.clone().run_loop(Duration::from_secs(60 * set.scan_interval_min.max(1) as u64), cancel.clone()),
    );

    // tuner + guide refresh
    {
        let st = state.clone();
        let cancel = cancel.clone();
        tokio::spawn(async move {
            loop {
                let hours = st.settings.get().guide_refresh_hours.max(1) as u64;
                match tokio::time::timeout(Duration::from_secs(30), st.hdhr.refresh()).await {
                    Ok(Ok(())) => {}
                    Ok(Err(e)) => tracing::warn!(error = %e, "hdhr refresh"),
                    Err(_) => tracing::warn!("hdhr refresh timed out"),
                }
                let chans = st.hdhr.channels();
                let auth = st.hdhr.device().map(|d| d.device_auth).filter(|a| !a.is_empty());
                match tokio::time::timeout(
                    Duration::from_secs(600),
                    st.guide.refresh_with_hdhr(&chans, auth.as_deref()),
                )
                .await
                {
                    Ok(Ok(n)) => {
                        if n > 0 {
                            st.hdhr.set_icons(st.guide.channel_icons()).await;
                            tracing::info!(airings = n, "guide refreshed");
                        }
                    }
                    Ok(Err(e)) => tracing::warn!(error = %e, "guide refresh"),
                    Err(_) => tracing::warn!("guide refresh timed out"),
                }
                tokio::select! {
                    _ = cancel.cancelled() => return,
                    _ = tokio::time::sleep(Duration::from_secs(3600 * hours)) => {}
                }
            }
        });
    }

    // daily housekeeping: activity retention + library gauges
    {
        let st = state.clone();
        let cancel = cancel.clone();
        tokio::spawn(async move {
            loop {
                let days = st.settings.get().activity_retention_days;
                if let Err(e) = db::activity::prune(&st.pool, days).await {
                    tracing::warn!(error = %e, "activity prune");
                }
                if let Ok(counts) = db::items::counts_by_kind(&st.pool).await {
                    for (kind, n) in counts {
                        metrics::gauge!("ontele_library_items", "kind" => kind).set(n as f64);
                    }
                }
                tokio::select! {
                    _ = cancel.cancelled() => return,
                    _ = tokio::time::sleep(Duration::from_secs(6 * 3600)) => {}
                }
            }
        });
    }
}
